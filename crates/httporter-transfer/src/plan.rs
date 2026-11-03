//! Planning a transfer before it starts: what to upload, download or delete, which files are
//! already there (conflicts, for the conflict sheet) and what can't be sent at all (skipped,
//! with a short reason).
//!
//! Remote folders are listed under the provider's own limits (`list_concurrency`). Local files
//! are read only when a conflict needs their digest, 8 at a time, off the runtime's workers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use httporter_core::{
    Checksum, ChecksumKind, CompareBy, Conflict, ConflictPolicy, DeleteSummary, EmptyFolders,
    Entry, Job, ProviderError, RemotePath, Resolution, StorageProvider, walk,
};

use crate::hash::{FileHashes, extended, hash_file};
use crate::lane::remote_of;
use crate::names::{first_free, keep_both_name, safe_local_name};

/// How many local files are hashed at once to compare them.
const HASH_AT_ONCE: usize = 8;

/// Why something is skipped or refused: the only words the planner writes, short fixed
/// phrases the app shows as they are. Everything else skipped carries the OS's own message.
pub mod reason {
    /// The name isn't valid Unicode, so the transfer journal can't record it (ledger 164).
    pub const NAME: &str = "Its name has characters HTTPorter can't handle.";
    /// A file would go where the site has a folder.
    pub const FOLDER_THERE: &str = "The site has a folder with this name.";
    /// A folder would go where the site has a file.
    pub const FILE_THERE: &str = "The site has a file with this name.";
    /// A link to a folder elsewhere, which an upload doesn't follow.
    pub const LINKED_FOLDER: &str = "Linked folders aren't uploaded.";
    /// An empty folder, on storage that can't keep one.
    pub const EMPTY_FOLDER: &str = "This storage can't keep an empty folder.";
    /// Another item chosen for the same upload has the same name.
    pub const SAME_NAME: &str = "Another item in this upload has the same name.";
    /// A delete that includes the site's top folder is refused whole.
    pub const ROOT_DELETE: &str = "The top folder of a site can't be deleted.";
}

/// What an upload will do. `jobs` holds the empty folders to make and the files new to the
/// site; `conflicts` the files already there, which [`resolve_uploads`] turns into jobs once
/// the user has chosen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadPlan {
    pub jobs: Vec<Job>,
    pub conflicts: Vec<Conflict>,
    /// Local items that won't be sent, and why: the OS's message or a [`reason`].
    pub skipped: Vec<(PathBuf, String)>,
    /// The files to send, conflicts included, and their size.
    pub files: u64,
    pub bytes: u64,
    /// The folders below the target that the upload makes, sorted.
    pub folders_to_create: Vec<RemotePath>,
    /// The names in use in each remote folder that holds a conflict, on the site or planned,
    /// so a copy kept beside an existing file gets a name of its own.
    pub taken: HashMap<RemotePath, HashSet<String>>,
}

/// What a download will do. `jobs` holds the files new to the PC; `conflicts` those already
/// there, which [`resolve_downloads`] turns into jobs once the user has chosen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadPlan {
    pub jobs: Vec<Job>,
    pub conflicts: Vec<Conflict>,
    /// Items given another name on the PC, sorted: a name Windows can't store, or one that
    /// another item of the download (or a local item of the other kind) already has, ignoring
    /// case. Every item of a group that shares a name is listed, the one that keeps it too.
    pub renamed: Vec<(RemotePath, String)>,
    /// The files to fetch, conflicts included, and their size.
    pub files: u64,
    pub bytes: u64,
    /// Each file's listed SHA-256 or MD5, for the engine's `Batch` to check what arrives
    /// (ruling Q27).
    pub checksums: HashMap<RemotePath, Checksum>,
    /// The names in use, lowercase (Windows ignores case), in each local folder that holds a
    /// conflict.
    pub taken: HashMap<PathBuf, HashSet<String>>,
}

