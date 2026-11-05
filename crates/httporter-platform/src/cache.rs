//! The app's file cache (T4): `%LOCALAPPDATA%\HTTPorter\cache\{preview,thumbs,edit,move}`.
#![allow(unused_variables, dead_code)]

use std::path::PathBuf;
use std::time::Duration;

/// The cache folder for `kind` (`preview`, `thumbs`, `edit` or `move`).
pub fn cache_dir(kind: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_default()
        .join("HTTPorter")
        .join("cache")
        .join(kind)
}

/// Deletes the cached files not changed for `older_than`; how many bytes went. Runs at start, on
/// its own thread. T1: does nothing yet (startup path; T4 fills it).
pub fn prune_cache(older_than: Duration) -> u64 {
    0
}

/// How many bytes the cache holds.
pub fn cache_bytes() -> u64 {
    todo!("T4")
}
