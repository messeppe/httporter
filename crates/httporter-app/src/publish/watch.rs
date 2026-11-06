// SPDX-License-Identifier: MIT OR Apache-2.0

//! The watch folder (§8.8.3, I6, T4): while HTTPorter is open, a profile with
//! `ProfileExtras::watch` publishes its folder's changes on its own, 10 s after the last one,
//! through the headless publish (`headless::run_headless`, `HistorySource::Watch`).
//!
//! - **One loop per watched profile** ([`watch_loop`], a task on the runtime), started at start
//!   ([`install`]) and when a saved profile turns the option on ([`profile_saved`]). It owns a
//!   `win::DirWatcher` on the folder; the watcher's thread drops what the skip rules leave out and
//!   sends the rest ([`classify`]).
//! - **Debounce** ([`Pending`]): a publish starts once the folder has been quiet for [`QUIET`]
//!   (10 s), or at the latest [`MAX_WAIT`] after the first change it publishes (a file that is
//!   written all the time would otherwise hold it back for ever).
//! - **One run at a time.** The loop runs one publish at a time; changes during it queue exactly
//!   one more. Across the window, the CLI and the schedule the profile's lock decides: a publish
//!   that finds it held (`HeadlessOutcome::Busy`) is tried again 10 s later.
//! - **Deletes:** without `unattended_deletes` the run deletes nothing (`no_delete`). When files
//!   were removed from the folder and the profile removes extras, a dry run afterwards counts the
//!   deletes that wait, and the profile page says "3 files were removed from the folder", with
//!   Review… (the Publish sheet). A delete guard that trips says the same. With
//!   `unattended_deletes` the run deletes without a review (`yes`).
//! - **Stops**, with a note on the profile page, a toast and an activity entry: the folder is gone
//!   or renamed for longer than [`GONE_GRACE`] (a build that deletes and makes it again is waited
//!   for, and nothing is published while it is gone; an empty folder is never published either),
//!   the watcher overflows twice in a row (after one full compare), the site is read-only or
//!   removed. Turning the option off or deleting the profile stops it quietly.
//! - Results: "Published 3 changed files" (a toast), failures as a warning toast; the uploads are
//!   the engine's, so the transfer drawer shows them like any batch.
//!
//! What the loop needs from the app is the [`Host`] trait ([`AppHost`] in the app, a fake in the
//! tests), so the state machine runs under the tests' paused clock without a window, a folder or a
//! site.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use httporter_core::{
    ActivityKind, HeadlessOptions, HeadlessOutcome, HistorySource, ProfileId, ProfileRecord,
    PurgeResult, SkipRules,
};
use httporter_platform::win;
use slint::{ComponentHandle, Model};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use super::headless::{self, HeadlessCtx, HeadlessError};
use super::{Publisher, copy, lock};
use crate::AppWindow;
use crate::comfort::toast::{self, ToastKind};
use crate::manage::activity_view;
use crate::manage::settings_tabs::site_read_only;
use crate::session::blocking;

/// How long the folder stays quiet before its changes are published.
const QUIET: Duration = Duration::from_secs(10);
/// The longest a publish waits for the folder to go quiet, from the first change it publishes.
const MAX_WAIT: Duration = Duration::from_secs(120);
/// How often the loop looks whether the folder is still there and still watched.
const TICK: Duration = Duration::from_secs(2);
/// How long the folder may be gone (a build deleting and making it again) before watching stops.
const GONE_GRACE: Duration = QUIET;
/// Overflows in a row that stop watching, once one full compare has run.
const OVERFLOWS_TO_STOP: u32 = 2;

type Sender = mpsc::UnboundedSender<Signal>;
type Receiver = mpsc::UnboundedReceiver<Signal>;

// ---- What the window keeps ----

/// The watched profiles and what their pages say. In the [`Publisher`]; any thread.
#[derive(Default)]
pub(super) struct WatchState {
    loops: HashMap<ProfileId, LoopEntry>,
    notes: HashMap<ProfileId, Notes>,
    /// Counts the loops started: a loop's news count only while it is still the profile's.
    serial: u64,
}

struct LoopEntry {
    tx: Sender,
    serial: u64,
    /// The folder is watched now (the sidebar's mark).
    active: bool,
}

/// What a profile page says about watching.
#[derive(Default)]
struct Notes {
    /// Files removed from the folder whose deletes wait for a review.
    held: u64,
    /// Why watching stopped, as the page says it.
    stopped: Option<String>,
}

/// Starts watching the profiles that ask for it (off the UI thread), and wires Review… of the
/// profile page's note. Starts no publish: only a change does.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    ui.on_watch_review({
        let (p, weak) = (p.clone(), ui.as_weak());
        move || {
            let Some(id) = super::profile_view::shown(&p) else {
                return;
            };
            // The Publish sheet lists the deletes now; the note has said its part.
            if let Some(notes) = lock(&p.watch).notes.get_mut(&id) {
                notes.held = 0;
            }
            if let Some(ui) = weak.upgrade() {
                show_note(&p, &ui);
            }
            p.open_sheet(id);
        }
    });
    let p2 = p.clone();
    p.link.rt().spawn(async move {
        for profile in p2.session.profiles().await {
            if profile.extras.watch {
                start(&p2, profile);
            }
        }
    });
}

/// Whether profile `id`'s folder is watched now (the sidebar's "Watching" mark).
pub(super) fn watching(p: &Publisher, id: &ProfileId) -> bool {
    lock(&p.watch).loops.get(id).is_some_and(|entry| entry.active)
}

/// A profile was saved (the profile sheet, Change Folder…): watching starts, follows the new
/// folder and skip rules, or stops, as its option now says.
pub(super) fn profile_saved(p: &Arc<Publisher>, record: &ProfileRecord) {
    let started = {
        let mut w = lock(&p.watch);
        if let Some(notes) = w.notes.get_mut(&record.id) {
            notes.stopped = None;
        }
        if record.extras.watch {
            w.loops.get(&record.id).is_some_and(|entry| {
                entry
                    .tx
                    .send(Signal::Update(Box::new(record.clone())))
                    .is_ok()
            })
        } else {
            if let Some(entry) = w.loops.remove(&record.id) {
                let _ = entry.tx.send(Signal::Stop);
            }
            true
        }
    };
    if !started {
        start(p, record.clone());
    }
    refresh_marks(p);
}

