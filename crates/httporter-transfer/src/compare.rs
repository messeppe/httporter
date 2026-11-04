//! Comparing a local folder with a folder of a site, file by file: what is new, changed, only on
//! the site or the same.
//!
//! The local folder is walked on a blocking thread while the site's folder is walked; the skip
//! rules apply on both sides. Files of equal size are hashed (8 at a time, off the runtime's
//! workers) only when the site lists a digest the mode can check.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use httporter_core::{
    Checksum, ChecksumKind, CompareBy, CompareStatus, Entry, ProviderError, ProviderKind,
    RemotePath, SkipRules, StorageProvider, looks_secret, walk,
};

use crate::hash::{FileHashes, extended, hash_file};
use crate::messages;

/// How many local files are hashed at once.
const HASH_AT_ONCE: usize = 8;

/// Progress is reported at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// What a compare may use to tell two files of the same size apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompareMode {
    pub sha256: bool,
    pub md5: bool,
    pub mtime_fallback: bool,
}

impl CompareMode {
    /// Bunny `{verify, false, false}` (size only when verify is off); S3 `{false, true, true}`.
    pub fn for_site(kind: ProviderKind, verify_checksums: bool) -> Self {
        match kind {
            ProviderKind::Bunny => Self {
                sha256: verify_checksums,
                md5: false,
                mtime_fallback: false,
            },
            ProviderKind::S3 => Self {
                sha256: false,
                md5: true,
                mtime_fallback: true,
            },
        }
    }

    /// The best way this mode compares two files.
    fn best(self) -> CompareBy {
        if self.sha256 {
            CompareBy::Sha256
        } else if self.md5 {
            CompareBy::Md5
        } else {
            CompareBy::Size
        }
    }

    /// The digest this mode checks against `checksum`, if any.
    fn digest_for(self, checksum: Option<&Checksum>) -> Option<CompareBy> {
        match checksum?.kind {
            ChecksumKind::Sha256 if self.sha256 => Some(CompareBy::Sha256),
            ChecksumKind::Md5 if self.md5 => Some(CompareBy::Md5),
            _ => None,
        }
    }
}

/// Local file digests by path, size and modification time, kept in memory only.
#[derive(Debug, Default)]
pub struct HashCache {
    hashes: Mutex<HashMap<(PathBuf, u64, SystemTime), FileHashes>>,
}

impl HashCache {
    /// `path`'s digests: the ones kept while its size and modification time are unchanged,
    /// else read from the file (and kept).
    pub fn get_or_hash(&self, path: &Path) -> io::Result<FileHashes> {
        let meta = fs::metadata(extended(path))?;
        if let Ok(modified) = meta.modified()
            && let Some(hashes) = self.cached(path, meta.len(), modified)
        {
            return Ok(hashes);
        }
        let hashes = hash_file(path)?;
        self.keep(path, &hashes);
        Ok(hashes)
    }

    fn cached(&self, path: &Path, size: u64, modified: SystemTime) -> Option<FileHashes> {
        self.hashes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(path.to_owned(), size, modified))
            .cloned()
    }

    fn keep(&self, path: &Path, hashes: &FileHashes) {
        self.hashes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (path.to_owned(), hashes.size, hashes.modified),
                hashes.clone(),
            );
    }
}

/// How far a compare is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub local_files: u64,
    pub remote_files: u64,
    pub hashed: u64,
    pub to_hash: u64,
}

/// One file of a compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareItem {
    pub rel: String,
    pub status: CompareStatus,
    pub local_size: Option<u64>,
    pub remote_size: Option<u64>,
    /// How the two sides were told apart: the mode's best way for a file on one side only.
    pub by: CompareBy,
    /// Told the same by the modification dates.
    pub by_date: bool,
}

/// How many files of each kind a compare found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompareCounts {
    pub changed: u64,
    pub new: u64,
    pub remote_only: u64,
    pub same: u64,
    pub upload_bytes: u64,
}

/// A finished compare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareResult {
    pub local_root: PathBuf,
    pub remote_root: RemotePath,
    /// Files only, by `rel`.
    pub items: Vec<CompareItem>,
    pub remote_folders: u64,
    /// Local files and folders left out: by a skip rule (a folder counts once), a name that
    /// isn't Unicode, or a link to a folder.
    pub skipped_local: u64,
    /// Local files compared that look like they hold a secret, by `rel`.
    pub secrets: Vec<String>,
    pub regions: u32,
}

