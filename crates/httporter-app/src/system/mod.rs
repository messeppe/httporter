//! System (I5, T4): Settings › Security (the saved passwords and keys), Reset HTTPorter (§11), the
//! opt-in update check (§16) and About.
//!
//! - this file: [`install`] for the main window, [`wire_settings`] for the Settings window;
//! - [`reset`]: the reset, run at the next start;
//! - [`update`]: the release feed;
//! - [`copy`]: the words of all of the above.

mod copy;
pub mod reset;
pub mod update;

use std::sync::Arc;

use crate::manage::Manager;
use crate::transfers::Transfers;
use crate::{AppWindow, SettingsWindow};

/// Wires the update check at start and the palette's About and Check for Updates. Runs once at
/// start, after the other installs.
pub fn install(ui: &AppWindow, m: &Arc<Manager>, transfers: &Arc<Transfers>) {
    // T4: the update check at start (only when opted in, a release repository is set and the
    // last check is more than 24 h old).
    let _ = (ui, m, transfers);
}

/// Wires Settings › Security and General's tray, update and About rows. Runs each time the
/// window is made, on the first press of the gear.
pub fn wire_settings(window: &SettingsWindow) {
    window.on_remove_unused_keys(|| todo!("T4"));
    window.on_open_credential_manager(|| todo!("T4"));
    window.on_reset_requested(|| todo!("T4"));
    window.on_reset_confirmed(|| todo!("T4"));
    window.on_check_now(|| todo!("T4"));
    window.on_open_licenses(|| todo!("T4"));
}
