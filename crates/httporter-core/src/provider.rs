use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
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
    /// A local file of `len` bytes. A file that turns out to have another
    /// length has changed since it was looked at, and the upload fails.
    File {
        path: PathBuf,
        len: u64,
    },
}

/// Told about an upload body piece by piece, for progress and bandwidth
/// limits.
#[async_trait]
pub trait BodyMeter: Send + Sync {
    /// The next `len` bytes of the body are about to be sent. The upload
    /// waits for this to return, so a bandwidth limit can hold it back.
    async fn chunk(&self, len: usize);
}

/// How far a multipart upload got: enough to resume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartProgress {
    pub upload_id: String,
    pub part_size: u64,
    /// The parts stored so far: their 1-based number and ETag.
    pub parts: Vec<(u32, String)>,
}

/// Where a provider keeps a multipart upload's progress, so an interrupted
/// upload resumes instead of starting over.
pub trait PartJournal: Send + Sync {
    /// The progress of an earlier attempt at this upload, if there was one.
    fn load(&self) -> Option<MultipartProgress>;
    /// A new multipart upload began.
    fn started(&self, upload_id: &str, part_size: u64);
    /// Part `number` (1-based) is stored, with this ETag.
    fn part_done(&self, number: u32, etag: &str);
    /// The upload completed; its progress can be forgotten.
    fn finished(&self);
    /// The upload was cancelled: abort it on the server rather than keep it
    /// for a resume.
    fn cancelled(&self) -> bool;
}

/// How to upload a file, beyond its bytes.
#[derive(Clone)]
pub struct UploadOptions {
    pub content_type: String,
    /// When set, the provider must reject the upload unless the content
    /// hashes to this SHA-256.
    pub sha256: Option<[u8; 32]>,
    /// When set, the provider sends it for the server to check (S3
    /// `Content-MD5`), where the protocol allows.
    pub md5: Option<[u8; 16]>,
    /// Told about every piece of the body before it is sent.
    pub meter: Option<Arc<dyn BodyMeter>>,
    /// Keeps a multipart upload's progress, for providers that use them.
    pub parts: Option<Arc<dyn PartJournal>>,
}

impl UploadOptions {
    /// Options with only the content type set.
    pub fn new(content_type: impl Into<String>) -> Self {
        Self {
            content_type: content_type.into(),
            sha256: None,
            md5: None,
            meter: None,
            parts: None,
        }
    }
}

impl fmt::Debug for UploadOptions {
    /// Says only whether a meter and a part journal are set.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let set = |is_set: bool| if is_set { "set" } else { "none" };
        f.debug_struct("UploadOptions")
            .field("content_type", &self.content_type)
            .field("sha256", &self.sha256)
            .field("md5", &self.md5)
            .field("meter", &format_args!("{}", set(self.meter.is_some())))
            .field("parts", &format_args!("{}", set(self.parts.is_some())))
            .finish()
    }
}

impl PartialEq for UploadOptions {
    /// The meter and the part journal are equal only when they are the same
    /// object.
    fn eq(&self, other: &Self) -> bool {
        fn same<T: ?Sized>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
        }
        self.content_type == other.content_type
            && self.sha256 == other.sha256
            && self.md5 == other.md5
            && same(&self.meter, &other.meter)
            && same(&self.parts, &other.parts)
    }
}