impl CompareResult {
    pub fn counts(&self) -> CompareCounts {
        let mut counts = CompareCounts::default();
        for item in &self.items {
            match item.status {
                CompareStatus::Changed => counts.changed += 1,
                CompareStatus::New => counts.new += 1,
                CompareStatus::RemoteOnly => counts.remote_only += 1,
                CompareStatus::Same => counts.same += 1,
            }
            if matches!(item.status, CompareStatus::Changed | CompareStatus::New) {
                counts.upload_bytes += item.local_size.unwrap_or(0);
            }
        }
        counts
    }

    /// The direct children of `rel_dir` ("" = the root). A folder is Changed if anything below
    /// it differs, New if it is only local, RemoteOnly if only remote, else Same.
    pub fn child_statuses(&self, rel_dir: &str) -> HashMap<String, CompareStatus> {
        let rel_dir = rel_dir.trim_matches('/');
        let mut statuses = HashMap::new();
        let mut folders: HashMap<&str, Below> = HashMap::new();
        for item in &self.items {
            let rest = if rel_dir.is_empty() {
                item.rel.as_str()
            } else {
                match item
                    .rel
                    .strip_prefix(rel_dir)
                    .and_then(|rest| rest.strip_prefix('/'))
                {
                    Some(rest) => rest,
                    None => continue,
                }
            };
            match rest.split_once('/') {
                Some((folder, _)) => folders.entry(folder).or_default().add(item.status),
                None => {
                    statuses.insert(rest.to_owned(), item.status);
                }
            }
        }
        for (name, below) in folders {
            statuses.insert(name.to_owned(), below.status());
        }
        statuses
    }

    /// The weakest way any file was compared ("Compared N files by SHA-256 checksum"): among
    /// the files of equal size on both sides, which are the ones the way mattered for. With
    /// none, the best way any file had.
    pub fn weakest_method(&self) -> CompareBy {
        let rank = |by: CompareBy| match by {
            CompareBy::Size => 0,
            CompareBy::Md5 => 1,
            CompareBy::Sha256 => 2,
        };
        let pairs = self
            .items
            .iter()
            .filter(|item| item.local_size.is_some() && item.local_size == item.remote_size)
            .map(|item| item.by)
            .min_by_key(|by| rank(*by));
        pairs
            .or_else(|| {
                self.items
                    .iter()
                    .map(|item| item.by)
                    .max_by_key(|by| rank(*by))
            })
            .unwrap_or(CompareBy::Size)
    }
}

/// What the files below a folder add up to.
#[derive(Debug, Default)]
struct Below {
    local: bool,
    remote: bool,
    differs: bool,
}

impl Below {
    fn add(&mut self, status: CompareStatus) {
        let (local, remote, differs) = match status {
            CompareStatus::New => (true, false, true),
            CompareStatus::RemoteOnly => (false, true, true),
            CompareStatus::Changed => (true, true, true),
            CompareStatus::Same => (true, true, false),
        };
        self.local |= local;
        self.remote |= remote;
        self.differs |= differs;
    }

    fn status(&self) -> CompareStatus {
        match (self.local, self.remote) {
            (true, false) => CompareStatus::New,
            (false, true) => CompareStatus::RemoteOnly,
            _ if self.differs => CompareStatus::Changed,
            _ => CompareStatus::Same,
        }
    }
}

/// Why a compare stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum CompareError {
    Local { path: PathBuf, message: String },
    Remote(ProviderError),
}

