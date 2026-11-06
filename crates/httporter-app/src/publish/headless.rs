// SPDX-License-Identifier: MIT OR Apache-2.0

//! The headless publish (I6): compare → plan → guards → run → purge → history, without a window.
//! The CLI, the scheduled publish (which runs the CLI) and the watch folder use it, and nothing
//! else does, so the three behave the same. The Publish button keeps its sheet.
//!
//! T2 builds it from the code the Publish sheet uses (factored out of `sheet_view`, not copied);
//! T4 calls it from the watch folder. T1 is the skeleton: the types are final, the bodies are
//! T2's.
//!
//! - The delete guard: a run that would trip it deletes nothing unless `opts.yes`; its uploads
//!   still run ([`HeadlessOutcome::GuardTripped`]).
//! - One publish per profile at a time, across processes (`win::ProfileLock`):
//!   [`HeadlessOutcome::Busy`] when another holds it.
//! - The history row records `opts.source`; the journal rows carry this process as their owner.

#![allow(dead_code)] // T2 and T4 use these.

use std::sync::Arc;

use httporter_core::{HeadlessOptions, HeadlessOutcome, Journal, ProfileRecord, exit};
use httporter_transfer::Engine;

use super::Publisher;
use crate::session::Session;

/// What a headless publish works with: the session (the site database, the credentials as the
/// window reads them, the storage clients), the transfer engine and its journal.
pub struct HeadlessCtx {
    pub(crate) session: Arc<Session>,
    pub(crate) engine: Engine,
    pub(crate) journal: Arc<dyn Journal>,
}

impl HeadlessCtx {
    /// For the CLI: a session over the default database with the Credential Manager and the real
    /// providers, an engine on `rt`, and a journal whose rows carry this process as their owner
    /// (`httporter_platform::process_owner`).
    pub fn open(rt: tokio::runtime::Handle) -> Result<HeadlessCtx, HeadlessError> {
        let _ = rt;
        todo!("T2")
    }

    /// For the window (the watch folder): the publisher's session, engine and journal.
    pub fn from_publisher(p: &Arc<Publisher>) -> HeadlessCtx {
        let _ = p;
        todo!("T2")
    }
}

/// How far a headless publish is, for the CLI's progress line (stderr, at most twice a second).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessProgress {
    Comparing { done: u64, total: u64 },
    Uploading { done: u64, total: u64, bytes: u64 },
    Deleting { done: u64, total: u64 },
    Purging,
}

/// Why a headless publish couldn't run. Each maps to an exit code ([`HeadlessError::exit_code`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessError {
    /// The profile, its site or its folder is wrong or missing (exit 2).
    Config(String),
    /// The site's credentials are missing or wrong (exit 3), with the window's words for it.
    Auth(String),
    /// Reading the folder or the database failed (exit 1).
    Io(String),
}

impl HeadlessError {
    /// The CLI's exit code for it.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) => exit::CONFIG,
            Self::Auth(_) => exit::AUTH,
            Self::Io(_) => exit::FAILURES,
        }
    }
}

/// Publishes `profile` as `opts` says: compares the folder with the site, plans, checks the
/// guards, uploads, deletes (unless held back), purges and stores the history row. `progress` is
/// called as it goes, from the engine's tasks.
pub async fn run_headless(
    ctx: &HeadlessCtx,
    profile: &ProfileRecord,
    opts: &HeadlessOptions,
    progress: &(dyn Fn(HeadlessProgress) + Send + Sync),
) -> Result<HeadlessOutcome, HeadlessError> {
    let _ = (ctx, profile, opts, progress);
    todo!("T2")
}
