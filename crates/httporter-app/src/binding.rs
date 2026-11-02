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
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use httporter_bunny::REGIONS;
use httporter_core::{S3Preset, SiteRecord};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::add_site::{AddSiteForm, ValidSite, show_field_error};
use crate::session::{
    LocalView, RemoteView, Session, SiteItemData, SiteProvider, Ticket, endpoint_preview, items,
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
}

impl Shown {
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
    /// only if that works ([`Session::add_site`]). The sheet stays busy meanwhile, and Cancel
    /// gives the whole run up.
    fn save_site(&self, form: AddSiteForm) {
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
            let added = session.add_site(&site).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let latest = runs.load(Ordering::SeqCst) == run;
                show_added(&ui, &session, latest, added);
            });
        });
        self.sheet.replace(task);
    }

    /// The sheet was cancelled: give up its test or save and let it be used again.
    fn cancel_sheet(&self) {
        self.sheet_run.fetch_add(1, Ordering::SeqCst);
        self.sheet.abort();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_add_site_busy(false);
        }
    }
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
pub fn install(ui: &AppWindow, rt: Handle, session: Session) -> Arc<Session> {
    let regions: Vec<SharedString> = REGIONS.iter().map(|(_, city)| (*city).into()).collect();
    ui.set_region_names(ModelRc::new(VecModel::from(regions)));
    let session = Arc::new(session);
    let shown = Arc::new(Mutex::new(Shown::default()));
    let local = session.local_view();
    {
        let mut shown = lock(&shown);
        shown.local = local.version;
        shown.local_error = local.error.clone();
        apply_local(ui, &local, Change::Listed);
        ui.set_error_text(shown.banner(session.notice()).into());
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
    }
    ui.set_error_text(shown.banner(session.notice()).into());
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
    }
    ui.set_error_text(shown.banner(session.notice()).into());
}

/// Another site was picked in the sidebar. The pane title follows the sidebar at once, so the
/// cloud pane is emptied (not the old site's files under the new site's name) and the old site's
/// error leaves the banner. The rest of the window keeps what it shows until the new site's view
/// arrives.
fn start_opening_site(ui: &AppWindow, shown: &Mutex<Shown>, notice: Option<String>) {
    ui.set_remote_rows(ModelRc::default());
    ui.set_remote_crumbs(ModelRc::default());
    ui.set_remote_selected(-1);
    let mut shown = lock(shown);
    shown.remote_error = None;
    ui.set_error_text(shown.banner(notice).into());
}

fn strings(list: &[String]) -> ModelRc<SharedString> {
    let list: Vec<SharedString> = list.iter().map(|s| s.as_str().into()).collect();
    ModelRc::new(VecModel::from(list))
}

/// Where the row that was selected in `old` is in `rows` (found by name), or -1.
fn reselect(old: &ModelRc<FileRow>, selected: i32, rows: &[RowData]) -> i32 {
    usize::try_from(selected)
        .ok()
        .and_then(|index| old.row_data(index))
        .and_then(|row| rows.iter().position(|r| r.name == row.name.as_str()))
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1)
}

/// The selection for a pane's new rows: none in a new folder; otherwise (a refresh, a new sort,
/// or a failed listing that left the folder as it was) the same row as before.
fn selection(
    change: Change,
    failed: bool,
    old: &ModelRc<FileRow>,
    selected: i32,
    rows: &[RowData],
) -> i32 {
    if change == Change::Listed && !failed {
        -1
    } else {
        reselect(old, selected, rows)
    }
}

/// Puts a cloud view on screen (everything but the banner, which [`Shown`] puts together). A
/// loading view only marks the pane: its rows stay until the listing is in.
fn apply_remote(ui: &AppWindow, view: &RemoteView, change: Change) {
    ui.set_remote_loading(view.loading);
    if change == Change::Loading {
        return;
    }
    let failed = view.error.is_some();
    let selected = selection(
        change,
        failed,
        &ui.get_remote_rows(),
        ui.get_remote_selected(),
        &view.rows,
    );
    ui.set_remote_rows(to_slint_rows(&view.rows));
    ui.set_remote_crumbs(strings(&view.crumbs));
    ui.set_remote_selected(selected);
    ui.set_connection_text(view.connection.as_str().into());
    ui.set_connection_ok(view.connection_ok);
    ui.set_endpoint_text(view.endpoint.as_str().into());
    ui.set_remote_chip(view.chip.as_str().into());
    ui.set_status_right(view.status_right.as_str().into());
    ui.set_remote_can_go_back(view.can_back);
    ui.set_remote_can_go_forward(view.can_forward);
    if change.counts() && !failed {
        ui.set_remote_status(items(view.rows.len()).into());
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
    let selected = selection(
        change,
        failed,
        &ui.get_local_rows(),
        ui.get_local_selected(),
        &view.rows,
    );
    ui.set_local_rows(to_slint_rows(&view.rows));
    ui.set_local_crumbs(strings(&view.crumbs));
    ui.set_local_selected(selected);
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
            mono: item.mono.as_str().into(),
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
