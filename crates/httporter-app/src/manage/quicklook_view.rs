// SPDX-License-Identifier: MIT OR Apache-2.0

//! Quick Look: a file of either pane, shown over the window without leaving it, text with line
//! numbers and syntax colours, an image, or the file's kind with a way to open it, and, for
//! text, its changes against the copy on the other side ("Changes vs Bunny").
//!
//! Space (or the menu, or the inspector) opens it on the pane's row; ← → step through the
//! selection, or the folder's files when one file is selected. A site's file is read from the
//! preview cache (`fetch_to_cache`). Text is shown up to 2 MB and 20,000 lines; the list is
//! virtualized, each row built when it scrolls into view.
//!
//! The counterpart for the changes is the file with the same name in the folder the other pane
//! shows, else the one a publish profile's folders pair it with. The site's side is the old text,
//! this PC's the new one.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use httporter_core::{
    DiffOp, Entry, ProviderKind, RemotePath, SiteRecord, diff_lines, encode_segment,
};
use httporter_platform::win;
use slint::{ComponentHandle, Model, ModelRc, ModelTracker, SharedString, VecModel};

use super::highlight::{self, Highlighter, Span};
use super::{Manager, Side, copy, edit};
use crate::session::blocking;
use crate::{AppWindow, CodeLine, CodeSpan, CompareStatus, QuickLookData, Theme};

/// Text is shown up to this many bytes …
const TEXT_MAX: usize = 2 << 20;
/// … and this many lines.
const LINES_MAX: usize = 20_000;
/// How much of a file of unknown kind is looked at to tell whether it is text.
const SNIFF: usize = 8 << 10;

/// What [`Manager::fetch_to_cache`] keeps Quick Look's copies of site files under.
const PREVIEW: &str = "preview";

/// `ql-mode` values; 2 is not a view but the "Open in Default App" button of a file that can't be
/// shown.
const MODE_SOURCE: i32 = 0;
const MODE_CHANGES: i32 = 1;
const MODE_OPEN_DEFAULT: i32 = 2;

/// `QuickLookData.kind`.
const KIND_TEXT: i32 = 0;
const KIND_IMAGE: i32 = 1;
const KIND_OTHER: i32 = 2;

/// A file of the list Quick Look steps through: a row of the pane it was opened on.
#[derive(Clone)]
struct Item {
    name: String,
    status: CompareStatus,
}

/// Where the file shown is. One at a time, so its size does not matter.
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum Source {
    Local(PathBuf),
    Remote(SiteRecord, Entry),
}

/// The other side's copy, for the changes.
#[derive(Clone)]
enum Counterpart {
    /// A site's file, for a file on this PC.
    Remote(SiteRecord, RemotePath),
    /// A file on this PC, for a site's file.
    Local(PathBuf),
}

/// One row of the list, as the window will show it.
#[derive(Clone)]
struct LineData {
    /// 1-based; 0 for none.
    no: u32,
    spans: Vec<Span>,
    /// 0 none, 1 added, 2 removed.
    mark: i32,
}

/// A file's text as read.
struct Text {
    text: String,
    encoding: &'static str,
    eol: &'static str,
    /// Only the first part is shown (2 MB, 20,000 lines).
    cut: bool,
    lines: Vec<LineData>,
}

enum View {
    Text(Text),
    Image,
    Other,
}

/// The file shown, once read.
struct Loaded {
    source: Source,
    /// The text, for the changes (only when it can be compared).
    text: Option<Arc<String>>,
    encoding: &'static str,
    eol: &'static str,
    lines: Arc<Vec<LineData>>,
    footer: String,
    counterpart: Option<Counterpart>,
    diff: Option<(Arc<Vec<LineData>>, String)>,
}

