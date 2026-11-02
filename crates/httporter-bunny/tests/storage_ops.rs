use std::path::PathBuf;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::TryStreamExt;
use futures_util::future::join_all;
use httporter_bunny::{BunnyConfig, BunnyStorage};
use httporter_core::{
    ByteStream, ProviderError, RemotePath, RetryPolicy, StorageProvider, UploadOptions,
    UploadSource,
};
use sha2::{Digest, Sha256};
use wiremock::matchers::{body_bytes, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn config(server: &MockServer) -> BunnyConfig {
    let mut cfg = BunnyConfig::new("testzone", "storage.bunnycdn.com", "secret");
    cfg.retry = RetryPolicy::immediate(5);
    cfg.base_url = Some(server.uri());
    cfg
}

fn storage(server: &MockServer) -> BunnyStorage {
    BunnyStorage::new(config(server)).unwrap()
}

fn rp(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

fn text_plain() -> UploadOptions {
    UploadOptions {
        content_type: "text/plain".into(),
        sha256: None,
    }
}

/// A file of `len` bytes that is not all one value, so a truncated or
/// shuffled body cannot pass for the real thing.
fn write_file(len: usize) -> (tempfile::TempDir, UploadSource, Vec<u8>) {
    let content: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let dir = tempfile::tempdir().unwrap();
    let file: PathBuf = dir.path().join("payload.bin");
    std::fs::write(&file, &content).unwrap();
    let source = UploadSource::File {
        path: file,
        len: len as u64,
    };
    (dir, source, content)
}

async fn collect(stream: ByteStream) -> Vec<u8> {
    let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
    chunks.concat()
}

#[tokio::test]
async fn upload_sends_checksum_content_type_and_body() {
    let s = MockServer::start().await;
    let sha = Sha256::digest(b"hello");
    Mock::given(method("PUT"))
        .and(path("/testzone/a/b.txt"))
        .and(header("AccessKey", "secret"))
        .and(header("Checksum", hex::encode_upper(sha).as_str()))
        .and(header("Content-Type", "text/plain"))
        .and(body_bytes(b"hello".to_vec()))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    storage(&s)
        .upload(
            &rp("a/b.txt"),
            UploadSource::Bytes("hello".into()),
            &UploadOptions {
                content_type: "text/plain".into(),
                sha256: Some(sha.into()),
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn upload_without_a_checksum_sends_no_checksum_header() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    storage(&s)
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap();
    let requests = s.received_requests().await.unwrap();
    assert!(requests[0].headers.get("checksum").is_none());
}

#[tokio::test]
async fn upload_file_source_streams_file() {
    let s = MockServer::start().await;
    let (_dir, source, content) = write_file(1024 * 1024);
    Mock::given(method("PUT"))
        .and(path("/testzone/big.bin"))
        .and(header("Content-Length", "1048576"))
        .and(body_bytes(content))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    storage(&s)
        .upload(&rp("big.bin"), source, &text_plain())
        .await
        .unwrap();
}

#[tokio::test]
async fn upload_400_with_checksum_is_checksum_mismatch() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400).set_body_string("checksum"))
        .expect(1)
        .mount(&s)
        .await;
    let opts = UploadOptions {
        content_type: "text/plain".into(),
        sha256: Some(Sha256::digest(b"hello").into()),
    };
    let err = storage(&s)
        .upload(&rp("a.txt"), UploadSource::Bytes("hello".into()), &opts)
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::ChecksumMismatch);
}

#[tokio::test]
async fn upload_400_without_a_checksum_is_permanent() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s)
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
}

#[tokio::test]
async fn upload_auth_failure_is_single_request() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s)
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::Auth);
}

#[tokio::test]
async fn upload_encodes_path_segments() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/testzone/x%20y/%23.txt"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    storage(&s)
        .upload(
            &rp("x y/#.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_retried_upload_resends_the_whole_body_from_either_source() {
    let (_dir, file_source, content) = write_file(300 * 1024);
    let sources = [
        file_source,
        UploadSource::Bytes(Bytes::from(content.clone())),
    ];
    for source in sources {
        let s = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&s)
            .await;
        storage(&s)
            .upload(&rp("retry.bin"), source, &text_plain())
            .await
            .unwrap();
        let requests = s.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            assert_eq!(request.body, content);
        }
    }
}

