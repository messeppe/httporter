//! One lane per site: its queue, the work in flight, its limits, and the code that runs a unit
//! of work against the site's provider.
//!
//! A *unit* is what holds one slot: a single job, or a group of file deletes sent as one
//! `delete_batch`. A unit runs in its own task until it ends or is interrupted (pause, cancel,
//! an auth stop, the engine going away); whatever it did not finish goes back to the engine.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures_util::StreamExt;
use httporter_core::{
    AuthHint, BodyMeter, Checksum, ChecksumKind, FileStamp, Job, JobId, ProviderError, RemotePath,
    RetryPolicy, StorageProvider, TransferSettings, UploadOptions, UploadSource, content_type_for,
};
use md5::Md5;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::engine::{Inner, Net, Slab};
use crate::limiter::AdaptiveLimiter;
use crate::local::{LocalError, LocalFiles, blocking};
use crate::messages;
use crate::offline::{NetworkErrors, is_disk_full};

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

/// Whether a lane is taking work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LaneState {
    Running,
    /// The site refused its credentials: nothing starts until `resume_site`.
    AuthStopped,
    /// The site found the PC's clock too far off: nothing starts until `resume_site`, once the
    /// clock is set (review I4).
    ClockSkew,
    /// The site can't be reached: nothing starts until a probe gets an answer (see
    /// [`crate::offline`]).
    Offline,
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
    /// The latest network errors, which take the lane offline when they come too close.
    pub(crate) network: NetworkErrors,
    /// How many times the lane has gone offline: a job's strikes are counted once per cycle.
    pub(crate) cycle: u64,
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
            network: NetworkErrors::default(),
            cycle: 0,
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
            } => {
                self.all.keys().any(|path| path.is_within(remote))
                    || self.under_folder_delete(remote)
            }
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
        /// What the contents must hash to, when the batch said (ruling Q27).
        checksum: Option<Checksum>,
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

impl Work {
    /// The remote path of a single job's work; `None` for file deletes sent together.
    fn remote(&self) -> Option<&RemotePath> {
        match self {
            Self::Upload { remote, .. }
            | Self::Download { remote, .. }
            | Self::Delete { remote, .. }
            | Self::CreateFolder { remote, .. } => Some(remote),
            Self::DeleteFiles { .. } => None,
        }
    }
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
    /// The local file operations: the disk, or a test's stand-in.
    pub(crate) files: Arc<dyn LocalFiles>,
}

/// How a job's work ended, short of success.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The site's error: an auth failure stops the site, anything else fails the job with the
    /// app's words for it.
    Provider(ProviderError),
    /// The job fails with this reason, as it is: the engine's own words (see
    /// [`crate::messages`]) or the system's message about a local file.
    Reason(String),
    /// The disk a download is written to is full: every lane stops and the job waits in the
    /// queue.
    DiskFull,
    /// The site can't be reached: the job goes back to the queue as it was.
    Offline,
}

