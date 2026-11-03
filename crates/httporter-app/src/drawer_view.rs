//! What the transfer drawer shows: the engine's [`EngineSnapshot`] turned into the texts, numbers
//! and rows of the window, and the code that puts them there.
//!
//! [`drawer_view`] is a plain function of the snapshot and the time, so it is tested without a
//! window or an engine. The words come from `copy.rs` and the numbers from `format.rs`; nothing is
//! written out here. [`Throttle`] keeps the engine's progress from redrawing the window more than
//! ten times a second. The bottom of the file is the Slint side: [`apply_drawer`] puts a view in the
//! window and [`install`] wires the drawer's callbacks.
//!
//! Until the engine is wired in (Task 23) the window shows the idle drawer and the callbacks do
//! nothing, but Show / Hide Transfers works: the drawer flips itself.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use httporter_core::{EngineSnapshot, EngineState, JobKind, JobView};
use slint::{Model, ModelRc, SharedString, VecModel};

use crate::format::{format_rate, format_size};
use crate::viewmodel::{FileKindData, kind_for, slint_kind};
use crate::{AppWindow, TransferRow, copy};

/// A megabyte, for the speed shown in MB/s.
const MB: f64 = 1024.0 * 1024.0;

/// Everything the drawer shows, as text and rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DrawerView {
    /// The engine has work (running, paused or stopped), not idle.
    pub running: bool,
    /// The user paused everything: the Pause button reads Resume.
    pub paused: bool,
    /// The one line of the collapsed drawer.
    pub collapsed_text: String,
    pub title: String,
    pub line: String,
    /// The speeds without their units (the window writes "files/s" and "MB/s").
    pub files_per_s: String,
    pub mb_per_s: String,
    /// The time left without "about" and "left"; empty when the engine can't tell.
    pub eta: String,
    /// Files done over files in all, 0 to 100.
    pub percent: f32,
    /// "Active 32", "Queued 5,349", "Failed 0", "Done 8,412".
    pub tab_titles: [String; 4],
    pub active: Vec<TransferRowData>,
    pub queued: Vec<TransferRowData>,
    pub failed: Vec<TransferRowData>,
    pub done: Vec<TransferRowData>,
}

/// One row of a tab, with its size and reason already written.
#[derive(Clone, Debug, PartialEq)]
pub struct TransferRowData {
    pub id: u64,
    pub name: String,
    pub kind: FileKindData,
    pub size: String,
    /// 0 to 100.
    pub percent: f32,
    /// The Failed tab's reason, or "Paused" on a queued row.
    pub detail: String,
    /// A queued job paused on its own: its menu offers Resume rather than Pause.
    pub paused: bool,
}

/// Which tab a row is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum List {
    Active,
    Queued,
    Failed,
    Done,
}

/// Whether a job is paused on its own. The only place a row reads it.
fn row_paused(view: &JobView) -> bool {
    view.paused
}

/// How many jobs run now: the engine's count, since `active` may hold only a window onto them.
fn active_count(s: &EngineSnapshot) -> u64 {
    s.active_count
}

/// How many jobs have failed: the engine's count, or the rows it sent if there are more of them.
fn failed_count(s: &EngineSnapshot) -> u64 {
    s.files_failed.max(s.failed.len() as u64)
}

/// `done` over `total` as a percentage, 0 to 100; 0 when there is nothing to do.
fn percent(done: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64 * 100.0).clamp(0.0, 100.0) as f32
    }
}

fn row(view: &JobView, list: List) -> TransferRowData {
    let paused = row_paused(view);
    let percent = match list {
        List::Done => 100.0,
        List::Queued => 0.0,
        List::Active | List::Failed => percent(view.done_bytes, view.size),
    };
    let detail = match list {
        List::Failed => view.error.clone().unwrap_or_default(),
        List::Queued if paused => copy::ROW_PAUSED.to_owned(),
        _ => String::new(),
    };
    let kind = match view.kind {
        JobKind::CreateFolder => FileKindData::Folder,
        _ => kind_for(&view.path, false),
    };
    TransferRowData {
        id: view.id.0,
        name: view.path.clone(),
        kind,
        size: format_size(view.size),
        percent,
        detail,
        paused,
    }
}

fn rows(views: &[JobView], list: List) -> Vec<TransferRowData> {
    views.iter().map(|view| row(view, list)).collect()
}

/// The drawer's view of `s` at time `now` (which dates the last batch: "Last transfer 2 hours
/// ago").
pub fn drawer_view(s: &EngineSnapshot, now: DateTime<Utc>) -> DrawerView {
    let running = s.state != EngineState::Idle;
    let files_per_s = format_rate(s.files_per_s);
    let last = s.last_batch.as_ref();
    let (title, line, collapsed_text) = if running {
        let title = copy::drawer_title(&s.state, s.direction, &s.site);
        let line = copy::drawer_line(
            s.direction,
            s.files_done,
            s.files_total,
            s.bytes_done,
            s.bytes_total,
            &s.host,
        );
        // Only a plain run has a speed to quote; any other state says why it isn't going.
        let collapsed = if s.state == EngineState::Running {
            copy::drawer_collapsed_running(s.direction, s.files_done, s.files_total, &files_per_s)
        } else {
            title.clone()
        };
        (title, line, collapsed)
    } else {
        (
            copy::NO_TRANSFERS.to_owned(),
            last.map(|batch| copy::drawer_last_transfer(batch, now))
                .unwrap_or_default(),
            copy::drawer_idle(last, now),
        )
    };
    DrawerView {
        running,
        paused: s.state == EngineState::Paused,
        collapsed_text,
        title,
        line,
        files_per_s,
        mb_per_s: format_rate(s.bytes_per_s / MB),
        eta: s.eta.map(copy::drawer_eta).unwrap_or_default(),
        percent: percent(s.files_done, s.files_total),
        tab_titles: copy::drawer_tab_titles(
            active_count(s),
            s.queued_count,
            failed_count(s),
            s.done_count,
        ),
        active: rows(&s.active, List::Active),
        queued: rows(&s.queued, List::Queued),
        failed: rows(&s.failed, List::Failed),
        done: rows(&s.done, List::Done),
    }
}

