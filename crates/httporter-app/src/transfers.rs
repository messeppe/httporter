//! The transfers: the engine, and everything between it and the window.
//!
//! An upload, a download or a delete is planned on the runtime (the folders it touches listed
//! with the browsing retries, files compared), then, when the conflict policy is Ask and files
//! already exist, the conflict sheet asks once; a delete always asks with the delete alert. What
//! is decided goes to the engine as one batch. Planning and submitting never run on the UI thread;
//! the window hears back through `upgrade_in_event_loop`.
//!
//! The engine's changes reach the window at most ten times a second ([`REDRAW_EVERY`]): the
//! drawer, the banners (offline, sign-in failed, disk full, the journal failing), the taskbar
//! button's progress (redrawn less often still: each redraw is COM work), the notification when a
//! long run ends in the background, and a refresh of any pane that shows a folder a finished batch
//! changed.
//!
//! [`Transfers`] is shared by the UI thread and the runtime's threads, so all it keeps is behind
//! one mutex, held only for short updates and never across an `.await`. The Settings window is
//! the one thing that lives on the UI thread only ([`install`] keeps it).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use httporter_core::{
    Checksum, Direction, EngineSnapshot, EngineState, Entry, Job, JobId, Journal, ProviderError,
    ProviderKind, RemotePath, Resolution, RetryPolicy, SiteId, SiteRecord, StorageProvider,
    TransferSettings, with_retry,
};
use httporter_platform::win::{self, TaskbarProgress, TaskbarState};
use httporter_transfer::{
    Batch, BatchTarget, DownloadPlan, Engine, SiteInfo, UploadPlan, plan_delete, plan_download,
    plan_upload, policy_to_resolution, resolve_downloads, resolve_uploads,
};
use slint::{CloseRequestResponse, ComponentHandle, Model, SharedString};
use tokio::sync::watch;

use crate::binding::Link;
use crate::copy::{self, Attempt};
use crate::drawer_view::{REDRAW_EVERY, Throttle, apply_drawer, drawer_view};
use crate::session::{RemotePlace, Session, SiteProvider, blocking, hub_error_text};
use crate::sheets_view;
use crate::site_hub::{Counting, Interactive};
use crate::{AppWindow, SettingsWindow, settings_view};

/// A run of transfers that took longer than this and ends while HTTPorter is in the background
/// is told in a notification.
const NOTIFY_AFTER: Duration = Duration::from_secs(20);

/// How often the taskbar button's progress may be redrawn while it only moves on: every redraw
/// is COM work. A change of state (paused, stopped, done) is drawn at once.
const TASKBAR_EVERY: Duration = Duration::from_millis(250);

/// How often the drawer shows how far the plans being worked out have got (review M4).
const PLANNING_EVERY: Duration = Duration::from_millis(250);

/// How long closing waits, at most, for the work in flight to stop, the journal to catch up and
/// the uploads left in parts to be given up (review C1). The journal writes its updates in
/// chunks of one transaction each, so 13,793 cancels take well under a second; this is for a
/// disk or a service that stopped answering.
const SETTLE_FOR: Duration = Duration::from_secs(10);

/// Closing that takes longer than this says "Saving…" in the status line meanwhile.
const SAVING_AFTER: Duration = Duration::from_millis(300);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A site and the client a batch for it goes through.
#[derive(Clone)]
struct Target {
    site: SiteRecord,
    provider: Arc<dyn StorageProvider>,
}

/// A plan the conflict sheet is asking about, with where it goes.
enum Planned {
    Upload(UploadPlan, RemotePath),
    Download(DownloadPlan, PathBuf),
}

/// A plan waiting for the conflict sheet's answer.
struct Pending {
    target: Target,
    planned: Planned,
}

/// What Publish adds to the delete alert (T7): its CDN purge. Installed once, by `publish`.
pub trait DeletePurge: Send + Sync {
    /// The alert is up for a delete on `site`: offer the purge (the checkbox and its note).
    fn offer(&self, site: &SiteRecord);
    /// A delete with the purge ticked went to the engine as tracked batch `batch`: purge what it
    /// removed once all of it is done. The delete never waits for this.
    fn after(&self, site: SiteRecord, batch: String);
}

/// A delete waiting for the delete alert's answer.
struct PendingDelete {
    target: Target,
    /// The folder the deleted items are in.
    dir: RemotePath,
    jobs: Vec<Job>,
}

/// Where the New Folder sheet makes its folder: the pane that was in use when it opened.
#[derive(Clone)]
enum FolderPlace {
    Cloud(RemotePlace),
    Local(PathBuf),
}

/// A folder a submitted batch changes, so a pane showing it is listed again when the batch ends.
#[derive(Clone, Debug)]
enum Affected {
    Remote(SiteId, RemotePath),
    Local(PathBuf),
}

/// How the window is being closed while transfers run (the quit alert's answer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quit {
    /// Stop; the journal keeps everything for a resume.
    Pause,
    /// Cancel everything not done, then stop.
    Cancel,
}

/// What the engine's feed remembers between two redraws.
struct Feed {
    /// When the engine last went from idle to busy; `None` while idle.
    run_started: Option<Instant>,
    /// When the last batch the window heard of finished.
    last_finished: Option<DateTime<Utc>>,
    /// The window's handle, once it has one (0 before).
    hwnd: isize,
    /// The taskbar button's progress, once made; `taskbar_tried` once that was tried.
    taskbar: Option<TaskbarProgress>,
    taskbar_tried: bool,
    taskbar_shown: Option<TaskbarState>,
    taskbar_pace: Throttle,
    /// How many cut-off uploads the status line has said were sent again (ruling Q55).
    resent_told: u64,
}

impl Default for Feed {
    fn default() -> Self {
        Self {
            run_started: None,
            last_finished: None,
            hwnd: 0,
            taskbar: None,
            taskbar_tried: false,
            taskbar_shown: None,
            taskbar_pace: Throttle::new(TASKBAR_EVERY),
            resent_told: 0,
        }
    }
}

/// A question that waits for the one on screen to be answered (review I6): a second plan that
/// needs the conflict sheet or the delete alert is never lost, nor shown over the first.
enum Waiting {
    Conflicts {
        /// When the user asked for it ([`Transfers::next_asked`]).
        asked: u64,
        pending: Pending,
        view: Box<sheets_view::ConflictSheetView>,
        note: Option<String>,
    },
    Delete {
        asked: u64,
        pending: PendingDelete,
        /// Its title, text and button.
        alert: (String, String, String),
    },
}

impl Waiting {
    fn asked(&self) -> u64 {
        match self {
            Self::Conflicts { asked, .. } | Self::Delete { asked, .. } => *asked,
        }
    }

    fn site(&self) -> &SiteId {
        match self {
            Self::Conflicts { pending, .. } => &pending.target.site.id,
            Self::Delete { pending, .. } => &pending.target.site.id,
        }
    }
}

/// What a plan being worked out is for, and how far it has got (review M4).
struct Planning {
    /// One of the `copy::PREPARING_*` lines.
    what: &'static str,
    /// The entries its listings have found so far.
    found: Arc<AtomicU64>,
    /// Stops it; set once its task is spawned.
    abort: Option<tokio::task::AbortHandle>,
}

#[derive(Default)]
struct State {
    conflict: Option<Pending>,
    delete: Option<PendingDelete>,
    /// Questions to ask after the one on screen, in the order the user asked them in (not the
    /// order their plans were ready in).
    waiting: VecDeque<Waiting>,
    /// The plans being worked out, by when they were asked.
    planning: HashMap<u64, Planning>,
    /// A task redraws the drawer while plans are worked out.
    planning_shown: bool,
    new_folder: Option<FolderPlace>,
    affected: Vec<Affected>,
    feed: Feed,
    quitting: Option<Quit>,
    /// When closing stops waiting, once set (see `Transfers::closing_deadline`).
    closing_by: Option<Instant>,
}