/// What this view remembers between callbacks.
#[derive(Default)]
pub struct QuickLookState {
    /// Goes up with every file shown and when Quick Look closes: a read or a diff finishing for
    /// an older one is dropped.
    generation: u64,
    side: Option<Side>,
    /// The pane's folder (This PC), or the open site, when Quick Look opened.
    local_dir: Option<PathBuf>,
    site: Option<SiteRecord>,
    items: Vec<Item>,
    index: usize,
    loaded: Option<Loaded>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wires this view's callbacks of `ui` to `m`.
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {
    let weak = ui.as_weak();
    ui.on_quick_look({
        let (m, weak) = (m.clone(), weak.clone());
        move |pane| {
            if let Some(ui) = weak.upgrade() {
                let side = if pane == 0 { Side::Local } else { Side::Remote };
                show(&ui, &m, side);
            }
        }
    });
    ui.on_ql_set_mode({
        let (m, weak) = (m.clone(), weak.clone());
        move |mode| {
            if let Some(ui) = weak.upgrade() {
                set_mode(&ui, &m, mode);
            }
        }
    });
    ui.on_ql_step({
        let (m, weak) = (m.clone(), weak.clone());
        move |by| {
            if let Some(ui) = weak.upgrade() {
                step(&ui, &m, by);
            }
        }
    });
    ui.on_ql_open_browser({
        let m = m.clone();
        move || open_browser(&m)
    });
    ui.on_ql_open_with({
        let m = m.clone();
        move || open_outside(&m, edit::How::With)
    });
    ui.on_ql_close({
        let (m, weak) = (m.clone(), weak.clone());
        move || {
            if let Some(ui) = weak.upgrade() {
                close(&ui, &m);
            }
        }
    });
}

/// Opens Quick Look on the row the keyboard is on in `side` (or its first selected file), to
/// step through the selection or the folder's files. The inspector's Quick Look uses it too.
pub(crate) fn show(ui: &AppWindow, m: &Arc<Manager>, side: Side) {
    let Some((items, index)) = pick(ui, side) else {
        return;
    };
    let (local_dir, site) = match side {
        Side::Local => match m.session.local_dir() {
            Some(dir) => (Some(dir), None),
            None => return,
        },
        Side::Remote => match m.session.remote_place() {
            Some(place) => (None, Some(place.site)),
            None => return,
        },
    };
    set_mono_font(ui);
    {
        let mut st = lock(&m.quicklook);
        st.side = Some(side);
        st.local_dir = local_dir;
        st.site = site;
        st.items = items;
        st.index = index;
        st.loaded = None;
    }
    ui.set_ql_mode(MODE_SOURCE);
    ui.set_ql_open(true);
    load(ui, m);
}

/// The files to step through and where to start: the selected files when there are two or more,
/// else every file of the pane; starting at the keyboard's row, or the first selected file.
fn pick(ui: &AppWindow, side: Side) -> Option<(Vec<Item>, usize)> {
    let (rows, focus) = match side {
        Side::Local => (ui.get_local_rows(), ui.get_local_selected()),
        Side::Remote => (ui.get_remote_rows(), ui.get_remote_selected()),
    };
    let files: Vec<(usize, crate::FileRow)> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| !row.folder)
        .collect();
    let selected = files.iter().filter(|(_, row)| row.selected).count();
    let list: Vec<(usize, crate::FileRow)> = if selected >= 2 {
        files.into_iter().filter(|(_, row)| row.selected).collect()
    } else {
        files
    };
    let focus = usize::try_from(focus).ok();
    let start = focus
        .and_then(|focus| list.iter().position(|(i, _)| *i == focus))
        .or_else(|| list.iter().position(|(_, row)| row.selected))?;
    let items = list
        .into_iter()
        .map(|(_, row)| Item {
            name: row.name.to_string(),
            status: row.status,
        })
        .collect();
    Some((items, start))
}

/// Sets the monospaced font the code is drawn with, the first time (ledger 135).
fn set_mono_font(ui: &AppWindow) {
    static FONT: OnceLock<&'static str> = OnceLock::new();
    let font = *FONT.get_or_init(win::mono_font_family);
    ui.global::<Theme>().set_font_mono(font.into());
}

