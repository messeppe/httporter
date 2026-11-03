//! One storage client per saved site, shared by browsing and transfers.
//!
//! A client is built the first time a site needs one, from the site's record, its secret in the
//! credential store and the Transfers settings ([`BunnyConfig::for_site`]): a single attempt per
//! call, so whoever makes a request owns its retries. Browsing wraps its listings in
//! [`list_interactive`] (a short policy, someone is waiting); the transfer engine uses its own,
//! longer policy. New settings drop every client, and each is rebuilt when next asked for.
//!
//! Building a client reads the credential store and sets up TLS, which can take a while:
//! [`SiteHub::provider`] blocks, so callers on the runtime run it on a blocking thread.
//!
//! The secret goes from the credential store straight into the client's configuration. It is
//! never kept here, logged or shown, and [`HubError`] carries none of it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use httporter_bunny::BunnyConfig;
use httporter_core::{
    ByteStream, Capabilities, Listing, ProviderError, ProviderKind, RemotePath, RetryPolicy,
    SiteId, SiteRecord, StorageProvider, TransferSettings, UploadOptions, UploadSource, with_retry,
};
use httporter_platform::{CredentialKey, CredentialStore, SiteSecret};

use crate::copy;
use crate::session::ProviderFactory;

/// Why a site has no client.
#[derive(Debug, Clone, PartialEq)]
pub enum HubError {
    /// No secret is saved for the site.
    MissingSecret,
    /// The credential store would not give the secret, or what it gave is not in the expected
    /// form. The text names no part of the secret.
    Credential(String),
    /// The client could not be built.
    Provider(ProviderError),
}

/// What the hub keeps: the settings clients are built with, and the clients built so far.
struct Clients {
    settings: TransferSettings,
    by_site: HashMap<SiteId, Arc<dyn StorageProvider>>,
}

/// The storage clients of the saved sites, one per site.
pub struct SiteHub {
    creds: Arc<dyn CredentialStore>,
    factory: Arc<dyn ProviderFactory>,
    clients: Mutex<Clients>,
}

impl SiteHub {
    pub fn new(
        creds: Arc<dyn CredentialStore>,
        factory: Arc<dyn ProviderFactory>,
        settings: TransferSettings,
    ) -> Self {
        Self {
            creds,
            factory,
            clients: Mutex::new(Clients {
                settings,
                by_site: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Clients> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The settings clients are built with now.
    pub fn settings(&self) -> TransferSettings {
        self.lock().settings.clone()
    }

    /// The factory clients are built with (Test Connection builds one for a site not yet saved).
    pub fn factory(&self) -> Arc<dyn ProviderFactory> {
        self.factory.clone()
    }

    /// The client for `site`: the one built before, or a new one. Blocks while a new one is
    /// built (the credential store, TLS set-up). Two callers asking for the same new site at once
    /// may both build one; both get the one kept.
    pub fn provider(&self, site: &SiteRecord) -> Result<Arc<dyn StorageProvider>, HubError> {
        let settings = {
            let clients = self.lock();
            if let Some(client) = clients.by_site.get(&site.id) {
                return Ok(client.clone());
            }
            clients.settings.clone()
        };
        // No lock is held while the secret is read and the client is built.
        let client = self.build(site, &settings)?;
        let mut clients = self.lock();
        if clients.settings != settings {
            // The settings changed meanwhile: this client is already out of date, so it is
            // handed out once and not kept.
            return Ok(client);
        }
        Ok(clients
            .by_site
            .entry(site.id.clone())
            .or_insert(client)
            .clone())
    }

    /// Whether `client` is the one the hub would hand out for `site` now (false once new
    /// settings or an edit of the site have dropped it).
    pub fn is_current(&self, site: &SiteId, client: &Arc<dyn StorageProvider>) -> bool {
        self.lock()
            .by_site
            .get(site)
            .is_some_and(|kept| Arc::ptr_eq(kept, client))
    }

    /// Drops the client of a site whose record or secret changed; the next request builds a new
    /// one.
    #[allow(dead_code)] // Editing and removing a site arrive with Task 24.
    pub fn invalidate(&self, id: &SiteId) {
        self.lock().by_site.remove(id);
    }

    /// New Transfers settings: every client is dropped, and rebuilt with them when next asked
    /// for.
    pub fn set_settings(&self, settings: TransferSettings) {
        let mut clients = self.lock();
        if clients.settings != settings {
            clients.settings = settings;
            clients.by_site.clear();
        }
    }

    fn build(
        &self,
        site: &SiteRecord,
        settings: &TransferSettings,
    ) -> Result<Arc<dyn StorageProvider>, HubError> {
        let stored = match self.creds.get(&CredentialKey::site(&site.id)) {
            Ok(Some(stored)) => stored,
            Ok(None) => return Err(HubError::MissingSecret),
            Err(e) => return Err(HubError::Credential(e.to_string())),
        };
        let secret = SiteSecret::from_stored(site.provider, &stored)
            .map_err(|e| HubError::Credential(e.to_string()))?;
        match (site.provider, secret) {
            (ProviderKind::Bunny, SiteSecret::BunnyPassword(password)) => self
                .factory
                .bunny(BunnyConfig::for_site(
                    &site.container,
                    &site.host,
                    password,
                    settings,
                ))
                .map_err(HubError::Provider),
            // S3 sites are opened from Task 24 on (Ruling Q9).
            _ => Err(HubError::Provider(ProviderError::Refused(
                copy::S3_LATER.to_owned(),
            ))),
        }
    }
}

/// Lists `dir` for someone who is waiting: a few quick retries
/// ([`RetryPolicy::interactive`]), and no waiting out a long `Retry-After`.
pub async fn list_interactive(
    provider: &dyn StorageProvider,
    dir: &RemotePath,
) -> Result<Listing, ProviderError> {
    with_retry(&RetryPolicy::interactive(), |_| provider.list(dir)).await
}

/// A site's client whose listings retry as browsing does ([`list_interactive`]). The planners
/// list the folders a transfer touches through it, so a passing failure does not stop a plan;
/// everything else goes to the client unchanged.
pub struct Interactive(pub Arc<dyn StorageProvider>);

#[async_trait]
impl StorageProvider for Interactive {
    fn capabilities(&self) -> &Capabilities {
        self.0.capabilities()
    }

    fn endpoint(&self) -> String {
        self.0.endpoint()
    }

    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        list_interactive(&*self.0, dir).await
    }

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        self.0.upload(path, source, opts).await
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        self.0.download(path).await
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        self.0.delete_file(path).await
    }

    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.0.delete_folder(dir).await
    }

    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        self.0.create_folder(dir).await
    }

    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        self.0.delete_batch(paths).await
    }

    async fn reachable(&self) -> Result<(), ProviderError> {
        self.0.reachable().await
    }
}
