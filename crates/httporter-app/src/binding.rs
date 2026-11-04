//! Connects the window to the [`Session`].
//!
//! The window and the tasks share one `Arc<Session>`. Every callback that lists a folder or talks
//! to the storage runs as a task on the tokio runtime, never on the UI thread, and hands the view
//! it produced back to the window with `upgrade_in_event_loop`.
//!
//! The session locks its parts separately, so tasks run side by side: a hanging cloud listing
//! holds up neither the This PC pane nor the Add Site sheet. Their views can therefore reach the
//! window out of order. Each pane's view carries a version, and the window applies only a view
//! newer than the one it shows ([`Shown`]). The sidebar and the banner's notice are read from the
//! session when a view is applied: both are short reads of state kept in memory.
//!
//! A request that lists a folder first hands the window a loading view (the pane says
//! "Loading…" and keeps its rows), then the listed one.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use httporter_bunny::{REGIONS, ZoneList};
use httporter_core::{Journal, RemotePath, S3Preset, SiteId, SiteRecord};
use httporter_platform::win;
use httporter_transfer::Engine;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

#[cfg(test)]
use crate::add_site::ValidSite;
use crate::add_site::{
    SiteForm, draft_for, field_blamed, show_buckets, show_form_errors, show_zones, validate,
};
use crate::copy;
use crate::import::{AwsProfile, aws_draft, env_draft, read_aws_profiles, read_env_file};
use crate::publish::{self, Publisher};
use crate::selection::{ActivePane, Selection, ToolbarInputs, ToolbarState, toolbar_state};
use crate::session::{
    LocalView, RemoteView, Session, SiteItemData, SiteProvider, TestResult, Ticket,
    endpoint_preview,
};
use crate::sheets_view;
use crate::sort::SortKey;
use crate::transfers::Transfers;
use crate::viewmodel::{RowData, sort_key_for_column, to_slint_rows};
use crate::{AppWindow, FileRow, ProviderTile, SiteDraft, SiteItem};

/// What a view changes: a new listing replaces the rows; a refresh replaces them with the same
/// folder's (so the selected row stays, found by name); a new sort only reorders them; a loading
/// view only says that a listing is on its way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Listed,
    Refreshed,
    Sorted,
    Loading,
}

impl Change {
    /// The view brings a new item count for the status bar (unless the listing failed).
    fn counts(self) -> bool {
        matches!(self, Self::Listed | Self::Refreshed)
    }
}

/// Something the cloud pane was asked to do. Clicks on a row or a crumb carry the version of the
/// view they were on (see [`RemoteView::version`]).
#[derive(Clone, Copy, Debug)]
enum RemoteOp {
    Select(usize),
    Open(usize, u64),
    Up,
    Crumb(usize, u64),
    Back,
    Forward,
    Refresh,
    Sort(SortKey, bool),
}

impl RemoteOp {
    fn change(self) -> Change {
        match self {
            Self::Sort(..) => Change::Sorted,
            Self::Refresh => Change::Refreshed,
            _ => Change::Listed,
        }
    }

    /// Runs a request other than [`RemoteOp::Select`] (which [`Ctx::remote`] runs itself);
    /// `started` gets the loading view of one that lists a folder.
    async fn run(
        self,
        session: &Session,
        ticket: Ticket,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        match self {
            Self::Select(_) => session.remote_view(),
            Self::Open(row, seen) => session.open_remote(ticket, row, seen, started).await,
            Self::Up => session.remote_up(ticket, started).await,
            Self::Crumb(index, seen) => session.remote_crumb(ticket, index, seen, started).await,
            Self::Back => session.remote_back(ticket, started).await,
            Self::Forward => session.remote_forward(ticket, started).await,
            Self::Refresh => session.remote_refresh(ticket, started).await,
            Self::Sort(key, ascending) => session.sort_remote(key, ascending),
        }
    }
}

/// Something the This PC pane was asked to do (as [`RemoteOp`]).
#[derive(Clone, Copy, Debug)]
enum LocalOp {
    Open(usize, u64),
    Up,
    Crumb(usize, u64),
    Back,
    Forward,
    Refresh,
    Sort(SortKey, bool),
}

impl LocalOp {
    /// Runs the request; `started` gets the loading view of one that lists a folder.
    async fn run(
        self,
        session: &Session,
        ticket: Ticket,
        started: impl FnOnce(LocalView) + Send,
    ) -> LocalView {
        match self {
            Self::Open(row, seen) => session.open_local(ticket, row, seen, started).await,
            Self::Up => session.local_up(ticket, started).await,
            Self::Crumb(index, seen) => session.local_crumb(ticket, index, seen, started).await,
            Self::Back => session.local_back(ticket, started).await,
            Self::Forward => session.local_forward(ticket, started).await,
            Self::Refresh => session.local_refresh(ticket, started).await,
            Self::Sort(key, ascending) => session.sort_local(key, ascending),
        }
    }

    fn change(self) -> Change {
        match self {
            Self::Sort(..) => Change::Sorted,
            Self::Refresh => Change::Refreshed,
            _ => Change::Listed,
        }
    }
}

/// A task that a newer one of the same kind replaces, giving it up.
#[derive(Default)]
struct Slot(RefCell<Option<JoinHandle<()>>>);

impl Slot {
    fn is_running(&self) -> bool {
        self.0
            .borrow()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
    }

    fn replace(&self, task: JoinHandle<()>) {
        if let Some(old) = self.0.replace(Some(task)) {
            old.abort();
        }
    }

    fn abort(&self) {
        if let Some(old) = self.0.take() {
            old.abort();
        }
    }
}

/// What the window shows of each pane: the version of the view on screen, and its error. Only
/// the UI thread uses it; the mutex is there so that the closures handed to the event loop can
/// carry it.
#[derive(Default)]
struct Shown {
    remote: u64,
    local: u64,
    remote_error: Option<String>,
    local_error: Option<String>,
    /// The rows of each pane as the window shows them, and which of them are selected (the
    /// window's rows carry the same in their `selected`). A view sets them with its rows
    /// ([`adopt`]); clicks change the selection ([`Ctx::select`]).
    remote_rows: Vec<RowData>,
    local_rows: Vec<RowData>,
    remote_selection: Selection,
    local_selection: Selection,
    /// The transfer engine can act on the toolbar's commands. Without it (the window's tests),
    /// every `can-*` stays false (see [`refresh_toolbar`]).
    transfers_ready: bool,
    /// What the transfer engine has to say about itself (its journal failing), for the banner.
    transfer_notice: Option<String>,
}

impl Shown {
    /// The selection and the rows of `pane`.
    fn pane(&mut self, pane: ActivePane) -> (&mut Selection, &[RowData]) {
        match pane {
            ActivePane::ThisPc => (&mut self.local_selection, &self.local_rows),
            ActivePane::Cloud => (&mut self.remote_selection, &self.remote_rows),
        }
    }

    /// As [`Shown::pane`], to read.
    fn view(&self, pane: ActivePane) -> (&Selection, &[RowData]) {
        match pane {
            ActivePane::ThisPc => (&self.local_selection, &self.local_rows),
            ActivePane::Cloud => (&self.remote_selection, &self.remote_rows),
        }
    }

