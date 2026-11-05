//! The windows' own title bars: the system caption goes, the app's top row takes its place.
//!
//! The app draws the caption buttons itself (`caption_buttons.slint`); this module hooks the
//! window procedure so Windows still gives the window its resize edges, shadow, Aero Snap and,
//! over the app's maximize button, the Windows 11 Snap Layouts flyout.
//!
//! - **Hook.** The window procedure is swapped (`SetWindowLongPtrW(GWLP_WNDPROC)`); every message
//!   this module does not answer goes on to winit's (`CallWindowProcW`). [`prepare_window`] swaps
//!   it while Windows creates the window (a one-shot CBT hook on the UI thread), so the window is
//!   never drawn with the system caption, and puts it where it was left. [`TitleBar::install`]
//!   then attaches the app's listener (or hooks a window made without [`prepare_window`]).
//! - **Frame.** `WM_NCCALCSIZE` keeps Windows' left, right and bottom frame (the invisible resize
//!   borders and the shadow) and moves the client top up to the window top. A maximized window
//!   hangs off the screen by its frame, so there the client top is inset by that much (and an
//!   auto-hidden taskbar keeps a 2 px edge to come out on).
//! - **Sizes.** winit turns client sizes into window sizes as if the system caption were there
//!   (`AdjustWindowRectEx`): its size requests, its minimum size and its DPI-change resize are
//!   made shorter by the caption here ([`Frame::correct_size`], [`Frame::min_max`]).
//! - **Hit testing.** `HTTOP` / `HTTOPLEFT` / `HTTOPRIGHT` in a band as high as the resize border
//!   at the top, when not maximized; `HTMAXBUTTON` over the app's maximize button (what Windows 11
//!   shows the Snap Layouts flyout for); Windows' own answer elsewhere (the side and bottom
//!   borders, `HTCLIENT` inside).
//! - **The maximize button.** Over it the mouse messages are non-client ones. They are swallowed,
//!   so Windows draws no classic button there, and become [`CaptionEvent`]s.
//! - **Drag, double-click, menu.** The app's empty title bar is client area: Slint sees the
//!   press and calls [`start_drag`], which posts what a press on a system caption would; the
//!   system menu is posted too. Modal loops so never run inside a Slint callback (winit's event
//!   handler is not re-entrant).
//! - **Events** reach the app from a posted message, never from inside a call Slint made.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use httporter_core::WindowPlacement;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::DwmExtendFrameIntoClientArea;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, ScreenToClient,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForWindow, GetSystemMetricsForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetActiveWindow, GetDoubleClickTime, GetKeyState, ReleaseCapture, TME_LEAVE, TME_NONCLIENT,
    TRACKMOUSEEVENT, TrackMouseEvent, VK_LBUTTON,
};
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETAUTOHIDEBAREX, ABM_GETSTATE, ABS_AUTOHIDE,
    APPBARDATA, SHAppBarMessage,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CBT_CREATEWNDW, CallNextHookEx, CallWindowProcW, DefWindowProcW, DestroyIcon, EnableMenuItem,
    GWL_EXSTYLE, GWL_STYLE, GWLP_WNDPROC, GetClassNameW, GetClientRect, GetCursorPos,
    GetSystemMenu, GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, HCBT_CREATEWND, HHOOK,
    HICON, HTCAPTION, HTCLIENT, HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT, ICON_BIG, ICON_SMALL,
    IMAGE_ICON, IsIconic, IsWindow, IsWindowVisible, IsZoomed, LR_DEFAULTCOLOR, LoadImageW,
    MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, MINMAXINFO, MSG, NCCALCSIZE_PARAMS, PM_NOREMOVE,
    PeekMessageW, PostMessageW, RegisterWindowMessageW, SC_MAXIMIZE, SC_MINIMIZE, SC_MOVE,
    SC_RESTORE, SC_SIZE, SET_WINDOW_POS_FLAGS, SIZE_MAXIMIZED, SIZE_MINIMIZED, SM_CXDOUBLECLK,
    SM_CXICON, SM_CXPADDEDBORDER, SM_CXSMICON, SM_CYDOUBLECLK, SM_CYICON, SM_CYSIZEFRAME,
    SM_CYSMICON, SM_MENUDROPALIGNMENT, SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetWindowLongPtrW,
    SetWindowPos, SetWindowsHookExW, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON,
    TrackPopupMenu, UnhookWindowsHookEx, WA_INACTIVE, WH_CBT, WINDOW_EX_STYLE, WINDOW_STYLE,
    WINDOWPOS, WM_ACTIVATE, WM_CREATE, WM_DPICHANGED, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE,
    WM_GETMINMAXINFO, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST,
    WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE,
    WM_NCRBUTTONDBLCLK, WM_NCRBUTTONDOWN, WM_NCRBUTTONUP, WM_SETICON, WM_SIZE, WM_SYSCOMMAND,
    WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING, WNDPROC, WS_CHILD, WS_MAXIMIZEBOX, WS_MINIMIZEBOX,
    WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};
use windows::core::{PCWSTR, w};

use super::as_hwnd;

/// What the hooked window procedure saw. The first three are about the app-drawn maximize
/// button: Windows answers `HTMAXBUTTON` there, so the mouse messages arrive as non-client ones
/// that Slint never sees. The others are the window's state, for the app's properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionEvent {
    MaxHover(bool),
    MaxPressed(bool),
    MaxClicked,
    /// The window was maximized (`true`) or is no longer.
    Maximized(bool),
    /// The window became the active one (`true`) or stopped being it: the caption dims.
    Active(bool),
    /// The window's DPI changed (it went to a monitor of another scale): the maximize button's
    /// rectangle, in physical pixels, must be given again.
    ScaleChanged,
    /// The window moved, was resized, maximized or restored (a drag reports once, at its end).
    Moved,
}

