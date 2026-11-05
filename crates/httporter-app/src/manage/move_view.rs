//! Rename, Move To…, dragging within the cloud pane, the move warning and pre-signed links.
//!
//! Rename and Move To… plan the move first ([`plan_move`]), with the sheet saying it is busy, so a
//! name that is taken shows in the sheet. A planned move that can't copy on the server (Bunny, or
//! an S3 file over 5 GiB) asks first with the warning sheet. The move then runs ([`run_move`])
//! with "Moving… n of m" in the status line, and both folders are listed again.

use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::Duration;

use httporter_core::{ActivityKind, Entry, ProviderKind, RemotePath, SiteRecord};
use httporter_platform::{cache_dir, win};
use httporter_transfer::{MovePhase, MovePlan, estimate_secs, plan_move, run_move};
use slint::{ComponentHandle, Model, Timer, TimerMode};

use super::{Manager, activity_view, cache, copy};
use crate::{AppWindow, MoveData, MoveWarningData, PresignData, RenameData};

/// How often the window's "Copy Presigned Link…" is brought in line with the site shown.
const PRESIGN_POLL: Duration = Duration::from_millis(500);

/// The rates a move that downloads and uploads again is estimated at.
const MOVE_FILES_PER_S: f64 = 4.0;
const MOVE_BYTES_PER_S: f64 = 4.0 * 1024.0 * 1024.0;

/// The expiries of the presign sheet, in its order.
const EXPIRIES: [u64; 4] = [15 * 60, 3600, 24 * 3600, 7 * 24 * 3600];

/// What a sheet works on: the site and the items.
#[derive(Clone)]
struct Job {
    site: SiteRecord,
    /// The folder the items are in.
    dir: RemotePath,
    entries: Vec<Entry>,
}

/// What this view remembers between callbacks.
#[derive(Default)]
pub struct MoveState {
    /// What the open Rename sheet renames.
    rename: Option<Job>,
    /// What the open Move To… sheet moves.
    moving: Option<Job>,
    /// What the open presign sheet makes links for.
    presign: Option<Job>,
    /// Goes up with every sheet opened, so an answer for one that has gone is dropped.
    serial: u64,
}

fn lock(m: &Manager) -> MutexGuard<'_, MoveState> {
    m.moves.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires this view's callbacks of `ui` to `m`.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_rename_start({
        let m = m.clone();
        move || rename_start(&m)
    });
    ui.on_rename_confirm({
        let m = m.clone();
        move |name| rename_confirm(&m, name.to_string())
    });
    ui.on_move_start({
        let m = m.clone();
        move || move_start(&m)
    });
    ui.on_move_confirm({
        let m = m.clone();
        move |to| move_confirm(&m, to.to_string())
    });
    ui.on_drag_within_remote({
        let m = m.clone();
        move |row| {
            if let Ok(row) = usize::try_from(row) {
                drag_within(&m, row);
            }
        }
    });
    // The warning sheet answers a question waiting (a move, or a big preview).
    ui.on_move_warning_confirm(|| {
        cache::answer_ask();
    });
    ui.on_presign_copy({
        let m = m.clone();
        move |expiry| presign_copy(&m, expiry)
    });

    // "Copy Presigned Link…" is offered on S3 sites. The window has no hook for the site shown
    // changing, so this looks a couple of times a second; the timer lives as long as the
    // window's Copy Presigned Link callback.
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, PRESIGN_POLL, {
        let (m, weak) = (Arc::downgrade(m), ui.as_weak());
        move || {
            let (Some(m), Some(ui)) = (m.upgrade(), weak.upgrade()) else {
                return;
            };
            let s3 = open_site(&m).is_some_and(|(site, _)| site.provider == ProviderKind::S3);
            if ui.get_presign_available() != s3 {
                ui.set_presign_available(s3);
            }
        }
    });
    ui.on_copy_presigned({
        let m = m.clone();
        move || {
            let _alive = &timer;
            presign_start(&m);
        }
    });
}