/// Shows the list's current file: its header at once, then its content once read.
fn load(ui: &AppWindow, m: &Arc<Manager>) {
    let (generation, item, can_step, source) = {
        let mut st = lock(&m.quicklook);
        st.generation += 1;
        st.loaded = None;
        let Some(item) = st.items.get(st.index).cloned() else {
            return;
        };
        let source = match (st.side, &st.local_dir, &st.site) {
            (Some(Side::Local), Some(dir), _) => Some(Source::Local(dir.join(&item.name))),
            (Some(Side::Remote), _, Some(site)) => m
                .session
                .remote_entries(std::slice::from_ref(&item.name))
                .pop()
                .map(|entry| Source::Remote(site.clone(), entry)),
            _ => None,
        };
        (st.generation, item, st.items.len() > 1, source)
    };
    let label = match &source {
        Some(Source::Remote(site, _)) => copy::ql_remote_label(site),
        _ => open_site_label(m),
    };
    ui.set_ql(QuickLookData {
        title: item.name.as_str().into(),
        subtitle: SharedString::new(),
        kind: if is_image(&item.name) {
            KIND_IMAGE
        } else {
            KIND_TEXT
        },
        can_diff: false,
        diff_label: copy::ql_changes_vs(&label).into(),
        diff_note: SharedString::new(),
        footer_left: copy::ql_hint(can_step).into(),
        footer_right: SharedString::new(),
        busy: true,
        error: SharedString::new(),
        can_browser: false,
    });
    ui.set_ql_lines(ModelRc::default());
    ui.set_ql_image(slint::Image::default());
    let Some(source) = source else {
        show_error(ui, copy::QL_FILE_GONE.to_owned());
        return;
    };
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        let read = read_source(&m, &source).await;
        let counterpart = match &read {
            Ok((_, _, View::Text(text))) if !text.cut => find_counterpart(&m, &source).await,
            _ => None,
        };
        let weak = m.link.ui().clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            loaded(&ui, &m, generation, item, source, read, counterpart);
        });
    });
}

/// How the changes name the cloud pane's site: Bunny, or an S3 site's name.
fn open_site_label(m: &Manager) -> String {
    m.session.remote_place().map_or_else(
        || crate::copy::BUNNY.to_owned(),
        |place| copy::ql_remote_label(&place.site),
    )
}

/// Reads the file of `source` (a site's through the preview cache): where its content is, its
/// size and what it shows.
async fn read_source(m: &Manager, source: &Source) -> Result<(PathBuf, u64, View), String> {
    let (file, name) = match source {
        Source::Local(path) => (path.clone(), file_name(path)),
        Source::Remote(site, entry) => (
            m.fetch_to_cache(site, entry, PREVIEW).await?,
            entry.name().to_owned(),
        ),
    };
    let path = file.clone();
    let (size, view) = blocking(move || read_file(&path, &name))
        .await
        .map_err(copy::ql_read_failed)?;
    Ok((file, size, view))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The extension of `name`, lower case ("" for none).
fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_image(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"
    )
}

fn is_html(name: &str) -> bool {
    matches!(extension(name).as_str(), "html" | "htm" | "xhtml")
}

/// Files that are text by their extension; others are looked at.
fn is_text_name(name: &str) -> bool {
    matches!(
        extension(name).as_str(),
        "html"
            | "htm"
            | "xhtml"
            | "css"
            | "js"
            | "mjs"
            | "cjs"
            | "json"
            | "xml"
            | "svg"
            | "txt"
            | "md"
            | "map"
            | "csv"
            | "yml"
            | "yaml"
            | "toml"
            | "ini"
            | "webmanifest"
            | "log"
            | "ts"
    )
}