/// A rectangle in the window's client area, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl CaptionRect {
    fn contains(&self, point: POINT) -> bool {
        (self.left..self.right).contains(&point.x) && (self.top..self.bottom).contains(&point.y)
    }
}

/// The hooked window procedure of one window. Keeps the subclass; dropping it restores the old
/// window procedure (and with it the system caption).
#[derive(Debug)]
pub struct TitleBar {
    hwnd: isize,
}

impl TitleBar {
    /// Removes the system caption, keeps resize edges, shadow and Snap; top-edge resize when not
    /// maximized; HTMAXBUTTON over the max rect (Win11 Snap Layouts); WM_SETICON from exe icon
    /// resource 1.
    ///
    /// A window hooked as it was created ([`prepare_window`]) only gets `on_event`; any other is
    /// hooked now. `on_event` is first told whether the window is maximized and active.
    pub fn install(hwnd: isize, on_event: Box<dyn Fn(CaptionEvent)>) -> Result<TitleBar, String> {
        let window = as_hwnd(hwnd);
        // SAFETY: plain query.
        if !unsafe { IsWindow(Some(window)) }.as_bool() {
            return Err(format!("{hwnd:#x} is not a window"));
        }
        let frame = match frame_of(hwnd) {
            Some(frame) => frame,
            None => {
                let frame = subclass(window, None)
                    .ok_or_else(|| "the window procedure could not be replaced".to_owned())?;
                frame.decorate(window);
                // Windows works the frame out again, now without the caption.
                // SAFETY: a window of this thread; no size or place changes.
                unsafe {
                    let _ = SetWindowPos(
                        window,
                        None,
                        0,
                        0,
                        0,
                        0,
                        SWP_FRAMECHANGED
                            | SWP_NOMOVE
                            | SWP_NOSIZE
                            | SWP_NOZORDER
                            | SWP_NOOWNERZORDER
                            | SWP_NOACTIVATE,
                    );
                }
                frame
            }
        };
        frame.passthrough.set(false);
        frame.on_event.replace(Some(Rc::from(on_event)));
        // The state so far, for the new listener.
        frame.last_maximized.set(None);
        frame.last_active.set(None);
        // SAFETY: plain queries about a window of this thread.
        let (maximized, active) =
            unsafe { (IsZoomed(window).as_bool(), GetActiveWindow() == window) };
        frame.note_maximized(window, maximized);
        frame.note_active(window, active);
        Ok(TitleBar { hwnd })
    }

    /// Where the maximize button is now. None = the window has no maximize button (Settings).
    pub fn set_max_rect(&self, rect: Option<CaptionRect>) {
        if let Some(frame) = frame_of(self.hwnd) {
            frame.max_rect.set(rect);
        }
    }
}

impl Drop for TitleBar {
    fn drop(&mut self) {
        let Some(frame) = frame_of(self.hwnd) else {
            return;
        };
        frame.on_event.replace(None);
        let window = as_hwnd(self.hwnd);
        // SAFETY: plain query.
        let current = unsafe { GetWindowLongPtrW(window, GWLP_WNDPROC) };
        if current == frame_proc as *const () as isize {
            // SAFETY: puts back the procedure that was in place before ours.
            unsafe { SetWindowLongPtrW(window, GWLP_WNDPROC, frame.next_raw()) };
            let _ = FRAMES.try_with(|frames| frames.borrow_mut().remove(&self.hwnd));
            // SAFETY: as in `install`.
            unsafe {
                let _ = SetWindowPos(
                    window,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED
                        | SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOZORDER
                        | SWP_NOOWNERZORDER
                        | SWP_NOACTIVATE,
                );
            }
        } else {
            // Another procedure stands in front of ours and calls it: keep passing messages on.
            frame.passthrough.set(true);
        }
    }
}

/// Hooks the next top-level winit window this thread creates, while Windows creates it, so that
/// it is never drawn with the system caption; with `place`, it is created there (the outer
/// rectangle, physical pixels) and put there again once winit has sized it. One window only; a
/// second call replaces the first. [`TitleBar::install`] attaches to the window afterwards.
pub fn prepare_window(place: Option<WindowPlacement>) {
    cancel_prepared_window();
    // SAFETY: a hook on this thread only, with a procedure of the HOOKPROC signature.
    let Ok(hook) =
        (unsafe { SetWindowsHookExW(WH_CBT, Some(cbt_proc), None, GetCurrentThreadId()) })
    else {
        return;
    };
    PREPARED.with(|prepared| *prepared.borrow_mut() = Some(Prepared { hook, place }));
}

/// Takes back [`prepare_window`] if no window was created since (the window turned out not to be
/// a winit one).
pub fn cancel_prepared_window() {
    if let Some(prepared) = PREPARED.with(|prepared| prepared.borrow_mut().take()) {
        // SAFETY: the hook this thread set.
        unsafe {
            let _ = UnhookWindowsHookEx(prepared.hook);
        }
    }
}

/// Whether the window is maximized.
pub fn is_maximized(hwnd: isize) -> bool {
    // SAFETY: plain query.
    unsafe { IsZoomed(as_hwnd(hwnd)) }.as_bool()
}

