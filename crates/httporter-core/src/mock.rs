use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::stream;
use md5::Md5;
use sha2::{Digest, Sha256};

use crate::{
    ByteStream, Capabilities, Checksum, ChecksumKind, EmptyFolders, Entry, Listing, ProviderError,
    RemotePath, RequestInfo, StorageProvider, UploadOptions, UploadSource,
};

/// The size of the pieces an upload body is metered in.
const METER_CHUNK: usize = 64 * 1024;

/// A [`StorageProvider`] operation, for counting calls, injecting failures
/// and adding latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MockOp {
    List,
    Upload,
    Download,
    DeleteFile,
    DeleteFolder,
    CreateFolder,
    DeleteBatch,
    Reachable,
}

struct File {
    data: Bytes,
    modified: DateTime<Utc>,
    content_type: Option<String>,
    /// Listed instead of a digest, as S3 lists a file uploaded in parts.
    etag: Option<String>,
}

enum Node {
    File(File),
    /// An explicitly created folder. Folders are otherwise implied by the file
    /// paths beneath them.
    Folder,
}

/// The files and folders. Never contains the root. A path is a file or a
/// folder, never both: `put_file` / `put_folder` refuse writes that would
/// break that.
#[derive(Default)]
struct State {
    nodes: BTreeMap<RemotePath, Node>,
}

/// What happened to the calls, and what is to happen to the next ones.
#[derive(Default)]
struct Calls {
    made: HashMap<MockOp, usize>,
    failures: HashMap<MockOp, VecDeque<ProviderError>>,
    latency: HashMap<MockOp, Duration>,
    in_flight: HashMap<MockOp, usize>,
    max_in_flight: HashMap<MockOp, usize>,
}

impl Calls {
    /// Counts a call of `op` as made and in flight. Returns how long it is
    /// to take and the error it is to fail with, if one is queued.
    fn begin(&mut self, op: MockOp) -> (Duration, Option<ProviderError>) {
        *self.made.entry(op).or_default() += 1;
        let in_flight = self.in_flight.entry(op).or_default();
        *in_flight += 1;
        let in_flight = *in_flight;
        let max = self.max_in_flight.entry(op).or_default();
        *max = (*max).max(in_flight);
        let latency = self.latency.get(&op).copied().unwrap_or_default();
        (latency, self.next_failure(op))
    }

    /// The next error queued for `op`, if any, taken off the queue.
    fn next_failure(&mut self, op: MockOp) -> Option<ProviderError> {
        self.failures.get_mut(&op).and_then(VecDeque::pop_front)
    }
}

/// A call in progress, counted as in flight until it is dropped: when the
/// call returns, or when it is given up part way.
struct InFlight<'a> {
    calls: &'a Mutex<Calls>,
    op: MockOp,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if let Some(in_flight) = lock(self.calls).in_flight.get_mut(&self.op) {
            *in_flight -= 1;
        }
    }
}

/// Locks `mutex`. Nothing is ever left half-updated under these locks, so a
/// poisoned one is safe to use.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl State {
    /// Everything strictly beneath `dir`, in map order.
    ///
    /// This is a full scan on purpose: `RemotePath` orders as plain strings,
    /// so `a-b` sorts before `a/b` and a folder's descendants are not
    /// contiguous in the map. A range scan would be wrong.
    fn beneath(&self, dir: &RemotePath) -> impl Iterator<Item = (&RemotePath, &Node)> {
        self.nodes
            .iter()
            .filter(move |(path, _)| *path != dir && path.is_within(dir))
    }

    fn is_folder(&self, path: &RemotePath) -> bool {
        if path.is_root() {
            return true;
        }
        match self.nodes.get(path) {
            Some(Node::Folder) => true,
            Some(Node::File(_)) => false,
            None => self.beneath(path).next().is_some(),
        }
    }

    /// A file at one of `path`'s ancestors would make `path` unreachable.
    fn has_file_above(&self, path: &RemotePath) -> bool {
        std::iter::successors(path.parent(), RemotePath::parent)
            .any(|ancestor| matches!(self.nodes.get(&ancestor), Some(Node::File(_))))
    }

    fn put_file(
        &mut self,
        path: &RemotePath,
        data: Bytes,
        content_type: Option<String>,
    ) -> Result<(), ProviderError> {
        if path.is_root() {
            return Err(ProviderError::Refused("the root is not a file".into()));
        }
        if self.is_folder(path) {
            return Err(ProviderError::Refused(format!(
                "{} is a folder",
                path.as_str()
            )));
        }
        if self.has_file_above(path) {
            return Err(ProviderError::Refused(format!(
                "a parent of {} is a file",
                path.as_str()
            )));
        }
        self.nodes.insert(
            path.clone(),
            Node::File(File {
                data,
                modified: Utc::now(),
                content_type,
                etag: None,
            }),
        );
        Ok(())
    }

    fn put_folder(&mut self, path: &RemotePath) -> Result<(), ProviderError> {
        if path.is_root() {
            return Ok(());
        }
        if matches!(self.nodes.get(path), Some(Node::File(_))) || self.has_file_above(path) {
            return Err(ProviderError::Refused(format!(
                "{} collides with a file",
                path.as_str()
            )));
        }
        self.nodes.entry(path.clone()).or_insert(Node::Folder);
        Ok(())
    }

    /// Removes the file at `path`; anything else there is `NotFound`.
    fn delete_file(&mut self, path: &RemotePath) -> Result<(), ProviderError> {
        if matches!(self.nodes.get(path), Some(Node::File(_))) {
            self.nodes.remove(path);
            Ok(())
        } else {
            Err(ProviderError::NotFound)
        }
    }

    /// The direct children of `dir`, sorted by name, with files carrying
    /// `checksums`. A folder implied by several deeper paths is listed once.
    fn children(&self, dir: &RemotePath, checksums: Option<ChecksumKind>) -> Vec<Entry> {
        let prefix_len = if dir.is_root() {
            0
        } else {
            dir.as_str().len() + 1
        };
        let mut by_name: BTreeMap<&str, Entry> = BTreeMap::new();
        for (path, node) in self.beneath(dir) {
            let rest = &path.as_str()[prefix_len..];
            let (name, file) = match rest.split_once('/') {
                Some((name, _)) => (name, None),
                None => match node {
                    Node::File(file) => (rest, Some(file)),
                    Node::Folder => (rest, None),
                },
            };
            by_name
                .entry(name)
                .or_insert_with(|| child_entry(dir, name, file, checksums));
        }
        by_name.into_values().collect()
    }
}

