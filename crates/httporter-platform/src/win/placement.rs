// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where the main window was left, and whether that place is still on a screen.
//!
//! A placement is the window's outer rectangle in physical screen pixels, which includes the
//! invisible left, right and bottom resize borders the title bar keeps (`titlebar.rs`); the top
//! of the rectangle is the top of what shows.

use httporter_core::WindowPlacement;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
    MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromPoint, MonitorFromRect,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowPlacement, GetWindowRect, IsIconic, IsZoomed, WINDOWPLACEMENT,
    WPF_RESTORETOMAXIMIZED,
};

use super::as_hwnd;
use super::titlebar::frame_insets;

/// How much of the title bar, in logical pixels from the window's top, must be on a screen for
/// the window to be dragged back.
const TITLE_BAR_HEIGHT: i32 = 32;

/// The window's normal (restored) outer rectangle and maximized state (GetWindowPlacement).
///
/// Maximized or minimized, the rectangle is the one it restores to. Otherwise it is where the
/// window is, also when it is snapped to an edge (what the user sees, rather than the rectangle
/// before the snap).
pub fn placement(hwnd: isize) -> Option<WindowPlacement> {
    let hwnd = as_hwnd(hwnd);
    let mut saved = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: plain queries writing only to their out-parameters.
    unsafe { GetWindowPlacement(hwnd, &mut saved) }.ok()?;
    let (maximized, minimized) = unsafe { (IsZoomed(hwnd).as_bool(), IsIconic(hwnd).as_bool()) };
    let rect = if maximized || minimized {
        workspace_to_screen(saved.rcNormalPosition)
    } else {
        let mut rect = RECT::default();
        // SAFETY: writes only to `rect`.
        unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
        rect
    };
    let maximized = maximized || (minimized && saved.flags.contains(WPF_RESTORETOMAXIMIZED));
    to_placement(rect, maximized)
}

/// The saved rect if it intersects a monitor (MonitorFromRect, MONITOR_DEFAULTTONULL), else None.
///
/// More exactly: its title bar is on a monitor (so it can be dragged), and what shows of it fits
/// that monitor's work area. A window left on a monitor that is gone, or bigger than the screen
/// now is, opens at its default place instead.
pub fn visible_placement(saved: &WindowPlacement) -> Option<WindowPlacement> {
    let rect = to_rect(saved)?;
    // SAFETY: plain query.
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) };
    if monitor.is_invalid() {
        return None;
    }
    let (work, dpi) = monitor_work_area(monitor)?;
    let (left, right, bottom) = frame_insets(dpi);
    let fits = rect.right - rect.left - left - right <= work.right - work.left
        && rect.bottom - rect.top - bottom <= work.bottom - work.top;
    let title_bar = RECT {
        left: rect.left + left,
        top: rect.top,
        right: rect.right - right,
        bottom: rect.top + scale(TITLE_BAR_HEIGHT, dpi),
    };
    // SAFETY: plain query.
    let reachable = !unsafe { MonitorFromRect(&title_bar, MONITOR_DEFAULTTONULL) }.is_invalid();
    (fits && reachable).then_some(*saved)
}

/// The window's first placement: `width` × `height` logical pixels of content, centred in the work
/// area of the monitor under the mouse (smaller if that does not fit). None if Windows will not say
/// where the monitors are.
pub fn default_placement(width: u32, height: u32) -> Option<WindowPlacement> {
    let mut cursor = POINT::default();
    // SAFETY: plain queries.
    let monitor = unsafe {
        let _ = GetCursorPos(&mut cursor);
        MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY)
    };
    let (work, dpi) = monitor_work_area(monitor)?;
    let (left, right, bottom) = frame_insets(dpi);
    let (work_width, work_height) = (work.right - work.left, work.bottom - work.top);
    let to_px = |logical: u32| scale(i32::try_from(logical).unwrap_or(i32::MAX), dpi);
    let content_width = to_px(width).min(work_width);
    let content_height = to_px(height).min(work_height);
    to_placement(
        RECT {
            left: work.left + (work_width - content_width) / 2 - left,
            top: work.top + (work_height - content_height) / 2,
            right: work.left + (work_width + content_width) / 2 + right,
            bottom: work.top + (work_height + content_height) / 2 + bottom,
        },
        false,
    )
}

