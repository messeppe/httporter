//! The command palette (T7): Ctrl+K and `?`.
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use super::Manager;
use crate::AppWindow;

/// What this view remembers between callbacks (T7).
#[derive(Default)]
pub struct PaletteState {}

/// Wires this view's callbacks of `ui` to `m` (T7).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}
