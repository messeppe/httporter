//! Publish profiles, their history and the CDN links, kept in the site database; and the Bunny
//! account keys that purge. Async wrappers that run the store on blocking threads (the pattern of
//! `store.rs`), with errors as text.
//!
//! A read that fails answers as if nothing were stored, like the settings: a broken history must
//! not keep the profile page from showing. The account key goes only to the credential store,
//! under [`CredentialKey::account`]; the database holds the account's record, never the key.

// The publish views (T7, T8) call these; drop this once they are merged.
#![allow(dead_code)]

use std::sync::Mutex;

use chrono::Utc;
use httporter_core::{
    AccountId, CdnLink, HistoryFile, HistoryRecord, ProfileId, ProfileRecord, ProviderKind,
    PurgeResult, SiteId,
};
use httporter_platform::{AccountRecord, CredentialKey, CredentialStore, Store, StoreError};

use super::{Session, blocking, lock};
use crate::copy;

impl Session {
    /// The publish profiles, ordered by name.
    pub async fn profiles(&self) -> Vec<ProfileRecord> {
        let store = self.store.clone();
        blocking(move || lock(&store).list_profiles().unwrap_or_default()).await
    }

    /// Adds the profile, or updates it.
    pub async fn save_profile(&self, p: ProfileRecord) -> Result<(), String> {
        let store = self.store.clone();
        blocking(move || {
            let store = lock(&store);
            match store.update_profile(&p) {
                Err(StoreError::ProfileNotFound) => store.add_profile(&p),
                other => other,
            }
            .map_err(|e| e.to_string())
        })
        .await
    }

    /// Removes the profile and its history.
    pub async fn remove_profile(&self, id: ProfileId) -> Result<(), String> {
        let store = self.store.clone();
        blocking(move || lock(&store).remove_profile(&id).map_err(|e| e.to_string())).await
    }

    /// Stores a publish; returns its id.
    pub async fn add_history(&self, h: HistoryRecord) -> Result<i64, String> {
        let store = self.store.clone();
        blocking(move || lock(&store).add_history(&h).map_err(|e| e.to_string())).await
    }

    /// Replaces what a stored publish's purge did (Retry Purge).
    pub async fn set_history_purge(&self, id: i64, purge: PurgeResult) -> Result<(), String> {
        let store = self.store.clone();
        blocking(move || {
            lock(&store)
                .update_history_purge(id, &purge)
                .map_err(|e| e.to_string())
        })
        .await
    }

    /// The profile's publishes, newest first, without their files; `search` filters them.
    pub async fn history(&self, profile: ProfileId, search: String) -> Vec<HistoryRecord> {
        let store = self.store.clone();
        blocking(move || {
            lock(&store)
                .list_history(&profile, &search)
                .unwrap_or_default()
        })
        .await
    }

    /// The files of a stored publish.
    pub async fn history_files(&self, id: i64) -> Vec<HistoryFile> {
        let store = self.store.clone();
        blocking(move || lock(&store).history_files(id).unwrap_or_default()).await
    }

    /// The site's CDN link, if one is stored.
    pub async fn cdn_link(&self, site: SiteId) -> Option<CdnLink> {
        let store = self.store.clone();
        blocking(move || lock(&store).cdn_link(&site).ok().flatten()).await
    }

    /// Stores the site's CDN link. With `key`: `creds.set(CredentialKey::account)` for
    /// `link.account` first, and a Bunny [`AccountRecord`] if it has none yet; a failure later on
    /// takes back what this call added.
    pub async fn save_cdn_link(
        &self,
        site: SiteId,
        link: CdnLink,
        key: Option<String>,
    ) -> Result<(), String> {
        let (creds, store) = (self.creds.clone(), self.store.clone());
        blocking(move || match key {
            None => lock(&store)
                .put_cdn_link(&site, &link)
                .map_err(|e| e.to_string()),
            Some(key) => save_link_with_key(&*creds, &store, &site, &link, key.trim()),
        })
        .await
    }

    /// The account's key, from the credential store. `None` when there is none or it cannot be
    /// read.
    pub async fn account_key(&self, account: AccountId) -> Option<String> {
        let creds = self.creds.clone();
        blocking(move || creds.get(&CredentialKey::account(&account)).ok().flatten()).await
    }

    /// The Bunny accounts whose keys are kept, ordered by name.
    pub async fn bunny_accounts(&self) -> Vec<AccountRecord> {
        let store = self.store.clone();
        blocking(move || {
            let accounts = lock(&store).list_accounts().unwrap_or_default();
            accounts
                .into_iter()
                .filter(|account| account.provider == ProviderKind::Bunny)
                .collect()
        })
        .await
    }

    /// Forgets the key, the account record and its CDN links.
    pub async fn forget_cdn_key(&self, account: AccountId) -> Result<(), String> {
        let (creds, store) = (self.creds.clone(), self.store.clone());
        blocking(move || {
            creds
                .delete(&CredentialKey::account(&account))
                .map_err(|e| e.to_string())?;
            let store = lock(&store);
            store.remove_account(&account).map_err(|e| e.to_string())?;
            store
                .remove_cdn_links_for(&account)
                .map_err(|e| e.to_string())
        })
        .await
    }
}

/// [`Session::save_cdn_link`] with a key: the key, then the account record if it is new, then the
/// link. If a later step fails, the earlier ones this call made are taken back.
fn save_link_with_key(
    creds: &dyn CredentialStore,
    store: &Mutex<Store>,
    site: &SiteId,
    link: &CdnLink,
    key: &str,
) -> Result<(), String> {
    if key.is_empty() {
        return Err(copy::ACCOUNT_KEY_MISSING.to_owned());
    }
    let cred = CredentialKey::account(&link.account);
    let before = creds.get(&cred).map_err(|e| e.to_string())?;
    let restore_key = |creds: &dyn CredentialStore| {
        let _ = match &before {
            Some(before) => creds.set(&cred, before),
            None => creds.delete(&cred),
        };
    };
    creds.set(&cred, key).map_err(|e| e.to_string())?;

    let store = lock(store);
    let known = match store.list_accounts() {
        Ok(accounts) => accounts.iter().any(|account| account.id == link.account),
        Err(e) => {
            restore_key(creds);
            return Err(e.to_string());
        }
    };
    if !known {
        let account = AccountRecord {
            id: link.account.clone(),
            provider: ProviderKind::Bunny,
            name: copy::BUNNY_ACCOUNT_NAME.to_owned(),
            created_at: Utc::now(),
        };
        if let Err(e) = store.add_account(&account) {
            restore_key(creds);
            return Err(e.to_string());
        }
    }
    if let Err(e) = store.put_cdn_link(site, link) {
        if !known {
            let _ = store.remove_account(&link.account);
        }
        restore_key(creds);
        return Err(e.to_string());
    }
    Ok(())
}
