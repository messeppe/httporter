//! The toast stack (§8.7.5): at most three shown, newest at the bottom; each goes after 4 s, or
//! 8 s when it has an action. The app keeps the list (`AppWindow.toasts`) and the actions.
#![allow(dead_code)] // T3 calls these.

use crate::AppWindow;

/// What a toast is about; its icon (`ToastData.kind` 0, 1, 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Done,
    Warning,
}

/// A toast's button ("Undo", "View Release") and what it does, run on the UI thread.
pub struct ToastAction {
    pub label: String,
    pub run: Box<dyn FnOnce()>,
}

/// Shows a toast: 4 s, 8 s with an action, three at most shown.
pub fn show(ui: &AppWindow, kind: ToastKind, text: impl Into<String>, action: Option<ToastAction>) {
    let _ = (ui, kind, text.into(), action);
    todo!("T3")
}