    /// The banner: the app's notice, the transfer engine's, then the cloud pane's error, then
    /// This PC's, one per line.
    fn banner(&self, notice: Option<String>) -> String {
        [
            notice.as_deref(),
            self.transfer_notice.as_deref(),
            self.remote_error.as_deref(),
            self.local_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Puts `text` in the status line, in place of the item count. A message about one pane (what a
/// drop on it did) belongs to that pane: only a listing of it, or the user turning to the other
/// pane, takes it away ([`clear_status`]). `None`: a message about neither, which a listing of
/// either pane ends.
pub fn show_status(ui: &AppWindow, owner: Option<ActivePane>, text: String) {
    ui.set_status_message_pane(owner.map_or(-1, ActivePane::index));
    ui.set_status_message(text.into());
}

/// A listing of `pane` (or the user using it) ends the status message, unless it belongs to the
/// other pane.
fn clear_status(ui: &AppWindow, pane: ActivePane) {
    let owner = ui.get_status_message_pane();
    if owner < 0 || owner == pane.index() {
        ui.set_status_message(SharedString::new());
        ui.set_status_message_pane(-1);
    }
}

struct Ctx {
    ui: Weak<AppWindow>,
    rt: Handle,
    session: Arc<Session>,
    shown: Arc<Mutex<Shown>>,
    /// The site the cloud pane is opening, while it is on its way.
    opening: Slot,
    sheet: Slot,
    /// Choose…'s bucket listing, which a newer one replaces.
    buckets: Slot,
    /// Counts the sheet's tests and saves, and its cancels. A result is shown only while its run
    /// is the latest, so a test that finishes after a cancel (or after the sheet reopened) is
    /// dropped.
    sheet_run: Arc<AtomicU64>,
    /// What the sheets know that the window does not show (see [`SheetState`]).
    sheet_state: Arc<Mutex<SheetState>>,
    /// A site is being written (an Add Site or Save, or Add n Sites, past the point where Cancel
    /// can stop it). Cancel clears the sheet's busy flag and the sheet can be opened again meanwhile;
    /// a second Add Site is ignored until this one has ended, so it can't add the site twice.
    writing: Arc<AtomicBool>,
    /// The `.env` file picker is open (it has no owner window: a second click is ignored).
    picking_env: Arc<AtomicBool>,
    /// The transfer engine and what it is waiting for; `None` in the window's tests.
    transfers: Option<Arc<Transfers>>,
}

/// What the Add Site and Import sheets and the Remove Site alert work with that the window does
/// not hold: secrets above all, which never go into a window property. Kept only while a sheet
/// needs them, and dropped when it closes.
#[derive(Default)]
struct SheetState {
    /// Edit Site: the site the sheet changes.
    editing: Option<SiteId>,
    /// The secret Import found (a `.env` password, an AWS profile's secret access key): used when
    /// the sheet's secret field is left empty.
    imported: Option<String>,
    /// What the last Find Zones found, and the account key it used.
    zones: Option<ZoneList>,
    account_key: Option<String>,
    /// The AWS profiles the Import sheet lists, in its order.
    profiles: Vec<AwsProfile>,
    /// The site the Remove Site alert asks about.
    removing: Option<SiteId>,
}

impl SheetState {
    /// The sheet closed: what it knew is let go.
    fn forget_sheet(&mut self) {
        self.editing = None;
        self.imported = None;
        self.zones = None;
        self.account_key = None;
    }
}

/// What the tasks need to show their results: the window, the panes' bookkeeping and the session.
/// Unlike [`Ctx`] it can be sent to the runtime's threads, so the transfers keep one.
#[derive(Clone)]
pub struct Link {
    ui: Weak<AppWindow>,
    rt: Handle,
    session: Arc<Session>,
    shown: Arc<Mutex<Shown>>,
}

impl Link {
    pub fn ui(&self) -> &Weak<AppWindow> {
        &self.ui
    }

    pub fn rt(&self) -> &Handle {
        &self.rt
    }

    /// Puts `text` in the status line (from any thread).
    pub fn status(&self, text: String) {
        let _ = self
            .ui
            .upgrade_in_event_loop(move |ui| show_status(&ui, None, text));
    }

    /// Lists the cloud pane's folder again if it shows `dir` of `site`, a folder within it, or the
    /// folder that holds it, and no listing is on its way. The pane in use stays the pane in use.
    pub fn refresh_remote_at(&self, site: &SiteId, dir: &RemotePath) {
        let Some((open, shows)) = self.session.remote_shows() else {
            return;
        };
        if open == *site && (shows.is_within(dir) || dir.parent().as_ref() == Some(&shows)) {
            spawn_remote(self, RemoteOp::Refresh);
        }
    }

    /// Lists the This PC pane's folder again if it is `dir` or a folder within it, and no
    /// listing is on its way.
    pub fn refresh_local_at(&self, dir: &Path) {
        if let Some(Some(shows)) = self.session.local_shows()
            && shows.starts_with(dir)
        {
            spawn_local(self, LocalOp::Refresh);
        }
    }

    /// Puts a cloud view the session made outside a listing (a folder New Folder made) on screen.
    pub fn show_remote(&self, view: RemoteView) {
        post_remote(
            &self.ui,
            &self.shown,
            &self.session,
            view,
            Change::Refreshed,
        );
    }

    /// Sets what the transfer engine has to say in the banner (on the UI thread).
    pub fn set_notice(&self, ui: &AppWindow, notice: Option<String>) {
        let mut shown = lock(&self.shown);
        if shown.transfer_notice != notice {
            shown.transfer_notice = notice;
            ui.set_error_text(shown.banner(self.session.notice()).into());
        }
    }
}

/// Runs a request for the cloud pane on the runtime and shows its views (a loading one first, for
/// a listing). Picking a site goes through [`Ctx::remote`], which also gives up the one before.
fn spawn_remote(link: &Link, op: RemoteOp) -> JoinHandle<()> {
    // Taken here, at the request, so that requests count in the order they were made.
    let ticket = link.session.remote_ticket();
    let link = link.clone();
    link.rt.clone().spawn(async move {
        let Link {
            ui, session, shown, ..
        } = link;
        let started = {
            let (session, shown, ui) = (session.clone(), shown.clone(), ui.clone());
            move |view| post_remote(&ui, &shown, &session, view, Change::Loading)
        };
        let view = match op {
            RemoteOp::Select(index) => {
                let switched = {
                    let (session, shown, ui) = (session.clone(), shown.clone(), ui.clone());
                    move |view| post_remote(&ui, &shown, &session, view, Change::Listed)
                };
                session.select_site(ticket, index, switched).await
            }
            op => op.run(&session, ticket, started).await,
        };
        post_remote(&ui, &shown, &session, view, op.change());
    })
}

/// As [`spawn_remote`], for the This PC pane.
fn spawn_local(link: &Link, op: LocalOp) {
    let ticket = link.session.local_ticket();
    let link = link.clone();
    link.rt.clone().spawn(async move {
        let Link {
            ui, session, shown, ..
        } = link;
        let started = {
            let (session, shown, ui) = (session.clone(), shown.clone(), ui.clone());
            move |view| post_local(&ui, &shown, &session, view, Change::Loading)
        };
        let view = op.run(&session, ticket, started).await;
        post_local(&ui, &shown, &session, view, op.change());
    });
}

impl Ctx {
    fn link(&self) -> Link {
        Link {
            ui: self.ui.clone(),
            rt: self.rt.clone(),
            session: self.session.clone(),
            shown: self.shown.clone(),
        }
    }

    /// The names of the rows selected in `pane`, as the window shows them.
    fn selected_names(&self, pane: ActivePane) -> Vec<String> {
        let shown = lock(&self.shown);
        let (selection, rows) = shown.view(pane);
        selection
            .rows()
            .into_iter()
            .filter_map(|i| rows.get(i))
            .map(|row| row.name.clone())
            .collect()
    }

    fn remote(&self, op: RemoteOp) {
        let moves = !matches!(op, RemoteOp::Select(_) | RemoteOp::Sort(..));
        // While another site opens, the pane has been emptied and the rows the user saw are gone:
        // moving around in them means nothing any more.
        if moves && self.opening.is_running() {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_local_pane_active(false);
        }
        let task = spawn_remote(&self.link(), op);
        if matches!(op, RemoteOp::Select(_)) {
            // A site picked before this one is of no interest any more. (Older listings within a
            // site need no aborting here: the session gives them up itself when a newer request
            // takes effect, which frees their connections, and drops what they answer.)
            self.opening.replace(task);
        }
    }

    fn local(&self, op: LocalOp) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_local_pane_active(true);
        }
        spawn_local(&self.link(), op);
    }

    /// Back or Forward, from the toolbar or the keyboard, for the pane last used or focused.
    fn travel(&self, back: bool) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_local_pane_active() {
            self.local(if back {
                LocalOp::Back
            } else {
                LocalOp::Forward
            });
        } else {
            self.remote(if back {
                RemoteOp::Back
            } else {
                RemoteOp::Forward
            });
        }
    }

    /// Refresh (F5, Ctrl+R) for the pane last used or focused, as Back and Forward.
    fn refresh(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_local_pane_active() {
            self.local(LocalOp::Refresh);
        } else {
            self.remote(RemoteOp::Refresh);
        }
    }

    /// Changes the selection of `pane` (0 This PC, 1 the cloud pane) with `change`, which is given
    /// the selection and the pane's row count, then brings the rows' `selected`, the status bar's
    /// summary and the toolbar up to date.
    fn select(&self, pane: i32, change: impl FnOnce(&mut Selection, usize)) {
        let Some(pane) = ActivePane::from_index(pane) else {
            return;
        };
        let Some(ui) = self.ui.upgrade() else { return };
        // Using a pane makes it the one the toolbar and the status bar follow. Set before taking
        // the lock: the window answers a change of it with `active-pane-changed`.
        ui.set_local_pane_active(pane == ActivePane::ThisPc);
        let mut shown = lock(&self.shown);
        let (selection, rows) = shown.pane(pane);
        let before = selection.rows();
        change(selection, rows.len());
        show_selection(&ui, &shown, pane, &before);
        clear_status(&ui, pane);
        refresh_toolbar(&ui, &shown);
    }

    /// A row was pressed, or the keyboard landed on it.
    fn pointer(&self, pane: i32, row: i32, ctrl: bool, shift: bool) {
        let Ok(row) = usize::try_from(row) else {
            return;
        };
        self.select(pane, move |selection, n| {
            if row < n {
                selection.pointer(row, ctrl, shift);
            }
        });
    }

    /// The rubber band moved over rows `from` to `to` (both negative when it covers none).
    fn band(&self, pane: i32, from: i32, to: i32, additive: bool) {
        self.select(pane, move |selection, n| {
            match (usize::try_from(from), usize::try_from(to)) {
                (Ok(from), Ok(to)) if from < n && to < n => selection.band(from, to, additive),
                _ if additive => {}
                _ => selection.clear(),
            }
        });
    }

    fn select_all(&self, pane: i32) {
        self.select(pane, |selection, n| selection.select_all(n));
    }

    /// The pane last used or focused changed: the toolbar follows it.
    fn active_pane_changed(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        refresh_toolbar(&ui, &lock(&self.shown));
    }

    /// The version of the view on screen in the cloud pane, for a click on its rows or crumbs.
    fn remote_seen(&self) -> u64 {
        lock(&self.shown).remote
    }

    fn local_seen(&self) -> u64 {
        lock(&self.shown).local
    }

    /// Starts a run of the sheet: marks it busy at once (the sheet turns its buttons off on that,
    /// so a second press cannot start another run) and returns the run's number.
    fn begin_sheet_run(&self, ui: &AppWindow) -> u64 {
        ui.set_add_site_busy(true);
        self.sheet_run.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The sheet's form with the secret Import found in its empty secret field, and the site it
    /// edits.
    fn complete(&self, form: SiteForm) -> (SiteForm, Option<SiteId>) {
        let state = lock(&self.sheet_state);
        let form = match &state.imported {
            Some(secret) => form.or_secret(secret),
            None => form,
        };
        (form, state.editing.clone())
    }

    /// Checks the sheet's form: shows every problem under its field (and returns false) or
    /// clears them. While editing, a secret left empty is no problem: the saved one is kept.
    fn checked(&self, ui: &AppWindow, form: &SiteForm, editing: bool) -> bool {
        let problems = validate(form, editing).err().unwrap_or_default();
        show_form_errors(ui, matches!(form, SiteForm::S3 { .. }), &problems);
        problems.is_empty()
    }

    /// Test Connection, for a Bunny zone or an S3 bucket: lists the root the form describes,
    /// signed in with its secret (the saved one while editing, if left empty).
    fn test_form(&self, form: SiteForm, name: String) {
        let Some(ui) = self.ui.upgrade() else { return };
        let (form, editing) = self.complete(form);
        if !self.checked(&ui, &form, editing.is_some()) {
            return;
        }
        let s3 = matches!(form, SiteForm::S3 { .. });
        let run = self.begin_sheet_run(&ui);
        let (session, runs, weak) = (
            self.session.clone(),
            self.sheet_run.clone(),
            self.ui.clone(),
        );
        let task = self.rt.spawn(async move {
            let result = match session.prepare(form, editing, name).await {
                Ok(prepared) => session.test_prepared(&prepared).await,
                Err(text) => TestResult { ok: false, text },
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if runs.load(Ordering::SeqCst) == run {
                    ui.set_add_site_busy(false);
                    show_result(&ui, s3, result.ok, result.text);
                }
            });
        });
        self.sheet.replace(task);
    }

    /// Add Site, or Save on Edit Site (also Return in a field of the sheet): tests the connection,
    /// and saves the site only if that works. The sheet stays busy meanwhile. Cancel gives the
    /// test up. Once the test has passed, the save is a task of its own that Cancel does not
    /// abort, but which asks, at the moment the write would begin, whether the run is still the
    /// latest: a Cancel before that saves nothing, and one after it leaves the site fully added,
    /// listed and shown (see [`show_added`]). A changed site gets a new client, and its transfers
    /// stopped by a failed sign-in go on with it.
    ///
    /// While such a write is going on, another press of Add Site (the sheet can be cancelled and
    /// opened again meanwhile) is ignored, so the site is not added twice.
    fn save_form(&self, form: SiteForm, name: String) {
        let Some(ui) = self.ui.upgrade() else { return };
        if self.writing.load(Ordering::SeqCst) {
            return;
        }
        let (form, editing) = self.complete(form);
        if !self.checked(&ui, &form, editing.is_some()) {
            return;
        }
        let edited = editing.is_some();
        let done = self.sheet_done(&ui, matches!(form, SiteForm::S3 { .. }));
        let (session, rt, transfers, writing) = (
            self.session.clone(),
            self.rt.clone(),
            self.transfers.clone(),
            self.writing.clone(),
        );
        let task = self.rt.spawn(async move {
            let prepared = match session.prepare(form, editing, name).await {
                Ok(prepared) => prepared,
                Err(text) => return done.post(Err(text)),
            };
            let tested = session.test_prepared(&prepared).await;
            if !tested.ok {
                return done.post(Err(tested.text));
            }
            rt.spawn(async move {
                let _writing = ClearOnDrop(writing.clone());
                let proceed = {
                    let (runs, run, writing) = (done.runs.clone(), done.run, writing.clone());
                    move || {
                        // Marked before the token is looked at, so that whoever sees the run
                        // go ahead can also see that a write has begun.
                        writing.store(true, Ordering::SeqCst);
                        let latest = runs.load(Ordering::SeqCst) == run;
                        if !latest {
                            writing.store(false, Ordering::SeqCst);
                        }
                        latest
                    }
                };
                // `None`: the sheet was cancelled before the write began, and nothing was saved.
                let Some(saved) = session.store_prepared_if(prepared, proceed).await else {
                    return;
                };
                if let (true, Ok(record), Some(transfers)) = (edited, &saved, &transfers) {
                    transfers.site_edited(record).await;
                }
                done.post(saved);
            });
        });
        self.sheet.replace(task);
    }

    /// Starts a run of the sheet whose outcome is a saved site (see [`SheetRun`]).
    fn sheet_done(&self, ui: &AppWindow, s3: bool) -> SheetRun {
        SheetRun {
            run: self.begin_sheet_run(ui),
            ui: self.ui.clone(),
            runs: self.sheet_run.clone(),
            session: self.session.clone(),
            state: self.sheet_state.clone(),
            s3,
        }
    }

    /// Find Zones: lists the zones of the Bunny account whose key is `key` (through the user's
    /// proxy), the usable ones ticked and the others greyed out with their reason (Ruling Q42).
    fn find_zones(&self, key: String) {
        let Some(ui) = self.ui.upgrade() else { return };
        let form = SiteForm::BunnyAccount { key };
        if !self.checked(&ui, &form, false) {
            return;
        }
        let SiteForm::BunnyAccount { key } = form else {
            return;
        };
        {
            let mut state = lock(&self.sheet_state);
            state.zones = None;
            state.account_key = None;
        }
        show_zones(&ui, &ZoneList::default());
        let run = self.begin_sheet_run(&ui);
        let (session, runs, weak, state) = (
            self.session.clone(),
            self.sheet_run.clone(),
            self.ui.clone(),
            self.sheet_state.clone(),
        );
        let task = self.rt.spawn(async move {
            let found = session.find_zones(key.clone()).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if runs.load(Ordering::SeqCst) != run {
                    return;
                }
                ui.set_add_site_busy(false);
                match found {
                    Ok(list) => {
                        show_zones(&ui, &list);
                        if list.zones.is_empty() {
                            ui.set_add_site_result(copy::NO_ZONES.into());
                            ui.set_add_site_result_ok(false);
                        }
                        let mut state = lock(&state);
                        state.zones = Some(list);
                        state.account_key = Some(key);
                    }
                    Err(text) => {
                        ui.set_add_site_result(text.into());
                        ui.set_add_site_result_ok(false);
                    }
                }
            });
        });
        self.sheet.replace(task);
    }

    /// Add n Sites: the ticked zones (only usable ones can be ticked) become sites, each with its
    /// password in the credential store; the account key is kept only with "Keep it…" (`keep`).
    /// Like a save, it is not given up by Cancel once begun.
    fn add_zones(&self, checked: &ModelRc<bool>, keep: bool) {
        let Some(ui) = self.ui.upgrade() else { return };
        if self.writing.load(Ordering::SeqCst) {
            return;
        }
        let (zones, key) = {
            let state = lock(&self.sheet_state);
            let Some(list) = &state.zones else { return };
            let zones: Vec<_> = list
                .zones
                .iter()
                .enumerate()
                .filter(|(i, _)| checked.row_data(*i).unwrap_or(false))
                .map(|(_, zone)| zone.clone())
                .collect();
            let key = if keep {
                state.account_key.clone()
            } else {
                None
            };
            (zones, key)
        };
        if zones.is_empty() {
            return;
        }
        let done = self.sheet_done(&ui, false);
        let (session, writing) = (self.session.clone(), self.writing.clone());
        writing.store(true, Ordering::SeqCst);
        let writing = ClearOnDrop(writing);
        self.rt.spawn(async move {
            let _writing = writing;
            let added = session.add_zones(zones, key).await;
            done.post_many(added);
        });
    }

    /// Choose…: lists the buckets the sheet's S3 key can see (the saved secret while editing, if
    /// the field is left empty).
    fn list_buckets(&self, draft: &SiteDraft, secret: &str) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(form) = SiteForm::from_s3_draft(draft, secret) else {
            return;
        };
        let (form, editing) = self.complete(form);
        show_buckets(&ui, &[]);
        ui.set_add_site_buckets_error(SharedString::new());
        ui.set_add_site_buckets_busy(true);
        let (session, weak) = (self.session.clone(), self.ui.clone());
        let task = self.rt.spawn(async move {
            let listed = session.list_buckets(form, editing).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_add_site_buckets_busy(false);
                match listed {
                    Ok(names) => show_buckets(&ui, &names),
                    Err(text) => ui.set_add_site_buckets_error(text.into()),
                }
            });
        });
        self.buckets.replace(task);
    }

    /// Another provider or way to sign in was chosen: what Import found belongs to the one before.
    fn forget_import(&self) {
        lock(&self.sheet_state).imported = None;
        if let Some(ui) = self.ui.upgrade() {
            ui.set_add_site_secret_imported(false);
        }
    }

    /// The sheet was cancelled: give up its test (a save that has begun is not given up, and a
    /// save that has not begun will find its run is no longer the latest and write nothing), forget
    /// what it knew, and let it be used again.
    fn cancel_sheet(&self) {
        self.sheet_run.fetch_add(1, Ordering::SeqCst);
        self.sheet.abort();
        self.buckets.abort();
        lock(&self.sheet_state).forget_sheet();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_add_site_busy(false);
            ui.set_add_site_editing(false);
            ui.set_add_site_secret_imported(false);
        }
    }
}

