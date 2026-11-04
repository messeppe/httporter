//! Publish profiles, their history and the CDN links, kept in the site database; and the Bunny
//! account keys that purge. Async wrappers that run the store on blocking threads (the pattern of
//! `store.rs`), with errors as text.

// T1 skeleton: drop this once T4 fills the bodies.
#![allow(dead_code, unused_variables)]

use httporter_core::{
    AccountId, CdnLink, HistoryFile, HistoryRecord, ProfileId, ProfileRecord, PurgeResult, SiteId,
};
use httporter_platform::AccountRecord;

use super::Session;

impl Session {
    pub async fn profiles(&self) -> Vec<ProfileRecord> {
        todo!("T4")
    }

    /// Adds the profile, or updates it.
    pub async fn save_profile(&self, p: ProfileRecord) -> Result<(), String> {
        todo!("T4")
    }

    pub async fn remove_profile(&self, id: ProfileId) -> Result<(), String> {
        todo!("T4")
    }

    pub async fn add_history(&self, h: HistoryRecord) -> Result<i64, String> {
        todo!("T4")
    }

    pub async fn set_history_purge(&self, id: i64, purge: PurgeResult) -> Result<(), String> {
        todo!("T4")
    }

    pub async fn history(&self, profile: ProfileId, search: String) -> Vec<HistoryRecord> {
        todo!("T4")
    }

    pub async fn history_files(&self, id: i64) -> Vec<HistoryFile> {
        todo!("T4")
    }

    pub async fn cdn_link(&self, site: SiteId) -> Option<CdnLink> {
        todo!("T4")
    }

    /// With `key`: `creds.set(CredentialKey::account)` and a Bunny [`AccountRecord`] too.
    pub async fn save_cdn_link(
        &self,
        site: SiteId,
        link: CdnLink,
        key: Option<String>,
    ) -> Result<(), String> {
        todo!("T4")
    }

    pub async fn account_key(&self, account: AccountId) -> Option<String> {
        todo!("T4")
    }

    pub async fn bunny_accounts(&self) -> Vec<AccountRecord> {
        todo!("T4")
    }

    /// Forgets the key, the account record and its CDN links.
    pub async fn forget_cdn_key(&self, account: AccountId) -> Result<(), String> {
        todo!("T4")
    }
}
