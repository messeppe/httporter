//! One HTTPorter per user session (review M10). A second one started while the first runs would
//! count the first one's live queue as "didn't finish last time": Resume would upload it twice,
//! Discard would cancel its journal rows. So a second start brings the first one's window to the
//! front and closes.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE,
    SetForegroundWindow, ShowWindow,
};
use windows_sys::core::BOOL;

/// The named mutex the first HTTPorter of a session holds while it runs.
const MUTEX_NAME: &str = "Local\\HTTPorter.Running";

/// Whether this is the only HTTPorter of the user's session. The first one keeps the mutex
/// open for as long as it runs; Windows closes it when the process ends, crash or not. When
/// Windows can't say, the app runs.
pub fn first() -> bool {
    let name = wide(MUTEX_NAME);
    // SAFETY: a NUL-terminated name that outlives the call; no security attributes.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return true;
    }
    // SAFETY: a plain call, right after the one whose error it reads.
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        // SAFETY: the handle was just opened and is not used again.
        unsafe { CloseHandle(handle) };
    }
    // The first one's handle is never closed: it is the mark that it runs.
    !already
}

/// Brings the window of the HTTPorter already running to the front: the first visible
/// top-level window titled `title` whose process runs this same program.
pub fn show_first(title: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut search = Search {
        title: title.encode_utf16().collect(),
        exe,
        own: std::process::id(),
        found: std::ptr::null_mut(),
    };
    // SAFETY: `visit` reads `search` through the pointer only during this call, which outlives
    // nothing it borrows.
    unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
    let window = search.found;
    if window.is_null() {
        return;
    }
    // SAFETY: plain calls on a window handle Windows just handed out; a window gone since
    // makes them fail harmlessly.
    unsafe {
        if IsIconic(window) != 0 {
            ShowWindow(window, SW_RESTORE);
        }
        SetForegroundWindow(window);
    }
}

struct Search {
    /// The window title, without the NUL.
    title: Vec<u16>,
    exe: PathBuf,
    /// This process, whose own windows don't count.
    own: u32,
    found: HWND,
}

/// One top-level window of [`EnumWindows`]: stops at the one [`show_first`] looks for.
unsafe extern "system" fn visit(window: HWND, search: LPARAM) -> BOOL {
    // SAFETY: `show_first` passes a live `Search` for the length of the enumeration.
    let search = unsafe { &mut *(search as *mut Search) };
    // SAFETY: plain calls on a window handle the enumeration handed out.
    let visible = unsafe { IsWindowVisible(window) } != 0;
    if !visible {
        return 1;
    }
    let mut text = [0u16; 64];
    // SAFETY: the buffer's length is passed with it.
    let len = unsafe { GetWindowTextW(window, text.as_mut_ptr(), text.len() as i32) };
    let len = usize::try_from(len).unwrap_or(0);
    if text[..len] != search.title[..] {
        return 1;
    }
    let mut pid = 0u32;
    // SAFETY: as above; `pid` outlives the call.
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    if pid == 0 || pid == search.own || !runs(pid, &search.exe) {
        return 1;
    }
    search.found = window;
    0
}

/// Whether process `pid` runs the program at `exe` (ignoring case, as Windows does).
fn runs(pid: u32, exe: &Path) -> bool {
    // SAFETY: plain call; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: the buffer's length is passed with it, and updated to what was written.
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    } != 0;
    // SAFETY: opened above, not used again.
    unsafe { CloseHandle(process) };
    if !ok {
        return false;
    }
    let path = PathBuf::from(OsString::from_wide(&buf[..(len as usize).min(buf.len())]));
    path.to_string_lossy().to_lowercase() == exe.to_string_lossy().to_lowercase()
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