/// What a delete will remove: the summary for the delete alert, and the jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletePlan {
    pub summary: DeleteSummary,
    pub jobs: Vec<Job>,
}

/// The resolution a conflict policy chooses for every conflict; `None` for
/// [`ConflictPolicy::Ask`], which shows the conflict sheet.
pub fn policy_to_resolution(p: ConflictPolicy) -> Option<Resolution> {
    match p {
        ConflictPolicy::Ask => None,
        ConflictPolicy::Replace => Some(Resolution::Replace),
        ConflictPolicy::ReplaceIfDifferent => Some(Resolution::ReplaceIfDifferent),
        ConflictPolicy::Skip => Some(Resolution::Skip),
        ConflictPolicy::KeepBoth => Some(Resolution::KeepBoth),
    }
}

/// Plans uploading `sources` (files and folders on the PC) into the folder `target`: each
/// source lands at `target/<its name>`, a folder with everything in it.
///
/// Only `target` and the folders of the upload that the site already has are listed. Links to
/// folders aren't followed, and names that aren't valid Unicode are skipped (the journal can't
/// record them). Paths longer than 260 characters work.
pub async fn plan_upload(
    provider: &dyn StorageProvider,
    sources: &[PathBuf],
    target: &RemotePath,
) -> Result<UploadPlan, ProviderError> {
    let scan = {
        let (sources, target) = (sources.to_vec(), target.clone());
        blocking(move || Scan::sources(&sources, &target)).await?
    };
    let mut items = scan.items;
    items.sort_by(|a, b| {
        a.source
            .cmp(&b.source)
            .then_with(|| a.remote.cmp(&b.remote))
    });
    let site = RemoteTree::list(provider, target, &items).await?;
    let keeps_empty = matches!(
        provider.capabilities().empty_folders,
        EmptyFolders::Supported | EmptyFolders::Markers
    );

    let mut plan = UploadPlan {
        skipped: scan.skipped,
        ..UploadPlan::default()
    };
    let mut creates = Vec::new();
    let mut uploads = Vec::new();
    let mut candidates = Vec::new();
    // Local folders where the site has a file: nothing in them goes.
    let mut dropped: Vec<RemotePath> = Vec::new();
    for item in items {
        if dropped.iter().any(|folder| item.remote.is_within(folder)) {
            continue;
        }
        let there = site.entries.get(&item.remote);
        match item.kind {
            LocalKind::File { size, modified } => match there {
                Some(entry) if entry.is_folder => {
                    plan.skipped
                        .push((item.local, reason::FOLDER_THERE.to_owned()));
                }
                Some(entry) => candidates.push(Candidate {
                    name: entry.name().to_owned(),
                    local: item.local,
                    size,
                    modified,
                    entry: entry.clone(),
                }),
                None => uploads.push(Job::Upload {
                    local: item.local,
                    remote: item.remote,
                    size,
                }),
            },
            LocalKind::Folder { empty } => match there {
                Some(entry) if !entry.is_folder => {
                    plan.skipped
                        .push((item.local, reason::FILE_THERE.to_owned()));
                    dropped.push(item.remote);
                }
                // There already, or made by the uploads into it.
                Some(_) => {}
                None if !empty => {}
                None if keeps_empty => creates.push(Job::CreateFolder {
                    remote: item.remote,
                }),
                None => plan
                    .skipped
                    .push((item.local, reason::EMPTY_FOLDER.to_owned())),
            },
        }
    }

    let compared = compare(&candidates).await;
    for (candidate, outcome) in candidates.into_iter().zip(compared) {
        match outcome {
            Ok(outcome) => {
                let index = plan.conflicts.len();
                plan.conflicts.push(candidate.conflict(index, outcome));
            }
            // A file that can't be read can't be sent either.
            Err(error) => plan.skipped.push((candidate.local, error.to_string())),
        }
    }

    let mut to_create = BTreeSet::new();
    for job in creates.iter().chain(&uploads) {
        if let Job::CreateFolder { remote } = job {
            to_create.insert(remote.clone());
        }
        let mut dir = remote_of(job).parent();
        while let Some(folder) = dir {
            if folder == *target || !folder.is_within(target) || site.has_folder(&folder) {
                break;
            }
            dir = folder.parent();
            to_create.insert(folder);
        }
    }

    let mut taken: HashMap<RemotePath, HashSet<String>> = plan
        .conflicts
        .iter()
        .filter_map(|conflict| conflict.remote.parent())
        .map(|dir| (dir, HashSet::new()))
        .collect();
    let in_use = site
        .entries
        .keys()
        .chain(creates.iter().chain(&uploads).map(remote_of))
        .chain(&to_create);
    for path in in_use {
        if let (Some(dir), Some(name)) = (path.parent(), path.name())
            && let Some(names) = taken.get_mut(&dir)
        {
            names.insert(name.to_owned());
        }
    }

    plan.files = (uploads.len() + plan.conflicts.len()) as u64;
    plan.bytes = uploads.iter().map(upload_size).sum::<u64>()
        + plan.conflicts.iter().map(|c| c.local_size).sum::<u64>();
    plan.folders_to_create = to_create.into_iter().collect();
    plan.taken = taken;
    plan.jobs = creates;
    plan.jobs.extend(uploads);
    Ok(plan)
}

