// SPDX-License-Identifier: MIT OR Apache-2.0

//! Undo for local UI actions (§8.7.7): a closed tab, a removed favorite. Remote deletes are not
//! undoable, and their alert says so.
//!
//! Each action is kept (the last 20, on the UI thread) and reported in a toast with "Undo". Ctrl+Z
//! (or the palette's Undo) undoes the newest; a toast's Undo undoes the one it reports; Ctrl+Shift+T
//! reopens the newest closed tab. An action undone goes from the list, and its toast with it.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;

use httporter_core::{Favorite, TabState};
use slint::ComponentHandle;

use super::copy;
use super::toast::{self, ToastAction, ToastKind};
use crate::AppWindow;
use crate::manage::{self, Manager};

/// How many actions are kept to undo.
pub const KEEP: usize = 20;

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

/// An action kept to undo, and the toast that reports it.
struct Kept {
    key: u64,
    action: UndoAction,
    toast: i32,
}

thread_local! {
    /// The actions to undo, oldest first (UI thread).
    static KEPT: RefCell<VecDeque<Kept>> = const { RefCell::new(VecDeque::new()) };
    static NEXT_KEY: Cell<u64> = const { Cell::new(1) };
}

/// Keeps `action` (the last 20), and shows a toast with "Undo".
pub fn push(ui: &AppWindow, action: UndoAction) {
    let key = NEXT_KEY.with(|next| {
        let key = next.get();
        next.set(key + 1);
        key
    });
    let weak = ui.as_weak();
    // The toast's Undo undoes this action, wherever it is in the list by then.
    let run = Box::new(move || {
        if let (Some(ui), Some(m)) = (weak.upgrade(), manage::manager()) {
            undo_where(&ui, &m, |kept| kept.key == key);
        }
    });
    let toast = toast::show_id(
        ui,
        ToastKind::Info,
        done_text(&action),
        Some(ToastAction {
            label: copy::UNDO.to_owned(),
            run,
        }),
    );
    KEPT.with(|kept| {
        let mut kept = kept.borrow_mut();
        kept.push_back(Kept { key, action, toast });
        while kept.len() > KEEP {
            kept.pop_front();
        }
    });
}

/// Undoes the newest action; false when there is none.
pub fn undo(ui: &AppWindow, m: &Arc<Manager>) -> bool {
    undo_where(ui, m, |_| true)
}

/// Reopens the newest closed tab (Ctrl+Shift+T); false when no closed tab is kept.
pub fn reopen_tab(ui: &AppWindow, m: &Arc<Manager>) -> bool {
    undo_where(ui, m, |kept| {
        matches!(kept.action, UndoAction::ReopenTab { .. })
    })
}

/// What Undo would do now, for the palette; `None` when there is nothing to undo.
pub fn next_undo() -> Option<String> {
    KEPT.with(|kept| kept.borrow().back().map(|kept| undo_text(&kept.action)))
}

/// Whether a closed tab is kept to reopen.
pub fn has_closed_tab() -> bool {
    KEPT.with(|kept| {
        kept.borrow()
            .iter()
            .any(|kept| matches!(kept.action, UndoAction::ReopenTab { .. }))
    })
}

/// Undoes the newest kept action that `which` picks: it goes from the list, its toast goes, and
/// the tab or the favorite comes back. False when none is picked.
fn undo_where(ui: &AppWindow, m: &Arc<Manager>, which: impl Fn(&Kept) -> bool) -> bool {
    let picked = KEPT.with(|kept| {
        let mut kept = kept.borrow_mut();
        let at = kept.iter().rposition(&which)?;
        kept.remove(at)
    });
    let Some(kept) = picked else {
        return false;
    };
    toast::dismiss(ui, kept.toast);
    match kept.action {
        UndoAction::ReopenTab { tab, index } => manage::tabs::reopen(ui, m, tab, index),
        UndoAction::RestoreFavorite(favorite) => manage::places::restore(m, favorite),
    }
    true
}

