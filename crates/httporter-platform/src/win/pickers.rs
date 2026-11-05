//! Native file and folder pickers (the standard Windows dialogs).
//!
//! Each call blocks the calling thread until the dialog is closed. COM is set up by the
//! dialog library itself, per call.
//!
//! Every dialog belongs to a window of the app (ledger 187), so that it stays in front of it and
//! the window waits for it: the app's window in front when the dialog opens (the main window or
//! Settings), else the main window ([`set_dialog_owner`]).

use std::num::NonZeroIsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};

use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawWindowHandle,
    Win32WindowHandle, WindowHandle,
};
use rfd::FileDialog;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId, IsWindow,
    IsWindowVisible,
};

use super::as_hwnd;

/// The main window, the dialogs' owner when no window of the app is in front.
static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);

/// The class of the app's windows (winit's).
const APP_WINDOW_CLASS: &str = "Window Class";

/// Makes `hwnd` (the main window) the owner of the dialogs opened while no window of the app is
/// in front.
pub fn set_dialog_owner(hwnd: isize) {
    MAIN_WINDOW.store(hwnd, Ordering::Relaxed);
}

/// Asks for one or more files. Empty if the user cancelled.
pub fn pick_files(title: &str) -> Vec<PathBuf> {
    dialog(title).pick_files().unwrap_or_default()
}

/// Asks for a folder. `None` if the user cancelled.
pub fn pick_folder(title: &str) -> Option<PathBuf> {
    dialog(title).pick_folder()
}

/// Asks for one file. `filters` are `(name, extensions)` pairs without dots, for example
/// `("Images", &["png", "jpg"])`; none means every file. `None` if the user cancelled.
pub fn pick_file(title: &str, filters: &[(&str, &[&str])]) -> Option<PathBuf> {
    let mut dialog = dialog(title);
    for &(name, extensions) in filters {
        dialog = dialog.add_filter(name, extensions);
    }
    dialog.pick_file()
}

/// Asks where to save a file, offering `default_name`. `filters` are as for [`pick_file`]. `None`
/// if the user cancelled.
pub fn pick_save_file(
    title: &str,
    default_name: &str,
    filters: &[(&str, &[&str])],
) -> Option<PathBuf> {
    let mut dialog = dialog(title).set_file_name(default_name);
    for &(name, extensions) in filters {
        dialog = dialog.add_filter(name, extensions);
    }
    dialog.save_file()
}

/// A dialog titled `title`, owned by the app's window (see the module's notes).
fn dialog(title: &str) -> FileDialog {
    let dialog = FileDialog::new().set_title(title);
    match owner() {
        Some(owner) => dialog.set_parent(&owner),
        None => dialog,
    }
}

/// The app's window in front, else the main window; None when neither is on screen.
fn owner() -> Option<Owner> {
    // SAFETY: plain queries.
    let front = unsafe {
        let window = GetForegroundWindow();
        let mut process = 0;
        GetWindowThreadProcessId(window, Some(&mut process));
        (!window.is_invalid() && process == GetCurrentProcessId())
            .then(|| GetAncestor(window, GA_ROOT))
            .filter(|root| is_app_window(root.0 as isize))
    };
    let hwnd = front.map_or_else(|| MAIN_WINDOW.load(Ordering::Relaxed), |w| w.0 as isize);
    NonZeroIsize::new(hwnd)
        .filter(|hwnd| is_app_window(hwnd.get()))
        .map(Owner)
}

/// A visible window of the app's own class.
fn is_app_window(hwnd: isize) -> bool {
    let window = as_hwnd(hwnd);
    let mut name = [0u16; 64];
    // SAFETY: plain queries; GetClassNameW writes at most `name.len()` units.
    unsafe {
        if !IsWindow(Some(window)).as_bool() || !IsWindowVisible(window).as_bool() {
            return false;
        }
        let len = GetClassNameW(window, &mut name);
        usize::try_from(len)
            .is_ok_and(|len| String::from_utf16_lossy(&name[..len]) == APP_WINDOW_CLASS)
    }
}

/// A window as rfd takes its parent: a raw Win32 handle.
struct Owner(NonZeroIsize);

impl HasWindowHandle for Owner {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let raw = RawWindowHandle::Win32(Win32WindowHandle::new(self.0));
        // SAFETY: a window of this process that was on screen a moment ago; rfd only reads the
        // handle, for the time of the dialog.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for Owner {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}
