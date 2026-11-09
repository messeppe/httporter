// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thumbnails for the grid (T5): images up to 5 MB, scaled to fit 240 × 240 and kept as PNGs in
//! `cache_dir("thumbs")` (named after the file's path, size and time), made 4 at a time on
//! blocking threads, for the tiles in view only. A tile gets its thumbnail averaged down to the
//! exact pixels it is drawn at: the renderer's own shrinking only samples, which looks jagged.
//!
//! Images are read with Slint's own loader (the decoders the app links already). The PNGs are
//! written uncompressed by [`write_png`], so that no encoder is linked for them; the cache is
//! pruned with the others. While the grid shows, a timer looks every 200 ms at which tiles are in
//! view (the pane's `bounds`, with pane.slint's tile sizes) and asks for the thumbnails they lack.
//! Those made are kept in memory for the folder shown, so that a new listing of it (a refresh, a
//! new sort) gets them back at once.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use httporter_core::{Entry, SiteRecord};
use httporter_platform::cache_dir;
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer};
use tokio::sync::Semaphore;

use super::inspector_view::lock;
use super::{Manager, Side};
use crate::{AppWindow, FileKind, FileRow, PaneBounds};

/// pane.slint's `Grid`: a tile's width and height and the grid's side padding, in logical pixels.
const TILE_WIDTH: f32 = 120.0;
const TILE_HEIGHT: f32 = 132.0;
const GRID_PAD: f32 = 4.0;
/// The picture's square in a tile: pane.slint's `Grid.thumb` less its 4 px margins.
const TILE_IMAGE: f32 = 80.0;
/// Larger images keep their kind's icon.
const MAX_BYTES: u64 = 5 << 20;
/// A cached thumbnail fits in this many pixels each way (a tile's picture at three times its size).
const SIZE: u32 = 240;
/// How often the tiles in view are looked at while the grid shows.
const POLL: Duration = Duration::from_millis(200);
/// Thumbnails made at once.
const AT_ONCE: usize = 4;
/// Thumbnails kept in memory for each pane's folder; past it, they are dropped and made again
/// from the cache as they come back into view.
const KEPT: usize = 400;

/// What the grid remembers (inside the inspector's state, which is T5's too).
pub(super) struct Thumbs {
    /// The folder of each pane (This PC, the cloud pane) the two below are for.
    places: [String; 2],
    /// The thumbnails made for it, by file name, with the size and date the row showed then.
    made: [HashMap<String, (String, SharedPixelBuffer<Rgba8Pixel>)>; 2],
    /// The files asked for already (made, on their way, or that can't have one).
    asked: [HashSet<String>; 2],
    gate: Arc<Semaphore>,
}

impl Default for Thumbs {
    fn default() -> Self {
        Self {
            places: Default::default(),
            made: Default::default(),
            asked: Default::default(),
            gate: Arc::new(Semaphore::new(AT_ONCE)),
        }
    }
}

thread_local! {
    /// While the grid shows: the timer that looks at the tiles in view (UI thread).
    static TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// Nothing to wire: the grid's thumbnails follow the view ([`follow`]).
pub fn install(_ui: &AppWindow, _m: &Arc<Manager>) {}

fn index(side: Side) -> usize {
    match side {
        Side::Local => 0,
        Side::Remote => 1,
    }
}

/// The rows and the geometry of `side`'s pane.
fn pane_of(ui: &AppWindow, side: Side) -> (ModelRc<FileRow>, PaneBounds) {
    match side {
        Side::Local => (ui.get_local_rows(), ui.get_local_bounds()),
        Side::Remote => (ui.get_remote_rows(), ui.get_remote_bounds()),
    }
}

/// Starts looking at the tiles in view when the window shows the grid, and stops when it doesn't.
pub(super) fn follow(ui: &AppWindow, m: &Arc<Manager>) {
    let on = ui.get_view_mode() == 1 && ui.get_grid();
    TIMER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if !on {
            *slot = None;
            return;
        }
        if slot.is_none() {
            let timer = slint::Timer::default();
            let (ui, m) = (ui.as_weak(), Arc::downgrade(m));
            timer.start(slint::TimerMode::Repeated, POLL, move || {
                if let (Some(ui), Some(m)) = (ui.upgrade(), m.upgrade()) {
                    look(&ui, &m);
                }
            });
            *slot = Some(timer);
        }
    });
    if on {
        look(ui, m);
    }
}

