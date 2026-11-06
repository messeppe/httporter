// SPDX-License-Identifier: MIT OR Apache-2.0

//! Editing a site's file with automatic re-upload (§8.5.2), and the sidebar's Editing rows.
//!
//! Opening a site's file (Enter, a double click, Open, Quick Look's Open With…) downloads it into
//! the edit cache (`fetch_to_cache("edit")`), remembering what the site had (the base), and opens
//! it in its program. One folder watcher covers the whole edit cache. A save is taken up a second
//! after the last change seen; a file whose size and SHA-256 are those last sent is left alone.
//! Before uploading, the site's copy is looked at (`stat`): if it changed since the base, the
//! conflict sheet asks whether to overwrite it, save the edit as a copy, or discard the edit.
//! The upload goes through the engine (`submit_tracked` + `wait_batch`, which also waits for
//! Bunny's late-delete checks); what the site then has becomes the new base.
//!
//! A file of this PC simply opens in its program. A read-only site's file opens as a copy that is
//! not watched.
//!
//! A re-upload’s result (uploaded, saved as a copy, failed) is told in a toast (I5).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use httporter_core::{
    ActivityEvent, ActivityKind, Checksum, ChecksumKind, Entry, Job, ProviderError, RemotePath,
    SiteRecord,
};
use httporter_platform::{cache_dir, win};
use httporter_transfer::{Batch, BatchTarget, hash_file};
use slint::{Model, ModelRc, VecModel};

use super::{Manager, Side, activity_view, copy};
use crate::comfort::toast::{self, ToastKind};
use crate::session::blocking;
use crate::{AppWindow, EditConflictData, EditItem};

/// A save is taken up this long after the last change seen.
const DEBOUNCE: Duration = Duration::from_secs(1);

/// What the window's closing waits at most for pending edits to be handed to the engine.
const FLUSH_WAIT: Duration = Duration::from_secs(5);

/// The cache kind edited files are kept under.
const EDIT: &str = "edit";

/// How a file is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// In its default program.
    Default,
    /// With Windows' Open With dialog.
    With,
}

/// The files being edited.
#[derive(Default)]
pub struct Edits {
    next_id: i32,
    files: Vec<EditFile>,
    /// The file whose conflict sheet is up.
    asking: Option<i32>,
}

/// One file being edited.
struct EditFile {
    id: i32,
    site: SiteRecord,
    remote: RemotePath,
    /// The copy being edited; `None` while it downloads.
    local: Option<PathBuf>,
    /// What the site had when last seen.
    base: Option<Base>,
    /// The size and SHA-256 of the content last downloaded or uploaded.
    sent: Option<(u64, [u8; 32])>,
    state: State,
    /// Counts the changes seen: a debounce acts only on the latest.
    changes: u64,
    /// A check or an upload is running.
    busy: bool,
    /// A change came while busy: take it up once done.
    dirty: bool,
}

/// What a site's file was, to tell whether it changed since.
#[derive(Debug, Clone)]
struct Base {
    size: u64,
    checksum: Option<Checksum>,
    modified: Option<DateTime<Utc>>,
}

impl Base {
    fn of(entry: &Entry) -> Self {
        Self {
            size: entry.size,
            checksum: entry.checksum.clone(),
            modified: entry.modified,
        }
    }

    /// Whether the site's `entry` is still this: the same digest when both have one of a kind,
    /// else the same size and modification time.
    fn same_as(&self, entry: &Entry) -> bool {
        match (&self.checksum, &entry.checksum) {
            (Some(a), Some(b)) if a.kind == b.kind && a.kind != ChecksumKind::MultipartEtag => {
                a.hex == b.hex
            }
            _ => {
                self.size == entry.size
                    && (self.modified.is_none()
                        || entry.modified.is_none()
                        || self.modified == entry.modified)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Opening,
    Editing,
    Uploading,
    /// At this local time ("15:12").
    Uploaded(String),
    /// Changed on the site since the base: the conflict sheet asks.
    Conflict,
    Failed,
}

/// What an upload of an edit does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Checks the site's copy against the base first.
    Checked,
    /// Over the site's copy, whatever it is now.
    Overwrite,
    /// To `name (edited).ext` beside it; the copy is what is edited from then on.
    Copy,
}

thread_local! {
    /// The edit cache's watcher, while something is edited. On the UI thread, which starts and
    /// stops it.
    static WATCHER: RefCell<Option<win::DirWatcher>> = const { RefCell::new(None) };
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires this view's callbacks of `ui` to `m`.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_edit_selected({
        let m = m.clone();
        move |index| selected(&m, index)
    });
    ui.on_edit_stop({
        let m = m.clone();
        move |index| stop(&m, index)
    });
    ui.on_edit_conflict_choice({
        let m = m.clone();
        move |choice| chosen(&m, choice)
    });
}