/// A profile was deleted: its loop ends, and its notes go.
pub(super) fn profile_removed(p: &Arc<Publisher>, id: &ProfileId) {
    {
        let mut w = lock(&p.watch);
        if let Some(entry) = w.loops.remove(id) {
            let _ = entry.tx.send(Signal::Stop);
        }
        w.notes.remove(id);
    }
    refresh_marks(p);
}

/// Puts the note of the profile the page shows on the page (empty hides it). UI thread.
pub(super) fn show_note(p: &Arc<Publisher>, ui: &AppWindow) {
    let (text, review) = super::profile_view::shown(p)
        .and_then(|id| lock(&p.watch).notes.get(&id).map(note_text))
        .unwrap_or_default();
    if ui.get_watch_note() != text.as_str() {
        ui.set_watch_note(text.into());
    }
    ui.set_watch_can_review(review);
}

/// The page's note: why watching stopped, then the deletes that wait; and whether Review… shows.
fn note_text(notes: &Notes) -> (String, bool) {
    let held = (notes.held > 0).then(|| copy::watch_held_back(notes.held));
    let text: Vec<String> = [notes.stopped.clone(), held].into_iter().flatten().collect();
    (text.join(" "), notes.held > 0)
}

/// Starts profile `record`'s loop (in place of one it may have).
fn start(p: &Arc<Publisher>, record: ProfileRecord) {
    let (tx, rx) = mpsc::unbounded_channel();
    let serial = {
        let mut w = lock(&p.watch);
        w.serial += 1;
        let serial = w.serial;
        let entry = LoopEntry {
            tx: tx.clone(),
            serial,
            active: false,
        };
        if let Some(old) = w.loops.insert(record.id.clone(), entry) {
            let _ = old.tx.send(Signal::Stop);
        }
        serial
    };
    let host = Arc::new(AppHost {
        p: Arc::downgrade(p),
        serial,
    });
    p.link.rt().spawn(watch_loop(host, record, tx, rx));
}

/// Brings the sidebar's "Watching" marks and the page's note up to date (from any thread).
fn refresh_marks(p: &Arc<Publisher>) {
    let p2 = p.clone();
    let _ = p.link.ui().upgrade_in_event_loop(move |ui| {
        show_marks(&p2, &ui);
        show_note(&p2, &ui);
    });
}

/// Sets each sidebar profile row's "Watching" mark as its loop says. UI thread.
fn show_marks(p: &Publisher, ui: &AppWindow) {
    let rows = ui.get_profiles();
    for i in 0..rows.row_count() {
        if let Some(mut row) = rows.row_data(i) {
            let on = watching(p, &ProfileId(row.id.to_string()));
            if row.watching != on {
                row.watching = on;
                rows.set_row_data(i, row);
            }
        }
    }
}

// ---- The loop ----

/// What a profile's loop hears.
enum Signal {
    Change(Change),
    /// The profile was saved with watching still on.
    Update(Box<ProfileRecord>),
    /// Watching was turned off, or the profile deleted.
    Stop,
}

/// A change under the folder that the skip rules let through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// A file or folder changed; `gone`: it isn't there any more (removed, or renamed away).
    File { gone: bool },
    /// Too many changes came at once: the watcher lost track of which.
    Overflow,
}

/// Whether the folder is there, and has anything in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Folder {
    Present,
    Empty,
    Gone,
}

/// Why watching a profile ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Stop {
    /// Turned off, or the profile deleted: nothing to say.
    Off,
    /// The folder is gone, renamed or moved away.
    FolderGone,
    /// The watcher overflowed twice in a row (after one full compare).
    Overflow,
    /// The site (its name) is read-only: watching is off.
    ReadOnly(String),
    /// The site was removed.
    SiteGone,
    /// The folder couldn't be watched (why).
    WatchFailed(String),
}

/// What a loop tells the window.
#[derive(Debug, Clone, PartialEq, Eq)]
enum News {
    /// The folder is watched (the sidebar's mark).
    Watching,
    /// A run published `files` changed files (uploads and deletes); `purge_failed`: its CDN purge
    /// didn't go.
    Published { files: u64, purge_failed: bool },
    /// A run's `failed` files didn't go; `files` did.
    Failed { failed: u64, files: u64 },
    /// A run couldn't happen, in the headless publish's words.
    Error(String),
    /// `n` files removed from the folder wait for a review to be deleted (0: none any more).
    HeldBack(u64),
    /// A run ended: the page reads its history again.
    Ran,
    Stopped(Stop),
}

/// What the loop needs from the app; the tests give it a fake.
trait Host: Send + Sync + 'static {
    /// Starts watching `dir`: each change the skip rules let through goes to `tx`.
    fn watch(&self, dir: &Path, rules: SkipRules, tx: Sender) -> io::Result<Box<dyn Source>>;
    /// Whether `dir` is there.
    fn folder(&self, dir: PathBuf) -> impl Future<Output = Folder> + Send + 'static;
    /// The profile as saved now, or why it can't be watched any more.
    fn check(&self, id: ProfileId)
    -> impl Future<Output = Result<ProfileRecord, Stop>> + Send + 'static;
    /// One headless publish of `profile`.
    fn publish(
        &self,
        profile: ProfileRecord,
        opts: HeadlessOptions,
    ) -> impl Future<Output = Result<HeadlessOutcome, HeadlessError>> + Send + 'static;
    /// Tells the window `news` about `profile`.
    fn tell(&self, profile: &ProfileRecord, news: News);
}

/// A running watcher; dropping it stops it.
trait Source: Send {
    /// It still reports changes.
    fn alive(&self) -> bool;
}

/// The changes waiting to be published.
#[derive(Debug, Default)]
struct Pending {
    first: Option<Instant>,
    last: Option<Instant>,
    /// A path that changed was gone, or the changes overflowed: files may have been removed.
    removed: bool,
    /// Overflows since the last ordinary change (kept across publishes: "in a row").
    overflows: u32,
}

impl Pending {
    fn note(&mut self, change: Change, at: Instant) {
        self.mark(at);
        match change {
            Change::File { gone } => {
                self.overflows = 0;
                self.removed |= gone;
            }
            Change::Overflow => {
                self.overflows += 1;
                self.removed = true;
            }
        }
    }