/// Where a run of the sheet that saves sites reports to, from a task. Whether the run is still
/// the latest is decided when the window gets to it.
struct SheetRun {
    ui: Weak<AppWindow>,
    runs: Arc<AtomicU64>,
    run: u64,
    session: Arc<Session>,
    state: Arc<Mutex<SheetState>>,
    /// The form was an S3 one (a failed sign-in's words then go under the field they blame).
    s3: bool,
}

impl SheetRun {
    /// Hands what became of Add Site or Save to the window.
    fn post(self, added: Result<SiteRecord, String>) {
        let ui = self.ui.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            let latest = self.runs.load(Ordering::SeqCst) == self.run;
            match &added {
                Ok(_) if latest => {
                    lock(&self.state).forget_sheet();
                    ui.set_add_site_editing(false);
                    ui.set_add_site_secret_imported(false);
                }
                Err(text) if latest && self.s3 => {
                    if let Some(field) = field_blamed(text) {
                        show_form_errors(&ui, true, &[(field, text.clone())]);
                    }
                }
                _ => {}
            }
            show_added(&ui, &self.session, latest, added);
        });
    }

    /// As [`Self::post`], for the zones of a Bunny account: the first one added opens. When one
    /// fails, those added before it stay, and join the sidebar.
    fn post_many(self, added: Result<Vec<SiteRecord>, String>) {
        let first = match added {
            Ok(records) => records
                .into_iter()
                .next()
                .ok_or_else(|| copy::NO_ZONES.to_owned()),
            Err(text) => {
                let (ui, session) = (self.ui.clone(), self.session.clone());
                let _ = ui.upgrade_in_event_loop(move |ui| sync_sites(&ui, &session.site_items()));
                Err(text)
            }
        };
        self.post(first);
    }
}

