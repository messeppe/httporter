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
//! the user has left, and is dropped. From the moment a request takes effect until its answer is
//! in, the pane says it is loading.
//!
//! A request that takes effect also gives up the cloud listing of the one before
//! ([`until_cancelled`]), which frees the connection it was using. The history moves when a
//! request takes effect, not when its answer is in ([`Course`]): two quick Backs are two steps,
//! F5 asks again for the folder on its way, and a listing that fails leaves the history as it was.
//!
//! Every site has one storage client, kept by the [`SiteHub`] and shared with the transfer engine.
//! Each call on it is a single attempt, so every listing someone waits for (a pane, Test
//! Connection, the test before a site is added) retries a little itself
//! ([`list_interactive`](crate::site_hub::list_interactive)):
//! a dead connection is reported within a minute rather than after twelve.
//!
//! A site's password is read from the credential store only when its client is first built, and
//! goes straight into the client's configuration. It is never kept here, written to the site
//! database, logged or put in a view.
//!
//! Anything that waits on Windows (the credential store, the site database, building an HTTP
//! client) runs on tokio's blocking threads ([`blocking`]), so that a slow call holds up no task.
//!
//! The parts, one file each:
//! - [`sites`]: the saved sites, adding, editing, duplicating and removing one, the Bunny account
//!   key and the S3 bucket list, and opening a site's storage client;
//! - [`remote`]: the cloud pane;
//! - [`local`]: the This PC pane;
//! - [`store`]: the settings kept in the site database;
//! - [`profiles`]: the publish profiles, their history and the CDN links;
//! - `fakes` (tests only): a storage factory and storages that can be held back.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::Poll;

use chrono::NaiveDateTime;
use httporter_bunny::{BunnyAccount, BunnyConfig, BunnyStorage, ZoneList};
use httporter_core::{ProviderError, StorageProvider, TransferSettings};
use httporter_platform::{CredentialStore, SETTINGS_TRANSFERS, Store};
use httporter_s3::{S3Config, S3Storage};
use tokio::sync::oneshot;

use crate::site_hub::SiteHub;
use crate::sort::SortKey;

mod local;
mod places;
mod profiles;
mod remote;
mod sites;
mod store;

pub use local::LocalView;
#[allow(unused_imports)] // T8 uses it.
pub use places::ImportReport;
pub use remote::{RemotePlace, RemoteView};
pub use sites::{SiteItemData, SiteProvider, TestResult, endpoint_preview, hub_error_text};

use local::LocalPane;
use remote::RemotePane;
use sites::Book;

/// What a provider call that is not a storage operation hands back: the account's zones or the
/// buckets a key can see. Boxed, so that [`ProviderFactory`] stays usable as a trait object.
pub type Answer<T> = Pin<Box<dyn Future<Output = Result<T, ProviderError>> + Send>>;

/// Builds the storage client for a site, and makes the two calls the Add Site sheet needs before
/// there is a site: the zones of a Bunny account and the buckets of an S3 key. The app uses
/// [`RealFactory`]; tests hand out mocks.
pub trait ProviderFactory: Send + Sync {
    fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError>;
    /// The client of an S3 site (Ruling Q9).
    fn s3(&self, cfg: S3Config) -> Result<Arc<dyn StorageProvider>, ProviderError>;
    /// The buckets the key in `cfg` can see (its bucket is ignored).
    fn s3_buckets(&self, cfg: S3Config) -> Answer<Vec<String>>;
    /// The storage zones of the Bunny account whose API key is `key`, reached as `settings` say
    /// (their proxy, Ruling Q41).
    fn bunny_zones(&self, key: String, settings: &TransferSettings) -> Answer<ZoneList>;
}

/// Talks to the real Bunny Storage, Bunny account and S3 APIs.
pub struct RealFactory;

impl ProviderFactory for RealFactory {
    fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError> {
        Ok(Arc::new(BunnyStorage::new(cfg)?))
    }