/// Whether `head` (the start of a file) looks like text: no NUL byte, and UTF-8 (a character cut
/// at the end of `head` is fine).
fn looks_like_text(head: &[u8]) -> bool {
    !head.contains(&0)
        && match std::str::from_utf8(head) {
            Ok(_) => true,
            Err(e) => e.error_len().is_none(),
        }
}

/// The line ending `text` uses.
fn eol_of(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    match (crlf, lf) {
        (_, 0) => "",
        (0, _) => copy::QL_LF,
        (c, l) if c == l => copy::QL_CRLF,
        _ => copy::QL_MIXED_EOL,
    }
}

/// Reads `path` (called `name`): its size, and its text (highlighted), an image, or neither.
fn read_file(path: &Path, name: &str) -> std::io::Result<(u64, View)> {
    let size = std::fs::metadata(path)?.len();
    if is_image(name) {
        return Ok((size, View::Image));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(TEXT_MAX as u64 + 1)
        .read_to_end(&mut bytes)?;
    if !is_text_name(name) && !looks_like_text(&bytes[..bytes.len().min(SNIFF)]) {
        return Ok((size, View::Other));
    }
    let mut cut = bytes.len() > TEXT_MAX;
    bytes.truncate(TEXT_MAX);
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let (text, encoding) = match std::str::from_utf8(body) {
        Ok(text) => (text.to_owned(), copy::QL_UTF8),
        // Cut in the middle of a character.
        Err(e) if cut && e.error_len().is_none() => (
            String::from_utf8_lossy(&body[..e.valid_up_to()]).into_owned(),
            copy::QL_UTF8,
        ),
        Err(_) => (
            String::from_utf8_lossy(body).into_owned(),
            copy::QL_NOT_UTF8,
        ),
    };
    let mut highlighter = Highlighter::new(highlight::lang_for(name));
    let mut lines = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i >= LINES_MAX {
            cut = true;
            break;
        }
        lines.push(LineData {
            no: u32::try_from(i + 1).unwrap_or(u32::MAX),
            spans: highlighter.line(line),
            mark: 0,
        });
    }
    let eol = eol_of(&text);
    Ok((
        size,
        View::Text(Text {
            text,
            encoding,
            eol,
            cut,
            lines,
        }),
    ))
}

/// The other side's copy of the file of `source`, if there is one.
async fn find_counterpart(m: &Manager, source: &Source) -> Option<Counterpart> {
    match source {
        Source::Local(path) => {
            let name = file_name(path);
            // The panes' folders: the file of the same name in the folder the cloud pane shows.
            if let (Some(dir), Some(place)) = (m.session.local_dir(), m.session.remote_place())
                && path.parent() == Some(dir.as_path())
                && !m
                    .session
                    .remote_entries(std::slice::from_ref(&name))
                    .is_empty()
                && let Ok(remote) = place.dir.join(&name)
            {
                return Some(Counterpart::Remote(place.site, remote));
            }
            // A publish profile whose folder holds it.
            for profile in m.session.profiles().await {
                let Ok(rel) = path.strip_prefix(&profile.local_dir) else {
                    continue;
                };
                let Some(site) = m.session.site(&profile.site) else {
                    continue;
                };
                let Some(remote) = remote_below(&profile.remote_dir, rel) else {
                    continue;
                };
                let Ok(provider) = m.session.connect(&site).await else {
                    continue;
                };
                if provider.stat(&remote).await.is_ok() {
                    return Some(Counterpart::Remote(site, remote));
                }
            }
            None
        }
        Source::Remote(site, entry) => {
            if let (Some(dir), Some((open, shows))) =
                (m.session.local_dir(), m.session.remote_shows())
                && open == site.id
                && entry.path.parent().as_ref() == Some(&shows)
            {
                let local = dir.join(entry.name());
                if is_file(&local).await {
                    return Some(Counterpart::Local(local));
                }
            }
            for profile in m.session.profiles().await {
                if profile.site != site.id || !entry.path.is_within(&profile.remote_dir) {
                    continue;
                }
                let skip = profile.remote_dir.segments().count();
                let mut local = profile.local_dir.clone();
                local.extend(entry.path.segments().skip(skip));
                if is_file(&local).await {
                    return Some(Counterpart::Local(local));
                }
            }
            None
        }
    }
}

