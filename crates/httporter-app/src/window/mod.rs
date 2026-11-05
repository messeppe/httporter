//! The window chrome (I5, T2): the toolbar row is the main window's title bar (caption buttons,
//! drag, double-click, the system menu and Snap Layouts, on `win::TitleBar`), the app icon on the
//! window, and the window's place remembered between runs (`UiState.window`).
//!
//! - this file: [`install`] for the main window, [`wire_settings`] for the Settings window;
//! - [`copy`]: the words of the above.

mod copy;

use std::sync::Arc;

use slint::ComponentHandle;

use crate::session::Session;
use crate::{AppWindow, SettingsWindow};

/// Wires the main window's chrome: the title bar, the icon, and the placement (restored before
/// the window shows, saved when it closes). Runs once at start, before the window is shown.
pub fn install(ui: &AppWindow, session: &Arc<Session>) {
    // T2: the title bar and icon (on the HWND once it exists) and the placement restore.
    let _ = session;
    ui.on_caption_minimize(|| todo!("T2"));
    ui.on_caption_toggle_maximize(|| todo!("T2"));
    ui.on_caption_close({
        let weak = ui.as_weak();
        move || {
            // The same as the system's Close: the window's close handling decides.
            if let Some(ui) = weak.upgrade() {
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::CloseRequested);
            }
        }
    });
    ui.on_caption_drag(|| todo!("T2"));
    ui.on_caption_menu(|_, _| todo!("T2"));
    // Raised as the layout settles at start too: never a stub that panics.
    ui.on_caption_layout_changed(|| {});
}

/// Wires the Settings window's title row (Close only). Runs each time the window is made, on
/// the first press of the gear.
pub fn wire_settings(window: &SettingsWindow) {
    window.on_settings_caption_close({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                window
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::CloseRequested);
            }
        }
    });
    window.on_settings_caption_drag(|| todo!("T2"));
    window.on_settings_caption_menu(|_, _| todo!("T2"));
}
