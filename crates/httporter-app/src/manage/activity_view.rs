//! The activity log and its sheet (T9).
#![allow(unused_variables, dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use httporter_core::ActivityEvent;

use super::Manager;
use crate::AppWindow;

/// What this view remembers between callbacks (T9).
#[derive(Default)]
pub struct ActivityState {}

/// Wires this view's callbacks of `ui` to `m` (T9).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}

/// Opens the activity log in `dir` (`main.rs`, at start). T1: does nothing yet (startup path).
pub fn open_log(dir: PathBuf) {}

/// Adds `e` to the activity log; nothing without an open log. T1: does nothing yet.
pub fn log(e: ActivityEvent) {}
