//! Comparing a local folder with a folder of a site, file by file: what is new, changed, only on
//! the site or the same.

// T1 skeleton: drop this once T2 fills the bodies.
#![allow(unused_variables)]

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use httporter_core::{
    CompareBy, CompareStatus, ProviderError, ProviderKind, RemotePath, SkipRules, StorageProvider,
};

use crate::hash::FileHashes;

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
        todo!("T2")
    }
}

/// Local file digests by path, size and modification time, kept in memory only.
#[derive(Debug, Default)]
pub struct HashCache {
    #[allow(dead_code)] // T1 skeleton: T2 reads it.
    hashes: Mutex<HashMap<(PathBuf, u64, SystemTime), FileHashes>>,
}

impl HashCache {
    pub fn get_or_hash(&self, path: &Path) -> io::Result<FileHashes> {
        todo!("T2")
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
    pub skipped_local: u64,
    pub secrets: Vec<String>,
    pub regions: u32,
}

impl CompareResult {
    pub fn counts(&self) -> CompareCounts {
        todo!("T2")
    }

    /// The direct children of `rel_dir` ("" = the root). A folder is Changed if anything below
    /// it differs, New if it is only local, RemoteOnly if only remote, else Same.
    pub fn child_statuses(&self, rel_dir: &str) -> HashMap<String, CompareStatus> {
        todo!("T2")
    }

    /// The weakest way any file was compared ("Compared N files by SHA-256 checksum").
    pub fn weakest_method(&self) -> CompareBy {
        todo!("T2")
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
pub async fn compare_trees(
    provider: &dyn StorageProvider,
    local_root: &Path,
    remote_root: &RemotePath,
    skip: &SkipRules,
    mode: CompareMode,
    hashes: &HashCache,
    progress: &(dyn Fn(ScanProgress) + Send + Sync),
) -> Result<CompareResult, CompareError> {
    todo!("T2")
}