impl State {
    /// The conflict sheet or the delete alert is up.
    fn asking(&self) -> bool {
        self.conflict.is_some() || self.delete.is_some()
    }

    /// Puts a question in line by when it was asked, so a quick plan asked later doesn't jump
    /// ahead of a slow one asked first.
    fn wait(&mut self, waiting: Waiting) {
        let at = self
            .waiting
            .iter()
            .position(|other| other.asked() > waiting.asked())
            .unwrap_or(self.waiting.len());
        self.waiting.insert(at, waiting);
    }

    /// The drawer's line for the plans being worked out: the first one asked, and how far it
    /// has got.
    fn planning_text(&self) -> Option<String> {
        let first = self.planning.keys().min()?;
        let planning = &self.planning[first];
        Some(copy::preparing(
            planning.what,
            planning.found.load(Ordering::Relaxed),
        ))
    }
}

/// The transfer engine and what it is waiting on the user for. Shared as `Arc<Transfers>` by the
/// window's callbacks and the tasks that plan.
pub struct Transfers {
    engine: Engine,
    journal: Arc<dyn Journal>,
    session: Arc<Session>,
    link: Link,
    /// A file or folder picker is open. The pickers have no owner window, so the main window
    /// stays usable while one is open; a second click on Upload or Download to… is ignored.
    picking: Arc<AtomicBool>,
    state: Mutex<State>,
    /// Held while new settings are handed to the engine, so that an older change never lands
    /// after a newer one.
    applying: Mutex<()>,
    /// Goes up with every change of settings: the clients built for one are handed to the
    /// engine only while no other change came since (see `apply_settings`).
    settings_generation: AtomicU64,
    /// Counts what the user asked for (an upload, a download, a delete), to keep the questions
    /// in that order.
    asked: AtomicU64,
    /// Publish's purge for the delete alert, once installed.
    delete_purge: std::sync::OnceLock<Arc<dyn DeletePurge>>,
}

/// Marks the picker closed when dropped (also if the picker panicked).
struct PickerOpen(Arc<AtomicBool>);

