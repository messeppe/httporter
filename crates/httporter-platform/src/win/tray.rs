// SPDX-License-Identifier: MIT OR Apache-2.0

//! The notification-area icon shown while transfers run with the window closed (§8.7.8).
//!
//! The icon's messages go to a window of its own, made on the calling thread (the UI thread),
//! whose message loop (winit's) dispatches them. It is a hidden top-level tool window rather than
//! a message-only one: it is never shown, so it is in neither the taskbar nor Alt+Tab, but unlike
//! a message-only window it hears the "TaskbarCreated" broadcast (the icon comes back after
//! Explorer restarts) and can be the foreground window, which the icon's menu needs in order to
//! close when the user clicks elsewhere.
//!
//! A left click (or Enter on the icon) is [`TrayEvent::Show`]; a right click (or the menu key)
//! opens the menu. The events are handed to the app's callback once the menu has closed, never
//! from inside it.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NINF_KEY, NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_MESSAGE, NOTIFYICON_VERSION_4,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, GWLP_USERDATA, GetSystemMetrics, GetWindowLongPtrW, HICON, IDI_APPLICATION,
    IMAGE_ICON, LR_DEFAULTCOLOR, LoadIconW, LoadImageW, MF_SEPARATOR, MF_STRING, PostMessageW,
    RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SM_MENUDROPALIGNMENT,
    SetForegroundWindow, SetMenuDefaultItem, SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_NULL, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

/// What the user picked on the tray icon: a left click is [`TrayEvent::Show`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Show,
    PauseResume,
    Quit,
}

/// The window class of the icon's window ([`TRAY_WINDOW_CLASS`], as `RegisterClassExW` takes it).
const CLASS: PCWSTR = w!("HTTPorter.Tray");

/// The class name of the icon's hidden window, by which a second HTTPorter finds it while the
/// first one's window is in the tray.
pub const TRAY_WINDOW_CLASS: &str = "HTTPorter.Tray";

/// Posted to that window (by a second HTTPorter): the same as a click on the icon,
/// [`TrayEvent::Show`].
pub const TRAY_SHOW_MESSAGE: u32 = WM_APP + 2;

/// The message the Shell sends the window about the icon.
const CALLBACK: u32 = WM_APP + 1;

/// Enter (or Space) on the focused icon: `NIN_SELECT | NINF_KEY` (not in the `windows` crate).
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

/// The icon's id within its window (one icon per window).
const ICON_ID: u32 = 1;

/// The most UTF-16 units a tooltip holds (`szTip` is 128, with its NUL).
const TIP_MAX: usize = 127;

/// The menu's items, by command id: Show HTTPorter, Pause All / Resume All, Quit.
const MENU_IDS: [usize; 3] = [1, 2, 3];

/// What the window procedure reaches through `GWLP_USERDATA`: one strong count of the `Rc` is
/// the window's, given up when the tray goes. The procedure holds another while it runs, so that a
/// tray dropped meanwhile (from inside the menu's loop, say) is freed only once it has returned.
struct Inner {
    hwnd: HWND,
    icon: HICON,
    /// The icon was loaded from the exe (and is destroyed with the tray), not the shared one.
    owns_icon: bool,
    /// The tooltip as the Shell takes it: UTF-16, NUL-terminated.
    tip: RefCell<[u16; 128]>,
    /// The menu's labels, NUL-terminated.
    labels: RefCell<[Vec<u16>; 3]>,
    on_event: Box<dyn Fn(TrayEvent)>,
    /// The "TaskbarCreated" message: Explorer (re)started, so the icon must be added again.
    taskbar_created: u32,
}