/// Maximizes the window, or restores it when it is maximized. Posted, as the system's own
/// command, so that it never runs inside the caller.
pub fn toggle_maximize(hwnd: isize) {
    let command = if is_maximized(hwnd) {
        SC_RESTORE
    } else {
        SC_MAXIMIZE
    };
    post(hwnd, WM_SYSCOMMAND, WPARAM(command as usize), LPARAM(0));
}

/// Minimizes the window (posted, as `toggle_maximize`).
pub fn minimize(hwnd: isize) {
    post(hwnd, WM_SYSCOMMAND, WPARAM(SC_MINIMIZE as usize), LPARAM(0));
}

/// The window's system menu (Restore, Move, Size, Minimize, Maximize, Close) at a screen point.
/// Posted: the menu's loop never runs inside the caller (a Slint callback).
pub fn show_system_menu(hwnd: isize, screen_x: i32, screen_y: i32) {
    if frame_of(hwnd).is_some() {
        post(
            hwnd,
            menu_message(),
            WPARAM(screen_x as isize as usize),
            LPARAM(screen_y as isize),
        );
    }
}

/// Starts moving the window with the mouse, as a press on a system caption would (Aero Snap
/// included): ReleaseCapture + WM_NCLBUTTONDOWN HTCAPTION. Posted, so the move loop does not run
/// inside the caller; when it ends, the window gets the button's release that the loop took.
///
/// The second press of a double-click (as Windows counts them, with the window not moved
/// between) instead maximizes or restores the window, as on a system caption.
pub fn start_drag(hwnd: isize) {
    let window = as_hwnd(hwnd);
    let mut cursor = POINT::default();
    // SAFETY: plain query.
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return;
    }
    let frame = frame_of(hwnd);
    let double = frame
        .as_ref()
        .is_some_and(|frame| frame.second_press(cursor, window_rect(window)));
    let at = point_lparam(cursor);
    if double {
        post(hwnd, WM_NCLBUTTONDBLCLK, WPARAM(HTCAPTION as usize), at);
        return;
    }
    if let Some(frame) = &frame {
        frame.drag_posted.set(true);
    }
    // SAFETY: the press that started this was the app's, on its own window.
    unsafe {
        let _ = ReleaseCapture();
    }
    post(hwnd, WM_NCLBUTTONDOWN, WPARAM(HTCAPTION as usize), at);
}

