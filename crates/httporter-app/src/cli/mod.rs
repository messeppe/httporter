// SPDX-License-Identifier: MIT OR Apache-2.0

//! The command line (§8.8.1, I6): `httporter.exe --cli <args…>` runs one command without a window
//! and exits with its code (`httporter_core::exit`). `httporter.com` (the `httporter-com` crate)
//! starts it that way from a terminal, with stdin, stdout and stderr inherited, so typing
//! `httporter publish …` works in cmd and PowerShell.
//!
//! `main` calls [`run`] before anything else (no Slint, no single-instance check, no reset), so
//! the CLI can run while the window is open. A publish takes the profile's lock
//! (`win::ProfileLock`) instead.
//!
//! **Ctrl+C.** `httporter.com` receives it and sets the named event
//! `Local\HTTPorter.Cli.Cancel.<this process id>` ([`cancel_event_name`]); the CLI creates that
//! event at start and waits on it alongside its work (T2).
//!
//! The parts:
//! - this file (T2): parsing the arguments and running the commands;
//! - [`copy`]: every word the CLI prints.
//!
//! T1 is the skeleton: every command says the command line isn't built yet and exits 2.

mod copy;

use std::ffi::OsString;

use httporter_core::exit;

/// The prefix of the cancel event's name; the CLI's process id follows. `httporter.com` names it
/// the same way (`httporter-com`'s `CANCEL_EVENT_PREFIX`): change both or neither.
pub const CANCEL_EVENT_PREFIX: &str = "Local\\HTTPorter.Cli.Cancel.";

/// The cancel event's name for the CLI running as process `pid`.
#[allow(dead_code)] // T2 creates the event.
pub fn cancel_event_name(pid: u32) -> String {
    format!("{CANCEL_EVENT_PREFIX}{pid}")
}

/// Runs the command in `args` (what followed `--cli`) and returns the process's exit code.
pub fn run(args: Vec<OsString>) -> i32 {
    let _ = args;
    eprintln!("{}", copy::NOT_BUILT_YET);
    exit::CONFIG
}