impl Inner {
    /// The icon's data with `flags` set, the tooltip and the icon filled in.
    fn data(&self, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: flags,
            uCallbackMessage: CALLBACK,
            hIcon: self.icon,
            szTip: *self.tip.borrow(),
            ..Default::default()
        }
    }

    fn notify(&self, message: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) -> bool {
        // SAFETY: `data` is a whole NOTIFYICONDATAW with its size set, alive for the call.
        unsafe { Shell_NotifyIconW(message, data) }.as_bool()
    }

    /// Puts the icon in the notification area, with the version 4 behaviour (`NIN_SELECT`, and
    /// `WM_CONTEXTMENU` with the anchor point).
    fn add(&self) -> bool {
        let data = self.data(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        if !self.notify(NIM_ADD, &data) {
            return false;
        }
        let mut version = self.data(NOTIFY_ICON_DATA_FLAGS(0));
        version.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        self.notify(NIM_SETVERSION, &version);
        true
    }

    fn delete(&self) {
        let data = self.data(NOTIFY_ICON_DATA_FLAGS(0));
        self.notify(NIM_DELETE, &data);
    }

    /// The icon's menu at screen point (`x`, `y`); what was picked, if anything.
    fn menu(&self, x: i32, y: i32) -> Option<TrayEvent> {
        // A copy: the app may change the labels while the menu's loop runs.
        let labels = self.labels.borrow().clone();
        // SAFETY: plain calls on the menu made here and on this tray's own window; every label
        // is NUL-terminated and outlives the calls.
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let _ = AppendMenuW(menu, MF_STRING, MENU_IDS[0], PCWSTR(labels[0].as_ptr()));
            let _ = AppendMenuW(menu, MF_STRING, MENU_IDS[1], PCWSTR(labels[1].as_ptr()));
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
            let _ = AppendMenuW(menu, MF_STRING, MENU_IDS[2], PCWSTR(labels[2].as_ptr()));
            // Bold, as the item a left click does.
            let _ = SetMenuDefaultItem(menu, MENU_IDS[0] as u32, 0);
            // Without this the menu stays up when the user clicks away from it.
            let _ = SetForegroundWindow(self.hwnd);
            let side = if GetSystemMetrics(SM_MENUDROPALIGNMENT) != 0 {
                TPM_RIGHTALIGN
            } else {
                TPM_LEFTALIGN
            };
            let flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | side;
            let picked = TrackPopupMenuEx(menu, flags.0, x, y, self.hwnd, None).0;
            // The documented companion of the foreground trick: lets the menu's loop end cleanly.
            let _ = PostMessageW(Some(self.hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            match usize::try_from(picked).ok()? {
                id if id == MENU_IDS[0] => Some(TrayEvent::Show),
                id if id == MENU_IDS[1] => Some(TrayEvent::PauseResume),
                id if id == MENU_IDS[2] => Some(TrayEvent::Quit),
                _ => None,
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if self.owns_icon {
            // SAFETY: the icon this tray loaded; nothing uses it once the tray is gone.
            let _ = unsafe { DestroyIcon(self.icon) };
        }
    }
}

/// The tray icon. Keeps a hidden window and its NOTIFYICONDATAW; dropping it deletes the icon.
/// It belongs to the thread that made it.
pub struct Tray {
    inner: Rc<Inner>,
}

impl fmt::Debug for Tray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tray").finish_non_exhaustive()
    }
}

impl Tray {
    /// Adds the icon (exe icon resource 1) with `tooltip` and a menu of `labels`: Show HTTPorter /
    /// Pause All or Resume All / Quit.
    pub fn new(
        tooltip: &str,
        labels: [&str; 3],
        on_event: Box<dyn Fn(TrayEvent)>,
    ) -> Result<Tray, String> {
        // SAFETY: the module handle of the running exe; never freed.
        let module = unsafe { GetModuleHandleW(None) }
            .map_err(|e| format!("can't find the program's module: {e}"))?;
        let instance = HINSTANCE::from(module);
        register_class(instance)?;
        // SAFETY: the class was registered above with this instance; the window is never shown.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                CLASS,
                w!("HTTPorter.Tray"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .map_err(|e| format!("can't make the tray icon's window: {e}"))?;
        let (icon, owns_icon) = load_icon(instance);
        // SAFETY: a NUL-terminated literal.
        let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let inner = Rc::new(Inner {
            hwnd,
            icon,
            owns_icon,
            tip: RefCell::new(tip_buffer(tooltip)),
            labels: RefCell::new(labels.map(wide)),
            on_event,
            taskbar_created,
        });
        // The window's own strong count, given back by `Drop`.
        let raw = Rc::into_raw(inner.clone());
        // SAFETY: `raw` stays valid while the window holds it (see `Inner`).
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize) };
        let tray = Tray { inner };
        if !tray.inner.add() {
            // Dropping it destroys the window (and deletes nothing, harmlessly).
            return Err("the notification area refused the icon".to_owned());
        }
        Ok(tray)
    }

    /// The tooltip, cut to 127 UTF-16 units with "…".
    pub fn set_tooltip(&self, text: &str) {
        let tip = tip_buffer(text);
        if *self.inner.tip.borrow() == tip {
            return;
        }
        *self.inner.tip.borrow_mut() = tip;
        let data = self.inner.data(NIF_TIP | NIF_SHOWTIP);
        self.inner.notify(NIM_MODIFY, &data);
    }

    /// The menu's labels: Show HTTPorter / Pause All|Resume All / Quit.
    pub fn set_labels(&self, labels: [&str; 3]) {
        *self.inner.labels.borrow_mut() = labels.map(wide);
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.inner.delete();
        let hwnd = self.inner.hwnd;
        // SAFETY: this tray's own window, on the thread that made it. The pointer is taken back
        // first, so no later message reaches the procedure with it; a call of the procedure still
        // running holds its own count.
        unsafe {
            let raw = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *const Inner;
            let _ = DestroyWindow(hwnd);
            if !raw.is_null() {
                drop(Rc::from_raw(raw));
            }
        }
    }
}

