// SPDX-License-Identifier: MIT OR Apache-2.0

//! The toast stack (§8.7.5): at most three shown, newest at the bottom; each goes after 4 s, or
//! 8 s when it has an action, and its time stands still while the pointer is over it or it has the
//! focus (`toast.slint` keeps the time and says when it is up).
//!
//! The list is the window's `toasts` model (a `VecModel` this file puts there on the first toast),
//! and what each action does is kept here, by toast id, on the UI thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use httporter_core::PurgeResult;
use slint::{Model, ModelRc, VecModel, Weak};

use crate::binding::Link;
use crate::{AppWindow, ToastData};

/// The most toasts shown at once: a fourth lets the oldest go.
pub const MAX_SHOWN: usize = 3;

/// What a toast is about; its icon (`ToastData.kind` 0, 1, 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Done,
    Warning,
}

impl ToastKind {
    fn code(self) -> i32 {
        match self {
            Self::Info => 0,
            Self::Done => 1,
            Self::Warning => 2,
        }
    }
}

/// A toast's button ("Undo", "View Release") and what it does, run on the UI thread.
pub struct ToastAction {
    pub label: String,
    pub run: Box<dyn FnOnce()>,
}

thread_local! {
    /// What the toasts' buttons do, by toast id (UI thread).
    static ACTIONS: RefCell<HashMap<i32, Box<dyn FnOnce()>>> = RefCell::new(HashMap::new());
    /// The next toast's id.
    static NEXT_ID: Cell<i32> = const { Cell::new(1) };
}

/// Shows a toast: 4 s, 8 s with an action, three at most shown.
pub fn show(ui: &AppWindow, kind: ToastKind, text: impl Into<String>, action: Option<ToastAction>) {
    show_id(ui, kind, text, action);
}

/// [`show`], giving the toast's id (for [`dismiss`]).
pub fn show_id(
    ui: &AppWindow,
    kind: ToastKind,
    text: impl Into<String>,
    action: Option<ToastAction>,
) -> i32 {
    let id = NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id.checked_add(1).unwrap_or(1));
        id
    });
    let label = action.as_ref().map(|a| a.label.clone()).unwrap_or_default();
    let gone = with_rows(ui, |rows| {
        let mut gone = Vec::new();
        while rows.row_count() >= MAX_SHOWN {
            gone.push(rows.remove(0).id);
        }
        rows.push(ToastData {
            id,
            text: text.into().into(),
            action: label.into(),
            kind: kind.code(),
        });
        gone
    });
    ACTIONS.with(|actions| {
        let mut actions = actions.borrow_mut();
        for old in gone {
            actions.remove(&old);
        }
        if let Some(action) = action {
            actions.insert(id, action.run);
        }
    });
    id
}

/// The toast for a CDN purge's result: done when it purged, a warning when it couldn't.
pub fn purge_kind(result: &PurgeResult) -> ToastKind {
    match result {
        PurgeResult::Urls(_) | PurgeResult::Zone => ToastKind::Done,
        PurgeResult::Off => ToastKind::Info,
        PurgeResult::Failed(_) | PurgeResult::Unavailable(_) => ToastKind::Warning,
    }
}

/// [`show`] from any thread (no action): the toast shows once the event loop gets to it.
pub fn post(ui: &Weak<AppWindow>, kind: ToastKind, text: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| show(&ui, kind, text, None));
}

/// [`post`] for the end of work whose progress the status line showed ("Purging CDN… 3 of 12"):
/// the status line is cleared, and the toast says how it went.
pub fn post_after_progress(link: &Link, kind: ToastKind, text: String) {
    link.status(String::new());
    post(link.ui(), kind, text);
}

/// Takes toast `id` down (its time ran out, Esc, or what it reported was undone another way).
pub fn dismiss(ui: &AppWindow, id: i32) {
    with_rows(ui, |rows| {
        if let Some(row) = rows.iter().position(|toast| toast.id == id) {
            rows.remove(row);
        }
    });
    let action = ACTIONS.with(|actions| actions.borrow_mut().remove(&id));
    drop(action);
}

/// Toast `id`'s button was pressed: it goes, and its action runs.
pub fn act(ui: &AppWindow, id: i32) {
    let action = ACTIONS.with(|actions| actions.borrow_mut().remove(&id));
    dismiss(ui, id);
    if let Some(run) = action {
        run();
    }
}