/// The frame the app's windows keep at `dpi`, in physical pixels: the left, right and bottom
/// borders (the top one goes with the caption).
pub(crate) fn frame_insets(dpi: u32) -> (i32, i32, i32) {
    let mut rect = RECT::default();
    // SAFETY: writes only to `rect`.
    let _ = unsafe {
        AdjustWindowRectExForDpi(
            &mut rect,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
    };
    (-rect.left, rect.right, rect.bottom)
}

// ---- The hooked window procedure ----

thread_local! {
    /// The hooked windows of this thread, by HWND.
    static FRAMES: RefCell<HashMap<isize, Rc<Frame>>> = RefCell::new(HashMap::new());
    /// The window [`prepare_window`] waits for.
    static PREPARED: RefCell<Option<Prepared>> = const { RefCell::new(None) };
}

struct Prepared {
    hook: HHOOK,
    place: Option<WindowPlacement>,
}

/// The class winit registers its windows under (its default `class_name`).
const WINIT_CLASS: &str = "Window Class";

/// The app's listener for a window's [`CaptionEvent`]s.
type Listener = Rc<dyn Fn(CaptionEvent)>;

/// What the hook keeps for one window.
struct Frame {
    /// The window procedure this one stands in front of (winit's, or AccessKit's in front of it).
    next: Cell<WNDPROC>,
    on_event: RefCell<Option<Listener>>,
    /// Raised but not yet delivered (see [`Frame::emit`]).
    events: RefCell<VecDeque<CaptionEvent>>,
    max_rect: Cell<Option<CaptionRect>>,
    /// The maximize button is hovered; it was pressed and the button is not let go yet; it shows
    /// pressed (pressed and hovered).
    max_hover: Cell<bool>,
    max_armed: Cell<bool>,
    max_shown_pressed: Cell<bool>,
    /// `TrackMouseEvent` will say when the mouse leaves the non-client area.
    tracking_leave: Cell<bool>,
    in_size_move: Cell<bool>,
    in_dpi_change: Cell<bool>,
    /// Inside this module's own `SetWindowPos`: its sizes are right as they are.
    own_move: Cell<bool>,
    /// Unhooked while another procedure stood in front of this one: pass everything on.
    passthrough: Cell<bool>,
    /// Where the window goes once created ([`prepare_window`]).
    place: Cell<Option<WindowPlacement>>,
    /// This module's big and small icons, destroyed with the window.
    icons: Cell<[isize; 2]>,
    last_maximized: Cell<Option<bool>>,
    last_active: Cell<Option<bool>>,
    /// [`start_drag`] posted a caption press: the next one is it.
    drag_posted: Cell<bool>,
    /// The last caption press: when, where on the screen, and where the window was.
    last_press: Cell<Option<(Instant, POINT, RECT)>>,
}

impl Frame {
    fn new(place: Option<WindowPlacement>) -> Frame {
        Frame {
            next: Cell::new(None),
            on_event: RefCell::new(None),
            events: RefCell::new(VecDeque::new()),
            max_rect: Cell::new(None),
            max_hover: Cell::new(false),
            max_armed: Cell::new(false),
            max_shown_pressed: Cell::new(false),
            tracking_leave: Cell::new(false),
            in_size_move: Cell::new(false),
            in_dpi_change: Cell::new(false),
            own_move: Cell::new(false),
            passthrough: Cell::new(false),
            place: Cell::new(place),
            icons: Cell::new([0, 0]),
            last_maximized: Cell::new(None),
            last_active: Cell::new(None),
            drag_posted: Cell::new(false),
            last_press: Cell::new(None),
        }
    }

    fn next_raw(&self) -> isize {
        self.next.get().map_or(0, |proc| proc as *const () as isize)
    }

    fn forward(&self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: `next` is the procedure that was in place before ours, for this window.
        unsafe { CallWindowProcW(self.next.get(), hwnd, msg, wparam, lparam) }
    }

    fn handle(&self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if self.passthrough.get() {
            return self.forward(hwnd, msg, wparam, lparam);
        }
        let hit = wparam.0 as u32;
        match msg {
            WM_NCCALCSIZE if wparam.0 != 0 => self.calc_size(hwnd, wparam, lparam),
            WM_NCHITTEST => self.hit_test(hwnd, wparam, lparam),
            WM_NCMOUSEMOVE => {
                let over = hit == HTMAXBUTTON;
                self.hover_max(hwnd, over);
                if over {
                    LRESULT(0)
                } else {
                    self.forward(hwnd, msg, wparam, lparam)
                }
            }
            WM_NCMOUSELEAVE => {
                self.tracking_leave.set(false);
                self.hover_max(hwnd, false);
                self.forward(hwnd, msg, wparam, lparam)
            }
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if hit == HTMAXBUTTON => {
                self.max_armed.set(true);
                self.show_max_pressed(hwnd, true);
                LRESULT(0)
            }
            WM_NCLBUTTONUP if hit == HTMAXBUTTON => {
                let armed = self.max_armed.replace(false);
                self.show_max_pressed(hwnd, false);
                if armed {
                    self.emit(hwnd, CaptionEvent::MaxClicked);
                }
                LRESULT(0)
            }
            WM_NCRBUTTONDOWN | WM_NCRBUTTONUP | WM_NCRBUTTONDBLCLK if hit == HTMAXBUTTON => {
                LRESULT(0)
            }
            WM_NCLBUTTONDOWN if hit == HTCAPTION && self.drag_posted.replace(false) => {
                self.drag(hwnd, wparam, lparam)
            }
            WM_NCLBUTTONUP | WM_LBUTTONUP => {
                self.disarm_max(hwnd);
                self.forward(hwnd, msg, wparam, lparam)
            }
            WM_MOUSEMOVE => {
                if self.max_hover.get() {
                    self.hover_max(hwnd, false);
                }
                self.forward(hwnd, msg, wparam, lparam)
            }
            WM_WINDOWPOSCHANGING => {
                self.correct_size(hwnd, lparam);
                self.forward(hwnd, msg, wparam, lparam)
            }
            WM_WINDOWPOSCHANGED => {
                let result = self.forward(hwnd, msg, wparam, lparam);
                self.note_moved(hwnd, lparam);
                result
            }
            WM_GETMINMAXINFO => self.min_max(hwnd, wparam, lparam),
            WM_DPICHANGED => {
                self.in_dpi_change.set(true);
                let result = self.forward(hwnd, msg, wparam, lparam);
                self.in_dpi_change.set(false);
                self.load_icons(hwnd);
                self.emit(hwnd, CaptionEvent::ScaleChanged);
                result
            }
            WM_SIZE => {
                let result = self.forward(hwnd, msg, wparam, lparam);
                if hit != SIZE_MINIMIZED {
                    self.note_maximized(hwnd, hit == SIZE_MAXIMIZED);
                }
                result
            }
            WM_ACTIVATE => {
                let result = self.forward(hwnd, msg, wparam, lparam);
                self.note_active(hwnd, (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE);
                result
            }
            WM_ENTERSIZEMOVE => {
                self.in_size_move.set(true);
                self.forward(hwnd, msg, wparam, lparam)
            }
            WM_EXITSIZEMOVE => {
                self.in_size_move.set(false);
                let result = self.forward(hwnd, msg, wparam, lparam);
                self.emit(hwnd, CaptionEvent::Moved);
                result
            }
            // winit clears the icon when it has none of its own: keep the app's.
            WM_SETICON if lparam.0 == 0 && self.icons.get()[0] != 0 => LRESULT(0),
            WM_CREATE => {
                let result = self.forward(hwnd, msg, wparam, lparam);
                if result.0 != -1 {
                    self.created(hwnd);
                }
                result
            }
            WM_NCDESTROY => self.destroyed(hwnd, msg, wparam, lparam),
            _ if msg == menu_message() => {
                self.system_menu(hwnd, wparam, lparam);
                LRESULT(0)
            }
            _ if msg == events_message() => {
                self.flush();
                LRESULT(0)
            }
            _ => self.forward(hwnd, msg, wparam, lparam),
        }
    }

    /// `WM_NCCALCSIZE`: Windows' frame, less the caption and the top border.
    fn calc_size(&self, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: with wParam TRUE, lParam points to an NCCALCSIZE_PARAMS for the call's time.
        let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
        let window = params.rgrc[0];
        // SAFETY: the default handling, on the same arguments.
        let result = unsafe { DefWindowProcW(hwnd, WM_NCCALCSIZE, wparam, lparam) };
        // SAFETY: plain queries.
        let (minimized, maximized) =
            unsafe { (IsIconic(hwnd).as_bool(), IsZoomed(hwnd).as_bool()) };
        if minimized {
            return result;
        }
        let client = &mut params.rgrc[0];
        if maximized {
            // A maximized window hangs off the screen by its frame, the same at every edge.
            client.top = window.top + (window.bottom - client.bottom);
            leave_edge_for_hidden_taskbar(hwnd, client);
        } else {
            client.top = window.top;
        }
        LRESULT(0)
    }

    /// `WM_NCHITTEST`: the top resize band, the maximize button, else Windows' answer.
    fn hit_test(&self, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let default = self.forward(hwnd, WM_NCHITTEST, wparam, lparam);
        if default.0 as u32 != HTCLIENT {
            return default;
        }
        let mut point = lparam_point(lparam);
        // SAFETY: converts `point` in place.
        if !unsafe { ScreenToClient(hwnd, &mut point) }.as_bool() {
            return default;
        }
        // SAFETY: plain query.
        let maximized = unsafe { IsZoomed(hwnd) }.as_bool();
        if !maximized && window_style(hwnd).contains(WS_THICKFRAME) {
            let band = resize_band(hwnd);
            if point.y < band {
                let mut client = RECT::default();
                // SAFETY: writes only to `client`.
                let _ = unsafe { GetClientRect(hwnd, &mut client) };
                let hit = if point.x < band {
                    HTTOPLEFT
                } else if point.x >= client.right - band {
                    HTTOPRIGHT
                } else {
                    HTTOP
                };
                return LRESULT(hit as isize);
            }
        }
        if self.max_rect.get().is_some_and(|rect| rect.contains(point)) {
            return LRESULT(HTMAXBUTTON as isize);
        }
        default
    }

    /// The mouse is over the maximize button (`over`) or not.
    fn hover_max(&self, hwnd: HWND, over: bool) {
        if over && !self.tracking_leave.get() {
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE | TME_NONCLIENT,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: `track` is filled in and lives through the call.
            if unsafe { TrackMouseEvent(&mut track) }.is_ok() {
                self.tracking_leave.set(true);
            }
        }
        if self.max_hover.replace(over) != over {
            self.emit(hwnd, CaptionEvent::MaxHover(over));
        }
        // Released somewhere the window did not hear of: no longer pressed.
        if over && self.max_armed.get() && !left_button_down() {
            self.max_armed.set(false);
        }
        // As on Windows' own buttons: pressed shows only while the mouse is over the button.
        self.show_max_pressed(hwnd, over && self.max_armed.get());
    }

    fn show_max_pressed(&self, hwnd: HWND, pressed: bool) {
        if self.max_shown_pressed.replace(pressed) != pressed {
            self.emit(hwnd, CaptionEvent::MaxPressed(pressed));
        }
    }

    fn disarm_max(&self, hwnd: HWND) {
        self.max_armed.set(false);
        self.show_max_pressed(hwnd, false);
    }

    /// The caption press [`start_drag`] posted: Windows' move loop (until the button is let go),
    /// then the release the loop took, for Slint, which saw the press.
    fn drag(&self, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let before = window_rect(hwnd);
        let result = self.forward(hwnd, WM_NCLBUTTONDOWN, wparam, lparam);
        if window_rect(hwnd) != before {
            // The window moved: the next press is a first one.
            self.last_press.set(None);
        }
        let mut pending = MSG::default();
        // SAFETY: looks at (does not take) this window's queued button releases.
        let release_queued = unsafe {
            PeekMessageW(
                &mut pending,
                Some(hwnd),
                WM_LBUTTONUP,
                WM_LBUTTONUP,
                PM_NOREMOVE,
            )
        }
        .as_bool();
        if !left_button_down() && !release_queued {
            let mut cursor = POINT::default();
            // SAFETY: plain queries writing only to `cursor`.
            unsafe {
                let _ = GetCursorPos(&mut cursor);
                let _ = ScreenToClient(hwnd, &mut cursor);
            }
            post(
                hwnd.0 as isize,
                WM_LBUTTONUP,
                WPARAM(0),
                point_lparam(cursor),
            );
        }
        result
    }

    /// Whether a caption press at `at` (screen), with the window at `rect`, is the second of a
    /// double-click. A third press starts over.
    fn second_press(&self, at: POINT, rect: Option<RECT>) -> bool {
        let now = Instant::now();
        let rect = rect.unwrap_or_default();
        let Some((then, was, was_rect)) = self.last_press.replace(Some((now, at, rect))) else {
            return false;
        };
        // SAFETY: plain queries.
        let (time, width, height) = unsafe {
            (
                GetDoubleClickTime(),
                GetSystemMetrics(SM_CXDOUBLECLK),
                GetSystemMetrics(SM_CYDOUBLECLK),
            )
        };
        let double = now.duration_since(then) <= Duration::from_millis(u64::from(time))
            && (at.x - was.x).abs() <= width / 2
            && (at.y - was.y).abs() <= height / 2
            && rect == was_rect;
        if double {
            self.last_press.set(None);
        }
        double
    }

    /// `WM_WINDOWPOSCHANGING`: winit's size requests, made from client sizes as if the system
    /// caption were there, lose its height. They are its `set_size` (the one call with exactly
    /// these flags) and the resize of a DPI change.
    fn correct_size(&self, hwnd: HWND, lparam: LPARAM) {
        const WINIT_SET_SIZE: SET_WINDOW_POS_FLAGS = SET_WINDOW_POS_FLAGS(
            SWP_ASYNCWINDOWPOS.0
                | SWP_NOZORDER.0
                | SWP_NOOWNERZORDER.0
                | SWP_NOMOVE.0
                | SWP_NOACTIVATE.0,
        );
        if self.own_move.get() {
            return;
        }
        // SAFETY: lParam points to the WINDOWPOS being decided, for the call's time.
        let pos = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };
        if pos.flags.contains(SWP_NOSIZE) {
            return;
        }
        if self.in_dpi_change.get() || pos.flags.contains(WINIT_SET_SIZE) {
            pos.cy = (pos.cy - caption_excess(hwnd)).max(1);
        }
    }

    /// `WM_GETMINMAXINFO`: winit's minimum and maximum sizes lose the caption's height too.
    fn min_max(&self, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let info = lparam.0 as *mut MINMAXINFO;
        // SAFETY: lParam points to a MINMAXINFO for the call's time.
        let (min, max) = unsafe { ((*info).ptMinTrackSize, (*info).ptMaxTrackSize) };
        let result = self.forward(hwnd, WM_GETMINMAXINFO, wparam, lparam);
        let excess = caption_excess(hwnd);
        // SAFETY: as above.
        unsafe {
            if (*info).ptMinTrackSize != min {
                (*info).ptMinTrackSize.y = ((*info).ptMinTrackSize.y - excess).max(1);
            }
            if (*info).ptMaxTrackSize != max {
                (*info).ptMaxTrackSize.y = ((*info).ptMaxTrackSize.y - excess).max(1);
            }
        }
        result
    }

    fn note_moved(&self, hwnd: HWND, lparam: LPARAM) {
        // SAFETY: lParam points to the WINDOWPOS that was applied, for the call's time.
        let flags = unsafe { (*(lparam.0 as *const WINDOWPOS)).flags };
        let changed = !flags.contains(SWP_NOMOVE) || !flags.contains(SWP_NOSIZE);
        // SAFETY: plain queries.
        let shown = unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() };
        if changed && shown && !self.in_size_move.get() {
            self.emit(hwnd, CaptionEvent::Moved);
        }
    }

    fn note_maximized(&self, hwnd: HWND, maximized: bool) {
        if self.last_maximized.replace(Some(maximized)) != Some(maximized) {
            self.emit(hwnd, CaptionEvent::Maximized(maximized));
        }
    }

    fn note_active(&self, hwnd: HWND, active: bool) {
        if self.last_active.replace(Some(active)) != Some(active) {
            self.emit(hwnd, CaptionEvent::Active(active));
        }
    }

    /// After `WM_CREATE` (the window was hooked as it was created): the shadow's top edge, the
    /// icons, and the place it was prepared for.
    fn created(&self, hwnd: HWND) {
        self.decorate(hwnd);
        if let Some(place) = self.place.take() {
            self.put(hwnd, &place);
        }
    }

    /// The frame's top edge drawn by DWM (one pixel into the client area), and the app icon.
    fn decorate(&self, hwnd: HWND) {
        let margins = MARGINS {
            cyTopHeight: 1,
            ..Default::default()
        };
        // SAFETY: `margins` lives through the call.
        let _ = unsafe { DwmExtendFrameIntoClientArea(hwnd, &margins) };
        self.load_icons(hwnd);
    }

    /// Puts the window's outer rectangle at `place`. Twice if the first move took it to a monitor
    /// of another DPI and winit resized it for that.
    fn put(&self, hwnd: HWND, place: &WindowPlacement) {
        let target = RECT {
            left: place.x,
            top: place.y,
            right: place.x.saturating_add_unsigned(place.width),
            bottom: place.y.saturating_add_unsigned(place.height),
        };
        self.own_move.set(true);
        for _ in 0..2 {
            // SAFETY: a window of this thread.
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    target.left,
                    target.top,
                    target.right - target.left,
                    target.bottom - target.top,
                    SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
                );
            }
            if window_rect(hwnd) == Some(target) {
                break;
            }
        }
        self.own_move.set(false);
    }

    /// The exe's icon resource 1 at the window's DPI, as its big and small icons.
    fn load_icons(&self, hwnd: HWND) {
        // SAFETY: plain query.
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        // SAFETY: the module handle of the exe itself, which lives as long as the process.
        let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
            return;
        };
        let load = |width, height| {
            // SAFETY: resource 1 of the exe, at the sizes Windows uses at this DPI.
            unsafe {
                LoadImageW(
                    Some(module.into()),
                    // MAKEINTRESOURCE(1): an id, not a pointer.
                    PCWSTR(std::ptr::without_provenance(1)),
                    IMAGE_ICON,
                    GetSystemMetricsForDpi(width, dpi),
                    GetSystemMetricsForDpi(height, dpi),
                    LR_DEFAULTCOLOR,
                )
            }
            .ok()
            .map(|icon| icon.0 as isize)
        };
        let (big, small) = (load(SM_CXICON, SM_CYICON), load(SM_CXSMICON, SM_CYSMICON));
        let (Some(big), Some(small)) = (big, small) else {
            // No icon resource (a test build): winit's stays.
            destroy_icons([big.unwrap_or(0), small.unwrap_or(0)]);
            return;
        };
        let old = self.icons.replace([big, small]);
        // SAFETY: icons this module owns, kept until replaced or the window goes.
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_BIG as usize)),
                Some(LPARAM(big)),
            );
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(ICON_SMALL as usize)),
                Some(LPARAM(small)),
            );
        }
        destroy_icons(old);
    }

    /// `WM_NCDESTROY`: winit's procedure back in place, this window forgotten, then winit's last
    /// message.
    fn destroyed(&self, hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: puts back the procedure that was in place before ours.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, self.next_raw()) };
        let key = hwnd.0 as isize;
        let _ = FRAMES.try_with(|frames| frames.borrow_mut().remove(&key));
        let result = self.forward(hwnd, msg, wparam, lparam);
        destroy_icons(self.icons.replace([0, 0]));
        result
    }

    /// The posted system menu: its items as the window's state allows, then the chosen command.
    fn system_menu(&self, hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
        let (x, y) = (wparam.0 as isize as i32, lparam.0 as i32);
        // SAFETY: the window's own system menu, shown and gone within this call.
        unsafe {
            let menu = GetSystemMenu(hwnd, false);
            if menu.is_invalid() {
                return;
            }
            let style = window_style(hwnd);
            let maximized = IsZoomed(hwnd).as_bool();
            let enable = |command: u32, on: bool| {
                let state = if on { MF_ENABLED } else { MF_GRAYED };
                let _ = EnableMenuItem(menu, command, MF_BYCOMMAND | state);
            };
            enable(SC_RESTORE, maximized);
            enable(SC_MOVE, !maximized);
            enable(SC_SIZE, !maximized && style.contains(WS_THICKFRAME));
            enable(SC_MINIMIZE, style.contains(WS_MINIMIZEBOX));
            enable(SC_MAXIMIZE, !maximized && style.contains(WS_MAXIMIZEBOX));
            let align = if GetSystemMetrics(SM_MENUDROPALIGNMENT) != 0 {
                TPM_RIGHTALIGN
            } else {
                TPM_LEFTALIGN
            };
            let command = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | align,
                x,
                y,
                None,
                hwnd,
                None,
            );
            if command.0 != 0 {
                post(
                    hwnd.0 as isize,
                    WM_SYSCOMMAND,
                    WPARAM(command.0 as usize),
                    LPARAM(0),
                );
            }
        }
    }

    /// Queues `event` for the app; the queue is delivered from a posted message, so the listener
    /// never runs inside a call into Slint or winit.
    fn emit(&self, hwnd: HWND, event: CaptionEvent) {
        let first = {
            let mut events = self.events.borrow_mut();
            if event == CaptionEvent::Moved && events.contains(&event) {
                return;
            }
            events.push_back(event);
            events.len() == 1
        };
        if first {
            post(hwnd.0 as isize, events_message(), WPARAM(0), LPARAM(0));
        }
    }

    fn flush(&self) {
        let listener = self.on_event.borrow().clone();
        loop {
            let Some(event) = self.events.borrow_mut().pop_front() else {
                break;
            };
            if let Some(listener) = &listener {
                listener(event);
            }
        }
    }
}