impl Drop for PickerOpen {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Transfers {
    pub fn new(
        engine: Engine,
        journal: Arc<dyn Journal>,
        session: Arc<Session>,
        link: Link,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            journal,
            session,
            link,
            picking: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(State::default()),
            applying: Mutex::new(()),
            settings_generation: AtomicU64::new(0),
            asked: AtomicU64::new(0),
            delete_purge: std::sync::OnceLock::new(),
        })
    }

    /// The next number for something the user asked for.
    fn next_asked(&self) -> u64 {
        self.asked.fetch_add(1, Ordering::SeqCst)
    }

    /// Works out a plan (`plan`, given what its listings find) on the runtime, shown in the
    /// drawer with how far it has got, where Cancel stops it (review M4).
    fn plan(
        self: &Arc<Self>,
        asked: u64,
        what: &'static str,
        plan: impl FnOnce(Arc<AtomicU64>) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
    ) {
        let found = Arc::new(AtomicU64::new(0));
        lock(&self.state).planning.insert(
            asked,
            Planning {
                what,
                found: found.clone(),
                abort: None,
            },
        );
        let work = plan(found);
        let this = self.clone();
        let task = self.link.rt().spawn(async move {
            work.await;
            lock(&this.state).planning.remove(&asked);
        });
        if let Some(planning) = lock(&self.state).planning.get_mut(&asked) {
            planning.abort = Some(task.abort_handle());
        }
        self.show_planning();
    }

    /// Redraws the drawer every [`PLANNING_EVERY`] while plans are worked out, and once more
    /// when the last one ends.
    fn show_planning(self: &Arc<Self>) {
        {
            let mut state = lock(&self.state);
            if state.planning_shown {
                return;
            }
            state.planning_shown = true;
        }
        let this = self.clone();
        self.link.rt().spawn(async move {
            loop {
                tokio::time::sleep(PLANNING_EVERY).await;
                let more = {
                    let mut state = lock(&this.state);
                    state.planning_shown = !state.planning.is_empty();
                    state.planning_shown
                };
                let tick = this.clone();
                let shown = this
                    .link
                    .ui()
                    .upgrade_in_event_loop(move |ui| tick.tick(&ui))
                    .is_ok();
                if !more || !shown {
                    return;
                }
            }
        });
    }

    /// The drawer's Cancel: the plans being worked out stop, before anything of theirs is
    /// transferred.
    fn cancel_planning(&self) {
        let stopped: Vec<Planning> = lock(&self.state)
            .planning
            .drain()
            .map(|(_, planning)| planning)
            .collect();
        if stopped.is_empty() {
            return;
        }
        for planning in stopped {
            if let Some(abort) = planning.abort {
                abort.abort();
            }
        }
        self.link.status(copy::PREPARING_CANCELLED.to_owned());
    }

    /// The site of `place` with the client a batch for it should use: the hub's current one (new
    /// settings may have replaced the one the pane opened the site with).
    async fn target(&self, place: RemotePlace) -> Target {
        let RemotePlace { site, provider, .. } = place;
        let provider = if self.session.hub().is_current(&site.id, &provider) {
            provider
        } else {
            self.session.connect(&site).await.unwrap_or(provider)
        };
        Target { site, provider }
    }

    /// The transfer engine, for Publish's batches.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Where a publish's batches for `site` go: the site with the client a batch for it should
    /// use (as [`Self::target`] picks it), or `None` when no client can be made.
    pub async fn batch_target(&self, site: &SiteRecord) -> Option<BatchTarget> {
        let provider = match self.session.remote_place() {
            Some(place) if place.site.id == site.id => self.target(place).await.provider,
            _ => self.session.connect(site).await.ok()?,
        };
        Some(BatchTarget {
            site: site.id.clone(),
            site_name: site.name.clone(),
            host: site.host.clone(),
            provider,
        })
    }

    /// Opens a picker on a blocking thread (it blocks until it is closed) and hands its answer to
    /// `then` there, unless a picker is open already.
    fn pick<T: Send + 'static>(
        &self,
        ask: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(T) + Send + 'static,
    ) {
        if self.picking.swap(true, Ordering::SeqCst) {
            return;
        }
        let open = PickerOpen(self.picking.clone());
        self.link.rt().spawn_blocking(move || {
            let answer = ask();
            drop(open);
            then(answer);
        });
    }

    /// Journals `jobs` as one batch for `target` and starts them. A batch the journal can't record
    /// is not started, and the status line says so; `note` (what was skipped) goes there
    /// otherwise.
    async fn submit(
        &self,
        target: Target,
        jobs: Vec<Job>,
        checksums: HashMap<RemotePath, Checksum>,
        affected: Affected,
        note: Option<String>,
    ) {
        if jobs.is_empty() {
            self.link
                .status(note.unwrap_or_else(|| copy::NOTHING_TO_TRANSFER.to_owned()));
            return;
        }
        // Removed while it was planned or asked about: its batch would run on a site the
        // sidebar no longer has.
        if self.session.site(&target.site.id).is_none() {
            self.link
                .status(copy::transfer_site_gone(&target.site.name));
            return;
        }
        let batch = Batch {
            site: target.site.id.clone(),
            site_name: target.site.name.clone(),
            host: target.site.host.clone(),
            provider: target.provider,
            jobs,
            checksums,
        };
        let engine = self.engine.clone();
        // The journal is written before anything starts: a disk write, so off the runtime.
        match blocking(move || engine.submit(batch)).await {
            Ok(_) => {
                lock(&self.state).affected.push(affected);
                self.link.status(note.unwrap_or_default());
            }
            Err(e) => self.link.status(copy::transfer_not_started(e.0)),
        }
    }

    // ---- Upload ----

    /// Uploads `sources` (files and folders on this PC) into `folder` of the site open in the
    /// cloud pane, or into the folder it shows.
    pub fn upload(self: &Arc<Self>, sources: Vec<PathBuf>, folder: Option<RemotePath>) {
        if sources.is_empty() {
            return;
        }
        let Some(place) = self.session.remote_place() else {
            return;
        };
        let dir = folder.unwrap_or_else(|| place.dir.clone());
        self.link.status(copy::PREPARING_UPLOAD.to_owned());
        let asked = self.next_asked();
        let this = self.clone();
        self.plan(asked, copy::PREPARING_UPLOAD, move |found| {
            Box::pin(async move {
                this.plan_upload(place, sources, dir, asked, found).await;
            })
        });
    }

    /// Files dropped from Explorer on the cloud pane: into its folder row `row` (when that is a
    /// folder), or the folder it shows.
    pub fn upload_dropped(self: &Arc<Self>, paths: Vec<PathBuf>, row: Option<usize>) {
        let folder = row.and_then(|row| self.session.remote_folder_at(row));
        self.upload(paths, folder);
    }

    /// Upload with nothing selected on This PC: asks for the files.
    pub fn pick_and_upload(self: &Arc<Self>) {
        let this = self.clone();
        self.pick(
            || win::pick_files(copy::PICK_FILES_TITLE),
            move |files| this.upload(files, None),
        );
    }

    /// Ctrl+V in the cloud pane: uploads the files on the clipboard into the folder it shows.
    pub fn paste(self: &Arc<Self>) {
        let files = win::clipboard_files();
        if files.is_empty() {
            self.link.status(copy::NOTHING_TO_PASTE.to_owned());
        } else {
            self.upload(files, None);
        }
    }

    async fn plan_upload(
        &self,
        place: RemotePlace,
        sources: Vec<PathBuf>,
        dir: RemotePath,
        asked: u64,
        found: Arc<AtomicU64>,
    ) {
        let target = self.target(place).await;
        let site = target.site.clone();
        let planner = Counting {
            inner: Interactive(target.provider.clone()),
            found,
        };
        let plan = match plan_upload(&planner, &sources, &dir).await {
            Ok(plan) => plan,
            Err(e) => {
                let text = copy::describe_for(
                    site.provider,
                    &e,
                    &site.container,
                    &site.host,
                    Attempt::List(&dir),
                );
                self.link.status(text);
                return;
            }
        };
        let note = skipped_note(&plan.skipped);
        let policy = self.session.conflict_policy(&site.id, true).await;
        let all = match policy_to_resolution(policy) {
            Some(all) => all,
            // Nothing to ask about: the resolution touches no file.
            None if plan.conflicts.is_empty() => Resolution::Replace,
            None => return self.ask(target, Planned::Upload(plan, dir), note, asked),
        };
        let jobs = resolve_uploads(plan, all, &HashMap::new());
        let affected = Affected::Remote(site.id, dir);
        self.submit(target, jobs, HashMap::new(), affected, note)
            .await;
    }

    // ---- Download ----

    /// Downloads `entries` (files and folders of the site open in the cloud pane) into `local`,
    /// or into a folder asked for when there is none.
    pub fn download(self: &Arc<Self>, entries: Vec<Entry>, local: Option<PathBuf>) {
        if entries.is_empty() {
            return;
        }
        let Some(place) = self.session.remote_place() else {
            return;
        };
        match local {
            Some(dir) => self.start_download(place, entries, dir),
            None => {
                let this = self.clone();
                self.pick(
                    || win::pick_folder(copy::PICK_FOLDER_TITLE),
                    move |dir| {
                        if let Some(dir) = dir {
                            this.start_download(place, entries, dir);
                        }
                    },
                );
            }
        }
    }

    fn start_download(self: &Arc<Self>, place: RemotePlace, entries: Vec<Entry>, dir: PathBuf) {
        self.link.status(copy::PREPARING_DOWNLOAD.to_owned());
        let asked = self.next_asked();
        let this = self.clone();
        self.plan(asked, copy::PREPARING_DOWNLOAD, move |found| {
            Box::pin(async move {
                this.plan_download(place, entries, dir, asked, found).await;
            })
        });
    }

    async fn plan_download(
        &self,
        place: RemotePlace,
        entries: Vec<Entry>,
        dir: PathBuf,
        asked: u64,
        found: Arc<AtomicU64>,
    ) {
        let from = place.dir.clone();
        let target = self.target(place).await;
        let site = target.site.clone();
        let planner = Counting {
            inner: Interactive(target.provider.clone()),
            found,
        };
        let plan = match plan_download(&planner, &entries, &dir).await {
            Ok(plan) => plan,
            Err(e) => {
                let text = copy::describe_for(
                    site.provider,
                    &e,
                    &site.container,
                    &site.host,
                    Attempt::List(&from),
                );
                self.link.status(text);
                return;
            }
        };
        // Names Windows can't store, or that clash ignoring case, are changed: the status line
        // says so (review I7).
        let note = renamed_note(&plan.renamed);
        let policy = self.session.conflict_policy(&site.id, false).await;
        let all = match policy_to_resolution(policy) {
            Some(all) => all,
            None if plan.conflicts.is_empty() => Resolution::Replace,
            None => return self.ask(target, Planned::Download(plan, dir), note, asked),
        };
        // What arrives is checked against the listing (ruling Q27).
        let checksums = plan.checksums.clone();
        let jobs = resolve_downloads(plan, all, &HashMap::new());
        self.submit(target, jobs, checksums, Affected::Local(dir), note)
            .await;
    }

    // ---- The conflict sheet ----

    /// Shows the conflict sheet for `planned` and keeps it until the sheet is answered. While
    /// another question is up, it waits its turn (review I6).
    fn ask(&self, target: Target, planned: Planned, note: Option<String>, asked: u64) {
        let short = match SiteProvider::of(&target.site) {
            SiteProvider::Bunny => copy::BUNNY,
            SiteProvider::S3(preset) => preset.label(),
        };
        let site = &target.site.name;
        let view = match &planned {
            Planned::Upload(plan, dir) => sheets_view::conflict_sheet(
                Direction::Upload,
                &plan.conflicts,
                plan.files,
                &copy::remote_folder_label(site, dir),
                site,
                short,
            ),
            Planned::Download(plan, dir) => sheets_view::conflict_sheet(
                Direction::Download,
                &plan.conflicts,
                plan.files,
                &copy::local_folder_label(dir),
                site,
                short,
            ),
        };
        let pending = Pending { target, planned };
        {
            let mut state = lock(&self.state);
            if state.asking() {
                state.wait(Waiting::Conflicts {
                    asked,
                    pending,
                    view: Box::new(view),
                    note,
                });
                drop(state);
                self.link.status(copy::ASKED_NEXT.to_owned());
                return;
            }
            state.conflict = Some(pending);
        }
        self.show_conflicts(view, note);
    }

    fn show_conflicts(&self, view: sheets_view::ConflictSheetView, note: Option<String>) {
        let _ = self.link.ui().upgrade_in_event_loop(move |ui| {
            sheets_view::apply_conflicts(&ui, &view);
            ui.set_conflicts_open(true);
            crate::binding::show_status(&ui, None, note.unwrap_or_default());
        });
    }

    fn show_delete_alert(&self, alert: (String, String, String)) {
        let _ = self.link.ui().upgrade_in_event_loop(move |ui| {
            crate::binding::show_status(&ui, None, String::new());
            // The CDN purge starts off; Publish offers it below, for the site it deletes from.
            ui.set_delete_purge(false);
            ui.set_delete_purge_available(false);
            sheets_view::show_delete_alert(&ui, &alert);
        });
        let site = lock(&self.state)
            .delete
            .as_ref()
            .map(|pending| pending.target.site.clone());
        if let (Some(site), Some(purge)) = (site, self.delete_purge.get()) {
            purge.offer(&site);
        }
    }

    /// The question on screen was answered: the next one waiting, if any, is asked.
    fn ask_next(&self) {
        let show = {
            let mut state = lock(&self.state);
            if state.asking() {
                return;
            }
            match state.waiting.pop_front() {
                None => return,
                Some(Waiting::Conflicts {
                    pending,
                    view,
                    note,
                    ..
                }) => {
                    state.conflict = Some(pending);
                    Ok((view, note))
                }
                Some(Waiting::Delete { pending, alert, .. }) => {
                    state.delete = Some(pending);
                    Err(alert)
                }
            }
        };
        match show {
            Ok((view, note)) => self.show_conflicts(*view, note),
            Err(alert) => self.show_delete_alert(alert),
        }
    }

    /// Continue on the conflict sheet: the default action `mode`, each row's own choice, and
    /// whether to remember the default for the site.
    fn conflicts_continue(self: &Arc<Self>, ui: &AppWindow, mode: i32, remember: bool) {
        let Some(Pending { target, planned }) = lock(&self.state).conflict.take() else {
            return;
        };
        let per_row: HashMap<usize, Resolution> = ui
            .get_conflicts_rows()
            .iter()
            .enumerate()
            .map(|(index, row)| (index, sheets_view::action_resolution(row.action)))
            .collect();
        let all = sheets_view::mode_resolution(mode);
        let this = self.clone();
        self.link.rt().spawn(async move {
            let upload = matches!(planned, Planned::Upload(..));
            if remember {
                let policy = sheets_view::mode_policy(mode);
                let site = &target.site.id;
                if let Err(e) = this.session.remember_conflicts(site, upload, policy).await {
                    this.link.status(copy::choice_not_remembered(e));
                }
            }
            match planned {
                Planned::Upload(plan, dir) => {
                    let jobs = resolve_uploads(plan, all, &per_row);
                    let affected = Affected::Remote(target.site.id.clone(), dir);
                    this.submit(target, jobs, HashMap::new(), affected, None)
                        .await;
                }
                Planned::Download(plan, dir) => {
                    let checksums = plan.checksums.clone();
                    let jobs = resolve_downloads(plan, all, &per_row);
                    this.submit(target, jobs, checksums, Affected::Local(dir), None)
                        .await;
                }
            }
        });
        self.ask_next();
    }

    fn conflicts_cancel(&self) {
        lock(&self.state).conflict = None;
        self.ask_next();
    }

    // ---- Delete ----

    /// Deletes `entries` of the site open in the cloud pane, once the delete alert, which says
    /// how much goes, has been answered.
    pub fn delete(self: &Arc<Self>, entries: Vec<Entry>) {
        if entries.is_empty() {
            return;
        }
        let Some(place) = self.session.remote_place() else {
            return;
        };
        self.link.status(copy::PREPARING_DELETE.to_owned());
        let asked = self.next_asked();
        let this = self.clone();
        self.plan(asked, copy::PREPARING_DELETE, move |found| {
            Box::pin(async move {
                let dir = place.dir.clone();
                let target = this.target(place).await;
                let site = target.site.clone();
                let planner = Counting {
                    inner: Interactive(target.provider.clone()),
                    found,
                };
                let plan = match plan_delete(&planner, &entries).await {
                    Ok(plan) => plan,
                    Err(e) => {
                        let text = copy::describe_for(
                            site.provider,
                            &e,
                            &site.container,
                            &site.host,
                            Attempt::List(&dir),
                        );
                        this.link.status(text);
                        return;
                    }
                };
                let alert = sheets_view::delete_alert(&plan.summary, &site.name, site.provider);
                let pending = PendingDelete {
                    target,
                    dir,
                    jobs: plan.jobs,
                };
                {
                    // A question already up keeps its place; this one waits (review I6).
                    let mut state = lock(&this.state);
                    if state.asking() {
                        state.wait(Waiting::Delete {
                            asked,
                            pending,
                            alert,
                        });
                        drop(state);
                        this.link.status(copy::ASKED_NEXT.to_owned());
                        return;
                    }
                    state.delete = Some(pending);
                }
                this.show_delete_alert(alert);
            })
        });
    }

    /// Publish's CDN purge for the delete alert (`publish::install` sets it).
    pub fn set_delete_purge(&self, purge: Arc<dyn DeletePurge>) {
        let _ = self.delete_purge.set(purge);
    }

    fn delete_confirm(self: &Arc<Self>) {
        let Some(PendingDelete { target, dir, jobs }) = lock(&self.state).delete.take() else {
            return;
        };
        // "Also purge these URLs from the CDN": read while the alert's answer is fresh (this runs
        // on the UI thread, from the alert's button).
        let purge = self
            .link
            .ui()
            .upgrade()
            .is_some_and(|ui| ui.get_delete_purge() && ui.get_delete_purge_available())
            .then(|| self.delete_purge.get().cloned())
            .flatten();
        let this = self.clone();
        self.link.rt().spawn(async move {
            let affected = Affected::Remote(target.site.id.clone(), dir);
            match purge {
                Some(purge) if !jobs.is_empty() => {
                    this.submit_purged_delete(target, jobs, affected, purge)
                        .await;
                }
                _ => {
                    this.submit(target, jobs, HashMap::new(), affected, None)
                        .await;
                }
            }
        });
        self.ask_next();
    }

    /// As [`Self::submit`] for a delete, tracked, so that Publish can purge what it removed once
    /// it is all done (including the late checks, ruling Q55).
    async fn submit_purged_delete(
        &self,
        target: Target,
        jobs: Vec<Job>,
        affected: Affected,
        purge: Arc<dyn DeletePurge>,
    ) {
        let site = target.site.clone();
        let batch = Batch {
            site: site.id.clone(),
            site_name: site.name.clone(),
            host: site.host.clone(),
            provider: target.provider,
            jobs,
            checksums: HashMap::new(),
        };
        let engine = self.engine.clone();
        match blocking(move || engine.submit_tracked(batch)).await {
            Ok(id) => {
                lock(&self.state).affected.push(affected);
                self.link.status(String::new());
                purge.after(site, id);
            }
            Err(e) => self.link.status(copy::transfer_not_started(e.0)),
        }
    }

    fn delete_cancel(&self) {
        lock(&self.state).delete = None;
        self.ask_next();
    }

    // ---- New Folder ----

    /// Opens the New Folder sheet for the cloud pane (`cloud`) or This PC.
    pub fn new_folder(&self, ui: &AppWindow, cloud: bool) {
        let place = if cloud {
            self.session.remote_place().map(FolderPlace::Cloud)
        } else {
            self.session.local_dir().map(FolderPlace::Local)
        };
        let Some(place) = place else {
            return;
        };
        lock(&self.state).new_folder = Some(place);
        ui.set_new_folder_name(copy::NEW_FOLDER_NAME.into());
        ui.set_new_folder_error(SharedString::new());
        ui.set_new_folder_open(true);
    }

    /// Create on the New Folder sheet: checks the name against the pane's items, then makes the
    /// folder (and closes the sheet) or says what is wrong under the field.
    fn new_folder_create(self: &Arc<Self>, ui: &AppWindow, name: &str) {
        let place = lock(&self.state).new_folder.clone();
        let Some(place) = place else {
            ui.set_new_folder_open(false);
            return;
        };
        let rows = match place {
            FolderPlace::Cloud(_) => ui.get_remote_rows(),
            FolderPlace::Local(_) => ui.get_local_rows(),
        };
        let existing: HashSet<String> = rows.iter().map(|row| row.name.to_string()).collect();
        if let Some(error) = sheets_view::new_folder_error(name, &existing) {
            ui.set_new_folder_error(error.into());
            return;
        }
        let name = name.trim().to_owned();
        lock(&self.state).new_folder = None;
        ui.set_new_folder_open(false);
        let this = self.clone();
        self.link.rt().spawn(async move {
            match place {
                FolderPlace::Cloud(place) => this.create_remote_folder(place, name).await,
                FolderPlace::Local(dir) => this.create_local_folder(dir, name).await,
            }
        });
    }

    fn new_folder_cancel(&self) {
        lock(&self.state).new_folder = None;
    }

    /// Makes the folder `name` in the cloud pane's folder. Bunny Storage keeps no empty folder:
    /// there the folder is shown, with a note, until a file put in it makes it real.
    async fn create_remote_folder(&self, place: RemotePlace, name: String) {
        let dir = place.dir.clone();
        let target = self.target(place).await;
        let path = match dir.join(&name) {
            Ok(path) => path,
            Err(e) => return self.link.status(copy::folder_not_created(&name, e)),
        };
        let provider = target.provider.clone();
        let made = with_retry(&RetryPolicy::interactive(), |_| {
            provider.create_folder(&path)
        })
        .await;
        match made {
            Ok(()) => self.link.refresh_remote_at(&target.site.id, &dir),
            Err(ProviderError::Refused(_)) if target.site.provider == ProviderKind::Bunny => {
                let view = self.session.add_virtual_folder(&target.site.id, path);
                self.link.show_remote(view);
            }
            Err(e) => self.link.status(copy::folder_not_created(
                &name,
                copy::describe_transfer_error(&e),
            )),
        }
    }

    async fn create_local_folder(&self, dir: PathBuf, name: String) {
        let path = dir.join(&name);
        match blocking(move || std::fs::create_dir(&path)).await {
            Ok(()) => self.link.refresh_local_at(&dir),
            Err(e) => self.link.status(copy::folder_not_created(&name, e)),
        }
    }

    // ---- Resuming what a crash or a quit left ----

    /// Shows the resume banner when the journal holds transfers that didn't finish, naming the
    /// deletes among them (review C1).
    fn offer_resume(&self, ui: &AppWindow) {
        let left = self.engine.unfinished();
        let text = if left.total > 0 {
            sheets_view::resume_banner(left.total, left.deletes)
        } else {
            String::new()
        };
        ui.set_resume_banner_text(text.into());
    }

    /// Resume on the resume banner: the unfinished jobs go back to their sites' queues, each
    /// site on its own (review M14). A site that still exists but can't be opened (its password
    /// can't be read) keeps its jobs as they are, and the banner offers them again: resuming
    /// without it would cancel them. The other sites' jobs go on.
    fn resume_journal(self: &Arc<Self>, ui: &AppWindow) {
        ui.set_resume_banner_text(SharedString::new());
        let this = self.clone();
        self.link.rt().spawn_blocking(move || {
            // The engine counts a journal it can't read; then there is nothing to resume.
            let records = this.journal.unfinished().unwrap_or_default();
            let mut sites: HashMap<SiteId, Option<SiteInfo>> = HashMap::new();
            let mut kept: HashSet<SiteId> = HashSet::new();
            for record in &records {
                if sites.contains_key(&record.site) || kept.contains(&record.site) {
                    continue;
                }
                let info = match this.session.site(&record.site) {
                    // A site removed since: its jobs are cancelled.
                    None => None,
                    Some(site) => match this.session.hub().provider(&site) {
                        Ok(provider) => Some((site.name.clone(), site.host.clone(), provider)),
                        Err(e) => {
                            let reason = hub_error_text(&e, &site);
                            this.link
                                .status(copy::resume_needs_site(&site.name, &reason));
                            kept.insert(record.site.clone());
                            continue;
                        }
                    },
                };
                sites.insert(record.site.clone(), info);
            }
            let lookup = |id: &SiteId| sites.get(id).cloned().flatten();
            this.engine.resume_from_journal_except(&lookup, &kept);
            if !kept.is_empty() {
                let again = this.clone();
                let _ = this
                    .link
                    .ui()
                    .upgrade_in_event_loop(move |ui| again.offer_resume(&ui));
            }
        });
    }

    // ---- Sites that change or go ----

    /// The site the sign-in banner is about (for its Edit Site…).
    pub fn auth_stopped_site(&self) -> Option<SiteId> {
        self.engine.auth_stopped_site()
    }

    /// `site` was edited and saved (the hub has dropped its old client). Its transfers go on
    /// with the new client (review M2): the queued ones of a running site, and all of them when a
    /// failed sign-in had stopped it.
    pub async fn site_edited(&self, site: &SiteRecord) {
        if !self.engine.sites().contains(&site.id) {
            return;
        }
        if let Ok(provider) = self.session.connect(site).await {
            if self.engine.is_auth_stopped(&site.id) {
                self.engine.resume_site(&site.id, provider);
            } else {
                self.engine.replace_provider(&site.id, provider);
            }
        }
    }

    /// Resume on the clock banner (review I4): the clock has been set, so the site stopped for
    /// it goes on, with the client it had.
    fn resume_clock(&self) {
        if let Some((site, provider)) = self.engine.clock_skewed_site() {
            self.engine.resume_site(&site, provider);
        }
    }

    /// `site` is being removed (Ruling Q23): its transfers are cancelled, and its records leave
    /// the journal (the unfinished ones a crash left too). The uploads it had left in parts are
    /// given up with its client (ruling Q49), so this runs before its password goes. Blocks (the
    /// journal, the site's client): call it off the UI thread.
    ///
    /// It returns as soon as the cancels are queued for the engine's journal writer, which
    /// closing waits for (review N2), so the site leaves the sidebar at once. Once the site's
    /// transfers have stopped and the journal has their cancels, the cancelled records go and
    /// the resume banner counts what is left.
    pub fn forget_site(self: &Arc<Self>, site: &SiteId) {
        // Its questions still waiting would ask about a site that is gone.
        lock(&self.state)
            .waiting
            .retain(|waiting| waiting.site() != site);
        let provider = self.engine.site_provider(site).or_else(|| {
            let record = self.session.site(site)?;
            self.session.hub().provider(&record).ok()
        });
        self.engine.forget_site(site, provider);
        let this = self.clone();
        let site = site.clone();
        self.link.rt().spawn_blocking(move || {
            let deadline = Instant::now() + SETTLE_FOR;
            wait_until(deadline, || !this.engine.site_has_work(&site));
            let mark = this.engine.journal_mark();
            wait_until(deadline, || this.engine.journal_reached(mark));
            // The cancelled records go, with the other finished ones (as at every start).
            let _ = this.journal.clear_finished();
            let again = this.clone();
            let _ = this
                .link
                .ui()
                .upgrade_in_event_loop(move |ui| again.offer_resume(&ui));
        });
    }

    /// Discard on the resume banner. The uploads left in parts are given up on their sites
    /// (ruling Q49), through the clients of the sites that can still be opened.
    fn discard_journal(self: &Arc<Self>, ui: &AppWindow) {
        ui.set_resume_banner_text(SharedString::new());
        let this = self.clone();
        self.link.rt().spawn_blocking(move || {
            let records = this.journal.unfinished().unwrap_or_default();
            let mut sites: HashMap<SiteId, Option<SiteInfo>> = HashMap::new();
            // Only the sites with an upload left in parts need their client.
            let in_parts = records
                .iter()
                .filter(|record| matches!(record.job, Job::Upload { .. }))
                .filter(|record| record.multipart.is_some());
            for record in in_parts {
                sites.entry(record.site.clone()).or_insert_with(|| {
                    let site = this.session.site(&record.site)?;
                    let provider = this.session.hub().provider(&site).ok()?;
                    Some((site.name.clone(), site.host.clone(), provider))
                });
            }
            let lookup = |id: &SiteId| sites.get(id).cloned().flatten();
            this.engine.discard_journal(&lookup);
        });
    }

    // ---- The engine's feed ----

    /// Redraws the window from the engine's changes, at most every [`REDRAW_EVERY`].
    fn spawn_feed(self: &Arc<Self>) {
        let changes = self.engine.changes();
        let this = self.clone();
        self.link.rt().spawn(paced(changes, move || {
            let tick = this.clone();
            this.link
                .ui()
                .upgrade_in_event_loop(move |ui| tick.tick(&ui))
                .is_ok()
        }));
    }

    /// Brings the window up to date with the engine (on the UI thread).
    fn tick(&self, ui: &AppWindow) {
        let mut state = lock(&self.state);
        // Read after the lock is taken, so a batch submitted meanwhile is either in this
        // snapshot or not yet among the affected folders.
        let snapshot = self.engine.snapshot();
        if state.feed.hwnd == 0 {
            state.feed.hwnd = window_handle(ui).unwrap_or(0);
        }
        let hwnd = state.feed.hwnd;
        let busy = snapshot.state != EngineState::Idle;

        // Whether a long run just ended.
        let ended = match (state.feed.run_started, busy) {
            (None, true) => {
                state.feed.run_started = Some(Instant::now());
                None
            }
            (Some(started), false) => {
                state.feed.run_started = None;
                Some(started.elapsed())
            }
            _ => None,
        };

        // The panes to list again: when a batch has finished, those showing what it changed.
        let finished = snapshot.last_batch.as_ref().map(|batch| batch.finished_at);
        let mut refresh = Vec::new();
        if finished != state.feed.last_finished || (!busy && !state.affected.is_empty()) {
            state.feed.last_finished = finished;
            refresh = if busy {
                state.affected.clone()
            } else {
                std::mem::take(&mut state.affected)
            };
        }

        let taskbar = self.taskbar_update(&mut state.feed, &snapshot, hwnd);
        // Uploads cut off mid-way are checked on their site; only when some had gone and were
        // sent again does the status line say so (ruling Q55).
        let rechecked = snapshot.rechecked;
        let resent_note = (rechecked.resent > state.feed.resent_told).then(|| {
            state.feed.resent_told = rechecked.resent;
            copy::rechecked(rechecked.checked, rechecked.resent)
        });
        let planning = state.planning_text();
        drop(state);

        let mut drawer = drawer_view(&snapshot, Utc::now());
        // A plan being worked out shows in an idle drawer, where Cancel stops it (review M4).
        if let Some(text) = planning
            && !drawer.running
        {
            drawer.running = true;
            drawer.title.clone_from(&text);
            drawer.collapsed_text = text;
            drawer.line = String::new();
        }
        apply_drawer(ui, &drawer);
        show_banners(ui, &snapshot.state);
        let notice =
            (snapshot.journal_errors > 0).then(|| copy::journal_errors(snapshot.journal_errors));
        self.link.set_notice(ui, notice);
        if let Some(note) = resent_note {
            self.link.status(note);
        }
        if let Some((progress, wanted)) = taskbar {
            progress.set(wanted);
        }
        for place in &refresh {
            match place {
                Affected::Remote(site, dir) => self.link.refresh_remote_at(site, dir),
                Affected::Local(dir) => self.link.refresh_local_at(dir),
            }
        }
        if let Some(took) = ended {
            self.notify_if_away(&snapshot, took, hwnd);
        }
    }

    /// What to show on the taskbar button now, if it is time to change it: at once for a new
    /// state, otherwise no more often than [`TASKBAR_EVERY`].
    fn taskbar_update(
        &self,
        feed: &mut Feed,
        snapshot: &EngineSnapshot,
        hwnd: isize,
    ) -> Option<(TaskbarProgress, TaskbarState)> {
        if !feed.taskbar_tried && hwnd != 0 {
            feed.taskbar_tried = true;
            feed.taskbar = TaskbarProgress::new(hwnd);
        }
        let progress = feed.taskbar?;
        let wanted = taskbar_state(snapshot);
        if feed.taskbar_shown == Some(wanted) {
            return None;
        }
        let same_kind = feed
            .taskbar_shown
            .is_some_and(|shown| std::mem::discriminant(&shown) == std::mem::discriminant(&wanted));
        if same_kind && !feed.taskbar_pace.ready(Instant::now()) {
            return None;
        }
        feed.taskbar_shown = Some(wanted);
        Some((progress, wanted))
    }

    /// A run that took longer than [`NOTIFY_AFTER`] ended: tells the user in a notification if
    /// HTTPorter isn't in front (or flashes its taskbar button when notifications are off).
    fn notify_if_away(&self, snapshot: &EngineSnapshot, took: Duration, hwnd: isize) {
        let files = snapshot.files_done + snapshot.files_failed;
        if took < NOTIFY_AFTER || files == 0 || hwnd == 0 || win::is_foreground(hwnd) {
            return;
        }
        let site = snapshot
            .last_batch
            .as_ref()
            .map_or_else(|| snapshot.site.clone(), |batch| batch.site.clone());
        let (title, body) = copy::finished_notification(
            snapshot.direction,
            snapshot.files_done,
            snapshot.files_failed,
            &site,
        );
        // A toast is WinRT and COM work: off the UI thread.
        self.link
            .rt()
            .spawn_blocking(move || win::notify_or_flash(hwnd, &title, &body));
    }

    // ---- Settings ----

    /// New Transfers settings from the Settings window: used by the engine and the sites'
    /// clients, and saved. Settings equal to those in use change nothing (so closing the window
    /// without an edit writes nothing).
    ///
    /// The hub drops every client, and builds each again with the new settings when next asked.
    /// The sites with transfers under way get theirs at once (review I3): built off the UI
    /// thread, then handed to the engine for the work still queued, and only then are the
    /// engine's new limits applied, so that no transfer starts on an old client under them (a
    /// raised Simultaneous would otherwise queue inside the old client's own slots, and a new
    /// proxy would never reach the queued jobs).
    ///
    /// Clients built for a change are handed over only if no other change came while they were
    /// built. That is told by a generation, not by comparing settings (review minor B): after A,
    /// B, then A again, a slow build for the first A may hold clients made while B was in use.
    fn apply_settings(self: &Arc<Self>, settings: TransferSettings) {
        if settings == self.session.settings() {
            return;
        }
        // Before the hub has them: a build that finds the generation unchanged was made under
        // the settings it was started for.
        let generation = self.settings_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.session.hub().set_settings(settings.clone());
        let saved = self.session.save_settings(self.link.rt(), settings.clone());
        let link = self.link.clone();
        self.link.rt().spawn(async move {
            if let Ok(Err(e)) = saved.await {
                link.status(copy::settings_not_saved(e));
            }
        });
        let sites = self.engine.sites();
        if sites.is_empty() {
            let _applying = lock(&self.applying);
            self.engine.set_settings(settings);
            return;
        }
        let this = self.clone();
        self.link.rt().spawn_blocking(move || {
            let fresh: Vec<(SiteId, Arc<dyn StorageProvider>)> = sites
                .into_iter()
                .filter_map(|id| {
                    let record = this.session.site(&id)?;
                    let provider = this.session.hub().provider(&record).ok()?;
                    Some((id, provider))
                })
                .collect();
            let _applying = lock(&this.applying);
            if this.settings_generation.load(Ordering::SeqCst) != generation {
                // Newer settings came meanwhile (the same as these again, maybe): theirs apply.
                return;
            }
            for (site, provider) in fresh {
                this.engine.replace_provider(&site, provider);
            }
            this.engine.set_settings(settings);
        });
    }

    // ---- Closing ----

    /// Whether closing the window must ask first: transfers are running or waiting. Journal
    /// writes still on their way are not transfers (review C1): closing waits for them instead.
    fn must_ask_before_closing(&self) -> bool {
        lock(&self.state).quitting.is_none() && self.engine.has_work()
    }

    /// Whether closing the window must wait for the journal to catch up (or for uploads left in
    /// parts to be given up) before the window goes: after Cancel All, say.
    fn must_wait_before_closing(&self) -> bool {
        lock(&self.state).quitting.is_none() && !self.settled(Quit::Pause)
    }

    /// Whether closing has nothing left to wait for: the work in flight has stopped (for Cancel,
    /// nothing is left at all), the journal has every change, and the uploads cancelled have
    /// been given up.
    fn settled(&self, how: Quit) -> bool {
        let stopped = match how {
            Quit::Cancel => !self.engine.has_work(),
            Quit::Pause => self.engine.snapshot().active_count == 0,
        };
        stopped && self.engine.pending_writes() == 0 && self.engine.pending_aborts() == 0
    }

    /// Waits until `deadline` at most for [`Self::settled`]; whether it got there.
    fn wait_settled(&self, how: Quit, deadline: Instant) -> bool {
        wait_until(deadline, || self.settled(how))
    }

    /// When closing stops waiting: [`SETTLE_FOR`] after the first [`Self::quit`], or from now
    /// when the window closed without one. `quit` and [`Self::finish`] share it, so closing
    /// waits [`SETTLE_FOR`] at most in all, not that twice (review minor A).
    fn closing_deadline(&self) -> Instant {
        *lock(&self.state)
            .closing_by
            .get_or_insert_with(|| Instant::now() + SETTLE_FOR)
    }

    /// The quit alert's answer, or a close that must wait (Pause): everything is paused, or
    /// cancelled for Cancel, and the event loop stops once that is saved (review C1). The window
    /// stays meanwhile, and says "Saving…" when it takes long enough to see; it closes by the
    /// closing deadline whatever happens.
    fn quit(self: &Arc<Self>, how: Quit) {
        lock(&self.state).quitting = Some(how);
        let deadline = self.closing_deadline();
        match how {
            Quit::Cancel => self.engine.cancel_all(),
            Quit::Pause => self.engine.pause_all(),
        }
        if self.settled(how) {
            let _ = slint::quit_event_loop();
            return;
        }
        let this = self.clone();
        self.link.rt().spawn_blocking(move || {
            let saving = (Instant::now() + SAVING_AFTER).min(deadline);
            if !this.wait_settled(how, saving) {
                this.link.status(copy::SAVING.to_owned());
                this.wait_settled(how, deadline);
            }
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        });
    }

    /// After the event loop has stopped: lets the work in flight stop, the journal catch up and
    /// the uploads cancelled be given up, until the closing deadline at most (what is left of
    /// it after `quit`). After Pause and Quit (or a close with nothing running) the jobs still
    /// queued stay in the journal for the resume banner; after Cancel and Quit nothing is left.
    pub fn finish(&self) {
        let how = lock(&self.state).quitting.unwrap_or(Quit::Pause);
        if how == Quit::Pause {
            self.engine.pause_all();
        }
        self.wait_settled(how, self.closing_deadline());
    }
}

