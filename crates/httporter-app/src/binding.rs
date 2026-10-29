//! Connects the window to the [`Session`].
//!
//! The session lives behind a tokio mutex. Every callback that lists a folder or talks to the
//! storage runs as a task on the tokio runtime, never on the UI thread, and hands the view it
//! produced back to the window with `upgrade_in_event_loop`. A task hands its view over while it
//! still holds the session, so views reach the window in the order the session made them.
//!
//! Each pane has at most one listing in flight. Asking a pane for another one gives the first up
//! (aborts its task): the user has moved on, and a row index from the click refers to the rows on
//! screen, which the abandoned listing would have replaced. A session method changes nothing
//! until its listing is back, so an aborted task leaves the session as it was.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use httporter_bunny::REGIONS;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::add_site::{AddSiteForm, ValidSite, show_field_error};
use crate::session::{LocalView, RemoteView, Session, SiteItemData, endpoint_preview, items};
use crate::sort::SortKey;
use crate::viewmodel::{RowData, sort_key_for_column, to_slint_rows};
use crate::{AppWindow, FileRow, SiteItem};

/// What a view changes: a new listing replaces the rows; a new sort only reorders them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Listed,
    Sorted,
}

/// Something the cloud pane was asked to do.
#[derive(Clone, Copy, Debug)]
enum RemoteOp {
    Select(usize),
    Open(usize),
    Up,
    Crumb(usize),
    Sort(SortKey, bool),
}

impl RemoteOp {
    async fn run(self, session: &mut Session) -> RemoteView {
        match self {
            Self::Select(index) => session.select_site(index).await,
            Self::Open(row) => session.open_remote(row).await,
            Self::Up => session.remote_up().await,
            Self::Crumb(index) => session.remote_crumb(index).await,
            Self::Sort(key, ascending) => session.sort_remote(key, ascending),
        }
    }

    fn change(self) -> Change {
        match self {
            Self::Sort(..) => Change::Sorted,
            _ => Change::Listed,
        }
    }
}

/// Something the This PC pane was asked to do.
#[derive(Clone, Copy, Debug)]
enum LocalOp {
    Open(usize),
    Up,
    Crumb(usize),
    Sort(SortKey, bool),
}

impl LocalOp {
    async fn run(self, session: &mut Session) -> LocalView {
        match self {
            Self::Open(row) => session.open_local(row).await,
            Self::Up => session.local_up().await,
            Self::Crumb(index) => session.local_crumb(index).await,
            Self::Sort(key, ascending) => session.sort_local(key, ascending),
        }
    }

    fn change(self) -> Change {
        match self {
            Self::Sort(..) => Change::Sorted,
            _ => Change::Listed,
        }
    }
}

/// The task a pane (or the Add Site sheet) is waiting for.
#[derive(Default)]
struct Slot(RefCell<Option<Running>>);

struct Running {
    task: JoinHandle<()>,
    /// The task opens a site, rather than moving around in the open one.
    opens_site: bool,
}

impl Slot {
    /// A site is being opened: what the cloud pane shows is about to go.
    fn opening_site(&self) -> bool {
        self.0
            .borrow()
            .as_ref()
            .is_some_and(|r| r.opens_site && !r.task.is_finished())
    }

    /// Waits for `task` instead, giving up the task waited for so far.
    fn replace(&self, task: JoinHandle<()>, opens_site: bool) {
        if let Some(old) = self.0.replace(Some(Running { task, opens_site })) {
            old.task.abort();
        }
    }

    fn abort(&self) {
        if let Some(old) = self.0.take() {
            old.task.abort();
        }
    }
}

struct Ctx {
    ui: Weak<AppWindow>,
    rt: Handle,
    session: Arc<Mutex<Session>>,
    remote: Slot,
    local: Slot,
    sheet: Slot,
    /// Counts the sheet's tests and saves, and its cancels. A result is shown only while its run
    /// is the latest, so a test that finishes after a cancel (or after the sheet reopened) is
    /// dropped.
    sheet_run: Arc<AtomicU64>,
}

impl Ctx {
    fn remote(&self, op: RemoteOp) {
        let tracked = !matches!(op, RemoteOp::Sort(..));
        let opens_site = matches!(op, RemoteOp::Select(_));
        // While another site opens, the rows on screen are on their way out: a click on one of
        // them (or Backspace) means nothing any more.
        if tracked && !opens_site && self.remote.opening_site() {
            return;
        }
        let session = self.session.clone();
        let ui = self.ui.clone();
        let task = self.rt.spawn(async move {
            let mut session = session.lock().await;
            let view = op.run(&mut session).await;
            let sites = session.site_items();
            let banner = session.error_banner();
            // An error means the window is gone (or, in tests, that there is no event loop).
            let _ = ui.upgrade_in_event_loop(move |ui| {
                apply_remote(&ui, &view, op.change(), &banner);
                sync_sites(&ui, &sites);
            });
        });
        if tracked {
            self.remote.replace(task, opens_site);
        }
    }

