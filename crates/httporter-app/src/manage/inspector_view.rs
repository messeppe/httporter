//! The inspector beside the single pane (T5): preview, metadata, links and actions.
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use super::Manager;
use crate::AppWindow;

/// What this view remembers between callbacks (T5).
#[derive(Default)]
pub struct InspectorState {}

/// Wires this view's callbacks of `ui` to `m` (T5).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}
