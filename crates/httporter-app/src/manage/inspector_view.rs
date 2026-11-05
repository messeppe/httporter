//! The inspector beside the single pane (T5): preview, metadata, links and actions; the view
//! switch (two panes, or a single pane with the inspector; list or thumbnails) and Purge from CDN.
//!
//! [`Manager::shown_changed`] runs whenever the panes' rows, their selection or the open site
//! change (binding.rs, `refresh_toolbar`). It sets `can-purge`, keeps the single pane on the side
//! that has something to show (the cloud pane while a site is open, else This PC), and fills the
//! inspector for that pane's selection. What takes a while (the site's `stat`, the preview, the
//! local checksum, the public link) comes on the runtime, each part filling its fields as it
//! arrives; a newer selection drops the parts still on their way for an older one.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use httporter_core::{
    ChecksumKind, Entry, META_SHA256, ProviderKind, SiteRecord, ViewMode, content_type_for,
};
use slint::{ComponentHandle, Model, Rgba8Pixel, SharedPixelBuffer};

use super::{Manager, Side, copy, links, thumbs};
use crate::session::blocking;
use crate::{AppWindow, FileKind, FileRow, InspectorData};

/// Previews are made of images up to this size.
const PREVIEW_MAX_BYTES: u64 = 20 << 20;
/// A preview fits in this many pixels each way (the 300 px panel, at twice its size).
const PREVIEW_SIZE: u32 = 640;
/// Local files up to this size get their SHA-256 shown (larger ones would keep the disk busy
/// for a selection).
const LOCAL_HASH_MAX_BYTES: u64 = 1 << 30;

/// A part of a subject, to run on the runtime once the subject's first data is in place.
type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the inspector shows, for which selection, and the parts still on their way.
#[derive(Default)]
pub struct InspectorState {
    /// Goes up with every new subject: parts still on their way for an older one are dropped.
    generation: u64,
    subject: Option<Subject>,
    data: InspectorData,
    preview: Option<SharedPixelBuffer<Rgba8Pixel>>,
    pending: u32,
    /// The grid's thumbnails ([`thumbs`]).
    pub(super) thumbs: thumbs::Thumbs,
}

/// What the inspector is about: one row of a pane's folder (by its name, size and date, so that
/// a changed file is a new subject), or several.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Subject {
    One {
        side: Side,
        place: String,
        name: String,
        stamp: String,
    },
    Several {
        side: Side,
        place: String,
        names: Vec<String>,
    },
}

impl Manager {
    /// The panes' rows, their selection or the open site changed (binding.rs calls it from
    /// `refresh_toolbar`, on the UI thread, with the panes' bookkeeping locked: nothing here goes
    /// back to it).
    pub fn shown_changed(self: &Arc<Self>, ui: &AppWindow) {
        refresh(self, ui);
    }
}

/// The site open in the cloud pane (the one chosen in the sidebar).
pub(super) fn open_site(m: &Manager, ui: &AppWindow) -> Option<SiteRecord> {
    let index = usize::try_from(ui.get_selected_site()).ok()?;
    let id = m.session.site_id_at(index)?;
    m.session.site(&id)
}

/// The names of the rows selected in `side`'s pane, as the window shows them.
pub(super) fn selected_names(ui: &AppWindow, side: Side) -> Vec<String> {
    selected_rows(ui, side)
        .into_iter()
        .map(|row| row.name.to_string())
        .collect()
}

fn selected_rows(ui: &AppWindow, side: Side) -> Vec<FileRow> {
    let rows = match side {
        Side::Local => ui.get_local_rows(),
        Side::Remote => ui.get_remote_rows(),
    };
    rows.iter().filter(|row| row.selected).collect()
}

/// The folder `side` shows (empty for the list of drives); `None` while a listing for it is on
/// its way, or with no site open.
fn place_of(m: &Manager, side: Side) -> Option<String> {
    match side {
        Side::Local => m
            .session
            .local_shows()
            .map(|dir| dir.map(|dir| dir.display().to_string()).unwrap_or_default()),
        Side::Remote => m
            .session
            .remote_shows()
            .map(|(site, dir)| format!("{site}/{}", dir.as_str())),
    }
}

fn single_side(ui: &AppWindow) -> Side {
    if ui.get_single_side() == 1 {
        Side::Remote
    } else {
        Side::Local
    }
}