/// The hooked windows' procedure.
unsafe extern "system" fn frame_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match frame_of(hwnd.0 as isize) {
        Some(frame) => frame.handle(hwnd, msg, wparam, lparam),
        // SAFETY: default handling of a message to a window of this thread.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The one-shot CBT hook of [`prepare_window`]: hooks the first top-level winit window created.
unsafe extern "system" fn cbt_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HCBT_CREATEWND as i32 {
        let hwnd = HWND(wparam.0 as *mut c_void);
        // SAFETY: for HCBT_CREATEWND, lParam points to a CBT_CREATEWNDW (and it to the
        // CREATESTRUCTW, which may be changed) for the call's time.
        let create = unsafe { &mut *(lparam.0 as *mut CBT_CREATEWNDW) };
        if is_app_window(hwnd, create)
            && let Some(prepared) = PREPARED.with(|prepared| prepared.borrow_mut().take())
        {
            // SAFETY: the hook this thread set; unhooking from inside it is allowed.
            unsafe {
                let _ = UnhookWindowsHookEx(prepared.hook);
            }
            if let Some(place) = prepared.place {
                // Created there, the window gets that monitor's DPI from the start.
                // SAFETY: as above.
                let cs = unsafe { &mut *create.lpcs };
                cs.x = place.x;
                cs.y = place.y;
                cs.cx = i32::try_from(place.width).unwrap_or(i32::MAX);
                cs.cy = i32::try_from(place.height).unwrap_or(i32::MAX);
            }
            let _ = subclass(hwnd, prepared.place);
        }
    }
    // SAFETY: passes the call on, as every hook must.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// A top-level window of winit's class.
fn is_app_window(hwnd: HWND, create: &CBT_CREATEWNDW) -> bool {
    // SAFETY: valid for the hook call's time (see `cbt_proc`).
    let cs = unsafe { &*create.lpcs };
    if !cs.hwndParent.is_invalid() || cs.style & WS_CHILD.0 as i32 != 0 {
        return false;
    }
    let mut name = [0u16; 64];
    // SAFETY: writes at most `name.len()` units.
    let len = unsafe { GetClassNameW(hwnd, &mut name) };
    usize::try_from(len).is_ok_and(|len| String::from_utf16_lossy(&name[..len]) == WINIT_CLASS)
}

/// Stands `frame_proc` in front of `hwnd`'s window procedure.
fn subclass(hwnd: HWND, place: Option<WindowPlacement>) -> Option<Rc<Frame>> {
    let key = hwnd.0 as isize;
    let frame = Rc::new(Frame::new(place));
    FRAMES.with(|frames| frames.borrow_mut().insert(key, frame.clone()));
    // SAFETY: a window of this thread, and a procedure of the WNDPROC signature.
    let next = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, frame_proc as *const () as isize) };
    if next == 0 {
        FRAMES.with(|frames| frames.borrow_mut().remove(&key));
        return None;
    }
    // SAFETY: what GWLP_WNDPROC held is a window procedure.
    frame
        .next
        .set(unsafe { std::mem::transmute::<isize, WNDPROC>(next) });
    Some(frame)
}