/// The plan's jobs plus a job for each conflict as resolved: the row's own choice in `per_row`
/// (by conflict index), otherwise `all`. Replace-if-different sends a file unless it is known
/// to be the same; keep-both sends it under a name not yet used in its folder.
pub fn resolve_uploads(
    plan: UploadPlan,
    all: Resolution,
    per_row: &HashMap<usize, Resolution>,
) -> Vec<Job> {
    let UploadPlan {
        mut jobs,
        conflicts,
        mut taken,
        ..
    } = plan;
    for conflict in conflicts {
        let remote = match per_row.get(&conflict.index).copied().unwrap_or(all) {
            Resolution::Replace => conflict.remote,
            Resolution::ReplaceIfDifferent if conflict.same != Some(true) => conflict.remote,
            Resolution::ReplaceIfDifferent | Resolution::Skip => continue,
            Resolution::KeepBoth => {
                let dir = conflict.remote.parent().unwrap_or_else(RemotePath::root);
                let names = taken.entry(dir.clone()).or_default();
                let name = keep_both_name(&conflict.name, names);
                let Ok(remote) = dir.join(&name) else {
                    continue;
                };
                names.insert(name);
                remote
            }
        };
        jobs.push(Job::Upload {
            local: conflict.local,
            remote,
            size: conflict.local_size,
        });
    }
    jobs
}

/// Plans downloading `entries` (files and folders as a listing shows them) into `local_dir`:
/// each lands at `local_dir\<its name>`, a folder with everything in it.
///
/// Names become ones Windows can store ([`safe_local_name`]), and names that would clash on
/// the PC, which ignores case, are numbered; both are listed in `renamed`. A file already on
/// the PC is a conflict.
pub async fn plan_download(
    provider: &dyn StorageProvider,
    entries: &[Entry],
    local_dir: &Path,
) -> Result<DownloadPlan, ProviderError> {
    let mut remote: Vec<(Entry, bool)> = Vec::new();
    for entry in outermost(entries) {
        if entry.is_folder {
            let inside = walk(provider, &entry.path).await?;
            remote.extend(inside.into_iter().map(|inside| (inside, false)));
        }
        if !entry.path.is_root() {
            remote.push((entry.clone(), true));
        }
    }
    // A folder sorts before everything in it, so it is named first.
    remote.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    let local_dir = local_dir.to_owned();
    let named = blocking(move || Named::download(remote, &local_dir)).await?;

    let compared = compare(&named.candidates).await;
    let mut conflicts = Vec::with_capacity(named.candidates.len());
    for (candidate, outcome) in named.candidates.into_iter().zip(compared) {
        // A local file that can't be read can't be told apart from the remote one.
        let outcome =
            outcome.unwrap_or_else(|_| (None, compared_by(candidate.entry.checksum.as_ref())));
        let index = conflicts.len();
        conflicts.push(candidate.conflict(index, outcome));
    }
    Ok(DownloadPlan {
        jobs: named.jobs,
        conflicts,
        renamed: named.renamed.into_iter().collect(),
        files: named.files,
        bytes: named.bytes,
        checksums: named.checksums,
        taken: named.taken,
    })
}

