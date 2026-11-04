//! Bunny CDN: the pull zones in front of a storage zone, and purging their URLs or the whole
//! zone through the account API.
//!
//! [`BunnyCdn`] keeps under Bunny's limits on its own: a token bucket for URL purges and another
//! for whole zones, at most [`IN_FLIGHT`] requests at once however many callers there are, and a
//! 429 is waited out (its Retry-After, clamped) and tried again, three tries in all. Each
//! [`BunnyAccount`] call stays a single attempt.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use httporter_core::{CdnPurger, CdnZone, ProviderError, encode_segment};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::account::BunnyAccount;

/// URL purges: a burst of this many, then [`URL_RATE`] a second.
const URL_BURST: u32 = 120;
const URL_RATE: f64 = 5.0;
/// Whole-zone purges: a burst of this many, then [`ZONE_RATE`] a second.
const ZONE_BURST: u32 = 20;
const ZONE_RATE: f64 = 0.5;
/// Purge requests on their way at once.
const IN_FLIGHT: usize = 8;
/// Tries of one purge that Bunny answers with 429.
const TRIES: u32 = 3;
/// The wait after a 429 that names none, and the longest wait whatever it names.
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(2);
const RATE_LIMIT_WAIT_MAX: Duration = Duration::from_secs(30);

impl BunnyAccount {
    /// The pull zones (OriginType 2) whose StorageZoneId is the zone named `storage_zone` (via
    /// `list_zones`); NotFound if none.
    ///
    /// The name must match exactly: Bunny's zone names are case-sensitive.
    pub async fn cdn_zones_for(&self, storage_zone: &str) -> Result<Vec<CdnZone>, ProviderError> {
        let list = self.list_zones().await?;
        let zone = list
            .zones
            .into_iter()
            .find(|zone| zone.name == storage_zone)
            .ok_or(ProviderError::NotFound)?;
        let zones: Vec<CdnZone> = zone
            .pull_zones
            .into_iter()
            .map(|pull_zone| CdnZone {
                id: pull_zone.id,
                name: pull_zone.name,
                hostnames: pull_zone.hostnames,
            })
            .collect();
        if zones.is_empty() {
            return Err(ProviderError::NotFound);
        }
        Ok(zones)
    }

    /// `POST /purge?url=<enc>&async=false`: Bunny answers once the URL is purged.
    pub async fn purge_url(&self, url: &str) -> Result<(), ProviderError> {
        let query = format!("/purge?url={}&async=false", encode_segment(url));
        self.post(&query, None).await
    }

    /// `POST /pullzone/{id}/purgeCache`, with an empty JSON body.
    pub async fn purge_pull_zone(&self, id: u64) -> Result<(), ProviderError> {
        self.post(&format!("/pullzone/{id}/purgeCache"), Some("{}"))
            .await
    }
}

/// Purges a site's pull zones. Rate-limited: URLs 120 burst + 5/s, zones 20 burst + 0.5/s. A 429
/// waits Retry-After (clamped), at most 3 tries.
pub struct BunnyCdn {
    account: BunnyAccount,
    zones: Vec<CdnZone>,
    urls: TokenBucket,
    zone_purges: TokenBucket,
    in_flight: Semaphore,
}

impl BunnyCdn {
    pub fn new(account: BunnyAccount, zones: Vec<CdnZone>) -> Self {
        Self {
            account,
            zones,
            urls: TokenBucket::new(URL_BURST, URL_RATE),
            zone_purges: TokenBucket::new(ZONE_BURST, ZONE_RATE),
            in_flight: Semaphore::new(IN_FLIGHT),
        }
    }

    /// Runs `call` once `bucket` has a token and a request slot is free, again after a 429 (up to
    /// [`TRIES`] tries in all).
    async fn limited<F, Fut>(&self, bucket: &TokenBucket, call: F) -> Result<(), ProviderError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<(), ProviderError>>,
    {
        let mut tries = 0;
        loop {
            bucket.take().await;
            tries += 1;
            let answer = {
                // Never closed, so a permit always comes.
                let _slot = self.in_flight.acquire().await;
                call().await
            };
            match answer {
                Err(ProviderError::RateLimited { retry_after }) if tries < TRIES => {
                    let wait = retry_after
                        .unwrap_or(RATE_LIMIT_WAIT)
                        .min(RATE_LIMIT_WAIT_MAX);
                    tokio::time::sleep(wait).await;
                }
                other => return other,
            }
        }
    }
}

#[async_trait]
impl CdnPurger for BunnyCdn {
    fn zones(&self) -> &[CdnZone] {
        &self.zones
    }

    async fn purge_url(&self, url: &str) -> Result<(), ProviderError> {
        self.limited(&self.urls, || self.account.purge_url(url))
            .await
    }

    async fn purge_zone(&self, id: u64) -> Result<(), ProviderError> {
        self.limited(&self.zone_purges, || self.account.purge_pull_zone(id))
            .await
    }
}

/// A token bucket on tokio's clock: `burst` tokens to start with, refilled at `rate` a second up
/// to `burst`. A taker reserves its token at once, even one not there yet, and then waits until
/// it would have been: takers are served in the order they came.
struct TokenBucket {
    burst: f64,
    rate: f64,
    state: Mutex<BucketState>,
}

struct BucketState {
    /// Below zero when tokens are reserved ahead.
    tokens: f64,
    at: Instant,
}

impl TokenBucket {
    fn new(burst: u32, rate: f64) -> Self {
        Self {
            burst: f64::from(burst),
            rate,
            state: Mutex::new(BucketState {
                tokens: f64::from(burst),
                at: Instant::now(),
            }),
        }
    }

    /// Takes a token, waiting until there is one.
    async fn take(&self) {
        let wait = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let refill = now.duration_since(state.at).as_secs_f64() * self.rate;
            state.tokens = (state.tokens + refill).min(self.burst);
            state.at = now;
            state.tokens -= 1.0;
            if state.tokens >= 0.0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64(-state.tokens / self.rate)
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}