/// See [`Manager::open_file`]: a file of this PC opens in its program; a site's file is edited.
pub(super) fn open_file(m: &Arc<Manager>, side: Side, row: usize) {
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    let rows = match side {
        Side::Local => ui.get_local_rows(),
        Side::Remote => ui.get_remote_rows(),
    };
    let Some(name) = rows
        .row_data(row)
        .filter(|row| !row.folder)
        .map(|row| row.name.to_string())
    else {
        return;
    };
    match side {
        Side::Local => {
            if let Some(dir) = m.session.local_dir() {
                open_local_file(m, dir.join(name), How::Default);
            }
        }
        Side::Remote => {
            let Some(place) = m.session.remote_place() else {
                return;
            };
            if let Some(entry) = m.session.remote_entries(&[name]).pop() {
                start(m, place.site, entry, How::Default);
            }
        }
    }
}

/// Opens `path`, a file of this PC, in its program (or with the Open With dialog).
pub(super) fn open_local_file(m: &Arc<Manager>, path: PathBuf, how: How) {
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        let name = file_name(&path);
        open_path(&m, path, &name, how).await;
    });
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Opens `path` as `how` says, off the UI thread; a failure goes to the status line.
async fn open_path(m: &Manager, path: PathBuf, name: &str, how: How) {
    let opened = blocking(move || match how {
        How::Default => win::open_default(&path),
        How::With => win::open_with(&path),
    })
    .await;
    if let Err(e) = opened {
        m.link.status(copy::ql_open_failed(name, e));
    }
}

/// Edits `entry` of `site`: downloads it into the edit cache and opens it as `how` says; saving
/// it uploads it again. A file edited already opens again as it is.
pub(super) fn start(m: &Arc<Manager>, site: SiteRecord, entry: Entry, how: How) {
    let name = entry.name().to_owned();
    if m.read_only(&site.id) {
        // A copy to look at: nothing to upload, so nothing watched.
        m.link.status(copy::edit_read_only(&site.name));
        let m = m.clone();
        let rt = m.link.rt().clone();
        rt.spawn(async move {
            match m.fetch_to_cache(&site, &entry, "preview").await {
                Ok(path) => open_path(&m, path, &name, how).await,
                Err(e) => m.link.status(copy::edit_open_failed(&name, e)),
            }
        });
        return;
    }
    let existing = lock(&m.edits)
        .files
        .iter()
        .find(|file| file.site.id == site.id && file.remote == entry.path)
        .map(|file| file.local.clone());
    if let Some(local) = existing {
        if let Some(local) = local {
            let m = m.clone();
            let rt = m.link.rt().clone();
            rt.spawn(async move { open_path(&m, local, &name, how).await });
        }
        return;
    }
    let id = {
        let mut edits = lock(&m.edits);
        edits.next_id += 1;
        let id = edits.next_id;
        edits.files.push(EditFile {
            id,
            site: site.clone(),
            remote: entry.path.clone(),
            local: None,
            base: Some(Base::of(&entry)),
            sent: None,
            state: State::Opening,
            changes: 0,
            busy: false,
            dirty: false,
        });
        id
    };
    show_edits(m);
    ensure_watcher(m);
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        match m.fetch_to_cache(&site, &entry, EDIT).await {
            Ok(path) => {
                let hashed = path.clone();
                let sent = blocking(move || hash_file(&hashed))
                    .await
                    .ok()
                    .map(|h| (h.size, h.sha256));
                update(&m, id, |file| {
                    file.local = Some(path.clone());
                    file.sent = sent;
                    file.state = State::Editing;
                });
                show_edits(&m);
                open_path(&m, path, &name, how).await;
            }
            Err(e) => {
                lock(&m.edits).files.retain(|file| file.id != id);
                show_edits(&m);
                m.link.status(copy::edit_open_failed(&name, e));
            }
        }
    });
}

