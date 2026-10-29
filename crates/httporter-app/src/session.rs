//! The state behind the window: the saved sites, the cloud site being browsed and the folder each
//! pane shows. Every method returns a plain view struct that the binding (`binding.rs`) applies to
//! the window, so all of this is testable without a window, a network or a real credential store.
//!
//! A site's password is read from the credential store only when the site is selected, and goes
//! straight into the storage client's configuration. It is never kept here, written to the site
//! database, logged or put in a view.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{NaiveDateTime, Utc};
use httporter_bunny::{BunnyConfig, BunnyStorage, REGIONS, storage_host};
use httporter_core::{
    Entry, Listing, ProviderError, ProviderKind, RemotePath, RequestInfo, SiteId, SiteRecord,
    StorageProvider,
};
use httporter_platform::{CredentialKey, CredentialStore, Store};

use crate::add_site::ValidSite;
use crate::local_fs::{LocalEntry, LocalListing, list_drives, list_local};
use crate::sort::{SortKey, sort_rows};
use crate::viewmodel::{
    RowData, local_crumbs, remote_crumbs, rows_from_local_sorted, rows_from_remote_sorted,
    sort_remote_entries,
};

/// Builds the storage client for a site. The app uses [`RealFactory`]; tests hand out mocks.
pub trait ProviderFactory: Send + Sync {
    fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError>;
}

/// Talks to the real Bunny Storage API.
pub struct RealFactory;

impl ProviderFactory for RealFactory {
    fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError> {
        Ok(Arc::new(BunnyStorage::new(cfg)?))
    }
}

/// What the cloud pane and the window chrome around it show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteView {
    pub rows: Vec<RowData>,
    pub crumbs: Vec<String>,
    /// The toolbar's connection pill: "HTTPS · 21 ms", or "Not connected" after a failure.
    pub connection: String,
    /// The toolbar subtitle: "Bunny Storage · {zone}".
    pub endpoint: String,
    /// The cloud pane's header chip: the storage endpoint, "https://{host}".
    pub chip: String,
    /// The right side of the status bar: "{http_version} · {host}".
    pub status_right: String,
    pub error: Option<String>,
}

/// What the This PC pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalView {
    pub rows: Vec<RowData>,
    pub crumbs: Vec<String>,
    /// For the status bar: "{n} items", plus how many could not be read.
    pub status: String,
    pub error: Option<String>,
}

/// The outcome of the Add Site sheet's Test Connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    pub ok: bool,
    pub text: String,
}

/// One row of the sidebar's site list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteItemData {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    pub mono: String,
    /// The last request to the site succeeded (a site no request has gone to yet counts as
    /// healthy).
    pub healthy: bool,
}

/// "1 item", "2 items".
pub fn items(n: usize) -> String {
    if n == 1 {
        "1 item".to_owned()
    } else {
        format!("{n} items")
    }
}

