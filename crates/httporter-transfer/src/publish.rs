//! A publish: the plan made from a compare, and running it as engine batches (uploads, then
//! deletes, then the CDN purge).

// T1 skeleton: drop this once T2 and T3 fill the bodies.
#![allow(unused_variables)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use httporter_core::{
    CdnPurger, CompareStatus, FileAction, FileOutcome, HistoryFile, HistoryRecord, Job,
    JournalError, ProfileId, PublishResult, PurgeMode, PurgeResult, RemotePath, SiteId,
    StorageProvider,
};

use crate::compare::{CompareResult, rel_of};
use crate::engine::Engine;
use crate::messages;

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
        let mut items: Vec<PlanItem> = r
            .items
            .iter()
            .filter(|item| item.status != CompareStatus::Same)
            .map(|item| {
                let remote_only = item.status == CompareStatus::RemoteOnly;
                PlanItem {
                    rel: item.rel.clone(),
                    status: item.status,
                    size: if remote_only {
                        item.remote_size
                    } else {
                        item.local_size
                    }
                    .unwrap_or(0),
                    include: !remote_only || remove_extras,
                }
            })
            .collect();
        sort_items(&mut items);
        Self {
            local_root: r.local_root.clone(),
            remote_root: r.remote_root.clone(),
            items,
            same: r
                .items
                .iter()
                .filter(|item| item.status == CompareStatus::Same)
                .count() as u64,
            remote_files: r
                .items
                .iter()
                .filter(|item| item.remote_size.is_some())
                .count() as u64,
            remote_folders: r.remote_folders,
            regions: r.regions,
        }
    }

    /// RemoteOnly items' `include` = `on`.
    pub fn set_remove_extras(&mut self, on: bool) {
        for item in &mut self.items {
            if item.status == CompareStatus::RemoteOnly {
                item.include = on;
            }
        }
    }

    pub fn uploads(&self) -> Vec<Job> {
        self.included_uploads()
            .filter_map(|item| {
                Some(Job::Upload {
                    local: local_path(&self.local_root, &item.rel),
                    remote: remote_path(&self.remote_root, &item.rel)?,
                    size: item.size,
                })
            })
            .collect()
    }

    /// One file job per delete (Q54).
    pub fn deletes(&self) -> Vec<Job> {
        self.included_deletes()
            .filter_map(|item| {
                Some(Job::Delete {
                    remote: remote_path(&self.remote_root, &item.rel)?,
                    folder: false,
                })
            })
            .collect()
    }

    /// (files, bytes).
    pub fn upload_totals(&self) -> (u64, u64) {
        self.included_uploads()
            .fold((0, 0), |(files, bytes), item| {
                (files + 1, bytes + item.size)
            })
    }

    pub fn delete_count(&self) -> u64 {
        self.included_deletes().count() as u64
    }

    /// The included items, as full remote paths.
    pub fn touched_paths(&self) -> Vec<RemotePath> {
        self.items
            .iter()
            .filter(|item| item.include)
            .filter_map(|item| remote_path(&self.remote_root, &item.rel))
            .collect()
    }

    /// The plan as text to save: a header (title, destination, folder, date, counts), then a
    /// line per file with its action, path and size in bytes, tab-separated.
    pub fn export_text(&self, title: &str, destination: &str) -> String {
        use messages::plan_export as words;

        let count = |status: CompareStatus, include: bool| {
            self.items
                .iter()
                .filter(|item| item.status == status && item.include == include)
                .count()
        };
        let (files, bytes) = self.upload_totals();
        let mut text = String::new();
        text.push_str(title);
        text.push('\n');
        for line in [
            words::DESTINATION.replace("{destination}", destination),
            words::FOLDER.replace("{folder}", &self.local_root.display().to_string()),
            words::DATE.replace(
                "{date}",
                &chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            ),
            words::COUNTS
                .replace(
                    "{changed}",
                    &count(CompareStatus::Changed, true).to_string(),
                )
                .replace("{new}", &count(CompareStatus::New, true).to_string())
                .replace("{deleted}", &self.delete_count().to_string())
                .replace(
                    "{kept}",
                    &count(CompareStatus::RemoteOnly, false).to_string(),
                )
                .replace("{same}", &self.same.to_string())
                .replace("{files}", &files.to_string())
                .replace("{bytes}", &bytes.to_string()),
        ] {
            text.push_str(&line);
            text.push('\n');
        }
        text.push('\n');
        for item in &self.items {
            let action = match (item.status, item.include) {
                (CompareStatus::Changed, true) => words::UPLOAD_CHANGED,
                (CompareStatus::Changed, false) => words::SKIP_CHANGED,
                (CompareStatus::New, true) => words::UPLOAD_NEW,
                (CompareStatus::New, false) => words::SKIP_NEW,
                (CompareStatus::RemoteOnly, true) => words::DELETE,
                (CompareStatus::RemoteOnly, false) => words::KEEP,
                (CompareStatus::Same, _) => continue,
            };
            text.push_str(&format!("{action}\t{}\t{}\n", item.rel, item.size));
        }
        text
    }

    /// A plan that tries again what a publish didn't do: its uploads as New, its deletes as
    /// RemoteOnly, all included. Files it did are left out.
    pub fn retry_of(local_root: PathBuf, remote_root: RemotePath, failed: &[HistoryFile]) -> Self {
        let mut items: Vec<PlanItem> = failed
            .iter()
            .filter(|file| file.outcome != FileOutcome::Done)
            .filter_map(|file| {
                Some(PlanItem {
                    rel: rel_of(&file.path, &remote_root)?.to_owned(),
                    status: match file.action {
                        FileAction::Upload => CompareStatus::New,
                        FileAction::Delete => CompareStatus::RemoteOnly,
                    },
                    size: file.size,
                    include: true,
                })
            })
            .collect();
        sort_items(&mut items);
        items.dedup_by(|a, b| a.rel == b.rel && a.status == b.status);
        Self {
            local_root,
            remote_root,
            items,
            same: 0,
            remote_files: 0,
            remote_folders: 0,
            regions: 0,
        }
    }

    fn included_uploads(&self) -> impl Iterator<Item = &PlanItem> {
        self.items
            .iter()
            .filter(|item| item.include && item.status != CompareStatus::RemoteOnly)
    }

    fn included_deletes(&self) -> impl Iterator<Item = &PlanItem> {
        self.items
            .iter()
            .filter(|item| item.include && item.status == CompareStatus::RemoteOnly)
    }
}

/// Changed, then New, then RemoteOnly; each by `rel`.
fn sort_items(items: &mut [PlanItem]) {
    let group = |status: CompareStatus| match status {
        CompareStatus::Changed => 0,
        CompareStatus::New => 1,
        CompareStatus::RemoteOnly => 2,
        CompareStatus::Same => 3,
    };
    items.sort_by(|a, b| {
        group(a.status)
            .cmp(&group(b.status))
            .then_with(|| a.rel.cmp(&b.rel))
    });
}

/// `root` joined with a relative '/' path, one segment at a time.
fn local_path(root: &Path, rel: &str) -> PathBuf {
    let mut path = root.to_owned();
    path.extend(rel.split('/').filter(|segment| !segment.is_empty()));
    path
}

/// `root` joined with a relative '/' path.
fn remote_path(root: &RemotePath, rel: &str) -> Option<RemotePath> {
    RemotePath::parse(&format!("{}/{rel}", root.as_str())).ok()
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
