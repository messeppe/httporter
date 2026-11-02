//! One lane per site: its queue, the work in flight, its limits, and the code that runs a unit
//! of work against the site's provider.
//!
//! A *unit* is what holds one slot: a single job, or a group of file deletes sent as one
//! `delete_batch`. A unit runs in its own task until it ends or is interrupted (pause, cancel,
//! an auth stop, the engine going away); whatever it did not finish goes back to the engine.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use httporter_core::{
    BodyMeter, Job, JobId, ProviderError, RemotePath, RetryPolicy, StorageProvider,
    TransferSettings, UploadOptions, UploadSource, content_type_for, with_retry,
};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::engine::{Inner, Slab};
use crate::hash::{extended, hash_file};
use crate::limiter::AdaptiveLimiter;

/// What a download is written to until it is complete: `<target>.httporter-part`.
pub(crate) const PART_SUFFIX: &str = ".httporter-part";

/// Identifies one unit of work while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct UnitId(pub u64);

/// The two kinds of slot a lane hands out, each with its own limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotKind {
    /// Uploads, downloads and new folders: `settings.transfer_limit(transfer_concurrency_max)`.
    Transfer,
    /// Deletes: `capabilities.delete_concurrency`.
    Delete,
}

impl SlotKind {
    pub(crate) fn of(job: &Job) -> Self {
        match job {
            Job::Delete { .. } => Self::Delete,
            Job::Upload { .. } | Job::Download { .. } | Job::CreateFolder { .. } => Self::Transfer,
        }
    }
}

/// The remote path a job works on.
pub(crate) fn remote_of(job: &Job) -> &RemotePath {
    match job {
        Job::Upload { remote, .. }
        | Job::Download { remote, .. }
        | Job::Delete { remote, .. }
        | Job::CreateFolder { remote } => remote,
    }
}

/// Whether a lane is taking work. Task 14 adds `Offline`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaneState {
    Running,
    /// The site refused its credentials: nothing starts until `resume_site`.
    AuthStopped,
}

/// The slots of one kind: the adaptive limit and how many are in use.
#[derive(Debug)]
pub(crate) struct Slots {
    pub(crate) limiter: AdaptiveLimiter,
    pub(crate) in_use: usize,
    /// Goes up with every slow-down that halved the limit. A slow-down reported by a request
    /// that started before the last halving has already been answered, so it doesn't halve
    /// again: a burst of refusals from one round of requests halves the limit once.
    pub(crate) generation: u64,
}

impl Slots {
    fn new(limit: usize) -> Self {
        Self {
            limiter: AdaptiveLimiter::new(limit),
            in_use: 0,
            generation: 0,
        }
    }
}

/// A unit in flight.
#[derive(Debug)]
pub(crate) struct ActiveUnit {
    pub(crate) jobs: Vec<JobId>,
    pub(crate) kind: SlotKind,
    /// Notified to interrupt the unit.
    pub(crate) stop: Arc<Notify>,
}

/// One site's work.
pub(crate) struct Lane {
    pub(crate) name: String,
    pub(crate) host: String,
    pub(crate) provider: Arc<dyn StorageProvider>,
    /// Jobs waiting, in the order they start. Paused jobs stay in place and are skipped.
    pub(crate) queue: VecDeque<JobId>,
    /// How many of the queued jobs are paused.
    pub(crate) paused: usize,
    pub(crate) units: HashMap<UnitId, ActiveUnit>,
    pub(crate) transfers: Slots,
    pub(crate) deletes: Slots,
    pub(crate) state: LaneState,
    pub(crate) paths: ActivePaths,
}

impl Lane {
    pub(crate) fn new(
        name: String,
        host: String,
        provider: Arc<dyn StorageProvider>,
        settings: &TransferSettings,
    ) -> Self {
        let mut lane = Self {
            name,
            host,
            provider,
            queue: VecDeque::new(),
            paused: 0,
            units: HashMap::new(),
            transfers: Slots::new(1),
            deletes: Slots::new(1),
            state: LaneState::Running,
            paths: ActivePaths::default(),
        };
        lane.configure(settings);
        lane
    }