/// `dir` on a site joined with the relative local path `rel`.
fn remote_below(dir: &RemotePath, rel: &Path) -> Option<RemotePath> {
    let mut path = dir.clone();
    for part in rel.components() {
        let std::path::Component::Normal(part) = part else {
            return None;
        };
        path = path.join(part.to_str()?).ok()?;
    }
    (!path.is_root()).then_some(path)
}

async fn is_file(path: &Path) -> bool {
    let path = path.to_path_buf();
    blocking(move || path.is_file()).await
}

/// A file's read is in: shows it, unless Quick Look has moved on (`generation`).
fn loaded(
    ui: &AppWindow,
    m: &Arc<Manager>,
    generation: u64,
    item: Item,
    source: Source,
    read: Result<(PathBuf, u64, View), String>,
    counterpart: Option<Counterpart>,
) {
    let mut st = lock(&m.quicklook);
    if st.generation != generation || !ui.get_ql_open() {
        return;
    }
    let (file, size, view) = match read {
        Ok(read) => read,
        Err(error) => {
            drop(st);
            show_error(ui, error);
            return;
        }
    };
    let side = st.side.unwrap_or(Side::Local);
    let mut data = ui.get_ql();
    data.busy = false;
    let place = match &source {
        Source::Local(_) => copy::QL_ON_THIS_PC.to_owned(),
        Source::Remote(site, _) => copy::ql_on_site(&site.name),
    };
    let note = match (side, item.status) {
        (Side::Local, CompareStatus::Changed) => Some(copy::QL_CHANGED_SINCE_PUBLISH),
        (Side::Local, CompareStatus::New) => Some(copy::QL_NOT_PUBLISHED),
        (Side::Remote, CompareStatus::Changed) => Some(copy::QL_DIFFERS_FROM_PC),
        (Side::Remote, CompareStatus::RemoteOnly) => Some(copy::QL_NOT_ON_PC),
        _ => None,
    };
    data.subtitle = copy::ql_subtitle(
        &extension(&item.name).to_ascii_uppercase(),
        size,
        &place,
        note,
    )
    .into();
    data.can_browser = match &source {
        Source::Local(_) => is_html(&item.name),
        Source::Remote(..) => true,
    };
    let label = match (&source, &counterpart) {
        (Source::Remote(site, _), _) | (_, Some(Counterpart::Remote(site, _))) => {
            copy::ql_remote_label(site)
        }
        _ => open_site_label(m),
    };
    data.diff_label = copy::ql_changes_vs(&label).into();
    let mut loaded = Loaded {
        source: source.clone(),
        text: None,
        encoding: "",
        eol: "",
        lines: Arc::new(Vec::new()),
        footer: String::new(),
        counterpart: None,
        diff: None,
    };
    match view {
        View::Text(text) => {
            data.kind = KIND_TEXT;
            loaded.footer =
                copy::ql_text_footer(text.encoding, text.eol, text.lines.len(), text.cut);
            data.footer_right = loaded.footer.as_str().into();
            loaded.lines = Arc::new(text.lines);
            loaded.encoding = text.encoding;
            loaded.eol = text.eol;
            data.can_diff = counterpart.is_some();
            data.diff_note = if text.cut {
                copy::QL_TOO_LARGE_TO_COMPARE.into()
            } else if counterpart.is_some() {
                SharedString::new()
            } else {
                match &source {
                    Source::Remote(..) => copy::QL_NO_LOCAL_COPY.into(),
                    Source::Local(_) => copy::ql_no_counterpart(&label).into(),
                }
            };
            loaded.text = data.can_diff.then(|| Arc::new(text.text));
            loaded.counterpart = counterpart;
            ui.set_ql_lines(lines_model(&loaded.lines));
        }
        View::Image => {
            data.kind = KIND_IMAGE;
            data.diff_note = copy::QL_ONLY_TEXT_COMPARES.into();
            match slint::Image::load_from_path(&file) {
                Ok(image) => {
                    let size = image.size();
                    data.footer_right = copy::ql_image_footer(size.width, size.height).into();
                    ui.set_ql_image(image);
                }
                Err(_) => data.error = copy::QL_IMAGE_FAILED.into(),
            }
        }
        View::Other => {
            data.kind = KIND_OTHER;
            data.diff_note = copy::QL_ONLY_TEXT_COMPARES.into();
        }
    }
    let wants_changes = ui.get_ql_mode() == MODE_CHANGES;
    let can_diff = data.can_diff;
    st.loaded = Some(loaded);
    drop(st);
    ui.set_ql(data);
    if wants_changes {
        if can_diff {
            show_changes(ui, m);
        } else {
            ui.set_ql_mode(MODE_SOURCE);
        }
    }
}

