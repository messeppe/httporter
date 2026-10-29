use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::stream;
use sha2::{Digest, Sha256};

use crate::{
    ByteStream, Capabilities, Checksum, ChecksumKind, Entry, Listing, ProviderError, RemotePath,
    RequestInfo, StorageProvider, UploadOptions, UploadSource,
};

/// A [`StorageProvider`] operation, for counting calls and injecting failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MockOp {
    List,
    Upload,
    Download,
    DeleteFile,
    DeleteFolder,
}

struct File {
    data: Bytes,
    modified: DateTime<Utc>,
    content_type: Option<String>,
    checksum: Checksum,
}

enum Node {
    File(File),
    /// An explicitly created folder. Folders are otherwise implied by the file
    /// paths beneath them.
    Folder,
}

#[derive(Default)]
struct State {
    /// Never contains the root. A path is a file or a folder, never both:
    /// `put_file` / `put_folder` refuse writes that would break that.
    nodes: BTreeMap<RemotePath, Node>,
    calls: HashMap<MockOp, usize>,
    failures: HashMap<MockOp, VecDeque<ProviderError>>,
}

impl State {
    /// Counts the call, then fails it if an injected error is queued.
    fn begin(&mut self, op: MockOp) -> Result<(), ProviderError> {
        *self.calls.entry(op).or_default() += 1;
        match self.failures.get_mut(&op).and_then(VecDeque::pop_front) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Everything strictly beneath `dir`, in map order.
    ///
    /// This is a full scan with a prefix test on purpose: `RemotePath` orders
    /// as plain strings, so `a-b` sorts before `a/b` and a folder's
    /// descendants are not contiguous in the map. A range scan would be wrong.
    fn beneath(&self, dir: &RemotePath) -> impl Iterator<Item = (&RemotePath, &Node)> {
        let prefix = if dir.is_root() {
            String::new()
        } else {
            format!("{}/", dir.as_str())
        };
        self.nodes
            .iter()
            .filter(move |(path, _)| path.as_str().starts_with(&prefix))
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
        let checksum = Checksum {
            kind: ChecksumKind::Sha256,
            hex: hex::encode(sha256(&data)),
        };
        self.nodes.insert(
            path.clone(),
            Node::File(File {
                data,
                modified: Utc::now(),
                content_type,
                checksum,
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

    /// The direct children of `dir`, sorted by name. A folder implied by
    /// several deeper paths is listed once.
    fn children(&self, dir: &RemotePath) -> Vec<Entry> {
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
                .or_insert_with(|| child_entry(dir, name, file));
        }
        by_name.into_values().collect()
    }
}

fn child_entry(dir: &RemotePath, name: &str, file: Option<&File>) -> Entry {
    let path = dir
        .join(name)
        .expect("a segment of a valid path is a valid segment");
    match file {
        Some(file) => Entry {
            path,
            is_folder: false,
            size: file.data.len() as u64,
            modified: Some(file.modified),
            checksum: Some(file.checksum.clone()),
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

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// An in-memory [`StorageProvider`] for tests of the UI and the engine.
///
/// Folders are implied by the files beneath them; `with_folder` makes an
/// empty one. Listing a path that is not a folder is `NotFound`. Every call is
/// counted, and [`fail_next`](Self::fail_next) makes upcoming calls fail
/// before they touch any state.
pub struct MockProvider {
    capabilities: Capabilities,
    state: Mutex<State>,
}

impl MockProvider {
    pub fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            state: Mutex::new(State::default()),
        }
    }

    /// Adds (or replaces) a file. Panics if `path` is not a valid, writable
    /// path: this is test setup, not an operation under test.
    pub fn with_file(self, path: &str, data: &[u8]) -> Self {
        let path = RemotePath::parse(path).expect("with_file: invalid path");
        self.lock()
            .put_file(&path, Bytes::copy_from_slice(data), None)
            .expect("with_file: path collides with an existing file or folder");
        self
    }

    /// Adds an explicit (possibly empty) folder. Panics like [`with_file`](Self::with_file).
    pub fn with_folder(self, path: &str) -> Self {
        let path = RemotePath::parse(path).expect("with_folder: invalid path");
        self.lock()
            .put_folder(&path)
            .expect("with_folder: path collides with an existing file");
        self
    }

    /// Makes the next `times` calls of `op` fail with `error`, one error per
    /// call, after being counted. Queues behind errors already pending for `op`.
    pub fn fail_next(&self, op: MockOp, error: ProviderError, times: usize) {
        self.lock()
            .failures
            .entry(op)
            .or_default()
            .extend(std::iter::repeat_n(error, times));
    }

    /// How many times `op` has been called, including calls that failed.
    pub fn calls(&self, op: MockOp) -> usize {
        self.lock().calls.get(&op).copied().unwrap_or(0)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is never left half-updated, so a poisoned lock is safe to use.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
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
        let mut state = self.lock();
        state.begin(MockOp::List)?;
        if !state.is_folder(dir) {
            return Err(ProviderError::NotFound);
        }
        Ok(Listing {
            entries: state.children(dir),
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
        self.lock().begin(MockOp::Upload)?;
        let data = match source {
            UploadSource::Bytes(bytes) => bytes,
            UploadSource::File { path: file, .. } => std::fs::read(&file)
                .map(Bytes::from)
                .map_err(|e| ProviderError::Permanent(format!("{}: {e}", file.display())))?,
        };
        if opts
            .sha256
            .is_some_and(|expected| expected != sha256(&data))
        {
            return Err(ProviderError::ChecksumMismatch);
        }
        self.lock()
            .put_file(path, data, Some(opts.content_type.clone()))
    }

    async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
        let mut state = self.lock();
        state.begin(MockOp::Download)?;
        match state.nodes.get(path) {
            Some(Node::File(file)) => {
                let chunk: Result<Bytes, ProviderError> = Ok(file.data.clone());
                Ok(Box::pin(stream::iter([chunk])))
            }
            _ => Err(ProviderError::NotFound),
        }
    }

    async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
        let mut state = self.lock();
        state.begin(MockOp::DeleteFile)?;
        if matches!(state.nodes.get(path), Some(Node::File(_))) {
            state.nodes.remove(path);
            Ok(())
        } else {
            Err(ProviderError::NotFound)
        }
    }

    async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
        let mut state = self.lock();
        state.begin(MockOp::DeleteFolder)?;
        if dir.is_root() {
            return Err(ProviderError::Refused(
                "the root folder cannot be deleted".into(),
            ));
        }
        if !state.is_folder(dir) {
            return Err(ProviderError::NotFound);
        }
        let doomed: Vec<RemotePath> = state.beneath(dir).map(|(path, _)| path.clone()).collect();
        for path in doomed {
            state.nodes.remove(&path);
        }
        state.nodes.remove(dir);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ByteStream, Capabilities, ChecksumKind, ProviderError, RemotePath, StorageProvider,
        UploadOptions, UploadSource,
    };
    use bytes::Bytes;
    use futures_util::TryStreamExt;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn p(path: &str) -> RemotePath {
        RemotePath::parse(path).unwrap()
    }

    fn opts() -> UploadOptions {
        UploadOptions {
            content_type: "text/plain".into(),
            sha256: None,
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
        let m = mock().with_file("dir/hello.txt", b"hello");
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
        let path = std::env::temp_dir().join(format!("httporter-mock-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"from disk").unwrap();
        let m = mock();
        let source = UploadSource::File {
            path: path.clone(),
            len: 9,
        };
        let result = m.upload(&p("disk.bin"), source, &opts()).await;
        std::fs::remove_file(&path).unwrap();
        result.unwrap();
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
            content_type: "text/plain".into(),
            sha256: Some(sha256),
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
    async fn mock_delete_folder_is_recursive() {
        let m = mock().with_file("a/b/c.txt", b"x");
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
        let m = mock()
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
        m.fail_next(MockOp::List, ProviderError::Auth, 1);
        assert_eq!(
            m.list(&RemotePath::root()).await.unwrap_err(),
            ProviderError::Auth
        );
        assert!(m.list(&RemotePath::root()).await.is_ok());
        assert_eq!(m.calls(MockOp::List), 2);
    }

    #[tokio::test]
    async fn mock_fail_next_repeats_and_is_scoped_to_one_operation() {
        let m = mock().with_file("f.txt", b"x");
        m.fail_next(MockOp::Download, ProviderError::Transient("boom".into()), 2);
        // Other operations are unaffected.
        assert!(m.list(&RemotePath::root()).await.is_ok());
        for _ in 0..2 {
            assert_eq!(
                m.download(&p("f.txt")).await.err(),
                Some(ProviderError::Transient("boom".into()))
            );
        }
        assert!(m.download(&p("f.txt")).await.is_ok());
        assert_eq!(m.calls(MockOp::Download), 3);
        assert_eq!(m.calls(MockOp::List), 1);
        assert_eq!(m.calls(MockOp::Upload), 0);
    }

    #[tokio::test]
    async fn mock_failed_mutations_change_nothing() {
        let m = mock().with_file("keep/me.txt", b"x");
        m.fail_next(MockOp::Upload, ProviderError::Transient("net".into()), 1);
        m.fail_next(
            MockOp::DeleteFile,
            ProviderError::RateLimited { retry_after: None },
            1,
        );
        m.fail_next(MockOp::DeleteFolder, ProviderError::Auth, 1);
        assert!(m.upload(&p("new.txt"), bytes(b"n"), &opts()).await.is_err());
        assert!(m.delete_file(&p("keep/me.txt")).await.is_err());
        assert!(m.delete_folder(&p("keep")).await.is_err());
        assert_eq!(names(&m, "").await, vec![("keep".to_string(), true)]);
        assert_eq!(names(&m, "keep").await, vec![("me.txt".to_string(), false)]);
        assert_eq!(m.calls(MockOp::Upload), 1);
        assert_eq!(m.calls(MockOp::DeleteFile), 1);
        assert_eq!(m.calls(MockOp::DeleteFolder), 1);
    }
}