/// Walks the local folder and the remote one side by side; the skip rules apply on both sides;
/// only pairs of equal size the mode can check are hashed. A missing remote root is empty.
/// Cancel by dropping the future.
///
/// A local folder that can't be read stops the compare: leaving it out would show the site's
/// copies of its files as only on the site, which a publish may delete. A file that can't be
/// read to hash it counts as Changed, so the publish tries it and says why it fails.
pub async fn compare_trees(
    provider: &dyn StorageProvider,
    local_root: &Path,
    remote_root: &RemotePath,
    skip: &SkipRules,
    mode: CompareMode,
    hashes: &HashCache,
    progress: &(dyn Fn(ScanProgress) + Send + Sync),
) -> Result<CompareResult, CompareError> {
    let found = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let _stop = StopOnDrop(stop.clone());

    let local = {
        let (root, skip, found) = (local_root.to_owned(), skip.clone(), found.clone());
        async move {
            let at = root.clone();
            tokio::task::spawn_blocking(move || LocalTree::walk(&root, &skip, &found, &stop))
                .await
                .unwrap_or_else(|_| {
                    Err(CompareError::Local {
                        path: at,
                        message: messages::FILE_WORK_STOPPED.to_owned(),
                    })
                })
        }
    };
    let remote = async {
        match walk(provider, remote_root).await {
            Ok(entries) => Ok(entries),
            Err(ProviderError::NotFound) => Ok(Vec::new()),
            Err(error) => Err(CompareError::Remote(error)),
        }
    };
    let both = async { tokio::try_join!(local, remote) };
    tokio::pin!(both);
    let mut tick = tokio::time::interval(PROGRESS_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (local, entries) = loop {
        tokio::select! {
            done = &mut both => break done?,
            _ = tick.tick() => progress(ScanProgress {
                local_files: found.load(Ordering::Relaxed),
                ..ScanProgress::default()
            }),
        }
    };

    let remote = RemoteTree::sort(entries, remote_root, skip, &local.left_out);
    let mut scan = ScanProgress {
        local_files: local.files.len() as u64,
        remote_files: remote.files.len() as u64,
        ..ScanProgress::default()
    };

    let mut items = Vec::with_capacity(local.files.len() + remote.files.len());
    // Equal-size pairs to hash: the item, the local file and the digest to check.
    let mut to_hash: Vec<(usize, &LocalFile, Checksum, CompareBy)> = Vec::new();
    let mut remote_files = remote.files;
    for (rel, file) in &local.files {
        let Some(entry) = remote_files.remove(rel) else {
            items.push(CompareItem {
                rel: rel.clone(),
                status: CompareStatus::New,
                local_size: Some(file.size),
                remote_size: None,
                by: mode.best(),
                by_date: false,
            });
            continue;
        };
        let mut item = CompareItem {
            rel: rel.clone(),
            status: CompareStatus::Changed,
            local_size: Some(file.size),
            remote_size: Some(entry.size),
            by: CompareBy::Size,
            by_date: false,
        };
        if file.size == entry.size {
            if let Some((by, checksum)) = mode
                .digest_for(entry.checksum.as_ref())
                .zip(entry.checksum.clone())
            {
                item.by = by;
                to_hash.push((items.len(), file, checksum, by));
            } else if mode.mtime_fallback {
                item.by_date = true;
                if newer_or_same(entry.modified, file.modified) {
                    item.status = CompareStatus::Same;
                }
            } else {
                item.status = CompareStatus::Same;
            }
        }
        items.push(item);
    }
    for (rel, entry) in remote_files {
        items.push(CompareItem {
            rel,
            status: CompareStatus::RemoteOnly,
            local_size: None,
            remote_size: Some(entry.size),
            by: mode.best(),
            by_date: false,
        });
    }

    scan.to_hash = to_hash.len() as u64;
    progress(scan);
    let mut last = tokio::time::Instant::now();
    let mut hashed = stream::iter(to_hash)
        .map(|(i, file, checksum, by)| async move {
            let same = digest(hashes, file).await.is_ok_and(|digests| {
                let local = match by {
                    CompareBy::Md5 => hex::encode(digests.md5),
                    _ => hex::encode(digests.sha256),
                };
                checksum.hex.eq_ignore_ascii_case(&local)
            });
            (i, same)
        })
        .buffer_unordered(HASH_AT_ONCE);
    while let Some((i, same)) = hashed.next().await {
        if same {
            items[i].status = CompareStatus::Same;
        }
        scan.hashed += 1;
        if last.elapsed() >= PROGRESS_EVERY {
            last = tokio::time::Instant::now();
            progress(scan);
        }
    }
    drop(hashed);
    progress(scan);

    items.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(CompareResult {
        local_root: local_root.to_owned(),
        remote_root: remote_root.clone(),
        items,
        remote_folders: remote.folders,
        skipped_local: local.skipped,
        secrets: local.secrets,
        regions: remote.regions,
    })
}

/// Whether the site's copy is at least as new as the local file, to the second. A date missing
/// on either side can't tell, so the file counts as changed.
fn newer_or_same(remote: Option<DateTime<Utc>>, local: Option<SystemTime>) -> bool {
    match (remote, local) {
        (Some(remote), Some(local)) => {
            remote.timestamp() >= DateTime::<Utc>::from(local).timestamp()
        }
        _ => false,
    }
}

/// A local file's digests, from the cache or read on a blocking thread.
async fn digest(cache: &HashCache, file: &LocalFile) -> io::Result<FileHashes> {
    if let Some(hashes) = file
        .modified
        .and_then(|modified| cache.cached(&file.path, file.size, modified))
    {
        return Ok(hashes);
    }
    let path = file.path.clone();
    let hashes = tokio::task::spawn_blocking(move || hash_file(&path))
        .await
        .unwrap_or_else(|_| Err(io::Error::other(messages::HASHING_STOPPED)))?;
    cache.keep(&file.path, &hashes);
    Ok(hashes)
}

/// Tells the local walk to stop when the compare is dropped.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct LocalFile {
    path: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
}