#[tokio::test]
async fn a_missing_local_file_is_permanent_and_sends_nothing() {
    let s = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let source = UploadSource::File {
        path: dir.path().join("gone.bin"),
        len: 10,
    };
    let err = storage(&s)
        .upload(&rp("gone.bin"), source, &text_plain())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn download_streams_body() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/testzone/a/b.txt"))
        .and(header("AccessKey", "secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .expect(1)
        .mount(&s)
        .await;
    let stream = storage(&s).download(&rp("a/b.txt")).await.unwrap();
    assert_eq!(collect(stream).await, b"hello");
}

#[tokio::test]
async fn download_missing_file_is_not_found() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&s)
        .await;
    let result = storage(&s).download(&rp("a.txt")).await;
    assert!(matches!(result, Err(ProviderError::NotFound)));
}

#[tokio::test]
async fn download_server_error_then_success_retries() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .expect(1)
        .mount(&s)
        .await;
    let stream = storage(&s).download(&rp("a.txt")).await.unwrap();
    assert_eq!(collect(stream).await, b"hello");
}

#[tokio::test]
async fn delete_file_and_folder_urls() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/testzone/a/b.txt"))
        .and(header("AccessKey", "secret"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/testzone/a/"))
        .and(header("AccessKey", "secret"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    let storage = storage(&s);
    storage.delete_file(&rp("a/b.txt")).await.unwrap();
    storage.delete_folder(&rp("a")).await.unwrap();
}

#[tokio::test]
async fn delete_root_is_refused_without_request() {
    let s = MockServer::start().await;
    let err = storage(&s)
        .delete_folder(&RemotePath::root())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ProviderError::Refused("Deleting the whole zone isn't allowed".into())
    );
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_root_is_not_a_file_for_any_operation() {
    let s = MockServer::start().await;
    let storage = storage(&s);
    let root = RemotePath::root();
    let upload = storage
        .upload(&root, UploadSource::Bytes("x".into()), &text_plain())
        .await;
    assert!(
        matches!(upload, Err(ProviderError::Refused(_))),
        "{upload:?}"
    );
    let download = storage.download(&root).await;
    assert!(matches!(download, Err(ProviderError::Refused(_))));
    let delete = storage.delete_file(&root).await;
    assert!(
        matches!(delete, Err(ProviderError::Refused(_))),
        "{delete:?}"
    );
    // A DELETE of the zone URL would remove the whole zone.
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_missing_file_is_not_found() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s).delete_file(&rp("a.txt")).await.unwrap_err();
    assert_eq!(err, ProviderError::NotFound);
}

#[tokio::test]
async fn delete_busy_400_is_retried() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(400).set_body_string("Unable to process request"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&s)
        .await;
    storage(&s).delete_file(&rp("a.txt")).await.unwrap();
}

#[tokio::test]
async fn delete_persistent_server_error_gives_up_after_six_attempts() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(503))
        .expect(6)
        .mount(&s)
        .await;
    let err = storage(&s).delete_folder(&rp("a")).await.unwrap_err();
    assert!(matches!(err, ProviderError::Transient(_)), "{err:?}");
}

#[tokio::test]
async fn concurrent_deletes_never_exceed_delete_concurrency() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(200)))
        .expect(6)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.delete_concurrency = 2;
    let storage = BunnyStorage::new(cfg).unwrap();
    let file = rp("a.txt");
    let started = Instant::now();
    let results = join_all((0..6).map(|_| storage.delete_file(&file))).await;
    let elapsed = started.elapsed();
    assert!(results.iter().all(Result::is_ok));
    // Six deletes through two slots is three waves of 200 ms.
    assert!(elapsed >= Duration::from_millis(590), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[tokio::test]
async fn concurrent_uploads_never_exceed_transfer_concurrency() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_millis(200)))
        .expect(6)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.transfer_concurrency = 2;
    let storage = BunnyStorage::new(cfg).unwrap();
    let file = rp("a.txt");
    let opts = text_plain();
    let started = Instant::now();
    let results =
        join_all((0..6).map(|_| storage.upload(&file, UploadSource::Bytes("hello".into()), &opts)))
            .await;
    let elapsed = started.elapsed();
    assert!(results.iter().all(Result::is_ok));
    assert!(elapsed >= Duration::from_millis(590), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[tokio::test]
async fn an_open_download_keeps_its_transfer_slot_until_dropped() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.transfer_concurrency = 1;
    let storage = BunnyStorage::new(cfg).unwrap();
    let file = rp("a.txt");

    let first = storage.download(&file).await.unwrap();
    let second = tokio::time::timeout(Duration::from_millis(300), storage.download(&file)).await;
    assert!(second.is_err(), "the only slot is still held");

    drop(first);
    let second = storage.download(&file).await.unwrap();
    assert_eq!(collect(second).await, b"hello");
}