/// Changes the file `id` (if it is still edited).
fn update(m: &Manager, id: i32, change: impl FnOnce(&mut EditFile)) {
    if let Some(file) = lock(&m.edits).files.iter_mut().find(|f| f.id == id) {
        change(file);
    }
}

/// Starts watching the edit cache, unless it is watched already (UI thread).
fn ensure_watcher(m: &Arc<Manager>) {
    WATCHER.with(|slot| {
        if slot.borrow().is_some() {
            return;
        }
        let root = cache_dir(EDIT);
        let _ = std::fs::create_dir_all(&root);
        let weak = Arc::downgrade(m);
        let watched = win::DirWatcher::start(
            &root,
            Box::new(move |path| {
                if let Some(m) = weak.upgrade() {
                    changed(&m, &path);
                }
            }),
        );
        match watched {
            Ok(watcher) => *slot.borrow_mut() = Some(watcher),
            Err(e) => m.link.status(copy::edit_watch_failed(e)),
        }
    });
}

/// Stops watching the edit cache (UI thread).
fn stop_watcher() {
    WATCHER.with(|slot| *slot.borrow_mut() = None);
}

/// Whether two paths name the same file (Windows: any case, either slash).
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    norm(a) == norm(b)
}

/// The watcher saw `path` change (the edit cache itself when it lost count): each file edited
/// there is taken up a second after its last change.
fn changed(m: &Arc<Manager>, path: &Path) {
    let everything = same_path(path, &cache_dir(EDIT));
    let mut due = Vec::new();
    for file in &mut lock(&m.edits).files {
        let Some(local) = &file.local else {
            continue;
        };
        if everything || same_path(local, path) {
            file.changes += 1;
            due.push((file.id, file.changes));
        }
    }
    for (id, changes) in due {
        let m = m.clone();
        let rt = m.link.rt().clone();
        rt.spawn(async move {
            tokio::time::sleep(DEBOUNCE).await;
            take_up(&m, id, Some(changes), Mode::Checked).await;
        });
    }
}

/// Uploads file `id`'s edit as `send` says, if it changed since it was last sent; `changes`, when
/// given, is the change that asked, and a later one takes over. A change while it runs is taken
/// up after it.
async fn take_up(m: &Arc<Manager>, id: i32, changes: Option<u64>, mut send: Mode) {
    let mut changes = changes;
    loop {
        let claimed = {
            let mut edits = lock(&m.edits);
            let Some(file) = edits.files.iter_mut().find(|f| f.id == id) else {
                return;
            };
            let current = changes.is_none_or(|n| n == file.changes);
            let waiting = file.state == State::Conflict && send == Mode::Checked;
            match &file.local {
                Some(local) if current && !waiting => {
                    if file.busy {
                        file.dirty = true;
                        None
                    } else {
                        file.busy = true;
                        file.dirty = false;
                        Some((
                            file.site.clone(),
                            file.remote.clone(),
                            local.clone(),
                            file.sent,
                        ))
                    }
                }
                _ => None,
            }
        };
        let Some((site, remote, local, sent)) = claimed else {
            return;
        };
        send_edit(m, id, &site, &remote, &local, sent, send).await;
        let again = {
            let mut edits = lock(&m.edits);
            match edits.files.iter_mut().find(|f| f.id == id) {
                Some(file) => {
                    file.busy = false;
                    std::mem::take(&mut file.dirty) && file.state != State::Conflict
                }
                None => false,
            }
        };
        show_edits(m);
        if !again {
            return;
        }
        changes = None;
        send = Mode::Checked;
    }
}

