//! A publish: the plan made from a compare, and running it as engine batches (uploads, then
//! deletes, then the CDN purge).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use httporter_core::{
    CdnPurger, CompareStatus, FileAction, FileOutcome, HistoryFile, HistoryRecord, Job,
    JournalError, ProfileId, PublishResult, PurgeMode, PurgeResult, RemotePath, SiteId,
    StorageProvider, run_purge,
};

use crate::compare::{CompareResult, rel_of};
use crate::engine::{Batch, BatchOutcome, Engine};
use crate::lane::remote_of;
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
    /// The history record of this publish of `plan`. What the site holds afterwards is the
    /// plan's count of its files, plus the new files uploaded, less the files deleted; files the
    /// plan left out count as skipped.
    pub fn into_record(
        self,
        profile: ProfileId,
        plan: &PublishPlan,
        verified: bool,
    ) -> HistoryRecord {
        let new: HashSet<RemotePath> = plan
            .items
            .iter()
            .filter(|item| item.status == CompareStatus::New)
            .filter_map(|item| remote_path(&plan.remote_root, &item.rel))
            .collect();
        let added = self
            .files
            .iter()
            .filter(|file| {
                file.action == FileAction::Upload
                    && file.outcome == FileOutcome::Done
                    && new.contains(&file.path)
            })
            .count() as u64;
        HistoryRecord {
            id: 0,
            profile,
            started_at: self.started_at,
            finished_at: self.finished_at,
            result: self.result,
            uploaded: self.uploaded,
            deleted: self.deleted,
            skipped: plan.items.iter().filter(|item| !item.include).count() as u64,
            failed: self.failed,
            bytes: self.bytes,
            purge: self.purge,
            verified,
            remote_files: (plan.remote_files + added).saturating_sub(self.deleted),
            remote_folders: plan.remote_folders,
            regions: plan.regions,
            files: self.files,
        }
    }
}

/// Uploads (`submit_tracked` + `wait_batch`), then deletes, only if no upload failed (else
/// `NotRun("Not deleted because uploads failed.")`), then `run_purge` on the paths done.
/// `wait_batch` includes the Q55 late-delete checks, so the purge never starts before them. A
/// purge failure never fails the publish.
///
/// A cancelled upload stops the publish: its deletes are cancelled too. The result is Cancelled
/// if any job was cancelled; else Ok if nothing failed; else Partial if something was done; else
/// Failed. Only the uploads' journal entry failing is an error; the deletes' fails each delete.
pub async fn run_publish(
    engine: &Engine,
    target: BatchTarget,
    plan: &PublishPlan,
    purge: PurgeMode,
    purger: Option<Arc<dyn CdnPurger>>,
    on_phase: &(dyn Fn(PublishPhase) + Send + Sync),
) -> Result<PublishOutcome, JournalError> {
    let started_at = Utc::now();
    let sizes: HashMap<RemotePath, u64> = plan
        .items
        .iter()
        .filter(|item| item.include)
        .filter_map(|item| Some((remote_path(&plan.remote_root, &item.rel)?, item.size)))
        .collect();
    let mut files = Vec::new();

    on_phase(PublishPhase::Uploading);
    let uploads = plan.uploads();
    let up = run_batch(engine, &target, uploads.clone()).await?;
    let bytes = up.bytes_done;
    let stopped = !up.cancelled.is_empty();
    let upload_failed = !up.failed.is_empty();
    files.extend(history(&uploads, up, FileAction::Upload, &sizes));

    let deletes = plan.deletes();
    if !deletes.is_empty() {
        let skipped = if stopped {
            Some(FileOutcome::Cancelled)
        } else if upload_failed {
            Some(FileOutcome::NotRun(
                messages::NOT_DELETED_UPLOADS_FAILED.to_owned(),
            ))
        } else {
            None
        };
        let down = match skipped {
            Some(outcome) => Err(outcome),
            None => {
                on_phase(PublishPhase::Deleting);
                run_batch(engine, &target, deletes.clone())
                    .await
                    .map_err(|error| FileOutcome::Failed(error.to_string()))
            }
        };
        match down {
            Ok(down) => files.extend(history(&deletes, down, FileAction::Delete, &sizes)),
            Err(outcome) => files.extend(deletes.iter().map(|job| {
                let path = remote_of(job).clone();
                HistoryFile {
                    size: sizes.get(&path).copied().unwrap_or(0),
                    path,
                    action: FileAction::Delete,
                    outcome: outcome.clone(),
                }
            })),
        }
    }

    let done = |action: FileAction| {
        files
            .iter()
            .filter(|file| file.action == action && file.outcome == FileOutcome::Done)
            .count() as u64
    };
    let (uploaded, deleted) = (done(FileAction::Upload), done(FileAction::Delete));
    let failed = files
        .iter()
        .filter(|file| matches!(file.outcome, FileOutcome::Failed(_)))
        .count() as u64;
    let cancelled = files
        .iter()
        .any(|file| file.outcome == FileOutcome::Cancelled);

    let touched: Vec<RemotePath> = files
        .iter()
        .filter(|file| file.outcome == FileOutcome::Done)
        .map(|file| file.path.clone())
        .collect();
    let purge = match (purge, purger) {
        (PurgeMode::Off, _) => PurgeResult::Off,
        // Nothing changed on the site: nothing to purge.
        _ if touched.is_empty() => PurgeResult::Urls(0),
        (_, None) => PurgeResult::Unavailable(messages::NO_PURGER.to_owned()),
        (mode, Some(purger)) => {
            let progress = |done: usize, total: usize| {
                on_phase(PublishPhase::Purging { done, total });
            };
            run_purge(&*purger, mode, &touched, &progress).await
        }
    };

    let result = if cancelled {
        PublishResult::Cancelled
    } else if failed == 0 {
        PublishResult::Ok
    } else if uploaded + deleted > 0 {
        PublishResult::Partial
    } else {
        PublishResult::Failed
    };
    on_phase(PublishPhase::Finished);
    Ok(PublishOutcome {
        result,
        files,
        uploaded,
        deleted,
        failed,
        bytes,
        purge,
        started_at,
        finished_at: Utc::now(),
    })
}