/// Shows a test's outcome in the result box; a failed S3 sign-in's words also go under the field
/// they blame.
fn show_result(ui: &AppWindow, s3: bool, ok: bool, text: String) {
    if let (false, true, Some(field)) = (ok, s3, field_blamed(&text)) {
        show_form_errors(ui, true, &[(field, text.clone())]);
    }
    ui.set_add_site_result(text.into());
    ui.set_add_site_result_ok(ok);
}

/// Shows what became of Add Site. `latest` is false when the sheet has been cancelled or
/// reopened since the run started; the sheet is then left as it is.
///
/// A saved site joins the sidebar and opens, and the sheet closes (a site saved after a cancel is
/// still listed and opened: it exists). A site that failed its connection test, or could not be
/// saved, was not added: the sheet stays open with the reason in its result box.
fn show_added(ui: &AppWindow, session: &Session, latest: bool, added: Result<SiteRecord, String>) {
    match added {
        Ok(record) => {
            // A site a `.env` import described gets the publish profile the file asks for.
            crate::add_site::site_saved(&record);
            sync_sites(ui, &session.site_items());
            if latest {
                ui.set_add_site_busy(false);
                ui.set_add_site_open(false);
            }
            open_site(ui, session, &record.id);
        }
        Err(message) => {
            if latest {
                ui.set_add_site_busy(false);
                ui.set_add_site_result(message.into());
                ui.set_add_site_result_ok(false);
            }
        }
    }
}

/// Selects the saved site `id` in the sidebar and opens it (again, if it is open: a changed site
/// is listed with its new client).
fn open_site(ui: &AppWindow, session: &Session, id: &SiteId) {
    let index = session
        .sites()
        .iter()
        .position(|site| site.id == *id)
        .and_then(|i| i32::try_from(i).ok());
    if let Some(index) = index {
        ui.set_selected_site(index);
        ui.invoke_site_selected(index);
    }
}

/// A fresh sheet's lists: no zones, no buckets.
fn clear_sheet_lists(ui: &AppWindow) {
    show_zones(ui, &ZoneList::default());
    show_buckets(ui, &[]);
    ui.set_add_site_buckets_error(SharedString::new());
    ui.set_add_site_buckets_busy(false);
}

/// Add Site from the sidebar or the Welcome screen, which open the sheet themselves: a blank
/// form, adding a site.
fn reset_sheet(ui: &AppWindow, state: &Mutex<SheetState>) {
    lock(state).forget_sheet();
    ui.set_add_site_editing(false);
    ui.set_add_site_secret_imported(false);
    ui.set_add_site_draft(SiteDraft::default());
    clear_sheet_lists(ui);
}

/// Opens the Add Site sheet with its fields starting from `draft`: as Edit Site for `editing`,
/// and with `imported` as the secret Import found (used when the secret field is left empty).
fn open_sheet(
    ui: &AppWindow,
    state: &Mutex<SheetState>,
    draft: SiteDraft,
    editing: Option<SiteId>,
    imported: Option<String>,
) {
    ui.set_add_site_editing(editing.is_some());
    ui.set_add_site_secret_imported(imported.is_some());
    {
        let mut state = lock(state);
        state.forget_sheet();
        state.editing = editing;
        state.imported = imported;
    }
    ui.set_add_site_draft(draft);
    clear_sheet_lists(ui);
    ui.set_add_site_busy(false);
    ui.set_add_site_open(true);
}

/// Edit… on a site's menu, and Edit Site… on the banners: the Add Site sheet as Edit Site,
/// starting from what is saved. An S3 site's access key ID is read from the credential store
/// first, off the UI thread; no secret is put in the sheet.
fn edit_site(ctx: &Ctx, index: usize) {
    let Some(site) = ctx.session.sites().get(index).cloned() else {
        return;
    };
    let (session, weak, state) = (ctx.session.clone(), ctx.ui.clone(), ctx.sheet_state.clone());
    ctx.rt.spawn_blocking(move || {
        let key_id = match site.s3 {
            Some(_) => session.saved_key_id(&site),
            None => String::new(),
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let draft = draft_for(&site, &key_id);
            open_sheet(&ui, &state, draft, Some(site.id), None);
        });
    });
}

/// Duplicate on a site's menu: a copy of the site and its secret under a new id, named "{name}
/// copy", which opens.
fn duplicate_site(ctx: &Ctx, index: usize) {
    let Some(site) = ctx.session.sites().get(index).cloned() else {
        return;
    };
    let (session, weak) = (ctx.session.clone(), ctx.ui.clone());
    ctx.rt.spawn_blocking(move || {
        let copied = session.duplicate_site(&site.id);
        let _ = weak.upgrade_in_event_loop(move |ui| match copied {
            Ok(record) => {
                sync_sites(&ui, &session.site_items());
                open_site(&ui, &session, &record.id);
            }
            Err(text) => show_status(&ui, None, text),
        });
    });
}

