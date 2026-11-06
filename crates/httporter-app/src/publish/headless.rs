// SPDX-License-Identifier: MIT OR Apache-2.0

//! The headless publish (I6): compare → plan → guards → run → purge → history, without a window.
//! The CLI, the scheduled publish (which runs the CLI) and the watch folder use it, and nothing
//! else does, so the three behave the same. The Publish button keeps its sheet; both are made of
//! the same steps ([`super::flow`]), so a headless publish compares, plans, guards, purges and
//! records exactly as the sheet does. What differs:
//!
//! - It never asks. The delete guard, which makes the sheet ask for Review Deletions, holds the
//!   deletes back unless `opts.yes`; the uploads still run ([`HeadlessOutcome::GuardTripped`]).
//!   A purge that needs a Bunny account key not kept yet is skipped, which the history says.
//! - One publish per profile at a time, across processes (`win::ProfileLock`, the sheet takes it
//!   too): [`HeadlessOutcome::Busy`] when another holds it. A scheduled run waits up to ten
//!   minutes for it first.
//! - The history row records `opts.source`; the journal rows carry their process as owner
//!   (the CLI's journal is opened `with_owner(process_owner())`, as the window's is).
//! - Without the window ([`HeadlessCtx::open`], the CLI) nothing waits for a person: a site that
//!   refuses its sign-in, a clock that is off, a full drive or a connection down for a minute
//!   ends the run ([`supervise`]) where the window would wait behind a banner. From the window
//!   ([`HeadlessCtx::from_publisher`], the watch folder) its batches are the drawer's like any
//!   other, and its banners deal with those.

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use chrono::Local;
use futures_util::future::{Either, select};
use httporter_core::{
    ActivityKind, CompareStatus, EngineState, HeadlessOptions, HeadlessOutcome, HistorySource,
    JobKind, Journal, ProfileRecord, ProviderError, ProviderKind, PurgeMode, PurgeResult,
    SiteRecord, StorageProvider, exit,
};
use httporter_platform::{
    KeyringStore, SqliteJournal, Store, default_db_path, default_log_dir, process_owner,
};
use httporter_transfer::{
    BatchTarget, Engine, HashCache, JobEventOutcome, PublishPhase, PublishPlan, ScanProgress,
};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::watch;

use super::{Publisher, PurgerAnswer, copy, flow};
use crate::manage::activity_view;
use crate::session::{RealFactory, Session, blocking, hub_error_text};
use crate::site_hub::{HubError, Interactive};

/// How long a scheduled run waits for another publish of its profile to end.
const SCHEDULED_WAIT: Duration = Duration::from_secs(10 * 60);

/// How often a run looks at the engine: for its progress and, without the window, for anything
/// that would make it wait for a person.
const TICK: Duration = Duration::from_millis(250);

/// How long a run without the window waits for a connection that dropped.
const OFFLINE_GIVE_UP: Duration = Duration::from_secs(60);