    /// Something changed that no watcher saw (the folder came back, the watcher started again).
    fn unknown(&mut self, at: Instant) {
        self.mark(at);
        self.removed = true;
    }

    fn mark(&mut self, at: Instant) {
        self.first.get_or_insert(at);
        self.last = Some(at);
    }

    /// When the waiting changes are published; `None` when none wait.
    fn due(&self) -> Option<Instant> {
        Some((self.last? + QUIET).min(self.first? + MAX_WAIT))
    }

    /// The changes go to a publish: whether files may have been removed among them.
    fn take(&mut self) -> bool {
        self.first = None;
        self.last = None;
        std::mem::take(&mut self.removed)
    }

    /// A publish that couldn't run (the lock was held): its changes wait another [`QUIET`].
    fn retry(&mut self, removed: bool, at: Instant) {
        self.first = Some(at);
        self.last = Some(self.last.map_or(at, |last| last.max(at)));
        self.removed |= removed;
    }
}

/// How a run ended: the publish, and the deletes waiting for a review when they were counted.
struct RunEnd {
    result: Result<HeadlessOutcome, HeadlessError>,
    held: Option<u64>,
}

/// A watched publish: deletes only when the profile allows them unattended (then past the delete
/// guard too), the profile's purge.
fn options(profile: &ProfileRecord) -> HeadlessOptions {
    let unattended = profile.extras.unattended_deletes;
    HeadlessOptions {
        dry_run: false,
        yes: unattended,
        no_delete: !unattended,
        purge: None,
        source: HistorySource::Watch,
    }
}

/// One publish; then, when files were removed and their deletes wait, a dry run that counts them
/// (the uploads are done by then, so what it finds to do is the deletes).
async fn run_once<H: Host>(host: Arc<H>, profile: ProfileRecord, removed: bool) -> RunEnd {
    let result = host.publish(profile.clone(), options(&profile)).await;
    let waits = removed && profile.remove_extras && !profile.extras.unattended_deletes;
    let held = match &result {
        Ok(HeadlessOutcome::GuardTripped { would_delete, .. }) => Some(*would_delete),
        Ok(HeadlessOutcome::Done { .. }) if waits => {
            let count = HeadlessOptions {
                dry_run: true,
                yes: false,
                no_delete: false,
                purge: None,
                source: HistorySource::Watch,
            };
            match host.publish(profile, count).await {
                Ok(HeadlessOutcome::DryRun { delete, .. }) => Some(delete),
                _ => None,
            }
        }
        _ => None,
    };
    RunEnd { result, held }
}

/// Watches one profile until it stops, then says why.
async fn watch_loop<H: Host>(host: Arc<H>, profile: ProfileRecord, tx: Sender, mut rx: Receiver) {
    let mut lp = Loop {
        host,
        profile,
        tx,
        source: None,
        pending: Pending::default(),
        ending: None,
        gone_since: None,
    };
    let stop = lp.run(&mut rx).await;
    lp.source = None;
    lp.host.tell(&lp.profile, News::Stopped(stop));
}

/// A loop's state.
struct Loop<H: Host> {
    host: Arc<H>,
    /// As last read or saved.
    profile: ProfileRecord,
    /// For the watchers this loop starts.
    tx: Sender,
    source: Option<Box<dyn Source>>,
    pending: Pending,
    /// Watching ends once the run under way is over (after the overflow's full compare, for
    /// [`Stop::Overflow`]).
    ending: Option<Stop>,
    /// The folder was found gone then; nothing is published while it is.
    gone_since: Option<Instant>,
}