    /// Takes the limits from the settings and the provider's capabilities.
    pub(crate) fn configure(&mut self, settings: &TransferSettings) {
        let caps = self.provider.capabilities();
        self.transfers
            .limiter
            .set_configured(settings.transfer_limit(caps.transfer_concurrency_max));
        self.deletes
            .limiter
            .set_configured(caps.delete_concurrency.max(1));
        if !settings.adaptive {
            self.transfers.limiter.reset();
            self.deletes.limiter.reset();
        }
    }

    pub(crate) fn slots(&self, kind: SlotKind) -> &Slots {
        match kind {
            SlotKind::Transfer => &self.transfers,
            SlotKind::Delete => &self.deletes,
        }
    }

    pub(crate) fn slots_mut(&mut self, kind: SlotKind) -> &mut Slots {
        match kind {
            SlotKind::Transfer => &mut self.transfers,
            SlotKind::Delete => &mut self.deletes,
        }
    }

    /// Something is queued or running.
    pub(crate) fn has_work(&self) -> bool {
        !self.queue.is_empty() || !self.units.is_empty()
    }

    /// Something is running, or queued and not paused.
    pub(crate) fn is_moving(&self) -> bool {
        !self.units.is_empty() || self.queue.len() > self.paused
    }

    /// Takes the next unit off the queue if it may start now, and books its slot and paths.
    ///
    /// Jobs start in queue order: when the first job that isn't paused can't start (no free
    /// slot of its kind, or it must wait for work in flight on an overlapping path), nothing
    /// behind it starts either. File deletes on a provider that deletes in batches start
    /// together, as many as `batch_delete_max`, while they follow one another in the queue.
    pub(crate) fn next_unit(&mut self, jobs: &Slab) -> Option<(SlotKind, Vec<JobId>)> {
        if self.state != LaneState::Running {
            return None;
        }
        let pos = self
            .queue
            .iter()
            .position(|id| jobs.get(*id).is_some_and(|entry| !entry.paused))?;
        let id = self.queue[pos];
        let job = &jobs.get(id)?.job;
        let kind = SlotKind::of(job);
        let slots = self.slots(kind);
        if slots.in_use >= slots.limiter.limit() || self.paths.blocks(job) {
            return None;
        }
        let batch_max = self.provider.capabilities().batch_delete_max as usize;
        let ids = if matches!(job, Job::Delete { folder: false, .. }) && batch_max > 1 {
            self.take_file_deletes(jobs, batch_max)
        } else {
            self.queue.remove(pos);
            self.paths.add(job);
            vec![id]
        };
        self.slots_mut(kind).in_use += 1;
        Some((kind, ids))
    }

    /// Up to `max` file deletes from the front of the queue (passing over paused jobs), until a
    /// job of another kind or one that must wait. The first job not paused is known to qualify.
    fn take_file_deletes(&mut self, jobs: &Slab, max: usize) -> Vec<JobId> {
        let mut group = Vec::new();
        let mut passed = Vec::new();
        while group.len() < max {
            let Some(&id) = self.queue.front() else {
                break;
            };
            let Some(entry) = jobs.get(id) else {
                break;
            };
            if entry.paused {
                passed.push(id);
                self.queue.pop_front();
                continue;
            }
            if !matches!(entry.job, Job::Delete { folder: false, .. })
                || self.paths.blocks(&entry.job)
            {
                break;
            }
            self.queue.pop_front();
            self.paths.add(&entry.job);
            group.push(id);
        }
        for id in passed.into_iter().rev() {
            self.queue.push_front(id);
        }
        group
    }
}

/// The remote paths of the work in flight on a lane, so that work on overlapping paths waits:
/// a delete waits for anything in flight at or below its path, and other work waits for a
/// delete in flight at or above its path.
#[derive(Debug, Default)]
pub(crate) struct ActivePaths {
    /// Every path in flight, with how many jobs work on it.
    all: HashMap<RemotePath, usize>,
    /// File deletes in flight.
    files_deleted: HashMap<RemotePath, usize>,
    /// Folder deletes in flight (few at a time).
    folders_deleted: Vec<RemotePath>,
}