/// The site the cloud pane shows and its folder.
fn open_site(m: &Manager) -> Option<(SiteRecord, RemotePath)> {
    let (id, dir) = m.session.remote_shows()?;
    Some((m.session.site(&id)?, dir))
}

/// The cloud pane's selected items (or the row with the focus when none is selected).
fn selected(m: &Manager, ui: &AppWindow) -> Vec<Entry> {
    let rows = ui.get_remote_rows();
    let mut names: Vec<String> = rows
        .iter()
        .filter(|row| row.selected)
        .map(|row| row.name.to_string())
        .collect();
    if names.is_empty()
        && let Some(row) = usize::try_from(ui.get_remote_selected())
            .ok()
            .and_then(|i| rows.row_data(i))
    {
        names.push(row.name.to_string());
    }
    m.session.remote_entries(&names)
}

/// The job for the selection, unless the site is read-only (said in the status line).
fn job(m: &Manager, ui: &AppWindow) -> Option<Job> {
    let (site, dir) = open_site(m)?;
    if m.read_only(&site.id) {
        m.link.status(copy::move_read_only(&site.name));
        return None;
    }
    let entries = selected(m, ui);
    if entries.is_empty() {
        return None;
    }
    Some(Job { site, dir, entries })
}

// ---- Rename ----

fn rename_start(m: &Arc<Manager>) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let Some(job) = job(m, &ui) else {
        return;
    };
    if job.entries.len() != 1 {
        m.link.status(copy::RENAME_ONE.to_owned());
        return;
    }
    let name = job.entries[0].name().to_owned();
    {
        let mut state = lock(m);
        state.serial += 1;
        state.rename = Some(job);
    }
    ui.set_rename_data(RenameData {
        title: copy::rename_title(&name).into(),
        name: name.into(),
        ..Default::default()
    });
    ui.set_rename_open(true);
}

fn rename_confirm(m: &Arc<Manager>, typed: String) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let Some(job) = lock(m).rename.clone() else {
        return;
    };
    let name = typed.trim().to_owned();
    let old = job.entries[0].name().to_owned();
    let refused = if name.is_empty() {
        Some(copy::NAME_EMPTY.to_owned())
    } else if name.contains('/') || name.contains('\\') {
        Some(copy::NAME_SLASH.to_owned())
    } else if job.dir.join(&name).is_err() {
        Some(copy::NAME_INVALID.to_owned())
    } else {
        None
    };
    if let Some(error) = refused {
        let mut data = ui.get_rename_data();
        data.error = error.into();
        ui.set_rename_data(data);
        return;
    }
    if name == old {
        ui.set_rename_open(false);
        return;
    }
    let mut data = ui.get_rename_data();
    data.busy = true;
    data.error = Default::default();
    ui.set_rename_data(data);
    let serial = lock(m).serial;
    let m2 = m.clone();
    m.link.rt().spawn(async move {
        let planned = plan(&m2, &job, &job.dir, Some(&name)).await;
        let ui = m2.link.ui().clone();
        let m3 = m2.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if lock(&m3).serial != serial || !ui.get_rename_open() {
                return;
            }
            match planned {
                Ok(plan) => {
                    lock(&m3).rename = None;
                    ui.set_rename_open(false);
                    start(&m3, job, plan, None);
                }
                Err(error) => {
                    let mut data = ui.get_rename_data();
                    data.busy = false;
                    data.error = error.into();
                    ui.set_rename_data(data);
                }
            }
        });
    });
}

// ---- Move To… and dragging ----

fn move_start(m: &Arc<Manager>) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let Some(job) = job(m, &ui) else {
        return;
    };
    let title = match job.entries.as_slice() {
        [one] => copy::move_title_one(one.name()),
        many => copy::move_title(many.len()),
    };
    let destination = format!("/{}", job.dir.as_str());
    {
        let mut state = lock(m);
        state.serial += 1;
        state.moving = Some(job);
    }
    ui.set_move_data(MoveData {
        title: title.into(),
        destination: destination.into(),
        note: copy::MOVE_NOTE.into(),
        ..Default::default()
    });
    ui.set_move_open(true);
}