fn refresh(m: &Arc<Manager>, ui: &AppWindow) {
    let site = open_site(m, ui);
    let site_ready = site.is_some() && ui.get_connection_ok() && !ui.get_remote_loading();
    let bunny = site
        .as_ref()
        .is_some_and(|site| site.provider == ProviderKind::Bunny);
    let inspector = ui.get_view_mode() == 1;
    let cloud_shown = !inspector || ui.get_single_side() == 1;
    ui.set_can_purge(bunny && site_ready && cloud_shown && ui.get_remote_selected_count() > 0);
    if !inspector {
        forget(m, ui);
        return;
    }
    let side = i32::from(site.is_some());
    if ui.get_single_side() != side {
        // Not from here: the pane in use changes with it, which binding.rs answers with
        // `refresh_toolbar`, and that is what called this, its lock held.
        let (ui, m) = (ui.as_weak(), m.clone());
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = ui.upgrade() {
                show_side(&ui, &m, side);
            }
        });
        return;
    }
    update(m, ui, site);
}

/// Shows `side` (0 This PC, 1 the cloud pane) in the single pane, as the pane in use.
fn show_side(ui: &AppWindow, m: &Arc<Manager>, side: i32) {
    ui.set_single_side(side);
    ui.set_local_pane_active(side == 0);
    refresh(m, ui);
    thumbs::follow(ui, m);
}

/// Back to two panes: the inspector lets go of what it showed.
fn forget(m: &Manager, ui: &AppWindow) {
    let mut state = lock(&m.inspector);
    if state.subject.is_some() || state.preview.is_some() {
        state.generation += 1;
        state.subject = None;
        state.preview = None;
        state.pending = 0;
        state.data = InspectorData::default();
        drop(state);
        ui.set_inspector(InspectorData::default());
        ui.set_inspector_image(slint::Image::default());
    }
}

/// Puts the inspector's data and preview in the window.
fn apply(m: &Manager, ui: &AppWindow) {
    if ui.get_view_mode() != 1 {
        return;
    }
    let (data, preview) = {
        let state = lock(&m.inspector);
        (state.data.clone(), state.preview.clone())
    };
    ui.set_inspector(data);
    ui.set_inspector_image(preview.map(slint::Image::from_rgba8).unwrap_or_default());
}

/// [`apply`], from any thread.
fn post(m: &Arc<Manager>) {
    let manager = m.clone();
    let _ = m
        .link
        .ui()
        .upgrade_in_event_loop(move |ui| apply(&manager, &ui));
}

/// One part of a subject on its way. While any is, the inspector is busy; it counts as done when
/// it is dropped, whether it filled its fields, failed or was cut short.
struct Part {
    m: Arc<Manager>,
    generation: u64,
}

impl Part {
    fn start(m: &Arc<Manager>, generation: u64) -> Self {
        let mut state = lock(&m.inspector);
        if state.generation == generation {
            state.pending += 1;
            state.data.busy = true;
        }
        Self {
            m: m.clone(),
            generation,
        }
    }

    /// Changes the inspector's state with `change` while it shows the same subject, and shows it.
    fn fill(&self, change: impl FnOnce(&mut InspectorState)) {
        {
            let mut state = lock(&self.m.inspector);
            if state.generation != self.generation {
                return;
            }
            change(&mut state);
        }
        post(&self.m);
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.m.inspector);
            if state.generation != self.generation {
                return;
            }
            state.pending = state.pending.saturating_sub(1);
            state.data.busy = state.pending > 0;
        }
        post(&self.m);
    }
}

