//! The main window's own title bar: the system caption goes, the toolbar row takes its place.
//!
//! The app draws the caption buttons itself (`caption_buttons.slint`); this module hooks the
//! window procedure so Windows still gives the window its resize edges, shadow, Aero Snap and,
//! over the app's maximize button, the Windows 11 Snap Layouts flyout.

/// What the hooked window procedure saw over the app-drawn maximize button. Windows answers
/// `HTMAXBUTTON` there, so the mouse messages arrive as non-client ones that Slint never sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionEvent {
    MaxHover(bool),
    MaxPressed(bool),
    MaxClicked,
}

/// A rectangle in the window's client area, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The hooked window procedure of one window. Keeps the subclass; dropping it restores the old
/// window procedure.
#[derive(Debug)]
pub struct TitleBar {
    _private: (),
}

impl TitleBar {
    /// Removes the system caption, keeps resize edges, shadow and Snap; top-edge resize when not
    /// maximized; HTMAXBUTTON over the max rect (Win11 Snap Layouts); WM_SETICON from exe icon
    /// resource 1.
    pub fn install(hwnd: isize, on_event: Box<dyn Fn(CaptionEvent)>) -> Result<TitleBar, String> {
        let _ = (hwnd, on_event);
        todo!("T2")
    }

    /// Where the maximize button is now. None = the window has no maximize button (Settings).
    pub fn set_max_rect(&self, rect: Option<CaptionRect>) {
        let _ = rect;
        todo!("T2")
    }
}

/// Whether the window is maximized.
pub fn is_maximized(hwnd: isize) -> bool {
    let _ = hwnd;
    todo!("T2")
}

/// Maximizes the window, or restores it when it is maximized.
pub fn toggle_maximize(hwnd: isize) {
    let _ = hwnd;
    todo!("T2")
}

/// Minimizes the window.
pub fn minimize(hwnd: isize) {
    let _ = hwnd;
    todo!("T2")
}

/// The window's system menu (Restore, Move, Size, Minimize, Maximize, Close) at a screen point.
pub fn show_system_menu(hwnd: isize, screen_x: i32, screen_y: i32) {
    let _ = (hwnd, screen_x, screen_y);
    todo!("T2")
}

/// Starts moving the window with the mouse, as a press on a system caption would (Aero Snap
/// included): ReleaseCapture + WM_NCLBUTTONDOWN HTCAPTION.
pub fn start_drag(hwnd: isize) {
    let _ = hwnd;
    todo!("T2")
}
