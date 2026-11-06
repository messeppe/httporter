// SPDX-License-Identifier: MIT OR Apache-2.0

//! The app's file cache: `%LOCALAPPDATA%\HTTPorter\cache\{preview,thumbs,edit,move}`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The cache folder for `kind` (`preview`, `thumbs`, `edit` or `move`).
pub fn cache_dir(kind: &str) -> PathBuf {
    cache_root().join(kind)
}

/// `%LOCALAPPDATA%\HTTPorter\cache`, or under `HTTPORTER_DATA_DIR` (see [`crate::paths`]).
fn cache_root() -> PathBuf {
    crate::paths::local_dir().join("cache")
}

/// Deletes the cached files not changed for `older_than`, and the folders that leaves empty
/// (never a kind's own folder); how many bytes went. Runs at start, on its own thread. Never
/// fails: a file that can't be deleted stays.
pub fn prune_cache(older_than: Duration) -> u64 {
    let cutoff = SystemTime::now()
        .checked_sub(older_than)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let root = cache_root();
    let Ok(kinds) = fs::read_dir(&root) else {
        return 0;
    };
    let mut freed = 0;
    for kind in kinds.flatten() {
        let path = kind.path();
        if kind.file_type().is_ok_and(|t| t.is_dir()) {
            freed += prune_dir(&path, cutoff);
        } else if let Some(bytes) = old_file(&path, cutoff)
            && fs::remove_file(&path).is_ok()
        {
            freed += bytes;
        }
    }
    freed
}

/// Prunes the files under `dir` last changed before `cutoff`, and the empty folders within it;
/// the bytes freed.
fn prune_dir(dir: &Path, cutoff: SystemTime) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut freed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            freed += prune_dir(&path, cutoff);
            // Fails, as it should, while anything is left in it.
            let _ = fs::remove_dir(&path);
        } else if let Some(bytes) = old_file(&path, cutoff)
            && fs::remove_file(&path).is_ok()
        {
            freed += bytes;
        }
    }
    freed
}

/// The size of the file at `path` if it was last changed before `cutoff`.
fn old_file(path: &Path, cutoff: SystemTime) -> Option<u64> {
    let meta = fs::symlink_metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    (modified < cutoff).then_some(meta.len())
}

/// How many bytes the cache holds.
pub fn cache_bytes() -> u64 {
    dir_bytes(&cache_root())
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => dir_bytes(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}
