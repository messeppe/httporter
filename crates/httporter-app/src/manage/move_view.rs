//! Rename, Move To…, the move warning and pre-signed links (T9).
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use super::Manager;
use crate::AppWindow;

/// What this view remembers between callbacks (T9).
#[derive(Default)]
pub struct MoveState {}

/// Wires this view's callbacks of `ui` to `m` (T9).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}
