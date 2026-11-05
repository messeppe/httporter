//! The window chrome (I5, T2): the toolbar row is the main window's title bar (caption buttons,
//! drag, double-click, the system menu and Snap Layouts, on `win::TitleBar`), the app icon on the
//! window, and the window's place remembered between runs (`UiState.window`).
//!
//! - this file: [`install`] for the main window, [`wire_settings`] for the Settings window;
//! - [`copy`]: the words of the above.
//!
//! winit makes a window's HWND only once the event loop runs. So before the window shows,
//! [`install`] asks `win::prepare_window` to hook the window while Windows creates it (it is
//! never drawn with the system caption) and to put it where it was left; once the window exists,
//! the `TitleBar` is attached and its events reach the Slint properties. The window's place is
//! saved whenever a move, resize, maximize or restore is over.

mod copy;

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use httporter_core::WindowPlacement;
use httporter_platform::win::{self, CaptionEvent, CaptionRect, TitleBar};
use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;
use slint::winit_030::winit::window::WindowButtons;

use crate::session::Session;
use crate::{AppWindow, SettingsWindow};

/// The main window's content size the first time, in logical pixels (`app.slint`'s
/// `preferred-width` and `preferred-height`).
const FIRST_SIZE: (u32, u32) = (1280, 800);

thread_local! {
    /// The main window's chrome and the session it saves its place in, for [`save_placement`].
    static MAIN: RefCell<Option<(Rc<Chrome>, Arc<Session>)>> = const { RefCell::new(None) };
}

/// One window's chrome, once its HWND is known.
#[derive(Default)]
struct Chrome {
    hwnd: Cell<isize>,
    bar: RefCell<Option<TitleBar>>,
    /// The placement last saved or restored: a move that changes nothing writes nothing.
    saved: Cell<Option<WindowPlacement>>,
}

impl Chrome {
    fn hwnd(&self) -> Option<isize> {
        Some(self.hwnd.get()).filter(|hwnd| *hwnd != 0)
    }
}

/// Wires the main window's chrome: the title bar, the icon, and the placement (restored before
/// the window shows, saved when it closes). Runs once at start, before the window is shown.
pub fn install(ui: &AppWindow, session: &Arc<Session>) {
    let chrome = Rc::new(Chrome::default());
    MAIN.with(|main| *main.borrow_mut() = Some((chrome.clone(), session.clone())));
    wire_main(ui, &chrome);
    // The tests' backend has no HWND to hook.
    if backed_by_winit(ui.window()) {
        restore_placement(ui, session, &chrome);
        attach_main(ui, session, &chrome);
    }
}

/// Saves where the main window is, if that changed since it was last saved. The window saves its
/// place whenever a move, resize, maximize or restore is over; `main` calls this once more after
/// the event loop, while the window still exists, whichever way the app is ending.
pub fn save_placement() {
    if let Some((chrome, session)) = MAIN.with(|main| main.borrow().clone()) {
        save_placement_of(&chrome, &session);
    }
}

/// Wires the Settings window's title row (Close only). Runs each time the window is made, on
/// the first press of the gear.
pub fn wire_settings(window: &SettingsWindow) {
    let chrome = Rc::new(Chrome::default());
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
    window.on_settings_caption_drag({
        let chrome = chrome.clone();
        move || {
            if let Some(hwnd) = chrome.hwnd() {
                win::start_drag(hwnd);
            }
        }
    });
    window.on_settings_caption_menu({
        let (weak, chrome) = (window.as_weak(), chrome.clone());
        move |x, y| {
            if let (Some(window), Some(hwnd)) = (weak.upgrade(), chrome.hwnd()) {
                show_menu(window.window(), hwnd, x, y);
            }
        }
    });
    if backed_by_winit(window.window()) {
        attach_settings(window, &chrome);
    }
}

