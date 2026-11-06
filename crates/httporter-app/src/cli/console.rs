// SPDX-License-Identifier: MIT OR Apache-2.0

//! The CLI's console: sharing the terminal it was run from, the progress lines, and Ctrl+C.
//!
//! - **The console.** A release `httporter.exe` is a GUI-subsystem program with no console of its
//!   own. Run by `httporter.com` (or a terminal), [`attach`] joins its parent's, so that what it
//!   prints shows there; the handles `httporter.com` passed down (a pipe or a file, when the
//!   output is redirected) are kept. Run by the Task Scheduler there is none, and nothing shows.
//! - **Progress** goes to stderr only when stderr is a console, at most twice a second
//!   ([`Progress`]); results go to stdout.
//! - **Ctrl+C.** [`Cancel::install`] creates the named event `httporter.com` sets on the first
//!   Ctrl+C (`cancel_event_name`), and handles Ctrl+C itself too (a debug build shares the
//!   console, and so does a release one once attached): the first sets the event, a second is
//!   left to Windows, which ends the process.

use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    ATTACH_PARENT_PROCESS, AttachConsole, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForSingleObject,
};
use windows_sys::core::BOOL;

/// Progress lines are at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);

/// Joins the console of the process that started this one, if it has one and this has none.
pub fn attach() {
    // SAFETY: a plain call; it fails harmlessly when there is no parent console, or when this
    // process has one already (a debug build).
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Whether stderr is a console (not redirected to a file or a pipe, and not missing).
fn stderr_is_console() -> bool {
    // SAFETY: plain calls; the handle is only read, never closed.
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut mode = 0;
        GetConsoleMode(handle, &mut mode) != 0
    }
}

/// A line on stdout. A closed pipe (`| more` quit early) loses it rather than panicking.
pub fn out(line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
}

/// A line on stderr.
pub fn err(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
}

/// The progress lines of a command: on stderr when it is a console, at most twice a second, and
/// never the same line twice in a row.
pub struct Progress {
    on: bool,
    last: Mutex<Option<(Instant, String)>>,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            on: stderr_is_console(),
            last: Mutex::new(None),
        }
    }

    pub fn show(&self, line: String) {
        if !self.on {
            return;
        }
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, shown)) = &*last
            && (*shown == line || at.elapsed() < PROGRESS_EVERY)
        {
            return;
        }
        err(&line);
        *last = Some((Instant::now(), line));
    }
}

/// The cancel event, once made (as a number: a handle isn't `Send`); 0 before.
static EVENT: AtomicUsize = AtomicUsize::new(0);
/// How many console control events (Ctrl+C, Ctrl+Break, close) arrived.
static PRESSES: AtomicU32 = AtomicU32::new(0);

/// Ctrl+C for the CLI: [`Cancel::fired`] ends once `httporter.com` or this process's own console
/// handler sets the cancel event.
pub struct Cancel {
    fired: Arc<watch::Sender<bool>>,
}

impl Cancel {
    /// Makes the cancel event `cancel_event_name(<this process id>)` and the console handler, and
    /// a thread that waits for the event. Without the event (Windows wouldn't make it) Ctrl+C is
    /// left to Windows, and `httporter.com` stops this process at once.
    pub fn install() -> Self {
        let fired = Arc::new(watch::channel(false).0);
        let name: Vec<u16> = super::cancel_event_name(std::process::id())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: a NUL-terminated name that outlives the call; no security attributes; manual
        // reset, not set. The handle is kept for the life of the process.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
        if event.is_null() {
            return Self { fired };
        }
        EVENT.store(event as usize, Ordering::SeqCst);
        // SAFETY: `on_control` is a plain function with the signature Windows calls; it only
        // touches the statics above.
        unsafe { SetConsoleCtrlHandler(Some(on_control), 1) };
        let (sender, raw) = (fired.clone(), event as usize);
        let _ = std::thread::Builder::new()
            .name("cli-cancel".into())
            .spawn(move || {
                // SAFETY: the event handle stays open for the life of the process.
                unsafe { WaitForSingleObject(raw as HANDLE, INFINITE) };
                sender.send_replace(true);
            });
        Self { fired }
    }

    /// Ends once Ctrl+C was pressed (never, without the event).
    pub async fn fired(&self) {
        let mut fired = self.fired.subscribe();
        // The sender lives as long as `self`: this fails only if that is gone.
        if fired.wait_for(|fired| *fired).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The console control handler: the first event sets the cancel event (a clean stop); a later
/// one goes to Windows' own handling, which ends the process.
unsafe extern "system" fn on_control(_kind: u32) -> BOOL {
    if PRESSES.fetch_add(1, Ordering::SeqCst) == 0 {
        let event = EVENT.load(Ordering::SeqCst);
        if event != 0 {
            // SAFETY: the event handle stays open for the life of the process.
            unsafe { SetEvent(event as HANDLE) };
            return 1;
        }
    }
    0
}
