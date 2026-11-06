// SPDX-License-Identifier: MIT OR Apache-2.0

//! The command line (§8.8.1, I6): `httporter.exe --cli <args…>` runs one command without a window
//! and exits with its code (`httporter_core::exit`). `httporter.com` (the `httporter-com` crate)
//! starts it that way from a terminal, with stdin, stdout and stderr inherited, so typing
//! `httporter publish …` works in cmd and PowerShell. A scheduled publish runs
//! `httporter.exe --cli publish --profile <id> --scheduled` itself (no console window flashes).
//!
//! `main` calls [`run`] before anything else (no Slint, no single-instance check, no reset), so
//! the CLI can run while the window is open. A publish takes the profile's lock
//! (`win::ProfileLock`) instead, as the window's Publish does; the CLI's journal rows carry this
//! process as their owner, so neither offers to resume the other's. A reset that is pending
//! (confirmed, not yet run at a start) makes every command but `--help` and `--version` refuse,
//! with exit 2.
//!
//! **Ctrl+C.** `httporter.com` receives it and sets the named event
//! `Local\HTTPorter.Cli.Cancel.<this process id>` ([`cancel_event_name`]); the CLI creates that
//! event at start and waits on it alongside its work ([`console::Cancel`]). The work then stops
//! cleanly: its batches are cancelled, a publish ends Cancelled in the history, and the journal
//! catches up before the process exits (1).
//!
//! The parts:
//! - this file: the order of things;
//! - [`args`]: reading the arguments;
//! - [`console`]: the console, the progress lines and Ctrl+C;
//! - [`commands`]: the commands;
//! - [`copy`]: every word the CLI prints.

mod args;
mod commands;
mod console;
mod copy;

use std::ffi::OsString;
use std::time::Duration;

use httporter_core::exit;

use crate::publish::headless::HeadlessCtx;
use args::Command;
use console::err;

/// The prefix of the cancel event's name; the CLI's process id follows. `httporter.com` names it
/// the same way (`httporter-com`'s `CANCEL_EVENT_PREFIX`): change both or neither.
pub const CANCEL_EVENT_PREFIX: &str = "Local\\HTTPorter.Cli.Cancel.";

/// The cancel event's name for the CLI running as process `pid`.
pub fn cancel_event_name(pid: u32) -> String {
    format!("{CANCEL_EVENT_PREFIX}{pid}")
}

/// Runs the command in `args` (what followed `--cli`) and returns the process's exit code.
pub fn run(args: Vec<OsString>) -> i32 {
    console::attach();
    let command = match args::parse(&args) {
        Ok(command) => command,
        Err(e) => {
            err(&copy::usage_error(&e));
            return exit::CONFIG;
        }
    };
    match command {
        Command::Version => {
            console::out(&copy::version());
            return exit::OK;
        }
        Command::Help(topic) => {
            console::out(&copy::help(topic));
            return exit::OK;
        }
        _ => {}
    }
    if crate::system::reset::pending() {
        err(&copy::error(copy::RESET_PENDING));
        return exit::CONFIG;
    }
    let cancel = console::Cancel::install();
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("httporter-cli")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            err(&copy::error(&copy::runtime_failed(e)));
            return exit::FAILURES;
        }
    };
    let opened = {
        let _inside = rt.enter();
        HeadlessCtx::open(rt.handle().clone())
    };
    let ctx = match opened {
        Ok(ctx) => ctx,
        Err(e) => {
            err(&copy::error(e.text()));
            return e.exit_code();
        }
    };
    let code = rt.block_on(async {
        let code = commands::run(&ctx, command, &cancel).await;
        ctx.finish().await;
        code
    });
    drop(ctx);
    rt.shutdown_timeout(Duration::from_secs(1));
    code
}