fn move_confirm(m: &Arc<Manager>, typed: String) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let Some(job) = lock(m).moving.clone() else {
        return;
    };
    let Ok(to) = RemotePath::parse(typed.trim().trim_matches('/')) else {
        let mut data = ui.get_move_data();
        data.error = copy::FOLDER_INVALID.into();
        ui.set_move_data(data);
        return;
    };
    if to == job.dir {
        ui.set_move_open(false);
        return;
    }
    let mut data = ui.get_move_data();
    data.busy = true;
    data.error = Default::default();
    ui.set_move_data(data);
    let serial = lock(m).serial;
    let m2 = m.clone();
    m.link.rt().spawn(async move {
        let planned = plan(&m2, &job, &to, None).await;
        let ui = m2.link.ui().clone();
        let m3 = m2.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if lock(&m3).serial != serial || !ui.get_move_open() {
                return;
            }
            match planned {
                Ok(plan) => {
                    lock(&m3).moving = None;
                    ui.set_move_open(false);
                    start(&m3, job, plan, Some(to));
                }
                Err(error) => {
                    let mut data = ui.get_move_data();
                    data.busy = false;
                    data.error = error.into();
                    ui.set_move_data(data);
                }
            }
        });
    });
}

/// The selection was dragged onto folder row `row` of the cloud pane.
fn drag_within(m: &Arc<Manager>, row: usize) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let Some(to) = m.session.remote_folder_at(row) else {
        return;
    };
    let Some(job) = job(m, &ui) else {
        return;
    };
    if job.entries.iter().any(|e| e.path == to) {
        return;
    }
    let m2 = m.clone();
    m.link.rt().spawn(async move {
        match plan(&m2, &job, &to, None).await {
            Ok(plan) => {
                let ui = m2.link.ui().clone();
                let m3 = m2.clone();
                let _ = ui.upgrade_in_event_loop(move |_| start(&m3, job, plan, Some(to)));
            }
            Err(error) => m2.link.status(error),
        }
    });
}

/// Plans moving the job's items into `to` (renamed to `rename_to`), or why not, in words.
async fn plan(
    m: &Manager,
    job: &Job,
    to: &RemotePath,
    rename_to: Option<&str>,
) -> Result<MovePlan, String> {
    let provider = m.session.connect(&job.site).await?;
    plan_move(&*provider, &job.entries, to, rename_to)
        .await
        .map_err(|e| copy::move_refused(&e))
}

/// Runs `plan` (asking first when it downloads and uploads again). `to`: the folder the items go
/// to, listed again after (a rename stays in its folder).
fn start(m: &Arc<Manager>, job: Job, plan: MovePlan, to: Option<RemotePath>) {
    let m2 = m.clone();
    m.link.rt().spawn(async move {
        if !plan.server_side {
            let files = plan.items.len() as u64;
            let minutes = estimate_secs(&plan, MOVE_FILES_PER_S, MOVE_BYTES_PER_S).div_ceil(60);
            let data = MoveWarningData {
                title: copy::move_warning_title(files).into(),
                body: copy::move_warning_body(files, plan.bytes, minutes).into(),
                button: copy::MOVE_BUTTON.into(),
            };
            if !cache::ask(&m2, data).await {
                return;
            }
        }
        run(&m2, job, plan, to).await;
    });
}

