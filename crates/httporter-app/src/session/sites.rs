//! The saved sites: the list the sidebar shows, how a site is added (tested, then saved), and how
//! a saved site's password turns into a storage client.
//!
//! Reading and writing the credential store, the site database and making the HTTP client can all
//! take a while (Windows Credential Manager, the disk, TLS set-up), so each runs on tokio's
//! blocking threads and never on a runtime worker.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use httporter_bunny::{BunnyConfig, REGIONS, storage_host};
use httporter_core::{
    ProviderError, ProviderKind, RemotePath, S3Preset, SiteId, SiteRecord, StorageProvider,
};
use httporter_platform::{CredentialKey, CredentialStore, Store, StoreError};

use super::{ProviderFactory, Session, blocking, interactive, lock};
use crate::add_site::ValidSite;
use crate::copy::{self, Attempt};

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
}

impl Book {
    /// The sites in `store`; if they cannot be read, none, and a notice that says so.
    pub(super) fn read(store: &Store) -> Self {
        let mut book = Self {
            sites: Vec::new(),
            healthy: HashMap::new(),
            notice: None,
        };
        book.take(store.list_sites());
        book
    }

    /// Takes in the database's list of sites, or notes that it could not be read.
    fn take(&mut self, listed: Result<Vec<SiteRecord>, StoreError>) {
        match listed {
            Ok(sites) => self.sites = sites,
            Err(e) => self.notice = Some(copy::sites_unreadable(e)),
        }
    }
}

impl Session {
    /// Shows `text` in the banner for the rest of the run (used when the site database could not
    /// be opened).
    pub fn set_notice(&self, text: String) {
        lock(&self.book).notice = Some(text);
    }

    /// A problem with the app itself, for the banner.
    pub fn notice(&self) -> Option<String> {
        lock(&self.book).notice.clone()
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
                    SiteProvider::S3(preset) => copy::site_subtitle(preset.label(), &site.region),
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

    /// Lists the root of the zone the sheet describes, with the password as typed (and the
    /// short policy of [`interactive`]). It uses nothing of the session but the factory, so it
    /// waits for no listing.
    pub async fn test_connection(&self, v: &ValidSite) -> TestResult {
        let Some(host) = storage_host(&v.region_code) else {
            return TestResult {
                ok: false,
                text: copy::CHOOSE_REGION.into(),
            };
        };
        let cfg = interactive(BunnyConfig::new(&v.zone, &host, &v.password));
        let provider = match build_client(self.factory.clone(), cfg).await {
            Ok(provider) => provider,
            Err(error) => {
                return TestResult {
                    ok: false,
                    text: copy::describe(&error, &v.zone, &host, Attempt::Setup),
                };
            }
        };
        let root = RemotePath::root();
        match provider.list(&root).await {
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
                text: copy::describe(&error, &v.zone, &host, Attempt::List(&root)),
            },
        }
    }

    /// Adds the site the sheet describes if it works: lists the zone's root first, exactly as
    /// [`Self::test_connection`] does, and saves the site only when that succeeds. Otherwise
    /// nothing is saved and the error is the test's ("Couldn't sign in to …").
    ///
    /// The window does the same in two tasks, so that Cancel can give up the test without
    /// reaching a save that has begun (`Ctx::save_site` in `binding.rs`); this does both in one
    /// call, for the tests of the session.
    #[cfg(test)]
    pub async fn add_site(&self, v: &ValidSite) -> Result<SiteRecord, String> {
        let tested = self.test_connection(v).await;
        if !tested.ok {
            return Err(tested.text);
        }
        self.save_site(v).await
    }

    /// Saves a new site as it is, without testing it: its password in the credential store,
    /// everything else in the database, and the site in the list. The sheet tests the zone first.
    ///
    /// Once this has begun to write, the site is saved and listed, whatever becomes of the caller:
    /// the writes and the update of the list are one blocking task, and dropping the future that
    /// waits for it (Cancel on the sheet) does not stop it. Nothing is left half done: either the
    /// password and the site are both stored and the site is listed, or neither is stored.
    pub async fn save_site(&self, v: &ValidSite) -> Result<SiteRecord, String> {
        let host = storage_host(&v.region_code).ok_or_else(|| copy::CHOOSE_REGION.to_owned())?;
        let record = SiteRecord {
            id: SiteId::new(),
            name: v.name.clone(),
            provider: ProviderKind::Bunny,
            container: v.zone.clone(),
            region: v.region_code.clone(),
            host,
            s3: None,
            created_at: Utc::now(),
        };
        let (creds, store, book) = (self.creds.clone(), self.store.clone(), self.book.clone());
        let password = v.password.clone();
        blocking(move || save_to_stores(&*creds, &store, &book, record, &password)).await
    }

    /// A storage client for `site`, with its password read from the credential store.
    pub(super) async fn connect(
        &self,
        site: &SiteRecord,
    ) -> Result<Arc<dyn StorageProvider>, String> {
        let (creds, factory, site) = (self.creds.clone(), self.factory.clone(), site.clone());
        blocking(move || connect_site(&*creds, &*factory, &site)).await
    }
}

/// Makes the storage client for `cfg`. Building one sets up TLS and a connection pool, which
/// takes a while, so it is done on a blocking thread.
async fn build_client(
    factory: Arc<dyn ProviderFactory>,
    cfg: BunnyConfig,
) -> Result<Arc<dyn StorageProvider>, ProviderError> {
    blocking(move || factory.bunny(cfg)).await
}

/// Puts the new site's password in the credential store and the site in the database, then reads
/// the list of sites back and lists the new site. Whichever write fails, the other leaves nothing
/// behind: no password stays for a site that was not saved. It is one blocking task from the first
/// write to the last change of the list, and takes its inputs by value, so that nobody waiting for
/// it can leave it half done.
fn save_to_stores(
    creds: &dyn CredentialStore,
    store: &Mutex<Store>,
    book: &Mutex<Book>,
    record: SiteRecord,
    password: &str,
) -> Result<SiteRecord, String> {
    let key = CredentialKey::site(&record.id);
    creds
        .set(&key, password)
        .map_err(copy::password_not_saved)?;
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

/// The storage client for `site`, with its password read from the credential store.
fn connect_site(
    creds: &dyn CredentialStore,
    factory: &dyn ProviderFactory,
    site: &SiteRecord,
) -> Result<Arc<dyn StorageProvider>, String> {
    let password = match creds.get(&CredentialKey::site(&site.id)) {
        Ok(Some(password)) => password,
        Ok(None) => return Err(copy::no_password_saved(&site.name)),
        Err(e) => return Err(copy::password_unreadable(&site.name, e)),
    };
    let client = match site.provider {
        ProviderKind::Bunny => factory.bunny(interactive(BunnyConfig::new(
            &site.container,
            &site.host,
            password,
        ))),
        // Nothing saves an S3 site before S3 browsing arrives.
        ProviderKind::S3 => return Err(copy::S3_LATER.into()),
    };
    client.map_err(|e| copy::describe(&e, &site.container, &site.host, Attempt::Setup))
}