/// One upload of file `id`'s edit (see [`take_up`]).
async fn send_edit(
    m: &Arc<Manager>,
    id: i32,
    site: &SiteRecord,
    remote: &RemotePath,
    local: &Path,
    sent: Option<(u64, [u8; 32])>,
    send: Mode,
) {
    let name = remote.name().unwrap_or_default().to_owned();
    let hashed = local.to_path_buf();
    // Unreadable while the editor writes it: the write's own change comes next.
    let Ok(hashes) = blocking(move || hash_file(&hashed)).await else {
        return;
    };
    if send == Mode::Checked && sent == Some((hashes.size, hashes.sha256)) {
        return;
    }
    update(m, id, |file| file.state = State::Uploading);
    show_edits(m);
    let Some(target) = m.transfers.batch_target(site).await else {
        failed(m, id, &name, copy::edit_no_connection(&site.name));
        return;
    };
    let to = match send {
        Mode::Copy => {
            let parent = remote.parent().unwrap_or_else(RemotePath::root);
            match parent.join(&copy::edit_copy_name(&name)) {
                Ok(to) => to,
                Err(e) => {
                    failed(m, id, &name, e.to_string());
                    return;
                }
            }
        }
        Mode::Checked | Mode::Overwrite => remote.clone(),
    };
    if send == Mode::Checked {
        let base = lock(&m.edits)
            .files
            .iter()
            .find(|f| f.id == id)
            .and_then(|f| f.base.clone());
        match target.provider.stat(remote).await {
            Ok(info) if base.as_ref().is_none_or(|base| base.same_as(&info.entry)) => {}
            Ok(_) | Err(ProviderError::NotFound) => {
                conflict(m, id);
                return;
            }
            Err(e) => {
                failed(m, id, &name, crate::copy::describe_transfer_error(&e));
                return;
            }
        }
    }
    match upload(m, &target, local, &to, hashes.size).await {
        Ok(()) => {
            let base = match target.provider.stat(&to).await {
                Ok(info) => Base::of(&info.entry),
                Err(_) => Base {
                    size: hashes.size,
                    checksum: None,
                    modified: None,
                },
            };
            let time = Local::now().format("%H:%M").to_string();
            update(m, id, |file| {
                file.remote = to.clone();
                file.base = Some(base);
                file.sent = Some((hashes.size, hashes.sha256));
                file.state = State::Uploaded(time);
            });
            let to_name = to.name().unwrap_or_default();
            // A toast (I5, §8.5.2).
            toast::post(
                m.link.ui(),
                ToastKind::Done,
                match send {
                    Mode::Copy => copy::edit_saved_copy(to_name, &site.name),
                    Mode::Checked | Mode::Overwrite => copy::edit_uploaded(to_name, &site.name),
                },
            );
            log(
                site,
                &to,
                true,
                copy::EDIT_EVENT_DETAIL.to_owned(),
                hashes.size,
            );
            if let Some(parent) = to.parent() {
                m.link.refresh_remote_at(&site.id, &parent);
            }
        }
        Err(Upload::Cancelled) => {
            update(m, id, |file| file.state = State::Editing);
            m.link.status(copy::edit_cancelled(&name));
        }
        Err(Upload::Failed(reason)) => {
            log(site, &to, false, reason.clone(), 0);
            failed(m, id, &name, reason);
        }
    }
}

/// How an upload of an edit ended, when it did not upload.
enum Upload {
    Cancelled,
    Failed(String),
}

/// Uploads `local` to `to` on the target's site as one tracked batch, and waits for it (Bunny's
/// late-delete checks included).
async fn upload(
    m: &Manager,
    target: &BatchTarget,
    local: &Path,
    to: &RemotePath,
    size: u64,
) -> Result<(), Upload> {
    let batch = edit_batch(target, local, to, size);
    let engine = m.transfers.engine().clone();
    let batch_id = blocking(move || engine.submit_tracked(batch))
        .await
        .map_err(|e| Upload::Failed(crate::copy::transfer_not_started(e.0)))?;
    let outcome = m.transfers.engine().wait_batch(&batch_id).await;
    if outcome.done.contains(to) {
        Ok(())
    } else if let Some((_, reason)) = outcome.failed.into_iter().next() {
        Err(Upload::Failed(reason))
    } else {
        Err(Upload::Cancelled)
    }
}

fn edit_batch(target: &BatchTarget, local: &Path, to: &RemotePath, size: u64) -> Batch {
    Batch {
        site: target.site.clone(),
        site_name: target.site_name.clone(),
        host: target.host.clone(),
        provider: target.provider.clone(),
        jobs: vec![Job::Upload {
            local: local.to_path_buf(),
            remote: to.clone(),
            size,
        }],
        checksums: HashMap::new(),
    }
}

/// An Edit event in the activity log.
fn log(site: &SiteRecord, path: &RemotePath, ok: bool, detail: String, bytes: u64) {
    activity_view::log(ActivityEvent {
        at: Utc::now(),
        kind: ActivityKind::Edit,
        site: site.name.clone(),
        path: path.as_str().to_owned(),
        ok,
        detail,
        bytes,
    });
}