impl ActivePaths {
    pub(crate) fn add(&mut self, job: &Job) {
        let path = remote_of(job);
        *self.all.entry(path.clone()).or_default() += 1;
        match job {
            Job::Delete { folder: true, .. } => self.folders_deleted.push(path.clone()),
            Job::Delete { folder: false, .. } => {
                *self.files_deleted.entry(path.clone()).or_default() += 1;
            }
            _ => {}
        }
    }

    pub(crate) fn remove(&mut self, job: &Job) {
        let path = remote_of(job);
        release(&mut self.all, path);
        match job {
            Job::Delete { folder: true, .. } => {
                if let Some(i) = self.folders_deleted.iter().position(|p| p == path) {
                    self.folders_deleted.swap_remove(i);
                }
            }
            Job::Delete { folder: false, .. } => release(&mut self.files_deleted, path),
            _ => {}
        }
    }

    /// `job` must wait for work in flight.
    pub(crate) fn blocks(&self, job: &Job) -> bool {
        match job {
            Job::Delete {
                remote,
                folder: true,
            } => self.all.keys().any(|path| path.is_within(remote)),
            Job::Delete {
                remote,
                folder: false,
            } => self.all.contains_key(remote) || self.under_folder_delete(remote),
            other => {
                let path = remote_of(other);
                self.files_deleted.contains_key(path) || self.under_folder_delete(path)
            }
        }
    }

    fn under_folder_delete(&self, path: &RemotePath) -> bool {
        self.folders_deleted
            .iter()
            .any(|folder| path.is_within(folder))
    }
}

fn release(counts: &mut HashMap<RemotePath, usize>, path: &RemotePath) {
    if let Some(n) = counts.get_mut(path) {
        *n -= 1;
        if *n == 0 {
            counts.remove(path);
        }
    }
}

/// What a unit does.
#[derive(Debug, Clone)]
pub(crate) enum Work {
    Upload {
        id: JobId,
        local: PathBuf,
        remote: RemotePath,
    },
    Download {
        id: JobId,
        remote: RemotePath,
        local: PathBuf,
    },
    Delete {
        id: JobId,
        remote: RemotePath,
        folder: bool,
        /// An earlier request for it may have reached the server.
        sent: bool,
    },
    /// File deletes sent together with `delete_batch`: (job, path, sent before).
    DeleteFiles {
        items: Vec<(JobId, RemotePath, bool)>,
    },
    CreateFolder {
        id: JobId,
        remote: RemotePath,
    },
}

/// Everything a unit's task needs, taken when it was dispatched.
pub(crate) struct UnitSpec {
    pub(crate) unit: UnitId,
    pub(crate) lane: usize,
    pub(crate) kind: SlotKind,
    pub(crate) provider: Arc<dyn StorageProvider>,
    pub(crate) policy: RetryPolicy,
    pub(crate) verify: bool,
    pub(crate) work: Work,
    pub(crate) stop: Arc<Notify>,
}

/// Tells the engine a unit has ended however it ends: finished, interrupted, panicked, or
/// dropped unpolled because the runtime went away.
struct Ended {
    inner: Arc<Inner>,
    lane: usize,
    unit: UnitId,
}

impl Drop for Ended {
    fn drop(&mut self) {
        self.inner
            .unit_ended(self.lane, self.unit, std::thread::panicking());
    }
}

/// Runs one unit until it finishes or its stop is notified. Every job outcome is reported to
/// the engine as it happens; what is left when the unit ends goes back to the engine.
pub(crate) fn run_unit(inner: Arc<Inner>, spec: UnitSpec) -> impl Future<Output = ()> + Send {
    let ended = Ended {
        inner: inner.clone(),
        lane: spec.lane,
        unit: spec.unit,
    };
    async move {
        let _ended = ended;
        let stop = spec.stop.clone();
        tokio::select! {
            biased;
            () = stop.notified() => {}
            () = run(&inner, &spec) => {}
        }
    }
}

