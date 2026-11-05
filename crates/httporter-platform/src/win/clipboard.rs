//! Files on the clipboard: what Explorer's Copy (or Cut) puts there.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

/// Another program may hold the clipboard open for a moment (clipboard managers and remote
/// desktop sync do), so opening it is retried briefly before giving up.
const OPEN_ATTEMPTS: u32 = 5;
const OPEN_PAUSE: Duration = Duration::from_millis(10);

/// Closes the clipboard when dropped.
struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn open() -> Option<Self> {
        for attempt in 0..OPEN_ATTEMPTS {
            if attempt > 0 {
                sleep(OPEN_PAUSE);
            }
            // SAFETY: no window handle is given, so the clipboard belongs to the current task.
            if unsafe { OpenClipboard(None::<HWND>) }.is_ok() {
                return Some(Self);
            }
        }
        None
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: only built after `OpenClipboard` succeeded on this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

/// The files and folders on the clipboard as a file drop (`CF_HDROP`), in clipboard order.
/// Empty if the clipboard holds something else, is busy, or can't be read. A "cut" is read
/// the same as a "copy": nothing is moved or removed here.
pub fn clipboard_files() -> Vec<PathBuf> {
    let format = u32::from(CF_HDROP.0);
    // SAFETY: plain query, no clipboard access needed.
    if unsafe { IsClipboardFormatAvailable(format) }.is_err() {
        return Vec::new();
    }
    let Some(_open) = OpenClipboardGuard::open() else {
        return Vec::new();
    };
    // SAFETY: the clipboard is open. The returned handle stays owned by the clipboard and
    // is only used below, before the guard closes it.
    let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
        return Vec::new();
    };
    let drop = HDROP(handle.0);

    // SAFETY: `drop` is the file-drop handle the clipboard just gave us; asking for index
    // u32::MAX returns the number of files.
    let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
    let mut files = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: as above; with no buffer the call returns the length in characters
        // without the terminating zero.
        let len = unsafe { DragQueryFileW(drop, index, None) } as usize;
        let mut buffer = vec![0u16; len + 1];
        // SAFETY: the buffer has room for the name and its terminating zero.
        let written = unsafe { DragQueryFileW(drop, index, Some(&mut buffer)) } as usize;
        if written > 0 {
            files.push(PathBuf::from(OsString::from_wide(
                &buffer[..written.min(len)],
            )));
        }
    }
    files
}

/// Puts `text` on the clipboard (T4).
#[allow(unused_variables)]
pub fn set_clipboard_text(text: &str) -> Result<(), String> {
    todo!("T4")
}