/// Remove… on a site's menu (and on the banner): asks with the Remove Site alert first.
fn ask_remove_site(ctx: &Ctx, index: usize) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let Some(site) = ctx.session.sites().get(index).cloned() else {
        return;
    };
    lock(&ctx.sheet_state).removing = Some(site.id.clone());
    let alert = sheets_view::remove_site_alert(&site.name, SiteProvider::of(&site).label());
    sheets_view::show_remove_site_alert(&ui, &alert);
}

/// Remove on the Remove Site alert (Ruling Q23): the site's transfers are cancelled and its
/// journal records dropped, then its secret, its row and its remembered choices are deleted, off
/// the UI thread. Files on the storage are never touched.
fn remove_site(ctx: &Ctx) {
    let Some(id) = lock(&ctx.sheet_state).removing.take() else {
        return;
    };
    let selected = ctx.ui.upgrade().and_then(|ui| {
        let index = usize::try_from(ui.get_selected_site()).ok()?;
        ctx.session.sites().get(index).map(|site| site.id.clone())
    });
    let link = ctx.link();
    let transfers = ctx.transfers.clone();
    ctx.rt.spawn_blocking(move || {
        // First, while the site and its password are still there: its uploads left in parts
        // are given up with its client (ruling Q49).
        if let Some(transfers) = &transfers {
            transfers.forget_site(&id);
        }
        let removed = link.session.remove_site(&id);
        let ui = link.ui.clone();
        let _ =
            ui.upgrade_in_event_loop(move |ui| show_removed(&ui, &link, &id, selected, removed));
    });
}

/// Shows that site `id` is gone: the sidebar without it, and the cloud pane on another site (the
/// first one) if it showed this one, or on none (the Welcome screen) if it was the last.
fn show_removed(
    ui: &AppWindow,
    link: &Link,
    id: &SiteId,
    selected: Option<SiteId>,
    removed: Result<(), String>,
) {
    if let Err(text) = removed {
        show_status(ui, None, text);
        return;
    }
    let session = &link.session;
    sync_sites(ui, &session.site_items());
    let sites = session.sites();
    match selected {
        Some(selected) if selected == *id => {
            if sites.is_empty() {
                ui.set_selected_site(-1);
                start_opening_site(ui, &link.shown, session.notice());
                spawn_remote(link, RemoteOp::Select(usize::MAX));
            } else {
                ui.set_selected_site(0);
                ui.invoke_site_selected(0);
            }
        }
        // The site on show stays; its row may have moved.
        selected => {
            let index = selected
                .and_then(|selected| sites.iter().position(|site| site.id == selected))
                .and_then(|i| i32::try_from(i).ok())
                .unwrap_or(-1);
            ui.set_selected_site(index);
        }
    }
}

/// Import on the Welcome screen: the Import sheet, listing the AWS CLI profiles on this PC (read
/// off the UI thread; their keys stay here, never in the window).
fn start_import(ctx: &Ctx) {
    let (weak, state) = (ctx.ui.clone(), ctx.sheet_state.clone());
    ctx.rt.spawn_blocking(move || {
        let profiles = read_aws_profiles();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let labels: Vec<SharedString> = profiles
                .iter()
                .map(|profile| copy::aws_profile_label(&profile.name, profile.region.as_deref()))
                .map(SharedString::from)
                .collect();
            lock(&state).profiles = profiles;
            ui.set_import_profiles(ModelRc::new(VecModel::from(labels)));
            ui.set_import_error(SharedString::new());
            ui.set_import_open(true);
        });
    });
}

/// Use Profile on the Import sheet: the Add Site sheet for Amazon S3, with the profile's region
/// and access key ID, and its secret held here.
fn import_profile(ctx: &Ctx, index: usize) {
    let Some(ui) = ctx.ui.upgrade() else { return };
    let profile = {
        let mut state = lock(&ctx.sheet_state);
        let profile = state.profiles.get(index).cloned();
        state.profiles.clear();
        profile
    };
    ui.set_import_profiles(ModelRc::default());
    if let Some(profile) = profile {
        open_sheet(
            &ui,
            &ctx.sheet_state,
            aws_draft(&profile),
            None,
            Some(profile.secret),
        );
    }
}

/// Clears a flag when dropped, also if the task or thread holding it panicked: the `.env` picker is
/// closed, the sheet's site is no longer being written.
struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Choose .env File… on the Import sheet: the file picker, then the Add Site sheet for Bunny
/// Storage with the file's zone and region, and its password held here (tested when the site is
/// saved). A file that has no zone says why on the Import sheet.
fn import_env(ctx: &Ctx) {
    if ctx.picking_env.swap(true, Ordering::SeqCst) {
        return;
    }
    let open = ClearOnDrop(ctx.picking_env.clone());
    let (weak, state) = (ctx.ui.clone(), ctx.sheet_state.clone());
    ctx.rt.spawn_blocking(move || {
        let filters: [(&str, &[&str]); 2] =
            [(copy::ENV_FILES, &["env"]), (copy::ALL_FILES, &["*"])];
        let picked = win::pick_file(copy::PICK_ENV_TITLE, &filters);
        drop(open);
        let Some(path) = picked else { return };
        let read = read_env_file(&path);
        let _ = weak.upgrade_in_event_loop(move |ui| match read {
            Ok(import) => {
                crate::add_site::expect_imported_site(&import);
                lock(&state).profiles.clear();
                ui.set_import_open(false);
                ui.set_import_profiles(ModelRc::default());
                open_sheet(&ui, &state, env_draft(&import), None, Some(import.password));
            }
            Err(text) => ui.set_import_error(text.into()),
        });
    });
}

/// Wires every callback of `ui` to `session`, fills the window with the session's starting state
/// and selects the first site, if there is one. Returns the shared session.
///
/// Without a transfer engine the toolbar's commands stay off (every `can-*` false): the window's
/// tests use this. The app installs with [`install_app`].
#[cfg(test)]
pub fn install(ui: &AppWindow, rt: Handle, session: Session) -> Arc<Session> {
    install_with(ui, rt, session, None).0
}

/// What the transfers run on: the engine and the journal it writes.
pub struct Engines {
    pub engine: Engine,
    pub journal: Arc<dyn Journal>,
}

/// Wires the window as `install` does, with the transfer engine acting on the toolbar's commands,
/// the menus, drags, drops and pastes, and feeding the drawer, the sheets and the banners; and
/// with Publish (compare, the Publish sheet, the profile pages). Returns the transfers.
pub fn install_app(
    ui: &AppWindow,
    rt: Handle,
    session: Session,
    engines: Engines,
) -> Arc<Transfers> {
    let (session, transfers, link) = install_with(ui, rt, session, Some(engines));
    let transfers = transfers.expect("installed with an engine");
    let publisher = Publisher::new(session, transfers.clone(), link);
    publish::install(ui, &publisher);
    transfers
}

