// SPDX-License-Identifier: MIT OR Apache-2.0

//! The watch folder (§8.8.3, I6, T4): while HTTPorter is open, a profile with
//! `ProfileExtras::watch` publishes its folder's changes on its own, 10 s after the last one,
//! through the headless publish (`headless::run_headless`, `HistorySource::Watch`).
//!
//! - One `win::DirWatcher` per watched profile, started at start and when the option is turned on;
//!   changes are filtered by the skip rules.
//! - Deletes wait for a review unless `unattended_deletes`: the profile page then says "N files
//!   were removed from the folder", with Review… (the Publish sheet).
//! - One run at a time per profile (the profile's lock); a change during a run queues one more.
//! - Watching stops, with a note on the profile page and in the activity log, when the folder is
//!   gone or renamed, when the watcher overflows twice in a row, or when the site is read-only.
//!
//! T1 is the skeleton: nothing is watched yet.

use std::sync::Arc;

use super::Publisher;
use crate::AppWindow;

/// Starts watching the profiles that ask for it, and wires the profile page's watch note (T4).
/// Nothing yet.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    let _ = (ui, p);
}
