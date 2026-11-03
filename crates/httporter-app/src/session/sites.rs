//! The saved sites: the list the sidebar shows, how a site is added or changed (tested, then
//! saved), duplicated and removed, the Bunny account key that adds several zones at once, the
//! S3 bucket list, and how a saved site's secret turns into a storage client.
//!
//! Reading and writing the credential store, the site database and making the HTTP client can all
//! take a while (Windows Credential Manager, the disk, TLS set-up), so each runs on tokio's
//! blocking threads and never on a runtime worker. [`Session::duplicate_site`] and
//! [`Session::remove_site`] block, and the window calls them from a blocking thread.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use httporter_bunny::{REGIONS, ZoneInfo, ZoneList, storage_host};
use httporter_core::{
    AccountId, ProviderKind, RemotePath, S3Preset, S3Site, SiteId, SiteRecord, StorageProvider,
};
use httporter_platform::{
    AccountRecord, CredentialKey, CredentialStore, SiteList, SiteSecret, Store, StoreError,
    site_conflicts_key,
};
use httporter_s3::{Credentials, S3Config};

use super::{Session, blocking, lock};
#[cfg(test)]
use crate::add_site::ValidSite;
use crate::add_site::{SiteForm, s3_host};
use crate::copy::{self, Attempt};
use crate::site_hub::{HubError, list_interactive, read_secret};

/// The outcome of the Add Site sheet's Test Connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub ok: bool,
    pub text: String,
}

/// The service a site is on, down to the S3 preset. A site's tile takes its colour and letters
/// from this, never from the site's name (ledger 129).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteProvider {
    Bunny,
    S3(S3Preset),
}

impl SiteProvider {
    pub fn of(site: &SiteRecord) -> Self {
        match site.provider {
            ProviderKind::Bunny => Self::Bunny,
            // An S3 site always carries its details; `Other` stands in should one not.
            ProviderKind::S3 => Self::S3(site.s3.as_ref().map_or(S3Preset::Other, |s3| s3.preset)),
        }
    }

    /// The letters on the tile: "B" for Bunny, the preset's for S3 ("R2", "W", …).
    pub fn mono(self) -> &'static str {
        match self {
            Self::Bunny => "B",
            Self::S3(preset) => preset.mono(),
        }
    }

    /// The service's name: "Bunny Storage", "Cloudflare R2", …
    pub fn label(self) -> &'static str {
        match self {
            Self::Bunny => copy::BUNNY_STORAGE,
            Self::S3(preset) => preset.label(),
        }
    }
}

/// One row of the sidebar's site list. The tile's letters are `provider.mono()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteItemData {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    /// Picks the tile's colour and letters.
    pub provider: SiteProvider,
    /// The last request to the site succeeded (a site no request has gone to yet counts as
    /// healthy).
    pub healthy: bool,
}

/// The endpoint the Add Site sheet shows for a zone and a region index into `REGIONS`:
/// `https://{host}/{zone}/`, or empty when no region is chosen.
pub fn endpoint_preview(zone: &str, region_index: i32) -> String {
    let host = usize::try_from(region_index)
        .ok()
        .and_then(|index| REGIONS.get(index))
        .and_then(|(code, _)| storage_host(code));
    match (host, zone.trim()) {
        (None, _) => String::new(),
        (Some(host), "") => format!("https://{host}/"),
        (Some(host), zone) => format!("https://{host}/{zone}/"),
    }
}

/// The city a region code stands for ("Singapore"); the code itself when it is not known.
fn region_city(code: &str) -> &str {
    REGIONS
        .iter()
        .find(|(known, _)| *known == code)
        .map_or(code, |(_, city)| city)
}

/// The saved sites, as the sidebar shows them.
pub(super) struct Book {
    /// As the database lists them: by name.
    pub(super) sites: Vec<SiteRecord>,
    /// Whether the last request to each site worked. Sites not in here have not been asked yet.
    pub(super) healthy: HashMap<SiteId, bool>,
    /// A problem with the app itself (the site database), shown until the app is restarted.
    pub(super) notice: Option<String>,
    /// Rows of the site database that could not be read (Ruling Q23), as of the last listing.
    pub(super) unreadable: usize,
}

impl Book {
    /// The sites in `store`; if they cannot be read, none, and a notice that says so.
    pub(super) fn read(store: &Store) -> Self {
        let mut book = Self {
            sites: Vec::new(),
            healthy: HashMap::new(),
            notice: None,
            unreadable: 0,
        };
        book.take(store.list_sites());
        book
    }