fn frame_of(hwnd: isize) -> Option<Rc<Frame>> {
    FRAMES
        .try_with(|frames| frames.borrow().get(&hwnd).cloned())
        .ok()
        .flatten()
}

fn post(hwnd: isize, msg: u32, wparam: WPARAM, lparam: LPARAM) {
    // SAFETY: posting copies the values; nothing is borrowed.
    unsafe {
        let _ = PostMessageW(Some(as_hwnd(hwnd)), msg, wparam, lparam);
    }
}

fn registered(name: PCWSTR, id: &OnceLock<u32>) -> u32 {
    // SAFETY: `name` is a NUL-terminated literal.
    *id.get_or_init(|| unsafe { RegisterWindowMessageW(name) })
}

/// Posted to show the system menu (wParam x, lParam y, screen pixels).
fn menu_message() -> u32 {
    static ID: OnceLock<u32> = OnceLock::new();
    registered(w!("HTTPorter.TitleBar.SystemMenu"), &ID)
}

/// Posted to deliver the queued [`CaptionEvent`]s.
fn events_message() -> u32 {
    static ID: OnceLock<u32> = OnceLock::new();
    registered(w!("HTTPorter.TitleBar.Events"), &ID)
}

fn window_style(hwnd: HWND) -> WINDOW_STYLE {
    // SAFETY: plain query.
    WINDOW_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32)
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    // SAFETY: writes only to `rect`.
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect)
}

