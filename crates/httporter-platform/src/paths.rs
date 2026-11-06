// SPDX-License-Identifier: MIT OR Apache-2.0
//! Where HTTPorter keeps its files on this PC.
//!
//! Normally the user's own folders: `%APPDATA%\HTTPorter` (the database, the logs) and
//! `%LOCALAPPDATA%\HTTPorter` (the cache). Windows reports those through its known-folder API,
//! which ignores environment variables, so a debug or test run would otherwise always open the
//! user's real database (and migrate it to a newer schema the installed release can't read).
//!
//! `HTTPORTER_DATA_DIR` (for development and tests only) moves both under one folder:
//! `<dir>\Roaming\HTTPorter` and `<dir>\Local\HTTPorter`. Both still end in `HTTPorter`, which
//! the reset's safety check relies on.

use std::path::PathBuf;

/// The environment variable that moves HTTPorter's folders (development and tests only).
pub const DATA_DIR_ENV: &str = "HTTPORTER_DATA_DIR";

/// The override folder, when `HTTPORTER_DATA_DIR` names an absolute path.
fn override_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os(DATA_DIR_ENV)?);
    dir.is_absolute().then_some(dir)
}

/// `%APPDATA%\HTTPorter`, or `<HTTPORTER_DATA_DIR>\Roaming\HTTPorter`.
///
/// If the platform reports no config folder the path is relative to the working directory.
pub fn roaming_dir() -> PathBuf {
    match override_dir() {
        Some(dir) => dir.join("Roaming").join("HTTPorter"),
        None => dirs::config_dir().unwrap_or_default().join("HTTPorter"),
    }
}

/// `%LOCALAPPDATA%\HTTPorter`, or `<HTTPORTER_DATA_DIR>\Local\HTTPorter`.
pub fn local_dir() -> PathBuf {
    match override_dir() {
        Some(dir) => dir.join("Local").join("HTTPorter"),
        None => dirs::data_local_dir().unwrap_or_default().join("HTTPorter"),
    }
}
