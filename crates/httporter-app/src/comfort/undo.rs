//! Undo for local UI actions (§8.7.7): a closed tab, a removed favorite. Remote deletes are not
//! undoable, and their alert says so.
#![allow(dead_code)] // T3 calls these.

use std::sync::Arc;

use httporter_core::{Favorite, TabState};

use crate::AppWindow;
use crate::manage::Manager;

/// One local action that can be undone.
#[derive(Debug, Clone)]
pub enum UndoAction {
    /// A closed tab, and where it was in the strip.
    ReopenTab {
        tab: TabState,
        index: usize,
    },
    RestoreFavorite(Favorite),
}

/// Keeps `action` (the last 20), and shows a toast with "Undo".
pub fn push(ui: &AppWindow, action: UndoAction) {
    let _ = (ui, action);
    todo!("T3")
}

/// Undoes the newest action; false when there is none.
pub fn undo(ui: &AppWindow, m: &Arc<Manager>) -> bool {
    let _ = (ui, m);
    todo!("T3")
}