    /// Takes in the database's list of sites, or notes that it could not be read.
    fn take(&mut self, listed: Result<SiteList, StoreError>) {
        match listed {
            Ok(list) => {
                self.sites = list.sites;
                self.unreadable = list.unreadable;
                let known: Vec<SiteId> = self.sites.iter().map(|site| site.id.clone()).collect();
                self.healthy.retain(|id, _| known.contains(id));
            }
            Err(e) => self.notice = Some(copy::sites_unreadable(e)),
        }
    }
}

/// A site as a form describes it, ready to be tested and saved: its record (a new id, or the id,
/// creation time of the site it changes) and the secret it signs in with.
pub struct Prepared {
    pub record: SiteRecord,
    secret: SiteSecret,
    /// It changes a saved site rather than adding one.
    editing: bool,
}

// `SiteSecret` hides its fields too; this says so once more for whoever reads a log.
impl fmt::Debug for Prepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Prepared")
            .field("record", &self.record)
            .field("secret", &"<hidden>")
            .field("editing", &self.editing)
            .finish()
    }
}

impl Session {
    /// Shows `text` in the banner for the rest of the run (used when the site database could not
    /// be opened).
    pub fn set_notice(&self, text: String) {
        lock(&self.book).notice = Some(text);
    }

    /// A problem with the app itself, for the banner: the site database that could not be opened
    /// or read, and saved sites that could not be read (one line each).
    pub fn notice(&self) -> Option<String> {
        let book = lock(&self.book);
        let skipped = (book.unreadable > 0).then(|| copy::sites_skipped(book.unreadable));
        match (book.notice.clone(), skipped) {
            (Some(notice), Some(skipped)) => Some(format!("{notice}\n{skipped}")),
            (notice, skipped) => notice.or(skipped),
        }
    }

    /// The saved sites, by name.
    pub fn sites(&self) -> Vec<SiteRecord> {
        lock(&self.book).sites.clone()
    }

    /// The sidebar's rows, in the order of [`Self::sites`].
    pub fn site_items(&self) -> Vec<SiteItemData> {
        let book = lock(&self.book);
        book.sites
            .iter()
            .map(|site| {
                let provider = SiteProvider::of(site);
                let subtitle = match provider {
                    SiteProvider::Bunny => {
                        copy::site_subtitle(copy::BUNNY, region_city(&site.region))
                    }
                    SiteProvider::S3(preset) => copy::s3_subtitle(preset.label(), &site.region),
                };
                SiteItemData {
                    id: site.id.0.clone(),
                    name: site.name.clone(),
                    subtitle,
                    provider,
                    healthy: book.healthy.get(&site.id).copied().unwrap_or(true),
                }
            })
            .collect()
    }

    /// Lists the root of the zone the sheet describes, with the password as typed, on a client
    /// made as the site's would be (the Transfers settings, a short listing timeout) and the
    /// short retries of [`list_interactive`]. It uses nothing of the session but the factory and
    /// the settings, so it waits for no listing.
    #[cfg(test)]
    pub async fn test_connection(&self, v: &ValidSite) -> TestResult {
        match self.prepare(form_of(v), None, v.name.clone()).await {
            Ok(prepared) => self.test_prepared(&prepared).await,
            Err(text) => TestResult { ok: false, text },
        }
    }

    /// Adds the site the sheet describes if it works: lists the zone's root first, exactly as
    /// [`Self::test_connection`] does, and saves the site only when that succeeds. Otherwise
    /// nothing is saved and the error is the test's ("Couldn't sign in to …").
    #[cfg(test)]
    pub async fn add_site(&self, v: &ValidSite) -> Result<SiteRecord, String> {
        let tested = self.test_connection(v).await;
        if !tested.ok {
            return Err(tested.text);
        }
        self.save_site(v).await
    }

    /// Saves a new Bunny site as it is, without testing it (the I1 tests' way in; the window
    /// goes through [`Self::prepare`] and [`Self::store_prepared`]).
    #[cfg(test)]
    pub async fn save_site(&self, v: &ValidSite) -> Result<SiteRecord, String> {
        let prepared = self.prepare(form_of(v), None, v.name.clone()).await?;
        self.store_prepared(prepared).await
    }