async fn run(inner: &Arc<Inner>, spec: &UnitSpec) {
    match &spec.work {
        Work::Upload { id, local, remote } => {
            let result = upload(inner, spec, *id, local, remote).await;
            inner.finish(spec.unit, *id, result);
        }
        Work::Download { id, remote, local } => {
            let result = download(inner, spec, *id, remote, local, None).await;
            inner.finish(spec.unit, *id, result);
        }
        Work::Delete {
            id,
            remote,
            folder,
            sent,
        } => {
            let result = delete(inner, spec, remote, *folder, *sent).await;
            inner.finish(spec.unit, *id, result);
        }
        Work::DeleteFiles { items } => delete_files(inner, spec, items).await,
        Work::CreateFolder { id, remote } => {
            let result = attempts(inner, spec, |_| spec.provider.create_folder(remote)).await;
            inner.finish(spec.unit, *id, result);
        }
    }
}

/// Runs `op` under the retry policy, telling the lane about every slow-down.
async fn attempts<T, F, Fut>(inner: &Inner, spec: &UnitSpec, mut op: F) -> Result<T, ProviderError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    with_retry(&spec.policy, |attempt| {
        let seen = inner.generation(spec.lane, spec.kind);
        let request = op(attempt);
        async move {
            let result = request.await;
            if let Err(error) = &result
                && error.is_slow_down()
            {
                inner.slow_down(spec.lane, spec.kind, seen);
            }
            result
        }
    })
    .await
}

/// A local file that can't be read or written: not worth retrying.
fn local_error(path: &Path, error: &std::io::Error) -> ProviderError {
    ProviderError::Permanent(format!("{}: {error}", path.display()))
}

/// Counts an upload's bytes as they go and holds them to the bandwidth limit.
struct JobMeter {
    inner: Arc<Inner>,
    unit: UnitId,
    id: JobId,
}

#[async_trait]
impl BodyMeter for JobMeter {
    async fn chunk(&self, len: usize) {
        self.inner.bucket.take(len).await;
        self.inner.progress(self.unit, self.id, len as u64);
    }
}

async fn upload(
    inner: &Arc<Inner>,
    spec: &UnitSpec,
    id: JobId,
    local: &Path,
    remote: &RemotePath,
) -> Result<(), ProviderError> {
    let (len, sha256, md5) = if spec.verify {
        let path = local.to_owned();
        let hashes = tokio::task::spawn_blocking(move || hash_file(&path))
            .await
            .map_err(|e| ProviderError::Permanent(format!("hashing stopped: {e}")))?
            .map_err(|e| local_error(local, &e))?;
        (hashes.size, Some(hashes.sha256), Some(hashes.md5))
    } else {
        let meta = std::fs::metadata(extended(local)).map_err(|e| local_error(local, &e))?;
        (meta.len(), None, None)
    };
    inner.set_size(spec.unit, id, len);
    let meter: Arc<dyn BodyMeter> = Arc::new(JobMeter {
        inner: inner.clone(),
        unit: spec.unit,
        id,
    });
    let opts = UploadOptions {
        sha256,
        md5,
        meter: Some(meter),
        ..UploadOptions::new(content_type_for(remote.name().unwrap_or_default()))
    };
    attempts(inner, spec, |_| {
        inner.restart_progress(spec.unit, id);
        let source = UploadSource::File {
            path: local.to_owned(),
            len,
        };
        spec.provider.upload(remote, source, &opts)
    })
    .await
}

/// `<target>.httporter-part`, beside the target.
pub(crate) fn part_path(local: &Path) -> PathBuf {
    let mut name = local.file_name().unwrap_or_default().to_owned();
    name.push(PART_SUFFIX);
    local.with_file_name(name)
}

/// Removes a part file when dropped, unless it was renamed into place.
struct PartFile {
    path: PathBuf,
    renamed: bool,
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.renamed {
            let _ = std::fs::remove_file(extended(&self.path));
        }
    }
}