/// How often the engine's progress may redraw the drawer.
#[allow(dead_code)] // The engine's progress feed uses it (Task 23).
pub const REDRAW_EVERY: Duration = Duration::from_millis(100);

/// Lets one thing through per `min`, so a feed of progress reports redraws the window at a steady
/// pace however fast they arrive.
#[allow(dead_code)] // The engine's progress feed uses it (Task 23).
pub struct Throttle {
    min: Duration,
    last: Option<Instant>,
}

#[allow(dead_code)] // The engine's progress feed uses it (Task 23).
impl Throttle {
    pub fn new(min: Duration) -> Self {
        Self { min, last: None }
    }

    /// Whether `now` is far enough from the last time this said yes. The first call says yes.
    pub fn ready(&mut self, now: Instant) -> bool {
        let wait = self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < self.min);
        if !wait {
            self.last = Some(now);
        }
        !wait
    }
}

// ---- The window ----

fn slint_rows(rows: &[TransferRowData]) -> Vec<TransferRow> {
    rows.iter()
        .map(|row| TransferRow {
            id: i32::try_from(row.id).unwrap_or(i32::MAX),
            name: row.name.as_str().into(),
            kind: slint_kind(row.kind),
            size: row.size.as_str().into(),
            percent: row.percent,
            detail: row.detail.as_str().into(),
            paused: row.paused,
        })
        .collect()
}

/// Brings the model a window property holds up to `rows` row by row, so the list keeps its scroll
/// position and its focus while progress arrives. Returns a new model when the property holds none
/// of ours yet, for the caller to set.
fn synced<T: Clone + PartialEq + 'static>(
    current: &ModelRc<T>,
    rows: Vec<T>,
) -> Option<ModelRc<T>> {
    let Some(model) = current.as_any().downcast_ref::<VecModel<T>>() else {
        return Some(ModelRc::new(VecModel::from(rows)));
    };
    let kept = model.row_count();
    for (i, row) in rows.iter().enumerate() {
        if i >= kept {
            model.push(row.clone());
        } else if model.row_data(i).as_ref() != Some(row) {
            model.set_row_data(i, row.clone());
        }
    }
    for i in (rows.len()..kept).rev() {
        model.remove(i);
    }
    None
}

/// Puts `view` in the window.
pub fn apply_drawer(ui: &AppWindow, view: &DrawerView) {
    ui.set_transfers_running(view.running);
    ui.set_transfers_paused(view.paused);
    ui.set_transfers_collapsed_text(view.collapsed_text.as_str().into());
    ui.set_transfers_title(view.title.as_str().into());
    ui.set_transfers_line(view.line.as_str().into());
    ui.set_transfers_files_per_s(view.files_per_s.as_str().into());
    ui.set_transfers_mb_per_s(view.mb_per_s.as_str().into());
    ui.set_transfers_eta(view.eta.as_str().into());
    ui.set_transfers_percent(view.percent);
    let titles = view
        .tab_titles
        .iter()
        .map(|title| SharedString::from(title.as_str()))
        .collect();
    if let Some(model) = synced(&ui.get_transfers_tab_titles(), titles) {
        ui.set_transfers_tab_titles(model);
    }
    if let Some(model) = synced(&ui.get_transfers_active(), slint_rows(&view.active)) {
        ui.set_transfers_active(model);
    }
    if let Some(model) = synced(&ui.get_transfers_queued(), slint_rows(&view.queued)) {
        ui.set_transfers_queued(model);
    }
    if let Some(model) = synced(&ui.get_transfers_failed(), slint_rows(&view.failed)) {
        ui.set_transfers_failed(model);
    }
    if let Some(model) = synced(&ui.get_transfers_done(), slint_rows(&view.done)) {
        ui.set_transfers_done(model);
    }
}

/// Shows the idle drawer and wires its callbacks. Task 23 replaces each stub below with a call
/// into the engine and feeds the window with [`apply_drawer`]; until then nothing runs, so the
/// buttons that act on transfers have nothing to act on. Show / Hide Transfers needs no wiring: the
/// drawer flips itself.
pub fn install(ui: &AppWindow) {
    apply_drawer(ui, &drawer_view(&EngineSnapshot::default(), Utc::now()));
    ui.on_transfers_toggle(|| {});
    ui.on_transfers_pause(|| {});
    ui.on_transfers_resume(|| {});
    ui.on_transfers_cancel_all(|| {});
    ui.on_transfer_cancel(|_id| {});
    ui.on_transfer_retry(|_id| {});
    ui.on_transfers_retry_failed(|| {});
    ui.on_transfer_move_to_top(|_id| {});
    ui.on_transfer_pause(|_id| {});
    ui.on_transfer_resume(|_id| {});
}

#[cfg(test)]
#[path = "drawer_view_tests.rs"]
mod tests;