    /// The site `form` describes, named `name` (blank: the zone or bucket). With `editing`, it
    /// changes that saved site: same id and creation time, and a secret left empty in the form
    /// means the saved one. Nothing is written; the credential store is read only for a kept
    /// secret, on a blocking thread.
    pub async fn prepare(
        &self,
        form: SiteForm,
        editing: Option<SiteId>,
        name: String,
    ) -> Result<Prepared, String> {
        let old = match &editing {
            Some(id) => Some(self.site(id).ok_or_else(|| copy::SITE_GONE.to_owned())?),
            None => None,
        };
        let (id, created_at) = match &old {
            Some(old) => (old.id.clone(), old.created_at),
            None => (SiteId::new(), Utc::now()),
        };
        let record = record_of(&form, id, &name, created_at)?;
        let creds = self.creds.clone();
        let secret = blocking(move || secret_of(&form, old.as_ref(), &*creds)).await?;
        Ok(Prepared {
            record,
            secret,
            editing: editing.is_some(),
        })
    }

    /// Lists the root of the zone or bucket `prepared` describes, signed in with its secret, on
    /// a client made as the site's would be (the Transfers settings) and the short retries of
    /// [`list_interactive`]. Nothing of the session is used but the factory and the settings, so
    /// it waits for no listing.
    pub async fn test_prepared(&self, prepared: &Prepared) -> TestResult {
        let (hub, record, secret) = (
            self.hub.clone(),
            prepared.record.clone(),
            prepared.secret.clone(),
        );
        let site = &prepared.record;
        let provider = match blocking(move || hub.client_for(&record, secret)).await {
            Ok(provider) => provider,
            Err(error) => {
                return TestResult {
                    ok: false,
                    text: hub_error_text(&error, site),
                };
            }
        };
        let root = RemotePath::root();
        match list_interactive(&*provider, &root).await {
            Ok(listing) => TestResult {
                ok: true,
                text: copy::connected(
                    &listing.info.http_version,
                    listing.info.latency,
                    listing.entries.len(),
                ),
            },
            Err(error) => TestResult {
                ok: false,
                text: copy::describe_for(
                    site.provider,
                    &error,
                    &site.container,
                    &site.host,
                    Attempt::List(&root),
                ),
            },
        }
    }

    /// Tests `form` as [`Self::test_prepared`] does, and saves it only when that works (Add Site
    /// and Save on Edit Site). The window does the same in two tasks, so that Cancel can give up
    /// the test without reaching a save that has begun (`Ctx::save_form` in `binding.rs`).
    #[allow(dead_code)] // The brief's way in for tests; the window splits it in two.
    pub async fn save_form(
        &self,
        form: SiteForm,
        editing: Option<SiteId>,
        name: String,
    ) -> Result<SiteRecord, String> {
        let prepared = self.prepare(form, editing, name).await?;
        let tested = self.test_prepared(&prepared).await;
        if !tested.ok {
            return Err(tested.text);
        }
        self.store_prepared(prepared).await
    }

    /// Saves a prepared site without testing it: its secret in the credential store, everything
    /// else in the database, and the site in the list. A changed site's client is dropped, so the
    /// next request builds one with what was saved.
    ///
    /// Once this has begun to write, the site is saved and listed, whatever becomes of the caller:
    /// the writes and the update of the list are one blocking task, and dropping the future that
    /// waits for it (Cancel on the sheet) does not stop it. Nothing is left half done: either the
    /// secret and the site are both stored and the site is listed, or neither is changed.
    pub async fn store_prepared(&self, prepared: Prepared) -> Result<SiteRecord, String> {
        let (creds, store, book, hub) = (
            self.creds.clone(),
            self.store.clone(),
            self.book.clone(),
            self.hub.clone(),
        );
        blocking(move || {
            let Prepared {
                record,
                secret,
                editing,
            } = prepared;
            let stored = secret.to_stored();
            let saved = if editing {
                update_in_stores(&*creds, &store, &book, record, &stored)
            } else {
                save_to_stores(&*creds, &store, &book, record, Some(&stored))
            };
            if let Ok(record) = &saved {
                hub.invalidate(&record.id);
            }
            saved
        })
        .await
    }

