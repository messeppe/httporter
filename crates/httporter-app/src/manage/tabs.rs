//! Tabs and synchronized browsing (T7).
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use super::{Manager, Side};
use crate::AppWindow;

/// What this view remembers between callbacks (T7).
#[derive(Default)]
pub struct TabsState {}

/// Wires this view's callbacks of `ui` to `m` (T7).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}

/// A pane of `ui` was put on screen (`binding.rs` calls it after `apply_local` / `apply_remote`):
/// the tab's title, synchronized browsing. T1: does nothing yet (startup path).
pub fn navigated(ui: &AppWindow, side: Side) {}
