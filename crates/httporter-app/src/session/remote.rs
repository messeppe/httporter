//! The cloud pane: the site being browsed, the folder shown from it, and the requests that move it
//! (open a site, a folder, a crumb, Back, Forward, Refresh, a new sort).

use std::sync::Arc;

use httporter_core::{
    Entry, Listing, ProviderError, RemotePath, RequestInfo, SiteRecord, StorageProvider,
};

use super::sites::SiteProvider;
use super::{History, Session, Step, Ticket, lock};
use crate::copy::{self, Attempt};
use crate::sort::SortKey;
use crate::viewmodel::{RowData, remote_crumbs, rows_from_remote_sorted, sort_remote_entries};

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
    /// A listing for the pane is on its way: the site's root while it opens, or the folder a
    /// request is going to. The rows are still those of the folder shown.
    pub loading: bool,
    pub error: Option<String>,
}

/// The cloud pane.
pub(super) struct RemotePane {
    /// The latest request that took effect. An older request is too late (the user has moved
    /// on); the answer to a listing is used only while its request is still this one.
    latest: Ticket,
    /// See [`RemoteView::version`].
    pub(super) version: u64,
    sort: (SortKey, bool),
    /// Folders within the open site.
    history: History<RemotePath>,
    pub(super) open: Option<Remote>,
    /// The listing for request `latest` is on its way.
    loading: bool,
}

impl RemotePane {
    /// A pane with no site open, sorted by `sort`.
    pub(super) fn new(sort: (SortKey, bool)) -> Self {
        Self {
            latest: Ticket(0),
            version: 0,
            sort,
            history: History::default(),
            open: None,
            loading: false,
        }
    }
}