/// The main window's caption callbacks. Before the HWND is known (or with another backend) the
/// buttons still work, through Slint.
fn wire_main(ui: &AppWindow, chrome: &Rc<Chrome>) {
    ui.on_caption_minimize({
        let (weak, chrome) = (ui.as_weak(), chrome.clone());
        move || match chrome.hwnd() {
            Some(hwnd) => win::minimize(hwnd),
            None => {
                if let Some(ui) = weak.upgrade() {
                    ui.window().set_minimized(true);
                }
            }
        }
    });
    ui.on_caption_toggle_maximize({
        let (weak, chrome) = (ui.as_weak(), chrome.clone());
        move || match chrome.hwnd() {
            Some(hwnd) => win::toggle_maximize(hwnd),
            None => {
                if let Some(ui) = weak.upgrade() {
                    let window = ui.window();
                    window.set_maximized(!window.is_maximized());
                }
            }
        }
    });
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
    ui.on_caption_drag({
        let chrome = chrome.clone();
        move || {
            if let Some(hwnd) = chrome.hwnd() {
                win::start_drag(hwnd);
            }
        }
    });
    ui.on_caption_menu({
        let (weak, chrome) = (ui.as_weak(), chrome.clone());
        move |x, y| {
            if let (Some(ui), Some(hwnd)) = (weak.upgrade(), chrome.hwnd()) {
                show_menu(ui.window(), hwnd, x, y);
            }
        }
    });
    // Raised as the layout settles at start too, before the title bar exists: then it does
    // nothing, and the title bar gets the rectangle when it is attached.
    ui.on_caption_layout_changed({
        let (weak, chrome) = (ui.as_weak(), chrome.clone());
        move || {
            if let Some(ui) = weak.upgrade() {
                push_max_rect(&ui, &chrome);
            }
        }
    });
}

/// Puts the window where it was left (`UiState.window`) if that is still on a screen, otherwise
/// centres it at its first size; before it shows.
fn restore_placement(ui: &AppWindow, session: &Session, chrome: &Chrome) {
    let saved = session.ui_state().window;
    chrome.saved.set(saved);
    let place = saved
        .as_ref()
        .and_then(win::visible_placement)
        .or_else(|| win::default_placement(FIRST_SIZE.0, FIRST_SIZE.1));
    win::prepare_window(place);
    let Some(place) = place else {
        return;
    };
    // An explicit size, too: without one Slint resizes the window to its preferred size as it
    // shows it.
    if let Some((width, height)) = win::client_logical_size(&place) {
        ui.window().set_size(slint::LogicalSize::new(width, height));
    }
    if place.maximized {
        ui.window().set_maximized(true);
    }
}

/// Attaches the main window's title bar once winit has made the window.
fn attach_main(ui: &AppWindow, session: &Arc<Session>, chrome: &Rc<Chrome>) {
    let (weak, session, chrome) = (ui.as_weak(), session.clone(), chrome.clone());
    let spawned = slint::spawn_local(async move {
        let Some(hwnd) = window_hwnd(weak.clone()).await else {
            win::cancel_prepared_window();
            return;
        };
        chrome.hwnd.set(hwnd);
        win::set_dialog_owner(hwnd);
        let listener = {
            let (weak, chrome) = (weak.clone(), Rc::downgrade(&chrome));
            move |event| main_event(&weak, &chrome, &session, event)
        };
        if let Ok(bar) = TitleBar::install(hwnd, Box::new(listener)) {
            chrome.bar.replace(Some(bar));
        }
        if let Some(ui) = weak.upgrade() {
            push_max_rect(&ui, &chrome);
        }
    });
    if spawned.is_err() {
        win::cancel_prepared_window();
    }
}

/// Attaches the Settings window's title bar (no maximize button, nor maximizing) once winit has
/// made the window.
fn attach_settings(window: &SettingsWindow, chrome: &Rc<Chrome>) {
    if !window.window().has_winit_window() {
        win::prepare_window(None);
    }
    let (weak, chrome) = (window.as_weak(), chrome.clone());
    let spawned = slint::spawn_local(async move {
        let Some(hwnd) = window_hwnd(weak.clone()).await else {
            win::cancel_prepared_window();
            return;
        };
        chrome.hwnd.set(hwnd);
        let listener = {
            let weak = weak.clone();
            move |event| {
                if let (CaptionEvent::Active(active), Some(window)) = (event, weak.upgrade()) {
                    window.set_settings_window_active(active);
                }
            }
        };
        if let Ok(bar) = TitleBar::install(hwnd, Box::new(listener)) {
            bar.set_max_rect(None);
            chrome.bar.replace(Some(bar));
        }
        // Close and minimize only: no maximizing from the keyboard, the system menu, a
        // double-click or a drag to the top of the screen.
        if let Some(window) = weak.upgrade() {
            window.window().with_winit_window(|window| {
                window.set_enabled_buttons(WindowButtons::CLOSE | WindowButtons::MINIMIZE);
            });
        }
    });
    if spawned.is_err() {
        win::cancel_prepared_window();
    }
}