/// Streams `remote` into the part file, taking each chunk from the bandwidth limit, checks it
/// against `sha256` when one is known, then renames it over `local`.
///
/// `sha256` is the listed digest of the remote file. A `Job::Download` carries none yet, so
/// the engine passes `None` until a batch can say it (see the Task 12 report).
async fn download(
    inner: &Arc<Inner>,
    spec: &UnitSpec,
    id: JobId,
    remote: &RemotePath,
    local: &Path,
    sha256: Option<[u8; 32]>,
) -> Result<(), ProviderError> {
    if let Some(parent) = local.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(extended(parent)).map_err(|e| local_error(parent, &e))?;
    }
    let mut part = PartFile {
        path: part_path(local),
        renamed: false,
    };
    attempts(inner, spec, |_| {
        fetch(inner, spec, id, remote, &part.path, sha256)
    })
    .await?;
    std::fs::rename(extended(&part.path), extended(local)).map_err(|e| local_error(local, &e))?;
    part.renamed = true;
    Ok(())
}

/// One attempt at a download: the whole body into a fresh part file.
async fn fetch(
    inner: &Inner,
    spec: &UnitSpec,
    id: JobId,
    remote: &RemotePath,
    part: &Path,
    sha256: Option<[u8; 32]>,
) -> Result<(), ProviderError> {
    inner.restart_progress(spec.unit, id);
    let mut body = spec.provider.download(remote).await?;
    let mut file = std::fs::File::create(extended(part)).map_err(|e| local_error(part, &e))?;
    let mut digest = sha256.map(|_| Sha256::new());
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        inner.bucket.take(chunk.len()).await;
        file.write_all(&chunk).map_err(|e| local_error(part, &e))?;
        if let Some(digest) = digest.as_mut() {
            digest.update(&chunk);
        }
        inner.progress(spec.unit, id, chunk.len() as u64);
    }
    drop(file);
    if let (Some(digest), Some(expected)) = (digest, sha256)
        && <[u8; 32]>::from(digest.finalize()) != expected
    {
        return Err(ProviderError::ChecksumMismatch);
    }
    Ok(())
}

/// Deletes one file or folder. `NotFound` after a request whose answer was lost (a retry, or a
/// request interrupted earlier) means it is gone: success (ledger 179).
///
/// A folder goes with `delete_folder`. With `recursive_delete` that is one request for the
/// whole subtree; without it the planner has already queued what is inside ahead of the folder
/// (ruling Q16), and the folder waits for those deletes, so only the empty folder or its marker
/// is left for `delete_folder`.
async fn delete(
    inner: &Inner,
    spec: &UnitSpec,
    remote: &RemotePath,
    folder: bool,
    sent: bool,
) -> Result<(), ProviderError> {
    attempts(inner, spec, |attempt| {
        let request = if folder {
            spec.provider.delete_folder(remote)
        } else {
            spec.provider.delete_file(remote)
        };
        async move {
            match request.await {
                Err(ProviderError::NotFound) if sent || attempt > 0 => Ok(()),
                other => other,
            }
        }
    })
    .await
}

/// File deletes sent together. Each path is answered for on its own: those that fail with a
/// retryable error are sent again (alone together) after the policy's wait, the others are
/// done or failed at once.
async fn delete_files(inner: &Inner, spec: &UnitSpec, items: &[(JobId, RemotePath, bool)]) {
    let policy = &spec.policy;
    let mut pending: Vec<(JobId, RemotePath, bool)> = items.to_vec();
    let mut attempt = 0usize;
    loop {
        let seen = inner.generation(spec.lane, spec.kind);
        let paths: Vec<RemotePath> = pending.iter().map(|(_, path, _)| path.clone()).collect();
        let mut answers = spec.provider.delete_batch(&paths).await.into_iter();
        let mut again = Vec::new();
        let mut wait = Duration::ZERO;
        let mut slowed = false;
        for (id, path, sent) in pending {
            let answer = answers.next().unwrap_or_else(|| {
                Err(ProviderError::Permanent(
                    "the server did not answer for this file".into(),
                ))
            });
            match answer {
                Ok(()) => inner.finish(spec.unit, id, Ok(())),
                Err(ProviderError::NotFound) if sent || attempt > 0 => {
                    inner.finish(spec.unit, id, Ok(()));
                }
                Err(error) if worth_retrying(&error, policy) => {
                    slowed |= error.is_slow_down();
                    if let ProviderError::RateLimited {
                        retry_after: Some(after),
                    } = &error
                    {
                        wait = wait.max(*after);
                    }
                    again.push((id, path, error));
                }
                Err(error) => inner.finish(spec.unit, id, Err(error)),
            }
        }
        if slowed {
            inner.slow_down(spec.lane, spec.kind, seen);
        }
        if again.is_empty() {
            return;
        }
        let Some(&delay) = policy.delays.get(attempt) else {
            for (id, _, error) in again {
                inner.finish(spec.unit, id, Err(error));
            }
            return;
        };
        tokio::time::sleep(delay.max(wait)).await;
        attempt += 1;
        pending = again
            .into_iter()
            .map(|(id, path, _)| (id, path, true))
            .collect();
    }
}

