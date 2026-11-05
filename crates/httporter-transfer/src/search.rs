//! Searching a site by name (T3): every folder from a root is listed, and the matches stream out
//! as each folder comes in.
#![allow(unused_variables, dead_code)]

use chrono::{DateTime, Utc};
use httporter_core::{Entry, ProviderError, RemotePath, StorageProvider};

/// A kind of file, by the media type of its extension (`core::mime`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KindFilter {
    Image,
    Code,
    Document,
    Archive,
    Media,
}

/// What to look for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// A substring of the name, any case; a glob (`glob_match`) when it has '*' or '?'.
    pub text: String,
    pub kind: Option<KindFilter>,
    pub min_size: Option<u64>,
    pub modified_after: Option<DateTime<Utc>>,
}

/// How far a search is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchProgress {
    pub folders: u64,
    pub found: u64,
    pub done: bool,
}

/// A search stops after this many matches.
pub const SEARCH_LIMIT: usize = 10_000;

/// Lists from `root` with `caps.list_concurrency`; each folder's matches (files and folders) go
/// to `on_found` as soon as it is listed; stops at [`SEARCH_LIMIT`]; cancel = drop.
pub async fn search(
    provider: &dyn StorageProvider,
    root: &RemotePath,
    q: &SearchQuery,
    on_found: &(dyn Fn(Vec<Entry>, SearchProgress) + Send + Sync),
) -> Result<SearchProgress, ProviderError> {
    todo!("T3")
}