/// The size of the window's content (its client area) at `place`, in logical pixels of the
/// monitor it is on: what to give Slint so that the window has that outer size.
pub fn client_logical_size(place: &WindowPlacement) -> Option<(f32, f32)> {
    let rect = to_rect(place)?;
    // SAFETY: plain query.
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST) };
    let (_, dpi) = monitor_work_area(monitor)?;
    let (left, right, bottom) = frame_insets(dpi);
    let factor = dpi as f32 / 96.0;
    let width = (rect.right - rect.left - left - right).max(1) as f32 / factor;
    let height = (rect.bottom - rect.top - bottom).max(1) as f32 / factor;
    Some((width, height))
}

/// A monitor's work area (screen pixels) and DPI.
fn monitor_work_area(monitor: HMONITOR) -> Option<(RECT, u32)> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: plain queries writing only to their out-parameters.
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (96, 96);
    // SAFETY: as above.
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err() {
        dpi_x = 96;
    }
    Some((info.rcWork, dpi_x.max(1)))
}

/// `GetWindowPlacement`'s rectangles are in workspace coordinates: relative to the work area,
/// which differs from the screen when the taskbar is at the top or left.
fn workspace_to_screen(rect: RECT) -> RECT {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: plain queries writing only to `info`.
    let known = unsafe {
        let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONEAREST);
        GetMonitorInfoW(monitor, &mut info).as_bool()
    };
    if !known {
        return rect;
    }
    let dx = info.rcWork.left - info.rcMonitor.left;
    let dy = info.rcWork.top - info.rcMonitor.top;
    RECT {
        left: rect.left + dx,
        top: rect.top + dy,
        right: rect.right + dx,
        bottom: rect.bottom + dy,
    }
}

fn to_placement(rect: RECT, maximized: bool) -> Option<WindowPlacement> {
    let width = u32::try_from(rect.right - rect.left)
        .ok()
        .filter(|w| *w > 0)?;
    let height = u32::try_from(rect.bottom - rect.top)
        .ok()
        .filter(|h| *h > 0)?;
    Some(WindowPlacement {
        x: rect.left,
        y: rect.top,
        width,
        height,
        maximized,
    })
}

fn to_rect(place: &WindowPlacement) -> Option<RECT> {
    let width = i32::try_from(place.width).ok().filter(|w| *w > 0)?;
    let height = i32::try_from(place.height).ok().filter(|h| *h > 0)?;
    Some(RECT {
        left: place.x,
        top: place.y,
        right: place.x.checked_add(width)?,
        bottom: place.y.checked_add(height)?,
    })
}

/// `logical` pixels at `dpi`.
fn scale(logical: i32, dpi: u32) -> i32 {
    (i64::from(logical) * i64::from(dpi) / 96).clamp(0, i64::from(i32::MAX)) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placement_and_its_rectangle_convert_both_ways() {
        let place = WindowPlacement {
            x: -1928,
            y: 12,
            width: 1296,
            height: 808,
            maximized: true,
        };
        let rect = to_rect(&place).unwrap();
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (-1928, 12, -632, 820)
        );
        assert_eq!(to_placement(rect, true), Some(place));
        assert_eq!(to_rect(&WindowPlacement { width: 0, ..place }), None);
    }

    #[test]
    fn logical_pixels_scale_with_the_dpi() {
        assert_eq!(scale(32, 96), 32);
        assert_eq!(scale(32, 144), 48);
        assert_eq!(scale(1280, 192), 2560);
    }
}