/// Runs `jobs` as one tracked batch and waits for it, Q55 checks included. No jobs, no batch.
async fn run_batch(
    engine: &Engine,
    target: &BatchTarget,
    jobs: Vec<Job>,
) -> Result<BatchOutcome, JournalError> {
    if jobs.is_empty() {
        return Ok(BatchOutcome::default());
    }
    let batch = Batch {
        site: target.site.clone(),
        site_name: target.site_name.clone(),
        host: target.host.clone(),
        provider: target.provider.clone(),
        jobs,
        checksums: HashMap::new(),
    };
    // The journal is written before anything starts: a disk write, so off the runtime.
    let submitter = engine.clone();
    let id = tokio::task::spawn_blocking(move || submitter.submit_tracked(batch))
        .await
        .map_err(|_| JournalError(messages::STOPPED_UNEXPECTEDLY.to_owned()))??;
    Ok(engine.wait_batch(&id).await)
}

/// A history line for each of `jobs`, from how its batch went.
fn history(
    jobs: &[Job],
    outcome: BatchOutcome,
    action: FileAction,
    sizes: &HashMap<RemotePath, u64>,
) -> Vec<HistoryFile> {
    let mut ends: HashMap<RemotePath, FileOutcome> = HashMap::new();
    ends.extend(
        outcome
            .done
            .into_iter()
            .map(|path| (path, FileOutcome::Done)),
    );
    ends.extend(
        outcome
            .failed
            .into_iter()
            .map(|(path, reason)| (path, FileOutcome::Failed(reason))),
    );
    ends.extend(
        outcome
            .cancelled
            .into_iter()
            .map(|path| (path, FileOutcome::Cancelled)),
    );
    jobs.iter()
        .map(|job| {
            let path = remote_of(job).clone();
            HistoryFile {
                outcome: ends.remove(&path).unwrap_or(FileOutcome::Cancelled),
                size: sizes.get(&path).copied().unwrap_or(0),
                path,
                action,
            }
        })
        .collect()
}