/// Fills the inspector for the single pane's selection.
fn update(m: &Arc<Manager>, ui: &AppWindow, site: Option<SiteRecord>) {
    let side = single_side(ui);
    let Some(place) = place_of(m, side) else {
        // A listing is on its way: what is shown stays until it is in.
        return;
    };
    let picked = selected_rows(ui, side);
    let subject = match picked.as_slice() {
        [] => None,
        [row] => Some(Subject::One {
            side,
            place,
            name: row.name.to_string(),
            stamp: format!("{}|{}", row.size, row.modified),
        }),
        rows => Some(Subject::Several {
            side,
            place,
            names: rows.iter().map(|row| row.name.to_string()).collect(),
        }),
    };
    let site = site.filter(|_| side == Side::Remote);
    let bunny = site
        .as_ref()
        .is_some_and(|site| site.provider == ProviderKind::Bunny);
    let can_write = site.is_some() && ui.get_can_write();
    let generation = {
        let mut state = lock(&m.inspector);
        if state.subject == subject {
            // The same subject: only what follows the window changes.
            let shown = state.data.shown;
            state.data.can_write = shown && can_write;
            drop(state);
            apply(m, ui);
            return;
        }
        state.generation += 1;
        state.subject = subject.clone();
        state.preview = None;
        state.pending = 0;
        state.data = InspectorData::default();
        state.generation
    };
    let mut data = InspectorData {
        can_write,
        can_purge: bunny,
        purge_note: if site.is_some() && !bunny {
            copy::PURGE_NEEDS_BUNNY.into()
        } else {
            Default::default()
        },
        storage_note: site
            .as_ref()
            .map(|site| links::storage_note(site).into())
            .unwrap_or_default(),
        ..Default::default()
    };
    let mut jobs = Vec::new();
    match subject {
        None => {}
        Some(Subject::Several { names, .. }) => {
            data.shown = true;
            several(m, generation, side, &names, &mut data, &mut jobs);
        }
        Some(Subject::One { name, .. }) => {
            data.shown = true;
            let image = picked
                .first()
                .is_some_and(|row| !row.folder && row.kind == FileKind::Image);
            match (&site, side) {
                (Some(site), Side::Remote) => {
                    match m.session.remote_entries(std::slice::from_ref(&name)).pop() {
                        Some(entry) => {
                            one_remote(m, generation, site, entry, image, &mut data, &mut jobs)
                        }
                        None => data.shown = false,
                    }
                }
                _ => one_local(m, generation, &name, image, &mut data, &mut jobs),
            }
        }
    }
    // In place before any part starts, so that none fills a field this would then overwrite.
    {
        let mut state = lock(&m.inspector);
        data.busy = state.pending > 0;
        state.data = data;
    }
    for job in jobs {
        m.link.rt().spawn(job);
    }
    apply(m, ui);
}

/// Several rows: "3 items · 1.2 MB" (the files' sizes, read from the disk for This PC's).
fn several(
    m: &Arc<Manager>,
    generation: u64,
    side: Side,
    names: &[String],
    data: &mut InspectorData,
    jobs: &mut Vec<Job>,
) {
    match side {
        Side::Remote => {
            let bytes = m
                .session
                .remote_entries(names)
                .iter()
                .filter(|entry| !entry.is_folder)
                .map(|entry| entry.size)
                .sum();
            data.multi = copy::several(names.len(), Some(bytes)).into();
        }
        Side::Local => {
            data.multi = copy::several(names.len(), None).into();
            let paths = m.session.local_paths(names);
            let count = names.len();
            let part = Part::start(m, generation);
            jobs.push(Box::pin(async move {
                let bytes = blocking(move || {
                    paths
                        .iter()
                        .filter_map(|path| std::fs::metadata(path).ok())
                        .filter(|meta| meta.is_file())
                        .map(|meta| meta.len())
                        .sum::<u64>()
                })
                .await;
                part.fill(|state| state.data.multi = copy::several(count, Some(bytes)).into());
            }));
        }
    }
}

/// The checksum row of a site's file: its label and its digest in capitals.
fn checksum_row(kind: ChecksumKind, hex: &str) -> (&'static str, String) {
    let label = match kind {
        ChecksumKind::Sha256 => copy::SHA256_LABEL,
        ChecksumKind::Md5 => copy::MD5_LABEL,
        ChecksumKind::MultipartEtag => copy::ETAG_LABEL,
    };
    (label, hex.to_ascii_uppercase())
}

