//! The state behind the window: the saved sites, the cloud site being browsed and the folder each
//! pane shows. Every method returns a plain view struct that the binding (`binding.rs`) applies to
//! the window, so all of this is testable without a window, a network or a real credential store.
//!
//! The state is in four parts, each behind its own lock: the site database, the site list (with
//! each site's health), the cloud pane and the This PC pane. No lock is held across an `await` or
//! while another is taken, so a listing that takes long (or never ends) holds up nothing else:
//! the other pane, saving a site and testing one go ahead.
//!
//! Requests to move a pane are put in order by [`Ticket`]s, taken on the UI thread when the user
//! asks. A request takes effect only if no later one already has, and a listing's answer is used
//! only if its request is still the latest to have taken effect; anything else is from a place
//! the user has left, and is dropped.
//!
//! A site's password is read from the credential store only when the site is selected, and goes
//! straight into the storage client's configuration. It is never kept here, written to the site
//! database, logged or put in a view.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

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

/// A place in line for moving one pane, taken on the UI thread at the moment the user asks
/// ([`Session::remote_ticket`], [`Session::local_ticket`]). Requests are thus ordered as the user
/// made them, not as their tasks happen to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticket(u64);

/// What the cloud pane and the window chrome around it show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteView {
    /// Goes up with every change to what the pane shows. Views are made side by side and can
    /// reach the window out of order; one older than the view on screen is dropped. A click on
    /// a row or a crumb names the version it saw, and is ignored if the pane has changed since.
    pub version: u64,
    pub rows: Vec<RowData>,
    pub crumbs: Vec<String>,
    /// The toolbar's connection pill: "HTTPS · 21 ms", "Connecting…" while a site opens, "Not
    /// connected" after a failed connection, or empty with no site.
    pub connection: String,
    /// The pill is green (connected or connecting) rather than orange.
    pub connection_ok: bool,
    /// The toolbar subtitle: "Bunny Storage · {zone}".
    pub endpoint: String,
    /// The cloud pane's header chip: the storage endpoint, "https://{host}".
    pub chip: String,
    /// The right side of the status bar: "{http_version} · {host}", or the host while connecting.
    pub status_right: String,
    /// There is a folder to go back to, or forward to.
    pub can_back: bool,
    pub can_forward: bool,
    pub error: Option<String>,
}

/// What the This PC pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalView {
    /// As [`RemoteView::version`], for this pane.
    pub version: u64,
    pub rows: Vec<RowData>,
    pub crumbs: Vec<String>,
    /// For the status bar: "{n} items", plus how many could not be read.
    pub status: String,
    pub can_back: bool,
    pub can_forward: bool,
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

/// Locks `mutex`. Every change made under these locks is a few assignments or a sort, so a panic
/// elsewhere cannot leave the state unusable, and a poisoned lock is used as it is.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a navigation moves through a pane's history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// To a new place: it goes on the back list, and the forward list is cleared.
    Go,
    Back,
    Forward,
}

/// The places a pane can go back and forward to, Finder-style. The last entry is the nearest.
#[derive(Debug)]
struct History<T> {
    back: Vec<T>,
    forward: Vec<T>,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            back: Vec::new(),
            forward: Vec::new(),
        }
    }
}

impl<T: Clone> History<T> {
    fn back_to(&self) -> Option<T> {
        self.back.last().cloned()
    }

    fn forward_to(&self) -> Option<T> {
        self.forward.last().cloned()
    }

    /// The pane moved away from `left` by `step`.
    fn record(&mut self, step: Step, left: T) {
        match step {
            Step::Go => {
                self.back.push(left);
                self.forward.clear();
            }
            Step::Back => {
                self.back.pop();
                self.forward.push(left);
            }
            Step::Forward => {
                self.forward.pop();
                self.back.push(left);
            }
        }
    }
}