fn show_error(ui: &AppWindow, error: String) {
    let mut data = ui.get_ql();
    data.busy = false;
    data.error = error.into();
    ui.set_ql(data);
}

/// The list's model: rows built as they are shown.
fn lines_model(lines: &Arc<Vec<LineData>>) -> ModelRc<CodeLine> {
    ModelRc::new(CodeLines(lines.clone()))
}

struct CodeLines(Arc<Vec<LineData>>);

impl Model for CodeLines {
    type Data = CodeLine;

    fn row_count(&self) -> usize {
        self.0.len()
    }

    fn row_data(&self, row: usize) -> Option<CodeLine> {
        let line = self.0.get(row)?;
        let spans: Vec<CodeSpan> = line
            .spans
            .iter()
            .map(|(text, style)| CodeSpan {
                text: text.as_str().into(),
                style: *style,
            })
            .collect();
        Some(CodeLine {
            no: if line.no == 0 {
                SharedString::new()
            } else {
                line.no.to_string().into()
            },
            spans: ModelRc::new(VecModel::from(spans)),
            mark: line.mark,
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}

/// Source or the changes (or, for a file that can't be shown, its "Open in Default App").
fn set_mode(ui: &AppWindow, m: &Arc<Manager>, mode: i32) {
    match mode {
        MODE_CHANGES => {
            if ui.get_ql().can_diff {
                show_changes(ui, m);
            } else {
                ui.set_ql_mode(MODE_SOURCE);
            }
        }
        MODE_OPEN_DEFAULT => open_outside(m, edit::How::Default),
        _ => {
            let st = lock(&m.quicklook);
            let Some(loaded) = &st.loaded else {
                return;
            };
            let (lines, footer) = (loaded.lines.clone(), loaded.footer.clone());
            drop(st);
            ui.set_ql_lines(lines_model(&lines));
            let mut data = ui.get_ql();
            data.footer_right = footer.into();
            ui.set_ql(data);
        }
    }
}

/// The changes: from what was worked out already, or worked out now.
fn show_changes(ui: &AppWindow, m: &Arc<Manager>) {
    let (generation, job) = {
        let st = lock(&m.quicklook);
        let Some(loaded) = &st.loaded else {
            return;
        };
        if let Some((lines, footer)) = &loaded.diff {
            let (lines, footer) = (lines.clone(), footer.clone());
            drop(st);
            ui.set_ql_lines(lines_model(&lines));
            let mut data = ui.get_ql();
            data.footer_right = footer.into();
            ui.set_ql(data);
            return;
        }
        let (Some(text), Some(counterpart)) = (&loaded.text, &loaded.counterpart) else {
            return;
        };
        let name = match &loaded.source {
            Source::Local(path) => file_name(path),
            Source::Remote(_, entry) => entry.name().to_owned(),
        };
        (
            st.generation,
            (
                text.clone(),
                counterpart.clone(),
                name,
                loaded.encoding,
                loaded.eol,
            ),
        )
    };
    let mut data = ui.get_ql();
    data.busy = true;
    ui.set_ql(data);
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        let (text, counterpart, name, encoding, eol) = job;
        let result = diff_against(&m, &text, &counterpart, &name).await;
        let weak = m.link.ui().clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let mut st = lock(&m.quicklook);
            if st.generation != generation {
                return;
            }
            let mut data = ui.get_ql();
            data.busy = false;
            match result {
                Ok((lines, changed)) => {
                    let lines = Arc::new(lines);
                    let footer = copy::ql_diff_footer(encoding, eol, changed);
                    if let Some(loaded) = st.loaded.as_mut() {
                        loaded.diff = Some((lines.clone(), footer.clone()));
                    }
                    drop(st);
                    if ui.get_ql_mode() == MODE_CHANGES {
                        ui.set_ql_lines(lines_model(&lines));
                        data.footer_right = footer.into();
                    }
                }
                Err(note) => {
                    drop(st);
                    data.can_diff = false;
                    data.diff_note = note.into();
                    ui.set_ql_mode(MODE_SOURCE);
                }
            }
            ui.set_ql(data);
        });
    });
}

