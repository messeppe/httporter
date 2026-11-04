//! Compare in the main window (T6): the toolbar toggle, the Status column, the "Comparing" chip
//! and the status bar's summary.

use std::sync::Arc;

use super::Publisher;
use crate::AppWindow;

/// What the compare remembers between callbacks.
#[derive(Default)]
pub(super) struct CompareState {}

/// Wires the compare callbacks. Runs at startup: must not start anything.
pub fn install(ui: &AppWindow, p: &Arc<Publisher>) {
    // T6 fills this.
}
