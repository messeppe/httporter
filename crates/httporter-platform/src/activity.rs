//! The activity log (T4): `%APPDATA%\HTTPorter\logs\activity.log`, then `activity.1.log` …
//! `activity.9.log`, each at most [`LOG_FILE_BYTES`], one JSON line per event.
#![allow(unused_variables, dead_code)]

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use httporter_core::{ActivityEvent, ActivityFilter};

/// How many files the log keeps (the current one and 9 older ones).
pub const LOG_FILES: usize = 10;
/// A log file is rotated once it reaches this size.
pub const LOG_FILE_BYTES: u64 = 5 << 20;

/// `%APPDATA%\HTTPorter\logs`.
pub fn default_log_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("HTTPorter")
        .join("logs")
}

/// The activity log, open for appending.
pub struct ActivityLog {
    dir: PathBuf,
    /// The current file and its size.
    file: Mutex<(File, u64)>,
}

impl ActivityLog {
    /// Opens (and makes) the log in `dir`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        todo!("T4")
    }

    /// Adds `e`, its path and detail redacted; rotates when the file is full. Never panics.
    pub fn append(&self, e: &ActivityEvent) {
        todo!("T4")
    }

    /// At most `limit` events that pass `f`, newest first, across the files.
    pub fn read(&self, f: &ActivityFilter, limit: usize) -> Vec<ActivityEvent> {
        todo!("T4")
    }

    /// Writes the events that pass `f` to `to` as text; how many.
    pub fn export_text(&self, f: &ActivityFilter, to: &Path) -> io::Result<u64> {
        todo!("T4")
    }

    /// The folder the log is in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