/// Waits, on this thread, until `done` or `deadline`; whether `done` got there.
fn wait_until(deadline: Instant, done: impl Fn() -> bool) -> bool {
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wires the drawer, the sheets, the alerts, the banners, Settings and closing the window to
/// `transfers`, offers to resume what the journal holds, and starts the engine's feed.
pub fn install(ui: &AppWindow, transfers: &Arc<Transfers>) {
    install_drawer(ui, transfers);
    install_sheets(ui, transfers);
    let settings = install_settings(ui, transfers);
    install_close(ui, transfers, settings);
    transfers.offer_resume(ui);
    transfers.tick(ui);
    transfers.spawn_feed();
}

fn install_drawer(ui: &AppWindow, transfers: &Arc<Transfers>) {
    let engine = &transfers.engine;
    ui.on_transfers_cancel_all({
        let transfers = transfers.clone();
        move || {
            transfers.cancel_planning();
            transfers.engine.cancel_all();
        }
    });
    let id = |id: i32| JobId(u64::try_from(id).unwrap_or(0));
    ui.on_transfers_pause({
        let engine = engine.clone();
        move || engine.pause_all()
    });
    ui.on_transfers_resume({
        let engine = engine.clone();
        move || engine.resume_all()
    });
    ui.on_transfer_cancel({
        let engine = engine.clone();
        move |row| engine.cancel(id(row))
    });
    ui.on_transfer_retry({
        let engine = engine.clone();
        move |row| engine.retry(&[id(row)])
    });
    ui.on_transfers_retry_failed({
        let engine = engine.clone();
        move || engine.retry_all_failed()
    });
    ui.on_transfer_move_to_top({
        let engine = engine.clone();
        move |row| engine.move_to_top(id(row))
    });
    ui.on_transfer_pause({
        let engine = engine.clone();
        move |row| engine.pause(id(row))
    });
    ui.on_transfer_resume({
        let engine = engine.clone();
        move |row| engine.resume(id(row))
    });
}

fn install_sheets(ui: &AppWindow, transfers: &Arc<Transfers>) {
    let weak = ui.as_weak();
    ui.on_conflicts_continue({
        let (transfers, weak) = (transfers.clone(), weak.clone());
        move |mode, remember| {
            if let Some(ui) = weak.upgrade() {
                transfers.conflicts_continue(&ui, mode, remember);
            }
        }
    });
    ui.on_conflicts_cancel({
        let transfers = transfers.clone();
        move || transfers.conflicts_cancel()
    });
    ui.on_delete_confirm({
        let transfers = transfers.clone();
        move || transfers.delete_confirm()
    });
    ui.on_delete_cancel({
        let transfers = transfers.clone();
        move || transfers.delete_cancel()
    });
    ui.on_new_folder_create({
        let (transfers, weak) = (transfers.clone(), weak.clone());
        move |name| {
            if let Some(ui) = weak.upgrade() {
                transfers.new_folder_create(&ui, &name);
            }
        }
    });
    ui.on_new_folder_cancel({
        let transfers = transfers.clone();
        move || transfers.new_folder_cancel()
    });
    // Keep Running: the alert has closed itself, and nothing else changes. (Its other two
    // answers are wired with closing the window, `install_close`.)
    ui.on_quit_keep_running(|| {});
    ui.on_resume_banner_resume({
        let (transfers, weak) = (transfers.clone(), weak.clone());
        move || {
            if let Some(ui) = weak.upgrade() {
                transfers.resume_journal(&ui);
            }
        }
    });
    ui.on_resume_banner_discard({
        let (transfers, weak) = (transfers.clone(), weak.clone());
        move || {
            if let Some(ui) = weak.upgrade() {
                transfers.discard_journal(&ui);
            }
        }
    });
    ui.on_disk_banner_resume({
        let engine = transfers.engine.clone();
        move || engine.resume_all()
    });
    ui.on_clock_banner_resume({
        let transfers = transfers.clone();
        move || transfers.resume_clock()
    });
    // Edit Site… opens the site in Edit Site, which is `binding.rs`'s; saving it calls
    // `Transfers::site_edited`.
}

/// The Settings window, made the first time the gear is pressed and kept (hidden) after.
type SettingsSlot = Rc<RefCell<Option<SettingsWindow>>>;

/// Turns the gear on and opens the Settings window from it. Every edit is checked, and settings
/// that pass go to [`Transfers::apply_settings`]; closing the window checks and applies the form
/// once more, for the proxy address, which is only reported when it is finished (Return, or
/// leaving the field).
fn install_settings(ui: &AppWindow, transfers: &Arc<Transfers>) -> SettingsSlot {
    ui.set_settings_available(true);
    let slot: SettingsSlot = Rc::default();
    ui.on_open_settings({
        let (transfers, slot) = (transfers.clone(), slot.clone());
        move || {
            let mut window = slot.borrow_mut();
            if window.is_none() {
                *window = settings_window(&transfers);
            }
            if let Some(window) = window.as_ref() {
                settings_view::show(window, &transfers.session.settings());
                let _ = window.show();
            }
        }
    });
    slot
}

fn settings_window(transfers: &Arc<Transfers>) -> Option<SettingsWindow> {
    let window = SettingsWindow::new().ok()?;
    settings_view::wire(&window, {
        let transfers = transfers.clone();
        move |settings| transfers.apply_settings(settings)
    });
    let weak = window.as_weak();
    window.window().on_close_requested(move || {
        if let Some(window) = weak.upgrade() {
            window.invoke_changed();
        }
        CloseRequestResponse::HideWindow
    });
    Some(window)
}

/// Closing the window while transfers run or wait asks first (the quit alert). Closing it hides
/// the Settings window too, so the event loop ends.
fn install_close(ui: &AppWindow, transfers: &Arc<Transfers>, settings: SettingsSlot) {
    let weak = ui.as_weak();
    let close_settings = {
        let settings = settings.clone();
        move || {
            if let Some(window) = settings.borrow().as_ref() {
                let _ = window.hide();
            }
        }
    };
    ui.window().on_close_requested({
        let transfers = transfers.clone();
        let close_settings = close_settings.clone();
        move || {
            let Some(ui) = weak.upgrade() else {
                return CloseRequestResponse::HideWindow;
            };
            if transfers.must_ask_before_closing() {
                let snapshot = transfers.engine.snapshot();
                let unfinished = snapshot.active_count + snapshot.queued_count;
                sheets_view::show_quit_alert(&ui, &sheets_view::quit_alert(unfinished));
                return CloseRequestResponse::KeepWindowShown;
            }
            close_settings();
            if transfers.must_wait_before_closing() {
                // Nothing to ask about, but changes still to save (after Cancel All, say): the
                // window closes once they are (review C1).
                transfers.quit(Quit::Pause);
                return CloseRequestResponse::KeepWindowShown;
            }
            CloseRequestResponse::HideWindow
        }
    });
    // The alert's two ways out end the event loop themselves (`Transfers::quit`); the Settings
    // window must not keep it running.
    let quit_pause = {
        let (transfers, close_settings) = (transfers.clone(), close_settings.clone());
        move || {
            close_settings();
            transfers.quit(Quit::Pause);
        }
    };
    let quit_cancel = {
        let transfers = transfers.clone();
        move || {
            close_settings();
            transfers.quit(Quit::Cancel);
        }
    };
    ui.on_quit_pause(quit_pause);
    ui.on_quit_cancel_all(quit_cancel);
}

/// Calls `draw` after the engine's changes, at most once every [`REDRAW_EVERY`]: the changes that
/// come during the wait are drawn with the next call. Ends when the engine is gone, or when `draw`
/// says the window is.
async fn paced(mut changes: watch::Receiver<u64>, mut draw: impl FnMut() -> bool) {
    let mut pace = Throttle::new(REDRAW_EVERY);
    while changes.changed().await.is_ok() {
        if !pace.ready(Instant::now()) {
            // Changes during the wait are drawn with this one.
            tokio::time::sleep(REDRAW_EVERY).await;
            pace.ready(Instant::now());
        }
        changes.borrow_and_update();
        if !draw() {
            break;
        }
    }
}

/// The banners the engine's state calls for: offline, sign-in failed, the PC's clock off (review
/// I4), or the disk full.
fn show_banners(ui: &AppWindow, state: &EngineState) {
    let (mut offline, mut auth, mut clock, mut disk) =
        (String::new(), String::new(), String::new(), String::new());
    match state {
        EngineState::Offline { .. } => offline = sheets_view::offline_banner(),
        EngineState::AuthStopped { site } => auth = sheets_view::auth_banner(site),
        EngineState::ClockSkew { site } => clock = sheets_view::clock_banner(site),
        EngineState::DiskFull { drive, needed } => {
            disk = sheets_view::disk_full_banner(drive, *needed);
        }
        EngineState::Idle | EngineState::Running | EngineState::Paused => {}
    }
    if ui.get_offline_banner_text() != offline.as_str() {
        ui.set_offline_banner_text(offline.into());
    }
    if ui.get_auth_banner_text() != auth.as_str() {
        ui.set_auth_banner_text(auth.into());
    }
    if ui.get_clock_banner_text() != clock.as_str() {
        ui.set_clock_banner_text(clock.into());
    }
    if ui.get_disk_banner_text() != disk.as_str() {
        ui.set_disk_banner_text(disk.into());
    }
}

/// The taskbar button's progress for a snapshot: by bytes when there are any (uploads and
/// downloads), by files otherwise (deletes).
fn taskbar_state(s: &EngineSnapshot) -> TaskbarState {
    let (done, total) = if s.bytes_total > 0 {
        (s.bytes_done, s.bytes_total)
    } else {
        (s.files_done, s.files_total)
    };
    match s.state {
        EngineState::Idle => TaskbarState::None,
        EngineState::Running => TaskbarState::Normal { done, total },
        EngineState::Paused | EngineState::Offline { .. } => TaskbarState::Paused { done, total },
        EngineState::AuthStopped { .. }
        | EngineState::ClockSkew { .. }
        | EngineState::DiskFull { .. } => TaskbarState::Error { done, total },
    }
}

/// The status line for the items of a download that get other names on this PC (review I7):
/// those whose names Windows can't store (`a:b`, `CON`), then, in their own sentence, those whose
/// names differ only in capital letters, which Windows takes for the same name. Each sentence
/// shows one of them.
pub(crate) fn renamed_note(renamed: &[(RemotePath, String)]) -> Option<String> {
    let mut unstorable: Vec<(&str, &str)> = Vec::new();
    let mut same_name: Vec<(&str, &str)> = Vec::new();
    for (path, name) in renamed {
        let original = path.name().unwrap_or_default();
        if httporter_transfer::safe_local_name(original) == original {
            same_name.push((original, name));
        } else {
            unstorable.push((original, name));
        }
    }
    let mut sentences = Vec::new();
    if let Some(&(from, to)) = unstorable.first() {
        sentences.push(copy::renamed_for_windows(unstorable.len(), from, to));
    }
    // An item of the group that keeps its name is listed too; the example is one that doesn't.
    if let Some(&(from, to)) = same_name
        .iter()
        .find(|pair| pair.0 != pair.1)
        .or(same_name.first())
    {
        sentences.push(copy::renamed_same_name(same_name.len(), from, to));
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

/// The status line for items a plan leaves out: how many, and the first with its reason.
fn skipped_note(skipped: &[(PathBuf, String)]) -> Option<String> {
    let (first, reason) = skipped.first()?;
    let name = first.file_name().map_or_else(
        || first.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    Some(copy::skipped_items(skipped.len(), &name, reason))
}

/// The window's handle (an `HWND`), once the window is on screen.
fn window_handle(ui: &AppWindow) -> Option<isize> {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    ui.window()
        .with_winit_window(|window| match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
            _ => None,
        })
        .flatten()
}

/// Opens Explorer at `path`'s folder with `path` selected.
pub fn show_in_explorer(path: &Path) {
    use std::os::windows::process::CommandExt;
    // Explorer reads `/select,"path"` itself; quoting the path keeps commas and spaces in it.
    let _ = std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn();
}

#[cfg(test)]
#[path = "transfers_tests.rs"]
mod tests;
