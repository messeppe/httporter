// SPDX-License-Identifier: MIT OR Apache-2.0

//! The activity log and its sheet.
//!
//! The log is opened at start ([`open_log`]); anything worth remembering goes through [`log`]:
//! every transfer job that ends (a task maps the engine's events), publishes, purges, moves,
//! renames, edits and sites added or removed. The sheet reads the newest [`SHOWN`] events that
//! pass its filter, off the UI thread, and reads them again (at most once a second) while it is
//! open and new events come in.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use chrono::{Local, Utc};
use httporter_core::{ActivityEvent, ActivityFilter, ActivityKind, JobKind};
use httporter_platform::ActivityLog;
use httporter_platform::win;
use httporter_transfer::{JobEvent, JobEventOutcome};
use slint::{ModelRc, SharedString, VecModel};
use tokio::sync::broadcast::error::RecvError;

use super::{Manager, copy};
use crate::session::blocking;
use crate::transfers::Transfers;
use crate::{ActivityFilterData, ActivityRow, AppWindow};

/// How many events the sheet lists.
const SHOWN: usize = 2000;
/// The search waits this long after the last key.
const SEARCH_WAIT: Duration = Duration::from_millis(250);
/// New events are read in at most this often while the sheet is open.
const RELOAD_EVERY: Duration = Duration::from_secs(1);

/// The log, once [`open_log`] opened it.
static LOG: OnceLock<ActivityLog> = OnceLock::new();
/// The manager whose sheet [`log`] brings up to date.
static VIEWER: Mutex<Option<Weak<Manager>>> = Mutex::new(None);

/// What this view remembers between callbacks.
#[derive(Default)]
pub struct ActivityState {
    /// Goes up with every filter change; a read that finds it moved is dropped.
    serial: u64,
    /// The filter in use.
    filter: ActivityFilter,
    /// The site pop-up's sites, in its order (after "All sites").
    sites: Vec<String>,
    /// A read for new events is on its way.
    reload_pending: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires this view's callbacks of `ui` to `m`, Retry Selected of the drawer (M11), and the task
/// that logs the transfer engine's events.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    *lock(&VIEWER) = Some(Arc::downgrade(m));
    ui.on_activity_filter_changed({
        let m = m.clone();
        move |data| filter_changed(&m, data)
    });
    ui.on_activity_export({
        let m = m.clone();
        move || export(&m)
    });
    ui.on_activity_open_folder({
        let m = m.clone();
        move || {
            if let Some(log) = LOG.get() {
                let dir = log.dir().to_path_buf();
                m.link.rt().spawn_blocking(move || {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = win::open_default(&dir);
                });
            }
        }
    });
    crate::drawer_view::install_retry_selected(ui, m.transfers.clone());
    spawn_engine_events(m.link.rt(), m.transfers.clone());
}

/// Opens the activity sheet (the palette and Settings › Advanced). Opening it reads the rows.
#[allow(dead_code)] // T7 and T8 open it.
pub fn show(ui: &AppWindow) {
    ui.set_activity_open(true);
}

/// Opens the activity log in `dir` (`main.rs`, at start). Without it, [`log`] keeps nothing.
pub fn open_log(dir: PathBuf) {
    if let Ok(log) = ActivityLog::open(&dir) {
        let _ = LOG.set(log);
    }
}

/// The folder the activity log is in, once open.
#[allow(dead_code)] // T8: Settings › Advanced, Open Log Folder.
pub fn log_dir() -> Option<PathBuf> {
    LOG.get().map(|log| log.dir().to_path_buf())
}

/// Adds `e` to the activity log; nothing without an open log. An open sheet shows it shortly.
pub fn log(e: ActivityEvent) {
    let Some(log) = LOG.get() else {
        return;
    };
    log.append(&e);
    let viewer = lock(&VIEWER).as_ref().and_then(Weak::upgrade);
    if let Some(m) = viewer {
        schedule_reload(&m);
    }
}

/// An event that happened now.
pub fn event(kind: ActivityKind, site: &str, path: &str, ok: bool, detail: String, bytes: u64) {
    log(ActivityEvent {
        at: Utc::now(),
        kind,
        site: site.to_owned(),
        path: path.to_owned(),
        ok,
        detail,
        bytes,
    });
}

/// A transfer job that ended, as the window logs the engine's (I6: the command line logs its own
/// engine's this way).
pub fn job_ended(e: JobEvent) {
    if let Some(event) = event_of(e) {
        log(event);
    }
}

/// A site was added (Add Site, Find Zones, Duplicate).
pub fn site_added(name: &str) {
    event(
        ActivityKind::Site,
        name,
        "",
        true,
        copy::SITE_ADDED.to_owned(),
        0,
    );
}

/// A site was removed.
pub fn site_removed(name: &str) {
    event(
        ActivityKind::Site,
        name,
        "",
        true,
        copy::SITE_REMOVED.to_owned(),
        0,
    );
}

