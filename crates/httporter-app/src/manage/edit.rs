//! Editing a site's file with automatic re-upload (T6), and the sidebar's Editing rows.
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use super::{Manager, Side};
use crate::AppWindow;

/// The files being edited (T6).
#[derive(Default)]
pub struct Edits {}

/// Wires this view's callbacks of `ui` to `m` (T6).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}

/// See [`Manager::open_file`]. T1: does nothing yet (startup path).
pub(super) fn open_file(m: &Arc<Manager>, side: Side, row: usize) {}

/// See [`Manager::flush_edits`]. T1: does nothing yet (close path).
pub(super) fn flush_edits(m: &Manager) {}