/// One row of the cloud pane: what its listing says at once, then the public link, the site's
/// `stat` (created, content type, an S3 file's SHA-256) with "Matches local", and the preview.
fn one_remote(
    m: &Arc<Manager>,
    generation: u64,
    site: &SiteRecord,
    entry: Entry,
    image: bool,
    data: &mut InspectorData,
    jobs: &mut Vec<Job>,
) {
    let folder = entry.is_folder;
    let name = entry.name().to_owned();
    let kind = if folder {
        copy::FOLDER_KIND.to_owned()
    } else {
        copy::kind_of(&name)
    };
    data.name = name.as_str().into();
    data.kind_text = if folder {
        kind.as_str().into()
    } else {
        copy::kind_line(&kind, None, Some(entry.size)).into()
    };
    data.is_image = image;
    data.modified = entry
        .modified
        .map(copy::long_date)
        .unwrap_or_default()
        .into();
    data.storage_url = links::storage_url(site, &entry.path, folder).into();
    if !folder {
        data.created = copy::NOT_KNOWN.into();
        data.content_type = entry
            .content_type
            .clone()
            .unwrap_or_else(|| content_type_for(&name))
            .into();
        if site.provider == ProviderKind::Bunny {
            data.regions = copy::regions(&site.region, &entry.replicated_regions).into();
        }
        if let Some(checksum) = &entry.checksum {
            let (label, hex) = checksum_row(checksum.kind, &checksum.hex);
            data.checksum_label = label.into();
            data.checksum = hex.into();
        }
    }

    // The public link.
    {
        let part = Part::start(m, generation);
        let (this, site, path) = (m.clone(), site.clone(), entry.path.clone());
        jobs.push(Box::pin(async move {
            let public = links::public_of(&this, &site).await;
            let url = public
                .urls(&path, folder)
                .into_iter()
                .next()
                .unwrap_or_default();
            part.fill(|state| {
                state.data.public_url = url.into();
                state.data.public_note = public.note.into();
            });
        }));
    }
    if folder {
        return;
    }

    // What the listing leaves out, then whether the file in This PC's folder is the same.
    {
        let part = Part::start(m, generation);
        let (this, site, entry) = (m.clone(), site.clone(), entry.clone());
        jobs.push(Box::pin(async move {
            let mut sha256 = entry
                .checksum
                .as_ref()
                .filter(|c| c.kind == ChecksumKind::Sha256)
                .map(|c| c.hex.clone());
            let md5 = entry
                .checksum
                .as_ref()
                .filter(|c| c.kind == ChecksumKind::Md5)
                .map(|c| c.hex.clone());
            let info = match this.session.connect(&site).await {
                Ok(provider) => provider.stat(&entry.path).await.ok(),
                Err(_) => None,
            };
            if let Some(info) = &info {
                if sha256.is_none() {
                    sha256 = info
                        .metadata
                        .iter()
                        .find(|(key, _)| key == META_SHA256)
                        .map(|(_, value)| value.to_ascii_lowercase());
                }
                let created = info.created.map(copy::long_date);
                let content_type = info.content_type.clone();
                let meta_sha = sha256.clone().filter(
                    |_| !matches!(&entry.checksum, Some(c) if c.kind == ChecksumKind::Sha256),
                );
                part.fill(|state| {
                    state.data.created =
                        created.unwrap_or_else(|| copy::NOT_KNOWN.to_owned()).into();
                    if let Some(content_type) = content_type {
                        state.data.content_type = content_type.into();
                    }
                    if let Some(hex) = meta_sha {
                        let (label, hex) = checksum_row(ChecksumKind::Sha256, &hex);
                        state.data.checksum_label = label.into();
                        state.data.checksum = hex.into();
                    }
                });
            }
            let matches = matches_local(&this, &entry, sha256, md5).await;
            part.fill(|state| state.data.matches = matches.into());
        }));
    }

    // The preview, and the image's size in the kind line.
    if image && entry.size <= PREVIEW_MAX_BYTES {
        let part = Part::start(m, generation);
        let (this, site) = (m.clone(), site.clone());
        jobs.push(Box::pin(async move {
            let Ok(file) = this.fetch_to_cache(&site, &entry, "preview").await else {
                return;
            };
            let decoded = blocking(move || thumbs::decode_scaled(&file, PREVIEW_SIZE)).await;
            if let Some((pixels, size)) = decoded {
                let line = copy::kind_line(&kind, Some(size), Some(entry.size));
                part.fill(|state| {
                    state.preview = Some(pixels);
                    state.data.kind_text = line.into();
                });
            }
        }));
    }
}