/// The toast that reports `action`.
fn done_text(action: &UndoAction) -> String {
    match action {
        UndoAction::ReopenTab { tab, .. } => copy::tab_closed(&tab_title(tab)),
        UndoAction::RestoreFavorite(favorite) => copy::favorite_removed(&favorite.name),
    }
}

/// What undoing `action` does, as the palette says it.
fn undo_text(action: &UndoAction) -> String {
    match action {
        UndoAction::ReopenTab { tab, .. } => copy::undo_reopen_tab(&tab_title(tab)),
        UndoAction::RestoreFavorite(favorite) => copy::undo_restore_favorite(&favorite.name),
    }
}

/// A tab's title, as the strip shows it.
fn tab_title(tab: &TabState) -> String {
    manage::manager()
        .map(|m| manage::tabs::title(&m, tab))
        .unwrap_or_default()
}

/// The actions kept, oldest first (the tests look at the list).
#[cfg(test)]
pub(crate) fn kept() -> Vec<UndoAction> {
    KEPT.with(|kept| kept.borrow().iter().map(|k| k.action.clone()).collect())
}

#[cfg(test)]
mod tests {
    use httporter_core::PlaceKind;
    use slint::Model;

    use super::*;

    fn window() -> AppWindow {
        thread_local! {
            static STARTED: Cell<bool> = const { Cell::new(false) };
        }
        if !STARTED.with(|started| started.replace(true)) {
            i_slint_backend_testing::init_no_event_loop();
        }
        AppWindow::new().unwrap()
    }

    fn favorite(name: &str) -> Favorite {
        Favorite {
            id: 7,
            kind: PlaceKind::Local,
            name: name.to_owned(),
            local: Some(std::path::PathBuf::from("D:\\site")),
            site: None,
            remote: None,
        }
    }

    fn names(actions: &[UndoAction]) -> Vec<String> {
        actions
            .iter()
            .map(|action| match action {
                UndoAction::RestoreFavorite(f) => f.name.clone(),
                UndoAction::ReopenTab { index, .. } => format!("tab {index}"),
            })
            .collect()
    }

    #[test]
    fn the_last_twenty_actions_are_kept() {
        let ui = window();
        for i in 0..25 {
            push(&ui, UndoAction::RestoreFavorite(favorite(&format!("f{i}"))));
        }
        let kept = kept();
        assert_eq!(kept.len(), KEEP);
        assert_eq!(KEEP, 20);
        let expected: Vec<String> = (5..25).map(|i| format!("f{i}")).collect();
        assert_eq!(names(&kept), expected);
        // Undo names the newest; the toasts stay three.
        assert_eq!(next_undo(), Some(copy::undo_restore_favorite("f24")));
        assert_eq!(ui.get_toasts().row_count(), toast::MAX_SHOWN);
        assert!(!has_closed_tab());
    }

    #[test]
    fn a_closed_tab_and_a_removed_favorite_each_have_their_toast() {
        let ui = window();
        assert_eq!(next_undo(), None);
        push(&ui, UndoAction::RestoreFavorite(favorite("Uploads")));
        let toasts = ui.get_toasts();
        let shown = toasts.row_data(0).unwrap();
        assert_eq!(shown.text, copy::favorite_removed("Uploads"));
        assert_eq!(shown.action, copy::UNDO);
        push(
            &ui,
            UndoAction::ReopenTab {
                tab: TabState::default(),
                index: 2,
            },
        );
        assert!(has_closed_tab());
        // Without a manager a tab has no title.
        assert_eq!(next_undo(), Some(copy::undo_reopen_tab("")));
        assert_eq!(
            ui.get_toasts().row_data(1).unwrap().text,
            copy::tab_closed("")
        );
        assert_eq!(names(&kept()), ["Uploads", "tab 2"]);
    }
}