/// File `id`'s upload failed for `reason`: a toast says so (I5, §8.5.2).
fn failed(m: &Manager, id: i32, name: &str, reason: String) {
    update(m, id, |file| file.state = State::Failed);
    toast::post(
        m.link.ui(),
        ToastKind::Warning,
        copy::edit_upload_failed(name, reason),
    );
}

/// File `id` changed on its site since it was opened: the sheet asks what to do.
fn conflict(m: &Arc<Manager>, id: i32) {
    update(m, id, |file| file.state = State::Conflict);
    show_edits(m);
    ask_next(m);
}

/// Puts up the conflict sheet for the first file waiting on one, unless a sheet is up.
fn ask_next(m: &Arc<Manager>) {
    let m = m.clone();
    let weak = m.link.ui().clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        if ui.get_edit_conflict_open() {
            return;
        }
        let mut edits = lock(&m.edits);
        let Some(file) = edits.files.iter().find(|f| f.state == State::Conflict) else {
            return;
        };
        let name = file.remote.name().unwrap_or_default().to_owned();
        let body = copy::edit_conflict_body(
            &copy::ql_remote_label(&file.site),
            &copy::edit_copy_name(&name),
        );
        edits.asking = Some(file.id);
        drop(edits);
        ui.set_edit_conflict(EditConflictData {
            name: name.into(),
            body: body.into(),
        });
        ui.set_edit_conflict_open(true);
    });
}

/// The conflict sheet's answer: 0 overwrite, 1 save as a copy, 2 discard the edit.
fn chosen(m: &Arc<Manager>, choice: i32) {
    let Some(id) = lock(&m.edits).asking.take() else {
        return;
    };
    let send = match choice {
        0 => Mode::Overwrite,
        1 => Mode::Copy,
        _ => {
            discard(m, id);
            return;
        }
    };
    update(m, id, |file| file.state = State::Editing);
    show_edits(m);
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        take_up(&m, id, None, send).await;
        ask_next(&m);
    });
}

/// Discard My Edit: the site's copy replaces the edited one.
fn discard(m: &Arc<Manager>, id: i32) {
    let file = lock(&m.edits)
        .files
        .iter()
        .find(|f| f.id == id)
        .map(|f| (f.site.clone(), f.remote.clone()));
    let Some((site, remote)) = file else {
        return;
    };
    update(m, id, |file| file.state = State::Opening);
    show_edits(m);
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        let name = remote.name().unwrap_or_default().to_owned();
        let Some(target) = m.transfers.batch_target(&site).await else {
            update(&m, id, |file| file.state = State::Conflict);
            show_edits(&m);
            m.link.status(copy::edit_no_connection(&site.name));
            return;
        };
        let entry = match target.provider.stat(&remote).await.map(|info| info.entry) {
            Ok(entry) => entry,
            Err(ProviderError::NotFound) => {
                // Gone from the site: there is nothing left to edit.
                lock(&m.edits).files.retain(|f| f.id != id);
                show_edits(&m);
                m.link.status(copy::edit_discarded(&name));
                ask_next(&m);
                return;
            }
            Err(e) => {
                update(&m, id, |file| file.state = State::Conflict);
                show_edits(&m);
                m.link.status(copy::edit_open_failed(
                    &name,
                    crate::copy::describe_transfer_error(&e),
                ));
                return;
            }
        };
        match m.fetch_to_cache(&site, &entry, EDIT).await {
            Ok(path) => {
                let hashed = path.clone();
                let sent = blocking(move || hash_file(&hashed))
                    .await
                    .ok()
                    .map(|h| (h.size, h.sha256));
                update(&m, id, |file| {
                    file.local = Some(path);
                    file.base = Some(Base::of(&entry));
                    file.sent = sent;
                    file.state = State::Editing;
                });
                m.link.status(copy::edit_discarded(&name));
            }
            Err(e) => {
                update(&m, id, |file| file.state = State::Conflict);
                m.link.status(copy::edit_open_failed(&name, e));
            }
        }
        show_edits(&m);
        ask_next(&m);
    });
}