/// How long [`HeadlessCtx::finish`] waits, at most, for the engine to stop and the journal to
/// catch up.
const SETTLE_FOR: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a headless publish works with: the session (the site database, the credentials as the
/// window reads them, the storage clients), the transfer engine and its journal.
pub struct HeadlessCtx {
    pub(crate) session: Arc<Session>,
    pub(crate) engine: Engine,
    pub(crate) journal: Arc<dyn Journal>,
    /// Local digests: the window's (shared with its compares), or the CLI's own.
    hashes: Arc<HashCache>,
    /// The window's publisher, when the window runs it: its profile page and panes are brought up
    /// to date after a run. `None` for the CLI, which nothing waits behind.
    publisher: Option<Weak<Publisher>>,
    /// Set by [`Self::cancel`].
    cancelled: watch::Sender<bool>,
    /// What the last run noted on the way (secrets uploaded, a purge that couldn't run, a history
    /// row that couldn't be saved), for the CLI to print.
    notes: Mutex<Vec<String>>,
    /// The plan of the last run as compared, deletes held back or not (the CLI's dry run lists
    /// it).
    last_plan: Mutex<Option<PublishPlan>>,
}

impl HeadlessCtx {
    /// For the CLI: a session over the default database with the Credential Manager and the real
    /// providers, an engine on `rt`, and a journal whose rows carry this process as their owner
    /// (`httporter_platform::process_owner`). The activity log is opened, and every transfer job
    /// that ends goes to it, as in the window.
    pub fn open(rt: tokio::runtime::Handle) -> Result<HeadlessCtx, HeadlessError> {
        let path = default_db_path();
        let store =
            Store::open(&path).map_err(|e| HeadlessError::Io(copy::database_unopened(&path, e)))?;
        let session = Arc::new(Session::new(
            store,
            Arc::new(KeyringStore),
            Arc::new(RealFactory),
            || Local::now().naive_local(),
        ));
        let journal = SqliteJournal::open(&path)
            .map_err(|e| HeadlessError::Io(copy::database_unopened(&path, e)))?
            .with_owner(process_owner());
        let journal: Arc<dyn Journal> = Arc::new(journal);
        let engine = Engine::new(
            rt.clone(),
            journal.clone(),
            session.settings(),
            crate::copy::describe_transfer_error,
        );
        activity_view::open_log(default_log_dir());
        let mut events = engine.events();
        crate::rt::Rt::new(rt).spawn(async move {
            loop {
                match events.recv().await {
                    Ok(e) => activity_view::job_ended(e),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
        });
        let hashes = Arc::new(HashCache::with_store(session.hash_store()));
        Ok(Self {
            session,
            engine,
            journal,
            hashes,
            publisher: None,
            cancelled: watch::channel(false).0,
            notes: Mutex::default(),
            last_plan: Mutex::default(),
        })
    }

    /// For the window (the watch folder): the publisher's session, engine, journal and digests.
    #[allow(dead_code)] // T4 (the watch folder) uses it.
    pub fn from_publisher(p: &Arc<Publisher>) -> HeadlessCtx {
        Self {
            session: p.session.clone(),
            engine: p.transfers.engine().clone(),
            journal: p.transfers.journal(),
            hashes: p.hashes.clone(),
            publisher: Some(Arc::downgrade(p)),
            cancelled: watch::channel(false).0,
            notes: Mutex::default(),
            last_plan: Mutex::default(),
        }
    }

    /// Runs without the window: nothing waits behind a banner for a person.
    fn standalone(&self) -> bool {
        self.publisher.is_none()
    }

    /// Stops what runs: a compare under way, or a run about to send. Without the window (the
    /// CLI's Ctrl+C) the engine's batches are cancelled too, and a publish under way ends
    /// Cancelled in the history; from the window, batches already sent are the drawer's to cancel.
    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
        if self.standalone() {
            self.engine.cancel_all();
        }
    }

    /// [`Self::cancel`] was called.
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// `work`, unless [`Self::cancel`] comes first (then `None`, and `work` is dropped).
    pub(crate) async fn until_cancelled<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        let mut cancelled = self.cancelled.subscribe();
        let stop = async move {
            // The sender lives as long as `self`, so this ends only once cancelled.
            let _ = cancelled.wait_for(|c| *c).await;
        };
        match select(pin!(work), pin!(stop)).await {
            Either::Left((value, _)) => Some(value),
            Either::Right(_) => None,
        }
    }

    /// What the last run noted on the way, taken.
    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.notes))
    }

    fn note(&self, text: String) {
        lock(&self.notes).push(text);
    }

    /// The plan of the last run as compared (before the delete guard held anything back).
    pub fn last_plan(&self) -> Option<PublishPlan> {
        lock(&self.last_plan).clone()
    }

    /// `site`'s storage client, built as the window builds it (its password from the Credential
    /// Manager). A missing or unreadable password is an [`HeadlessError::Auth`]; anything else
    /// about the site's details a [`HeadlessError::Config`].
    pub(crate) async fn provider(
        &self,
        site: &SiteRecord,
    ) -> Result<Arc<dyn StorageProvider>, HeadlessError> {
        let (hub, record) = (self.session.hub().clone(), site.clone());
        blocking(move || hub.provider(&record))
            .await
            .map_err(|e| match &e {
                HubError::MissingSecret
                | HubError::Credential(_)
                | HubError::Provider(ProviderError::Auth { .. }) => {
                    HeadlessError::Auth(hub_error_text(&e, site))
                }
                HubError::Provider(_) => HeadlessError::Config(hub_error_text(&e, site)),
            })
    }

    /// Tells the engine the Cache-Control of every S3 profile on `site`, as the window does at
    /// start: uploads under a profile's folder carry it, whether they come from a publish or not.
    pub(crate) async fn register_cache_control(&self, site: &SiteRecord) {
        if site.provider != ProviderKind::S3 {
            return;
        }
        for profile in self.session.profiles().await {
            if profile.site == site.id && profile.cache_control.is_some() {
                self.engine.set_cache_control(
                    &site.id,
                    &profile.remote_dir,
                    profile.cache_control.clone(),
                );
            }
        }
    }

    /// Once the CLI's work is over: waits (ten seconds at most) for the engine to stop and the
    /// journal to catch up, then lets go of the finished journal rows (other live processes' rows
    /// are left alone). From the window, nothing: the window's own closing does that.
    pub async fn finish(&self) {
        if !self.standalone() {
            return;
        }
        let deadline = Instant::now() + SETTLE_FOR;
        while Instant::now() < deadline
            && (self.engine.has_work()
                || self.engine.pending_writes() > 0
                || self.engine.pending_aborts() > 0)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let journal = self.journal.clone();
        let _ = blocking(move || journal.clear_finished()).await;
    }
}