/// "Matches local" for a site's file: the file at the same path under This PC's folder (relative
/// to the folder the cloud pane shows), compared by size, then by SHA-256 (or MD5) through the
/// Publisher's digest cache.
async fn matches_local(
    m: &Arc<Manager>,
    entry: &Entry,
    sha256: Option<String>,
    md5: Option<String>,
) -> String {
    let Some(local_dir) = m.session.local_dir() else {
        return copy::NO_LOCAL_COPY.to_owned();
    };
    let shows = m.session.remote_shows().map(|(_, dir)| dir);
    let relative: Vec<String> = match shows.filter(|dir| entry.path.is_within(dir)) {
        Some(dir) => entry
            .path
            .segments()
            .skip(dir.segments().count())
            .map(str::to_owned)
            .collect(),
        None => vec![entry.name().to_owned()],
    };
    let local: PathBuf = relative
        .iter()
        .fold(local_dir, |path, part| path.join(part));
    let (publisher, size) = (m.publisher.clone(), entry.size);
    blocking(move || {
        let Ok(meta) = std::fs::metadata(&local) else {
            return copy::NO_LOCAL_COPY.to_owned();
        };
        if !meta.is_file() {
            return copy::NO_LOCAL_COPY.to_owned();
        }
        if meta.len() != size {
            return copy::MATCHES_NO.to_owned();
        }
        if sha256.is_none() && md5.is_none() {
            return copy::MATCHES_UNKNOWN.to_owned();
        }
        let Ok(hashes) = publisher.local_hashes(&local) else {
            return copy::MATCHES_UNREADABLE.to_owned();
        };
        let same = match (&sha256, &md5) {
            (Some(sha256), _) => hex(&hashes.sha256).eq_ignore_ascii_case(sha256),
            (None, Some(md5)) => hex(&hashes.md5).eq_ignore_ascii_case(md5),
            (None, None) => false,
        };
        if same {
            copy::MATCHES_YES.to_owned()
        } else {
            copy::MATCHES_NO.to_owned()
        }
    })
    .await
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One row of This PC: what the disk says (size, dates), the file's SHA-256 through the
/// Publisher's digest cache, and the preview.
fn one_local(
    m: &Arc<Manager>,
    generation: u64,
    name: &str,
    image: bool,
    data: &mut InspectorData,
    jobs: &mut Vec<Job>,
) {
    data.name = name.into();
    data.is_image = image;
    let Some(path) = m.session.local_paths(&[name.to_owned()]).pop() else {
        return;
    };
    let part = Part::start(m, generation);
    let (this, name) = (m.clone(), name.to_owned());
    jobs.push(Box::pin(async move {
        let read = {
            let path = path.clone();
            blocking(move || std::fs::metadata(path)).await
        };
        let Ok(meta) = read else {
            return;
        };
        let folder = meta.is_dir();
        let kind = if folder {
            copy::FOLDER_KIND.to_owned()
        } else {
            copy::kind_of(&name)
        };
        let size = meta.len();
        let date = |t: std::io::Result<std::time::SystemTime>| {
            t.ok()
                .map(|t| copy::long_date(DateTime::<Utc>::from(t)))
                .unwrap_or_default()
        };
        let (modified, created) = (date(meta.modified()), date(meta.created()));
        {
            let kind = kind.clone();
            part.fill(|state| {
                state.data.kind_text = if folder {
                    kind.into()
                } else {
                    copy::kind_line(&kind, None, Some(size)).into()
                };
                state.data.modified = modified.into();
                state.data.created = created.into();
                if !folder {
                    state.data.content_type = content_type_for(&name).into();
                }
            });
        }
        if folder {
            return;
        }
        if image && size <= PREVIEW_MAX_BYTES {
            let file = path.clone();
            let decoded = blocking(move || thumbs::decode_scaled(&file, PREVIEW_SIZE)).await;
            if let Some((pixels, dimensions)) = decoded {
                let line = copy::kind_line(&kind, Some(dimensions), Some(size));
                part.fill(|state| {
                    state.preview = Some(pixels);
                    state.data.kind_text = line.into();
                });
            }
        }
        if size <= LOCAL_HASH_MAX_BYTES {
            let publisher = this.publisher.clone();
            if let Ok(hashes) = blocking(move || publisher.local_hashes(&path)).await {
                let (label, digest) = checksum_row(ChecksumKind::Sha256, &hex(&hashes.sha256));
                part.fill(|state| {
                    state.data.checksum_label = label.into();
                    state.data.checksum = digest.into();
                });
            }
        }
    }));
}

/// The toolbar's View group: 0 two panes, 1 a single pane with the inspector. Compare needs two
/// panes, so it ends with them.
fn set_view(ui: &AppWindow, m: &Arc<Manager>, mode: i32) {
    let mode = mode.clamp(0, 1);
    if mode == ui.get_view_mode() {
        return;
    }
    if mode == 1 && ui.get_comparing() {
        ui.invoke_toggle_compare();
    }
    ui.set_view_mode(mode);
    if mode == 1 {
        show_side(ui, m, i32::from(open_site(m, ui).is_some()));
    } else {
        refresh(m, ui);
        thumbs::follow(ui, m);
    }
    save_view(m, mode == 1, ui.get_grid());
}

/// The toolbar's Layout group in the inspector view: the rows as a list or as thumbnails.
fn set_grid(ui: &AppWindow, m: &Arc<Manager>, grid: bool) {
    if grid == ui.get_grid() {
        return;
    }
    ui.set_grid(grid);
    thumbs::follow(ui, m);
    save_view(m, ui.get_view_mode() == 1, grid);
}

/// Keeps the view in the UI state, for the next start (on a blocking thread: it is written to the
/// site database).
fn save_view(m: &Manager, inspector: bool, grid: bool) {
    let session = m.session.clone();
    m.link.rt().spawn_blocking(move || {
        let mut state = session.ui_state();
        state.view = if inspector {
            ViewMode::Inspector
        } else {
            ViewMode::TwoPanes
        };
        state.grid = grid;
        session.save_ui_state(&state);
    });
}

/// Purge from CDN (the toolbar, the cloud menu, the inspector): the cloud selection's files and
/// folders, from the open site's pull zones.
fn purge(m: &Arc<Manager>, ui: &AppWindow) {
    let Some(site) = open_site(m, ui) else {
        return;
    };
    let entries = m.session.remote_entries(&selected_names(ui, Side::Remote));
    let (folders, files): (Vec<Entry>, Vec<Entry>) =
        entries.into_iter().partition(|entry| entry.is_folder);
    m.publisher.purge_now(
        site,
        files.into_iter().map(|entry| entry.path).collect(),
        folders.into_iter().map(|entry| entry.path).collect(),
    );
}

/// Wires the view switch, the inspector's buttons and Purge from CDN of `ui` to `m`.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    ui.on_set_view({
        let (ui, m) = (ui.as_weak(), m.clone());
        move |mode| {
            if let Some(ui) = ui.upgrade() {
                set_view(&ui, &m, mode);
            }
        }
    });
    ui.on_set_grid({
        let (ui, m) = (ui.as_weak(), m.clone());
        move |grid| {
            if let Some(ui) = ui.upgrade() {
                set_grid(&ui, &m, grid);
            }
        }
    });
    ui.on_inspector_quick_look({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade() {
                // The window's `quick-look`, which `quicklook_view` handles, opens it.
                ui.invoke_quick_look(ui.get_single_side());
            }
        }
    });
    ui.on_inspector_download({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade()
                && ui.get_single_side() == 1
            {
                ui.invoke_download();
            }
        }
    });
    ui.on_inspector_delete({
        let ui = ui.as_weak();
        move || {
            if let Some(ui) = ui.upgrade()
                && ui.get_single_side() == 1
                && ui.get_can_write()
            {
                ui.invoke_delete();
            }
        }
    });
    ui.on_inspector_purge({
        let (ui, m) = (ui.as_weak(), m.clone());
        move || {
            if let Some(ui) = ui.upgrade() {
                purge(&m, &ui);
            }
        }
    });
    ui.on_purge({
        let (ui, m) = (ui.as_weak(), m.clone());
        move || {
            if let Some(ui) = ui.upgrade() {
                purge(&m, &ui);
            }
        }
    });
    restore_view(ui, m);
}

/// Opens in the view the user left at the last start (read on a blocking thread: the UI state is
/// in the site database).
fn restore_view(ui: &AppWindow, m: &Manager) {
    let (weak, session) = (ui.as_weak(), m.session.clone());
    m.link.rt().spawn(async move {
        let Ok(state) = tokio::task::spawn_blocking(move || session.ui_state()).await else {
            return;
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let Some(m) = super::manager() else {
                return;
            };
            if matches!(state.view, ViewMode::Inspector) {
                set_view(&ui, &m, 1);
            }
            if state.grid {
                set_grid(&ui, &m, true);
            }
        });
    });
}