    fn s3(&self, cfg: S3Config) -> Result<Arc<dyn StorageProvider>, ProviderError> {
        Ok(Arc::new(S3Storage::new(cfg)?))
    }

    fn s3_buckets(&self, cfg: S3Config) -> Answer<Vec<String>> {
        // Made here, so that the TLS set-up runs where this is called (a blocking thread).
        let storage = S3Storage::new(cfg);
        Box::pin(async move { storage?.list_buckets().await })
    }

    fn bunny_zones(&self, key: String, settings: &TransferSettings) -> Answer<ZoneList> {
        let account = BunnyAccount::with_settings(key, settings);
        Box::pin(async move { account?.list_zones().await })
    }
}

/// Runs `work` on tokio's blocking threads and returns what it returned. A panic in `work`
/// panics the caller, as it would had `work` run in place. (The pool is only gone when the
/// runtime is shutting down, and then nothing is waiting for the answer any more.)
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match crate::rt::spawn_blocking(work).await {
        Ok(value) => value,
        Err(e) => match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => std::future::pending().await,
        },
    }
}

/// A place in line for moving one pane, taken on the UI thread at the moment the user asks
/// ([`Session::remote_ticket`], [`Session::local_ticket`]). Requests are thus ordered as the user
/// made them, not as their tasks happen to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ticket(u64);

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
    /// To the place already shown, or already on its way: the history stays as it is.
    Refresh,
}

/// The places a pane can go back and forward to, Finder-style. The last entry is the nearest.
#[derive(Clone, Debug)]
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
            Step::Refresh => {}
        }
    }
}

/// A request that has taken effect and whose listing is on its way.
#[derive(Clone, Debug)]
struct Pending<T> {
    step: Step,
    /// The place it is going to.
    at: T,
    /// The history as it will be once the listing is in, counted from `at`.
    history: History<T>,
}

/// A pane's history, and the request on its way, if any.
///
/// The history is moved when a request takes effect, not when its listing comes in: a second Back
/// pressed while the first one is still being listed goes one step further, from where the first
/// is going, rather than to the same place again. If the listing fails, the history is as it was
/// (a Back to a folder that is gone can be tried again, or Forward taken).
#[derive(Clone, Debug)]
struct Course<T> {
    /// Counted from the place shown.
    history: History<T>,
    pending: Option<Pending<T>>,
}

impl<T> Default for Course<T> {
    fn default() -> Self {
        Self {
            history: History::default(),
            pending: None,
        }
    }
}

impl<T: Clone + PartialEq> Course<T> {
    /// The request on its way, if it is a Back or a Forward, which the next one carries on from.
    fn travelling(&self) -> Option<&Pending<T>> {
        self.pending
            .as_ref()
            .filter(|p| matches!(p.step, Step::Back | Step::Forward))
    }

    /// The history a Back or Forward takes its place from: where the Back or Forward on its way
    /// is going, or else the place shown.
    fn ahead(&self) -> &History<T> {
        self.travelling().map_or(&self.history, |p| &p.history)
    }

    /// Where a Refresh goes: the place on its way, or else `shown`.
    fn refresh_target(&self, shown: &T) -> T {
        self.pending
            .as_ref()
            .map_or_else(|| shown.clone(), |p| p.at.clone())
    }

    /// A request to go to `target` by `step` has taken effect, with `shown` on show.
    fn begin(&mut self, step: Step, shown: &T, target: T) {
        if step == Step::Refresh {
            // It asks again for what is on its way, which has moved the history already.
            match &mut self.pending {
                Some(pending) => pending.at = target,
                None => {
                    self.pending = Some(Pending {
                        step,
                        at: target,
                        history: self.history.clone(),
                    });
                }
            }
            return;
        }
        let along = matches!(step, Step::Back | Step::Forward);
        let (from, mut history) = match self.travelling() {
            Some(p) if along => (p.at.clone(), p.history.clone()),
            _ => (shown.clone(), self.history.clone()),
        };
        // A Back or Forward always moves along its list; a new place only if it is another one.
        if along || target != from {
            history.record(step, from);
        }
        self.pending = Some(Pending {
            step,
            at: target,
            history,
        });
    }