/// As [`resolve_uploads`], for a download: a kept copy gets a name not yet used in its local
/// folder, ignoring case.
pub fn resolve_downloads(
    plan: DownloadPlan,
    all: Resolution,
    per_row: &HashMap<usize, Resolution>,
) -> Vec<Job> {
    let DownloadPlan {
        mut jobs,
        conflicts,
        mut taken,
        ..
    } = plan;
    for conflict in conflicts {
        let local = match per_row.get(&conflict.index).copied().unwrap_or(all) {
            Resolution::Replace => conflict.local,
            Resolution::ReplaceIfDifferent if conflict.same != Some(true) => conflict.local,
            Resolution::ReplaceIfDifferent | Resolution::Skip => continue,
            Resolution::KeepBoth => {
                let dir = conflict
                    .local
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                let names = taken.entry(dir.clone()).or_default();
                let name = first_free(&conflict.name, 1, |name| {
                    names.contains(&name.to_lowercase())
                });
                names.insert(name.to_lowercase());
                dir.join(name)
            }
        };
        jobs.push(Job::Download {
            remote: conflict.remote,
            local,
            size: conflict.remote_size,
        });
    }
    jobs
}

/// Plans deleting `entries`, with the summary the delete alert shows. A selection that
/// includes the site's top folder is refused before anything is listed.
///
/// A folder goes in one `Delete { folder: true }`. On storage without `recursive_delete` the
/// files inside it go first, one job each, and the folder's job comes last, after every file
/// (ruling Q38): the engine runs it once the files are gone, and `delete_folder` then removes
/// what is left (folder markers).
pub async fn plan_delete(
    provider: &dyn StorageProvider,
    entries: &[Entry],
) -> Result<DeletePlan, ProviderError> {
    if entries.iter().any(|entry| entry.path.is_root()) {
        return Err(ProviderError::Refused(reason::ROOT_DELETE.to_owned()));
    }
    let folders = entries.iter().filter(|entry| entry.is_folder).count() as u64;
    let mut summary = DeleteSummary {
        folders,
        files: entries.len() as u64 - folders,
        total_files: 0,
        total_bytes: 0,
        single: match entries {
            [only] => Some((only.name().to_owned(), only.is_folder)),
            _ => None,
        },
    };
    let recursive = provider.capabilities().recursive_delete;
    let mut files = BTreeSet::new();
    let mut folder_jobs = BTreeSet::new();
    for entry in outermost(entries) {
        if !entry.is_folder {
            summary.total_files += 1;
            summary.total_bytes += entry.size;
            files.insert(entry.path.clone());
            continue;
        }
        for inside in walk(provider, &entry.path).await? {
            if inside.is_folder {
                continue;
            }
            summary.total_files += 1;
            summary.total_bytes += inside.size;
            if !recursive {
                files.insert(inside.path);
            }
        }
        folder_jobs.insert(entry.path.clone());
    }
    let jobs = files
        .into_iter()
        .map(|remote| Job::Delete {
            remote,
            folder: false,
        })
        .chain(folder_jobs.into_iter().map(|remote| Job::Delete {
            remote,
            folder: true,
        }))
        .collect();
    Ok(DeletePlan { summary, jobs })
}

