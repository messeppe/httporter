//! The taskbar button: progress bar, attention flash and "is HTTPorter in front".

use std::mem::size_of;

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::{
    ITaskbarList3, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
    TBPFLAG, TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FLASHW_ALL, FLASHW_TIMERNOFG, FLASHWINFO, FlashWindowEx, GA_ROOTOWNER, GetAncestor,
    GetForegroundWindow, IsWindow,
};

use super::{as_hwnd, with_com};

/// What the progress bar on HTTPorter's taskbar button shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskbarState {
    /// No progress bar.
    None,
    /// Green bar, `done` of `total`. A `total` of 0 means "unknown": a moving indicator.
    Normal { done: u64, total: u64 },
    /// Yellow bar: the transfer is paused.
    Paused { done: u64, total: u64 },
    /// Red bar: the transfer stopped on an error.
    Error { done: u64, total: u64 },
}

/// The taskbar flag for a state, with the `(completed, total)` to draw. A `total` of 0 in
/// the result means there is no fraction to set (no bar, or the moving indicator).
fn flags(state: &TaskbarState) -> (TBPFLAG, u64, u64) {
    // `done` past `total` can't be drawn; the bar just stops at full. An unknown total has
    // no fraction either: the moving indicator for a running transfer, and a full bar for
    // paused/error, since an empty bar would show no colour and hide the state.
    fn fraction(flag: TBPFLAG, done: u64, total: u64) -> (TBPFLAG, u64, u64) {
        if total == 0 {
            (flag, 1, 1)
        } else {
            (flag, done.min(total), total)
        }
    }
    match *state {
        TaskbarState::None => (TBPF_NOPROGRESS, 0, 0),
        TaskbarState::Normal { total: 0, .. } => (TBPF_INDETERMINATE, 0, 0),
        TaskbarState::Normal { done, total } => fraction(TBPF_NORMAL, done, total),
        TaskbarState::Paused { done, total } => fraction(TBPF_PAUSED, done, total),
        TaskbarState::Error { done, total } => fraction(TBPF_ERROR, done, total),
    }
}

/// Progress on one window's taskbar button.
///
/// Holds only the window handle, so it can be shared and used from any thread: the shell
/// object behind it belongs to the thread that created it, so every call makes its own
/// (a cheap in-process object) inside a short COM scope.
#[derive(Debug, Clone, Copy)]
pub struct TaskbarProgress {
    hwnd: isize,
}

impl TaskbarProgress {
    /// Progress for the window `hwnd`. `None` if it isn't a window, or the shell won't
    /// provide the taskbar interface (then the app simply has no taskbar progress).
    pub fn new(hwnd: isize) -> Option<Self> {
        // SAFETY: `IsWindow` accepts any handle value, valid or not.
        if hwnd == 0 || !unsafe { IsWindow(Some(as_hwnd(hwnd))) }.as_bool() {
            return None;
        }
        with_com(taskbar_list).ok()?;
        Some(Self { hwnd })
    }

    /// Shows `state` on the button. Never fails visibly: progress is decoration, and the
    /// shell refuses while the window has no taskbar button (not shown yet, or Explorer is
    /// restarting). Calling it again with the next state sets everything afresh, so a
    /// button that came back is brought up to date by the next progress tick.
    pub fn set(&self, state: TaskbarState) {
        let _ = with_com(|| apply(self.hwnd, state));
    }
}

fn taskbar_list() -> windows::core::Result<ITaskbarList3> {
    // SAFETY: COM is initialised by the caller (`with_com`); the CLSID is the shell's.
    let list: ITaskbarList3 =
        unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)? };
    // SAFETY: `list` is a live interface.
    unsafe { list.HrInit()? };
    Ok(list)
}

fn apply(hwnd: isize, state: TaskbarState) -> windows::core::Result<()> {
    let list = taskbar_list()?;
    let window = as_hwnd(hwnd);
    let (flag, completed, total) = flags(&state);
    // SAFETY: `list` is a live interface; the shell validates `window` itself.
    unsafe {
        if total > 0 {
            // The value first, the state last: the state is what the user must end up
            // seeing, whatever the value call did to it.
            list.SetProgressValue(window, completed, total)?;
        }
        list.SetProgressState(window, flag)
    }
}

/// Flashes the taskbar button (and the title bar) until the window is brought to the front.
/// This is the fallback when a notification can't be shown.
pub fn flash(hwnd: isize) {
    let info = FLASHWINFO {
        cbSize: size_of::<FLASHWINFO>() as u32,
        hwnd: as_hwnd(hwnd),
        dwFlags: FLASHW_ALL | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: `info` is fully initialised and lives for the call. The return value is the
    // window's previous flash state, which nobody needs.
    let _ = unsafe { FlashWindowEx(&info) };
}

/// Whether HTTPorter is the program the user is working in: the window itself, or a dialog
/// it owns (a file picker, say), is the foreground window. False if it is minimised, hidden
/// or behind another program.
pub fn is_foreground(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    let window = as_hwnd(hwnd);
    // SAFETY: no arguments; may return a null handle when no window has the focus.
    let front = unsafe { GetForegroundWindow() };
    if front.0.is_null() {
        return false;
    }
    // SAFETY: `front` was just returned by the system; a stale one only yields a null result.
    front == window || unsafe { GetAncestor(front, GA_ROOTOWNER) } == window
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taskbar_state_maps_to_flags_and_values() {
        assert_eq!(flags(&TaskbarState::None), (TBPF_NOPROGRESS, 0, 0));
        assert_eq!(
            flags(&TaskbarState::Normal { done: 3, total: 10 }),
            (TBPF_NORMAL, 3, 10)
        );
        assert_eq!(
            flags(&TaskbarState::Paused { done: 4, total: 8 }),
            (TBPF_PAUSED, 4, 8)
        );
        assert_eq!(
            flags(&TaskbarState::Error { done: 0, total: 5 }),
            (TBPF_ERROR, 0, 5)
        );

        // More done than total can't be drawn: the bar stops at full.
        assert_eq!(
            flags(&TaskbarState::Normal {
                done: 12,
                total: 10
            }),
            (TBPF_NORMAL, 10, 10)
        );

        // An unknown total has no fraction to draw. Normal becomes the moving indicator;
        // paused and error show a full bar, because an empty bar would show no colour at all.
        assert_eq!(
            flags(&TaskbarState::Normal { done: 0, total: 0 }),
            (TBPF_INDETERMINATE, 0, 0)
        );
        assert_eq!(
            flags(&TaskbarState::Paused { done: 0, total: 0 }),
            (TBPF_PAUSED, 1, 1)
        );
        assert_eq!(
            flags(&TaskbarState::Error { done: 7, total: 0 }),
            (TBPF_ERROR, 1, 1)
        );
    }
}