/// Runs the move and says how it went.
async fn run(m: &Manager, job: Job, plan: MovePlan, to: Option<RemotePath>) {
    let site = &job.site;
    let Some(target) = m.transfers.batch_target(site).await else {
        m.link.status(copy::move_failed_to_start(&site.name));
        return;
    };
    let temp = cache_dir("move").join(temp_name());
    let link = m.link.clone();
    let on_phase = move |phase: MovePhase| match phase {
        MovePhase::Copying { done, total } => link.status(copy::moving(done, total)),
        MovePhase::Deleting => link.status(copy::MOVE_DELETING.to_owned()),
        MovePhase::Finished => {}
    };
    let ran = run_move(m.transfers.engine(), target, &plan, &temp, &on_phase).await;
    let _ = std::fs::remove_dir_all(&temp);
    let first = job
        .entries
        .first()
        .map(|e| e.path.as_str().to_owned())
        .unwrap_or_default();
    let to_dir = to.clone().unwrap_or_else(|| job.dir.clone());
    let (ok, message) = match ran {
        Ok(outcome) if outcome.failed.is_empty() => (true, copy::moved(job.entries.len(), &to_dir)),
        Ok(outcome) => (
            false,
            copy::move_partly_failed(outcome.failed.len(), &outcome.kept_copies),
        ),
        Err(e) => (false, copy::move_failed(&e.0)),
    };
    activity_view::event(
        ActivityKind::Move,
        &site.name,
        &copy::move_activity_path(&first, job.entries.len(), &to_dir),
        ok,
        message.clone(),
        plan.bytes,
    );
    m.link.status(message);
    m.link.refresh_remote_at(&site.id, &job.dir);
    if let Some(to) = to {
        m.link.refresh_remote_at(&site.id, &to);
    }
}

/// A fresh name for a move's temporary folder.
fn temp_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{}-{nanos}", std::process::id())
}

// ---- Pre-signed links ----

fn presign_start(m: &Arc<Manager>) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let Some((site, dir)) = open_site(m) else {
        return;
    };
    let entries: Vec<Entry> = selected(m, &ui)
        .into_iter()
        .filter(|e| !e.is_folder)
        .collect();
    if entries.is_empty() {
        m.link.status(copy::PRESIGN_FILES_ONLY.to_owned());
        return;
    }
    let default_secs = match m.session.site_extras(&site.id).presign_expiry_secs {
        0 => 3600,
        secs => u64::from(secs),
    };
    // The choice nearest the site's default.
    let expiry = EXPIRIES
        .iter()
        .enumerate()
        .min_by_key(|(_, secs)| secs.abs_diff(default_secs))
        .map_or(1, |(i, _)| i as i32);
    let title = copy::presign_title(entries.len());
    {
        let mut state = lock(m);
        state.serial += 1;
        state.presign = Some(Job { site, dir, entries });
    }
    ui.set_presign_data(PresignData {
        title: title.into(),
        expiry,
        error: Default::default(),
    });
    ui.set_presign_open(true);
}

fn presign_copy(m: &Arc<Manager>, expiry: i32) {
    let Some(job) = lock(m).presign.clone() else {
        return;
    };
    let secs = usize::try_from(expiry)
        .ok()
        .and_then(|i| EXPIRIES.get(i).copied())
        .unwrap_or(3600);
    let serial = lock(m).serial;
    let m2 = m.clone();
    m.link.rt().spawn(async move {
        let made = async {
            let provider = m2.session.connect(&job.site).await?;
            let mut links = Vec::with_capacity(job.entries.len());
            for entry in &job.entries {
                let link = provider
                    .presign(&entry.path, Duration::from_secs(secs))
                    .map_err(|e| crate::copy::describe_transfer_error(&e))?;
                links.push(link);
            }
            let text = links.join("\r\n");
            crate::session::blocking(move || win::set_clipboard_text(&text)).await?;
            Ok::<usize, String>(links.len())
        }
        .await;
        let ui = m2.link.ui().clone();
        let m3 = m2.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if lock(&m3).serial != serial {
                return;
            }
            match made {
                Ok(n) => {
                    lock(&m3).presign = None;
                    ui.set_presign_open(false);
                    m3.link.status(copy::presigned_copied(n, secs));
                }
                Err(error) => {
                    let mut data = ui.get_presign_data();
                    data.error = error.into();
                    ui.set_presign_data(data);
                }
            }
        });
    });
}