/// Runs `f` on the window's list of toasts, putting one there first if it has none of ours.
fn with_rows<R>(ui: &AppWindow, f: impl FnOnce(&VecModel<ToastData>) -> R) -> R {
    let model = ui.get_toasts();
    if let Some(rows) = model.as_any().downcast_ref::<VecModel<ToastData>>() {
        return f(rows);
    }
    let rows = Rc::new(VecModel::default());
    ui.set_toasts(ModelRc::from(rows.clone()));
    f(&rows)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use i_slint_backend_testing as testing;
    use slint::ComponentHandle;

    use super::*;

    /// The main window, shown, with its toasts' callbacks wired as `comfort::install` wires them.
    fn window() -> AppWindow {
        thread_local! {
            static STARTED: Cell<bool> = const { Cell::new(false) };
        }
        if !STARTED.with(|started| started.replace(true)) {
            testing::init_no_event_loop();
        }
        let ui = AppWindow::new().unwrap();
        ui.on_toast_dismissed({
            let weak = ui.as_weak();
            move |id| {
                if let Some(ui) = weak.upgrade() {
                    dismiss(&ui, id);
                }
            }
        });
        ui.on_toast_action({
            let weak = ui.as_weak();
            move |id| {
                if let Some(ui) = weak.upgrade() {
                    act(&ui, id);
                }
            }
        });
        ui.show().unwrap();
        ui
    }

    fn texts(ui: &AppWindow) -> Vec<String> {
        ui.get_toasts().iter().map(|t| t.text.to_string()).collect()
    }

    /// Lets `ms` of mocked time go by, a tenth of a second at a time (the toasts' tick).
    fn wait(ms: u64) {
        for _ in 0..ms / 100 {
            testing::mock_elapsed_time(Duration::from_millis(100));
        }
    }

    fn noting(label: &str, ran: &Rc<Cell<u32>>) -> Option<ToastAction> {
        let ran = ran.clone();
        Some(ToastAction {
            label: label.to_owned(),
            run: Box::new(move || ran.set(ran.get() + 1)),
        })
    }

    #[test]
    fn at_most_three_show_and_a_fourth_lets_the_oldest_go() {
        let ui = window();
        let ran = Rc::new(Cell::new(0));
        let first = show_id(&ui, ToastKind::Info, "one", noting("Undo", &ran));
        show(&ui, ToastKind::Done, "two", None);
        show(&ui, ToastKind::Warning, "three", None);
        assert_eq!(texts(&ui), ["one", "two", "three"]);
        show(&ui, ToastKind::Info, "four", None);
        assert_eq!(texts(&ui), ["two", "three", "four"]);
        // The one that went took its action with it.
        act(&ui, first);
        assert_eq!(ran.get(), 0);
        let kinds: Vec<i32> = ui.get_toasts().iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [1, 2, 0]);
        assert_eq!(MAX_SHOWN, 3);
    }

    #[test]
    fn its_button_runs_the_action_once_and_takes_the_toast_down() {
        let ui = window();
        let ran = Rc::new(Cell::new(0));
        let id = show_id(&ui, ToastKind::Info, "Tab closed", noting("Undo", &ran));
        assert_eq!(ui.get_toasts().row_data(0).unwrap().action, "Undo");
        ui.invoke_toast_action(id);
        assert_eq!(ran.get(), 1);
        assert!(texts(&ui).is_empty());
        act(&ui, id);
        assert_eq!(ran.get(), 1, "once only");
    }

    #[test]
    fn a_toast_goes_after_4_s_and_one_with_an_action_after_8_s() {
        let ui = window();
        let ran = Rc::new(Cell::new(0));
        show(&ui, ToastKind::Info, "plain", None);
        show(&ui, ToastKind::Info, "with undo", noting("Undo", &ran));
        // Each toast's time starts once it is shown.
        wait(100);
        wait(3_800);
        assert_eq!(texts(&ui), ["plain", "with undo"]);
        wait(300);
        assert_eq!(texts(&ui), ["with undo"]);
        wait(3_600);
        assert_eq!(texts(&ui), ["with undo"]);
        wait(400);
        assert!(texts(&ui).is_empty());
        // Going by itself is not pressing its button.
        assert_eq!(ran.get(), 0);
    }
}