/// The folder `side` shows, as a key for the thumbnails made in it; `None` while a listing is on
/// its way, at the list of drives, or with no site open.
fn place_of(m: &Manager, side: Side) -> Option<String> {
    match side {
        Side::Local => m
            .session
            .local_shows()
            .flatten()
            .map(|dir| dir.display().to_string()),
        Side::Remote => m
            .session
            .remote_shows()
            .map(|(site, dir)| format!("{site}/{}", dir.as_str())),
    }
}

/// The size and date a row shows: a file changed under the same name has other ones.
fn stamp(row: &FileRow) -> String {
    format!("{}|{}", row.size, row.modified)
}

/// The rows (of `count`) whose tiles are in view in a grid placed as `b` says, and a line more
/// on each side.
fn in_view(b: &PaneBounds, count: usize) -> Range<usize> {
    let columns = (((b.width - 2.0 * GRID_PAD) / TILE_WIDTH).floor() as usize).max(1);
    let list_height = (b.height - (b.list_top - b.y)).max(0.0);
    let first = (b.scroll_y.max(0.0) / TILE_HEIGHT).floor() as usize;
    let last = ((b.scroll_y.max(0.0) + list_height) / TILE_HEIGHT).ceil() as usize;
    let start = (first.saturating_sub(1) * columns).min(count);
    let end = ((last + 1) * columns).min(count);
    start..end.max(start)
}

/// Puts the thumbnails made already on the image tiles in view that lack one, and asks for the
/// others.
fn look(ui: &AppWindow, m: &Arc<Manager>) {
    if ui.get_view_mode() != 1 || !ui.get_grid() {
        return;
    }
    let side = if ui.get_single_side() == 1 {
        Side::Remote
    } else {
        Side::Local
    };
    let Some(place) = place_of(m, side) else {
        return;
    };
    let (rows, bounds) = pane_of(ui, side);
    let mut wanted = Vec::new();
    let gate = {
        let mut state = lock(&m.inspector);
        let thumbs = &mut state.thumbs;
        let i = index(side);
        if thumbs.places[i] != place {
            thumbs.places[i] = place.clone();
            thumbs.made[i].clear();
            thumbs.asked[i].clear();
        }
        for at in in_view(&bounds, rows.row_count()) {
            let Some(mut row) = rows.row_data(at) else {
                continue;
            };
            if row.folder || row.kind != FileKind::Image || row.thumb.size().width > 0 {
                continue;
            }
            let name = row.name.to_string();
            let now = stamp(&row);
            match thumbs.made[i].get(&name) {
                Some((then, pixels)) if *then == now => {
                    row.thumb = slint::Image::from_rgba8(pixels.clone());
                    rows.set_row_data(at, row);
                    continue;
                }
                // The file changed since: made again.
                Some(_) => {
                    thumbs.made[i].remove(&name);
                    thumbs.asked[i].remove(&name);
                }
                None => {}
            }
            if thumbs.asked[i].insert(name.clone()) {
                wanted.push((at, name, now));
            }
        }
        thumbs.gate.clone()
    };
    let fit = (TILE_IMAGE * ui.window().scale_factor()) as u32;
    for (at, name, now) in wanted {
        start(m, &gate, side, place.clone(), at, name, now, fit);
    }
}

/// Where a thumbnail is made from.
enum Source {
    Local(PathBuf),
    /// A site's file, fetched into the cache first.
    Remote(Box<(SiteRecord, Entry)>),
}