/// The entries that aren't inside another folder among them, each once.
fn outermost(entries: &[Entry]) -> Vec<&Entry> {
    let folders: Vec<&RemotePath> = entries
        .iter()
        .filter(|entry| entry.is_folder)
        .map(|entry| &entry.path)
        .collect();
    let mut seen = HashSet::new();
    entries
        .iter()
        .filter(|entry| {
            !folders
                .iter()
                .any(|&folder| entry.path != *folder && entry.path.is_within(folder))
        })
        .filter(|entry| seen.insert(&entry.path))
        .collect()
}

/// Runs blocking work (the local file system) off the runtime's workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ProviderError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ProviderError::Permanent(format!("planning stopped: {e}")))
}

fn upload_size(job: &Job) -> u64 {
    match job {
        Job::Upload { size, .. } => *size,
        _ => 0,
    }
}

/// A local item to upload, as found.
#[derive(Debug, Clone, Copy)]
enum LocalKind {
    File {
        size: u64,
        modified: Option<SystemTime>,
    },
    /// `empty`: nothing at all is in it.
    Folder { empty: bool },
}

#[derive(Debug)]
struct LocalItem {
    /// Which source it came from, to keep the sources' order.
    source: usize,
    local: PathBuf,
    remote: RemotePath,
    kind: LocalKind,
}

/// The local side of an upload: every file and folder to send, and what can't be.
#[derive(Debug, Default)]
struct Scan {
    items: Vec<LocalItem>,
    skipped: Vec<(PathBuf, String)>,
}

impl Scan {
    fn sources(sources: &[PathBuf], target: &RemotePath) -> Self {
        let mut scan = Self::default();
        let mut roots: HashMap<RemotePath, &Path> = HashMap::new();
        for (source, path) in sources.iter().enumerate() {
            // The whole path must be valid Unicode for the journal to record it.
            let name = path
                .to_str()
                .and(path.file_name())
                .and_then(|name| name.to_str());
            let Some(remote) = name.and_then(|name| target.join(name).ok()) else {
                scan.skip(path.clone(), reason::NAME);
                continue;
            };
            match roots.get(&remote) {
                Some(&first) if first == path.as_path() => continue,
                Some(_) => {
                    scan.skip(path.clone(), reason::SAME_NAME);
                    continue;
                }
                None => {
                    roots.insert(remote.clone(), path);
                }
            }
            match fs::symlink_metadata(extended(path)) {
                Ok(meta) => scan.visit(source, path.clone(), remote, meta.file_type(), || Ok(meta)),
                Err(error) => scan.skip(path.clone(), error.to_string()),
            }
        }
        scan
    }

    fn skip(&mut self, path: PathBuf, why: impl Into<String>) {
        self.skipped.push((path, why.into()));
    }

    /// One local item, given its type as found (links not followed) and a way to its
    /// metadata. A link to a folder is skipped; a link to a file sends the file.
    fn visit(
        &mut self,
        source: usize,
        local: PathBuf,
        remote: RemotePath,
        kind: FileType,
        meta: impl FnOnce() -> io::Result<Metadata>,
    ) {
        let meta = if kind.is_symlink() {
            match fs::metadata(extended(&local)) {
                Ok(meta) if meta.is_dir() => {
                    self.skip(local, reason::LINKED_FOLDER);
                    return;
                }
                other => other,
            }
        } else if kind.is_dir() {
            self.folder(source, local, remote);
            return;
        } else {
            meta()
        };
        match meta {
            Ok(meta) => self.items.push(LocalItem {
                source,
                local,
                remote,
                kind: LocalKind::File {
                    size: meta.len(),
                    modified: meta.modified().ok(),
                },
            }),
            Err(error) => self.skip(local, error.to_string()),
        }
    }