    /// Saves a copy of site `id` under a new id, with its secret, named "{name} copy". Blocks
    /// (the credential store, the database): call it from a blocking thread.
    pub fn duplicate_site(&self, id: &SiteId) -> Result<SiteRecord, String> {
        let site = self.site(id).ok_or_else(|| copy::SITE_GONE.to_owned())?;
        let stored = self
            .creds
            .get(&CredentialKey::site(id))
            .map_err(copy::site_not_duplicated)?;
        let record = SiteRecord {
            id: SiteId::new(),
            name: copy::duplicate_name(&site.name),
            created_at: Utc::now(),
            ..site
        };
        // A site with no secret saved is copied as it is: the copy asks for one the same way.
        save_to_stores(
            &*self.creds,
            &self.store,
            &self.book,
            record,
            stored.as_deref(),
        )
    }

    /// Forgets site `id` (Ruling Q23): its secret in the credential store, its row in the
    /// database, the conflict choices remembered for it, and its client. Files on the storage are
    /// never touched. The secret goes first, so a failure never leaves a secret behind for a site
    /// that is gone. Blocks: call it from a blocking thread. (Its transfers are the engine's to
    /// cancel, which the window does first.)
    pub fn remove_site(&self, id: &SiteId) -> Result<(), String> {
        self.creds
            .delete(&CredentialKey::site(id))
            .map_err(copy::site_not_removed)?;
        let listed = {
            let store = lock(&self.store);
            store.remove_site(id).map_err(copy::site_not_removed)?;
            for upload in [true, false] {
                // A remembered choice left behind is never read again: no reason to fail over it.
                let _ = store.remove_setting(&site_conflicts_key(id, upload));
            }
            store.list_sites()
        };
        self.hub.invalidate(id);
        let mut book = lock(&self.book);
        book.take(listed);
        book.sites.retain(|site| site.id != *id);
        Ok(())
    }

    /// The zones of the Bunny account whose API key is `key` (Find Zones), reached through the
    /// user's proxy setting (Ruling Q41): the ones that can be added and the ones that can't.
    pub async fn find_zones(&self, key: String) -> Result<ZoneList, String> {
        let (factory, settings) = (self.hub.factory(), self.hub.settings());
        // Making the client sets up TLS, which takes a while.
        let answer = blocking(move || factory.bunny_zones(key, &settings)).await;
        answer.await.map_err(|error| copy::account_failed(&error))
    }

    /// Adds every zone of `zones` as a site, with its password, as Find Zones found them (Ruling
    /// Q42). With `keep_key`, the account key is kept too: in the credential store under
    /// `HTTPorter/account/<id>` and in the accounts table; otherwise it is forgotten. A failure
    /// stops there: the sites already added stay, and the error says what failed.
    pub async fn add_zones(
        &self,
        zones: Vec<ZoneInfo>,
        keep_key: Option<String>,
    ) -> Result<Vec<SiteRecord>, String> {
        let (creds, store, book) = (self.creds.clone(), self.store.clone(), self.book.clone());
        blocking(move || {
            let mut added = Vec::new();
            for zone in zones {
                let record = SiteRecord {
                    id: SiteId::new(),
                    name: zone.name.clone(),
                    provider: ProviderKind::Bunny,
                    container: zone.name.clone(),
                    region: zone.region.clone(),
                    host: zone.host.clone(),
                    s3: None,
                    created_at: Utc::now(),
                };
                added.push(save_to_stores(
                    &*creds,
                    &store,
                    &book,
                    record,
                    Some(&zone.password),
                )?);
            }
            if let Some(key) = keep_key {
                keep_account_key(&*creds, &store, &key).map_err(copy::account_key_not_saved)?;
            }
            Ok(added)
        })
        .await
    }

    /// The buckets the S3 key of `form` can see (Choose…). With `editing` and the secret left
    /// empty, the saved secret of that site is used.
    pub async fn list_buckets(
        &self,
        form: SiteForm,
        editing: Option<SiteId>,
    ) -> Result<Vec<String>, String> {
        let SiteForm::S3 {
            preset,
            region,
            account_id,
            endpoint,
            path_style,
            ..
        } = &form
        else {
            return Ok(Vec::new());
        };
        let found = httporter_s3::endpoint(*preset, region, account_id, endpoint, *path_style)?;
        let host = s3_host(&found.base);
        let old = match &editing {
            Some(id) => self.site(id),
            None => None,
        };
        let creds = self.creds.clone();
        let secret = blocking(move || secret_of(&form, old.as_ref(), &*creds)).await?;
        let SiteSecret::S3 {
            access_key_id,
            secret_access_key,
        } = secret
        else {
            return Ok(Vec::new());
        };
        let creds = Credentials {
            access_key_id,
            secret_access_key,
            session_token: None,
        };
        let cfg = S3Config::for_site("", found, creds, &self.hub.settings());
        let factory = self.hub.factory();
        let answer = blocking(move || factory.s3_buckets(cfg)).await;
        let mut names = answer
            .await
            .map_err(|error| copy::buckets_failed(&error, &host))?;
        names.sort_by_key(|name| name.to_lowercase());
        Ok(names)
    }