impl From<ProviderError> for Failure {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

impl LocalError {
    /// What a local error means for the job: a full disk under a download stops everything;
    /// anything else fails the job with the system's message.
    fn failure(self, path: &Path, download: bool, stopped: &str) -> Failure {
        match self {
            Self::Io(error) if download && is_disk_full(&error) => Failure::DiskFull,
            Self::Io(error) => Failure::Reason(messages::local_file(path, &error)),
            Self::Stopped => Failure::Reason(stopped.to_owned()),
        }
    }
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
/// the engine as it happens; what is left when the unit ends goes back to the engine, once
/// a download's leftover part file is gone (so a job sent again never meets it).
pub(crate) fn run_unit(inner: Arc<Inner>, spec: UnitSpec) -> impl Future<Output = ()> + Send {
    let ended = Ended {
        inner: inner.clone(),
        lane: spec.lane,
        unit: spec.unit,
    };
    async move {
        let _ended = ended;
        let part = matches!(spec.work, Work::Download { .. })
            .then(|| PartOwner(PartSlot::new(spec.files.clone())));
        let stop = spec.stop.clone();
        tokio::select! {
            biased;
            () = stop.notified() => {}
            () = run(&inner, &spec, part.as_ref().map(|owner| &owner.0)) => {}
        }
        if let Some(part) = &part {
            part.0.clean_up().await;
        }
    }
}

async fn run(inner: &Arc<Inner>, spec: &UnitSpec, part: Option<&PartSlot>) {
    match &spec.work {
        Work::Upload { id, local, remote } => {
            let result = upload(inner, spec, *id, local, remote).await;
            inner.finish(spec.unit, *id, result);
        }
        Work::Download {
            id,
            remote,
            local,
            checksum,
        } => {
            let result = match part {
                Some(part) => {
                    download(inner, spec, *id, remote, local, checksum.as_ref(), part).await
                }
                None => Err(Failure::Reason(messages::FILE_WORK_STOPPED.to_owned())),
            };
            inner.finish(spec.unit, *id, result);
        }
        Work::Delete {
            id,
            remote,
            folder,
            sent,
        } => {
            let result = delete(inner, spec, *id, remote, *folder, *sent).await;
            inner.finish(spec.unit, *id, result);
        }
        Work::DeleteFiles { items } => delete_files(inner, spec, items).await,
        Work::CreateFolder { id, remote } => {
            let result = attempts(inner, spec, *id, |_| async {
                spec.provider
                    .create_folder(remote)
                    .await
                    .map_err(Failure::from)
            })
            .await;
            inner.finish(spec.unit, *id, result);
        }
    }
}

/// Runs `op` under the retry policy, as `with_retry` does, telling the lane about every
/// slow-down and every network error.
///
/// A network error that takes the lane offline, or one met while it is offline, ends the
/// attempts with [`Failure::Offline`]: the job waits in the queue for the site, and the
/// attempts it used don't count. So does using up the retries on network errors, which takes
/// the lane offline. Each time is a strike for the job; at the third it fails instead (ruling
/// Q50, [`Inner::network_error`]).
///
/// An unexplained refusal ([`AuthHint::Unknown`]) is checked before the site is stopped for it
/// ([`denied_or_auth`], review I5).
async fn attempts<T, F, Fut>(
    inner: &Arc<Inner>,
    spec: &UnitSpec,
    id: JobId,
    mut op: F,
) -> Result<T, Failure>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, Failure>>,
{
    let policy = &spec.policy;
    let mut attempt: u32 = 0;
    loop {
        if inner.lane_offline(spec.lane) {
            return Err(Failure::Offline);
        }
        let seen = inner.generation(spec.lane, spec.kind);
        let error = match op(attempt).await {
            Ok(value) => return Ok(value),
            Err(Failure::Provider(error)) => error,
            Err(other) => return Err(other),
        };
        if error.is_slow_down() {
            inner.slow_down(spec.lane, spec.kind, seen);
        }
        if let ProviderError::Auth {
            hint: AuthHint::Unknown,
        } = error
        {
            return Err(denied_or_auth(inner, spec, error).await);
        }
        let delay = policy.delays.get(attempt as usize).copied();
        if matches!(error, ProviderError::Network(_)) {
            let exhausted = delay.is_none();
            match inner.network_error(spec.lane, spec.unit, &[id], exhausted)[0] {
                Net::Retry => {}
                Net::Offline => return Err(Failure::Offline),
                Net::Fail => return Err(error.into()),
            }
        }
        if !worth_retrying(&error, policy) {
            return Err(error.into());
        }
        let Some(delay) = delay else {
            return Err(error.into());
        };
        let wait = match &error {
            ProviderError::RateLimited {
                retry_after: Some(after),
            } => delay.max(*after),
            _ => delay,
        };
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

/// What an unexplained refusal of one file's request (`error`, S3 `AccessDenied` with no hint)
/// means (review I5). It may be the sign-in failing, or a key or a bucket policy that allows some
/// paths only: the file's folder is listed to tell. Listed, or found missing (S3 answers so for
/// a folder with no key under it yet, the folder an upload makes), the sign-in works, and only
/// this file fails, with [`messages::ACCESS_DENIED`]; stopping the site would loop, since Edit
/// Site's test would pass and the same file be refused again. Any other answer to the listing
/// (refused as well, a server error, a request to slow down) or none tells nothing for sure
/// (re-review of I5), and the site stops as for any refusal ([`Failure::Provider`] with
/// `error`). A site already stopped isn't asked again.
async fn denied_or_auth(inner: &Arc<Inner>, spec: &UnitSpec, error: ProviderError) -> Failure {
    let Some(remote) = spec.work.remote() else {
        return error.into();
    };
    if inner.lane_stopped(spec.lane) {
        return error.into();
    }
    let dir = remote.parent().unwrap_or_else(RemotePath::root);
    match spec.provider.list(&dir).await {
        Ok(_) | Err(ProviderError::NotFound) => Failure::Reason(messages::ACCESS_DENIED.to_owned()),
        Err(_) => error.into(),
    }
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

/// What an upload sends: the file's length when it was last looked at, when it last changed,
/// and its digests when checksums are verified.
#[derive(Debug, Clone)]
struct Prepared {
    len: u64,
    modified: Option<SystemTime>,
    sha256: Option<[u8; 32]>,
    md5: Option<[u8; 16]>,
}

/// Looks the file up again before an attempt and hashes it again when it changed since it was
/// hashed (its size or modification time differs), or when it never was.
async fn prepare(
    files: &Arc<dyn LocalFiles>,
    local: &Path,
    verify: bool,
    current: Option<Prepared>,
) -> Result<Prepared, Failure> {
    let (len, modified) = {
        let (files, path) = (files.clone(), local.to_owned());
        blocking(move || files.stat(&path))
            .await
            .map_err(|e| e.failure(local, false, messages::FILE_WORK_STOPPED))?
    };
    if let Some(current) = current
        && current.len == len
        && current.modified == modified
        && (!verify || current.sha256.is_some())
    {
        return Ok(current);
    }
    if !verify {
        return Ok(Prepared {
            len,
            modified,
            sha256: None,
            md5: None,
        });
    }
    let (files, path) = (files.clone(), local.to_owned());
    let hashes = blocking(move || files.hash(&path))
        .await
        .map_err(|e| e.failure(local, false, messages::HASHING_STOPPED))?;
    Ok(Prepared {
        len: hashes.size,
        modified: Some(hashes.modified),
        sha256: Some(hashes.sha256),
        md5: Some(hashes.md5),
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing under these locks can stop part-way, so a poisoned one still holds whole values.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sends a local file. Before each attempt the file is looked at again and re-hashed if it
/// changed. A checksum mismatch re-hashes it and sends it once more; a second one fails the job
/// as damaged twice.
async fn upload(
    inner: &Arc<Inner>,
    spec: &UnitSpec,
    id: JobId,
    local: &Path,
    remote: &RemotePath,
) -> Result<(), Failure> {
    let meter: Arc<dyn BodyMeter> = Arc::new(JobMeter {
        inner: inner.clone(),
        unit: spec.unit,
        id,
    });
    let content_type = content_type_for(remote.name().unwrap_or_default());
    let prepared: Mutex<Option<Prepared>> = Mutex::new(None);
    let mut mismatches = 0;
    loop {
        let result = attempts(inner, spec, id, |_| {
            let (prepared, meter, content_type) = (&prepared, &meter, &content_type);
            async move {
                let current = lock(prepared).clone();
                let fresh = prepare(&spec.files, local, spec.verify, current).await?;
                *lock(prepared) = Some(fresh.clone());
                inner.set_size(spec.unit, id, fresh.len);
                inner.restart_progress(spec.unit, id);
                // Every upload keeps its multipart progress in the journal, for providers that
                // send large files in parts (ruling Q49).
                let parts = inner.job_parts(
                    spec.unit,
                    id,
                    FileStamp::new(fresh.len, fresh.modified),
                    spec.provider.clone(),
                    remote.clone(),
                );
                let opts = UploadOptions {
                    sha256: fresh.sha256,
                    md5: fresh.md5,
                    meter: Some(meter.clone()),
                    parts: Some(parts),
                    ..UploadOptions::new(content_type.clone())
                };
                let source = UploadSource::File {
                    path: local.to_owned(),
                    len: fresh.len,
                };
                spec.provider
                    .upload(remote, source, &opts)
                    .await
                    .map_err(Failure::from)
            }
        })
        .await;
        match result {
            Err(Failure::Provider(ProviderError::ChecksumMismatch)) if mismatches == 0 => {
                mismatches += 1;
                // Hash it again whatever its size and time say.
                *lock(&prepared) = None;
            }
            Err(Failure::Provider(ProviderError::ChecksumMismatch)) => {
                return Err(Failure::Reason(messages::DAMAGED_TWICE.to_owned()));
            }
            other => return other,
        }
    }
}

/// `<target>.httporter-part`, beside the target.
pub(crate) fn part_path(local: &Path) -> PathBuf {
    let mut name = local.file_name().unwrap_or_default().to_owned();
    name.push(PART_SUFFIX);
    local.with_file_name(name)
}

/// How much of a download is gathered before it is written, in one blocking call.
const WRITE_BATCH: usize = 256 * 1024;

/// The part file a download unit writes, shared with the blocking tasks that write it.
///
/// Every file operation runs on a blocking thread and takes the lock there, so they happen one
/// after another: the clean-up at the end of the unit waits for a write still under way, closes
/// the file, then removes it.
#[derive(Clone)]
pub(crate) struct PartSlot {
    files: Arc<dyn LocalFiles>,
    state: Arc<Mutex<PartState>>,
}

#[derive(Default)]
struct PartState {
    /// The part file, while there is one to remove.
    path: Option<PathBuf>,
    writer: Option<Box<dyn Write + Send>>,
}

impl PartSlot {
    fn new(files: Arc<dyn LocalFiles>) -> Self {
        Self {
            files,
            state: Arc::new(Mutex::new(PartState::default())),
        }
    }

    /// Runs `f` with the part file's state on a blocking thread.
    async fn with<T, F>(&self, f: F) -> Result<T, LocalError>
    where
        T: Send + 'static,
        F: FnOnce(&dyn LocalFiles, &mut PartState) -> std::io::Result<T> + Send + 'static,
    {
        let (files, state) = (self.files.clone(), self.state.clone());
        blocking(move || f(&*files, &mut lock(&state))).await
    }

    /// Creates (or empties) the part file at `path`.
    async fn create(&self, path: &Path) -> Result<(), LocalError> {
        let path = path.to_owned();
        self.with(move |files, state| {
            state.writer = None;
            state.path = Some(path.clone());
            state.writer = Some(files.create(&path)?);
            Ok(())
        })
        .await
    }

    async fn write(&self, data: Vec<u8>) -> Result<(), LocalError> {
        self.with(move |_, state| match state.writer.as_mut() {
            Some(writer) => writer.write_all(&data),
            None => Err(std::io::Error::other("the part file is closed")),
        })
        .await
    }

    /// Flushes and closes the part file.
    async fn close(&self) -> Result<(), LocalError> {
        self.with(|_, state| match state.writer.take() {
            Some(mut writer) => writer.flush(),
            None => Ok(()),
        })
        .await
    }

    /// Moves the closed part file onto `target`; it is then no longer the unit's to remove.
    async fn rename_to(&self, target: &Path) -> Result<(), LocalError> {
        let target = target.to_owned();
        self.with(move |files, state| {
            state.writer = None;
            if let Some(path) = &state.path {
                files.rename(path, &target)?;
            }
            state.path = None;
            Ok(())
        })
        .await
    }

    /// Closes and removes whatever part file is left.
    async fn clean_up(&self) {
        let _ = self
            .with(|files, state| {
                state.writer = None;
                if let Some(path) = state.path.take() {
                    let _ = files.remove(&path);
                }
                Ok(())
            })
            .await;
    }
}

/// The unit's own hold on its part slot. The unit cleans up before it ends; only when the
/// runtime drops it unfinished (as it shuts down) is the leftover removed here, at once.
struct PartOwner(PartSlot);

impl Drop for PartOwner {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        state.writer = None;
        if let Some(path) = state.path.take() {
            let _ = self.0.files.remove(&path);
        }
    }
}

/// Streams `remote` into the part file, taking each chunk from the bandwidth limit, checks it
/// against `expected` when there is one to check, then renames it over `local`.
///
/// `expected` is the listed digest of the remote file, from the job's batch (ruling Q27); it
/// is checked when checksums are verified and it is a SHA-256 or an MD5. Contents that don't
/// match are fetched once more, then the job fails as damaged twice. A download resumed from
/// the journal has no digest and completes unchecked. A full disk stops every lane.
async fn download(
    inner: &Arc<Inner>,
    spec: &UnitSpec,
    id: JobId,
    remote: &RemotePath,
    local: &Path,
    expected: Option<&Checksum>,
    part: &PartSlot,
) -> Result<(), Failure> {
    if let Some(parent) = local.parent().filter(|p| !p.as_os_str().is_empty()) {
        let (files, dir) = (spec.files.clone(), parent.to_owned());
        blocking(move || files.create_dir_all(&dir))
            .await
            .map_err(|e| e.failure(parent, true, messages::FILE_WORK_STOPPED))?;
    }
    let part_path = part_path(local);
    let expected = expected.filter(|checksum| {
        spec.verify && matches!(checksum.kind, ChecksumKind::Sha256 | ChecksumKind::Md5)
    });
    let mut mismatches = 0;
    loop {
        let fetched = attempts(inner, spec, id, |_| {
            fetch(inner, spec, id, remote, &part_path, expected, part)
        })
        .await;
        match fetched {
            Ok(()) => break,
            Err(Failure::Provider(ProviderError::ChecksumMismatch)) if mismatches == 0 => {
                mismatches += 1;
            }
            Err(Failure::Provider(ProviderError::ChecksumMismatch)) => {
                return Err(Failure::Reason(messages::DAMAGED_TWICE.to_owned()));
            }
            Err(error) => return Err(error),
        }
    }
    part.rename_to(local)
        .await
        .map_err(|e| e.failure(local, true, messages::FILE_WORK_STOPPED))
}

/// A digest being computed over a download, of the kind its expected checksum is.
enum Hasher {
    Sha256(Sha256),
    Md5(Md5),
}

impl Hasher {
    fn for_checksum(checksum: &Checksum) -> Option<Self> {
        match checksum.kind {
            ChecksumKind::Sha256 => Some(Self::Sha256(Sha256::new())),
            ChecksumKind::Md5 => Some(Self::Md5(Md5::new())),
            ChecksumKind::MultipartEtag => None,
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(digest) => digest.update(data),
            Self::Md5(digest) => digest.update(data),
        }
    }

    /// The digest, in hex.
    fn finish(self) -> String {
        match self {
            Self::Sha256(digest) => hex::encode(digest.finalize()),
            Self::Md5(digest) => hex::encode(digest.finalize()),
        }
    }
}

/// One attempt at a download: the whole body into a fresh part file, checked against
/// `expected` when given. The file is written on blocking threads, [`WRITE_BATCH`] at a time.
async fn fetch(
    inner: &Inner,
    spec: &UnitSpec,
    id: JobId,
    remote: &RemotePath,
    part_path: &Path,
    expected: Option<&Checksum>,
    part: &PartSlot,
) -> Result<(), Failure> {
    let local = |e: LocalError| e.failure(part_path, true, messages::FILE_WORK_STOPPED);
    inner.restart_progress(spec.unit, id);
    let mut body = spec.provider.download(remote).await?;
    part.create(part_path).await.map_err(local)?;
    let mut digest = expected.and_then(Hasher::for_checksum);
    let mut pending: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        inner.bucket.take(chunk.len()).await;
        if let Some(digest) = digest.as_mut() {
            digest.update(&chunk);
        }
        pending.extend_from_slice(&chunk);
        if pending.len() >= WRITE_BATCH {
            part.write(std::mem::take(&mut pending))
                .await
                .map_err(local)?;
        }
        inner.progress(spec.unit, id, chunk.len() as u64);
    }
    if !pending.is_empty() {
        part.write(pending).await.map_err(local)?;
    }
    part.close().await.map_err(local)?;
    if let (Some(digest), Some(expected)) = (digest, expected)
        && !digest.finish().eq_ignore_ascii_case(&expected.hex)
    {
        return Err(ProviderError::ChecksumMismatch.into());
    }
    Ok(())
}

/// Deletes one file or folder. `NotFound` after a request whose answer was lost (a retry, or a
/// request that may have been sent before: interrupted, or before a crash) means it is gone:
/// success (ledger 179).
///
/// A folder goes with `delete_folder`. With `recursive_delete` that is one request for the
/// whole subtree; without it the planner has already queued what is inside ahead of the folder
/// (rulings Q16, Q38), and the folder waits for those deletes, so only the empty folder or its
/// marker is left for `delete_folder`. A folder that is already gone is done as well: one that
/// only its files implied vanished with the last of them.
async fn delete(
    inner: &Arc<Inner>,
    spec: &UnitSpec,
    id: JobId,
    remote: &RemotePath,
    folder: bool,
    sent: bool,
) -> Result<(), Failure> {
    attempts(inner, spec, id, |attempt| {
        let request = if folder {
            spec.provider.delete_folder(remote)
        } else {
            spec.provider.delete_file(remote)
        };
        async move {
            match request.await {
                Err(ProviderError::NotFound) if folder || sent || attempt > 0 => Ok(()),
                other => other.map_err(Failure::from),
            }
        }
    })
    .await
}

/// File deletes sent together. Each path is answered for on its own: those that fail with a
/// retryable error are sent again (alone together) after the policy's wait, the others are
/// done or failed at once. A request that deleted something and wasn't asked to slow down is
/// one success for the lane's limiter, however many paths it carried (ruling Q37).
///
/// A request that got no answer counts as one network error for the lane. Once the lane is
/// offline the paths still to delete are left unfinished, so they go back to the queue as they
/// were; each file whose own request took the lane offline, or met it offline, gets a strike,
/// and fails at the third (ruling Q50), as a single job does.
async fn delete_files(inner: &Arc<Inner>, spec: &UnitSpec, items: &[(JobId, RemotePath, bool)]) {
    let policy = &spec.policy;
    let mut pending: Vec<(JobId, RemotePath, bool)> = items.to_vec();
    let mut attempt = 0usize;
    loop {
        if inner.lane_offline(spec.lane) {
            return;
        }
        let seen = inner.generation(spec.lane, spec.kind);
        let paths: Vec<RemotePath> = pending.iter().map(|(_, path, _)| path.clone()).collect();
        let mut answers = spec.provider.delete_batch(&paths).await.into_iter();
        let mut again = Vec::new();
        let mut wait = Duration::ZERO;
        let mut slowed = false;
        let mut deleted = false;
        let mut unanswered = false;
        for (id, path, sent) in pending {
            let Some(answer) = answers.next() else {
                let reason = messages::NO_ANSWER_FOR_FILE.to_owned();
                inner.finish_in_batch(spec.unit, id, Err(Failure::Reason(reason)));
                continue;
            };
            match answer {
                Ok(()) => {
                    deleted = true;
                    inner.finish_in_batch(spec.unit, id, Ok(()));
                }
                Err(ProviderError::NotFound) if sent || attempt > 0 => {
                    deleted = true;
                    inner.finish_in_batch(spec.unit, id, Ok(()));
                }
                Err(error) if worth_retrying(&error, policy) => {
                    slowed |= error.is_slow_down();
                    unanswered |= matches!(error, ProviderError::Network(_));
                    if let ProviderError::RateLimited {
                        retry_after: Some(after),
                    } = &error
                    {
                        wait = wait.max(*after);
                    }
                    again.push((id, path, error));
                }
                Err(error) => inner.finish_in_batch(spec.unit, id, Err(error.into())),
            }
        }
        if slowed {
            inner.slow_down(spec.lane, spec.kind, seen);
        } else if deleted {
            inner.batch_succeeded(spec.lane, spec.kind);
        }
        if again.is_empty() {
            return;
        }
        let delay = policy.delays.get(attempt).copied();
        if unanswered {
            let unreached: Vec<JobId> = again
                .iter()
                .filter(|(_, _, error)| matches!(error, ProviderError::Network(_)))
                .map(|(id, _, _)| *id)
                .collect();
            let outcomes = inner.network_error(spec.lane, spec.unit, &unreached, delay.is_none());
            if outcomes.iter().any(|net| *net != Net::Retry) {
                // The lane is offline: what is left goes back to the queue as it was, except the
                // files that struck out, and those whose retries of another kind are used up.
                let mut outcomes = outcomes.into_iter();
                for (id, _, error) in again {
                    let fails = if matches!(error, ProviderError::Network(_)) {
                        outcomes.next() == Some(Net::Fail)
                    } else {
                        delay.is_none()
                    };
                    if fails {
                        inner.finish_in_batch(spec.unit, id, Err(error.into()));
                    }
                }
                return;
            }
        }
        let Some(delay) = delay else {
            for (id, _, error) in again {
                inner.finish_in_batch(spec.unit, id, Err(error.into()));
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