    /// A local folder and everything in it, unless it can't be read.
    fn folder(&mut self, source: usize, local: PathBuf, remote: RemotePath) {
        let children = match fs::read_dir(extended(&local)) {
            Ok(children) => children,
            Err(error) => {
                self.skip(local, error.to_string());
                return;
            }
        };
        let mut found = Vec::new();
        let mut unreadable = false;
        for child in children {
            match child {
                Ok(child) => found.push(child),
                Err(error) => {
                    unreadable = true;
                    self.skip(local.clone(), error.to_string());
                }
            }
        }
        found.sort_by_key(fs::DirEntry::file_name);
        self.items.push(LocalItem {
            source,
            local: local.clone(),
            remote: remote.clone(),
            kind: LocalKind::Folder {
                empty: found.is_empty() && !unreadable,
            },
        });
        for child in found {
            let name = child.file_name();
            // Joined to the path as given, not the `\\?\` form the folder was read through.
            let path = local.join(&name);
            let Some(child_remote) = name.to_str().and_then(|name| remote.join(name).ok()) else {
                self.skip(path, reason::NAME);
                continue;
            };
            match child.file_type() {
                Ok(kind) => self.visit(source, path, child_remote, kind, || child.metadata()),
                Err(error) => self.skip(path, error.to_string()),
            }
        }
    }
}

/// What the site has where an upload goes.
#[derive(Debug, Default)]
struct RemoteTree {
    /// Every entry of the folders listed, by path.
    entries: HashMap<RemotePath, Entry>,
    /// The folders listed (so they exist).
    listed: HashSet<RemotePath>,
}

impl RemoteTree {
    /// Lists `target`, then each folder of the upload that the site has, level by level, at
    /// most `list_concurrency` at once. A folder the site doesn't have is never listed, nor
    /// anything below it.
    async fn list(
        provider: &dyn StorageProvider,
        target: &RemotePath,
        items: &[LocalItem],
    ) -> Result<Self, ProviderError> {
        let mut folders_in: HashMap<RemotePath, Vec<&RemotePath>> = HashMap::new();
        for item in items {
            if matches!(item.kind, LocalKind::Folder { .. })
                && let Some(parent) = item.remote.parent()
            {
                folders_in.entry(parent).or_default().push(&item.remote);
            }
        }
        let limit = provider.capabilities().list_concurrency.max(1);
        let mut tree = Self::default();
        let mut level = vec![target.clone()];
        while !level.is_empty() {
            let listings: Vec<_> = stream::iter(level)
                .map(|dir| async move {
                    let listing = provider.list(&dir).await;
                    (dir, listing)
                })
                .buffer_unordered(limit)
                .collect()
                .await;
            let mut next = Vec::new();
            for (dir, listing) in listings {
                let listing = match listing {
                    Ok(listing) => listing,
                    Err(ProviderError::NotFound) => continue,
                    Err(error) => return Err(error),
                };
                for entry in listing.entries {
                    if entry.path.parent().as_ref() == Some(&dir) {
                        tree.entries.insert(entry.path.clone(), entry);
                    }
                }
                for &folder in folders_in.get(&dir).into_iter().flatten() {
                    if tree
                        .entries
                        .get(folder)
                        .is_some_and(|entry| entry.is_folder)
                    {
                        next.push(folder.clone());
                    }
                }
                tree.listed.insert(dir);
            }
            level = next;
        }
        Ok(tree)
    }

    fn has_folder(&self, path: &RemotePath) -> bool {
        self.listed.contains(path) || self.entries.get(path).is_some_and(|entry| entry.is_folder)
    }
}

/// How a conflict compared: whether both sides hold the same contents (`None`: couldn't tell),
/// and how that was told.
type Compared = (Option<bool>, CompareBy);

/// A file on both sides: one conflict, once compared.
#[derive(Debug)]
struct Candidate {
    /// The name the conflict sheet shows: the file's name where it would go.
    name: String,
    local: PathBuf,
    size: u64,
    modified: Option<SystemTime>,
    /// The remote file.
    entry: Entry,
}