/// The open cloud site and the folder shown from it.
pub(super) struct Remote {
    site: SiteRecord,
    /// `None` while the site opens, and when no client could be made (no password saved, say).
    provider: Option<Arc<dyn StorageProvider>>,
    dir: RemotePath,
    /// In the order the rows show them, so a row index is an index into this list.
    pub(super) entries: Vec<Entry>,
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

impl Session {
    /// Opens the site at `index` in [`Self::sites`] and lists its root.
    ///
    /// The pane switches to the site at once: empty, "Connecting…", loading, with the site's
    /// endpoint and a fresh history. `switched` gets that view, so the window can show it before
    /// the password is read, the client is set up and the listing comes back, all of which can
    /// take a while. Anything still on its way from the site shown before is dropped when it
    /// arrives.
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
            pane.loading = site.is_some();
            self.remote_view_of(&pane)
        };
        let Some(site) = site else {
            return switch;
        };
        switched(switch);
        // No lock is held while the password is read and the client is made.
        let connected = self.connect(&site).await;
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
                    pane.loading = false;
                    pane.version += 1;
                    return self.remote_view_of(&pane);
                }
            }
        };
        let root = RemotePath::root();
        let result = provider.list(&root).await;
        self.finish_listing(ticket, Step::Go, root, result)
    }

    // Each request that moves the cloud pane (or the This PC pane, in `local.rs`) hands `started`
    // the pane as it is once the request has taken effect: loading, still showing the folder it
    // was in. A request that goes nowhere does not call it.

    /// Opens row `row` of the cloud pane if it is a folder. `seen` is the version of the view
    /// the row was on: if the pane has changed since, the row may be another one, and nothing
    /// happens.
    pub async fn open_remote(
        &self,
        ticket: Ticket,
        row: usize,
        seen: u64,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let pick = |pane: &RemotePane| {
            if pane.version != seen {
                return None;
            }
            pane.open
                .as_ref()?
                .entries
                .get(row)
                .filter(|entry| entry.is_folder)
                .map(|entry| entry.path.clone())
        };
        self.navigate_remote(ticket, Step::Go, pick, started).await
    }

    pub async fn remote_up(
        &self,
        ticket: Ticket,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let pick = |pane: &RemotePane| pane.open.as_ref()?.dir.parent();
        self.navigate_remote(ticket, Step::Go, pick, started).await
    }

    /// Goes to the folder of cloud breadcrumb `index` (0 is the zone's root), if the crumbs are
    /// still those of view version `seen`.
    pub async fn remote_crumb(
        &self,
        ticket: Ticket,
        index: usize,
        seen: u64,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let pick = |pane: &RemotePane| {
            if pane.version != seen {
                return None;
            }
            remote_ancestor(&pane.open.as_ref()?.dir, index)
        };
        self.navigate_remote(ticket, Step::Go, pick, started).await
    }

    pub async fn remote_back(
        &self,
        ticket: Ticket,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let pick = |pane: &RemotePane| pane.history.back_to();
        self.navigate_remote(ticket, Step::Back, pick, started)
            .await
    }

    pub async fn remote_forward(
        &self,
        ticket: Ticket,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let pick = |pane: &RemotePane| pane.history.forward_to();
        self.navigate_remote(ticket, Step::Forward, pick, started)
            .await
    }

    /// Lists the folder shown again (F5, Ctrl+R). The history stays as it is. A request still on
    /// its way to another folder is given up, as for any later request.
    pub async fn remote_refresh(
        &self,
        ticket: Ticket,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let pick = |pane: &RemotePane| Some(pane.open.as_ref()?.dir.clone());
        self.navigate_remote(ticket, Step::Go, pick, started).await
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

    /// Lists the folder `pick` chooses in the open site (if it chooses one) and goes there by
    /// `step`. A request older than one that has taken effect is dropped; a request that goes
    /// nowhere takes no effect. One that takes effect makes the pane loading and hands that view
    /// to `started` before the listing is sent.
    pub(super) async fn navigate_remote(
        &self,
        ticket: Ticket,
        step: Step,
        pick: impl FnOnce(&RemotePane) -> Option<RemotePath>,
        started: impl FnOnce(RemoteView) + Send,
    ) -> RemoteView {
        let (provider, dir, loading) = {
            let mut pane = lock(&self.remote);
            if ticket < pane.latest {
                return self.remote_view_of(&pane);
            }
            let provider = pane.open.as_ref().and_then(|r| r.provider.clone());
            let Some((provider, dir)) = provider.zip(pick(&pane)) else {
                return self.remote_view_of(&pane);
            };
            pane.latest = ticket;
            pane.loading = true;
            pane.version += 1;
            (provider, dir, self.remote_view_of(&pane))
        };
        started(loading);
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
                    remote.error = Some(copy::describe(
                        &ProviderError::NotFound,
                        &remote.site.container,
                        &remote.site.host,
                        Attempt::List(&dir),
                    ));
                }
                Err(error) => {
                    remote.info = None;
                    remote.error = Some(copy::describe(
                        &error,
                        &remote.site.container,
                        &remote.site.host,
                        Attempt::List(&dir),
                    ));
                }
            }
            let site = remote.site.id.clone();
            if let Some(left) = left {
                pane.history.record(step, left);
            }
            pane.loading = false;
            pane.version += 1;
            (self.remote_view_of(&pane), site)
        };
        lock(&self.book).healthy.insert(site, healthy);
        view
    }

    pub(super) fn remote_view_of(&self, pane: &RemotePane) -> RemoteView {
        let Some(remote) = &pane.open else {
            return RemoteView {
                version: pane.version,
                ..RemoteView::default()
            };
        };
        let (key, ascending) = pane.sort;
        let site = &remote.site;
        let service = match SiteProvider::of(site) {
            SiteProvider::Bunny => copy::BUNNY_STORAGE,
            SiteProvider::S3(preset) => preset.label(),
        };
        let (connection, status_right) = match (&remote.info, &remote.error) {
            (Some(info), _) => (
                copy::connection(info.latency),
                copy::status_right(&info.http_version, &site.host, None),
            ),
            (None, Some(_)) => (copy::NOT_CONNECTED.to_owned(), String::new()),
            (None, None) => (copy::CONNECTING.to_owned(), site.host.clone()),
        };
        RemoteView {
            version: pane.version,
            rows: rows_from_remote_sorted(&remote.entries, (self.now)(), key, ascending),
            crumbs: remote_crumbs(&site.container, &remote.dir),
            connection,
            connection_ok: remote.info.is_some() || remote.error.is_none(),
            endpoint: copy::endpoint(service, &site.container),
            chip: copy::chip(&site.host),
            status_right,
            can_back: !pane.history.back.is_empty(),
            can_forward: !pane.history.forward.is_empty(),
            loading: pane.loading,
            error: remote.error.clone(),
        }
    }
}
