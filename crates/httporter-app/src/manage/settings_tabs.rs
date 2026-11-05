//! The Settings tabs General, Sites and Advanced, and read-only sites (T8).
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use httporter_core::SiteId;

use super::Manager;
use crate::{AppWindow, SettingsWindow};

/// What this view remembers between callbacks (T8).
#[derive(Default)]
pub struct SettingsState {}

/// Wires this view's callbacks of `ui` to `m` (T8).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}

/// Wires the General, Sites and Advanced tabs of a new Settings window (`transfers.rs` calls it
/// from `settings_window()`). T1: does nothing yet.
pub fn wire(window: &SettingsWindow) {}

/// See [`Manager::read_only`]. T1: no site is read-only yet (startup path).
pub(super) fn read_only(m: &Manager, site: &SiteId) -> bool {
    false
}