/// Wires the window, with the transfer engine (the toolbar's commands switched on) or without.
fn install_with(
    ui: &AppWindow,
    rt: Handle,
    session: Session,
    engines: Option<Engines>,
) -> (Arc<Session>, Option<Arc<Transfers>>, Link) {
    let regions: Vec<SharedString> = REGIONS.iter().map(|(_, city)| (*city).into()).collect();
    ui.set_region_names(ModelRc::new(VecModel::from(regions)));
    let session = Arc::new(session);
    let shown = Arc::new(Mutex::new(Shown::default()));
    let local = session.local_view();
    {
        let mut shown = lock(&shown);
        shown.local = local.version;
        shown.local_error = local.error.clone();
        shown.transfers_ready = engines.is_some();
        apply_local(ui, &local, Change::Listed);
        adopt(ui, &mut shown, ActivePane::ThisPc, &local.rows);
        ui.set_error_text(shown.banner(session.notice()).into());
        refresh_toolbar(ui, &shown);
    }
    let sites = session.site_items();
    sync_sites(ui, &sites);

    let link = Link {
        ui: ui.as_weak(),
        rt: rt.clone(),
        session: session.clone(),
        shown: shown.clone(),
    };
    let transfers = engines.map(|Engines { engine, journal }| {
        Transfers::new(engine, journal, session.clone(), link.clone())
    });
    let ctx = Rc::new(Ctx {
        ui: ui.as_weak(),
        rt,
        session: session.clone(),
        shown,
        opening: Slot::default(),
        sheet: Slot::default(),
        buckets: Slot::default(),
        sheet_run: Arc::new(AtomicU64::new(0)),
        sheet_state: Arc::default(),
        writing: Arc::default(),
        picking_env: Arc::default(),
        transfers: transfers.clone(),
    });
    let row = |index: i32| usize::try_from(index).ok();

    ui.on_site_selected({
        let ctx = ctx.clone();
        move |index| {
            let Some(index) = row(index) else { return };
            if let Some(ui) = ctx.ui.upgrade() {
                start_opening_site(&ui, &ctx.shown, ctx.session.notice());
            }
            ctx.remote(RemoteOp::Select(index));
        }
    });
    ui.on_open_remote({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                ctx.remote(RemoteOp::Open(index, ctx.remote_seen()));
            }
        }
    });
    ui.on_remote_up({
        let ctx = ctx.clone();
        move || ctx.remote(RemoteOp::Up)
    });
    ui.on_remote_crumb_clicked({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                ctx.remote(RemoteOp::Crumb(index, ctx.remote_seen()));
            }
        }
    });
    ui.on_sort_remote({
        let ctx = ctx.clone();
        move |column| {
            if let Some(ui) = ctx.ui.upgrade() {
                let ascending = ui.get_remote_sort_ascending();
                ctx.remote(RemoteOp::Sort(sort_key_for_column(column), ascending));
            }
        }
    });

    ui.on_open_local({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                ctx.local(LocalOp::Open(index, ctx.local_seen()));
            }
        }
    });
    ui.on_local_up({
        let ctx = ctx.clone();
        move || ctx.local(LocalOp::Up)
    });
    ui.on_local_crumb_clicked({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                ctx.local(LocalOp::Crumb(index, ctx.local_seen()));
            }
        }
    });
    ui.on_sort_local({
        let ctx = ctx.clone();
        move |column| {
            if let Some(ui) = ctx.ui.upgrade() {
                let ascending = ui.get_local_sort_ascending();
                ctx.local(LocalOp::Sort(sort_key_for_column(column), ascending));
            }
        }
    });

    ui.on_row_pointer({
        let ctx = ctx.clone();
        move |pane, row, ctrl, shift| ctx.pointer(pane, row, ctrl, shift)
    });
    ui.on_band_select({
        let ctx = ctx.clone();
        move |pane, from, to, additive| ctx.band(pane, from, to, additive)
    });
    ui.on_select_all({
        let ctx = ctx.clone();
        move |pane| ctx.select_all(pane)
    });
    ui.on_active_pane_changed({
        let ctx = ctx.clone();
        move || ctx.active_pane_changed()
    });
    ui.on_upload({
        let ctx = ctx.clone();
        move || upload(&ctx)
    });
    ui.on_download({
        let ctx = ctx.clone();
        move || download(&ctx)
    });
    ui.on_download_to({
        let ctx = ctx.clone();
        move || download_to(&ctx)
    });
    ui.on_new_folder({
        let ctx = ctx.clone();
        move || new_folder(&ctx)
    });
    ui.on_delete({
        let ctx = ctx.clone();
        move || delete(&ctx)
    });
    ui.on_show_in_explorer({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                show_in_explorer(&ctx, index);
            }
        }
    });
    ui.on_drag_to_remote({
        let ctx = ctx.clone();
        move |index| drag_to_remote(&ctx, row(index))
    });
    ui.on_drag_to_local({
        let ctx = ctx.clone();
        move |index| drag_to_local(&ctx, row(index))
    });
    ui.on_paste({
        let ctx = ctx.clone();
        move || {
            if let Some(transfers) = &ctx.transfers {
                transfers.paste();
            }
        }
    });

    ui.on_go_back({
        let ctx = ctx.clone();
        move || ctx.travel(true)
    });
    ui.on_go_forward({
        let ctx = ctx.clone();
        move || ctx.travel(false)
    });
    ui.on_refresh({
        let ctx = ctx.clone();
        move || ctx.refresh()
    });

    ui.on_add_site_changed({
        let weak = ui.as_weak();
        move |zone, region| {
            if let Some(ui) = weak.upgrade() {
                ui.set_add_site_endpoint(endpoint_preview(&zone, region).into());
            }
        }
    });
    // The sheet's arguments for a Bunny zone come in this order: site name, zone, region index,
    // password.
    ui.on_add_site_test({
        let ctx = ctx.clone();
        move |name, zone, region, password| {
            ctx.test_form(SiteForm::bunny(&zone, region, &password), name.into());
        }
    });
    ui.on_add_site_save({
        let ctx = ctx.clone();
        move |name, zone, region, password| {
            ctx.save_form(SiteForm::bunny(&zone, region, &password), name.into());
        }
    });
    ui.on_add_site_test_s3({
        let ctx = ctx.clone();
        move |draft, secret| {
            if let Some(form) = SiteForm::from_s3_draft(&draft, &secret) {
                ctx.test_form(form, draft.name.into());
            }
        }
    });
    ui.on_add_site_save_s3({
        let ctx = ctx.clone();
        move |draft, secret| {
            if let Some(form) = SiteForm::from_s3_draft(&draft, &secret) {
                ctx.save_form(form, draft.name.into());
            }
        }
    });
    ui.on_add_site_find_zones({
        let ctx = ctx.clone();
        move |key| ctx.find_zones(key.into())
    });
    ui.on_add_site_zones_chosen({
        let ctx = ctx.clone();
        move |checked, keep| ctx.add_zones(&checked, keep)
    });
    ui.on_add_site_list_buckets({
        let ctx = ctx.clone();
        move |draft, secret| ctx.list_buckets(&draft, &secret)
    });
    ui.on_add_site_provider({
        let ctx = ctx.clone();
        move |_| ctx.forget_import()
    });
    ui.on_add_site_sign_in_mode({
        let ctx = ctx.clone();
        move |_| ctx.forget_import()
    });
    ui.on_add_site({
        let ctx = ctx.clone();
        move || {
            if let Some(ui) = ctx.ui.upgrade() {
                reset_sheet(&ui, &ctx.sheet_state);
            }
        }
    });
    ui.on_add_site_cancel({
        let ctx = ctx.clone();
        move || ctx.cancel_sheet()
    });

    // The sidebar's site menu, the Remove Site alert and the banners' Edit Site….
    ui.on_site_edit({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                edit_site(&ctx, index);
            }
        }
    });
    ui.on_site_duplicate({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                duplicate_site(&ctx, index);
            }
        }
    });
    ui.on_site_remove({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                ask_remove_site(&ctx, index);
            }
        }
    });
    ui.on_remove_site_confirm({
        let ctx = ctx.clone();
        move || remove_site(&ctx)
    });
    ui.on_remove_site_cancel({
        let ctx = ctx.clone();
        move || lock(&ctx.sheet_state).removing = None
    });

    // Import on the Welcome screen.
    ui.on_welcome_import({
        let ctx = ctx.clone();
        move || start_import(&ctx)
    });
    ui.on_import_profile({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                import_profile(&ctx, index);
            }
        }
    });
    ui.on_import_env({
        let ctx = ctx.clone();
        move || import_env(&ctx)
    });
    ui.on_import_cancel({
        let ctx = ctx.clone();
        move || lock(&ctx.sheet_state).profiles.clear()
    });

    // The Add Site sheet's S3 address preview.
    crate::add_site::install(ui);

    // The transfer drawer: its idle state, and callbacks that do nothing without an engine.
    crate::drawer_view::install(ui);

    // The sheets, alerts and banners, likewise.
    crate::sheets_view::install(ui);

    // With an engine: the drawer, the sheets, the banners, Settings and closing act on it.
    if let Some(transfers) = &transfers {
        crate::transfers::install(ui, transfers);
    }

    // Edit Site… on the sign-in banner: the site whose transfers a failed sign-in stopped.
    ui.on_auth_banner_edit_site({
        let ctx = ctx.clone();
        move || {
            let Some(id) = ctx.transfers.as_ref().and_then(|t| t.auth_stopped_site()) else {
                return;
            };
            if let Some(index) = ctx.session.sites().iter().position(|site| site.id == id) {
                edit_site(&ctx, index);
            }
        }
    });

    // Every control of the second round of Add Site (Ruling Q10) is wired now.
    ui.set_sites_v2_ready(true);

    if !sites.is_empty() {
        ui.set_selected_site(0);
        ctx.remote(RemoteOp::Select(0));
    }
    (session, transfers, link)
}

/// Hands a cloud view to the window (from a task).
fn post_remote(
    ui: &Weak<AppWindow>,
    shown: &Arc<Mutex<Shown>>,
    session: &Arc<Session>,
    view: RemoteView,
    change: Change,
) {
    let (shown, session) = (shown.clone(), session.clone());
    // An error means the window is gone (or, in tests, that there is no event loop).
    let _ = ui.upgrade_in_event_loop(move |ui| show_remote(&ui, &shown, &session, &view, change));
}

