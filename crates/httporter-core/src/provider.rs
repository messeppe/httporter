use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::stream::Stream;

use crate::{Capabilities, Entry, ProviderError, RemotePath};

/// The S3 object metadata (`x-amz-meta-…`) an upload records its content's SHA-256 under, as hex.
pub const META_SHA256: &str = "httporter-sha256";
/// The S3 object metadata a multipart upload records its part size under, in bytes.
pub const META_PART_SIZE: &str = "httporter-part-size";

/// What [`StorageProvider::stat`] knows about one file: its listing entry and what a listing
/// leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub entry: Entry,
    pub created: Option<DateTime<Utc>>,
    pub content_type: Option<String>,
    /// S3 `x-amz-meta-*` without the prefix, names in lower case.
    pub metadata: Vec<(String, String)>,
}

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

    /// `len` bytes of the body an earlier attempt already stored, which this
    /// one doesn't send (the parts a resumed multipart upload skips): done,
    /// but not sent now. The default does nothing.
    async fn skipped(&self, len: u64) {
        let _ = len;
    }
}

/// How far a multipart upload got: enough to resume it. The journal stores it as JSON.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MultipartProgress {
    pub upload_id: String,
    pub part_size: u64,
    /// The parts stored so far: their 1-based number and ETag.
    pub parts: Vec<(u32, String)>,
    /// The local file as it was when the upload began. The stored parts are reused only while
    /// the file still matches it. `None` in progress written before it was kept, which is never
    /// reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileStamp>,
}

/// A local file's size and last change, to tell whether it changed since.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileStamp {
    pub len: u64,
    /// When the file last changed, in nanoseconds since the Unix epoch, where the system keeps
    /// it.
    pub modified_ns: Option<u64>,
}

impl FileStamp {
    pub fn new(len: u64, modified: Option<std::time::SystemTime>) -> Self {
        let modified_ns = modified
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|since| u64::try_from(since.as_nanos()).ok());
        Self { len, modified_ns }
    }
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
    /// Sent as the object's `Cache-Control`, where the provider stores one (S3).
    pub cache_control: Option<String>,
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
            cache_control: None,
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
            .field("cache_control", &self.cache_control)
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
            && self.cache_control == other.cache_control
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
///   deleting one (`delete_file`, `delete_folder`), except that providers
///   that cannot tell may return `Ok` for a missing path (S3's DELETE
///   answers the same either way);
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

    /// Deletes the file at `path`. A missing file is
    /// [`ProviderError::NotFound`]; providers that cannot tell may return
    /// `Ok` for a missing path.
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

    /// One page of at most `limit` children of `dir` whose names start with
    /// `prefix`: a cheap probe of whether `dir` may be listed (S3
    /// `ListObjectsV2` with `prefix` and `max-keys`). The default lists the
    /// whole folder, so its answer may hold more, and other names.
    async fn list_prefix(
        &self,
        dir: &RemotePath,
        prefix: &str,
        limit: u32,
    ) -> Result<Listing, ProviderError> {
        let _ = (prefix, limit);
        self.list(dir).await
    }

    /// Whether the store answers at all, for telling when a connection is
    /// back. The default lists the root.
    async fn reachable(&self) -> Result<(), ProviderError> {
        self.list(&RemotePath::root()).await.map(|_| ())
    }

    /// Gives up the unfinished multipart upload `upload_id` of the file at
    /// `path`, so the service frees the parts it stored (S3
    /// `AbortMultipartUpload`). For an upload a [`PartJournal`] kept for a
    /// resume that will never come: the user cancelled or discarded it.
    /// Providers without multipart uploads have nothing to give up: the
    /// default does nothing.
    async fn abort_multipart(
        &self,
        path: &RemotePath,
        upload_id: &str,
    ) -> Result<(), ProviderError> {
        let _ = (path, upload_id);
        Ok(())
    }

    /// What is known about the file at `path`, beyond a listing. A missing file is
    /// [`ProviderError::NotFound`]. The default lists the parent folder and finds the file there.
    async fn stat(&self, path: &RemotePath) -> Result<ObjectInfo, ProviderError> {
        let parent = path.parent().ok_or(ProviderError::NotFound)?;
        let listing = self.list(&parent).await?;
        let entry = listing
            .entries
            .into_iter()
            .find(|entry| entry.path == *path)
            .ok_or(ProviderError::NotFound)?;
        Ok(ObjectInfo {
            content_type: entry.content_type.clone(),
            entry,
            created: None,
            metadata: Vec::new(),
        })
    }

    /// Copies the file at `from` to `to` on the server (S3 `CopyObject`). The default: the
    /// provider can't, [`ProviderError::Permanent`].
    async fn copy(&self, from: &RemotePath, to: &RemotePath) -> Result<(), ProviderError> {
        let _ = (from, to);
        Err(ProviderError::Permanent(crate::copy::NO_COPY.to_owned()))
    }

    /// A link that downloads the file at `path` without credentials until `expires` from now.
    /// The default: the provider can't, [`ProviderError::Permanent`].
    fn presign(&self, path: &RemotePath, expires: Duration) -> Result<String, ProviderError> {
        let _ = (path, expires);
        Err(ProviderError::Permanent(crate::copy::NO_PRESIGN.to_owned()))
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
    async fn abort_multipart_defaults_to_nothing_to_give_up() {
        let m = Minimal(MockProvider::new(Capabilities::default()));
        assert_eq!(m.abort_multipart(&p("big.bin"), "u-1").await, Ok(()));
    }

    #[test]
    fn a_file_stamp_keeps_the_size_and_the_exact_time() {
        // Windows keeps file times in steps of 100 ns.
        let time = std::time::UNIX_EPOCH + Duration::new(1_700_000_000, 300);
        let stamp = FileStamp::new(42, Some(time));
        assert_eq!(stamp.len, 42);
        assert_eq!(stamp.modified_ns, Some(1_700_000_000_000_000_300));
        assert_eq!(FileStamp::new(42, None).modified_ns, None);
        assert_ne!(
            FileStamp::new(42, Some(time + Duration::new(0, 100))),
            stamp
        );

        // Progress written before stamps were kept still reads, with none.
        let old = r#"{"upload_id":"u","part_size":5,"parts":[[1,"e"]]}"#;
        let read: MultipartProgress = serde_json::from_str(old).unwrap();
        assert_eq!(read.file, None);
        let with = MultipartProgress {
            file: Some(stamp),
            ..read
        };
        let json = serde_json::to_string(&with).unwrap();
        assert_eq!(
            serde_json::from_str::<MultipartProgress>(&json).unwrap(),
            with
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
