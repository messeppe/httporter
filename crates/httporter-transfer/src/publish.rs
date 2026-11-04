//! A publish: the plan made from a compare, and running it as engine batches (uploads, then
//! deletes, then the CDN purge).

// T1 skeleton: drop this once T2 and T3 fill the bodies.
#![allow(unused_variables)]

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use httporter_core::{
    CdnPurger, CompareStatus, HistoryFile, HistoryRecord, Job, JournalError, ProfileId,
    PublishResult, PurgeMode, PurgeResult, RemotePath, SiteId, StorageProvider,
};

use crate::compare::CompareResult;
use crate::engine::Engine;

/// One file of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    pub rel: String,
    pub status: CompareStatus,
    pub size: u64,
    pub include: bool,
}

/// What a publish will upload and delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishPlan {
    pub local_root: PathBuf,
    pub remote_root: RemotePath,
    /// No Same items.
    pub items: Vec<PlanItem>,
    pub same: u64,
    pub remote_files: u64,
    pub remote_folders: u64,
    pub regions: u32,
}

impl PublishPlan {
    /// Changed, New and RemoteOnly items, then by `rel`.
    pub fn from_compare(r: &CompareResult, remove_extras: bool) -> Self {
        todo!("T2")
    }

    /// RemoteOnly items' `include` = `on`.
    pub fn set_remove_extras(&mut self, on: bool) {
        todo!("T2")
    }

    pub fn uploads(&self) -> Vec<Job> {
        todo!("T2")
    }

    /// One file job per delete (Q54).
    pub fn deletes(&self) -> Vec<Job> {
        todo!("T2")
    }

    /// (files, bytes).
    pub fn upload_totals(&self) -> (u64, u64) {
        todo!("T2")
    }

    pub fn delete_count(&self) -> u64 {
        todo!("T2")
    }

    /// The included items, as full remote paths.
    pub fn touched_paths(&self) -> Vec<RemotePath> {
        todo!("T2")
    }

    pub fn export_text(&self, title: &str, destination: &str) -> String {
        todo!("T2")
    }

    pub fn retry_of(local_root: PathBuf, remote_root: RemotePath, failed: &[HistoryFile]) -> Self {
        todo!("T2")
    }
}

/// The site a publish's batches go to.
#[derive(Clone)]
pub struct BatchTarget {
    pub site: SiteId,
    pub site_name: String,
    pub host: String,
    pub provider: Arc<dyn StorageProvider>,
}

/// Where a running publish is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishPhase {
    Uploading,
    Deleting,
    Purging { done: usize, total: usize },
    Finished,
}

/// How a publish went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishOutcome {
    pub result: PublishResult,
    pub files: Vec<HistoryFile>,
    pub uploaded: u64,
    pub deleted: u64,
    pub failed: u64,
    pub bytes: u64,
    pub purge: PurgeResult,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

impl PublishOutcome {
    pub fn into_record(
        self,
        profile: ProfileId,
        plan: &PublishPlan,
        verified: bool,
    ) -> HistoryRecord {
        todo!("T3")
    }
}

/// Uploads (`submit_tracked` + `wait_batch`), then deletes, only if no upload failed (else
/// `NotRun("Not deleted because uploads failed.")`), then `run_purge` on the paths done.
/// `wait_batch` includes the Q55 late-delete checks, so the purge never starts before them. A
/// purge failure never fails the publish.
pub async fn run_publish(
    engine: &Engine,
    target: BatchTarget,
    plan: &PublishPlan,
    purge: PurgeMode,
    purger: Option<Arc<dyn CdnPurger>>,
    on_phase: &(dyn Fn(PublishPhase) + Send + Sync),
) -> Result<PublishOutcome, JournalError> {
    todo!("T3")
}
