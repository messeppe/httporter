//! Searching a site by name: every folder from a root is listed, and the matches stream out as
//! each folder comes in.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use httporter_core::{
    Entry, ProviderError, RemotePath, StorageProvider, content_type_for, glob_match,
};

/// A kind of file, by the media type of its extension (`core::mime`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KindFilter {
    Image,
    Code,
    Document,
    Archive,
    Media,
}

impl KindFilter {
    /// The kind of a file called `name`, if it is one of these.
    pub fn of(name: &str) -> Option<Self> {
        let extension = name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();
        if CODE.contains(&extension.as_str()) {
            return Some(Self::Code);
        }
        let mime = content_type_for(name);
        let (top, sub) = mime.split_once('/').unwrap_or((mime.as_str(), ""));
        match top {
            "image" => Some(Self::Image),
            "audio" | "video" => Some(Self::Media),
            "text" if matches!(sub, "plain" | "csv" | "rtf" | "markdown") => Some(Self::Document),
            "text" => Some(Self::Code),
            "application" if ARCHIVES.contains(&sub) => Some(Self::Archive),
            "application"
                if DOCUMENTS.contains(&sub)
                    || sub.starts_with("vnd.openxmlformats")
                    || sub.starts_with("vnd.oasis.opendocument")
                    || sub.starts_with("vnd.ms-") =>
            {
                Some(Self::Document)
            }
            _ => None,
        }
    }
}

/// Extensions that are code whatever media type they are given (`.ts` is a video stream to
/// some tables).
const CODE: &[&str] = &[
    "html",
    "htm",
    "xhtml",
    "css",
    "scss",
    "sass",
    "less",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "tsx",
    "json",
    "xml",
    "php",
    "py",
    "rb",
    "go",
    "rs",
    "java",
    "c",
    "h",
    "cpp",
    "cs",
    "sh",
    "bat",
    "ps1",
    "yml",
    "yaml",
    "toml",
    "ini",
    "conf",
    "htaccess",
    "vue",
    "svelte",
    "sql",
    "map",
    "webmanifest",
    "twig",
    "liquid",
];

const ARCHIVES: &[&str] = &[
    "zip",
    "x-zip-compressed",
    "gzip",
    "x-gzip",
    "x-tar",
    "x-gtar",
    "x-bzip",
    "x-bzip2",
    "x-xz",
    "x-7z-compressed",
    "x-rar-compressed",
    "vnd.rar",
    "zstd",
];

const DOCUMENTS: &[&str] = &["pdf", "msword", "rtf", "epub+zip"];

/// What to look for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// A substring of the name, any case; a glob (`glob_match`) when it has '*' or '?'.
    pub text: String,
    pub kind: Option<KindFilter>,
    pub min_size: Option<u64>,
    pub modified_after: Option<DateTime<Utc>>,
}

impl SearchQuery {
    /// Whether `entry` is a match. A folder matches by its name alone, and only while no
    /// filter that only files have (kind, size, date) is set.
    pub fn matches(&self, entry: &Entry) -> bool {
        let name = entry.name();
        let text = self.text.trim();
        let named = if text.is_empty() {
            true
        } else if text.contains(['*', '?']) {
            glob_match(text, name)
        } else {
            name.to_lowercase().contains(&text.to_lowercase())
        };
        if !named {
            return false;
        }
        if entry.is_folder {
            return self.kind.is_none() && self.min_size.is_none() && self.modified_after.is_none();
        }
        self.kind
            .is_none_or(|kind| KindFilter::of(name) == Some(kind))
            && self.min_size.is_none_or(|min| entry.size >= min)
            && self
                .modified_after
                .is_none_or(|after| entry.modified.is_some_and(|modified| modified >= after))
    }
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
///
/// `on_found` hears of every folder listed, matches or not, so the folder count moves; the
/// last call has `done` set (with no entries unless the limit was reached). A folder gone
/// since its parent was listed is passed over; any other failure ends the search with it.
pub async fn search(
    provider: &dyn StorageProvider,
    root: &RemotePath,
    q: &SearchQuery,
    on_found: &(dyn Fn(Vec<Entry>, SearchProgress) + Send + Sync),
) -> Result<SearchProgress, ProviderError> {
    let limit = provider.capabilities().list_concurrency.max(1);
    let mut waiting = VecDeque::from([root.clone()]);
    let mut listing = FuturesUnordered::new();
    let mut progress = SearchProgress::default();
    loop {
        while listing.len() < limit
            && let Some(dir) = waiting.pop_front()
        {
            listing.push(async move {
                let result = provider.list(&dir).await;
                (dir, result)
            });
        }
        let Some((dir, result)) = listing.next().await else {
            break;
        };
        let entries = match result {
            Ok(listed) => listed.entries,
            Err(ProviderError::NotFound) if dir != *root => Vec::new(),
            Err(error) => return Err(error),
        };
        progress.folders += 1;
        let mut found = Vec::new();
        for entry in entries {
            // As `walk`: the folder itself, or a path from elsewhere, would lead astray.
            if entry.path == dir || !entry.path.is_within(&dir) {
                continue;
            }
            if entry.is_folder {
                waiting.push_back(entry.path.clone());
            }
            if q.matches(&entry) {
                found.push(entry);
            }
        }
        let room = SEARCH_LIMIT.saturating_sub(progress.found as usize);
        if found.len() >= room {
            found.truncate(room);
            progress.found += found.len() as u64;
            progress.done = true;
            on_found(found, progress);
            return Ok(progress);
        }
        progress.found += found.len() as u64;
        on_found(found, progress);
    }
    progress.done = true;
    on_found(Vec::new(), progress);
    Ok(progress)
}