/// How far a headless publish is, for the CLI's progress line (stderr, at most twice a second).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessProgress {
    Comparing { done: u64, total: u64 },
    Uploading { done: u64, total: u64, bytes: u64 },
    Deleting { done: u64, total: u64 },
    Purging,
}

/// Why a headless publish couldn't run. Each maps to an exit code ([`HeadlessError::exit_code`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessError {
    /// The profile, its site or its folder is wrong or missing (exit 2).
    Config(String),
    /// The site's credentials are missing or wrong (exit 3), with the window's words for it.
    Auth(String),
    /// Reading the folder or the database failed (exit 1).
    Io(String),
}

impl HeadlessError {
    /// The CLI's exit code for it.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) => exit::CONFIG,
            Self::Auth(_) => exit::AUTH,
            Self::Io(_) => exit::FAILURES,
        }
    }

    /// What to tell the user.
    pub fn text(&self) -> &str {
        match self {
            Self::Config(text) | Self::Auth(text) | Self::Io(text) => text,
        }
    }
}

/// A compare's progress as the CLI shows it: the files checked so far once there are any to
/// check, else the files listed on both sides (no total yet).
fn comparing(scan: ScanProgress) -> HeadlessProgress {
    if scan.to_hash > 0 {
        HeadlessProgress::Comparing {
            done: scan.hashed,
            total: scan.to_hash,
        }
    } else {
        HeadlessProgress::Comparing {
            done: scan.local_files + scan.remote_files,
            total: 0,
        }
    }
}

/// Runs `work` (a publish's or a transfer's batches on `site`), calling `tick` every [`TICK`]
/// meanwhile. Without the window, a site the engine stops for something only a person can fix
/// (a refused sign-in, a clock that is off, a full drive, or no connection for
/// [`OFFLINE_GIVE_UP`]) has its work cancelled, so `work` ends; the second value says why.
pub(crate) async fn supervise<T>(
    ctx: &HeadlessCtx,
    site: &SiteRecord,
    mut tick: impl FnMut(),
    work: impl Future<Output = T>,
) -> (T, Option<HeadlessError>) {
    let mut work = pin!(work);
    let mut stuck = None;
    let mut offline_since = None;
    loop {
        let sleep = pin!(tokio::time::sleep(TICK));
        if let Either::Left((value, _)) = select(work.as_mut(), sleep).await {
            return (value, stuck);
        }
        tick();
        if ctx.standalone()
            && stuck.is_none()
            && let Some(why) = stalled(&ctx.engine, site, &mut offline_since)
        {
            ctx.engine.cancel_site(&site.id);
            stuck = Some(why);
        }
    }
}

/// Why the engine waits for a person now, if it does; `offline_since` keeps when the connection
/// dropped.
fn stalled(
    engine: &Engine,
    site: &SiteRecord,
    offline_since: &mut Option<Instant>,
) -> Option<HeadlessError> {
    match engine.snapshot().state {
        EngineState::AuthStopped { .. } => Some(HeadlessError::Auth(
            copy::headless_sign_in_failed(&site.name),
        )),
        EngineState::ClockSkew { .. } => Some(HeadlessError::Io(crate::copy::CLOCK_OFF.to_owned())),
        EngineState::DiskFull { drive, needed } => {
            Some(HeadlessError::Io(copy::headless_disk_full(&drive, needed)))
        }
        EngineState::Offline { .. } => {
            let since = *offline_since.get_or_insert_with(Instant::now);
            (since.elapsed() >= OFFLINE_GIVE_UP)
                .then(|| HeadlessError::Io(copy::headless_offline(&site.name)))
        }
        EngineState::Idle | EngineState::Running | EngineState::Paused => {
            *offline_since = None;
            None
        }
    }
}