impl Candidate {
    fn conflict(self, index: usize, (same, compared_by): Compared) -> Conflict {
        Conflict {
            index,
            name: self.name,
            local: self.local,
            remote: self.entry.path,
            local_size: self.size,
            local_modified: self.modified.map(DateTime::<Utc>::from),
            remote_size: self.entry.size,
            remote_modified: self.entry.modified,
            same,
            compared_by,
        }
    }
}

/// Whether each candidate's two sides hold the same contents, and how that was told. Sizes
/// that differ settle it; otherwise the local file is hashed (8 at a time) when the listing
/// gives a SHA-256 or MD5 to compare with, and equal sizes are all there is when it doesn't.
async fn compare(candidates: &[Candidate]) -> Vec<io::Result<Compared>> {
    let mut outcomes: Vec<Option<io::Result<Compared>>> = Vec::new();
    let mut to_hash = Vec::new();
    for (i, candidate) in candidates.iter().enumerate() {
        let outcome = without_reading(candidate.size, &candidate.entry);
        if outcome.is_none() {
            to_hash.push((i, candidate.local.clone()));
        }
        outcomes.push(outcome.map(Ok));
    }
    let hashed: Vec<(usize, io::Result<FileHashes>)> = stream::iter(to_hash)
        .map(|(i, path)| async move { (i, hash_local(path).await) })
        .buffer_unordered(HASH_AT_ONCE)
        .collect()
        .await;
    for (i, hashes) in hashed {
        let checksum = candidates[i].entry.checksum.as_ref();
        outcomes[i] = Some(hashes.map(|hashes| by_digest(&hashes, checksum)));
    }
    outcomes
        .into_iter()
        .map(|outcome| outcome.unwrap_or(Ok((None, CompareBy::Size))))
        .collect()
}

/// How a conflict compares without reading the local file, or `None` when it needs the file's
/// digest.
fn without_reading(local_size: u64, remote: &Entry) -> Option<Compared> {
    if local_size != remote.size {
        return Some((Some(false), CompareBy::Size));
    }
    match remote.checksum.as_ref().map(|checksum| checksum.kind) {
        Some(ChecksumKind::Sha256 | ChecksumKind::Md5) => None,
        // A multipart ETag is no digest of the contents: the sizes are all there is.
        Some(ChecksumKind::MultipartEtag) | None => Some((Some(true), CompareBy::Size)),
    }
}

fn by_digest(hashes: &FileHashes, checksum: Option<&Checksum>) -> Compared {
    let by = compared_by(checksum);
    let local = match by {
        CompareBy::Sha256 => hex::encode(hashes.sha256),
        CompareBy::Md5 => hex::encode(hashes.md5),
        CompareBy::Size => return (None, by),
    };
    let same = checksum.is_some_and(|checksum| checksum.hex.eq_ignore_ascii_case(&local));
    (Some(same), by)
}

/// The digest a listing's checksum lets a comparison use.
fn compared_by(checksum: Option<&Checksum>) -> CompareBy {
    match checksum.map(|checksum| checksum.kind) {
        Some(ChecksumKind::Sha256) => CompareBy::Sha256,
        Some(ChecksumKind::Md5) => CompareBy::Md5,
        Some(ChecksumKind::MultipartEtag) | None => CompareBy::Size,
    }
}

async fn hash_local(path: PathBuf) -> io::Result<FileHashes> {
    tokio::task::spawn_blocking(move || hash_file(&path))
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
}

/// A local folder a download writes into.
struct LocalFolder {
    /// What is in it already, by lowercase name.
    existing: HashMap<String, Metadata>,
    /// The names this download gives in it, by lowercase name, with the item each went to.
    given: HashMap<String, (RemotePath, String)>,
}

impl LocalFolder {
    /// Reads what `dir` holds; a folder that isn't there (yet) holds nothing.
    fn read(dir: &Path) -> Self {
        let existing = match fs::read_dir(extended(dir)) {
            Ok(children) => children
                .flatten()
                .filter_map(|child| {
                    let meta = child.metadata().ok()?;
                    Some((child.file_name().to_string_lossy().to_lowercase(), meta))
                })
                .collect(),
            Err(_) => HashMap::new(),
        };
        Self {
            existing,
            given: HashMap::new(),
        }
    }