/// How much taller winit takes the frame to be than it is: the system caption and top border it
/// counts, less the top inset this module keeps (a maximized window's).
fn caption_excess(hwnd: HWND) -> i32 {
    // SAFETY: plain queries.
    let (style, ex_style, dpi, maximized) = unsafe {
        (
            window_style(hwnd),
            WINDOW_EX_STYLE(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32),
            GetDpiForWindow(hwnd),
            IsZoomed(hwnd).as_bool(),
        )
    };
    let mut rect = RECT::default();
    // SAFETY: writes only to `rect`.
    if unsafe { AdjustWindowRectExForDpi(&mut rect, style, false, ex_style, dpi) }.is_err() {
        return 0;
    }
    let top = -rect.top;
    if maximized { top - rect.bottom } else { top }
}

/// How high the top resize band is: Windows' resize border at the window's DPI.
fn resize_band(hwnd: HWND) -> i32 {
    // SAFETY: plain queries.
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

/// A maximized window covers its monitor's work area, which includes an auto-hidden taskbar's
/// edge: leave 2 px there so the taskbar can still come out.
fn leave_edge_for_hidden_taskbar(hwnd: HWND, client: &mut RECT) {
    let mut state = APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    // SAFETY: `state` is filled in and lives through the call.
    if unsafe { SHAppBarMessage(ABM_GETSTATE, &mut state) } as u32 & ABS_AUTOHIDE == 0 {
        return;
    }
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: plain queries writing only to `info`.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return;
    }
    for edge in [ABE_TOP, ABE_BOTTOM, ABE_LEFT, ABE_RIGHT] {
        let mut bar = APPBARDATA {
            cbSize: size_of::<APPBARDATA>() as u32,
            uEdge: edge,
            rc: info.rcMonitor,
            ..Default::default()
        };
        // SAFETY: as above.
        if unsafe { SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut bar) } == 0 {
            continue;
        }
        match edge {
            ABE_TOP => client.top += 2,
            ABE_BOTTOM => client.bottom -= 2,
            ABE_LEFT => client.left += 2,
            _ => client.right -= 2,
        }
    }
}