/// Puts a cloud view on screen, unless the window already shows a newer one; then brings the
/// banner and the sidebar (whose health dots follow the listings) up to date.
fn show_remote(
    ui: &AppWindow,
    shown: &Mutex<Shown>,
    session: &Session,
    view: &RemoteView,
    change: Change,
) {
    let mut shown = lock(shown);
    // A view of a site the user has left (its listing was on its way when another was chosen) is
    // not shown under the new site's name.
    let of_chosen_site = view.site == chosen_site(ui, session);
    if view.version > shown.remote && of_chosen_site {
        shown.remote = view.version;
        shown.remote_error = view.error.clone();
        apply_remote(ui, view, change);
        if change != Change::Loading {
            adopt(ui, &mut shown, ActivePane::Cloud, &view.rows);
        }
    }
    ui.set_error_text(shown.banner(session.notice()).into());
    refresh_toolbar(ui, &shown);
    drop(shown);
    ui.set_remote_offer_edit(session.remote_offers_edit());
    sync_sites(ui, &session.site_items());
}

/// Hands a This PC view to the window (from a task).
fn post_local(
    ui: &Weak<AppWindow>,
    shown: &Arc<Mutex<Shown>>,
    session: &Arc<Session>,
    view: LocalView,
    change: Change,
) {
    let (shown, session) = (shown.clone(), session.clone());
    let _ = ui.upgrade_in_event_loop(move |ui| show_local(&ui, &shown, &session, &view, change));
}

/// As [`show_remote`], for the This PC pane.
fn show_local(
    ui: &AppWindow,
    shown: &Mutex<Shown>,
    session: &Session,
    view: &LocalView,
    change: Change,
) {
    let mut shown = lock(shown);
    if view.version > shown.local {
        shown.local = view.version;
        shown.local_error = view.error.clone();
        apply_local(ui, view, change);
        if change != Change::Loading {
            adopt(ui, &mut shown, ActivePane::ThisPc, &view.rows);
        }
    }
    ui.set_error_text(shown.banner(session.notice()).into());
    refresh_toolbar(ui, &shown);
}

/// The id of the site chosen in the sidebar, if any.
fn chosen_site(ui: &AppWindow, session: &Session) -> Option<SiteId> {
    let index = usize::try_from(ui.get_selected_site()).ok()?;
    session.site_id_at(index)
}

/// Another site was picked in the sidebar. The pane title follows the sidebar at once, so the
/// cloud pane is emptied (not the old site's files under the new site's name) and the old site's
/// error leaves the banner. The rest of the window keeps what it shows until the new site's view
/// arrives.
fn start_opening_site(ui: &AppWindow, shown: &Mutex<Shown>, notice: Option<String>) {
    ui.set_remote_rows(ModelRc::default());
    ui.set_remote_crumbs(ModelRc::default());
    ui.set_remote_selected(-1);
    ui.set_remote_offer_edit(false);
    ui.set_remote_selection_text(SharedString::new());
    // Nothing is known of the new site yet: the status bar says it is connecting (not the old
    // site's count, nor "0 items"), and there is nowhere to go back or forward to until its view
    // arrives with a history of its own.
    ui.set_remote_status(copy::CONNECTING.into());
    ui.set_connection_text(copy::CONNECTING.into());
    ui.set_connection_ok(true);
    ui.set_remote_loading(true);
    ui.set_remote_can_go_back(false);
    ui.set_remote_can_go_forward(false);
    let mut shown = lock(shown);
    shown.remote_error = None;
    shown.remote_rows.clear();
    shown.remote_selection.clear();
    ui.set_error_text(shown.banner(notice).into());
    refresh_toolbar(ui, &shown);
}

fn strings(list: &[String]) -> ModelRc<SharedString> {
    let list: Vec<SharedString> = list.iter().map(|s| s.as_str().into()).collect();
    ModelRc::new(VecModel::from(list))
}

/// The selection for a pane's new rows, as the indexes of the rows to mark and the row the
/// keyboard is on (-1 for none): nothing in a new folder; otherwise (a refresh, a new sort, or a
/// failed listing that left the folder as it was) the same rows as before, found by name.
fn carried_selection(
    change: Change,
    failed: bool,
    old: &ModelRc<FileRow>,
    focus: i32,
    rows: &[RowData],
) -> (Vec<usize>, i32) {
    if change == Change::Listed && !failed {
        return (Vec::new(), -1);
    }
    let old_focus = usize::try_from(focus).ok().and_then(|i| old.row_data(i));
    let old_picked: Vec<FileRow> = old.iter().filter(|row| row.selected).collect();
    if old_focus.is_none() && old_picked.is_empty() {
        return (Vec::new(), -1);
    }
    let index: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.name.as_str(), i))
        .collect();
    let find = |row: &FileRow| index.get(row.name.as_str()).copied();
    let picked = old_picked.iter().filter_map(find).collect();
    let focus = old_focus
        .as_ref()
        .and_then(find)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    (picked, focus)
}

/// Sets `selected` on the rows of `model` listed in `picked`.
fn mark_selected(model: &ModelRc<FileRow>, picked: &[usize]) {
    for &index in picked {
        if let Some(mut row) = model.row_data(index) {
            row.selected = true;
            model.set_row_data(index, row);
        }
    }
}

/// The rows and the row the keyboard is on, of a pane as the window shows them.
fn pane_model(ui: &AppWindow, pane: ActivePane) -> (ModelRc<FileRow>, i32) {
    match pane {
        ActivePane::ThisPc => (ui.get_local_rows(), ui.get_local_selected()),
        ActivePane::Cloud => (ui.get_remote_rows(), ui.get_remote_selected()),
    }
}

/// The selection a window's rows carry in their `selected`, with `focus` as the keyboard's row.
fn selection_of(model: &ModelRc<FileRow>, focus: i32) -> Selection {
    let picked = model
        .iter()
        .enumerate()
        .filter(|(_, row)| row.selected)
        .map(|(i, _)| i);
    let focus = usize::try_from(focus)
        .ok()
        .filter(|&i| i < model.row_count());
    let mut selection = Selection::default();
    selection.restore(picked, focus);
    selection
}

/// A view has put new rows in `pane`: `shown` takes up their data and the selection they carry
/// (the window's rows hold it in their `selected`), and the status bar's summary follows.
fn adopt(ui: &AppWindow, shown: &mut Shown, pane: ActivePane, rows: &[RowData]) {
    let (model, focus) = pane_model(ui, pane);
    let selection = selection_of(&model, focus);
    match pane {
        ActivePane::ThisPc => {
            shown.local_rows = rows.to_vec();
            shown.local_selection = selection;
        }
        ActivePane::Cloud => {
            shown.remote_rows = rows.to_vec();
            shown.remote_selection = selection;
        }
    }
    show_summary(ui, shown, pane);
}

/// The rows in either of two sorted lists but not in both.
fn differing(a: &[usize], b: &[usize]) -> Vec<usize> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                out.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(b[j]);
                j += 1;
            }
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

/// Puts the status bar's summary of the rows selected in `pane` in the window.
fn show_summary(ui: &AppWindow, shown: &Shown, pane: ActivePane) {
    let (selection, rows) = shown.view(pane);
    let picked: Vec<&RowData> = selection
        .rows()
        .into_iter()
        .filter_map(|i| rows.get(i))
        .collect();
    let text = SharedString::from(copy::selection_summary(&picked));
    // What a drag of the selection carries (the ghost counts it).
    let count = i32::try_from(picked.len()).unwrap_or(i32::MAX);
    match pane {
        ActivePane::ThisPc => {
            ui.set_local_selection_text(text);
            ui.set_local_selected_count(count);
        }
        ActivePane::Cloud => {
            ui.set_remote_selection_text(text);
            ui.set_remote_selected_count(count);
        }
    }
}

/// Makes the window show `shown`'s selection of `pane`: the rows' `selected` (only those that
/// differ from `before`, the rows that were selected), the row the keyboard is on, and the
/// summary.
fn show_selection(ui: &AppWindow, shown: &Shown, pane: ActivePane, before: &[usize]) {
    let (selection, _) = shown.view(pane);
    let after = selection.rows();
    let (model, _) = pane_model(ui, pane);
    for index in differing(before, &after) {
        if let Some(mut row) = model.row_data(index) {
            row.selected = after.binary_search(&index).is_ok();
            model.set_row_data(index, row);
        }
    }
    let focus = selection
        .focus()
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    match pane {
        ActivePane::ThisPc => ui.set_local_selected(focus),
        ActivePane::Cloud => ui.set_remote_selected(focus),
    }
    show_summary(ui, shown, pane);
}

