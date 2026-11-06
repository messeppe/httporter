// SPDX-License-Identifier: MIT OR Apache-2.0

//! The tray (§8.7.8): the window closed while transfers run goes into the notification area, and
//! the transfers carry on.
//!
//! - The quit alert's Keep Running hides the window ([`hide_into_tray`]) when Settings › General ›
//!   "Keep running in the tray…" is on (`UiState.tray`); otherwise the window stays, as before.
//! - The icon's tooltip says how far the transfers are (from the engine's snapshot, at most once a
//!   second); its menu shows the window, pauses or resumes everything, or quits.
//! - A left click, or Show HTTPorter, brings the window back, and the icon goes.
//! - Quit brings the window back with its quit alert: the same choices as closing it, the journal
//!   untouched.
//! - When the last transfer ends with the window still hidden: the finish notification, then the
//!   icon goes, then the event loop ends (`main` closes as after any quit).

use std::cell::RefCell;
use std::time::Duration;

use httporter_core::{EngineSnapshot, EngineState};
use httporter_platform::win::{self, Tray, TrayEvent};
use slint::{ComponentHandle, Timer, TimerMode};

use super::copy;
use crate::AppWindow;
use crate::manage::{self, Manager};

/// How often the tooltip follows the transfers.
const EVERY: Duration = Duration::from_secs(1);

/// The window is in the tray.
struct InTray {
    tray: Tray,
    /// Brings the tooltip up to date, and sees the queue empty.
    timer: Timer,
    /// The menu says Resume All.
    paused: bool,
    /// The queue has emptied: the notification is on its way, then the event loop ends.
    finishing: bool,
}

thread_local! {
    /// The tray while the window is in it (UI thread).
    static TRAY: RefCell<Option<InTray>> = const { RefCell::new(None) };
}

/// Whether the window is in the tray now.
pub fn in_tray() -> bool {
    TRAY.with(|tray| tray.borrow().is_some())
}

/// Keep Running on the quit alert: with the tray setting on, the window hides and the tray icon
/// shows. False (and the window stays) when the setting is off, or the icon can't be shown.
pub fn hide_into_tray(ui: &AppWindow) -> bool {
    let Some(m) = manage::manager() else {
        return false;
    };
    if in_tray() || !m.session.ui_state().tray {
        return false;
    }
    let snapshot = m.transfers.engine().snapshot();
    let paused = is_paused(&snapshot);
    let on_event: Box<dyn Fn(TrayEvent)> = Box::new(|event| {
        // After the icon's window procedure has returned, never inside it.
        let _ = slint::invoke_from_event_loop(move || tray_event(event));
    });
    let Ok(tray) = Tray::new(
        &copy::tray_tooltip(&snapshot),
        copy::tray_labels(paused),
        on_event,
    ) else {
        return false;
    };
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, EVERY, tick);
    TRAY.with(|slot| {
        *slot.borrow_mut() = Some(InTray {
            tray,
            timer,
            paused,
            finishing: false,
        });
    });
    let _ = ui.hide();
    true
}

/// Brings the window back from the tray (the icon goes), if it is there.
pub fn show_from_tray(ui: &AppWindow) {
    if leave_tray() {
        let _ = ui.show();
        bring_to_front(ui);
    }
}

/// The icon goes and its timer stops; whether the window was in the tray.
fn leave_tray() -> bool {
    let left = TRAY.with(|slot| slot.borrow_mut().take());
    // Dropped outside the borrow: deleting the icon destroys its window.
    left.is_some()
}

/// A click on the icon or a pick from its menu, on the UI thread.
fn tray_event(event: TrayEvent) {
    let Some(m) = manage::manager() else {
        return;
    };
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    match event {
        TrayEvent::Show => show_from_tray(&ui),
        TrayEvent::PauseResume => {
            let engine = m.transfers.engine();
            let paused = is_paused(&engine.snapshot());
            if paused {
                engine.resume_all();
            } else {
                engine.pause_all();
            }
            TRAY.with(|slot| {
                if let Some(in_tray) = slot.borrow_mut().as_mut() {
                    in_tray.paused = !paused;
                    in_tray.tray.set_labels(copy::tray_labels(!paused));
                }
            });
        }
        TrayEvent::Quit => {
            show_from_tray(&ui);
            // The window's own closing: the quit alert while transfers run, as from its Close.
            ui.window()
                .dispatch_event(slint::platform::WindowEvent::CloseRequested);
        }
    }
}

/// Once a second while the window is in the tray: the tooltip and the menu follow the engine, and
/// an empty queue ends the app.
fn tick() {
    let Some(m) = manage::manager() else {
        return;
    };
    let Some(ui) = m.link.ui().upgrade() else {
        return;
    };
    if ui.window().is_visible() {
        // Shown some other way meanwhile: the icon has nothing left to do.
        leave_tray();
        return;
    }
    let engine = m.transfers.engine();
    if !engine.has_work() {
        finish(&ui, &m);
        return;
    }
    let snapshot = engine.snapshot();
    let paused = is_paused(&snapshot);
    TRAY.with(|slot| {
        if let Some(in_tray) = slot.borrow_mut().as_mut() {
            in_tray.tray.set_tooltip(&copy::tray_tooltip(&snapshot));
            if in_tray.paused != paused {
                in_tray.paused = paused;
                in_tray.tray.set_labels(copy::tray_labels(paused));
            }
        }
    });
}

/// The queue emptied with the window in the tray: the finish notification (the one a run that
/// ends in the background gets), then the icon goes, then the event loop ends. Showing the window
/// meanwhile stops it.
fn finish(ui: &AppWindow, m: &Manager) {
    let started = TRAY.with(|slot| match slot.borrow_mut().as_mut() {
        Some(in_tray) if !in_tray.finishing => {
            in_tray.finishing = true;
            in_tray.timer.stop();
            true
        }
        _ => false,
    });
    if !started {
        return;
    }
    let note = finished_note(&m.transfers.engine().snapshot());
    let hwnd = window_handle(ui).unwrap_or(0);
    // A notification is WinRT and COM work: off the UI thread, and the loop ends after it.
    m.link.rt().spawn_blocking(move || {
        if let Some((title, body)) = note {
            win::notify_or_flash(hwnd, &title, &body);
        }
        let _ = slint::invoke_from_event_loop(|| {
            let finishing = TRAY.with(|slot| slot.borrow().as_ref().is_some_and(|t| t.finishing));
            if finishing {
                leave_tray();
                let _ = slint::quit_event_loop();
            }
        });
    });
}

/// The finish notification's title and text, when anything was transferred.
fn finished_note(s: &EngineSnapshot) -> Option<(String, String)> {
    if s.files_done + s.files_failed == 0 {
        return None;
    }
    let site = s
        .last_batch
        .as_ref()
        .map_or_else(|| s.site.clone(), |batch| batch.site.clone());
    Some(crate::copy::finished_notification(
        s.direction,
        s.files_done,
        s.files_failed,
        &site,
    ))
}

fn is_paused(s: &EngineSnapshot) -> bool {
    matches!(s.state, EngineState::Paused)
}

/// Puts the window in front (a click on the tray icon lets the app take the foreground).
fn bring_to_front(ui: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    ui.window()
        .with_winit_window(|window| window.focus_window());
}

/// The window's handle (an `HWND`), once it has one.
fn window_handle(ui: &AppWindow) -> Option<isize> {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    ui.window()
        .with_winit_window(|window| match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
            _ => None,
        })
        .flatten()
}