/// What the main window's title bar reports.
fn main_event(
    weak: &slint::Weak<AppWindow>,
    chrome: &Weak<Chrome>,
    session: &Session,
    event: CaptionEvent,
) {
    let (Some(ui), Some(chrome)) = (weak.upgrade(), chrome.upgrade()) else {
        return;
    };
    match event {
        CaptionEvent::MaxHover(hovered) => ui.set_caption_max_hovered(hovered),
        CaptionEvent::MaxPressed(pressed) => ui.set_caption_max_pressed(pressed),
        CaptionEvent::MaxClicked => {
            if let Some(hwnd) = chrome.hwnd() {
                win::toggle_maximize(hwnd);
            }
        }
        CaptionEvent::Maximized(maximized) => {
            ui.set_window_maximized(maximized);
            push_max_rect(&ui, &chrome);
        }
        CaptionEvent::Active(active) => ui.set_window_active(active),
        CaptionEvent::ScaleChanged => push_max_rect(&ui, &chrome),
        CaptionEvent::Moved => save_placement_of(&chrome, session),
    }
}

/// Tells the title bar where the maximize button is, in physical pixels.
fn push_max_rect(ui: &AppWindow, chrome: &Chrome) {
    let bar = chrome.bar.borrow();
    let Some(bar) = bar.as_ref() else {
        return;
    };
    bar.set_max_rect(physical_rect(
        (ui.get_caption_max_x(), ui.get_caption_max_y()),
        (ui.get_caption_max_width(), ui.get_caption_max_height()),
        ui.window().scale_factor(),
    ));
}

/// A rectangle in logical window pixels as physical client pixels; None when it is empty.
fn physical_rect(origin: (f32, f32), size: (f32, f32), scale: f32) -> Option<CaptionRect> {
    let px = |logical: f32| (logical * scale).round() as i32;
    (size.0 > 0.0 && size.1 > 0.0).then(|| CaptionRect {
        left: px(origin.0),
        top: px(origin.1),
        right: px(origin.0 + size.0),
        bottom: px(origin.1 + size.1),
    })
}

/// Saves where the window is, if that changed.
fn save_placement_of(chrome: &Chrome, session: &Session) {
    let Some(place) = chrome.hwnd().and_then(win::placement) else {
        return;
    };
    if chrome.saved.replace(Some(place)) == Some(place) {
        return;
    }
    let mut state = session.ui_state();
    state.window = Some(place);
    session.save_ui_state(&state);
}

/// The system menu at `(x, y)`, logical pixels in the window.
fn show_menu(window: &slint::Window, hwnd: isize, x: f32, y: f32) {
    let Some(origin) = window
        .with_winit_window(|window| window.inner_position().ok())
        .flatten()
    else {
        return;
    };
    let scale = window.scale_factor();
    win::show_system_menu(
        hwnd,
        origin.x + (x * scale).round() as i32,
        origin.y + (y * scale).round() as i32,
    );
}

/// Whether `window` is a winit window (the app's backend; the tests' is not), made already or to
/// be made when it shows.
fn backed_by_winit(window: &slint::Window) -> bool {
    if window.has_winit_window() {
        return true;
    }
    // Not made yet: a winit window's future waits, any other's fails at once.
    let mut made = std::pin::pin!(window.winit_window());
    !matches!(
        made.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(_))
    )
}

/// The HWND of a window once winit has made it; None if the window went away first.
async fn window_hwnd<T: ComponentHandle>(weak: slint::Weak<T>) -> Option<isize> {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let component = weak.upgrade()?;
    let window = component.window().winit_window().await.ok()?;
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_max_rect_scales_to_physical_pixels() {
        assert_eq!(
            physical_rect((1142.0, 0.0), (46.0, 52.0), 1.5),
            Some(CaptionRect {
                left: 1713,
                top: 0,
                right: 1782,
                bottom: 78,
            })
        );
        assert_eq!(physical_rect((10.0, 0.0), (0.0, 52.0), 1.0), None);
    }
}