impl<H: Host> Loop<H> {
    async fn run(&mut self, rx: &mut Receiver) -> Stop {
        match self.host.check(self.profile.id.clone()).await {
            Ok(fresh) => self.profile = fresh,
            Err(stop) => return stop,
        }
        if self.host.folder(self.profile.local_dir.clone()).await == Folder::Gone {
            return Stop::FolderGone;
        }
        if let Err(stop) = self.open() {
            return stop;
        }
        self.host.tell(&self.profile, News::Watching);

        // The run under way, and whether files may have been removed among its changes.
        let mut running: Option<(JoinHandle<RunEnd>, bool)> = None;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            if running.is_none()
                && let Some(stop) = &self.ending
                && (*stop != Stop::Overflow || self.pending.due().is_none())
            {
                return stop.clone();
            }
            let due = if running.is_none() && self.gone_since.is_none() {
                self.pending.due()
            } else {
                None
            };
            tokio::select! {
                signal = rx.recv() => self.heard(signal),
                () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now)), if due.is_some() => {
                    if let Some(removed) = self.ready().await {
                        let run = run_once(self.host.clone(), self.profile.clone(), removed);
                        running = Some((tokio::spawn(run), removed));
                    }
                }
                end = async {
                    match running.as_mut() {
                        Some((run, _)) => run.await,
                        None => std::future::pending().await,
                    }
                }, if running.is_some() => {
                    let removed = running.take().is_some_and(|(_, removed)| removed);
                    let end = end.unwrap_or_else(|_| RunEnd {
                        result: Err(HeadlessError::Io(copy::WATCH_RUN_BROKE.to_owned())),
                        held: None,
                    });
                    self.report(end, removed);
                }
                _ = tick.tick(), if !self.ends_now() => self.look().await,
            }
        }
    }

    /// Watching ends as soon as the run under way (if any) is over.
    fn ends_now(&self) -> bool {
        self.ending
            .as_ref()
            .is_some_and(|stop| *stop != Stop::Overflow)
    }

    /// Starts watching the folder (in place of the watcher there was).
    fn open(&mut self) -> Result<(), Stop> {
        self.source = None;
        let rules = SkipRules::new(&self.profile.skip_rules);
        match self
            .host
            .watch(&self.profile.local_dir, rules, self.tx.clone())
        {
            Ok(source) => {
                self.source = Some(source);
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(Stop::FolderGone),
            Err(e) => Err(Stop::WatchFailed(e.to_string())),
        }
    }

    /// Takes the profile as saved now; a new folder or new skip rules get a new watcher.
    fn follow(&mut self, fresh: ProfileRecord) {
        let rewatch = fresh.local_dir != self.profile.local_dir
            || fresh.skip_rules != self.profile.skip_rules;
        self.profile = fresh;
        if rewatch {
            self.gone_since = None;
            if let Err(stop) = self.open() {
                self.ending = Some(stop);
            }
        }
    }

    fn heard(&mut self, signal: Option<Signal>) {
        match signal {
            None | Some(Signal::Stop) => self.ending = Some(Stop::Off),
            Some(Signal::Change(change)) if !self.ends_now() => {
                self.pending.note(change, Instant::now());
                if self.pending.overflows >= OVERFLOWS_TO_STOP && self.ending.is_none() {
                    // No more watching: the changes waiting get one full compare, then it stops.
                    self.ending = Some(Stop::Overflow);
                    self.source = None;
                }
            }
            Some(Signal::Change(_)) => {}
            Some(Signal::Update(fresh)) => {
                if self.ending.is_none() {
                    self.follow(*fresh);
                }
            }
        }
    }

    /// The changes are due: whether a publish may run now (and whether files may have been
    /// removed among them). Not when the profile can't be watched any more, the folder is gone
    /// (it waits for the folder to come back) or empty (nothing to publish; the changes are
    /// dropped).
    async fn ready(&mut self) -> Option<bool> {
        match self.host.check(self.profile.id.clone()).await {
            Err(stop) => {
                self.ending = Some(stop);
                return None;
            }
            Ok(fresh) => {
                if self.ending.is_none() {
                    self.follow(fresh);
                }
            }
        }
        if self.ends_now() {
            return None;
        }
        match self.host.folder(self.profile.local_dir.clone()).await {
            Folder::Gone => {
                self.gone_since.get_or_insert_with(Instant::now);
                self.source = None;
                None
            }
            Folder::Empty => {
                self.pending.take();
                None
            }
            Folder::Present => Some(self.pending.take()),
        }
    }

    /// Says how a run went. A publish that found the lock held tries again later.
    fn report(&mut self, end: RunEnd, removed: bool) {
        let (host, profile) = (&self.host, &self.profile);
        match end.result {
            Ok(HeadlessOutcome::Busy) => {
                self.pending.retry(removed, Instant::now());
                return;
            }
            Ok(HeadlessOutcome::Done {
                uploaded,
                deleted,
                failed,
                purge,
                ..
            }) => {
                let files = uploaded + deleted;
                if failed > 0 {
                    host.tell(profile, News::Failed { failed, files });
                } else if files > 0 {
                    let purge_failed = matches!(purge, PurgeResult::Failed(_));
                    host.tell(
                        profile,
                        News::Published {
                            files,
                            purge_failed,
                        },
                    );
                }
            }
            Ok(HeadlessOutcome::GuardTripped { uploaded, .. }) => {
                if uploaded > 0 {
                    let news = News::Published {
                        files: uploaded,
                        purge_failed: false,
                    };
                    host.tell(profile, news);
                }
            }
            Ok(HeadlessOutcome::DryRun { .. }) => {}
            Err(HeadlessError::Config(why) | HeadlessError::Auth(why) | HeadlessError::Io(why)) => {
                host.tell(profile, News::Error(why));
            }
        }
        if let Some(n) = end.held {
            host.tell(profile, News::HeldBack(n));
        }
        host.tell(profile, News::Ran);
    }

    /// Every [`TICK`]: is the folder still there, and still watched? A folder gone for
    /// [`GONE_GRACE`] stops watching; one that came back (or whose watcher ended) is watched
    /// again, and published once it is quiet.
    async fn look(&mut self) {
        match self.host.folder(self.profile.local_dir.clone()).await {
            Folder::Gone => {
                self.source = None;
                let since = *self.gone_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= GONE_GRACE {
                    self.ending = Some(Stop::FolderGone);
                }
            }
            Folder::Present | Folder::Empty => {
                let back = self.gone_since.take().is_some();
                let ended = !self.source.as_ref().is_some_and(|source| source.alive());
                if self.ending.is_none() && (back || ended) {
                    match self.open() {
                        Ok(()) => self.pending.unknown(Instant::now()),
                        Err(stop) => self.ending = Some(stop),
                    }
                }
            }
        }
    }
}

/// What a change the watcher reported is, or `None` when the skip rules leave it out. `path` is
/// under `root`; `root` itself means the changes overflowed. Looks at the disk (the watcher's
/// thread calls it).
fn classify(root: &Path, rules: &SkipRules, path: &Path) -> Option<Change> {
    let rel = path.strip_prefix(root).ok()?;
    let rel: Vec<_> = rel
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect();
    if rel.is_empty() {
        return Some(Change::Overflow);
    }
    let rel = rel.join("/");
    let found = std::fs::symlink_metadata(path).ok();
    let skipped = match &found {
        Some(meta) => rules.is_skipped(&rel, meta.is_dir()),
        // Gone: a file or a folder, whichever a rule matches.
        None => rules.is_skipped(&rel, false) || rules.is_skipped(&rel, true),
    };
    (!skipped).then_some(Change::File {
        gone: found.is_none(),
    })
}

/// Whether `dir` is there, and has anything in it. Reads the disk.
fn folder_state(dir: &Path) -> Folder {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                Folder::Present
            } else {
                Folder::Empty
            }
        }
        Err(_) => Folder::Gone,
    }
}

// ---- The app's side ----

/// The loop's [`Host`] in the app: the folder on disk, the session and the headless publish.
struct AppHost {
    p: Weak<Publisher>,
    /// The loop's, in [`WatchState`].
    serial: u64,
}

/// A `DirWatcher`; dropped off the runtime's threads, since dropping it waits for its thread.
struct DirSource(Option<win::DirWatcher>);

impl Source for DirSource {
    fn alive(&self) -> bool {
        self.0.as_ref().is_some_and(win::DirWatcher::is_running)
    }
}

impl Drop for DirSource {
    fn drop(&mut self) {
        if let Some(watcher) = self.0.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => drop(rt.spawn_blocking(move || drop(watcher))),
                Err(_) => drop(watcher),
            }
        }
    }
}