/// A click on Editing row `index`: a conflict asks again, a failure is tried again, anything
/// else opens the file in its program.
fn selected(m: &Arc<Manager>, index: i32) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    let file = lock(&m.edits)
        .files
        .get(index)
        .map(|f| (f.id, f.state.clone(), f.local.clone(), f.remote.clone()));
    let Some((id, state, local, remote)) = file else {
        return;
    };
    match (state, local) {
        (State::Conflict, _) => ask_next(m),
        (State::Failed, _) => {
            let m = m.clone();
            let rt = m.link.rt().clone();
            rt.spawn(async move { take_up(&m, id, None, Mode::Checked).await });
        }
        (State::Opening, _) | (_, None) => {}
        (_, Some(local)) => {
            let name = remote.name().unwrap_or_default().to_owned();
            let m = m.clone();
            let rt = m.link.rt().clone();
            rt.spawn(async move { open_path(&m, local, &name, How::Default).await });
        }
    }
}

/// Stop on Editing row `index`: the file is no longer watched (an upload under way finishes).
fn stop(m: &Arc<Manager>, index: i32) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    let none_left = {
        let mut edits = lock(&m.edits);
        if index >= edits.files.len() {
            return;
        }
        let file = edits.files.remove(index);
        if edits.asking == Some(file.id) {
            edits.asking = None;
        }
        edits.files.is_empty()
    };
    if none_left {
        stop_watcher();
    }
    show_edits(m);
}

/// Puts the Editing rows in the window (from any thread).
fn show_edits(m: &Manager) {
    let rows: Vec<(i32, String, String, bool, bool)> = lock(&m.edits)
        .files
        .iter()
        .map(|file| {
            let state = match &file.state {
                State::Opening => copy::EDIT_OPENING.to_owned(),
                State::Editing => copy::EDIT_EDITING.to_owned(),
                State::Uploading => copy::EDIT_UPLOADING.to_owned(),
                State::Uploaded(time) => copy::edit_uploaded_at(time),
                State::Conflict => copy::edit_changed_on(&copy::ql_remote_label(&file.site)),
                State::Failed => copy::EDIT_FAILED.to_owned(),
            };
            (
                file.id,
                file.remote.name().unwrap_or_default().to_owned(),
                state,
                matches!(file.state, State::Opening | State::Uploading),
                matches!(file.state, State::Failed | State::Conflict),
            )
        })
        .collect();
    let _ = m.link.ui().upgrade_in_event_loop(move |ui| {
        let rows: Vec<EditItem> = rows
            .into_iter()
            .map(|(id, name, state, busy, failed)| EditItem {
                id,
                name: name.into(),
                state: state.into(),
                busy,
                failed,
            })
            .collect();
        ui.set_edits(ModelRc::new(VecModel::from(rows)));
    });
}

/// See [`Manager::flush_edits`]: the window is closing. Watching stops; each edit saved but not
/// uploaded yet, whose site's copy is still the base, is handed to the engine (journalled, so it
/// finishes now or resumes at the next start). A site that can't be reached in time keeps its
/// edit in the cache.
pub(super) fn flush_edits(m: &Manager) {
    /// An edit that may have a save not uploaded yet.
    struct Pending {
        site: SiteRecord,
        remote: RemotePath,
        local: PathBuf,
        sent: Option<(u64, [u8; 32])>,
        base: Option<Base>,
    }

    stop_watcher();
    let pending: Vec<Pending> = lock(&m.edits)
        .files
        .iter()
        .filter(|file| !file.busy && file.state != State::Conflict)
        .filter_map(|file| {
            Some(Pending {
                site: file.site.clone(),
                remote: file.remote.clone(),
                local: file.local.clone()?,
                sent: file.sent,
                base: file.base.clone(),
            })
        })
        .collect();
    if pending.is_empty() {
        return;
    }
    let work = async {
        for Pending {
            site,
            remote,
            local,
            sent,
            base,
        } in pending
        {
            let Ok(hashes) = hash_file(&local) else {
                continue;
            };
            if sent == Some((hashes.size, hashes.sha256)) {
                continue;
            }
            let Some(target) = m.transfers.batch_target(&site).await else {
                continue;
            };
            let unchanged = match target.provider.stat(&remote).await {
                Ok(info) => base.as_ref().is_none_or(|base| base.same_as(&info.entry)),
                Err(_) => false,
            };
            if unchanged {
                let batch = edit_batch(&target, &local, &remote, hashes.size);
                let _ = m.transfers.engine().submit(batch);
            }
        }
    };
    let _ = m
        .link
        .rt()
        .block_on(async { tokio::time::timeout(FLUSH_WAIT, work).await });
}