fn child_entry(
    dir: &RemotePath,
    name: &str,
    file: Option<&File>,
    checksums: Option<ChecksumKind>,
) -> Entry {
    let path = dir
        .join(name)
        .expect("a segment of a valid path is a valid segment");
    match file {
        Some(file) => Entry {
            path,
            is_folder: false,
            size: file.data.len() as u64,
            modified: Some(file.modified),
            checksum: listed_checksum(file, checksums),
            content_type: file.content_type.clone(),
            replicated_regions: Vec::new(),
        },
        None => Entry {
            path,
            is_folder: true,
            size: 0,
            modified: None,
            checksum: None,
            content_type: None,
            replicated_regions: Vec::new(),
        },
    }
}

/// What a listing shows as `file`'s checksum: its multipart ETag if it was
/// given one, otherwise the digest of its content that `kind` names.
fn listed_checksum(file: &File, kind: Option<ChecksumKind>) -> Option<Checksum> {
    if let Some(etag) = &file.etag {
        return Some(Checksum {
            kind: ChecksumKind::MultipartEtag,
            hex: etag.clone(),
        });
    }
    let kind = kind?;
    let hex = match kind {
        ChecksumKind::Sha256 => hex::encode(sha256(&file.data)),
        ChecksumKind::Md5 => hex::encode(md5(&file.data)),
        // Only a file given one with `with_file_etag` lists a multipart ETag.
        ChecksumKind::MultipartEtag => return None,
    };
    Some(Checksum { kind, hex })
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn md5(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

/// The bytes of an upload. A file must still be the length the caller gave.
fn read_source(source: UploadSource) -> Result<Bytes, ProviderError> {
    match source {
        UploadSource::Bytes(bytes) => Ok(bytes),
        UploadSource::File { path, len } => {
            let data = std::fs::read(&path)
                .map_err(|e| ProviderError::Permanent(format!("{}: {e}", path.display())))?;
            if data.len() as u64 != len {
                return Err(ProviderError::Permanent(format!(
                    "{} is {} bytes, not the {len} expected",
                    path.display(),
                    data.len()
                )));
            }
            Ok(Bytes::from(data))
        }
    }
}

/// An in-memory [`StorageProvider`] for tests of the UI and the engine.
///
/// Folders are implied by the files beneath them; `with_folder` makes an
/// empty one. It follows the trait's error rules (a missing path is
/// `NotFound`, a file/folder collision `Refused`, an upload overwrites) and
/// its own capabilities:
/// - listings carry the checksum `checksum_in_listing` names (the SHA-256 or
///   MD5 of the content, or none); a file given one with
///   [`with_file_etag`](Self::with_file_etag) lists that multipart ETag;
/// - without `recursive_delete`, `delete_folder` refuses a folder with
///   anything in it;
/// - `create_folder` refuses unless `empty_folders` is `Markers` or
///   `Supported`;
/// - `delete_batch` refuses more than `batch_delete_max` paths.
///
/// A file source must be the length it claims, an upload with `sha256` or
/// `md5` must match it, and an upload's meter is told about its body 64 KiB
/// at a time.
///
/// Every call is counted and tracked while in flight. [`set_latency`](Self::set_latency)
/// makes calls take (virtual) time, and [`fail_next`](Self::fail_next) makes
/// upcoming calls fail before they touch any state.
pub struct MockProvider {
    capabilities: Capabilities,
    state: Mutex<State>,
    calls: Mutex<Calls>,
}

impl MockProvider {
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            state: Mutex::new(State::default()),
            calls: Mutex::new(Calls::default()),
        }
    }

    /// Adds (or replaces) a file. Panics if `path` is not a valid, writable
    /// path: this is test setup, not an operation under test.
    pub fn with_file(self, path: &str, data: &[u8]) -> Self {
        let path = RemotePath::parse(path).expect("with_file: invalid path");
        lock(&self.state)
            .put_file(&path, Bytes::copy_from_slice(data), None)
            .expect("with_file: path collides with an existing file or folder");
        self
    }

    /// Makes the file at `path` list a multipart ETag (`etag`, as given)
    /// instead of a digest, as S3 lists a file uploaded in parts. An upload
    /// to the path later replaces it with an ordinary file. Panics if there
    /// is no file at `path`.
    pub fn with_file_etag(self, path: &str, etag: &str) -> Self {
        let path = RemotePath::parse(path).expect("with_file_etag: invalid path");
        match lock(&self.state).nodes.get_mut(&path) {
            Some(Node::File(file)) => file.etag = Some(etag.to_owned()),
            _ => panic!("with_file_etag: no file at {}", path.as_str()),
        }
        self
    }

    /// Adds an explicit (possibly empty) folder, whatever the capabilities
    /// say. Panics like [`with_file`](Self::with_file).
    pub fn with_folder(self, path: &str) -> Self {
        let path = RemotePath::parse(path).expect("with_folder: invalid path");
        lock(&self.state)
            .put_folder(&path)
            .expect("with_folder: path collides with an existing file");
        self
    }

    /// Makes the next `times` calls of `op` fail with `error`, one error per
    /// call, after being counted. Queues behind errors already pending for `op`.
    /// A `delete_batch` also takes queued `DeleteFile` errors, one per path, so
    /// single paths of a batch can fail.
    pub fn fail_next(&self, op: MockOp, error: ProviderError, times: usize) {
        lock(&self.calls)
            .failures
            .entry(op)
            .or_default()
            .extend(std::iter::repeat_n(error, times));
    }

    /// Makes every later call of `op` take `latency` (a tokio sleep, so a
    /// paused test clock skips it), failing calls included.
    pub fn set_latency(&self, op: MockOp, latency: Duration) {
        lock(&self.calls).latency.insert(op, latency);
    }

    /// How many times `op` has been called, including calls that failed.
    pub fn calls(&self, op: MockOp) -> usize {
        lock(&self.calls).made.get(&op).copied().unwrap_or(0)
    }

    /// The most calls of `op` that have been in flight at once.
    pub fn max_in_flight(&self, op: MockOp) -> usize {
        lock(&self.calls)
            .max_in_flight
            .get(&op)
            .copied()
            .unwrap_or(0)
    }

    /// The bytes of the file at `path`, or `None` if there is no file there.
    /// Not a call: it is not counted and cannot fail.
    pub fn contents(&self, path: &RemotePath) -> Option<Bytes> {
        match lock(&self.state).nodes.get(path) {
            Some(Node::File(file)) => Some(file.data.clone()),
            _ => None,
        }
    }

    /// Starts a call of `op`: counts it, lets the latency set for `op` pass,
    /// then fails it if an error is queued. The call stays in flight until the
    /// returned guard is dropped.
    async fn call(&self, op: MockOp) -> Result<InFlight<'_>, ProviderError> {
        let (latency, failure) = lock(&self.calls).begin(op);
        let in_flight = InFlight {
            calls: &self.calls,
            op,
        };
        if !latency.is_zero() {
            tokio::time::sleep(latency).await;
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(in_flight),
        }
    }
}