    /// The name `entry` gets here: its Windows-safe name, or a numbered one when this download
    /// already gave that name (ignoring case) or the PC has an item of the other kind there.
    fn give(&mut self, entry: &Entry, renamed: &mut BTreeMap<RemotePath, String>) -> String {
        let safe = safe_local_name(entry.name());
        let taken = |name: &str| {
            let key = name.to_lowercase();
            self.given.contains_key(&key)
                || self
                    .existing
                    .get(&key)
                    .is_some_and(|meta| meta.is_dir() != entry.is_folder)
        };
        let name = if taken(&safe) {
            first_free(&safe, 2, taken)
        } else {
            safe.clone()
        };
        if let Some((holder, holder_name)) = self.given.get(&safe.to_lowercase()) {
            renamed.insert(holder.clone(), holder_name.clone());
        }
        if name != entry.name() {
            renamed.insert(entry.path.clone(), name.clone());
        }
        self.given
            .insert(name.to_lowercase(), (entry.path.clone(), name.clone()));
        name
    }

    /// A file already here under `name`, ignoring case.
    fn file(&self, name: &str) -> Option<&Metadata> {
        self.existing
            .get(&name.to_lowercase())
            .filter(|meta| !meta.is_dir())
    }
}

/// The local side of a download, once every item has its name.
#[derive(Debug, Default)]
struct Named {
    jobs: Vec<Job>,
    candidates: Vec<Candidate>,
    renamed: BTreeMap<RemotePath, String>,
    files: u64,
    bytes: u64,
    checksums: HashMap<RemotePath, Checksum>,
    taken: HashMap<PathBuf, HashSet<String>>,
}

impl Named {
    /// Names every item of `remote` (sorted by path, each with whether it was picked) inside
    /// `local_dir`, and sorts the files into new ones and conflicts.
    fn download(remote: Vec<(Entry, bool)>, local_dir: &Path) -> Self {
        let mut named = Self::default();
        let mut folders: HashMap<PathBuf, LocalFolder> = HashMap::new();
        let mut local_of: HashMap<RemotePath, PathBuf> =
            HashMap::from([(RemotePath::root(), local_dir.to_owned())]);
        for (entry, picked) in remote {
            let parent = if picked {
                Some(local_dir.to_owned())
            } else {
                entry
                    .path
                    .parent()
                    .and_then(|parent| local_of.get(&parent).cloned())
            };
            let Some(parent) = parent else {
                continue;
            };
            let folder = folders
                .entry(parent.clone())
                .or_insert_with(|| LocalFolder::read(&parent));
            let name = folder.give(&entry, &mut named.renamed);
            let local = parent.join(&name);
            if entry.is_folder {
                local_of.insert(entry.path, local);
                continue;
            }
            named.files += 1;
            named.bytes += entry.size;
            if let Some(checksum) = entry.checksum.as_ref().filter(|checksum| {
                matches!(checksum.kind, ChecksumKind::Sha256 | ChecksumKind::Md5)
            }) {
                named.checksums.insert(entry.path.clone(), checksum.clone());
            }
            match folder.file(&name) {
                Some(meta) => named.candidates.push(Candidate {
                    name,
                    local,
                    size: meta.len(),
                    modified: meta.modified().ok(),
                    entry,
                }),
                None => named.jobs.push(Job::Download {
                    remote: entry.path,
                    local,
                    size: entry.size,
                }),
            }
        }
        for candidate in &named.candidates {
            if let Some(dir) = candidate.local.parent()
                && let Some(folder) = folders.get(dir)
            {
                named.taken.entry(dir.to_owned()).or_insert_with(|| {
                    folder
                        .existing
                        .keys()
                        .chain(folder.given.keys())
                        .cloned()
                        .collect()
                });
            }
        }
        named
    }
}
