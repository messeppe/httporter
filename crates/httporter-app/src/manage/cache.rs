// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fetching a site's file into the local cache: `cache_dir(kind)/<site>/<path>`, written as
//! `.part`, checked against the listed checksum, then renamed.
//!
//! A file over [`ASK_ABOVE`] asks first (unless "Ask before previewing files over 50 MB" is off),
//! with the main window's warning sheet. The sheet's Escape closes it without a callback, so the
//! wait also watches the sheet's open flag.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use httporter_core::{ChecksumKind, Entry, SiteRecord};
use httporter_platform::cache_dir;
use httporter_transfer::{hash_file, safe_local_name};
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use super::{Manager, copy};
use crate::session::blocking;
use crate::{AppWindow, MoveWarningData};

/// Files above this size ask before they are fetched for a preview.
pub(super) const ASK_ABOVE: u64 = 50 * 1000 * 1000;

/// How often the wait for an answer checks whether the sheet was closed without one.
const ASK_POLL: Duration = Duration::from_millis(250);

/// The question on the warning sheet waiting for its answer, if any.
static PENDING_ASK: Mutex<Option<oneshot::Sender<()>>> = Mutex::new(None);

/// Wires this view's callbacks of `ui` to `m`: the warning sheet's confirm answers the question
/// waiting. (`move_view` wires it again later, handing it here first.)
pub fn install(ui: &AppWindow, _m: &Arc<Manager>) {
    ui.on_move_warning_confirm(|| {
        answer_ask();
    });
}

/// The warning sheet's confirm: answers the question waiting, if there is one (then `true`).
pub(super) fn answer_ask() -> bool {
    let pending = PENDING_ASK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    match pending {
        Some(answer) => {
            let _ = answer.send(());
            true
        }
        None => false,
    }
}

/// Asks on the warning sheet whether to go on; `true` if the user confirmed.
pub(super) async fn ask(m: &Manager, data: MoveWarningData) -> bool {
    let (answer, mut answered) = oneshot::channel();
    // A question still waiting is given up: its sender is dropped.
    *PENDING_ASK.lock().unwrap_or_else(PoisonError::into_inner) = Some(answer);
    let ui = m.link.ui().clone();
    let shown = ui.upgrade_in_event_loop(move |ui| {
        ui.set_move_warning(data);
        ui.set_move_warning_open(true);
    });
    if shown.is_err() {
        return false;
    }
    loop {
        match tokio::time::timeout(ASK_POLL, &mut answered).await {
            Ok(result) => return result.is_ok(),
            Err(_) => {
                let (tell, open) = oneshot::channel();
                let asked = ui.upgrade_in_event_loop(move |ui| {
                    let _ = tell.send(ui.get_move_warning_open());
                });
                if asked.is_err() || !open.await.unwrap_or(false) {
                    // Closed: by the confirm (whose answer is already in) or by Escape.
                    return answered.try_recv().is_ok();
                }
            }
        }
    }
}

/// See [`Manager::fetch_to_cache`].
pub(super) async fn fetch_to_cache(
    m: &Manager,
    site: &SiteRecord,
    entry: &Entry,
    kind: &str,
) -> Result<PathBuf, String> {
    let target = cache_path(kind, site, entry);
    let cached = {
        let (target, entry) = (target.clone(), entry.clone());
        blocking(move || reusable(&target, &entry)).await
    };
    if cached {
        return Ok(target);
    }
    if entry.size > ASK_ABOVE && m.session.ui_state().ask_big_preview {
        let data = MoveWarningData {
            title: copy::big_file_title(entry.name()).into(),
            body: copy::big_file_body(entry.size).into(),
            button: copy::BIG_FILE_BUTTON.into(),
        };
        if !ask(m, data).await {
            return Err(copy::BIG_FILE_DECLINED.to_owned());
        }
    }
    let provider = m.session.connect(site).await?;
    let mut stream = provider
        .download(&entry.path)
        .await
        .map_err(|e| crate::copy::describe_transfer_error(&e))?;
    let part = part_path(&target);
    if let Some(parent) = part.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }
    let written = async {
        let mut file = tokio::fs::File::create(&part)
            .await
            .map_err(|e| e.to_string())?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| crate::copy::describe_transfer_error(&e))?;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    let checked = {
        let (part, entry) = (part.clone(), entry.clone());
        blocking(move || matches_listing(&part, &entry)).await
    };
    if !checked {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(copy::FETCH_DAMAGED.to_owned());
    }
    // A copy left from before is replaced.
    let _ = tokio::fs::remove_file(&target).await;
    tokio::fs::rename(&part, &target)
        .await
        .map_err(|e| e.to_string())?;
    Ok(target)
}

/// `cache_dir(kind)/<site id>/<path>`, each segment made safe for Windows.
fn cache_path(kind: &str, site: &SiteRecord, entry: &Entry) -> PathBuf {
    let mut path = cache_dir(kind).join(safe_local_name(&site.id.0));
    for segment in entry.path.as_str().split('/').filter(|s| !s.is_empty()) {
        let safe = safe_local_name(segment);
        path.push(if safe.is_empty() {
            "_".to_owned()
        } else {
            safe
        });
    }
    path
}

/// `target` with `.part` added to its name.
fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

/// Whether the cached copy at `target` can stand for `entry`: the same size, and the same
/// checksum when one is listed (else not older than the remote file).
fn reusable(target: &Path, entry: &Entry) -> bool {
    let Ok(meta) = fs::metadata(target) else {
        return false;
    };
    if meta.len() != entry.size {
        return false;
    }
    if has_digest(entry) {
        return matches_listing(target, entry);
    }
    match (meta.modified(), entry.modified) {
        (Ok(cached), Some(remote)) => cached >= SystemTime::from(remote),
        _ => false,
    }
}

/// Whether the listing gives a digest of the content (not a multipart ETag).
fn has_digest(entry: &Entry) -> bool {
    entry
        .checksum
        .as_ref()
        .is_some_and(|c| c.kind != ChecksumKind::MultipartEtag)
}

/// Whether the file at `path` has the size and (when listed) the checksum of `entry`.
fn matches_listing(path: &Path, entry: &Entry) -> bool {
    let Ok(hashes) = hash_file(path) else {
        return false;
    };
    if hashes.size != entry.size {
        return false;
    }
    match &entry.checksum {
        Some(c) if c.kind == ChecksumKind::Sha256 => hex(&hashes.sha256) == c.hex,
        Some(c) if c.kind == ChecksumKind::Md5 => hex(&hashes.md5) == c.hex,
        _ => true,
    }
}

/// `bytes` as lower-case hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
