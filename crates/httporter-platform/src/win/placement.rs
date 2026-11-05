//! Where the main window was left, and whether that place is still on a screen.

use httporter_core::WindowPlacement;

/// The window's normal (restored) outer rectangle and maximized state (GetWindowPlacement).
pub fn placement(hwnd: isize) -> Option<WindowPlacement> {
    let _ = hwnd;
    todo!("T2")
}

/// The saved rect if it intersects a monitor (MonitorFromRect, MONITOR_DEFAULTTONULL), else None.
pub fn visible_placement(saved: &WindowPlacement) -> Option<WindowPlacement> {
    let _ = saved;
    todo!("T2")
}