/// Sets which of the toolbar's commands may be pressed (and the notes that say why not). Without
/// the transfer engine (`transfers_ready`), none may: the buttons are off, not inert.
fn refresh_toolbar(ui: &AppWindow, shown: &Shown) {
    let state = if shown.transfers_ready {
        toolbar_state(ToolbarInputs {
            active: if ui.get_local_pane_active() {
                ActivePane::ThisPc
            } else {
                ActivePane::Cloud
            },
            local_selected: shown.local_selection.count(),
            remote_selected: shown.remote_selection.count(),
            // A site is open once it is chosen, connected and not in the middle of a listing.
            site_open: ui.get_selected_site() >= 0
                && ui.get_connection_ok()
                && !ui.get_remote_loading(),
            // Read-only sites arrive in I4.
            read_only: false,
            at_drive_list: ui.get_local_crumbs().row_count() == 0,
        })
    } else {
        ToolbarState::default()
    };
    ui.set_can_upload(state.can_upload);
    ui.set_can_download(state.can_download);
    ui.set_can_new_folder(state.can_new_folder);
    ui.set_can_delete(state.can_delete);
    ui.set_delete_note(state.delete_note.into());
    ui.set_actions_note(if shown.transfers_ready {
        SharedString::new()
    } else {
        copy::TRANSFERS_LATER.into()
    });
}

// ---- The toolbar's commands, the context menus', drags between the panes ----
//
// Each gathers what is selected and where, and hands it to the transfers, which plan on the
// runtime, ask through a sheet or an alert when they must, and submit to the engine. Without an
// engine (the window's tests) every `can-*` is false, so nothing reaches these.

/// Upload: the This PC selection into the cloud pane's folder, or the file picker with none (or
/// at the list of drives, whose rows are drives).
fn upload(ctx: &Ctx) {
    let Some(transfers) = &ctx.transfers else {
        return;
    };
    let names = ctx.selected_names(ActivePane::ThisPc);
    if names.is_empty() || ctx.session.local_dir().is_none() {
        transfers.pick_and_upload();
    } else {
        transfers.upload(ctx.session.local_paths(&names), None);
    }
}

/// Download: the cloud selection into the This PC pane's folder (asks for one at the list of
/// drives).
fn download(ctx: &Ctx) {
    let Some(transfers) = &ctx.transfers else {
        return;
    };
    let entries = ctx
        .session
        .remote_entries(&ctx.selected_names(ActivePane::Cloud));
    transfers.download(entries, ctx.session.local_dir());
}

/// Download to…: the cloud selection into a folder asked for.
fn download_to(ctx: &Ctx) {
    let Some(transfers) = &ctx.transfers else {
        return;
    };
    let entries = ctx
        .session
        .remote_entries(&ctx.selected_names(ActivePane::Cloud));
    transfers.download(entries, None);
}

/// New folder, in the pane in use: opens the New Folder sheet.
fn new_folder(ctx: &Ctx) {
    let (Some(transfers), Some(ui)) = (&ctx.transfers, ctx.ui.upgrade()) else {
        return;
    };
    transfers.new_folder(&ui, !ui.get_local_pane_active());
}

/// Delete the cloud selection: counts what goes, then asks with the delete alert.
fn delete(ctx: &Ctx) {
    let Some(transfers) = &ctx.transfers else {
        return;
    };
    let entries = ctx
        .session
        .remote_entries(&ctx.selected_names(ActivePane::Cloud));
    transfers.delete(entries);
}

/// The This PC selection was dragged onto the cloud pane: onto its folder row `row`, or the
/// folder it shows.
fn drag_to_remote(ctx: &Ctx, row: Option<usize>) {
    let Some(transfers) = &ctx.transfers else {
        return;
    };
    let names = ctx.selected_names(ActivePane::ThisPc);
    if names.is_empty() || ctx.session.local_dir().is_none() {
        return;
    }
    let folder = row.and_then(|row| ctx.session.remote_folder_at(row));
    transfers.upload(ctx.session.local_paths(&names), folder);
}

/// The cloud selection was dragged onto the This PC pane: onto its folder row `row`, or the
/// folder it shows (asks for one at the list of drives).
fn drag_to_local(ctx: &Ctx, row: Option<usize>) {
    let Some(transfers) = &ctx.transfers else {
        return;
    };
    let entries = ctx
        .session
        .remote_entries(&ctx.selected_names(ActivePane::Cloud));
    let local = row
        .and_then(|row| ctx.session.local_folder_at(row))
        .or_else(|| ctx.session.local_dir());
    transfers.download(entries, local);
}

/// Show row `row` of the This PC pane in Explorer, selected.
fn show_in_explorer(ctx: &Ctx, row: usize) {
    let name = lock(&ctx.shown)
        .local_rows
        .get(row)
        .map(|row| row.name.clone());
    let Some(path) = name.and_then(|name| ctx.session.local_paths(&[name]).pop()) else {
        return;
    };
    crate::transfers::show_in_explorer(&path);
}

/// Puts a cloud view on screen (everything but the banner, which [`Shown`] puts together). A
/// loading view only marks the pane: its rows stay until the listing is in.
fn apply_remote(ui: &AppWindow, view: &RemoteView, change: Change) {
    ui.set_remote_loading(view.loading);
    if change == Change::Loading {
        return;
    }
    let failed = view.error.is_some();
    let (picked, focus) = carried_selection(
        change,
        failed,
        &ui.get_remote_rows(),
        ui.get_remote_selected(),
        &view.rows,
    );
    let rows = to_slint_rows(&view.rows);
    mark_selected(&rows, &picked);
    ui.set_remote_rows(rows);
    ui.set_remote_crumbs(strings(&view.crumbs));
    ui.set_remote_selected(focus);
    ui.set_connection_text(view.connection.as_str().into());
    ui.set_connection_ok(view.connection_ok);
    ui.set_endpoint_text(view.endpoint.as_str().into());
    ui.set_remote_chip(view.chip.as_str().into());
    ui.set_status_right(view.status_right.as_str().into());
    ui.set_remote_can_go_back(view.can_back);
    ui.set_remote_can_go_forward(view.can_forward);
    if view.connecting {
        // Nothing has come back from the site yet: no count, and not a "0 items" that would pass
        // for one.
        ui.set_remote_status(copy::CONNECTING.into());
    } else if (change.counts() && !failed) || (failed && view.rows.is_empty()) {
        ui.set_remote_status(copy::items(view.rows.len()).into());
    }
    if change.counts() && !failed {
        clear_status(ui, ActivePane::Cloud);
    }
}

/// Puts a This PC view on screen (everything but the banner), as [`apply_remote`].
fn apply_local(ui: &AppWindow, view: &LocalView, change: Change) {
    ui.set_local_loading(view.loading);
    if change == Change::Loading {
        return;
    }
    let failed = view.error.is_some();
    let (picked, focus) = carried_selection(
        change,
        failed,
        &ui.get_local_rows(),
        ui.get_local_selected(),
        &view.rows,
    );
    let rows = to_slint_rows(&view.rows);
    mark_selected(&rows, &picked);
    ui.set_local_rows(rows);
    ui.set_local_crumbs(strings(&view.crumbs));
    ui.set_local_selected(focus);
    ui.set_local_can_go_back(view.can_back);
    ui.set_local_can_go_forward(view.can_forward);
    if change.counts() && !failed {
        ui.set_local_status(view.status.as_str().into());
        clear_status(ui, ActivePane::ThisPc);
    }
}

/// The Slint tile for a site's provider: the tile's colour comes from this.
fn slint_tile(provider: SiteProvider) -> ProviderTile {
    match provider {
        SiteProvider::Bunny => ProviderTile::Bunny,
        SiteProvider::S3(preset) => match preset {
            S3Preset::AwsS3 => ProviderTile::S3,
            S3Preset::CloudflareR2 => ProviderTile::R2,
            S3Preset::BackblazeB2 => ProviderTile::B2,
            S3Preset::Wasabi => ProviderTile::Wasabi,
            S3Preset::DigitalOceanSpaces => ProviderTile::DigitalOcean,
            S3Preset::Minio => ProviderTile::Minio,
            S3Preset::Other => ProviderTile::Other,
        },
    }
}

/// Puts the sidebar's rows in the window. The same sites are updated row by row, so the rows (and
/// the keyboard focus on one of them) stay; a new list of sites replaces the model.
fn sync_sites(ui: &AppWindow, items: &[SiteItemData]) {
    let rows: Vec<SiteItem> = items
        .iter()
        .map(|item| SiteItem {
            id: item.id.as_str().into(),
            name: item.name.as_str().into(),
            subtitle: item.subtitle.as_str().into(),
            provider: slint_tile(item.provider),
            mono: item.provider.mono().into(),
            healthy: item.healthy,
        })
        .collect();
    let model = ui.get_sites();
    let same_sites = model.row_count() == rows.len()
        && rows
            .iter()
            .enumerate()
            .all(|(i, row)| model.row_data(i).is_some_and(|old| old.id == row.id));
    if same_sites {
        for (i, row) in rows.into_iter().enumerate() {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        }
    } else {
        ui.set_sites(ModelRc::new(VecModel::from(rows)));
    }
}

#[cfg(test)]
#[path = "binding_tests.rs"]
mod tests;
