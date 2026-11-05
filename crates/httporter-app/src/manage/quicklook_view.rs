//! Quick Look (T6): text with highlighting, images, "Changes vs Bunny".
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use super::Manager;
use crate::AppWindow;

/// What this view remembers between callbacks (T6).
#[derive(Default)]
pub struct QuickLookState {}

/// Wires this view's callbacks of `ui` to `m` (T6).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}