/// Registers the icon window's class once per process (a second time is no error).
fn register_class(instance: HINSTANCE) -> Result<(), String> {
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: CLASS,
        ..Default::default()
    };
    // SAFETY: the class data is complete and lives for the call; the name is a static literal.
    if unsafe { RegisterClassExW(&class) } == 0 {
        // SAFETY: plain call, right after the one that failed.
        let error = unsafe { GetLastError() };
        if error != ERROR_CLASS_ALREADY_EXISTS {
            return Err(format!(
                "can't register the tray icon's window class ({})",
                error.0
            ));
        }
    }
    Ok(())
}

/// The exe's icon (resource 1) at the small-icon size, or the system's application icon when the
/// exe has none. The second value says whether the icon is ours to destroy.
fn load_icon(instance: HINSTANCE) -> (HICON, bool) {
    // SAFETY: plain calls; resource 1 is looked up by id (MAKEINTRESOURCE).
    unsafe {
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        if let Ok(handle) = LoadImageW(
            Some(instance),
            PCWSTR(std::ptr::without_provenance(1)),
            IMAGE_ICON,
            cx,
            cy,
            LR_DEFAULTCOLOR,
        ) {
            return (HICON(handle.0), true);
        }
        (LoadIconW(None, IDI_APPLICATION).unwrap_or_default(), false)
    }
}

/// `text` as NUL-terminated UTF-16.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `text` cut to at most [`TIP_MAX`] UTF-16 units: when it is longer, it ends in "…" and keeps
/// as much before it as fits, never half of a surrogate pair.
fn fit_tip(text: &str) -> Vec<u16> {
    let mut units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= TIP_MAX {
        return units;
    }
    let mut keep = TIP_MAX - 1;
    // A high surrogate left last would lose its pair.
    if (0xD800..0xDC00).contains(&units[keep - 1]) {
        keep -= 1;
    }
    units.truncate(keep);
    units.push(0x2026);
    units
}

/// The tooltip as `szTip` holds it.
fn tip_buffer(text: &str) -> [u16; 128] {
    let mut buffer = [0u16; 128];
    let units = fit_tip(text);
    buffer[..units.len()].copy_from_slice(&units);
    buffer
}

/// The icon window's procedure: the icon's clicks, the menu, and the icon coming back after
/// Explorer restarts.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the pointer is either 0 or the window's strong count of its `Inner` (see `Inner`).
    let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Inner;
    if raw.is_null() {
        // SAFETY: default handling for a message before (or after) the tray is attached.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    // SAFETY: `raw` came from `Rc::into_raw` and the window's count is still held (single
    // thread): this call takes a count of its own for as long as it runs.
    let inner = unsafe {
        Rc::increment_strong_count(raw);
        Rc::from_raw(raw)
    };
    if message == CALLBACK {
        // Version 4: the event in the low word of `lparam`, the anchor point in `wparam`.
        let event = (lparam.0 as u32) & 0xFFFF;
        let picked = match event {
            NIN_SELECT | NIN_KEYSELECT => Some(TrayEvent::Show),
            WM_CONTEXTMENU => {
                let x = i32::from((wparam.0 & 0xFFFF) as u16 as i16);
                let y = i32::from(((wparam.0 >> 16) & 0xFFFF) as u16 as i16);
                inner.menu(x, y)
            }
            _ => None,
        };
        if let Some(event) = picked {
            (inner.on_event)(event);
        }
        return LRESULT(0);
    }
    if message == TRAY_SHOW_MESSAGE {
        (inner.on_event)(TrayEvent::Show);
        return LRESULT(0);
    }
    if message == inner.taskbar_created && inner.taskbar_created != 0 {
        inner.add();
        return LRESULT(0);
    }
    if message == WM_CLOSE {
        // Only the tray destroys its window (a WM_CLOSE sent to every top-level window of the
        // process, by taskkill say, must not).
        return LRESULT(0);
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
