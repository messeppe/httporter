//! Moving and renaming files and folders on a site (T3): a server-side copy where the provider
//! has one, else download and upload again; the originals are deleted only once every copy is
//! made.
#![allow(unused_variables, dead_code)]

use std::path::Path;

use httporter_core::{Entry, JournalError, ProviderError, RemotePath, StorageProvider};

use crate::{BatchTarget, Engine};

/// One file to move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveItem {
    pub from: RemotePath,
    pub to: RemotePath,
    pub size: u64,
}

/// What a move does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MovePlan {
    /// The files.
    pub items: Vec<MoveItem>,
    /// The folders, from and to.
    pub folders: Vec<(RemotePath, RemotePath)>,
    pub bytes: u64,
    /// `caps.server_side_copy` and every file ≤ 5 GiB.
    pub server_side: bool,
}

/// Plans moving `sources` into `to_dir`. Folders expand via `walk()`; `rename_to` is the new
/// last segment (one source). An existing target is `Conflict`; a folder into itself is refused.
pub async fn plan_move(
    provider: &dyn StorageProvider,
    sources: &[Entry],
    to_dir: &RemotePath,
    rename_to: Option<&str>,
) -> Result<MovePlan, ProviderError> {
    todo!("T3")
}

/// About how long `plan` takes, in seconds, at these rates.
pub fn estimate_secs(plan: &MovePlan, files_per_s: f64, bytes_per_s: f64) -> u64 {
    todo!("T3")
}

/// Where a running move is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovePhase {
    Copying { done: u64, total: u64 },
    Deleting,
    Finished,
}

/// How a move ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveOutcome {
    pub copied: u64,
    pub deleted: u64,
    pub failed: Vec<(RemotePath, String)>,
    /// The copies made when some failed (the originals were kept).
    pub kept_copies: Vec<RemotePath>,
}

/// Runs `plan`. Server-side: `provider.copy`, 8 at a time. Else a download batch into `temp_dir`,
/// then an upload batch (checksums), each `submit_tracked` + `wait_batch`. One delete batch, only
/// if every copy succeeded; else nothing is deleted and `kept_copies` lists the copies. Bunny:
/// empty folders are created. `temp_dir` is emptied at the end.
pub async fn run_move(
    engine: &Engine,
    target: BatchTarget,
    plan: &MovePlan,
    temp_dir: &Path,
    on_phase: &(dyn Fn(MovePhase) + Send + Sync),
) -> Result<MoveOutcome, JournalError> {
    todo!("T3")
}