fn destroy_icons(icons: [isize; 2]) {
    for icon in icons.into_iter().filter(|icon| *icon != 0) {
        // SAFETY: an icon this module loaded and no longer uses.
        unsafe {
            let _ = DestroyIcon(HICON(icon as *mut c_void));
        }
    }
}

/// The primary button is down (as this thread's messages last said).
fn left_button_down() -> bool {
    // SAFETY: plain query.
    (unsafe { GetKeyState(i32::from(VK_LBUTTON.0)) }) < 0
}

/// A point packed as mouse messages carry it (16-bit signed x and y).
fn point_lparam(point: POINT) -> LPARAM {
    let (x, y) = (point.x as i16 as u16 as u32, point.y as i16 as u16 as u32);
    LPARAM((y << 16 | x) as isize)
}

fn lparam_point(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xFFFF) as u16 as i16 as i32,
        y: ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_pack_as_mouse_messages_carry_them() {
        for (x, y) in [(0, 0), (12, 34), (-8, -31), (3839, 2159), (-1920, 1080)] {
            let point = POINT { x, y };
            assert_eq!(lparam_point(point_lparam(point)), point);
        }
    }

    #[test]
    fn the_max_rect_holds_its_left_and_top_edges_only() {
        let rect = CaptionRect {
            left: 10,
            top: 0,
            right: 56,
            bottom: 52,
        };
        assert!(rect.contains(POINT { x: 10, y: 0 }));
        assert!(rect.contains(POINT { x: 55, y: 51 }));
        assert!(!rect.contains(POINT { x: 56, y: 10 }));
        assert!(!rect.contains(POINT { x: 9, y: 10 }));
        assert!(!rect.contains(POINT { x: 20, y: 52 }));
    }
}