#[async_trait]
impl StorageProvider for MockProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn endpoint(&self) -> String {
        "mock://".to_owned()
    }

    async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
        let _call = self.call(MockOp::List).await?;
        let state = lock(&self.state);
        if !state.is_folder(dir) {
            return Err(ProviderError::NotFound);
        }
        Ok(Listing {
            entries: state.children(dir, self.capabilities.checksum_in_listing),
            info: RequestInfo {
                latency: Duration::ZERO,
                http_version: "HTTP/2".to_owned(),
            },
        })
    }

    async fn upload(
        &self,
        path: &RemotePath,
        source: UploadSource,
        opts: &UploadOptions,
    ) -> Result<(), ProviderError> {
        let _call = self.call(MockOp::Upload).await?;
        let data = read_source(source)?;
        if let Some(meter) = &opts.meter {
            for piece in data.chunks(METER_CHUNK) {
                meter.chunk(piece.len()).await;
            }
        }
        let sha256_ok = opts.sha256.is_none_or(|expected| expected == sha256(&data));
        let md5_ok = opts.md5.is_none_or(|expected| expected == md5(&data));
        if !(sha256_ok && md5_ok) {
            return Err(ProviderError::ChecksumMismatch);
        }
        lock(&self.state).put_file(path, data, Some(opts.content_type.clone()))
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        let _call = self.call(MockOp::Download).await?;
        let data = self.contents(path).ok_or(ProviderError::NotFound)?;
        let chunk: Result<Bytes, ProviderError> = Ok(data);
        Ok(Box::pin(stream::iter([chunk])))
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        let _call = self.call(MockOp::DeleteFile).await?;
        lock(&self.state).delete_file(path)
    }

    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        let _call = self.call(MockOp::DeleteFolder).await?;
        if dir.is_root() {
            return Err(ProviderError::Refused(
                "the root folder cannot be deleted".into(),
            ));
        }
        let mut state = lock(&self.state);
        if !state.is_folder(dir) {
            return Err(ProviderError::NotFound);
        }
        let doomed: Vec<RemotePath> = state.beneath(dir).map(|(path, _)| path.clone()).collect();
        if !doomed.is_empty() && !self.capabilities.recursive_delete {
            return Err(ProviderError::Refused(format!(
                "{} is not empty",
                dir.as_str()
            )));
        }
        for path in doomed {
            state.nodes.remove(&path);
        }
        state.nodes.remove(dir);
        Ok(())
    }

    async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        let _call = self.call(MockOp::CreateFolder).await?;
        match self.capabilities.empty_folders {
            EmptyFolders::Markers | EmptyFolders::Supported => lock(&self.state).put_folder(dir),
            EmptyFolders::Unsupported | EmptyFolders::Unknown => Err(ProviderError::Refused(
                "this storage can't keep an empty folder".into(),
            )),
        }
    }

    /// One call for the whole batch: a `DeleteBatch` failure fails every
    /// path, and queued `DeleteFile` failures fail single paths.
    async fn delete_batch(&self, paths: &[RemotePath]) -> Vec<Result<(), ProviderError>> {
        let _call = match self.call(MockOp::DeleteBatch).await {
            Ok(call) => call,
            Err(error) => return vec![Err(error); paths.len()],
        };
        let max = self.capabilities.batch_delete_max;
        if paths.len() > max as usize {
            let refused = ProviderError::Refused(format!(
                "batch of {} exceeds batch_delete_max {max}",
                paths.len()
            ));
            return vec![Err(refused); paths.len()];
        }
        let failures: Vec<Option<ProviderError>> = {
            let mut calls = lock(&self.calls);
            paths
                .iter()
                .map(|_| calls.next_failure(MockOp::DeleteFile))
                .collect()
        };
        let mut state = lock(&self.state);
        paths
            .iter()
            .zip(failures)
            .map(|(path, failure)| match failure {
                Some(error) => Err(error),
                None => state.delete_file(path),
            })
            .collect()
    }

    async fn reachable(&self) -> Result<(), ProviderError> {
        self.call(MockOp::Reachable).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthHint, BodyMeter, EmptyFolders};
    use futures_util::TryStreamExt;
    use futures_util::future::join_all;
    use std::sync::Arc;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    const HELLO_MD5: &str = "5d41402abc4b2a76b9719d911017c592";

    fn p(path: &str) -> RemotePath {
        RemotePath::parse(path).unwrap()
    }

    fn opts() -> UploadOptions {
        UploadOptions::new("text/plain")
    }

    fn auth() -> ProviderError {
        ProviderError::Auth {
            hint: AuthHint::Unknown,
        }
    }

    fn with_caps(caps: Capabilities) -> MockProvider {
        MockProvider::new(caps)
    }

    /// A mock whose listings carry `kind` checksums.
    fn listing(kind: Option<ChecksumKind>) -> MockProvider {
        with_caps(Capabilities {
            checksum_in_listing: kind,
            ..Capabilities::default()
        })
    }

    /// A mock that deletes up to `max` files per batch.
    fn batching(max: u32) -> MockProvider {
        with_caps(Capabilities {
            batch_delete_max: max,
            ..Capabilities::default()
        })
    }

    fn recursive() -> MockProvider {
        with_caps(Capabilities {
            recursive_delete: true,
            ..Capabilities::default()
        })
    }

    /// A temporary file holding `data`, removed when dropped.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(data: &[u8]) -> Self {
            let path =
                std::env::temp_dir().join(format!("httporter-mock-{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, data).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Notes the size of every piece of body it is told about.
    #[derive(Default)]
    struct RecordingMeter(Mutex<Vec<usize>>);

    #[async_trait]
    impl BodyMeter for RecordingMeter {
        async fn chunk(&self, len: usize) {
            self.0.lock().unwrap().push(len);
        }
    }

    fn bytes(data: &'static [u8]) -> UploadSource {
        UploadSource::Bytes(Bytes::from_static(data))
    }

    async fn read_all(stream: ByteStream) -> Vec<u8> {
        let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
        chunks.concat()
    }

    async fn names(m: &MockProvider, dir: &str) -> Vec<(String, bool)> {
        let l = m.list(&p(dir)).await.unwrap();
        l.entries
            .iter()
            .map(|e| (e.name().to_string(), e.is_folder))
            .collect()
    }

    fn mock() -> MockProvider {
        MockProvider::new(Capabilities::default())
    }

    #[tokio::test]
    async fn mock_lists_direct_children_only() {
        let m = MockProvider::new(Capabilities::default())
            .with_file("a/b.txt", b"hi")
            .with_file("a/c/d.txt", b"x")
            .with_file("e.txt", b"y");
        let l = m.list(&RemotePath::parse("a").unwrap()).await.unwrap();
        let got: Vec<_> = l
            .entries
            .iter()
            .map(|e| (e.name().to_string(), e.is_folder))
            .collect();
        assert_eq!(
            got,
            vec![("b.txt".to_string(), false), ("c".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn mock_listing_dedupes_implicit_folders_and_ignores_lookalike_siblings() {
        // Plain string order puts "a-b.txt" and "a.txt" before "a/..." and
        // "a/y-1.txt" before "a/y/...", so a folder's descendants are not
        // contiguous in the map.
        let m = mock()
            .with_file("a-b.txt", b"1")
            .with_file("a.txt", b"2")
            .with_file("a/x.txt", b"3")
            .with_file("a/y-1.txt", b"4")
            .with_file("a/y/w.txt", b"5")
            .with_file("a/y/z.txt", b"6");
        assert_eq!(
            names(&m, "a").await,
            vec![
                ("x.txt".to_string(), false),
                ("y".to_string(), true),
                ("y-1.txt".to_string(), false),
            ]
        );
        assert_eq!(
            names(&m, "").await,
            vec![
                ("a".to_string(), true),
                ("a-b.txt".to_string(), false),
                ("a.txt".to_string(), false),
            ]
        );
    }

    #[tokio::test]
    async fn mock_explicit_folders_list_even_when_empty() {
        let m = mock()
            .with_folder("empty")
            .with_folder("full/inner")
            .with_file("full/f.txt", b"x");
        assert_eq!(
            names(&m, "").await,
            vec![("empty".to_string(), true), ("full".to_string(), true)]
        );
        assert!(names(&m, "empty").await.is_empty());
        assert_eq!(
            names(&m, "full").await,
            vec![("f.txt".to_string(), false), ("inner".to_string(), true)]
        );
    }

    #[tokio::test]
    async fn mock_list_entries_carry_path_size_checksum_and_request_info() {
        let m = listing(Some(ChecksumKind::Sha256)).with_file("dir/hello.txt", b"hello");
        let l = m.list(&p("dir")).await.unwrap();
        assert_eq!(l.info.latency, std::time::Duration::ZERO);
        assert_eq!(l.info.http_version, "HTTP/2");
        let e = &l.entries[0];
        assert_eq!(e.path, p("dir/hello.txt"));
        assert!(!e.is_folder);
        assert_eq!(e.size, 5);
        let checksum = e.checksum.as_ref().unwrap();
        assert_eq!(checksum.kind, ChecksumKind::Sha256);
        assert_eq!(checksum.hex, HELLO_SHA256);

        let root = m.list(&RemotePath::root()).await.unwrap();
        assert!(root.entries[0].is_folder);
        assert_eq!(root.entries[0].path, p("dir"));
        assert!(root.entries[0].checksum.is_none());
    }

    #[tokio::test]
    async fn mock_list_of_missing_folder_or_a_file_is_not_found() {
        let m = mock().with_file("a/b.txt", b"x");
        assert_eq!(
            m.list(&p("nope")).await.unwrap_err(),
            ProviderError::NotFound
        );
        assert_eq!(
            m.list(&p("a/b.txt")).await.unwrap_err(),
            ProviderError::NotFound
        );
    }

    #[test]
    fn mock_reports_its_capabilities_and_endpoint() {
        let caps = Capabilities {
            read_only: true,
            ..Capabilities::default()
        };
        let m = MockProvider::new(caps.clone());
        assert_eq!(m.capabilities(), &caps);
        assert_eq!(m.endpoint(), "mock://");
    }

    #[test]
    fn mock_is_usable_as_a_trait_object() {
        let m: std::sync::Arc<dyn StorageProvider> = std::sync::Arc::new(mock());
        assert_eq!(m.endpoint(), "mock://");
    }

    #[tokio::test]
    async fn mock_upload_then_download_roundtrip() {
        let m = mock();
        let path = p("x/y.txt");
        m.upload(&path, bytes(b"hello"), &opts()).await.unwrap();
        let got = read_all(m.download(&path).await.unwrap()).await;
        assert_eq!(got, b"hello");
        // The upload also made the implied folder visible.
        assert_eq!(names(&m, "").await, vec![("x".to_string(), true)]);
    }

    #[tokio::test]
    async fn mock_upload_overwrites_and_records_content_type() {
        let m = mock().with_file("f.txt", b"old");
        m.upload(&p("f.txt"), bytes(b"newer"), &opts())
            .await
            .unwrap();
        let l = m.list(&RemotePath::root()).await.unwrap();
        assert_eq!(l.entries.len(), 1);
        assert_eq!(l.entries[0].size, 5);
        assert_eq!(l.entries[0].content_type.as_deref(), Some("text/plain"));
        assert_eq!(
            read_all(m.download(&p("f.txt")).await.unwrap()).await,
            b"newer"
        );
    }

    #[tokio::test]
    async fn mock_upload_from_file_source_reads_the_file() {
        let file = TempFile::new(b"from disk");
        let m = mock();
        let source = UploadSource::File {
            path: file.0.clone(),
            len: 9,
        };
        m.upload(&p("disk.bin"), source, &opts()).await.unwrap();
        assert_eq!(
            read_all(m.download(&p("disk.bin")).await.unwrap()).await,
            b"from disk"
        );

        let missing = UploadSource::File {
            path: std::env::temp_dir().join(format!("httporter-gone-{}", uuid::Uuid::new_v4())),
            len: 1,
        };
        let err = m
            .upload(&p("gone.bin"), missing, &opts())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    }

    #[tokio::test]
    async fn mock_upload_verifies_a_supplied_sha256() {
        let m = mock();
        let good: [u8; 32] = hex::decode(HELLO_SHA256).unwrap().try_into().unwrap();
        let with_hash = |sha256| UploadOptions {
            sha256: Some(sha256),
            ..opts()
        };
        m.upload(&p("ok.txt"), bytes(b"hello"), &with_hash(good))
            .await
            .unwrap();

        let err = m
            .upload(&p("bad.txt"), bytes(b"hello!"), &with_hash(good))
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::ChecksumMismatch);
        assert_eq!(
            m.download(&p("bad.txt")).await.err(),
            Some(ProviderError::NotFound)
        );
    }

    #[tokio::test]
    async fn mock_upload_cannot_collide_with_a_folder_or_a_file_ancestor() {
        let m = mock()
            .with_file("dir/f.txt", b"x")
            .with_file("top.txt", b"y");
        let err = m.upload(&p("dir"), bytes(b"z"), &opts()).await.unwrap_err();
        assert!(matches!(err, ProviderError::Refused(_)), "{err:?}");
        let err = m
            .upload(&p("top.txt/inner.txt"), bytes(b"z"), &opts())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Refused(_)), "{err:?}");
        let err = m
            .upload(&RemotePath::root(), bytes(b"z"), &opts())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Refused(_)), "{err:?}");
        // Nothing was disturbed.
        assert_eq!(
            names(&m, "").await,
            vec![("dir".to_string(), true), ("top.txt".to_string(), false)]
        );
    }

    #[tokio::test]
    async fn mock_download_of_missing_file_or_folder_is_not_found() {
        let m = mock().with_file("a/b.txt", b"x");
        assert_eq!(
            m.download(&p("nope.txt")).await.err(),
            Some(ProviderError::NotFound)
        );
        assert_eq!(
            m.download(&p("a")).await.err(),
            Some(ProviderError::NotFound)
        );
    }

    #[tokio::test]
    async fn mock_delete_file_removes_only_that_file() {
        let m = mock().with_file("a/b.txt", b"x").with_file("a/c.txt", b"y");
        m.delete_file(&p("a/b.txt")).await.unwrap();
        assert_eq!(names(&m, "a").await, vec![("c.txt".to_string(), false)]);
        assert_eq!(
            m.delete_file(&p("a/b.txt")).await.unwrap_err(),
            ProviderError::NotFound
        );
        assert_eq!(
            m.delete_file(&p("a")).await.unwrap_err(),
            ProviderError::NotFound
        );
    }

    #[tokio::test]
    async fn mock_delete_folder_is_recursive_with_the_capability() {
        let m = recursive().with_file("a/b/c.txt", b"x");
        m.delete_folder(&p("a")).await.unwrap();
        assert!(
            m.list(&RemotePath::root())
                .await
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[tokio::test]
    async fn mock_delete_folder_leaves_lookalike_siblings_alone() {
        let m = recursive()
            .with_file("a-b.txt", b"1")
            .with_file("a.txt", b"2")
            .with_file("a/x.txt", b"3")
            .with_folder("a/empty")
            .with_file("a2/x.txt", b"4");
        m.delete_folder(&p("a")).await.unwrap();
        assert_eq!(
            names(&m, "").await,
            vec![
                ("a-b.txt".to_string(), false),
                ("a.txt".to_string(), false),
                ("a2".to_string(), true),
            ]
        );
        assert_eq!(
            m.delete_folder(&p("a")).await.unwrap_err(),
            ProviderError::NotFound
        );
    }

    #[tokio::test]
    async fn mock_delete_folder_refuses_the_root_and_files() {
        let m = mock().with_file("a.txt", b"x");
        let err = m.delete_folder(&RemotePath::root()).await.unwrap_err();
        assert!(matches!(err, ProviderError::Refused(_)), "{err:?}");
        assert_eq!(
            m.delete_folder(&p("a.txt")).await.unwrap_err(),
            ProviderError::NotFound
        );
        assert_eq!(names(&m, "").await, vec![("a.txt".to_string(), false)]);
    }

    #[tokio::test]
    async fn mock_fail_next_injects_errors() {
        let m = MockProvider::new(Capabilities::default());
        m.fail_next(MockOp::List, auth(), 1);
        assert_eq!(m.list(&RemotePath::root()).await.unwrap_err(), auth());
        assert!(m.list(&RemotePath::root()).await.is_ok());
        assert_eq!(m.calls(MockOp::List), 2);
    }

    #[tokio::test]
    async fn mock_fail_next_repeats_and_is_scoped_to_one_operation() {
        let m = mock().with_file("f.txt", b"x");
        m.fail_next(MockOp::Download, ProviderError::Network("boom".into()), 2);
        // Other operations are unaffected.
        assert!(m.list(&RemotePath::root()).await.is_ok());
        for _ in 0..2 {
            assert_eq!(
                m.download(&p("f.txt")).await.err(),
                Some(ProviderError::Network("boom".into()))
            );
        }
        assert!(m.download(&p("f.txt")).await.is_ok());
        assert_eq!(m.calls(MockOp::Download), 3);
        assert_eq!(m.calls(MockOp::List), 1);
        assert_eq!(m.calls(MockOp::Upload), 0);
    }

    #[tokio::test]
    async fn mock_failed_mutations_change_nothing() {
        let m = with_caps(Capabilities {
            recursive_delete: true,
            empty_folders: EmptyFolders::Supported,
            ..Capabilities::default()
        })
        .with_file("keep/me.txt", b"x");
        m.fail_next(MockOp::Upload, ProviderError::Network("net".into()), 1);
        m.fail_next(
            MockOp::DeleteFile,
            ProviderError::RateLimited { retry_after: None },
            1,
        );
        m.fail_next(MockOp::DeleteFolder, auth(), 1);
        m.fail_next(MockOp::CreateFolder, ProviderError::ClockSkew, 1);
        m.fail_next(MockOp::DeleteBatch, ProviderError::Network("net".into()), 1);
        assert!(m.upload(&p("new.txt"), bytes(b"n"), &opts()).await.is_err());
        assert!(m.delete_file(&p("keep/me.txt")).await.is_err());
        assert!(m.delete_folder(&p("keep")).await.is_err());
        assert!(m.create_folder(&p("made")).await.is_err());
        let batch = m.delete_batch(&[p("keep/me.txt")]).await;
        assert_eq!(batch, [Err(ProviderError::Network("net".into()))]);
        assert_eq!(names(&m, "").await, vec![("keep".to_string(), true)]);
        assert_eq!(names(&m, "keep").await, vec![("me.txt".to_string(), false)]);
        for op in [
            MockOp::Upload,
            MockOp::DeleteFile,
            MockOp::DeleteFolder,
            MockOp::CreateFolder,
            MockOp::DeleteBatch,
        ] {
            assert_eq!(m.calls(op), 1, "{op:?}");
        }
    }

    /// Ledger 75: without `recursive_delete` a folder goes only once it is empty.
    #[tokio::test]
    async fn mock_refuses_recursive_delete_without_capability() {
        let m = mock().with_file("full/f.txt", b"x").with_folder("empty");
        let err = m.delete_folder(&p("full")).await.unwrap_err();
        assert!(matches!(err, ProviderError::Refused(_)), "{err:?}");
        assert_eq!(names(&m, "full").await, vec![("f.txt".to_string(), false)]);

        m.delete_folder(&p("empty")).await.unwrap();
        assert_eq!(names(&m, "").await, vec![("full".to_string(), true)]);

        // Emptied file by file, it can go too.
        m.delete_file(&p("full/f.txt")).await.unwrap();
        assert_eq!(
            m.delete_folder(&p("full")).await.unwrap_err(),
            ProviderError::NotFound,
            "a folder implied only by its files vanished with the last one"
        );
    }

    /// Ledger 77: a file that is not the length the caller said has changed since it was
    /// looked at, and sending it would store something other than what was meant.
    #[tokio::test]
    async fn mock_rejects_file_source_with_wrong_len() {
        let file = TempFile::new(b"from disk");
        let m = mock();
        for len in [8, 10] {
            let source = UploadSource::File {
                path: file.0.clone(),
                len,
            };
            let err = m.upload(&p("disk.bin"), source, &opts()).await.unwrap_err();
            assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
        }
        assert_eq!(m.contents(&p("disk.bin")), None);
    }

    #[tokio::test]
    async fn mock_meters_upload_chunks() {
        let data: Vec<u8> = (0..150 * 1024).map(|i| (i % 251) as u8).collect();
        let file = TempFile::new(&data);
        let m = mock();
        for (name, source) in [
            ("bytes.bin", UploadSource::Bytes(Bytes::from(data.clone()))),
            (
                "file.bin",
                UploadSource::File {
                    path: file.0.clone(),
                    len: data.len() as u64,
                },
            ),
        ] {
            let meter = Arc::new(RecordingMeter::default());
            let metered = UploadOptions {
                meter: Some(meter.clone()),
                ..opts()
            };
            m.upload(&p(name), source, &metered).await.unwrap();
            let chunks = meter.0.lock().unwrap().clone();
            assert_eq!(chunks.iter().sum::<usize>(), data.len(), "{name}");
            assert_eq!(chunks, [64 * 1024, 64 * 1024, 22 * 1024], "{name}");
            assert_eq!(m.contents(&p(name)).as_deref(), Some(&data[..]));
        }

        // An empty body has nothing to meter.
        let meter = Arc::new(RecordingMeter::default());
        let metered = UploadOptions {
            meter: Some(meter.clone()),
            ..opts()
        };
        m.upload(&p("empty"), bytes(b""), &metered).await.unwrap();
        assert!(meter.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mock_create_folder_follows_empty_folders() {
        for (kind, makes) in [
            (EmptyFolders::Unknown, false),
            (EmptyFolders::Unsupported, false),
            (EmptyFolders::Markers, true),
            (EmptyFolders::Supported, true),
        ] {
            let m = with_caps(Capabilities {
                empty_folders: kind,
                ..Capabilities::default()
            })
            .with_file("f.txt", b"x");
            let made = m.create_folder(&p("new/inner")).await;
            if makes {
                made.unwrap();
                assert_eq!(
                    names(&m, "").await,
                    vec![("f.txt".to_string(), false), ("new".to_string(), true)],
                    "{kind:?}"
                );
                assert_eq!(names(&m, "new").await, vec![("inner".to_string(), true)]);
                assert!(names(&m, "new/inner").await.is_empty());
                // Making it again is fine.
                m.create_folder(&p("new/inner")).await.unwrap();
            } else {
                assert!(matches!(made, Err(ProviderError::Refused(_))), "{kind:?}");
                assert_eq!(names(&m, "").await, vec![("f.txt".to_string(), false)]);
            }
            // A file in the way is refused whatever the provider can do.
            for blocked in ["f.txt", "f.txt/below"] {
                let err = m.create_folder(&p(blocked)).await.unwrap_err();
                assert!(matches!(err, ProviderError::Refused(_)), "{kind:?} {err:?}");
            }
            let calls = if makes { 4 } else { 3 };
            assert_eq!(m.calls(MockOp::CreateFolder), calls, "{kind:?}");
        }
    }

    #[tokio::test]
    async fn mock_contents_is_a_files_bytes() {
        let m = mock().with_file("a/b.txt", b"hi");
        assert_eq!(m.contents(&p("a/b.txt")), Some(Bytes::from_static(b"hi")));
        assert_eq!(m.contents(&p("a")), None);
        assert_eq!(m.contents(&p("nope")), None);
        // Looking does not count as a call.
        assert_eq!(m.calls(MockOp::Download), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn mock_latency_holds_calls_in_flight() {
        let m = mock();
        m.set_latency(MockOp::Upload, Duration::from_millis(100));
        let start = tokio::time::Instant::now();
        let uploads = (0..3).map(|i| {
            let m = &m;
            async move {
                let path = p(&format!("f{i}.txt"));
                m.upload(&path, bytes(b"x"), &opts()).await
            }
        });
        for result in join_all(uploads).await {
            result.unwrap();
        }
        assert_eq!(start.elapsed(), Duration::from_millis(100));
        assert_eq!(m.max_in_flight(MockOp::Upload), 3);
        assert_eq!(m.max_in_flight(MockOp::List), 0);

        // One at a time never overlaps, and other operations take no time.
        m.list(&RemotePath::root()).await.unwrap();
        m.list(&RemotePath::root()).await.unwrap();
        assert_eq!(m.max_in_flight(MockOp::List), 1);
        assert_eq!(start.elapsed(), Duration::from_millis(100));
    }

    #[tokio::test(start_paused = true)]
    async fn mock_a_call_given_up_while_in_flight_is_no_longer_counted() {
        let m = mock();
        let root = RemotePath::root();
        m.set_latency(MockOp::List, Duration::from_secs(10));
        let gave_up = tokio::time::timeout(Duration::from_secs(1), m.list(&root)).await;
        assert!(gave_up.is_err());
        m.set_latency(MockOp::List, Duration::from_millis(10));
        let both = join_all([m.list(&root), m.list(&root)]).await;
        assert!(both.iter().all(Result::is_ok));
        assert_eq!(m.max_in_flight(MockOp::List), 2, "not 3");
    }

    #[tokio::test]
    async fn mock_delete_batch_answers_for_each_path_in_one_call() {
        let m = batching(1000)
            .with_file("a.txt", b"1")
            .with_file("b.txt", b"2")
            .with_file("dir/c.txt", b"3");
        let results = m
            .delete_batch(&[p("a.txt"), p("missing.txt"), p("dir"), p("b.txt")])
            .await;
        assert_eq!(
            results,
            [
                Ok(()),
                Err(ProviderError::NotFound),
                Err(ProviderError::NotFound),
                Ok(())
            ]
        );
        assert_eq!(names(&m, "").await, vec![("dir".to_string(), true)]);
        assert_eq!(m.calls(MockOp::DeleteBatch), 1);
        assert_eq!(m.calls(MockOp::DeleteFile), 0);
        assert!(m.delete_batch(&[]).await.is_empty());
    }

    #[tokio::test]
    async fn mock_delete_batch_enforces_batch_delete_max() {
        let m = batching(2)
            .with_file("a.txt", b"1")
            .with_file("b.txt", b"2")
            .with_file("c.txt", b"3");
        let too_many = m.delete_batch(&[p("a.txt"), p("b.txt"), p("c.txt")]).await;
        let refused = ProviderError::Refused("batch of 3 exceeds batch_delete_max 2".into());
        assert_eq!(too_many, vec![Err(refused); 3]);
        assert_eq!(names(&m, "").await.len(), 3, "nothing was deleted");

        assert_eq!(
            m.delete_batch(&[p("a.txt"), p("b.txt")]).await,
            [Ok(()), Ok(())]
        );
        assert_eq!(names(&m, "").await, vec![("c.txt".to_string(), false)]);
        assert_eq!(m.calls(MockOp::DeleteBatch), 2);
    }

    #[tokio::test]
    async fn mock_delete_batch_injects_per_path_failures() {
        let m = batching(1000)
            .with_file("a.txt", b"1")
            .with_file("b.txt", b"2")
            .with_file("c.txt", b"3");
        let reset = ProviderError::Network("reset".into());
        let limited = ProviderError::RateLimited { retry_after: None };
        m.fail_next(MockOp::DeleteFile, reset.clone(), 1);
        m.fail_next(MockOp::DeleteFile, limited.clone(), 1);
        let results = m.delete_batch(&[p("a.txt"), p("b.txt"), p("c.txt")]).await;
        assert_eq!(results, [Err(reset), Err(limited), Ok(())]);
        assert_eq!(
            names(&m, "").await,
            vec![("a.txt".to_string(), false), ("b.txt".to_string(), false)]
        );
        // The batch is one call, and the injected failures are used up.
        assert_eq!(m.calls(MockOp::DeleteBatch), 1);
        assert_eq!(m.calls(MockOp::DeleteFile), 0);
        assert_eq!(
            m.delete_batch(&[p("a.txt"), p("b.txt")]).await,
            [Ok(()), Ok(())]
        );
    }

    #[tokio::test]
    async fn mock_listing_checksum_follows_capabilities() {
        let sha256 = Checksum {
            kind: ChecksumKind::Sha256,
            hex: HELLO_SHA256.into(),
        };
        let md5 = Checksum {
            kind: ChecksumKind::Md5,
            hex: HELLO_MD5.into(),
        };
        for (kind, expected) in [
            (Some(ChecksumKind::Sha256), Some(sha256)),
            (Some(ChecksumKind::Md5), Some(md5)),
            (None, None),
        ] {
            let m = listing(kind).with_file("a.txt", b"hello");
            m.upload(&p("b.txt"), bytes(b"hello"), &opts())
                .await
                .unwrap();
            let entries = m.list(&RemotePath::root()).await.unwrap().entries;
            for entry in &entries {
                assert_eq!(entry.checksum, expected, "{kind:?} {:?}", entry.path);
            }
            assert_eq!(entries.len(), 2);
        }
    }

    #[tokio::test]
    async fn mock_with_file_etag_reports_multipart() {
        let etag = "9b2cf535f27731c974343645a3985328-5";
        let m = listing(Some(ChecksumKind::Md5))
            .with_file("big.bin", b"parts")
            .with_file_etag("big.bin", etag)
            .with_file("small.txt", b"hello");
        let checksums = |entries: Vec<Entry>| -> Vec<Option<Checksum>> {
            entries.into_iter().map(|e| e.checksum).collect()
        };
        let md5 = Checksum {
            kind: ChecksumKind::Md5,
            hex: HELLO_MD5.into(),
        };
        assert_eq!(
            checksums(m.list(&RemotePath::root()).await.unwrap().entries),
            [
                Some(Checksum {
                    kind: ChecksumKind::MultipartEtag,
                    hex: etag.into(),
                }),
                Some(md5.clone()),
            ]
        );
        // Uploaded again in one piece, it is listed like any other file.
        m.upload(&p("big.bin"), bytes(b"hello"), &opts())
            .await
            .unwrap();
        assert_eq!(
            checksums(m.list(&RemotePath::root()).await.unwrap().entries),
            [Some(md5.clone()), Some(md5)]
        );
    }

    #[tokio::test]
    async fn mock_rejects_md5_mismatch() {
        let m = mock();
        let good: [u8; 16] = hex::decode(HELLO_MD5).unwrap().try_into().unwrap();
        let with_md5 = |md5| UploadOptions {
            md5: Some(md5),
            ..opts()
        };
        m.upload(&p("ok.txt"), bytes(b"hello"), &with_md5(good))
            .await
            .unwrap();
        let err = m
            .upload(&p("bad.txt"), bytes(b"hello!"), &with_md5(good))
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::ChecksumMismatch);
        assert_eq!(m.contents(&p("bad.txt")), None);
        assert_eq!(m.contents(&p("ok.txt")), Some(Bytes::from_static(b"hello")));
    }

    #[tokio::test]
    async fn mock_reachable_answers_without_listing() {
        let m = mock();
        m.reachable().await.unwrap();
        m.fail_next(
            MockOp::Reachable,
            ProviderError::Network("offline".into()),
            1,
        );
        assert_eq!(
            m.reachable().await,
            Err(ProviderError::Network("offline".into()))
        );
        assert_eq!(m.calls(MockOp::Reachable), 2);
        assert_eq!(m.calls(MockOp::List), 0);
    }
}
