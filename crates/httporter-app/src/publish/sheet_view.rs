//! The Publish sheet, Review Deletions, running a publish and the sidebar's profiles (T7).

use std::sync::Arc;

use super::Publisher;
use crate::AppWindow;

/// What the sheet remembers between callbacks.
#[derive(Default)]
pub(super) struct SheetState {}

/// Wires the Publish sheet, CDN key sheet, delete-alert purge and sidebar callbacks. Runs at
/// startup: must not start anything.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    // T7 fills this.
}
