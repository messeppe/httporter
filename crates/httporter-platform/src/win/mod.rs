//! Windows services: taskbar progress, notifications, file pickers, clipboard files, the title
//! bar, window placement, the tray icon and the accessibility settings.
//!
//! The test suite covers only the pure parts (state-to-flag mapping, toast XML, the
//! registration values). Anything that touches the real desktop (a toast, a picker, the
//! clipboard, the registry, a taskbar button) is left to a person to check, because running
//! it from tests would pop windows and notifications up at whoever runs them.

mod a11y;
mod clipboard;
mod notify;
mod pickers;
mod placement;
mod shell;
mod taskbar;
mod titlebar;
mod tray;
mod watch;

pub use a11y::{animations_enabled, text_scale};
pub use clipboard::{clipboard_files, set_clipboard_text};
pub use notify::{ensure_app_identity, notify, notify_or_flash};
pub use pickers::{pick_file, pick_files, pick_folder, pick_save_file, set_dialog_owner};
pub use placement::{client_logical_size, default_placement, placement, visible_placement};
pub use shell::{mono_font_family, open_default, open_url, open_with};
pub use taskbar::{TaskbarProgress, TaskbarState, flash, is_foreground};
pub use titlebar::{
    CaptionEvent, CaptionRect, TitleBar, cancel_prepared_window, is_maximized, minimize,
    prepare_window, show_system_menu, start_drag, toggle_maximize,
};
pub use tray::{Tray, TrayEvent};
pub use watch::DirWatcher;

use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};

/// A window handle as the app passes it around (the numeric value of an `HWND`).
fn as_hwnd(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

/// Runs `f` with COM initialised on the calling thread (a single-threaded apartment), and
/// balances it afterwards.
///
/// The app's UI thread usually has COM up already; then this only bumps a count. A thread
/// that already chose the multi-threaded model keeps it: COM works there without our help,
/// and we must not call `CoUninitialize` for an initialisation we did not make. Anything
/// `f` creates must be dropped before `f` returns, which a closure's locals always are.
fn with_com<T>(f: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
    /// Undoes a successful `CoInitializeEx`, also when `f` panics.
    struct Uninit;
    impl Drop for Uninit {
        fn drop(&mut self) {
            // SAFETY: only built after a `CoInitializeEx` on this same thread succeeded.
            unsafe { CoUninitialize() };
        }
    }

    // DISABLE_OLE1DDE: without it every first init on a thread creates (and every last
    // uninit destroys) a hidden DDE window, which is wasted work for a per-call apartment.
    // SAFETY: plain call; the result is checked below.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
    let _guard = if hr.is_ok() {
        // S_OK or S_FALSE (already initialised, same model): both need a matching uninit.
        Some(Uninit)
    } else if hr == RPC_E_CHANGED_MODE {
        None
    } else {
        return Err(hr.into());
    };
    f()
}

#[cfg(test)]
mod tests {
    /// The app sets its identity at startup, so the function must be reachable from outside
    /// the crate as `win::ensure_app_identity`. Only its type is checked: calling it would
    /// write to the registry.
    #[test]
    fn ensure_app_identity_is_exported() {
        let _startup: fn() -> Result<(), String> = super::ensure_app_identity;
    }
}
