//! CDN purge: the pull zones in front of a site, a purger for them, and which URLs a publish
//! purges.

use std::collections::BTreeSet;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::copy;
use crate::{AccountId, ProviderError, PurgeMode, PurgeResult, RemotePath};

/// A pull zone. `hostnames` have no scheme.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdnZone {
    pub id: u64,
    pub name: String,
    pub hostnames: Vec<String>,
}

/// The pull zones of a site, and the account whose key purges them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdnLink {
    pub account: AccountId,
    pub zones: Vec<CdnZone>,
    pub checked_at: DateTime<Utc>,
}

/// Purges URLs or whole zones of a CDN.
#[async_trait]
pub trait CdnPurger: Send + Sync {
    fn zones(&self) -> &[CdnZone];
    /// Rate-limited inside.
    async fn purge_url(&self, url: &str) -> Result<(), ProviderError>;
    async fn purge_zone(&self, id: u64) -> Result<(), ProviderError>;
}

/// Above this many URLs the publish sheet offers the whole zone instead.
pub const PURGE_URL_LIMIT: usize = 300;

/// `https://{host}/{encoded path}` for every host; `.../index.html` also gives `.../` and `...`;
/// the root `index.html` also gives `/`. Deduplicated and sorted.
///
/// Each segment is percent-encoded, keeping the RFC 3986 unreserved characters
/// (`a b/ü.html` → `a%20b/%C3%BC.html`). The paths are full remote paths: a pull zone serves its
/// storage zone from the root.
pub fn purge_urls(zones: &[CdnZone], paths: &[RemotePath]) -> Vec<String> {
    let forms: Vec<String> = paths.iter().flat_map(url_paths).collect();
    let mut urls = BTreeSet::new();
    for host in zones.iter().flat_map(|zone| &zone.hostnames) {
        let host = host.trim();
        let host = host
            .strip_prefix("https://")
            .or_else(|| host.strip_prefix("http://"))
            .unwrap_or(host)
            .trim_end_matches('/');
        if host.is_empty() {
            continue;
        }
        for form in &forms {
            urls.insert(format!("https://{host}/{form}"));
        }
    }
    urls.into_iter().collect()
}

/// What follows the host's `/` in the URLs that serve `path`: its own, and for an `index.html`
/// its folder's, with and without the trailing `/` (the root's only as `/`).
fn url_paths(path: &RemotePath) -> Vec<String> {
    if path.is_root() {
        return Vec::new();
    }
    let mut forms = vec![path.to_url_path(false)];
    if path.name() == Some("index.html") {
        match path.parent().filter(|dir| !dir.is_root()) {
            Some(dir) => {
                forms.push(dir.to_url_path(true));
                forms.push(dir.to_url_path(false));
            }
            None => forms.push(String::new()),
        }
    }
    forms
}

/// Off → Off. Changed → every [`purge_urls`] URL, going on past failures
/// (`Failed("n of m failed: …")`). Zone → `purge_zone` for each zone (no zones → Unavailable).
/// `on_progress` hears (done, total) before the first request and after each.
pub async fn run_purge(
    p: &dyn CdnPurger,
    mode: PurgeMode,
    paths: &[RemotePath],
    on_progress: &(dyn Fn(usize, usize) + Send + Sync),
) -> PurgeResult {
    if mode == PurgeMode::Off {
        return PurgeResult::Off;
    }
    let zones = p.zones();
    if zones.is_empty() {
        return PurgeResult::Unavailable(copy::NO_PULL_ZONE.to_owned());
    }
    let mut failed = 0;
    let mut first: Option<ProviderError> = None;
    let total;
    match mode {
        PurgeMode::Changed => {
            let urls = purge_urls(zones, paths);
            total = urls.len();
            on_progress(0, total);
            for (i, url) in urls.iter().enumerate() {
                if let Err(error) = p.purge_url(url).await {
                    failed += 1;
                    first.get_or_insert(error);
                }
                on_progress(i + 1, total);
            }
        }
        PurgeMode::Zone | PurgeMode::Off => {
            let ids: Vec<u64> = zones.iter().map(|zone| zone.id).collect();
            total = ids.len();
            on_progress(0, total);
            for (i, id) in ids.into_iter().enumerate() {
                if let Err(error) = p.purge_zone(id).await {
                    failed += 1;
                    first.get_or_insert(error);
                }
                on_progress(i + 1, total);
            }
        }
    }
    match first {
        Some(error) => PurgeResult::Failed(copy::purge_failed(failed, total, &error.to_string())),
        None if mode == PurgeMode::Zone => PurgeResult::Zone,
        None => PurgeResult::Urls(total as u64),
    }
}