impl Host for AppHost {
    fn watch(&self, dir: &Path, rules: SkipRules, tx: Sender) -> io::Result<Box<dyn Source>> {
        let root = dir.to_path_buf();
        let watcher = win::DirWatcher::start(
            dir,
            Box::new(move |path| {
                if let Some(change) = classify(&root, &rules, &path) {
                    let _ = tx.send(Signal::Change(change));
                }
            }),
        )?;
        Ok(Box::new(DirSource(Some(watcher))))
    }

    fn folder(&self, dir: PathBuf) -> impl Future<Output = Folder> + Send + 'static {
        blocking(move || folder_state(&dir))
    }

    fn check(
        &self,
        id: ProfileId,
    ) -> impl Future<Output = Result<ProfileRecord, Stop>> + Send + 'static {
        let p = self.p.clone();
        async move {
            let p = p.upgrade().ok_or(Stop::Off)?;
            let profile = p
                .session
                .profiles()
                .await
                .into_iter()
                .find(|r| r.id == id)
                .ok_or(Stop::Off)?;
            if !profile.extras.watch {
                return Err(Stop::Off);
            }
            let site = p.session.site(&profile.site).ok_or(Stop::SiteGone)?;
            if site_read_only(&p.session, &site.id) {
                return Err(Stop::ReadOnly(site.name));
            }
            Ok(profile)
        }
    }

    fn publish(
        &self,
        profile: ProfileRecord,
        opts: HeadlessOptions,
    ) -> impl Future<Output = Result<HeadlessOutcome, HeadlessError>> + Send + 'static {
        let p = self.p.clone();
        async move {
            let p = p
                .upgrade()
                .ok_or_else(|| HeadlessError::Io(copy::WATCH_CLOSING.to_owned()))?;
            let ctx = HeadlessCtx::from_publisher(&p);
            // The transfer drawer shows the uploads; nothing else follows the progress.
            headless::run_headless(&ctx, &profile, &opts, &|_| {}).await
        }
    }

    fn tell(&self, profile: &ProfileRecord, news: News) {
        let Some(p) = self.p.upgrade() else {
            return;
        };
        {
            let mut w = lock(&p.watch);
            let ours = w
                .loops
                .get(&profile.id)
                .is_some_and(|entry| entry.serial == self.serial);
            match &news {
                News::Watching if ours => {
                    if let Some(entry) = w.loops.get_mut(&profile.id) {
                        entry.active = true;
                    }
                }
                News::Stopped(_) if ours => {
                    w.loops.remove(&profile.id);
                }
                _ => {}
            }
        }
        let about = About {
            id: profile.id.clone(),
            name: profile.name.clone(),
            site: p
                .session
                .site(&profile.site)
                .map(|site| site.name)
                .unwrap_or_default(),
            folder: profile.local_dir.display().to_string(),
        };
        let p2 = p.clone();
        let _ = p
            .link
            .ui()
            .upgrade_in_event_loop(move |ui| show_news(&p2, &ui, &about, news));
    }
}

/// The profile a loop's news is about.
struct About {
    id: ProfileId,
    name: String,
    site: String,
    folder: String,
}