/// The saved sites, as the sidebar shows them.
struct Book {
    /// As the database lists them: by name.
    sites: Vec<SiteRecord>,
    /// Whether the last request to each site worked. Sites not in here have not been asked yet.
    healthy: HashMap<SiteId, bool>,
    /// A problem with the app itself (the site database), shown until the app is restarted.
    notice: Option<String>,
}

/// The cloud pane.
struct RemotePane {
    /// The latest request that took effect. An older request is too late (the user has moved
    /// on); the answer to a listing is used only while its request is still this one.
    latest: Ticket,
    /// See [`RemoteView::version`].
    version: u64,
    sort: (SortKey, bool),
    /// Folders within the open site.
    history: History<RemotePath>,
    open: Option<Remote>,
}

/// The open cloud site and the folder shown from it.
struct Remote {
    site: SiteRecord,
    /// `None` while the site opens, and when no client could be made (no password saved, say).
    provider: Option<Arc<dyn StorageProvider>>,
    dir: RemotePath,
    /// In the order the rows show them, so a row index is an index into this list.
    entries: Vec<Entry>,
    /// How the last successful listing travelled; `None` while the site opens and after a
    /// failed connection.
    info: Option<RequestInfo>,
    error: Option<String>,
}

impl Remote {
    fn new(site: SiteRecord) -> Self {
        Self {
            site,
            provider: None,
            dir: RemotePath::root(),
            entries: Vec::new(),
            info: None,
            error: None,
        }
    }
}

/// The This PC pane.
struct LocalPane {
    /// As [`RemotePane::latest`].
    latest: Ticket,
    /// See [`LocalView::version`].
    version: u64,
    sort: (SortKey, bool),
    /// Places: `None` is the list of drives.
    history: History<Option<PathBuf>>,
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

/// The app's state. It is `Send` and `Sync`: the app shares one `Arc<Session>` between the UI
/// thread and the tasks that list folders.
pub struct Session {
    creds: Arc<dyn CredentialStore>,
    factory: Arc<dyn ProviderFactory>,
    /// Local time, for the "Today 10:02" dates in the rows.
    now: fn() -> NaiveDateTime,
    /// The site database. Its own lock, so that a slow disk write holds up only other writes.
    store: Mutex<Store>,
    book: Mutex<Book>,
    remote: Mutex<RemotePane>,
    local: Mutex<LocalPane>,
    /// The last ticket given out for each pane.
    remote_tickets: AtomicU64,
    local_tickets: AtomicU64,
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
        let mut book = Book {
            sites: Vec::new(),
            healthy: HashMap::new(),
            notice: None,
        };
        match store.list_sites() {
            Ok(sites) => book.sites = sites,
            Err(e) => book.notice = Some(format!("Couldn't read the saved sites: {e}")),
        }
        let sort = (SortKey::Name, true);
        let mut drives = drive_entries();
        sort_rows(&mut drives, sort.0, sort.1);
        Self {
            creds,
            factory,
            now,
            store: Mutex::new(store),
            book: Mutex::new(book),
            remote: Mutex::new(RemotePane {
                latest: Ticket(0),
                version: 0,
                sort,
                history: History::default(),
                open: None,
            }),
            local: Mutex::new(LocalPane {
                latest: Ticket(0),
                version: 0,
                sort,
                history: History::default(),
                dir: None,
                entries: drives,
                skipped: 0,
                error: None,
            }),
            remote_tickets: AtomicU64::new(0),
            local_tickets: AtomicU64::new(0),
        }
    }