    /// The listing of the request on its way is in: the pane is there.
    fn arrive(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.history = pending.history;
        }
    }

    /// The listing failed: the pane stays where it was, with the history it had.
    fn fail(&mut self) {
        self.pending = None;
    }
}

/// Runs `work` until `cancel` goes off (it is sent to, or its sender is dropped), and returns
/// what `work` returned, or `None` if `cancel` went off first. Then `work` is dropped, which
/// gives up the request it was making and frees its connection.
pub(crate) async fn until_cancelled<T>(
    work: impl Future<Output = T>,
    cancel: oneshot::Receiver<()>,
) -> Option<T> {
    let mut work = std::pin::pin!(work);
    let mut cancel = cancel;
    std::future::poll_fn(move |cx| {
        // Cancelled already: `work` is not even started.
        if Pin::new(&mut cancel).poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        work.as_mut().poll(cx).map(Some)
    })
    .await
}

/// The app's state. It is `Send` and `Sync`: the app shares one `Arc<Session>` between the UI
/// thread and the tasks that list folders.
pub struct Session {
    creds: Arc<dyn CredentialStore>,
    /// Every site's storage client, shared with the transfer engine.
    hub: Arc<SiteHub>,
    /// Local time, for the "Today 10:02" dates in the rows.
    now: fn() -> NaiveDateTime,
    /// The site database. Its own lock, so that a slow disk write holds up only other writes.
    /// Shared, so that a write can run on a blocking thread.
    store: Arc<Mutex<Store>>,
    /// Transfers settings waiting to be written (see [`Session::save_settings`]).
    pending_settings: Arc<Mutex<Option<TransferSettings>>>,
    /// Shared, so that a save can update it from the blocking thread that writes the site.
    book: Arc<Mutex<Book>>,
    /// The UI state and the site extras as last read or saved (`places`).
    places: Arc<Mutex<places::PlacesCache>>,
    remote: Mutex<RemotePane>,
    local: Mutex<LocalPane>,
    /// The last ticket given out for each pane.
    remote_tickets: AtomicU64,
    local_tickets: AtomicU64,
}

impl Session {
    /// A session over the sites in `store`, with the This PC pane at the list of drives and no
    /// site open. The sites' clients are built with the Transfers settings stored in `store`.
    pub fn new(
        store: Store,
        creds: Arc<dyn CredentialStore>,
        factory: Arc<dyn ProviderFactory>,
        now: fn() -> NaiveDateTime,
    ) -> Self {
        let sort = (SortKey::Name, true);
        // The defaults when nothing (readable) is stored; nothing is written back.
        let settings: TransferSettings = store.settings(SETTINGS_TRANSFERS);
        let hub = Arc::new(SiteHub::new(creds.clone(), factory, settings));
        Self {
            creds,
            hub,
            now,
            book: Arc::new(Mutex::new(Book::read(&store))),
            store: Arc::new(Mutex::new(store)),
            pending_settings: Arc::new(Mutex::new(None)),
            places: Arc::default(),
            remote: Mutex::new(RemotePane::new(sort)),
            local: Mutex::new(LocalPane::new(sort)),
            remote_tickets: AtomicU64::new(0),
            local_tickets: AtomicU64::new(0),
        }
    }

    /// The sites' storage clients, which the transfer engine shares.
    pub fn hub(&self) -> &Arc<SiteHub> {
        &self.hub
    }

    /// The credential store the secrets are in, for Settings › Security (I5), which counts and
    /// removes HTTPorter's own keys there.
    pub fn creds(&self) -> Arc<dyn CredentialStore> {
        self.creds.clone()
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
}

#[cfg(test)]
pub(crate) mod fakes;

#[cfg(test)]
#[path = "../session_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../session_browse_tests.rs"]
mod browse_tests;
