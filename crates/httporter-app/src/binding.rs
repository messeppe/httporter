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
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use httporter_bunny::REGIONS;
use httporter_core::{S3Preset, SiteRecord};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::add_site::{AddSiteForm, ValidSite, show_field_error};
use crate::copy;
use crate::selection::{ActivePane, Selection, ToolbarInputs, ToolbarState, toolbar_state};
use crate::session::{
    LocalView, RemoteView, Session, SiteItemData, SiteProvider, Ticket, endpoint_preview,
};
use crate::sort::SortKey;
use crate::viewmodel::{RowData, sort_key_for_column, to_slint_rows};
use crate::{AppWindow, FileRow, ProviderTile, SiteItem};

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
    /// The transfer engine can act on the toolbar's commands. Until Task 23 sets it, every
    /// `can-*` stays false (see [`refresh_toolbar`]).
    transfers_ready: bool,
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

    /// The banner: the app's notice, then the cloud pane's error, then This PC's, one per line.
    fn banner(&self, notice: Option<String>) -> String {
        [
            notice.as_deref(),
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

struct Ctx {
    ui: Weak<AppWindow>,
    rt: Handle,
    session: Arc<Session>,
    shown: Arc<Mutex<Shown>>,
    /// The site the cloud pane is opening, while it is on its way.
    opening: Slot,
    sheet: Slot,
    /// Counts the sheet's tests and saves, and its cancels. A result is shown only while its run
    /// is the latest, so a test that finishes after a cancel (or after the sheet reopened) is
    /// dropped.
    sheet_run: Arc<AtomicU64>,
}

impl Ctx {
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
        // Taken here, at the click, so that requests count in the order the user made them.
        let ticket = self.session.remote_ticket();
        let (session, shown, ui) = (self.session.clone(), self.shown.clone(), self.ui.clone());
        let task = self.rt.spawn(async move {
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
                RemoteOp::Open(row, seen) => session.open_remote(ticket, row, seen, started).await,
                RemoteOp::Up => session.remote_up(ticket, started).await,
                RemoteOp::Crumb(index, seen) => {
                    session.remote_crumb(ticket, index, seen, started).await
                }
                RemoteOp::Back => session.remote_back(ticket, started).await,
                RemoteOp::Forward => session.remote_forward(ticket, started).await,
                RemoteOp::Refresh => session.remote_refresh(ticket, started).await,
                RemoteOp::Sort(key, ascending) => session.sort_remote(key, ascending),
            };
            post_remote(&ui, &shown, &session, view, op.change());
        });
        if matches!(op, RemoteOp::Select(_)) {
            // A site picked before this one is of no interest any more. (Older listings within a
            // site need no aborting: the session drops their answers.)
            self.opening.replace(task);
        }
    }

    fn local(&self, op: LocalOp) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_local_pane_active(true);
        }
        let ticket = self.session.local_ticket();
        let (session, shown, ui) = (self.session.clone(), self.shown.clone(), self.ui.clone());
        self.rt.spawn(async move {
            let started = {
                let (session, shown, ui) = (session.clone(), shown.clone(), ui.clone());
                move |view| post_local(&ui, &shown, &session, view, Change::Loading)
            };
            let view = op.run(&session, ticket, started).await;
            post_local(&ui, &shown, &session, view, op.change());
        });
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
        ui.set_status_message(SharedString::new());
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

    fn test_site(&self, form: AddSiteForm) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(site) = validated(&ui, &form) else {
            return;
        };
        let run = self.begin_sheet_run(&ui);
        let (session, runs, weak) = (
            self.session.clone(),
            self.sheet_run.clone(),
            self.ui.clone(),
        );
        let task = self.rt.spawn(async move {
            let result = session.test_connection(&site).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if runs.load(Ordering::SeqCst) == run {
                    ui.set_add_site_busy(false);
                    ui.set_add_site_result(result.text.into());
                    ui.set_add_site_result_ok(result.ok);
                }
            });
        });
        self.sheet.replace(task);
    }

    /// Add Site (also Return in a field of the sheet): tests the connection, and saves the site
    /// only if that works. The sheet stays busy meanwhile. Cancel gives the test up; once the
    /// test has passed, the save is a task of its own that Cancel does not reach, so a site whose
    /// password is being written is always saved, listed and shown (see [`show_added`]).
    fn save_site(&self, form: AddSiteForm) {
        let Some(ui) = self.ui.upgrade() else { return };
        let Some(site) = validated(&ui, &form) else {
            return;
        };
        let run = self.begin_sheet_run(&ui);
        let (session, runs, weak, rt) = (
            self.session.clone(),
            self.sheet_run.clone(),
            self.ui.clone(),
            self.rt.clone(),
        );
        let task = self.rt.spawn(async move {
            let tested = session.test_connection(&site).await;
            if !tested.ok {
                post_added(&weak, &runs, run, &session, Err(tested.text));
                return;
            }
            rt.spawn(async move {
                let added = session.save_site(&site).await;
                post_added(&weak, &runs, run, &session, added);
            });
        });
        self.sheet.replace(task);
    }

    /// The sheet was cancelled: give up its test (a save that has begun is not given up) and let it
    /// be used again.
    fn cancel_sheet(&self) {
        self.sheet_run.fetch_add(1, Ordering::SeqCst);
        self.sheet.abort();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_add_site_busy(false);
        }
    }
}