#[tokio::test]
async fn an_upload_that_gets_no_answer_for_the_idle_timeout_is_abandoned() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_millis(1500)))
        .expect(1)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.transfer_idle_timeout = Duration::from_millis(250);
    cfg.retry = RetryPolicy::immediate(0);
    let started = Instant::now();
    let err = BunnyStorage::new(cfg)
        .unwrap()
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap_err();
    match err {
        ProviderError::Transient(m) => assert!(m.contains("upload stalled"), "{m}"),
        other => panic!("expected Transient, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_millis(1200));
}

#[tokio::test]
async fn a_download_that_gets_no_answer_for_the_idle_timeout_is_abandoned() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(1500)))
        .expect(1)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.transfer_idle_timeout = Duration::from_millis(250);
    cfg.retry = RetryPolicy::immediate(0);
    let started = Instant::now();
    let result = BunnyStorage::new(cfg).unwrap().download(&rp("a.txt")).await;
    match result {
        Err(ProviderError::Transient(m)) => assert!(m.contains("download stalled"), "{m}"),
        other => panic!("expected Transient, got {:?}", other.map(|_| "a stream")),
    }
    assert!(started.elapsed() < Duration::from_millis(1200));
}

#[tokio::test]
async fn a_stalled_upload_is_retried() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201).set_delay(Duration::from_millis(1500)))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.transfer_idle_timeout = Duration::from_millis(250);
    BunnyStorage::new(cfg)
        .unwrap()
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn an_empty_file_uploads_with_a_zero_length_body() {
    let s = MockServer::start().await;
    let (_dir, source, _) = write_file(0);
    Mock::given(method("PUT"))
        .and(path("/testzone/empty.txt"))
        .and(header("Content-Length", "0"))
        .and(body_bytes(Vec::new()))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    storage(&s)
        .upload(&rp("empty.txt"), source, &text_plain())
        .await
        .unwrap();
}

fn checksummed() -> UploadOptions {
    UploadOptions {
        content_type: "text/plain".into(),
        sha256: Some(Sha256::digest(b"hello").into()),
    }
}

#[tokio::test]
async fn a_busy_400_on_a_checksummed_upload_is_retried() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400).set_body_string("Unable to process request"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&s)
        .await;
    storage(&s)
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &checksummed(),
        )
        .await
        .unwrap();
    assert_eq!(s.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn any_other_400_on_a_checksummed_upload_is_a_single_checksum_mismatch() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400).set_body_string("The checksum does not match"))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s)
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &checksummed(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, ProviderError::ChecksumMismatch);
    assert_eq!(s.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_400_on_an_unchecksummed_upload_is_permanent_and_quotes_the_server() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400).set_body_string("Bad thing"))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s)
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap_err();
    match err {
        ProviderError::Permanent(m) => {
            assert!(m.contains("Bad thing"), "{m}");
            assert!(!m.contains("secret"), "the access key must not leak: {m}");
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
    assert_eq!(s.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn only_a_short_snippet_of_a_huge_400_body_reaches_the_error() {
    let s = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(400).set_body_string("x".repeat(10 * 1024)))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s)
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &text_plain(),
        )
        .await
        .unwrap_err();
    match err {
        ProviderError::Permanent(m) => assert!(m.len() < 400, "{} bytes", m.len()),
        other => panic!("expected Permanent, got {other:?}"),
    }
}

/// A short listing policy (for someone waiting on a folder) leaves deletes alone: they keep the
/// general retries and their own, longer time limit.
#[tokio::test]
async fn the_listing_policy_does_not_apply_to_deletes() {
    let s = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/testzone/slow.txt"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(400)))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/testzone/failing.txt"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.retry = RetryPolicy::immediate(2);
    cfg.list_timeout = Duration::from_millis(100);
    cfg.list_retry = Some(RetryPolicy::immediate(0));
    let storage = BunnyStorage::new(cfg).unwrap();
    storage.delete_file(&rp("slow.txt")).await.unwrap();
    let err = storage.delete_file(&rp("failing.txt")).await.unwrap_err();
    assert!(matches!(err, ProviderError::Transient(_)), "{err:?}");
}