/// Logs every transfer job that ends (cancelled ones aside). The engine's channel drops the
/// oldest events for a reader that lags; those are not logged.
fn spawn_engine_events(rt: &crate::rt::Rt, transfers: Arc<Transfers>) {
    rt.spawn(async move {
        let mut events = transfers.engine().events();
        loop {
            match events.recv().await {
                Ok(e) => {
                    if let Some(event) = event_of(e) {
                        log(event);
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// The activity event of an ended job; none for a cancelled one or a new folder made on the way.
fn event_of(e: JobEvent) -> Option<ActivityEvent> {
    let kind = match e.kind {
        JobKind::Upload => ActivityKind::Upload,
        JobKind::Download => ActivityKind::Download,
        JobKind::Delete => ActivityKind::Delete,
        JobKind::CreateFolder => return None,
    };
    let (ok, detail) = match e.outcome {
        JobEventOutcome::Done => (true, String::new()),
        JobEventOutcome::Failed(why) => (false, why),
        JobEventOutcome::Cancelled => return None,
    };
    Some(ActivityEvent {
        at: Utc::now(),
        kind,
        site: e.site_name,
        path: e.path,
        ok,
        detail,
        bytes: e.bytes,
    })
}

/// Reads the rows again in a moment, if the sheet is open then (and no read is on its way).
fn schedule_reload(m: &Arc<Manager>) {
    {
        let mut state = lock(&m.activity);
        if state.reload_pending {
            return;
        }
        state.reload_pending = true;
    }
    let m2 = m.clone();
    m.link.rt().spawn(async move {
        tokio::time::sleep(RELOAD_EVERY).await;
        lock(&m2.activity).reload_pending = false;
        let ui = m2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if ui.get_activity_open() {
                load(&m2, &ui, Duration::ZERO);
            }
        });
    });
}

/// The sheet's controls changed (or it opened): the site pop-up is filled in and the rows read
/// for the new filter (after a pause while the search is typed).
fn filter_changed(m: &Arc<Manager>, data: ActivityFilterData) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let sites: Vec<String> = m.session.sites().into_iter().map(|s| s.name).collect();
    let mut names = vec![SharedString::from(copy::ALL_SITES)];
    names.extend(sites.iter().map(|name| SharedString::from(name.as_str())));
    ui.set_activity_sites(ModelRc::new(VecModel::from(names)));
    let kind = match data.kind {
        1 => Some(ActivityKind::Upload),
        2 => Some(ActivityKind::Download),
        3 => Some(ActivityKind::Delete),
        4 => Some(ActivityKind::Move),
        5 => Some(ActivityKind::Publish),
        _ => None,
    };
    let filter = ActivityFilter {
        kind,
        failed_only: data.kind == 6,
        site: usize::try_from(data.site)
            .ok()
            .and_then(|i| sites.get(i).cloned()),
        text: data.text.to_string(),
    };
    let wait = {
        let mut state = lock(&m.activity);
        let typed = state.filter.text != filter.text;
        state.filter = filter;
        state.sites = sites;
        if typed { SEARCH_WAIT } else { Duration::ZERO }
    };
    load(m, &ui, wait);
}

/// Reads the newest events that pass the filter (after `wait`), off the UI thread, and lists them.
fn load(m: &Arc<Manager>, _ui: &AppWindow, wait: Duration) {
    let (serial, filter) = {
        let mut state = lock(&m.activity);
        state.serial += 1;
        (state.serial, state.filter.clone())
    };
    let m2 = m.clone();
    m.link.rt().spawn(async move {
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
            if lock(&m2.activity).serial != serial {
                return;
            }
        }
        let events = blocking(move || {
            LOG.get()
                .map(|log| log.read(&filter, SHOWN))
                .unwrap_or_default()
        })
        .await;
        let now = Local::now().naive_local();
        let rows: Vec<ActivityRow> = events
            .iter()
            .map(|e| ActivityRow {
                when: copy::activity_when(e.at.with_timezone(&Local).naive_local(), now).into(),
                kind: copy::activity_kind(e.kind).into(),
                site: e.site.as_str().into(),
                path: e.path.as_str().into(),
                detail: copy::activity_detail(e).into(),
                failed: !e.ok,
            })
            .collect();
        let ui = m2.link.ui().clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if lock(&m2.activity).serial == serial {
                ui.set_activity_rows(ModelRc::new(VecModel::from(rows)));
            }
        });
    });
}

/// Export…: the events that pass the filter, as text, to a file picked.
fn export(m: &Arc<Manager>) {
    let Some(log) = LOG.get() else {
        return;
    };
    let filter = lock(&m.activity).filter.clone();
    let link = m.link.clone();
    m.link.rt().spawn(async move {
        let written = blocking(move || {
            let to = win::pick_save_file(
                copy::EXPORT_ACTIVITY_TITLE,
                copy::EXPORT_ACTIVITY_NAME,
                &[(copy::TEXT_FILES, &["txt"])],
            )?;
            Some(log.export_text(&filter, &to).map_err(|e| e.to_string()))
        })
        .await;
        match written {
            Some(Ok(n)) => link.status(copy::activity_exported(n)),
            Some(Err(e)) => link.status(copy::activity_not_exported(&e)),
            None => {}
        }
    });
}
