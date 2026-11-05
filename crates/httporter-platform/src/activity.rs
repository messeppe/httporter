//! The activity log: `%APPDATA%\HTTPorter\logs\activity.log`, then `activity.1.log` …
//! `activity.9.log`, each at most [`LOG_FILE_BYTES`], one JSON line per event.
//!
//! Every event's path and detail are redacted ([`redact`]) before they are written, so a signed
//! URL or a header that slipped into an error text never reaches the disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use httporter_core::{ActivityEvent, ActivityFilter, redact};

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
    /// The current file (none after a rotation that could not open the next one) and its size.
    file: Mutex<(Option<File>, u64)>,
}

impl ActivityLog {
    /// Opens (and makes) the log in `dir`.
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let file = open_current(dir)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir: dir.to_path_buf(),
            file: Mutex::new((Some(file), size)),
        })
    }

    /// Adds `e`, its path and detail redacted; rotates when the file is full. Never panics: an
    /// event that can't be written is lost.
    pub fn append(&self, e: &ActivityEvent) {
        let event = ActivityEvent {
            path: redact(&e.path),
            detail: redact(&e.detail),
            ..e.clone()
        };
        let Ok(mut line) = serde_json::to_string(&event) else {
            return;
        };
        line.push('\n');
        let mut guard = self.lock();
        let (file, size) = &mut *guard;
        if *size > 0 && *size + line.len() as u64 > LOG_FILE_BYTES {
            // Closed first: Windows won't rename an open file.
            *file = None;
            rotate(&self.dir);
            *size = 0;
        }
        if file.is_none() {
            *file = open_current(&self.dir).ok();
            *size = file
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .map_or(0, |m| m.len());
        }
        if let Some(f) = file
            && f.write_all(line.as_bytes()).is_ok()
        {
            *size += line.len() as u64;
        }
    }

    /// At most `limit` events that pass `f`, newest first, across the files.
    pub fn read(&self, f: &ActivityFilter, limit: usize) -> Vec<ActivityEvent> {
        let mut found = Vec::new();
        if limit == 0 {
            return found;
        }
        // Held while reading, so that a rotation can't move a file away halfway.
        let _guard = self.lock();
        for index in 0..LOG_FILES {
            let Ok(text) = fs::read_to_string(file_path(&self.dir, index)) else {
                continue;
            };
            for line in text.lines().rev() {
                let Ok(event) = serde_json::from_str::<ActivityEvent>(line) else {
                    continue;
                };
                if f.matches(&event) {
                    found.push(event);
                    if found.len() >= limit {
                        return found;
                    }
                }
            }
        }
        found
    }

    /// Writes the events that pass `f` to `to` as text, oldest first, one per line with tabs
    /// between the fields; how many.
    pub fn export_text(&self, f: &ActivityFilter, to: &Path) -> io::Result<u64> {
        let mut events = self.read(f, usize::MAX);
        events.reverse();
        let mut out = BufWriter::new(File::create(to)?);
        for e in &events {
            let kind = serde_json::to_value(e.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let at = e.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let ok = if e.ok { "ok" } else { "failed" };
            writeln!(
                out,
                "{at}\t{kind}\t{}\t{ok}\t{}\t{}\t{}",
                one_line(&e.site),
                e.bytes,
                one_line(&e.path),
                one_line(&e.detail)
            )?;
        }
        out.flush()?;
        Ok(events.len() as u64)
    }

    /// The folder the log is in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> MutexGuard<'_, (Option<File>, u64)> {
        self.file.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// `activity.log` for 0, else `activity.<index>.log`.
fn file_path(dir: &Path, index: usize) -> PathBuf {
    if index == 0 {
        dir.join("activity.log")
    } else {
        dir.join(format!("activity.{index}.log"))
    }
}

fn open_current(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(file_path(dir, 0))
}

/// Drops the oldest file and moves each of the others one up. Failures are ignored: at worst a
/// file is overwritten sooner than planned.
fn rotate(dir: &Path) {
    let _ = fs::remove_file(file_path(dir, LOG_FILES - 1));
    for index in (0..LOG_FILES - 1).rev() {
        let from = file_path(dir, index);
        if from.exists() {
            let _ = fs::rename(&from, file_path(dir, index + 1));
        }
    }
}

/// `text` with its line breaks and tabs as spaces, for the exported text.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ")
}