/// The local side of a compare.
#[derive(Debug, Default)]
struct LocalTree {
    files: BTreeMap<String, LocalFile>,
    skipped: u64,
    secrets: Vec<String>,
    /// Links to folders, which aren't followed (nor uploaded), and links that lead nowhere: the
    /// site's side of them is left out too, so a publish never deletes it.
    left_out: Vec<String>,
}

impl LocalTree {
    /// Walks `root`, counting the files found into `found`, until `stop` is set.
    fn walk(
        root: &Path,
        skip: &SkipRules,
        found: &AtomicU64,
        stop: &AtomicBool,
    ) -> Result<Self, CompareError> {
        let mut tree = Self::default();
        let mut waiting = vec![(root.to_owned(), String::new())];
        while let Some((dir, rel_dir)) = waiting.pop() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let children = fs::read_dir(extended(&dir)).map_err(|e| local_error(&dir, &e))?;
            for child in children {
                let child = child.map_err(|e| local_error(&dir, &e))?;
                let name = child.file_name();
                // Joined to the path as given, not the `\\?\` form the folder was read through.
                let path = dir.join(&name);
                let Some(name) = name.to_str() else {
                    tree.skipped += 1;
                    continue;
                };
                let rel = if rel_dir.is_empty() {
                    name.to_owned()
                } else {
                    format!("{rel_dir}/{name}")
                };
                let kind = child.file_type().map_err(|e| local_error(&path, &e))?;
                let meta = if kind.is_symlink() {
                    match fs::metadata(extended(&path)) {
                        Ok(meta) if !meta.is_dir() => meta,
                        _ => {
                            tree.skipped += 1;
                            tree.left_out.push(rel);
                            continue;
                        }
                    }
                } else if kind.is_dir() {
                    if skip.is_skipped(&rel, true) {
                        tree.skipped += 1;
                    } else {
                        waiting.push((path, rel));
                    }
                    continue;
                } else {
                    child.metadata().map_err(|e| local_error(&path, &e))?
                };
                if skip.is_skipped(&rel, false) {
                    tree.skipped += 1;
                    continue;
                }
                if looks_secret(&rel) {
                    tree.secrets.push(rel.clone());
                }
                found.fetch_add(1, Ordering::Relaxed);
                tree.files.insert(
                    rel,
                    LocalFile {
                        path,
                        size: meta.len(),
                        modified: meta.modified().ok(),
                    },
                );
            }
        }
        tree.secrets.sort();
        Ok(tree)
    }
}

fn local_error(path: &Path, error: &io::Error) -> CompareError {
    CompareError::Local {
        path: path.to_owned(),
        message: error.to_string(),
    }
}

/// The site's side of a compare.
#[derive(Debug, Default)]
struct RemoteTree {
    files: BTreeMap<String, Entry>,
    folders: u64,
    regions: u32,
}

impl RemoteTree {
    /// Sorts a walk of `root` into files by relative path, leaving out what the skip rules
    /// skip and what is in `left_out` (or below it).
    fn sort(entries: Vec<Entry>, root: &RemotePath, skip: &SkipRules, left_out: &[String]) -> Self {
        let mut tree = Self::default();
        for entry in entries {
            let Some(rel) = rel_of(&entry.path, root) else {
                continue;
            };
            let outside = left_out.iter().any(|out| {
                rel.strip_prefix(out.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            });
            if outside || skip.is_skipped(rel, entry.is_folder) {
                continue;
            }
            tree.regions = tree
                .regions
                .max(u32::try_from(entry.replicated_regions.len()).unwrap_or(u32::MAX));
            if entry.is_folder {
                tree.folders += 1;
            } else {
                tree.files.insert(rel.to_owned(), entry);
            }
        }
        tree
    }
}

/// `path` relative to `root`, which it is inside.
pub(crate) fn rel_of<'a>(path: &'a RemotePath, root: &RemotePath) -> Option<&'a str> {
    if root.is_root() {
        return Some(path.as_str()).filter(|rel| !rel.is_empty());
    }
    path.as_str()
        .strip_prefix(root.as_str())?
        .strip_prefix('/')
        .filter(|rel| !rel.is_empty())
}