    /// A place in line for moving the cloud pane. Take it when the user asks, before handing the
    /// request to a task.
    pub fn remote_ticket(&self) -> Ticket {
        Ticket(self.remote_tickets.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// As [`Self::remote_ticket`], for the This PC pane.
    pub fn local_ticket(&self) -> Ticket {
        Ticket(self.local_tickets.fetch_add(1, Ordering::SeqCst) + 1)
    }

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
                let (service, mono) = match site.provider {
                    ProviderKind::Bunny => ("Bunny", "B"),
                };
                SiteItemData {
                    id: site.id.0.clone(),
                    name: site.name.clone(),
                    subtitle: format!("{service} · {}", region_city(&site.region)),
                    mono: mono.to_owned(),
                    healthy: book.healthy.get(&site.id).copied().unwrap_or(true),
                }
            })
            .collect()
    }

    /// Opens the site at `index` in [`Self::sites`] and lists its root.
    ///
    /// The pane switches to the site at once: empty, "Connecting…", with the site's endpoint and
    /// a fresh history. `switched` gets that view, so the window can show it before the password
    /// is read, the client is set up and the listing comes back, all of which can take a while.
    /// Anything still on its way from the site shown before is dropped when it arrives.
    pub async fn select_site(
        &self,
        ticket: Ticket,
        index: usize,
        switched: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let site = lock(&self.book).sites.get(index).cloned();
        let switch = {
            let mut pane = lock(&self.remote);
            if ticket < pane.latest {
                return self.remote_view_of(&pane);
            }
            pane.latest = ticket;
            pane.version += 1;
            pane.history = History::default();
            pane.open = site.clone().map(Remote::new);
            self.remote_view_of(&pane)
        };
        let Some(site) = site else {
            return switch;
        };
        switched(switch);
        // No lock is held while the password is read and the client is made.
        let connected = self.connect(&site);
        let provider = {
            let mut pane = lock(&self.remote);
            let current = pane.latest == ticket;
            let Some(remote) = pane.open.as_mut().filter(|_| current) else {
                return self.remote_view_of(&pane);
            };
            match connected {
                Ok(provider) => {
                    remote.provider = Some(provider.clone());
                    provider
                }
                Err(message) => {
                    remote.error = Some(message);
                    pane.version += 1;
                    return self.remote_view_of(&pane);
                }
            }
        };
        let root = RemotePath::root();
        let result = provider.list(&root).await;
        self.finish_listing(ticket, Step::Go, root, result)
    }

    /// Opens row `row` of the cloud pane if it is a folder. `seen` is the version of the view
    /// the row was on: if the pane has changed since, the row may be another one, and nothing
    /// happens.
    pub async fn open_remote(&self, ticket: Ticket, row: usize, seen: u64) -> RemoteView {
        self.navigate_remote(ticket, Step::Go, |pane| {
            if pane.version != seen {
                return None;
            }
            pane.open
                .as_ref()?
                .entries
                .get(row)
                .filter(|entry| entry.is_folder)
                .map(|entry| entry.path.clone())
        })
        .await
    }

    pub async fn remote_up(&self, ticket: Ticket) -> RemoteView {
        self.navigate_remote(ticket, Step::Go, |pane| pane.open.as_ref()?.dir.parent())
            .await
    }

    /// Goes to the folder of cloud breadcrumb `index` (0 is the zone's root), if the crumbs are
    /// still those of view version `seen`.
    pub async fn remote_crumb(&self, ticket: Ticket, index: usize, seen: u64) -> RemoteView {
        self.navigate_remote(ticket, Step::Go, |pane| {
            if pane.version != seen {
                return None;
            }
            remote_ancestor(&pane.open.as_ref()?.dir, index)
        })
        .await
    }

    pub async fn remote_back(&self, ticket: Ticket) -> RemoteView {
        self.navigate_remote(ticket, Step::Back, |pane| pane.history.back_to())
            .await
    }

    pub async fn remote_forward(&self, ticket: Ticket) -> RemoteView {
        self.navigate_remote(ticket, Step::Forward, |pane| pane.history.forward_to())
            .await
    }

    /// Reorders the cloud pane (and every later listing in it).
    pub fn sort_remote(&self, key: SortKey, ascending: bool) -> RemoteView {
        let mut pane = lock(&self.remote);
        pane.sort = (key, ascending);
        if let Some(remote) = pane.open.as_mut() {
            sort_remote_entries(&mut remote.entries, key, ascending);
        }
        pane.version += 1;
        self.remote_view_of(&pane)
    }

    /// Lists the root of the zone the sheet describes, with the password as typed. It uses
    /// nothing of the session but the factory, so it waits for no listing.
    pub async fn test_connection(&self, v: &ValidSite) -> TestResult {
        let Some(host) = storage_host(&v.region_code) else {
            return TestResult {
                ok: false,
                text: "Choose a region.".into(),
            };
        };
        let root = RemotePath::root();
        let listed = match self
            .factory
            .bunny(BunnyConfig::new(&v.zone, &host, &v.password))
        {
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

    /// Saves a new site: its password in the credential store, everything else in the database.
    pub async fn save_site(&self, v: &ValidSite) -> Result<SiteRecord, String> {
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
        let listed = {
            let store = lock(&self.store);
            if let Err(e) = store.add_site(&record) {
                drop(store);
                // No password may stay behind for a site that was not saved.
                let _ = self.creds.delete(&key);
                return Err(format!("Couldn't save the site: {e}"));
            }
            store.list_sites()
        };
        let mut book = lock(&self.book);
        match listed {
            Ok(sites) => book.sites = sites,
            Err(e) => book.notice = Some(format!("Couldn't read the saved sites: {e}")),
        }
        if !book.sites.iter().any(|site| site.id == record.id) {
            book.sites.push(record.clone());
        }
        Ok(record)
    }

    pub fn local_view(&self) -> LocalView {
        self.local_view_of(&lock(&self.local))
    }

    /// Opens row `row` of the This PC pane if it is a folder (or a drive), and if the pane is
    /// still at view version `seen` (as for [`Self::open_remote`]).
    pub async fn open_local(&self, ticket: Ticket, row: usize, seen: u64) -> LocalView {
        self.navigate_local(ticket, Step::Go, |pane| {
            if pane.version != seen {
                return None;
            }
            let entry = pane.entries.get(row).filter(|entry| entry.is_folder)?;
            Some(Some(match &pane.dir {
                Some(dir) => dir.join(&entry.name),
                None => PathBuf::from(format!("{}\\", entry.name)),
            }))
        })
        .await
    }

    /// Goes to the parent folder; from a drive's root, to the list of drives.
    pub async fn local_up(&self, ticket: Ticket) -> LocalView {
        self.navigate_local(ticket, Step::Go, |pane| {
            pane.dir
                .as_deref()
                .map(|dir| dir.parent().map(Path::to_path_buf))
        })
        .await
    }

    /// Goes to the folder of This PC breadcrumb `index` (0 is the drive), if the crumbs are
    /// still those of view version `seen`.
    pub async fn local_crumb(&self, ticket: Ticket, index: usize, seen: u64) -> LocalView {
        self.navigate_local(ticket, Step::Go, |pane| {
            if pane.version != seen {
                return None;
            }
            pane.dir
                .as_deref()
                .and_then(|dir| local_ancestor(dir, index))
                .map(Some)
        })
        .await
    }

    pub async fn local_back(&self, ticket: Ticket) -> LocalView {
        self.navigate_local(ticket, Step::Back, |pane| pane.history.back_to())
            .await
    }

    pub async fn local_forward(&self, ticket: Ticket) -> LocalView {
        self.navigate_local(ticket, Step::Forward, |pane| pane.history.forward_to())
            .await
    }

    /// Reorders the This PC pane (and every later listing in it).
    pub fn sort_local(&self, key: SortKey, ascending: bool) -> LocalView {
        let mut pane = lock(&self.local);
        pane.sort = (key, ascending);
        sort_rows(&mut pane.entries, key, ascending);
        pane.version += 1;
        self.local_view_of(&pane)
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

    /// Lists the folder `pick` chooses in the open site (if it chooses one) and goes there by
    /// `step`. A request older than one that has taken effect is dropped; a request that goes
    /// nowhere takes no effect.
    async fn navigate_remote(
        &self,
        ticket: Ticket,
        step: Step,
        pick: impl FnOnce(&RemotePane) -> Option<RemotePath>,
    ) -> RemoteView {
        let (provider, dir) = {
            let mut pane = lock(&self.remote);
            if ticket < pane.latest {
                return self.remote_view_of(&pane);
            }
            let provider = pane.open.as_ref().and_then(|r| r.provider.clone());
            let Some((provider, dir)) = provider.zip(pick(&pane)) else {
                return self.remote_view_of(&pane);
            };
            pane.latest = ticket;
            (provider, dir)
        };
        let result = provider.list(&dir).await;
        self.finish_listing(ticket, step, dir, result)
    }

    /// Takes in the answer to the listing of `dir` that request `ticket` asked for. If a later
    /// request has taken effect since, the answer is dropped and the pane stays as it is.
    /// Otherwise, on success the pane moves to `dir`. A folder that is not there leaves the pane
    /// where it was, connected; any other failure leaves it there, not connected.
    fn finish_listing(
        &self,
        ticket: Ticket,
        step: Step,
        dir: RemotePath,
        result: Result<Listing, ProviderError>,
    ) -> RemoteView {
        // A missing folder is an answer from the server: the site itself is fine.
        let healthy = !matches!(&result, Err(e) if *e != ProviderError::NotFound);
        let (view, site) = {
            let mut pane = lock(&self.remote);
            let (key, ascending) = pane.sort;
            let current = pane.latest == ticket;
            let Some(remote) = pane.open.as_mut().filter(|_| current) else {
                return self.remote_view_of(&pane);
            };
            let mut left = None;
            match result {
                Ok(listing) => {
                    if remote.dir != dir {
                        left = Some(std::mem::replace(&mut remote.dir, dir));
                    }
                    remote.entries = listing.entries;
                    sort_remote_entries(&mut remote.entries, key, ascending);
                    remote.info = Some(listing.info);
                    remote.error = None;
                }
                Err(ProviderError::NotFound) => {
                    remote.error = Some(describe(
                        &ProviderError::NotFound,
                        &remote.site.zone,
                        &remote.site.host,
                        &dir,
                    ));
                }
                Err(error) => {
                    remote.info = None;
                    remote.error =
                        Some(describe(&error, &remote.site.zone, &remote.site.host, &dir));
                }
            }
            let site = remote.site.id.clone();
            if let Some(left) = left {
                pane.history.record(step, left);
            }
            pane.version += 1;
            (self.remote_view_of(&pane), site)
        };
        lock(&self.book).healthy.insert(site, healthy);
        view
    }

    fn remote_view_of(&self, pane: &RemotePane) -> RemoteView {
        let Some(remote) = &pane.open else {
            return RemoteView {
                version: pane.version,
                ..RemoteView::default()
            };
        };
        let (key, ascending) = pane.sort;
        let site = &remote.site;
        let service = match site.provider {
            ProviderKind::Bunny => "Bunny Storage",
        };
        let (connection, status_right) = match (&remote.info, &remote.error) {
            (Some(info), _) => (
                format!("HTTPS · {} ms", info.latency.as_millis()),
                format!("{} · {}", info.http_version, site.host),
            ),
            (None, Some(_)) => ("Not connected".to_owned(), String::new()),
            (None, None) => ("Connecting…".to_owned(), site.host.clone()),
        };
        RemoteView {
            version: pane.version,
            rows: rows_from_remote_sorted(&remote.entries, (self.now)(), key, ascending),
            crumbs: remote_crumbs(&site.zone, &remote.dir),
            connection,
            connection_ok: remote.info.is_some() || remote.error.is_none(),
            endpoint: format!("{service} · {}", site.zone),
            chip: format!("https://{}", site.host),
            status_right,
            can_back: !pane.history.back.is_empty(),
            can_forward: !pane.history.forward.is_empty(),
            error: remote.error.clone(),
        }
    }

    /// Lists the place `pick` chooses (if it chooses one; `None` inside is the list of drives) on
    /// tokio's blocking threads and goes there by `step`. As for the cloud pane, a request older
    /// than one that has taken effect is dropped, an answer is used only while its request is
    /// the latest, and a failure leaves the pane where it was.
    async fn navigate_local(
        &self,
        ticket: Ticket,
        step: Step,
        pick: impl FnOnce(&LocalPane) -> Option<Option<PathBuf>>,
    ) -> LocalView {
        let target = {
            let mut pane = lock(&self.local);
            if ticket < pane.latest {
                return self.local_view_of(&pane);
            }
            let Some(target) = pick(&pane) else {
                return self.local_view_of(&pane);
            };
            pane.latest = ticket;
            target
        };
        let listed = match &target {
            None => Ok(LocalListing {
                entries: drive_entries(),
                skipped: 0,
            }),
            Some(dir) => {
                let dir = dir.clone();
                tokio::task::spawn_blocking(move || list_local(&dir))
                    .await
                    .unwrap_or_else(|e| Err(io::Error::other(e)))
            }
        };
        let mut pane = lock(&self.local);
        if pane.latest != ticket {
            return self.local_view_of(&pane);
        }
        match listed {
            Ok(listing) => {
                let (key, ascending) = pane.sort;
                if pane.dir != target {
                    let left = std::mem::replace(&mut pane.dir, target);
                    pane.history.record(step, left);
                }
                pane.entries = listing.entries;
                sort_rows(&mut pane.entries, key, ascending);
                pane.skipped = listing.skipped;
                pane.error = None;
            }
            Err(e) => {
                let place = target
                    .as_deref()
                    .map_or_else(|| "This PC".to_owned(), |dir| dir.display().to_string());
                pane.error = Some(format!("Couldn't open {place}: {e}"));
            }
        }
        pane.version += 1;
        self.local_view_of(&pane)
    }

    fn local_view_of(&self, pane: &LocalPane) -> LocalView {
        let (key, ascending) = pane.sort;
        LocalView {
            version: pane.version,
            rows: rows_from_local_sorted(&pane.entries, (self.now)(), key, ascending),
            crumbs: pane.dir.as_deref().map(local_crumbs).unwrap_or_default(),
            status: items_status(pane.entries.len(), pane.skipped),
            can_back: !pane.history.back.is_empty(),
            can_forward: !pane.history.forward.is_empty(),
            error: pane.error.clone(),
        }
    }
}

#[cfg(test)]
pub(crate) mod fakes {
    use super::*;
    use chrono::NaiveDate;
    use httporter_core::{ByteStream, Capabilities, MockProvider, UploadOptions, UploadSource};
    use httporter_platform::MemoryStore;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    /// Hands out storages by zone (a default one for every other zone) and remembers the
    /// configurations it was given.
    pub(crate) struct FakeFactory {
        provider: Arc<dyn StorageProvider>,
        by_zone: Mutex<HashMap<String, Arc<dyn StorageProvider>>>,
        held: Mutex<HashMap<String, Held>>,
        configs: Mutex<Vec<BunnyConfig>>,
    }

    /// The factory's side of a [`Hold`].
    struct Held {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }

    /// Holds the next client made for one zone inside `bunny()` (where the real factory reads
    /// no password but builds HTTP clients, which takes a while), until released.
    pub(crate) struct Hold {
        entered: mpsc::Receiver<()>,
        release: mpsc::Sender<()>,
    }

    impl Hold {
        /// Waits (off the async threads) until the client is being made.
        pub(crate) async fn entered(&self) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match self.entered.try_recv() {
                    Ok(()) => return,
                    Err(mpsc::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    Err(e) => panic!("the client was never made: {e:?}"),
                }
            }
        }

        pub(crate) fn release(&self) {
            self.release.send(()).expect("the factory is still waiting");
        }
    }

    impl FakeFactory {
        pub(crate) fn new(provider: Arc<dyn StorageProvider>) -> Arc<Self> {
            Arc::new(Self {
                provider,
                by_zone: Mutex::new(HashMap::new()),
                held: Mutex::new(HashMap::new()),
                configs: Mutex::new(Vec::new()),
            })
        }

        /// Hands out `provider` for `zone` from now on.
        pub(crate) fn serve(&self, zone: &str, provider: Arc<dyn StorageProvider>) {
            lock(&self.by_zone).insert(zone.to_owned(), provider);
        }

        /// Holds the next client made for `zone` until the returned [`Hold`] releases it. The
        /// hold blocks the thread it happens on, so tests using it run on several threads.
        pub(crate) fn hold(&self, zone: &str) -> Hold {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            lock(&self.held).insert(
                zone.to_owned(),
                Held {
                    entered: entered_tx,
                    release: release_rx,
                },
            );
            Hold {
                entered: entered_rx,
                release: release_tx,
            }
        }

        /// (zone, host, password) of every configuration handed in so far.
        pub(crate) fn configs(&self) -> Vec<(String, String, String)> {
            lock(&self.configs)
                .iter()
                .map(|c| (c.zone.clone(), c.host.clone(), c.password.clone()))
                .collect()
        }
    }

    impl ProviderFactory for FakeFactory {
        fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError> {
            let held = lock(&self.held).remove(&cfg.zone);
            if let Some(held) = held {
                let _ = held.entered.send(());
                let _ = held.release.recv();
            }
            let provider = lock(&self.by_zone)
                .get(&cfg.zone)
                .cloned()
                .unwrap_or_else(|| self.provider.clone());
            lock(&self.configs).push(cfg);
            Ok(provider)
        }
    }

    /// A storage that holds back every listing of one folder until [`Gated::open`] lets one
    /// through: a server that has stopped answering, for as long as it is not opened. Everything
    /// else goes to the mock inside.
    pub(crate) struct Gated {
        inner: MockProvider,
        gate: RemotePath,
        passes: tokio::sync::Semaphore,
        waiting: AtomicUsize,
    }

    /// Counts a listing as held back for as long as it waits (also if it is given up).
    struct Waiting<'a>(&'a AtomicUsize);

    impl Drop for Waiting<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl Gated {
        pub(crate) fn new(inner: MockProvider, gate: &str) -> Arc<Self> {
            Arc::new(Self {
                inner,
                gate: RemotePath::parse(gate).unwrap(),
                passes: tokio::sync::Semaphore::new(0),
                waiting: AtomicUsize::new(0),
            })
        }

        /// Lets one held-back listing (or the next one) through.
        pub(crate) fn open(&self) {
            self.passes.add_permits(1);
        }

        /// How many listings are being held back right now.
        pub(crate) fn waiting(&self) -> usize {
            self.waiting.load(Ordering::SeqCst)
        }

        pub(crate) fn mock(&self) -> &MockProvider {
            &self.inner
        }
    }

    #[async_trait::async_trait]
    impl StorageProvider for Gated {
        fn capabilities(&self) -> &Capabilities {
            self.inner.capabilities()
        }
        fn endpoint(&self) -> String {
            self.inner.endpoint()
        }
        async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
            if *dir == self.gate {
                self.waiting.fetch_add(1, Ordering::SeqCst);
                let _waiting = Waiting(&self.waiting);
                self.passes
                    .acquire()
                    .await
                    .expect("the gate is never closed")
                    .forget();
            }
            self.inner.list(dir).await
        }
        async fn upload(
            &self,
            path: &RemotePath,
            source: UploadSource,
            opts: &UploadOptions,
        ) -> Result<(), ProviderError> {
            self.inner.upload(path, source, opts).await
        }
        async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
            self.inner.download(path).await
        }
        async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
            self.inner.delete_file(path).await
        }
        async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
            self.inner.delete_folder(dir).await
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
