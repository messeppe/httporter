//! Bunny CDN: the pull zones in front of a storage zone, and purging their URLs or the whole
//! zone through the account API.

// T1 skeleton: drop this once T5 fills the bodies.
#![allow(unused_variables)]

use async_trait::async_trait;
use httporter_core::{CdnPurger, CdnZone, ProviderError};

use crate::account::BunnyAccount;

impl BunnyAccount {
    /// The pull zones (OriginType 2) whose StorageZoneId is the zone named `storage_zone` (via
    /// `list_zones`); NotFound if none.
    pub async fn cdn_zones_for(&self, storage_zone: &str) -> Result<Vec<CdnZone>, ProviderError> {
        todo!("T5")
    }

    /// `POST /purge?url=<enc>&async=false`.
    pub async fn purge_url(&self, url: &str) -> Result<(), ProviderError> {
        todo!("T5")
    }

    /// `POST /pullzone/{id}/purgeCache`.
    pub async fn purge_pull_zone(&self, id: u64) -> Result<(), ProviderError> {
        todo!("T5")
    }
}

/// Purges a site's pull zones. Rate-limited: URLs 120 burst + 5/s, zones 20 burst + 0.5/s. A 429
/// waits Retry-After (clamped), at most 3 tries.
pub struct BunnyCdn {
    #[allow(dead_code)] // T1 skeleton: T5 reads it.
    account: BunnyAccount,
    zones: Vec<CdnZone>,
}

impl BunnyCdn {
    pub fn new(account: BunnyAccount, zones: Vec<CdnZone>) -> Self {
        Self { account, zones }
    }
}

#[async_trait]
impl CdnPurger for BunnyCdn {
    fn zones(&self) -> &[CdnZone] {
        &self.zones
    }

    async fn purge_url(&self, url: &str) -> Result<(), ProviderError> {
        todo!("T5")
    }

    async fn purge_zone(&self, id: u64) -> Result<(), ProviderError> {
        todo!("T5")
    }
}
