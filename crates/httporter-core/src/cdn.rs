//! CDN purge: the pull zones in front of a site, a purger for them, and which URLs a publish
//! purges.

// T1 skeleton: drop this once T3 fills the bodies.
#![allow(unused_variables)]

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
pub fn purge_urls(zones: &[CdnZone], paths: &[RemotePath]) -> Vec<String> {
    todo!("T3")
}

/// Off → Off. Changed → every [`purge_urls`] URL, going on past failures
/// (`Failed("n of m failed: …")`). Zone → `purge_zone` for each zone (no zones → Unavailable).
pub async fn run_purge(
    p: &dyn CdnPurger,
    mode: PurgeMode,
    paths: &[RemotePath],
    on_progress: &(dyn Fn(usize, usize) + Send + Sync),
) -> PurgeResult {
    todo!("T3")
}