/// The changes of `text` (the file shown) against `counterpart`: the site's copy is the old
/// text, this PC's the new one. The lines, and how many changed.
async fn diff_against(
    m: &Manager,
    text: &str,
    counterpart: &Counterpart,
    name: &str,
) -> Result<(Vec<LineData>, usize), String> {
    let (other, local_is_new) = match counterpart {
        Counterpart::Remote(site, path) => {
            let provider = m.session.connect(site).await?;
            let info = provider
                .stat(path)
                .await
                .map_err(|e| crate::copy::describe_transfer_error(&e))?;
            if info.entry.size > TEXT_MAX as u64 {
                return Err(copy::QL_TOO_LARGE_TO_COMPARE.to_owned());
            }
            let file = m.fetch_to_cache(site, &info.entry, PREVIEW).await?;
            (read_text(file).await?, true)
        }
        Counterpart::Local(path) => (read_text(path.clone()).await?, false),
    };
    let (old, new) = if local_is_new {
        (other, text.to_owned())
    } else {
        (text.to_owned(), other)
    };
    let lang = highlight::lang_for(name);
    blocking(move || -> Result<(Vec<LineData>, usize), String> {
        let diff =
            diff_lines(&old, &new).ok_or_else(|| copy::QL_TOO_LARGE_TO_COMPARE.to_owned())?;
        let (mut old_hl, mut new_hl) = (Highlighter::new(lang), Highlighter::new(lang));
        let mut changed = 0;
        let lines = diff
            .into_iter()
            .map(|line| match line.op {
                DiffOp::Same => {
                    old_hl.line(&line.text);
                    LineData {
                        no: line.new_no.unwrap_or(0),
                        spans: new_hl.line(&line.text),
                        mark: 0,
                    }
                }
                DiffOp::Added => {
                    changed += 1;
                    LineData {
                        no: line.new_no.unwrap_or(0),
                        spans: new_hl.line(&line.text),
                        mark: 1,
                    }
                }
                DiffOp::Removed => {
                    changed += 1;
                    LineData {
                        no: line.old_no.unwrap_or(0),
                        spans: old_hl.line(&line.text),
                        mark: 2,
                    }
                }
            })
            .collect();
        Ok((lines, changed))
    })
    .await
}

/// The text of `path`, if it is small enough to compare.
async fn read_text(path: PathBuf) -> Result<String, String> {
    blocking(move || {
        let size = std::fs::metadata(&path)
            .map_err(copy::ql_read_failed)?
            .len();
        if size > TEXT_MAX as u64 {
            return Err(copy::QL_TOO_LARGE_TO_COMPARE.to_owned());
        }
        let bytes = std::fs::read(&path).map_err(copy::ql_read_failed)?;
        let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        Ok(String::from_utf8_lossy(body).into_owned())
    })
    .await
}