/// Logs a headless publish of `profile` that didn't run, or stopped short, in the activity log:
/// `why`, with who started it.
pub fn log_not_run(ctx: &HeadlessCtx, profile: &ProfileRecord, source: HistorySource, why: &str) {
    let site = ctx
        .session
        .site(&profile.site)
        .map(|site| site.name)
        .unwrap_or_default();
    activity_view::event(
        ActivityKind::Publish,
        &site,
        &format!("/{}", profile.remote_dir.as_str()),
        false,
        copy::from_source(source, why),
        0,
    );
}

/// Logs, in the activity log, that another publish of `profile` held its lock, so this one didn't
/// run.
pub fn log_busy(ctx: &HeadlessCtx, profile: &ProfileRecord, source: HistorySource) {
    log_not_run(ctx, profile, source, &copy::headless_busy(&profile.name));
}

/// The jobs of a publish that ended so far, from the engine's events.
#[derive(Default)]
struct Counted {
    uploads: u64,
    bytes: u64,
    deletes: u64,
}

/// Publishes `profile` as `opts` says: compares the folder with the site, plans, checks the
/// guards, uploads, deletes (unless held back), purges and stores the history row. `progress` is
/// called as it goes, from the engine's tasks.
pub async fn run_headless(
    ctx: &HeadlessCtx,
    profile: &ProfileRecord,
    opts: &HeadlessOptions,
    progress: &(dyn Fn(HeadlessProgress) + Send + Sync),
) -> Result<HeadlessOutcome, HeadlessError> {
    lock(&ctx.notes).clear();
    *lock(&ctx.last_plan) = None;
    let session = &ctx.session;
    let site = session
        .site(&profile.site)
        .ok_or_else(|| HeadlessError::Config(copy::PROFILE_SITE_GONE.to_owned()))?;
    if !profile.local_dir.is_dir() {
        return Err(HeadlessError::Config(copy::headless_folder_missing(
            &profile.local_dir,
        )));
    }
    if !opts.dry_run && crate::manage::settings_tabs::site_read_only(session, &site.id) {
        return Err(HeadlessError::Config(
            crate::manage::settings_tabs::read_only_refused(&site.name),
        ));
    }
    // One publish of a profile at a time. A dry run changes nothing, so it needs no lock.
    let _lock = if opts.dry_run {
        None
    } else {
        let wait = if opts.source == HistorySource::Scheduled {
            SCHEDULED_WAIT
        } else {
            Duration::ZERO
        };
        match flow::lock_profile(&profile.id, wait).await {
            Some(lock) => Some(lock),
            None => return Ok(HeadlessOutcome::Busy),
        }
    };
    let provider = ctx.provider(&site).await?;

    // Compare, as the sheet does; with nobody to click again, its listings retry a little.
    let on_scan = |scan: ScanProgress| progress(comparing(scan));
    let lister = Interactive(provider.clone());
    let compare = flow::compare(session, &ctx.hashes, &lister, &site, profile, &on_scan);
    let compared = match ctx.until_cancelled(compare).await {
        None => return Err(HeadlessError::Io(copy::HEADLESS_CANCELLED.to_owned())),
        Some(Ok(compared)) => compared,
        Some(Err(failed)) => {
            return Err(match failed.remote {
                Some(ProviderError::Auth { .. }) => HeadlessError::Auth(failed.text),
                _ => HeadlessError::Io(failed.text),
            });
        }
    };

    // Plan, purge and guards, as the sheet does; the deletes go only as the profile says.
    let deletes_allowed = profile.remove_extras && !opts.no_delete;
    let mut plan = PublishPlan::from_compare(&compared.result, deletes_allowed);
    let wanted = flow::purge_for_site(&site, opts.purge.unwrap_or(profile.purge));
    let purge = match flow::zone_instead(&site, wanted, compared.zones, &plan) {
        Some(_) => PurgeMode::Zone,
        None => wanted,
    };
    let secrets = flow::secrets_uploaded(&compared.result.secrets, &plan);
    if !secrets.is_empty() {
        ctx.note(copy::secrets_warning(&secrets));
    }
    let would_delete = plan.delete_count();
    let guarded = !opts.yes && flow::guard_trips(profile, &plan);
    *lock(&ctx.last_plan) = Some(plan.clone());
    if opts.dry_run {
        let count = |status: CompareStatus| {
            plan.items
                .iter()
                .filter(|item| item.status == status && item.include)
                .count() as u64
        };
        let (_, bytes) = plan.upload_totals();
        return Ok(HeadlessOutcome::DryRun {
            new: count(CompareStatus::New),
            changed: count(CompareStatus::Changed),
            delete: would_delete,
            bytes,
        });
    }
    // Nothing is deleted without a review the guard asks for: the uploads still go.
    if guarded {
        plan.set_remove_extras(false);
    }
    let remote = format!("/{}", profile.remote_dir.as_str());
    if guarded {
        activity_view::event(
            ActivityKind::Publish,
            &site.name,
            &remote,
            false,
            copy::from_source(
                opts.source,
                &copy::headless_deletes_held(&site.name, would_delete),
            ),
            0,
        );
    }
    let (uploads, _) = plan.upload_totals();
    let deletes = plan.delete_count();
    if uploads + deletes == 0 {
        if guarded {
            return Ok(HeadlessOutcome::GuardTripped {
                would_delete,
                uploaded: 0,
            });
        }
        activity_view::event(
            ActivityKind::Publish,
            &site.name,
            &remote,
            true,
            copy::from_source(opts.source, &copy::headless_up_to_date(&site.name)),
            0,
        );
        return Ok(HeadlessOutcome::Done {
            uploaded: 0,
            deleted: 0,
            failed: 0,
            bytes: 0,
            purge: PurgeResult::Off,
        });
    }

    // The purger, never asking for a key: without one the publish goes on unpurged.
    let purger: flow::Purger = if purge == PurgeMode::Off {
        Ok(None)
    } else {
        match super::cdn::purger_for(session, &site).await {
            PurgerAnswer::Ready(purger) => Ok(Some(purger)),
            PurgerAnswer::NeedKey => Err(copy::PURGE_NO_KEY.to_owned()),
            PurgerAnswer::Unavailable(why) => Err(why),
        }
    };
    if ctx.is_cancelled() {
        return Err(HeadlessError::Io(copy::HEADLESS_CANCELLED.to_owned()));
    }

    // Run and record, as the sheet does.
    let target = BatchTarget {
        site: site.id.clone(),
        site_name: site.name.clone(),
        host: site.host.clone(),
        provider,
    };
    let run = flow::Run {
        profile: profile.clone(),
        site: site.clone(),
        target,
        plan,
        purge,
        source: opts.source,
    };
    let phase = Mutex::new(None::<PublishPhase>);
    let on_phase = |now: PublishPhase| {
        *lock(&phase) = Some(now);
        if matches!(now, PublishPhase::Purging { .. }) {
            progress(HeadlessProgress::Purging);
        }
    };
    let mut events = ctx.engine.events();
    let mut counted = Counted::default();
    let tick = || {
        loop {
            match events.try_recv() {
                Ok(e) if e.site == site.id => match e.kind {
                    JobKind::Upload => {
                        counted.uploads += 1;
                        if e.outcome == JobEventOutcome::Done {
                            counted.bytes += e.bytes;
                        }
                    }
                    JobKind::Delete => counted.deletes += 1,
                    JobKind::Download | JobKind::CreateFolder => {}
                },
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        let now = *lock(&phase);
        match now {
            Some(PublishPhase::Uploading) => progress(HeadlessProgress::Uploading {
                done: counted.uploads.min(uploads),
                total: uploads,
                bytes: counted.bytes,
            }),
            Some(PublishPhase::Deleting) => progress(HeadlessProgress::Deleting {
                done: counted.deletes.min(deletes),
                total: deletes,
            }),
            Some(PublishPhase::Purging { .. }) => progress(HeadlessProgress::Purging),
            Some(PublishPhase::Finished) | None => {}
        }
    };
    let execute = flow::execute(&ctx.engine, session, &run, purger, &on_phase);
    let (executed, stuck) = supervise(ctx, &site, tick, execute).await;
    let record = executed.map_err(HeadlessError::Io)?;
    let (uploaded, deleted, failed, bytes) =
        (record.uploaded, record.deleted, record.failed, record.bytes);
    let purge_result = record.purge.clone();
    let (_, saved) = flow::record(session, &run, record).await;
    if let Err(e) = saved {
        ctx.note(copy::history_not_saved(&e));
    }
    if let Some(p) = ctx.publisher.as_ref().and_then(Weak::upgrade) {
        p.link.refresh_remote_at(&site.id, &profile.remote_dir);
        super::profile_view::refresh(&p);
    }
    if let Some(why) = stuck {
        return Err(why);
    }
    if guarded && failed == 0 {
        return Ok(HeadlessOutcome::GuardTripped {
            would_delete,
            uploaded,
        });
    }
    if guarded {
        ctx.note(copy::headless_deletes_held(&site.name, would_delete));
    }
    Ok(HeadlessOutcome::Done {
        uploaded,
        deleted,
        failed,
        bytes,
        purge: purge_result,
    })
}