/// Shows a loop's news: the sidebar's mark, the page's note, a toast, the activity log. UI thread.
fn show_news(p: &Arc<Publisher>, ui: &AppWindow, about: &About, news: News) {
    match news {
        News::Watching => {
            if let Some(notes) = lock(&p.watch).notes.get_mut(&about.id) {
                notes.stopped = None;
            }
            show_marks(p, ui);
            show_note(p, ui);
        }
        News::Published {
            files,
            purge_failed,
        } => {
            let kind = if purge_failed {
                ToastKind::Warning
            } else {
                ToastKind::Done
            };
            let text = copy::watch_published(&about.name, files, purge_failed);
            toast::show(ui, kind, text, None);
        }
        News::Failed { failed, files } => {
            let text = copy::watch_failed(&about.name, failed, files);
            toast::show(ui, ToastKind::Warning, text, None);
        }
        News::Error(why) => {
            let text = copy::watch_not_published(&about.name, &why);
            toast::show(ui, ToastKind::Warning, text, None);
        }
        News::HeldBack(n) => {
            lock(&p.watch).notes.entry(about.id.clone()).or_default().held = n;
            show_note(p, ui);
        }
        News::Ran => super::profile_view::refresh(p),
        News::Stopped(Stop::Off) => show_marks(p, ui),
        News::Stopped(stop) => {
            let note = copy::watch_stopped_note(&stop);
            lock(&p.watch)
                .notes
                .entry(about.id.clone())
                .or_default()
                .stopped = Some(note);
            let said = copy::watch_stopped(&about.name, &stop);
            activity_view::event(
                ActivityKind::Publish,
                &about.site,
                &about.folder,
                false,
                said.clone(),
                0,
            );
            // A read-only site is a setting, not news: the page says it.
            if !matches!(stop, Stop::ReadOnly(_)) {
                toast::show(ui, ToastKind::Warning, said, None);
            }
            show_marks(p, ui);
            show_note(p, ui);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use httporter_core::{DeleteGuard, ProfileExtras, PurgeMode, RemotePath, SiteId};
    use tokio::sync::Notify;
    use tokio::time::sleep;

    use super::*;

    const FILE: Change = Change::File { gone: false };
    const REMOVED: Change = Change::File { gone: true };

    fn secs(n: f64) -> Duration {
        Duration::from_secs_f64(n)
    }

    fn profile(unattended: bool, remove_extras: bool) -> ProfileRecord {
        ProfileRecord {
            id: ProfileId("p1".into()),
            site: SiteId("s1".into()),
            name: "Blog".into(),
            local_dir: PathBuf::from(r"C:\site"),
            remote_dir: RemotePath::root(),
            skip_rules: SkipRules::defaults(),
            remove_extras,
            purge: PurgeMode::Off,
            guard: DeleteGuard::default(),
            created_at: chrono::Utc::now(),
            last_published_at: None,
            cache_control: None,
            extras: ProfileExtras {
                watch: true,
                unattended_deletes: unattended,
                schedule: None,
            },
        }
    }

    fn done(uploaded: u64) -> HeadlessOutcome {
        HeadlessOutcome::Done {
            uploaded,
            deleted: 0,
            failed: 0,
            bytes: 0,
            purge: PurgeResult::Off,
        }
    }

    /// The options of a watched publish that deletes nothing.
    fn no_delete() -> HeadlessOptions {
        HeadlessOptions {
            dry_run: false,
            yes: false,
            no_delete: true,
            purge: None,
            source: HistorySource::Watch,
        }
    }

    /// A fake app: a folder whose state the test sets, scripts of publish and dry-run outcomes
    /// (a publish waits for `gate` while there is one), and what was asked and told.
    struct Fake {
        profile: ProfileRecord,
        folder: Mutex<Folder>,
        check: Mutex<Option<Stop>>,
        outcomes: Mutex<VecDeque<HeadlessOutcome>>,
        dry_runs: Mutex<VecDeque<HeadlessOutcome>>,
        gate: Mutex<Option<Arc<Notify>>>,
        alive: Arc<AtomicBool>,
        watches: AtomicU32,
        published: Mutex<Vec<(Instant, HeadlessOptions)>>,
        told: Mutex<Vec<News>>,
    }

    struct FakeSource(Arc<AtomicBool>);

    impl Source for FakeSource {
        fn alive(&self) -> bool {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl Fake {
        fn new(profile: ProfileRecord) -> Arc<Self> {
            Arc::new(Self {
                profile,
                folder: Mutex::new(Folder::Present),
                check: Mutex::new(None),
                outcomes: Mutex::new(VecDeque::new()),
                dry_runs: Mutex::new(VecDeque::new()),
                gate: Mutex::new(None),
                alive: Arc::new(AtomicBool::new(true)),
                watches: AtomicU32::new(0),
                published: Mutex::new(Vec::new()),
                told: Mutex::new(Vec::new()),
            })
        }

        fn script(&self, outcomes: impl IntoIterator<Item = HeadlessOutcome>) {
            self.outcomes.lock().unwrap().extend(outcomes);
        }

        fn set_folder(&self, folder: Folder) {
            *self.folder.lock().unwrap() = folder;
        }

        /// Publishes wait for the gate from now on (`notify_one` lets one go).
        fn hold(&self) -> Arc<Notify> {
            let gate = Arc::new(Notify::new());
            *self.gate.lock().unwrap() = Some(gate.clone());
            gate
        }

        fn runs(&self) -> Vec<HeadlessOptions> {
            let published = self.published.lock().unwrap();
            published.iter().map(|(_, opts)| opts.clone()).collect()
        }

        fn run_times(&self) -> Vec<Instant> {
            let published = self.published.lock().unwrap();
            published.iter().map(|(at, _)| *at).collect()
        }

        fn told(&self) -> Vec<News> {
            self.told.lock().unwrap().clone()
        }

        fn watches(&self) -> u32 {
            self.watches.load(Ordering::SeqCst)
        }
    }

    impl Host for Fake {
        fn watch(&self, _dir: &Path, _rules: SkipRules, _tx: Sender) -> io::Result<Box<dyn Source>> {
            self.watches.fetch_add(1, Ordering::SeqCst);
            self.alive.store(true, Ordering::SeqCst);
            Ok(Box::new(FakeSource(self.alive.clone())))
        }

        fn folder(&self, _dir: PathBuf) -> impl Future<Output = Folder> + Send + 'static {
            std::future::ready(*self.folder.lock().unwrap())
        }

        fn check(
            &self,
            _id: ProfileId,
        ) -> impl Future<Output = Result<ProfileRecord, Stop>> + Send + 'static {
            let answer = match self.check.lock().unwrap().clone() {
                Some(stop) => Err(stop),
                None => Ok(self.profile.clone()),
            };
            std::future::ready(answer)
        }

        fn publish(
            &self,
            _profile: ProfileRecord,
            opts: HeadlessOptions,
        ) -> impl Future<Output = Result<HeadlessOutcome, HeadlessError>> + Send + 'static {
            self.published
                .lock()
                .unwrap()
                .push((Instant::now(), opts.clone()));
            let script = if opts.dry_run {
                &self.dry_runs
            } else {
                &self.outcomes
            };
            let outcome = script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| done(1));
            let gate = self
                .gate
                .lock()
                .unwrap()
                .clone()
                .filter(|_| !opts.dry_run);
            async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                Ok(outcome)
            }
        }

        fn tell(&self, _profile: &ProfileRecord, news: News) {
            self.told.lock().unwrap().push(news);
        }
    }

    /// A loop over a fake that `set` prepared, the loop's sender, and its task.
    async fn bed(
        profile: ProfileRecord,
        set: impl FnOnce(&Fake),
    ) -> (Arc<Fake>, Sender, JoinHandle<()>) {
        let fake = Fake::new(profile.clone());
        set(&fake);
        let (tx, rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(watch_loop(fake.clone(), profile, tx.clone(), rx));
        sleep(secs(0.1)).await;
        (fake, tx, task)
    }

    fn send(tx: &Sender, change: Change) {
        tx.send(Signal::Change(change)).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_save_storm_publishes_once_ten_seconds_after_the_last_change() {
        let (fake, tx, task) = bed(profile(false, false), |_| {}).await;
        assert_eq!(fake.told(), vec![News::Watching]);
        let mut last = Instant::now();
        for _ in 0..50 {
            send(&tx, FILE);
            last = Instant::now();
            sleep(secs(0.1)).await;
        }
        sleep(secs(9.5)).await;
        assert!(fake.runs().is_empty(), "not before 10 s of quiet");
        sleep(secs(1.0)).await;
        assert_eq!(fake.runs(), vec![no_delete()]);
        let at = fake.run_times()[0];
        assert!(
            at >= last + QUIET && at < last + QUIET + secs(0.2),
            "{:?}",
            at - last
        );
        assert_eq!(
            fake.told(),
            vec![
                News::Watching,
                News::Published {
                    files: 1,
                    purge_failed: false
                },
                News::Ran
            ]
        );
        sleep(secs(60.0)).await;
        assert_eq!(fake.runs().len(), 1, "one publish for the storm");
        assert!(!task.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_folder_that_keeps_changing_is_published_after_two_minutes_at_the_latest() {
        let (fake, tx, _task) = bed(profile(false, false), |_| {}).await;
        let first = Instant::now();
        for _ in 0..40 {
            send(&tx, FILE);
            sleep(secs(5.0)).await;
        }
        let at = fake.run_times()[0];
        assert!(at >= first + MAX_WAIT && at < first + MAX_WAIT + secs(0.2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_during_a_run_queues_exactly_one_more() {
        let (fake, tx, _task) = bed(profile(false, false), |_| {}).await;
        let gate = fake.hold();
        send(&tx, FILE);
        sleep(secs(11.0)).await;
        assert_eq!(fake.runs().len(), 1);
        for _ in 0..3 {
            send(&tx, FILE);
            sleep(secs(0.3)).await;
        }
        sleep(secs(30.0)).await;
        assert_eq!(fake.runs().len(), 1, "one run at a time");
        gate.notify_one();
        sleep(secs(0.1)).await;
        assert_eq!(fake.runs().len(), 2, "the queued changes are long quiet: at once");
        gate.notify_one();
        sleep(secs(60.0)).await;
        assert_eq!(fake.runs().len(), 2, "exactly one more");
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_lock_is_tried_again_ten_seconds_later() {
        let (fake, tx, _task) = bed(profile(false, false), |f| {
            f.script([HeadlessOutcome::Busy, done(2)]);
        })
        .await;
        send(&tx, FILE);
        sleep(secs(10.5)).await;
        assert_eq!(fake.runs().len(), 1);
        sleep(secs(10.0)).await;
        let times = fake.run_times();
        assert_eq!(times.len(), 2);
        assert!(times[1] - times[0] >= QUIET);
        assert!(fake.told().contains(&News::Published {
            files: 2,
            purge_failed: false
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn two_overflows_in_a_row_stop_watching_after_one_full_compare() {
        let (fake, tx, task) = bed(profile(false, false), |_| {}).await;
        send(&tx, Change::Overflow);
        send(&tx, FILE);
        send(&tx, Change::Overflow);
        sleep(secs(11.0)).await;
        assert_eq!(
            fake.runs().len(),
            1,
            "an overflow, a change, an overflow: still watching"
        );
        assert!(!task.is_finished());

        send(&tx, Change::Overflow);
        sleep(secs(11.0)).await;
        assert_eq!(fake.runs().len(), 2, "the full compare ran first");
        assert!(task.is_finished());
        assert_eq!(fake.told().last(), Some(&News::Stopped(Stop::Overflow)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_folder_gone_stops_watching_and_publishes_nothing() {
        let (fake, tx, task) = bed(profile(true, true), |_| {}).await;
        send(&tx, REMOVED);
        sleep(secs(1.0)).await;
        fake.set_folder(Folder::Gone);
        sleep(secs(30.0)).await;
        assert!(fake.runs().is_empty());
        assert!(task.is_finished());
        assert_eq!(fake.told().last(), Some(&News::Stopped(Stop::FolderGone)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_folder_gone_at_start_is_not_watched() {
        let (fake, _tx, task) = bed(profile(false, false), |f| f.set_folder(Folder::Gone)).await;
        assert!(task.is_finished());
        assert_eq!(fake.told(), vec![News::Stopped(Stop::FolderGone)]);
        assert_eq!(fake.watches(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_folder_made_again_is_watched_again_and_published() {
        let (fake, tx, task) = bed(profile(false, false), |_| {}).await;
        send(&tx, REMOVED);
        fake.set_folder(Folder::Gone);
        sleep(secs(5.0)).await;
        fake.set_folder(Folder::Present);
        sleep(secs(15.0)).await;
        assert_eq!(fake.watches(), 2);
        assert_eq!(fake.runs().len(), 1);
        assert!(!task.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_folder_is_never_published() {
        let (fake, tx, task) = bed(profile(true, true), |f| f.set_folder(Folder::Empty)).await;
        send(&tx, REMOVED);
        sleep(secs(30.0)).await;
        assert!(fake.runs().is_empty());
        assert!(!task.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn removed_files_wait_for_a_review_and_a_dry_run_counts_them() {
        let (fake, tx, _task) = bed(profile(false, true), |f| {
            f.dry_runs
                .lock()
                .unwrap()
                .push_back(HeadlessOutcome::DryRun {
                    new: 0,
                    changed: 0,
                    delete: 3,
                    bytes: 0,
                });
        })
        .await;
        send(&tx, REMOVED);
        sleep(secs(11.0)).await;
        let runs = fake.runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], no_delete());
        assert!(runs[1].dry_run && !runs[1].yes && !runs[1].no_delete);
        assert!(fake.told().contains(&News::HeldBack(3)));

        // Only changes, nothing removed: no count.
        send(&tx, FILE);
        sleep(secs(11.0)).await;
        assert_eq!(fake.runs().len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tripped_guard_says_how_many_deletes_wait() {
        let (fake, tx, _task) = bed(profile(false, true), |f| {
            f.script([HeadlessOutcome::GuardTripped {
                would_delete: 120,
                uploaded: 4,
            }]);
        })
        .await;
        send(&tx, REMOVED);
        sleep(secs(11.0)).await;
        assert_eq!(fake.runs().len(), 1, "no dry run: the guard counted them");
        let told = fake.told();
        assert!(told.contains(&News::HeldBack(120)));
        assert!(told.contains(&News::Published {
            files: 4,
            purge_failed: false
        }));
    }

    #[tokio::test(start_paused = true)]
    async fn unattended_deletes_go_without_a_review() {
        let (fake, tx, _task) = bed(profile(true, true), |_| {}).await;
        send(&tx, REMOVED);
        sleep(secs(11.0)).await;
        let runs = fake.runs();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].yes && !runs[0].no_delete && !runs[0].dry_run);
        assert_eq!(runs[0].source, HistorySource::Watch);
        assert_eq!(runs[0].purge, None);
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_during_a_run_lets_it_finish_and_runs_nothing_more() {
        let (fake, tx, task) = bed(profile(false, false), |_| {}).await;
        let gate = fake.hold();
        send(&tx, FILE);
        sleep(secs(11.0)).await;
        send(&tx, FILE);
        tx.send(Signal::Stop).unwrap();
        sleep(secs(1.0)).await;
        assert!(!task.is_finished(), "the run under way goes on");
        gate.notify_one();
        sleep(secs(30.0)).await;
        assert_eq!(fake.runs().len(), 1);
        assert!(task.is_finished());
        assert_eq!(fake.told().last(), Some(&News::Stopped(Stop::Off)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_only_site_is_not_watched() {
        let (fake, _tx, task) = bed(profile(false, false), |f| {
            *f.check.lock().unwrap() = Some(Stop::ReadOnly("example-shop.com".into()));
        })
        .await;
        assert!(task.is_finished());
        assert_eq!(
            fake.told(),
            vec![News::Stopped(Stop::ReadOnly("example-shop.com".into()))]
        );
        assert_eq!(fake.watches(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_site_made_read_only_meanwhile_stops_watching_before_publishing() {
        let (fake, tx, task) = bed(profile(false, false), |_| {}).await;
        send(&tx, FILE);
        sleep(secs(5.0)).await;
        *fake.check.lock().unwrap() = Some(Stop::ReadOnly("example-shop.com".into()));
        sleep(secs(10.0)).await;
        assert!(fake.runs().is_empty());
        assert!(task.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn a_watcher_that_ended_is_started_again() {
        let (fake, _tx, task) = bed(profile(false, false), |_| {}).await;
        fake.alive.store(false, Ordering::SeqCst);
        sleep(secs(3.0)).await;
        assert_eq!(fake.watches(), 2);
        sleep(secs(11.0)).await;
        assert_eq!(fake.runs().len(), 1, "what changed meanwhile is published");
        assert!(!task.is_finished());
    }

    #[test]
    fn the_skip_rules_and_the_disk_say_what_a_change_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("index.html"), "x").unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("node_modules").join("a.js"), "x").unwrap();
        std::fs::create_dir(root.join("blog")).unwrap();
        let rules = SkipRules::new(&SkipRules::defaults());
        let at = |rel: &str| classify(root, &rules, &root.join(rel));
        assert_eq!(classify(root, &rules, root), Some(Change::Overflow));
        assert_eq!(at("index.html"), Some(FILE));
        assert_eq!(at("blog"), Some(FILE));
        assert_eq!(at("gone.html"), Some(REMOVED));
        assert_eq!(at(r"blog\gone.html"), Some(REMOVED));
        assert_eq!(at(r"node_modules\a.js"), None);
        assert_eq!(at("node_modules"), None);
        assert_eq!(at(r"blog\Thumbs.db"), None);
        assert_eq!(at("draft.tmp"), None);
        assert_eq!(
            classify(root, &rules, Path::new(r"D:\elsewhere\x")),
            None
        );
    }

    #[test]
    fn a_folder_is_there_empty_or_gone() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(folder_state(dir.path()), Folder::Empty);
        std::fs::write(dir.path().join("a"), "x").unwrap();
        assert_eq!(folder_state(dir.path()), Folder::Present);
        assert_eq!(folder_state(&dir.path().join("missing")), Folder::Gone);
    }

    #[test]
    fn the_note_says_why_watching_stopped_then_what_waits() {
        let notes = Notes {
            held: 3,
            stopped: Some(copy::watch_stopped_note(&Stop::FolderGone)),
        };
        let (text, review) = note_text(&notes);
        assert!(review);
        assert!(text.starts_with("Watching stopped"), "{text}");
        assert!(
            text.ends_with("3 files were removed from the folder."),
            "{text}"
        );
        assert_eq!(note_text(&Notes::default()), (String::new(), false));
    }
}

/// The profile sheet with the watch options, at the window's smallest size.
#[cfg(test)]
mod sheet_tests {
    use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
    use slint::{ComponentHandle, LogicalSize};

    use crate::{AppWindow, ProfileDraft};

    fn sheet_at(width: f32, height: f32, remove_extras: bool) -> AppWindow {
        thread_local! {
            static PLATFORM_SET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        if !PLATFORM_SET.replace(true) {
            i_slint_backend_testing::init_no_event_loop();
        }
        let w = AppWindow::new().unwrap();
        w.window().set_size(LogicalSize::new(width, height));
        w.set_profile_draft(ProfileDraft {
            id: "p1".into(),
            name: "Blog".into(),
            site_name: "example-shop.com".into(),
            folder: r"D:\sites\blog\public".into(),
            skip_rules: ".env\n*.tmp".into(),
            remove_extras,
            purge: 1,
            guard_files: "100".into(),
            guard_percent: "10".into(),
            cdn_note: "Purges pull zone 7654321 with the account key kept.".into(),
            cache_control_note: "Bunny sets caching in the pull zone.".into(),
            watch: true,
            unattended_deletes: true,
            ..ProfileDraft::default()
        });
        w.set_profile_sheet_open(true);
        w.show().unwrap();
        slint::platform::update_timers_and_animations();
        w
    }

    fn labelled(w: &AppWindow, role: AccessibleRole, label: &str) -> ElementHandle {
        let wanted = label.to_owned();
        ElementQuery::from_root(w)
            .match_accessible_role(role)
            .match_predicate(move |e| e.accessible_label().is_some_and(|l| l.as_str() == wanted))
            .find_first()
            .unwrap_or_else(|| panic!("no {role:?} labelled {label:?}"))
    }

    #[test]
    fn the_profile_sheet_fits_the_smallest_window() {
        for (width, height) in [(960.0, 600.0), (760.0, 600.0)] {
            let w = sheet_at(width, height, true);
            for label in ["Delete Profile…", "Cancel", "Save"] {
                let button = labelled(&w, AccessibleRole::Button, label);
                let (p, s) = (button.absolute_position(), button.size());
                assert!(
                    p.y >= 0.0 && p.y + s.height <= height,
                    "{width}×{height}: {label} is cut off: {p:?} {s:?}"
                );
            }
        }
    }

    #[test]
    fn deleting_without_asking_needs_removing_files() {
        let w = sheet_at(960.0, 700.0, false);
        let check = labelled(
            &w,
            AccessibleRole::Checkbox,
            "Delete without asking when publishing automatically",
        );
        assert_eq!(check.accessible_enabled(), Some(false));
        assert_eq!(check.accessible_checked(), Some(false));
        let w = sheet_at(960.0, 700.0, true);
        let check = labelled(
            &w,
            AccessibleRole::Checkbox,
            "Delete without asking when publishing automatically",
        );
        assert_eq!(check.accessible_enabled(), Some(true));
        assert_eq!(check.accessible_checked(), Some(true));
    }
}