    /// The access key ID saved for S3 site `site`, to show when it is edited (it is not the
    /// secret). Empty when there is none or it can't be read. Blocks: call it from a blocking
    /// thread.
    pub fn saved_key_id(&self, site: &SiteRecord) -> String {
        match read_secret(&*self.creds, site) {
            Ok(SiteSecret::S3 { access_key_id, .. }) => access_key_id,
            _ => String::new(),
        }
    }

    /// The storage client of `site` (the hub's, built on first use with its password from the
    /// credential store), or why there is none, in words.
    pub async fn connect(&self, site: &SiteRecord) -> Result<Arc<dyn StorageProvider>, String> {
        let (hub, record) = (self.hub.clone(), site.clone());
        blocking(move || hub.provider(&record))
            .await
            .map_err(|e| hub_error_text(&e, site))
    }

    /// The saved site with id `id`.
    pub fn site(&self, id: &SiteId) -> Option<SiteRecord> {
        lock(&self.book)
            .sites
            .iter()
            .find(|site| site.id == *id)
            .cloned()
    }
}

/// The form the I1 tests' `ValidSite` stands for.
#[cfg(test)]
fn form_of(v: &ValidSite) -> SiteForm {
    let region_index = REGIONS
        .iter()
        .position(|(code, _)| *code == v.region_code)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    SiteForm::BunnyPassword {
        zone: v.zone.clone(),
        region_index,
        password: v.password.clone(),
    }
}

/// The record a form describes, with `id`, `created_at` and `name` (a blank name is the zone or
/// the bucket). The error is the sentence for the sheet's result box.
fn record_of(
    form: &SiteForm,
    id: SiteId,
    name: &str,
    created_at: DateTime<Utc>,
) -> Result<SiteRecord, String> {
    let named = |container: &str| {
        let name = name.trim();
        if name.is_empty() { container } else { name }.to_owned()
    };
    match form {
        SiteForm::BunnyPassword {
            zone, region_index, ..
        } => {
            let code = usize::try_from(*region_index)
                .ok()
                .and_then(|index| REGIONS.get(index))
                .map(|(code, _)| *code)
                .ok_or_else(|| copy::CHOOSE_REGION.to_owned())?;
            let host = storage_host(code).ok_or_else(|| copy::CHOOSE_REGION.to_owned())?;
            let zone = zone.trim();
            Ok(SiteRecord {
                id,
                name: named(zone),
                provider: ProviderKind::Bunny,
                container: zone.to_owned(),
                region: code.to_owned(),
                host,
                s3: None,
                created_at,
            })
        }
        SiteForm::S3 {
            preset,
            region,
            account_id,
            endpoint,
            path_style,
            bucket,
            ..
        } => {
            let found = httporter_s3::endpoint(*preset, region, account_id, endpoint, *path_style)?;
            let bucket = bucket.trim();
            Ok(SiteRecord {
                id,
                name: named(bucket),
                provider: ProviderKind::S3,
                container: bucket.to_owned(),
                region: found.region.clone(),
                host: s3_host(&found.base),
                s3: Some(S3Site {
                    preset: *preset,
                    endpoint: found.base.as_str().trim_end_matches('/').to_owned(),
                    path_style: found.path_style,
                }),
                created_at,
            })
        }
        SiteForm::BunnyAccount { .. } => Err(copy::ACCOUNT_KEY_MISSING.to_owned()),
    }
}