/// ← and →: the previous or next file of the list (no wrapping round).
fn step(ui: &AppWindow, m: &Arc<Manager>, by: i32) {
    {
        let mut st = lock(&m.quicklook);
        let Some(next) = st.index.checked_add_signed(by as isize) else {
            return;
        };
        if next >= st.items.len() || next == st.index {
            return;
        }
        st.index = next;
    }
    load(ui, m);
}

/// Quick Look closed: what it held goes.
fn close(ui: &AppWindow, m: &Arc<Manager>) {
    {
        let mut st = lock(&m.quicklook);
        st.generation += 1;
        st.items.clear();
        st.loaded = None;
        st.site = None;
        st.local_dir = None;
    }
    ui.set_ql_lines(ModelRc::default());
    ui.set_ql_image(slint::Image::default());
}

/// Open in Browser: a site's file at its public address, an HTML file of this PC as itself.
fn open_browser(m: &Arc<Manager>) {
    let source = lock(&m.quicklook).loaded.as_ref().map(|l| l.source.clone());
    let Some(source) = source else {
        return;
    };
    let m = m.clone();
    let rt = m.link.rt().clone();
    rt.spawn(async move {
        let (url, name) = match &source {
            Source::Local(path) => (Some(file_url(path)), file_name(path)),
            Source::Remote(site, entry) => {
                let url = public_url(&m, site, &entry.path).await;
                if url.is_none() {
                    m.link.status(copy::ql_no_public_url(&site.name));
                    return;
                }
                (url, entry.name().to_owned())
            }
        };
        let Some(url) = url else {
            return;
        };
        if let Err(e) = blocking(move || win::open_url(&url)).await {
            m.link.status(copy::ql_open_failed(&name, e));
        }
    });
}

/// `path` as a `file:` URL.
fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let encoded: Vec<String> = path
        .split('/')
        .enumerate()
        .map(|(i, part)| {
            // The drive ("C:") stays as it is.
            if i == 0 && part.ends_with(':') {
                part.to_owned()
            } else {
                encode_segment(part)
            }
        })
        .collect();
    format!("file:///{}", encoded.join("/"))
}

/// Where a site's file is served: the site's public base URL (Settings › Sites), else its Bunny
/// pull zone's first hostname.
async fn public_url(m: &Manager, site: &SiteRecord, path: &RemotePath) -> Option<String> {
    let encoded: Vec<String> = path.segments().map(encode_segment).collect();
    let encoded = encoded.join("/");
    let base = m
        .session
        .site_extras(&site.id)
        .public_base_url
        .filter(|base| !base.trim().is_empty());
    if let Some(base) = base {
        return Some(format!("{}/{encoded}", base.trim().trim_end_matches('/')));
    }
    if site.provider != ProviderKind::Bunny {
        return None;
    }
    let link = m.session.cdn_link(site.id.clone()).await?;
    let host = link
        .zones
        .iter()
        .flat_map(|zone| zone.hostnames.iter())
        .next()?
        .clone();
    Some(format!("https://{host}/{encoded}"))
}

/// Open With… (or Open in Default App): a file of this PC opens in place; a site's file is
/// edited, so that saving it uploads it again.
fn open_outside(m: &Arc<Manager>, how: edit::How) {
    let source = {
        let st = lock(&m.quicklook);
        match &st.loaded {
            Some(loaded) => Some(loaded.source.clone()),
            None => match (st.side, &st.local_dir, st.items.get(st.index)) {
                (Some(Side::Local), Some(dir), Some(item)) => {
                    Some(Source::Local(dir.join(&item.name)))
                }
                _ => None,
            },
        }
    };
    match source {
        Some(Source::Local(path)) => edit::open_local_file(m, path, how),
        Some(Source::Remote(site, entry)) => edit::start(m, site, entry, how),
        None => {}
    }
}