/// The status line for a local listing: "{n} items", and " · {m} couldn't be read" when some
/// entries were skipped.
pub fn items_status(n: usize, skipped: usize) -> String {
    if skipped == 0 {
        items(n)
    } else {
        format!("{} · {skipped} couldn't be read", items(n))
    }
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

/// What to say about a failed request to `zone` on `host` that tried to list `dir`.
fn describe(error: &ProviderError, zone: &str, host: &str, dir: &RemotePath) -> String {
    match error {
        ProviderError::Auth => {
            format!("Couldn't sign in to {zone}. Check the password, zone name and region.")
        }
        ProviderError::NotFound => format!("Couldn't find /{} in {zone}.", dir.as_str()),
        other => format!("Couldn't reach {host}: {other}"),
    }
}

/// The city a region code stands for ("Singapore"); the code itself when it is not known.
fn region_city(code: &str) -> &str {
    REGIONS
        .iter()
        .find(|(known, _)| *known == code)
        .map_or(code, |(_, city)| city)
}

/// The open cloud site and the folder shown from it.
struct Remote {
    site: SiteRecord,
    /// `None` when no client could be made (no password saved, for one).
    provider: Option<Arc<dyn StorageProvider>>,
    dir: RemotePath,
    /// In the order the rows show them, so a row index is an index into this list.
    entries: Vec<Entry>,
    /// How the last successful listing travelled; `None` after a failure.
    info: Option<RequestInfo>,
    error: Option<String>,
}

impl Remote {
    fn new(site: SiteRecord, provider: Option<Arc<dyn StorageProvider>>) -> Self {
        Self {
            site,
            provider,
            dir: RemotePath::root(),
            entries: Vec::new(),
            info: None,
            error: None,
        }
    }
}

/// The folder the This PC pane shows.
struct Local {
    /// `None` for the list of drives.
    dir: Option<PathBuf>,
    /// In the order the rows show them, so a row index is an index into this list.
    entries: Vec<LocalEntry>,
    skipped: usize,
    error: Option<String>,
}

/// The PC's drives, as folders named like their breadcrumb ("C:").
fn drive_entries() -> Vec<LocalEntry> {
    list_drives()
        .iter()
        .map(|root| LocalEntry {
            name: root.to_string_lossy().trim_end_matches('\\').to_owned(),
            is_folder: true,
            size: 0,
            modified: None,
        })
        .collect()
}

/// The folder This PC breadcrumb `index` of `dir` stands for (0 is the drive), or `None` for the
/// current folder and past it.
fn local_ancestor(dir: &Path, index: usize) -> Option<PathBuf> {
    let mut chain: Vec<&Path> = dir.ancestors().collect();
    chain.reverse();
    // The last one is `dir` itself.
    (index + 1 < chain.len()).then(|| chain[index].to_path_buf())
}

/// The folder cloud breadcrumb `index` of `dir` stands for (0 is the zone's root), or `None` for
/// the current folder and past it.
fn remote_ancestor(dir: &RemotePath, index: usize) -> Option<RemotePath> {
    let segments: Vec<&str> = dir.segments().collect();
    if index >= segments.len() {
        return None;
    }
    segments[..index]
        .iter()
        .try_fold(RemotePath::root(), |path, segment| path.join(segment))
        .ok()
}

/// The app's state. It is `Send` but not `Sync` (the site database is a SQLite connection): the
/// app keeps it behind a `tokio::sync::Mutex`.
pub struct Session {
    store: Store,
    creds: Arc<dyn CredentialStore>,
    factory: Arc<dyn ProviderFactory>,
    /// Local time, for the "Today 10:02" dates in the rows.
    now: fn() -> NaiveDateTime,
    /// As the database lists them: by name.
    sites: Vec<SiteRecord>,
    /// Whether the last request to each site worked. Sites not in here have not been asked yet.
    healthy: HashMap<SiteId, bool>,
    /// A problem with the app itself (the site database), shown until the app is restarted.
    notice: Option<String>,
    remote: Option<Remote>,
    remote_sort: (SortKey, bool),
    local: Local,
    local_sort: (SortKey, bool),
}

impl Session {
    /// A session over the sites in `store`, with the This PC pane at the list of drives and no
    /// site open.
    pub fn new(
        store: Store,
        creds: Arc<dyn CredentialStore>,
        factory: Arc<dyn ProviderFactory>,
        now: fn() -> NaiveDateTime,
    ) -> Self {
        let mut session = Self {
            store,
            creds,
            factory,
            now,
            sites: Vec::new(),
            healthy: HashMap::new(),
            notice: None,
            remote: None,
            remote_sort: (SortKey::Name, true),
            local: Local {
                dir: None,
                entries: Vec::new(),
                skipped: 0,
                error: None,
            },
            local_sort: (SortKey::Name, true),
        };
        session.reload_sites();
        session.show_local(
            None,
            LocalListing {
                entries: drive_entries(),
                skipped: 0,
            },
        );
        session
    }

    /// Shows `text` in the banner for the rest of the run (used when the site database could not
    /// be opened).
    pub fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
    }

    /// The saved sites, by name.
    pub fn sites(&self) -> Vec<SiteRecord> {
        self.sites.clone()
    }

    /// The sidebar's rows, in the order of [`Self::sites`].
    pub fn site_items(&self) -> Vec<SiteItemData> {
        self.sites
            .iter()
            .map(|site| {
                let (service, mono) = match site.provider {
                    ProviderKind::Bunny => ("Bunny", "B"),
                };
                SiteItemData {
                    id: site.id.0.clone(),
                    name: site.name.clone(),
                    subtitle: format!("{service} · {}", region_city(&site.region)),
                    mono: mono.to_owned(),
                    healthy: self.healthy.get(&site.id).copied().unwrap_or(true),
                }
            })
            .collect()
    }

    /// Every problem worth a banner, one per line: the app's own, the cloud pane's, This PC's.
    pub fn error_banner(&self) -> String {
        [
            self.notice.as_deref(),
            self.remote.as_ref().and_then(|r| r.error.as_deref()),
            self.local.error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
    }

    /// Opens the site at `index` in [`Self::sites`] and lists its root. Nothing changes while the
    /// listing is on its way, so a selection given up half-way (its task aborted) leaves the
    /// session as it was.
    pub async fn select_site(&mut self, index: usize) -> RemoteView {
        let Some(site) = self.sites.get(index).cloned() else {
            self.remote = None;
            return self.remote_view();
        };
        let provider = match self.connect(&site) {
            Ok(provider) => provider,
            Err(message) => {
                self.remote = Some(Remote {
                    error: Some(message),
                    ..Remote::new(site, None)
                });
                return self.remote_view();
            }
        };
        let root = RemotePath::root();
        let result = provider.list(&root).await;
        self.remote = Some(Remote::new(site, Some(provider)));
        self.finish_listing(root, result)
    }

    /// Opens row `row` of the cloud pane if it is a folder.
    pub async fn open_remote(&mut self, row: usize) -> RemoteView {
        let folder = self
            .remote
            .as_ref()
            .and_then(|r| r.entries.get(row))
            .filter(|entry| entry.is_folder)
            .map(|entry| entry.path.clone());
        match folder {
            Some(dir) => self.navigate_remote(dir).await,
            None => self.remote_view(),
        }
    }

    pub async fn remote_up(&mut self) -> RemoteView {
        match self.remote.as_ref().and_then(|r| r.dir.parent()) {
            Some(dir) => self.navigate_remote(dir).await,
            None => self.remote_view(),
        }
    }

    /// Goes to the folder of cloud breadcrumb `index` (0 is the zone's root).
    pub async fn remote_crumb(&mut self, index: usize) -> RemoteView {
        match self
            .remote
            .as_ref()
            .and_then(|r| remote_ancestor(&r.dir, index))
        {
            Some(dir) => self.navigate_remote(dir).await,
            None => self.remote_view(),
        }
    }

    /// Reorders the cloud pane (and every later listing in it).
    pub fn sort_remote(&mut self, key: SortKey, ascending: bool) -> RemoteView {
        self.remote_sort = (key, ascending);
        if let Some(remote) = self.remote.as_mut() {
            sort_remote_entries(&mut remote.entries, key, ascending);
        }
        self.remote_view()
    }

    /// Lists the root of the zone the sheet describes, with the password as typed.
    ///
    /// The test needs nothing from the session but its factory, so the future it returns owns
    /// what it uses: the session can be unlocked (and used for browsing) while the test runs.
    pub fn test_connection(
        &self,
        v: &ValidSite,
    ) -> impl Future<Output = TestResult> + Send + 'static {
        let factory = self.factory.clone();
        let v = v.clone();
        async move {
            let Some(host) = storage_host(&v.region_code) else {
                return TestResult {
                    ok: false,
                    text: "Choose a region.".into(),
                };
            };
            let root = RemotePath::root();
            let listed = match factory.bunny(BunnyConfig::new(&v.zone, &host, &v.password)) {
                Ok(provider) => provider.list(&root).await,
                Err(error) => Err(error),
            };
            match listed {
                Ok(listing) => TestResult {
                    ok: true,
                    text: format!(
                        "Connected over HTTPS · {} · {} ms · {} in /",
                        listing.info.http_version,
                        listing.info.latency.as_millis(),
                        items(listing.entries.len())
                    ),
                },
                Err(error) => TestResult {
                    ok: false,
                    text: describe(&error, &v.zone, &host, &root),
                },
            }
        }
    }

    /// Saves a new site: its password in the credential store, everything else in the database.
    pub async fn save_site(&mut self, v: &ValidSite) -> Result<SiteRecord, String> {
        let host = storage_host(&v.region_code).ok_or_else(|| "Choose a region.".to_owned())?;
        let record = SiteRecord {
            id: SiteId::new(),
            name: v.name.clone(),
            provider: ProviderKind::Bunny,
            zone: v.zone.clone(),
            region: v.region_code.clone(),
            host,
            created_at: Utc::now(),
        };
        let key = CredentialKey::site(&record.id);
        self.creds.set(&key, &v.password).map_err(|e| {
            format!("Couldn't save the password in Windows Credential Manager: {e}")
        })?;
        if let Err(e) = self.store.add_site(&record) {
            // No password may stay behind for a site that was not saved.
            let _ = self.creds.delete(&key);
            return Err(format!("Couldn't save the site: {e}"));
        }
        self.reload_sites();
        if !self.sites.iter().any(|site| site.id == record.id) {
            self.sites.push(record.clone());
        }
        Ok(record)
    }

    pub fn local_view(&self) -> LocalView {
        let (key, ascending) = self.local_sort;
        LocalView {
            rows: rows_from_local_sorted(&self.local.entries, (self.now)(), key, ascending),
            crumbs: self
                .local
                .dir
                .as_deref()
                .map(local_crumbs)
                .unwrap_or_default(),
            status: items_status(self.local.entries.len(), self.local.skipped),
            error: self.local.error.clone(),
        }
    }

    /// Opens row `row` of the This PC pane if it is a folder (or a drive).
    pub async fn open_local(&mut self, row: usize) -> LocalView {
        let folder = self
            .local
            .entries
            .get(row)
            .filter(|entry| entry.is_folder)
            .map(|entry| match &self.local.dir {
                Some(dir) => dir.join(&entry.name),
                None => PathBuf::from(format!("{}\\", entry.name)),
            });
        match folder {
            Some(dir) => self.navigate_local(Some(dir)).await,
            None => self.local_view(),
        }
    }

    /// Goes to the parent folder; from a drive's root, to the list of drives.
    pub async fn local_up(&mut self) -> LocalView {
        match self.local.dir.clone() {
            Some(dir) => {
                self.navigate_local(dir.parent().map(Path::to_path_buf))
                    .await
            }
            None => self.local_view(),
        }
    }

    /// Goes to the folder of This PC breadcrumb `index` (0 is the drive).
    pub async fn local_crumb(&mut self, index: usize) -> LocalView {
        match self
            .local
            .dir
            .as_deref()
            .and_then(|dir| local_ancestor(dir, index))
        {
            Some(dir) => self.navigate_local(Some(dir)).await,
            None => self.local_view(),
        }
    }

    /// Reorders the This PC pane (and every later listing in it).
    pub fn sort_local(&mut self, key: SortKey, ascending: bool) -> LocalView {
        self.local_sort = (key, ascending);
        sort_rows(&mut self.local.entries, key, ascending);
        self.local_view()
    }

    /// A storage client for `site`, with its password read from the credential store.
    fn connect(&self, site: &SiteRecord) -> Result<Arc<dyn StorageProvider>, String> {
        let password = match self.creds.get(&CredentialKey::site(&site.id)) {
            Ok(Some(password)) => password,
            Ok(None) => {
                return Err(format!(
                    "No password saved for {}. Edit the site to add it.",
                    site.name
                ));
            }
            Err(e) => {
                return Err(format!(
                    "Couldn't read the password for {} from Windows Credential Manager: {e}",
                    site.name
                ));
            }
        };
        let client = match site.provider {
            ProviderKind::Bunny => self
                .factory
                .bunny(BunnyConfig::new(&site.zone, &site.host, password)),
        };
        client.map_err(|e| describe(&e, &site.zone, &site.host, &RemotePath::root()))
    }

    async fn navigate_remote(&mut self, dir: RemotePath) -> RemoteView {
        let Some(provider) = self.remote.as_ref().and_then(|r| r.provider.clone()) else {
            return self.remote_view();
        };
        let result = provider.list(&dir).await;
        self.finish_listing(dir, result)
    }

    /// Takes in the answer to a listing of `dir`: on success the pane moves there; on failure it
    /// stays where it was and says why.
    fn finish_listing(
        &mut self,
        dir: RemotePath,
        result: Result<Listing, ProviderError>,
    ) -> RemoteView {
        let (key, ascending) = self.remote_sort;
        let Some(remote) = self.remote.as_mut() else {
            return RemoteView::default();
        };
        self.healthy.insert(remote.site.id.clone(), result.is_ok());
        match result {
            Ok(listing) => {
                remote.dir = dir;
                remote.entries = listing.entries;
                sort_remote_entries(&mut remote.entries, key, ascending);
                remote.info = Some(listing.info);
                remote.error = None;
            }
            Err(error) => {
                remote.info = None;
                remote.error = Some(describe(&error, &remote.site.zone, &remote.site.host, &dir));
            }
        }
        self.remote_view()
    }

    fn remote_view(&self) -> RemoteView {
        let Some(remote) = &self.remote else {
            return RemoteView::default();
        };
        let (key, ascending) = self.remote_sort;
        let site = &remote.site;
        let service = match site.provider {
            ProviderKind::Bunny => "Bunny Storage",
        };
        RemoteView {
            rows: rows_from_remote_sorted(&remote.entries, (self.now)(), key, ascending),
            crumbs: remote_crumbs(&site.zone, &remote.dir),
            connection: remote.info.as_ref().map_or_else(
                || "Not connected".to_owned(),
                |info| format!("HTTPS · {} ms", info.latency.as_millis()),
            ),
            endpoint: format!("{service} · {}", site.zone),
            chip: format!("https://{}", site.host),
            status_right: remote.info.as_ref().map_or_else(String::new, |info| {
                format!("{} · {}", info.http_version, site.host)
            }),
            error: remote.error.clone(),
        }
    }

    /// Lists `dir` (`None`: the drives) on tokio's blocking threads and, if that worked, shows
    /// it. On failure the pane stays where it was and says why.
    async fn navigate_local(&mut self, dir: Option<PathBuf>) -> LocalView {
        let Some(dir) = dir else {
            self.show_local(
                None,
                LocalListing {
                    entries: drive_entries(),
                    skipped: 0,
                },
            );
            return self.local_view();
        };
        let listed = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || list_local(&dir))
                .await
                .unwrap_or_else(|e| Err(io::Error::other(e)))
        };
        match listed {
            Ok(listing) => self.show_local(Some(dir), listing),
            Err(e) => {
                self.local.error = Some(format!("Couldn't open {}: {e}", dir.display()));
            }
        }
        self.local_view()
    }

    fn show_local(&mut self, dir: Option<PathBuf>, listing: LocalListing) {
        let (key, ascending) = self.local_sort;
        self.local = Local {
            dir,
            entries: listing.entries,
            skipped: listing.skipped,
            error: None,
        };
        sort_rows(&mut self.local.entries, key, ascending);
    }

    fn reload_sites(&mut self) {
        match self.store.list_sites() {
            Ok(sites) => self.sites = sites,
            Err(e) => self.notice = Some(format!("Couldn't read the saved sites: {e}")),
        }
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    use super::*;
    use chrono::NaiveDate;
    use httporter_platform::MemoryStore;
    use std::sync::Mutex;

    /// Hands out the same provider for every site and remembers the configurations it was given.
    pub(crate) struct FakeFactory {
        provider: Arc<dyn StorageProvider>,
        configs: Mutex<Vec<BunnyConfig>>,
    }

    impl FakeFactory {
        pub(crate) fn new(provider: Arc<dyn StorageProvider>) -> Arc<Self> {
            Arc::new(Self {
                provider,
                configs: Mutex::new(Vec::new()),
            })
        }

        /// (zone, host, password) of every configuration handed in so far.
        pub(crate) fn configs(&self) -> Vec<(String, String, String)> {
            self.configs
                .lock()
                .unwrap()
                .iter()
                .map(|c| (c.zone.clone(), c.host.clone(), c.password.clone()))
                .collect()
        }
    }

    impl ProviderFactory for FakeFactory {
        fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError> {
            self.configs.lock().unwrap().push(cfg);
            Ok(self.provider.clone())
        }
    }

    pub(crate) fn fixed_now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, 29)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    /// A session over an empty in-memory database and credential store, browsing `provider`.
    pub(crate) fn session_with(
        provider: Arc<dyn StorageProvider>,
    ) -> (Session, Arc<MemoryStore>, Arc<FakeFactory>) {
        let creds = Arc::new(MemoryStore::default());
        let factory = FakeFactory::new(provider);
        let session = Session::new(
            Store::open_in_memory().unwrap(),
            creds.clone(),
            factory.clone(),
            fixed_now,
        );
        (session, creds, factory)
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