/// The secret a form signs in with. A secret left empty is `old`'s saved one (Edit Site keeps
/// it); for S3 the access key ID is the form's even then. Reads the credential store only for
/// that, so it may block.
fn secret_of(
    form: &SiteForm,
    old: Option<&SiteRecord>,
    creds: &dyn CredentialStore,
) -> Result<SiteSecret, String> {
    let saved = || -> Result<SiteSecret, String> {
        let old = old.ok_or_else(|| copy::SECRET_MISSING.to_owned())?;
        read_secret(creds, old).map_err(|e| hub_error_text(&e, old))
    };
    match form {
        SiteForm::BunnyPassword { password, .. } if password.is_empty() => match saved()? {
            secret @ SiteSecret::BunnyPassword(_) => Ok(secret),
            SiteSecret::S3 { .. } => Err(copy::PASSWORD_MISSING.to_owned()),
        },
        SiteForm::BunnyPassword { password, .. } => Ok(SiteSecret::BunnyPassword(password.clone())),
        SiteForm::S3 { key_id, secret, .. } => {
            let secret_access_key = if secret.is_empty() {
                match saved()? {
                    SiteSecret::S3 {
                        secret_access_key, ..
                    } => secret_access_key,
                    SiteSecret::BunnyPassword(_) => return Err(copy::SECRET_MISSING.to_owned()),
                }
            } else {
                secret.clone()
            };
            Ok(SiteSecret::S3 {
                access_key_id: key_id.trim().to_owned(),
                secret_access_key,
            })
        }
        SiteForm::BunnyAccount { .. } => Err(copy::ACCOUNT_KEY_MISSING.to_owned()),
    }
}

/// Puts a new site's secret in the credential store (when it has one) and the site in the
/// database, then reads the list of sites back and lists the new site. Whichever write fails, the
/// other leaves nothing behind: no secret stays for a site that was not saved. It is one blocking
/// task from the first write to the last change of the list, and takes its inputs by value, so
/// that nobody waiting for it can leave it half done.
fn save_to_stores(
    creds: &dyn CredentialStore,
    store: &Mutex<Store>,
    book: &Mutex<Book>,
    record: SiteRecord,
    secret: Option<&str>,
) -> Result<SiteRecord, String> {
    let key = CredentialKey::site(&record.id);
    if let Some(secret) = secret {
        creds.set(&key, secret).map_err(copy::password_not_saved)?;
    }
    let listed = {
        let store = lock(store);
        if let Err(e) = store.add_site(&record) {
            drop(store);
            let _ = creds.delete(&key);
            return Err(copy::site_not_saved(e));
        }
        store.list_sites()
    };
    let mut book = lock(book);
    book.take(listed);
    if !book.sites.iter().any(|site| site.id == record.id) {
        book.sites.push(record.clone());
    }
    Ok(record)
}

/// Saves the changes to a site: its new secret, then its record. If the record can't be saved,
/// the secret it had is put back, so a failed save changes nothing.
fn update_in_stores(
    creds: &dyn CredentialStore,
    store: &Mutex<Store>,
    book: &Mutex<Book>,
    record: SiteRecord,
    secret: &str,
) -> Result<SiteRecord, String> {
    let key = CredentialKey::site(&record.id);
    let before = creds.get(&key).map_err(copy::password_not_saved)?;
    creds.set(&key, secret).map_err(copy::password_not_saved)?;
    let listed = {
        let store = lock(store);
        if let Err(e) = store.update_site(&record) {
            drop(store);
            let _ = match before {
                Some(before) => creds.set(&key, &before),
                None => creds.delete(&key),
            };
            return Err(copy::site_not_updated(e));
        }
        store.list_sites()
    };
    let mut book = lock(book);
    book.take(listed);
    if let Some(site) = book.sites.iter_mut().find(|site| site.id == record.id) {
        site.clone_from(&record);
    }
    // The old record's health says nothing about the changed one.
    book.healthy.remove(&record.id);
    Ok(record)
}

/// Keeps a Bunny account key: the key in the credential store, then the account in the
/// database (the key is taken back out if that fails).
fn keep_account_key(
    creds: &dyn CredentialStore,
    store: &Mutex<Store>,
    key: &str,
) -> Result<(), String> {
    let account = AccountRecord {
        id: AccountId::new(),
        provider: ProviderKind::Bunny,
        name: copy::BUNNY_ACCOUNT_NAME.to_owned(),
        created_at: Utc::now(),
    };
    let cred = CredentialKey::account(&account.id);
    creds.set(&cred, key.trim()).map_err(|e| e.to_string())?;
    if let Err(e) = lock(store).add_account(&account) {
        let _ = creds.delete(&cred);
        return Err(e.to_string());
    }
    Ok(())
}

/// Why `site` has no client, in words.
pub fn hub_error_text(error: &HubError, site: &SiteRecord) -> String {
    match error {
        HubError::MissingSecret => copy::no_password_saved(&site.name),
        HubError::Credential(reason) => copy::password_unreadable(&site.name, reason),
        HubError::Provider(e) => copy::describe_for(
            site.provider,
            e,
            &site.container,
            &site.host,
            Attempt::Setup,
        ),
    }
}
