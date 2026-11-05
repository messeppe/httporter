//! Test fakes for the session: a storage factory that hands out mocks (and can hold one back),
//! a storage that holds back the listing of one folder, and a session over an in-memory database.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::ThreadId;

use chrono::{NaiveDate, NaiveDateTime};
use httporter_bunny::{BunnyConfig, ZoneList};
use httporter_core::{
    ByteStream, Capabilities, Listing, MockProvider, ProviderError, RemotePath, StorageProvider,
    TransferSettings, UploadOptions, UploadSource,
};
use httporter_platform::{CredError, CredentialKey, CredentialStore, MemoryStore, Store};
use httporter_s3::S3Config;

use super::{Answer, ProviderFactory, Session, lock};

/// Hands out storages by zone (a default one for every other zone) and remembers the
/// configurations it was given.
pub(crate) struct FakeFactory {
    provider: Arc<dyn StorageProvider>,
    by_zone: Mutex<HashMap<String, Arc<dyn StorageProvider>>>,
    held: Mutex<HashMap<String, Held>>,
    configs: Mutex<Vec<BunnyConfig>>,
    /// The thread each client was made on.
    threads: Mutex<Vec<ThreadId>>,
    /// The configurations of the S3 clients made, and of the bucket lists asked for.
    s3_configs: Mutex<Vec<S3Config>>,
    /// What Choose… and Find Zones answer next (empty lists when not scripted), and the keys
    /// Find Zones was asked with.
    buckets: Mutex<Option<Result<Vec<String>, ProviderError>>>,
    zones: Mutex<Option<Result<ZoneList, ProviderError>>>,
    zone_keys: Mutex<Vec<String>>,
}

/// The factory's side of a [`Hold`].
struct Held {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

/// Holds the next client made for one zone inside `bunny()` (where the real factory reads
/// no password but builds HTTP clients, which takes a while), or the next `set` of a
/// [`SpyStore`], until released.
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
                Err(e) => panic!("the held call was never made: {e:?}"),
            }
        }
    }

    /// Whether the client is being made by now, without waiting (once true, it is not said
    /// again).
    pub(crate) fn has_entered(&self) -> bool {
        self.entered.try_recv().is_ok()
    }

    /// As [`Self::entered`], for a test that is not async: blocks its thread until then.
    pub(crate) fn wait_entered(&self) {
        self.entered
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the held call was never made");
    }

    pub(crate) fn release(&self) {
        self.release.send(()).expect("the call is still waiting");
    }
}

impl FakeFactory {
    pub(crate) fn new(provider: Arc<dyn StorageProvider>) -> Arc<Self> {
        Arc::new(Self {
            provider,
            by_zone: Mutex::new(HashMap::new()),
            held: Mutex::new(HashMap::new()),
            configs: Mutex::new(Vec::new()),
            threads: Mutex::new(Vec::new()),
            s3_configs: Mutex::new(Vec::new()),
            buckets: Mutex::new(None),
            zones: Mutex::new(None),
            zone_keys: Mutex::new(Vec::new()),
        })
    }

    /// Every S3 configuration handed in so far (clients and bucket lists alike).
    pub(crate) fn s3_configs(&self) -> Vec<S3Config> {
        lock(&self.s3_configs).clone()
    }

    /// What the next bucket lists answer.
    pub(crate) fn script_buckets(&self, answer: Result<Vec<String>, ProviderError>) {
        *lock(&self.buckets) = Some(answer);
    }

    /// What the next zone lists answer.
    pub(crate) fn script_zones(&self, answer: Result<ZoneList, ProviderError>) {
        *lock(&self.zones) = Some(answer);
    }

    /// The account keys the zones were asked for with.
    pub(crate) fn zone_keys(&self) -> Vec<String> {
        lock(&self.zone_keys).clone()
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

    /// Every configuration handed in so far, whole.
    pub(crate) fn full_configs(&self) -> Vec<BunnyConfig> {
        lock(&self.configs).clone()
    }

    /// The thread every client made so far was made on.
    pub(crate) fn threads(&self) -> Vec<ThreadId> {
        lock(&self.threads).clone()
    }
}

impl ProviderFactory for FakeFactory {
    fn bunny(&self, cfg: BunnyConfig) -> Result<Arc<dyn StorageProvider>, ProviderError> {
        lock(&self.threads).push(std::thread::current().id());
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

    /// Hands out the storage served for the bucket (the default one otherwise).
    fn s3(&self, cfg: S3Config) -> Result<Arc<dyn StorageProvider>, ProviderError> {
        lock(&self.threads).push(std::thread::current().id());
        let provider = lock(&self.by_zone)
            .get(&cfg.bucket)
            .cloned()
            .unwrap_or_else(|| self.provider.clone());
        lock(&self.s3_configs).push(cfg);
        Ok(provider)
    }

    /// No buckets, unless a test scripted some (`script_buckets`).
    fn s3_buckets(&self, cfg: S3Config) -> Answer<Vec<String>> {
        lock(&self.s3_configs).push(cfg);
        let answer = lock(&self.buckets).clone().unwrap_or(Ok(Vec::new()));
        Box::pin(async move { answer })
    }

    /// No zones, unless a test scripted some (`script_zones`).
    fn bunny_zones(&self, key: String, _settings: &TransferSettings) -> Answer<ZoneList> {
        lock(&self.zone_keys).push(key);
        let answer = lock(&self.zones).clone().unwrap_or(Ok(ZoneList::default()));
        Box::pin(async move { answer })
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
    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.inner.create_folder(dir).await
    }
    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        self.inner.delete_batch(paths).await
    }
    async fn reachable(&self) -> Result<(), ProviderError> {
        self.inner.reachable().await
    }
    async fn abort_multipart(
        &self,
        path: &RemotePath,
        upload_id: &str,
    ) -> Result<(), ProviderError> {
        self.inner.abort_multipart(path, upload_id).await
    }
}

/// A credential store in memory that notes the thread every call was made on, and can hold the
/// next `set` (a slow Windows Credential Manager) until released.
#[derive(Default)]
pub(crate) struct SpyStore {
    inner: MemoryStore,
    threads: Mutex<Vec<ThreadId>>,
    held: Mutex<Option<Held>>,
}

impl SpyStore {
    /// The thread every call so far was made on.
    pub(crate) fn threads(&self) -> Vec<ThreadId> {
        lock(&self.threads).clone()
    }

    /// Holds the next `set` until the returned [`Hold`] releases it. The hold blocks the thread
    /// the call is made on, and gives up after ten seconds if nobody releases it.
    pub(crate) fn hold_set(&self) -> Hold {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *lock(&self.held) = Some(Held {
            entered: entered_tx,
            release: release_rx,
        });
        Hold {
            entered: entered_rx,
            release: release_tx,
        }
    }

    fn note(&self) {
        lock(&self.threads).push(std::thread::current().id());
    }
}

impl CredentialStore for SpyStore {
    fn set(&self, key: &CredentialKey, secret: &str) -> Result<(), CredError> {
        self.note();
        let held = lock(&self.held).take();
        if let Some(held) = held {
            let _ = held.entered.send(());
            let _ = held
                .release
                .recv_timeout(std::time::Duration::from_secs(10));
        }
        self.inner.set(key, secret)
    }

    fn get(&self, key: &CredentialKey) -> Result<Option<String>, CredError> {
        self.note();
        self.inner.get(key)
    }

    fn delete(&self, key: &CredentialKey) -> Result<(), CredError> {
        self.note();
        self.inner.delete(key)
    }

    fn keys(&self) -> Result<Vec<CredentialKey>, CredError> {
        self.note();
        self.inner.keys()
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