/// Makes the thumbnail of `name` (row `at` of `side`, in the folder `place`) on the runtime, then
/// shows it. `fit` is the picture's size on screen, in pixels.
#[allow(clippy::too_many_arguments)]
fn start(
    m: &Arc<Manager>,
    gate: &Arc<Semaphore>,
    side: Side,
    place: String,
    at: usize,
    name: String,
    now: String,
    fit: u32,
) {
    let source = match side {
        Side::Local => m
            .session
            .local_paths(std::slice::from_ref(&name))
            .pop()
            .map(Source::Local),
        Side::Remote => {
            let entry = m.session.remote_entries(std::slice::from_ref(&name)).pop();
            let site = m.session.remote_place().map(|place| place.site);
            entry
                .zip(site)
                .map(|(entry, site)| Source::Remote(Box::new((site, entry))))
        }
    };
    let Some(source) = source else {
        return;
    };
    let (m, gate) = (m.clone(), gate.clone());
    m.link.rt().clone().spawn(async move {
        let Ok(_turn) = gate.acquire_owned().await else {
            return;
        };
        let Some(pixels) = make(&m, source, fit).await else {
            return;
        };
        let manager = m.clone();
        let _ = m.link.ui().upgrade_in_event_loop(move |ui| {
            shown(&ui, &manager, side, &place, at, &name, &now, pixels);
        });
    });
}

/// The cache file of the thumbnail for `key` (a file's path, size and time).
fn cached_path(key: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    cache_dir("thumbs").join(format!("{:016x}.png", hasher.finish()))
}

/// The thumbnail of `source` at `fit` pixels: from the cache, or made (and kept there). `None`
/// for a file too large, one that can't be read or fetched, or one Slint can't decode.
async fn make(m: &Manager, source: Source, fit: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let (key, file) = match source {
        Source::Local(path) => {
            let meta = {
                let path = path.clone();
                crate::rt::spawn_blocking(move || fs::metadata(path))
                    .await
                    .ok()?
                    .ok()?
            };
            if meta.len() > MAX_BYTES {
                return None;
            }
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |t| t.as_nanos());
            let key = format!("local|{}|{}|{modified}", path.display(), meta.len());
            (key, Some(path))
        }
        Source::Remote(remote) => {
            let (site, entry) = *remote;
            if entry.size > MAX_BYTES {
                return None;
            }
            let key = format!(
                "site|{}|{}|{}|{:?}|{:?}",
                site.id,
                entry.path.as_str(),
                entry.size,
                entry.modified,
                entry.checksum.as_ref().map(|c| &c.hex)
            );
            let file = if cached_path(&key).exists() {
                None
            } else {
                Some(m.fetch_to_cache(&site, &entry, "preview").await.ok()?)
            };
            (key, file)
        }
    };
    let out = cached_path(&key);
    crate::rt::spawn_blocking(move || {
        if out.exists()
            && let Some((pixels, _)) = decode_scaled(&out, SIZE)
        {
            return Some(scale_down(&pixels, fit, fit));
        }
        let (pixels, _) = decode_scaled(&file?, SIZE)?;
        // A thumbnail that can't be kept is made again next time.
        let _ = write_png(&out, &pixels);
        Some(scale_down(&pixels, fit, fit))
    })
    .await
    .ok()
    .flatten()
}

/// A thumbnail is made: kept for the folder, and put on its tile if that still shows the file.
#[allow(clippy::too_many_arguments)]
fn shown(
    ui: &AppWindow,
    m: &Manager,
    side: Side,
    place: &str,
    at: usize,
    name: &str,
    now: &str,
    pixels: SharedPixelBuffer<Rgba8Pixel>,
) {
    {
        let mut state = lock(&m.inspector);
        let thumbs = &mut state.thumbs;
        let i = index(side);
        if thumbs.places[i] != place {
            return;
        }
        if thumbs.made[i].len() >= KEPT {
            // The tiles keep theirs; the others are made again (from the cache) as they show.
            thumbs.made[i].clear();
            thumbs.asked[i].clear();
        }
        thumbs.made[i].insert(name.to_owned(), (now.to_owned(), pixels.clone()));
    }
    let (rows, _) = pane_of(ui, side);
    // The row, where it was or wherever a new sort put it.
    let found = rows
        .row_data(at)
        .filter(|row| row.name == name)
        .map(|row| (at, row))
        .or_else(|| rows.iter().enumerate().find(|(_, row)| row.name == name));
    if let Some((at, mut row)) = found
        && stamp(&row) == now
    {
        row.thumb = slint::Image::from_rgba8(pixels);
        rows.set_row_data(at, row);
    }
}