impl Eq for UploadOptions {}

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
///
/// Every implementation reports failures the same way:
/// - a missing folder or file is [`ProviderError::NotFound`], and so is
///   deleting one (`delete_file`, `delete_folder`);
/// - a write that collides with what is there, such as a file uploaded onto
///   a folder or below another file, is [`ProviderError::Refused`];
/// - an upload to an existing file overwrites it;
/// - a server's Retry-After is clamped to [`crate::MAX_RETRY_AFTER`] before
///   it is returned in [`ProviderError::RateLimited`] (spec: honoured up to
///   300 s).
///
/// A wrapper around another provider forwards every method, those with
/// defaults included, or the inner provider's own versions are bypassed.
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

    /// Deletes `dir` and everything under it. Without
    /// [`Capabilities::recursive_delete`] a provider may refuse a folder that
    /// still has something in it.
    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError>;

    /// Makes an empty folder at `dir`; one that is already there is fine.
    /// A provider that cannot keep empty folders (see
    /// [`Capabilities::empty_folders`]) refuses.
    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError>;

    /// Deletes the files at `paths`, answering for each, in order. Providers
    /// with [`Capabilities::batch_delete_max`] above 1 do it in fewer
    /// requests; the default calls [`Self::delete_file`] once per path.
    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        let mut results = Vec::with_capacity(paths.len());
        for path in paths {
            results.push(self.delete_file(path).await);
        }
        results
    }

    /// Whether the store answers at all, for telling when a connection is
    /// back. The default lists the root.
    async fn reachable(&self) -> Result<(), ProviderError> {
        self.list(&RemotePath::root()).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthHint, MockOp, MockProvider};

    struct Meter;

    #[async_trait]
    impl BodyMeter for Meter {
        async fn chunk(&self, _len: usize) {}
    }

    struct Parts;

    impl PartJournal for Parts {
        fn load(&self) -> Option<MultipartProgress> {
            None
        }
        fn started(&self, _upload_id: &str, _part_size: u64) {}
        fn part_done(&self, _number: u32, _etag: &str) {}
        fn finished(&self) {}
        fn cancelled(&self) -> bool {
            false
        }
    }

    #[test]
    fn upload_options_new_sets_only_the_content_type() {
        let opts = UploadOptions::new("text/html");
        assert_eq!(opts.content_type, "text/html");
        assert_eq!(opts.sha256, None);
        assert_eq!(opts.md5, None);
        assert!(opts.meter.is_none());
        assert!(opts.parts.is_none());
    }

    #[test]
    fn upload_options_debug_says_whether_meter_and_parts_are_set() {
        let plain = format!("{:?}", UploadOptions::new("text/plain"));
        assert!(plain.contains("meter: none"), "{plain}");
        assert!(plain.contains("parts: none"), "{plain}");
        let full = UploadOptions {
            meter: Some(Arc::new(Meter)),
            parts: Some(Arc::new(Parts)),
            ..UploadOptions::new("text/plain")
        };
        let full = format!("{full:?}");
        assert!(full.contains("meter: set"), "{full}");
        assert!(full.contains("parts: set"), "{full}");
        assert!(full.contains("content_type: \"text/plain\""), "{full}");
    }

    #[test]
    fn upload_options_compare_meter_and_parts_by_identity() {
        let meter: Arc<dyn BodyMeter> = Arc::new(Meter);
        let parts: Arc<dyn PartJournal> = Arc::new(Parts);
        let with = |meter: &Arc<dyn BodyMeter>, parts: &Arc<dyn PartJournal>| UploadOptions {
            meter: Some(meter.clone()),
            parts: Some(parts.clone()),
            md5: Some([7; 16]),
            ..UploadOptions::new("text/plain")
        };
        let other_meter: Arc<dyn BodyMeter> = Arc::new(Meter);
        let other_parts: Arc<dyn PartJournal> = Arc::new(Parts);
        assert_eq!(with(&meter, &parts), with(&meter, &parts));
        assert_ne!(with(&meter, &parts), with(&other_meter, &parts));
        assert_ne!(with(&meter, &parts), with(&meter, &other_parts));
        assert_ne!(
            with(&meter, &parts),
            UploadOptions {
                meter: None,
                ..with(&meter, &parts)
            }
        );
        assert_ne!(
            with(&meter, &parts),
            UploadOptions {
                md5: None,
                ..with(&meter, &parts)
            }
        );
        assert_eq!(UploadOptions::new("a"), UploadOptions::new("a"));
    }

    /// Implements only what a provider must, so the trait's defaults are what run.
    struct Minimal(MockProvider);

    #[async_trait]
    impl StorageProvider for Minimal {
        fn capabilities(&self) -> &Capabilities {
            self.0.capabilities()
        }
        fn endpoint(&self) -> String {
            self.0.endpoint()
        }
        async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
            self.0.list(dir).await
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
    }

    fn p(path: &str) -> RemotePath {
        RemotePath::parse(path).unwrap()
    }

    #[tokio::test]
    async fn delete_batch_defaults_to_one_delete_file_per_path_in_order() {
        let m = Minimal(
            MockProvider::new(Capabilities::default())
                .with_file("a.txt", b"1")
                .with_file("b.txt", b"2"),
        );
        let results = m
            .delete_batch(&[p("b.txt"), p("gone.txt"), p("a.txt")])
            .await;
        assert_eq!(results, [Ok(()), Err(ProviderError::NotFound), Ok(())]);
        assert_eq!(m.0.calls(MockOp::DeleteFile), 3);
        assert_eq!(m.0.calls(MockOp::DeleteBatch), 0);
        assert!(
            m.0.list(&RemotePath::root())
                .await
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reachable_defaults_to_listing_the_root() {
        let m = Minimal(MockProvider::new(Capabilities::default()));
        m.reachable().await.unwrap();
        let auth = ProviderError::Auth {
            hint: AuthHint::Unknown,
        };
        m.0.fail_next(MockOp::List, auth.clone(), 1);
        assert_eq!(m.reachable().await, Err(auth));
        assert_eq!(m.0.calls(MockOp::List), 2);
        assert_eq!(m.0.calls(MockOp::Reachable), 0);
    }
}
