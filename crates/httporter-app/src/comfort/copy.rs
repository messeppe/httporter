// SPDX-License-Identifier: MIT OR Apache-2.0

//! The words of the comfort features (I5, T3): the tray icon's tooltip and menu, the toasts and
//! undo, and the palette's rows for them. Everything they show that Rust builds.

use httporter_core::{Direction, EngineSnapshot, EngineState};

use crate::format::format_count;

// ---- The tray icon (§8.7.8) ----

/// The tray menu's labels: Show HTTPorter, Pause All or Resume All (while paused), Quit.
pub fn tray_labels(paused: bool) -> [&'static str; 3] {
    [
        "Show HTTPorter",
        if paused { "Resume All" } else { "Pause All" },
        "Quit",
    ]
}

/// The tray icon's tooltip: "HTTPorter: Uploading 1,204 of 13,793 · 43 files/s".
pub fn tray_tooltip(s: &EngineSnapshot) -> String {
    let done = format_count(s.files_done);
    let total = format_count(s.files_total);
    let what = match &s.state {
        EngineState::Idle => return "HTTPorter".to_owned(),
        EngineState::Running => {
            let doing = match s.direction {
                Direction::Upload => "Uploading",
                Direction::Download => "Downloading",
                Direction::Delete => "Deleting",
                Direction::Mixed => "Transferring",
            };
            match files_rate(s.files_per_s) {
                Some(rate) => format!("{doing} {done} of {total} \u{b7} {rate} files/s"),
                None => format!("{doing} {done} of {total}"),
            }
        }
        EngineState::Paused => format!("Paused \u{b7} {done} of {total} done"),
        EngineState::Offline { .. } => {
            format!("Offline, goes on when the connection is back \u{b7} {done} of {total} done")
        }
        EngineState::AuthStopped { .. }
        | EngineState::ClockSkew { .. }
        | EngineState::DiskFull { .. } => {
            format!("Stopped \u{b7} {done} of {total} done \u{b7} open HTTPorter to see why")
        }
    };
    format!("HTTPorter: {what}")
}

/// Files a second as the tooltip shows it: whole above 10, one decimal below; none at 0.
fn files_rate(per_s: f64) -> Option<String> {
    if !per_s.is_finite() || per_s < 0.05 {
        None
    } else if per_s >= 10.0 {
        Some(format_count(per_s.round() as u64))
    } else {
        Some(format!("{per_s:.1}"))
    }
}

// ---- Undo (§8.7.7) ----

/// The toasts' button that undoes what they report.
pub const UNDO: &str = "Undo";

/// The toast after a tab was closed.
pub fn tab_closed(title: &str) -> String {
    format!("Closed the tab \u{201c}{title}\u{201d}")
}

/// The toast after a favorite was removed.
pub fn favorite_removed(name: &str) -> String {
    format!("Removed \u{201c}{name}\u{201d} from Favorites")
}

/// What Undo would do, as the palette's Undo row says it.
pub fn undo_reopen_tab(title: &str) -> String {
    format!("Reopen the tab \u{201c}{title}\u{201d}")
}

pub fn undo_restore_favorite(name: &str) -> String {
    format!("Put \u{201c}{name}\u{201d} back in Favorites")
}

// ---- The palette ----

pub const PALETTE_UNDO: &str = "Undo";
pub const PALETTE_REOPEN_TAB: &str = "Reopen Closed Tab";
pub const PALETTE_NOTHING_TO_UNDO: &str = "Nothing to undo";
pub const PALETTE_NO_CLOSED_TAB: &str = "No tab has been closed";
pub const KEYS_UNDO: &str = "Ctrl Z";
pub const KEYS_REOPEN_TAB: &str = "Ctrl Shift T";

/// The shortcuts I5 adds to the palette's `?` list (after those of `manage/copy.rs`): what, and
/// the keys.
pub const SHORTCUTS: [(&str, &str); 5] = [
    ("Undo", KEYS_UNDO),
    ("Reopen closed tab", KEYS_REOPEN_TAB),
    ("Settings", "Ctrl ,"),
    ("Enclosing folder", "Alt \u{2191}"),
    ("Switch pane", "F6"),
];