/// The image at `path`, decoded by Slint and scaled down to fit `max` × `max` pixels, and its own
/// size. `None` when Slint can't read it. Call it on a blocking thread.
pub(super) fn decode_scaled(
    path: &Path,
    max: u32,
) -> Option<(SharedPixelBuffer<Rgba8Pixel>, (u32, u32))> {
    let image = slint::Image::load_from_path(path).ok()?;
    let size = image.size();
    let pixels = image.to_rgba8()?;
    Some((scale_down(&pixels, max, max), (size.width, size.height)))
}

/// `src` scaled down (keeping its shape) to fit `max_w` × `max_h`, each pixel the average of those
/// it covers (weighted by their alpha, so that transparent edges don't darken); `src` itself when
/// it fits.
pub(super) fn scale_down(
    src: &SharedPixelBuffer<Rgba8Pixel>,
    max_w: u32,
    max_h: u32,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let (w, h) = (src.width(), src.height());
    if (w <= max_w && h <= max_h) || w == 0 || h == 0 || max_w == 0 || max_h == 0 {
        return src.clone();
    }
    let scale = (f64::from(max_w) / f64::from(w)).min(f64::from(max_h) / f64::from(h));
    let tw = ((f64::from(w) * scale).round() as u32).clamp(1, max_w);
    let th = ((f64::from(h) * scale).round() as u32).clamp(1, max_h);
    let span = |i: u32, to: u32, from: u32| {
        let start = (u64::from(i) * u64::from(from) / u64::from(to)) as u32;
        let end = ((u64::from(i + 1) * u64::from(from) / u64::from(to)) as u32)
            .max(start + 1)
            .min(from);
        start..end
    };
    let pixels = src.as_slice();
    let mut out = SharedPixelBuffer::<Rgba8Pixel>::new(tw, th);
    let target = out.make_mut_slice();
    for ty in 0..th {
        let ys = span(ty, th, h);
        for tx in 0..tw {
            let xs = span(tx, tw, w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for y in ys.clone() {
                let line = &pixels[(y * w) as usize..((y + 1) * w) as usize];
                for p in &line[xs.start as usize..xs.end as usize] {
                    let alpha = u64::from(p.a);
                    r += u64::from(p.r) * alpha;
                    g += u64::from(p.g) * alpha;
                    b += u64::from(p.b) * alpha;
                    a += alpha;
                    n += 1;
                }
            }
            // Fully transparent: no colour to average.
            let clear = Rgba8Pixel {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            };
            target[(ty * tw + tx) as usize] = r.checked_div(a).map_or(clear, |red| Rgba8Pixel {
                r: red as u8,
                g: (g / a) as u8,
                b: (b / a) as u8,
                a: (a / n.max(1)) as u8,
            });
        }
    }
    out
}

/// Writes `image` to `path` as an RGBA PNG, its pixels stored without compression (deflate's
/// stored blocks), through a `.part` file renamed into place.
fn write_png(path: &Path, image: &SharedPixelBuffer<Rgba8Pixel>) -> io::Result<()> {
    let (w, h) = (image.width(), image.height());
    // Each line: filter type 0 (none), then its pixels.
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for line in image.as_slice().chunks(w.max(1) as usize) {
        raw.push(0);
        for p in line {
            raw.extend_from_slice(&[p.r, p.g, p.b, p.a]);
        }
    }
    // zlib: a header for deflate with no compression, stored blocks of up to 65,535 bytes, then
    // the Adler-32 of the raw bytes.
    let mut zlib = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(block) = blocks.next() {
        zlib.push(u8::from(blocks.peek().is_none()));
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&w.to_be_bytes());
    header.extend_from_slice(&h.to_be_bytes());
    // 8 bits per channel, RGBA, deflate, no filter method extras, not interlaced.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png_chunk(&mut png, b"IHDR", &header);
    png_chunk(&mut png, b"IDAT", &zlib);
    png_chunk(&mut png, b"IEND", &[]);

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let part = path.with_extension("part");
    fs::write(&part, &png)?;
    fs::rename(&part, path)
}

/// Appends a PNG chunk: its length, type, data and the CRC-32 of type and data.
fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}