/// Hands what became of Add Site to the window (from a task). Whether the run is still the
/// latest is decided when the window gets to it.
fn post_added(
    ui: &Weak<AppWindow>,
    runs: &Arc<AtomicU64>,
    run: u64,
    session: &Arc<Session>,
    added: Result<SiteRecord, String>,
) {
    let (runs, session) = (runs.clone(), session.clone());
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let latest = runs.load(Ordering::SeqCst) == run;
        show_added(&ui, &session, latest, added);
    });
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
            sync_sites(ui, &session.site_items());
            if latest {
                ui.set_add_site_busy(false);
                ui.set_add_site_open(false);
            }
            let index = session
                .sites()
                .iter()
                .position(|s| s.id == record.id)
                .and_then(|i| i32::try_from(i).ok());
            if let Some(index) = index {
                ui.set_selected_site(index);
                ui.invoke_site_selected(index);
            }
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

/// Checks the sheet: shows the first problem under its field (and returns `None`) or clears them.
fn validated(ui: &AppWindow, form: &AddSiteForm) -> Option<ValidSite> {
    match form.validate(&REGIONS.map(|(code, _)| code)) {
        Ok(site) => {
            show_field_error(ui, None);
            Some(site)
        }
        Err(error) => {
            show_field_error(ui, Some(&error));
            None
        }
    }
}

/// Wires every callback of `ui` to `session`, fills the window with the session's starting state
/// and selects the first site, if there is one. Returns the shared session.
///
/// The toolbar's commands stay off (every `can-*` false) until Task 23 has the transfer engine
/// act on them and installs with `transfers_ready`.
pub fn install(ui: &AppWindow, rt: Handle, session: Session) -> Arc<Session> {
    install_with(ui, rt, session, false)
}

/// As [`install`], with the toolbar's commands switched on (`transfers_ready`) or off.
fn install_with(
    ui: &AppWindow,
    rt: Handle,
    session: Session,
    transfers_ready: bool,
) -> Arc<Session> {
    let regions: Vec<SharedString> = REGIONS.iter().map(|(_, city)| (*city).into()).collect();
    ui.set_region_names(ModelRc::new(VecModel::from(regions)));
    let session = Arc::new(session);
    let shown = Arc::new(Mutex::new(Shown::default()));
    let local = session.local_view();
    {
        let mut shown = lock(&shown);
        shown.local = local.version;
        shown.local_error = local.error.clone();
        shown.transfers_ready = transfers_ready;
        apply_local(ui, &local, Change::Listed);
        adopt(ui, &mut shown, ActivePane::ThisPc, &local.rows);
        ui.set_error_text(shown.banner(session.notice()).into());
        refresh_toolbar(ui, &shown);
    }
    let sites = session.site_items();
    sync_sites(ui, &sites);

    let ctx = Rc::new(Ctx {
        ui: ui.as_weak(),
        rt,
        session: session.clone(),
        shown,
        opening: Slot::default(),
        sheet: Slot::default(),
        sheet_run: Arc::new(AtomicU64::new(0)),
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
    ui.on_add_site_test({
        let ctx = ctx.clone();
        move |name, zone, region, password| {
            ctx.test_site(AddSiteForm::from_sheet(&name, &zone, region, &password));
        }
    });
    ui.on_add_site_save({
        let ctx = ctx.clone();
        move |name, zone, region, password| {
            ctx.save_site(AddSiteForm::from_sheet(&name, &zone, region, &password));
        }
    });
    ui.on_add_site_cancel({
        let ctx = ctx.clone();
        move || ctx.cancel_sheet()
    });

    if !sites.is_empty() {
        ui.set_selected_site(0);
        ctx.remote(RemoteOp::Select(0));
    }
    session
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
    if view.version > shown.remote {
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

/// Another site was picked in the sidebar. The pane title follows the sidebar at once, so the
/// cloud pane is emptied (not the old site's files under the new site's name) and the old site's
/// error leaves the banner. The rest of the window keeps what it shows until the new site's view
/// arrives.
fn start_opening_site(ui: &AppWindow, shown: &Mutex<Shown>, notice: Option<String>) {
    ui.set_remote_rows(ModelRc::default());
    ui.set_remote_crumbs(ModelRc::default());
    ui.set_remote_selected(-1);
    ui.set_remote_selection_text(SharedString::new());
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
    match pane {
        ActivePane::ThisPc => ui.set_local_selection_text(text),
        ActivePane::Cloud => ui.set_remote_selection_text(text),
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

/// Sets which of the toolbar's commands may be pressed (and the notes that say why not). Until
/// the transfer engine is wired in (`transfers_ready`), none may: the buttons are off, not inert.
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

// ---- The toolbar's commands and the context menus' ----
//
// Stubs for Task 23, which wires them to the transfer engine and the file dialogs (Ruling Q6).
// Until it sets `transfers_ready` every `can-*` is false, so the buttons, the menu items and the
// shortcuts that reach these are off.

/// Upload: the This PC selection into the cloud pane's folder, or the file picker with none.
fn upload(_ctx: &Ctx) {}

/// Download: the cloud selection into the This PC pane's folder (asks at the list of drives).
fn download(_ctx: &Ctx) {}

/// Download to…: the cloud selection into a folder asked for.
fn download_to(_ctx: &Ctx) {}

/// New folder, in the pane in use.
fn new_folder(_ctx: &Ctx) {}

/// Delete the cloud selection (after the alert).
fn delete(_ctx: &Ctx) {}

/// Show row `row` of the This PC pane in Explorer.
fn show_in_explorer(_ctx: &Ctx, _row: usize) {}

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
    if change.counts() && !failed {
        ui.set_remote_status(copy::items(view.rows.len()).into());
        ui.set_status_message(SharedString::new());
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
        ui.set_status_message(SharedString::new());
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