    fn local(&self, op: LocalOp) {
        let session = self.session.clone();
        let ui = self.ui.clone();
        let task = self.rt.spawn(async move {
            let mut session = session.lock().await;
            let view = op.run(&mut session).await;
            let banner = session.error_banner();
            let _ = ui.upgrade_in_event_loop(move |ui| {
                apply_local(&ui, &view, op.change(), &banner);
            });
        });
        if !matches!(op, LocalOp::Sort(..)) {
            self.local.replace(task, false);
        }
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
            // The test does not need the session once it has started.
            let test = session.lock().await.test_connection(&site);
            let result = test.await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if runs.load(Ordering::SeqCst) == run {
                    ui.set_add_site_busy(false);
                    ui.set_add_site_result(result.text.into());
                    ui.set_add_site_result_ok(result.ok);
                }
            });
        });
        self.sheet.replace(task, false);
    }

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
            let mut session = session.lock().await;
            let saved = session.save_site(&site).await;
            let sites = session.site_items();
            let index = saved.as_ref().ok().and_then(|record| {
                session
                    .sites()
                    .iter()
                    .position(|s| s.id == record.id)
                    .and_then(|i| i32::try_from(i).ok())
            });
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let latest = runs.load(Ordering::SeqCst) == run;
                match saved {
                    Ok(_) => {
                        // Saved even if the sheet was cancelled meanwhile: the list must show it.
                        sync_sites(&ui, &sites);
                        if latest {
                            ui.set_add_site_busy(false);
                            ui.set_add_site_open(false);
                        }
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
            });
        });
        self.sheet.replace(task, false);
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
pub fn install(ui: &AppWindow, rt: Handle, session: Session) -> Arc<Mutex<Session>> {
    let regions: Vec<SharedString> = REGIONS.iter().map(|(_, city)| (*city).into()).collect();
    ui.set_region_names(ModelRc::new(VecModel::from(regions)));
    let sites = session.site_items();
    sync_sites(ui, &sites);
    apply_local(
        ui,
        &session.local_view(),
        Change::Listed,
        &session.error_banner(),
    );

    let session = Arc::new(Mutex::new(session));
    let ctx = Rc::new(Ctx {
        ui: ui.as_weak(),
        rt,
        session: session.clone(),
        remote: Slot::default(),
        local: Slot::default(),
        sheet: Slot::default(),
        sheet_run: Arc::new(AtomicU64::new(0)),
    });
    let row = |index: i32| usize::try_from(index).ok();

    ui.on_site_selected({
        let ctx = ctx.clone();
        move |index| {
            let Some(index) = row(index) else { return };
            if let Some(ui) = ctx.ui.upgrade() {
                clear_remote_rows(&ui);
            }
            ctx.remote(RemoteOp::Select(index));
        }
    });
    ui.on_open_remote({
        let ctx = ctx.clone();
        move |index| {
            if let Some(index) = row(index) {
                ctx.remote(RemoteOp::Open(index));
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
                ctx.remote(RemoteOp::Crumb(index));
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
                ctx.local(LocalOp::Open(index));
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
                ctx.local(LocalOp::Crumb(index));
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

/// The selection for a pane's new rows: none in a new folder; otherwise (a new sort, or a failed
/// listing that left the folder as it was) the same row as before.
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

fn apply_remote(ui: &AppWindow, view: &RemoteView, change: Change, banner: &str) {
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
    ui.set_connection_ok(!failed);
    ui.set_endpoint_text(view.endpoint.as_str().into());
    ui.set_remote_chip(view.chip.as_str().into());
    ui.set_status_right(view.status_right.as_str().into());
    ui.set_error_text(banner.into());
    if change == Change::Listed && !failed {
        ui.set_status_left(items(view.rows.len()).into());
    }
}

/// Empties the cloud pane while another site opens, so the old site's files are not shown under
/// the new site's name (the pane title follows the sidebar at once). The rest of the window keeps
/// what it shows until the new site's view arrives.
fn clear_remote_rows(ui: &AppWindow) {
    ui.set_remote_rows(ModelRc::default());
    ui.set_remote_crumbs(ModelRc::default());
    ui.set_remote_selected(-1);
}

fn apply_local(ui: &AppWindow, view: &LocalView, change: Change, banner: &str) {
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
    ui.set_error_text(banner.into());
    if change == Change::Listed && !failed {
        ui.set_status_left(view.status.as_str().into());
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