/// As `with_retry` decides: a retryable error, unless it asks for a longer wait than the policy
/// honours.
fn worth_retrying(error: &ProviderError, policy: &RetryPolicy) -> bool {
    match error {
        ProviderError::RateLimited {
            retry_after: Some(after),
        } => *after <= policy.max_retry_after,
        other => other.is_retryable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str) -> RemotePath {
        RemotePath::parse(path).unwrap()
    }

    fn upload(remote: &str) -> Job {
        Job::Upload {
            local: PathBuf::from("x"),
            remote: p(remote),
            size: 1,
        }
    }

    fn delete(remote: &str, folder: bool) -> Job {
        Job::Delete {
            remote: p(remote),
            folder,
        }
    }

    #[test]
    fn a_folder_delete_waits_for_anything_beneath_it() {
        let mut paths = ActivePaths::default();
        paths.add(&upload("site/a/b.txt"));
        assert!(paths.blocks(&delete("site", true)));
        assert!(paths.blocks(&delete("site/a", true)));
        assert!(
            !paths.blocks(&delete("site/ab", true)),
            "whole segments only"
        );
        assert!(!paths.blocks(&delete("other", true)));
        assert!(paths.blocks(&delete("site/a/b.txt", false)));
        assert!(!paths.blocks(&delete("site/a/c.txt", false)));
        paths.remove(&upload("site/a/b.txt"));
        assert!(!paths.blocks(&delete("site", true)));
    }

    #[test]
    fn work_waits_for_a_delete_above_it() {
        let mut paths = ActivePaths::default();
        paths.add(&delete("old", true));
        paths.add(&delete("x.txt", false));
        assert!(paths.blocks(&upload("old/new.txt")));
        assert!(paths.blocks(&upload("x.txt")));
        assert!(!paths.blocks(&upload("older/new.txt")));
        assert!(
            paths.blocks(&delete("old/inner.txt", false)),
            "a file delete below a folder delete in flight"
        );
        paths.remove(&delete("old", true));
        paths.remove(&delete("x.txt", false));
        assert!(!paths.blocks(&upload("old/new.txt")));
        assert!(!paths.blocks(&upload("x.txt")));
    }

    #[test]
    fn the_same_path_twice_is_counted() {
        let mut paths = ActivePaths::default();
        paths.add(&upload("a.txt"));
        paths.add(&upload("a.txt"));
        paths.remove(&upload("a.txt"));
        assert!(paths.blocks(&delete("a.txt", false)));
        paths.remove(&upload("a.txt"));
        assert!(!paths.blocks(&delete("a.txt", false)));
    }

    #[test]
    fn part_files_sit_beside_the_target() {
        assert_eq!(
            part_path(Path::new(r"D:\out\photo.jpg")),
            PathBuf::from(r"D:\out\photo.jpg.httporter-part")
        );
    }

    #[test]
    fn slot_kinds() {
        assert_eq!(SlotKind::of(&upload("a")), SlotKind::Transfer);
        assert_eq!(SlotKind::of(&delete("a", true)), SlotKind::Delete);
        assert_eq!(
            SlotKind::of(&Job::CreateFolder { remote: p("a") }),
            SlotKind::Transfer
        );
    }

    #[test]
    fn retry_follows_with_retry_rules() {
        let policy = RetryPolicy::default();
        let limited = |s| ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(s)),
        };
        assert!(worth_retrying(&limited(300), &policy));
        assert!(!worth_retrying(&limited(301), &policy));
        assert!(worth_retrying(&ProviderError::Network("x".into()), &policy));
        assert!(!worth_retrying(&ProviderError::NotFound, &policy));
    }
}
