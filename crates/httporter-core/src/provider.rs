use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::Stream;

use crate::{Capabilities, Entry, ProviderError, RemotePath};

/// A download body, delivered in chunks as it arrives.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

/// Where an upload's bytes come from.
#[derive(Debug, Clone)]
pub enum UploadSource {
    Bytes(Bytes),
    /// A local file of `len` bytes.
    File {
        path: PathBuf,
        len: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadOptions {
    pub content_type: String,
    /// When set, the provider must reject the upload unless the content
    /// hashes to this SHA-256.
    pub sha256: Option<[u8; 32]>,
}

/// What the request that produced a result looked like on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestInfo {
    pub latency: Duration,
    pub http_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub info: RequestInfo,
}

/// A remote store the app can browse and transfer to and from.
#[async_trait]
pub trait StorageProvider: Send + Sync {
    fn capabilities(&self) -> &Capabilities;

    /// A human-readable address of the store, for display.
    fn endpoint(&self) -> String;

    /// The direct children of `dir` (not the whole subtree).
    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError>;

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError>;

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError>;

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError>;

    /// Deletes `dir` and everything under it.
    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError>;
}
