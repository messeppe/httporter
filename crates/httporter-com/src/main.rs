// SPDX-License-Identifier: MIT OR Apache-2.0

//! `httporter.com`, the console companion of `httporter.exe` (spec §8.8.1).
//!
//! `httporter.exe` is a GUI-subsystem program: a terminal neither waits for it nor shows what it
//! prints. This tiny console program does both. It starts `httporter.exe --cli <args…>` from its
//! own folder with stdin, stdout and stderr inherited, waits for it, and exits with its code.
//! Typing `httporter …` in cmd or PowerShell finds `httporter.com` first (`.com` comes before
//! `.exe` in `PATHEXT`). Every command is implemented once, in `httporter.exe`.
//!
//! **Ctrl+C.** Only this process receives it: the child has no console of its own, and
//! `GenerateConsoleCtrlEvent` cannot reach a process outside the console. So the first Ctrl+C
//! (or Ctrl+Break, or closing the console) sets the named event
//! `Local\HTTPorter.Cli.Cancel.<child pid>`, which the CLI creates and waits on alongside its
//! work, and the child stops cleanly and exits with its own code. If the child made no such
//! event, or on a second Ctrl+C, the child is stopped at once, with the code Windows gives a
//! console program stopped by Ctrl+C.

mod copy;

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Threading::{
    EVENT_MODIFY_STATE, OpenEventW, SetEvent, TerminateProcess,
};
use windows_sys::core::BOOL;

/// The prefix of the event the CLI (`httporter.exe --cli`) creates and waits on; its process id
/// follows. The CLI side names it the same way (`httporter-app`, `cli::cancel_event_name`).
const CANCEL_EVENT_PREFIX: &str = "Local\\HTTPorter.Cli.Cancel.";

/// The exit code for a configuration problem (`httporter_core::publish::exit::CONFIG`): here,
/// `httporter.exe` missing or not startable.
const EXIT_CONFIG: i32 = 2;

/// `STATUS_CONTROL_C_EXIT`: what Windows reports for a console program stopped by Ctrl+C.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// The child's process id, 0 until it runs.
static CHILD_PID: AtomicU32 = AtomicU32::new(0);
/// The child's process handle (owned by its `Child`, which outlives every use), null until it runs.
static CHILD_PROCESS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
/// How many console control events (Ctrl+C, Ctrl+Break, close) arrived since the child started.
static PRESSES: AtomicU32 = AtomicU32::new(0);

fn main() {
    let exe = match companion_exe() {
        Some(exe) => exe,
        None => {
            eprintln!("{}", copy::no_own_path());
            std::process::exit(EXIT_CONFIG);
        }
    };
    if !exe.is_file() {
        eprintln!("{}", copy::exe_missing(&exe));
        std::process::exit(EXIT_CONFIG);
    }
    let mut child = match Command::new(&exe)
        .arg("--cli")
        .args(std::env::args_os().skip(1))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!("{}", copy::exe_not_started(&exe, &error));
            std::process::exit(EXIT_CONFIG);
        }
    };
    CHILD_PROCESS.store(child.as_raw_handle(), Ordering::SeqCst);
    CHILD_PID.store(child.id(), Ordering::SeqCst);
    // SAFETY: `on_control` is a plain function with the signature Windows calls, and it only reads
    // the statics above.
    unsafe { SetConsoleCtrlHandler(Some(on_control), 1) };

    let code = match child.wait() {
        // Windows always has a code; `None` cannot happen there.
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("{}", copy::wait_failed(&error));
            1
        }
    };
    // The handler must not touch the handle once `child` may be dropped.
    CHILD_PROCESS.store(std::ptr::null_mut(), Ordering::SeqCst);
    CHILD_PID.store(0, Ordering::SeqCst);
    std::process::exit(code);
}

/// `httporter.exe` in this program's own folder.
fn companion_exe() -> Option<PathBuf> {
    Some(
        std::env::current_exe()
            .ok()?
            .with_file_name("httporter.exe"),
    )
}

/// The console control handler: the first event asks the child to stop cleanly; without its
/// cancel event, or on a second event, the child is stopped at once. Either way this process
/// keeps waiting, and exits with the child's code.
unsafe extern "system" fn on_control(_kind: u32) -> BOOL {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid == 0 {
        // No child (any more): Windows' own handling, which ends this process.
        return 0;
    }
    let first = PRESSES.fetch_add(1, Ordering::SeqCst) == 0;
    if !(first && signal_cancel(pid)) {
        stop_child();
    }
    1
}

/// Sets the CLI's cancel event for process `pid`; false when it has none.
fn signal_cancel(pid: u32) -> bool {
    let name: Vec<u16> = format!("{CANCEL_EVENT_PREFIX}{pid}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: a NUL-terminated name that outlives the call; the handle is closed below.
    let event: HANDLE = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if event.is_null() {
        return false;
    }
    // SAFETY: a valid event handle, opened above with the right to set it, closed once.
    unsafe {
        let set = SetEvent(event) != 0;
        CloseHandle(event);
        set
    }
}

/// Ends the child now, with the code of a console program stopped by Ctrl+C.
fn stop_child() {
    let process = CHILD_PROCESS.load(Ordering::SeqCst);
    if !process.is_null() {
        // SAFETY: the handle belongs to the running child's `Child`, which `main` keeps until the
        // child has ended and this handle is cleared; a child that already ended makes it fail
        // harmlessly.
        unsafe { TerminateProcess(process, STATUS_CONTROL_C_EXIT) };
    }
}
