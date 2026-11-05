//! Thumbnails for the grid (T5): images ≤ 5 MB scaled to 240 × 240 PNGs in `cache_dir("thumbs")`.
#![allow(unused_variables, dead_code)]

use std::sync::Arc;

use super::Manager;
use crate::AppWindow;

/// Wires this view's callbacks of `ui` to `m` (T5).
pub fn install(ui: &AppWindow, m: &Arc<Manager>) {}
