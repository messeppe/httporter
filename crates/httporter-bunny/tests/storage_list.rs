use std::time::{Duration, Instant};

use futures_util::future::join_all;
use httporter_bunny::{BunnyConfig, BunnyStorage};
use httporter_core::{
    AuthHint, Capabilities, ChecksumKind, EmptyFolders, ProviderError, RemotePath, RetryPolicy,
    StorageProvider,
};
use wiremock::matchers::{header, method, path};
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

#[tokio::test]
async fn list_root_requests_zone_with_trailing_slash_and_access_key() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/testzone/"))
        .and(header("AccessKey", "secret"))
        .and(header("Accept", "application/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/list_root.json")),
        )
        .expect(1)
        .mount(&s)
        .await;
    let l = storage(&s).list(&RemotePath::root()).await.unwrap();
    assert_eq!(l.entries.len(), 2);
    assert_eq!(storage(&s).endpoint(), format!("{}/testzone/", s.uri()));
}

#[tokio::test]
async fn endpoint_defaults_to_the_regional_https_host() {
    let cfg = BunnyConfig::new("testzone", "sg.storage.bunnycdn.com", "secret");
    assert_eq!(
        BunnyStorage::new(cfg).unwrap().endpoint(),
        "https://sg.storage.bunnycdn.com/testzone/"
    );
}

#[tokio::test]
async fn list_joins_entry_names_onto_the_listed_directory() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/testzone/wp-content/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/list_root.json")),
        )
        .expect(1)
        .mount(&s)
        .await;
    let dir = RemotePath::parse("wp-content").unwrap();
    let l = storage(&s).list(&dir).await.unwrap();
    let paths: Vec<&str> = l.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        ["wp-content/author-sitemap.xml", "wp-content/wp-content"]
    );
    assert!(!l.entries[0].is_folder && l.entries[1].is_folder);
}

#[tokio::test]
async fn list_encodes_segments() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/testzone/x%20y%23/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&s)
        .await;
    let dir = RemotePath::parse("x y#").unwrap();
    let l = storage(&s).list(&dir).await.unwrap();
    assert!(l.entries.is_empty());
}

#[tokio::test]
async fn auth_failure_is_single_request() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s).list(&RemotePath::root()).await.unwrap_err();
    assert_eq!(
        err,
        ProviderError::Auth {
            hint: AuthHint::Unknown
        }
    );
}

#[tokio::test]
async fn server_error_then_success_retries() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&s)
        .await;
    let l = storage(&s).list(&RemotePath::root()).await.unwrap();
    assert!(l.entries.is_empty());
}

#[tokio::test]
async fn persistent_server_error_gives_up_after_six_attempts() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(6)
        .mount(&s)
        .await;
    let err = storage(&s).list(&RemotePath::root()).await.unwrap_err();
    assert!(
        matches!(err, ProviderError::Server { status: 503, .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn busy_400_is_a_server_error_and_retried() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_string("Unable to process request"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&s)
        .await;
    let l = storage(&s).list(&RemotePath::root()).await.unwrap();
    assert!(l.entries.is_empty());
}

#[tokio::test]
async fn not_found_is_not_retried() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s).list(&RemotePath::root()).await.unwrap_err();
    assert_eq!(err, ProviderError::NotFound);
}

#[tokio::test]
async fn non_json_body_is_permanent_error() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>Login</html>"))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s).list(&RemotePath::root()).await.unwrap_err();
    match err {
        ProviderError::Permanent(m) => assert!(m.contains("not JSON"), "{m}"),
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn json_of_the_wrong_shape_is_permanent_error() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"Message":"nope"}"#))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s).list(&RemotePath::root()).await.unwrap_err();
    match err {
        ProviderError::Permanent(m) => {
            assert!(m.contains("unexpected response from server"), "{m}");
            assert!(!m.contains("not JSON"), "{m}");
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn a_malformed_object_fails_the_whole_listing_and_is_named() {
    let s = MockServer::start().await;
    let body = r#"[
        {"ObjectName":"good.txt","Length":1,"IsDirectory":false,"LastChanged":"2026-10-28T10:02:51.089","Checksum":null},
        {"ObjectName":"bad.txt","Length":1,"IsDirectory":false,"LastChanged":"2026-10-28T10:02:51.089","Checksum":"XYZ"}
    ]"#;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&s)
        .await;
    let err = storage(&s).list(&RemotePath::root()).await.unwrap_err();
    match err {
        ProviderError::Permanent(m) => {
            assert!(
                m.starts_with("unexpected entry \"bad.txt\" in listing: "),
                "{m}"
            );
            assert!(m.contains("SHA-256"), "{m}");
        }
        other => panic!("expected Permanent, got {other:?}"),
    }
}

#[tokio::test]
async fn rate_limited_maps_retry_after() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
        .expect(1)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.retry = RetryPolicy::immediate(0);
    let err = BunnyStorage::new(cfg)
        .unwrap()
        .list(&RemotePath::root())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(2))
        }
    );
}

/// Ruling F4: a listing someone is waiting for reports a long `Retry-After` at once instead of
/// waiting it out (up to five minutes per try before).
#[tokio::test]
async fn an_interactive_listing_does_not_wait_out_a_long_retry_after() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "120"))
        .expect(1)
        .mount(&s)
        .await;
    let mut cfg = config(&s);
    cfg.list_retry = Some(RetryPolicy::interactive());
    let storage = BunnyStorage::new(cfg).unwrap();
    let root = RemotePath::root();
    let started = Instant::now();
    let err = tokio::time::timeout(Duration::from_secs(10), storage.list(&root))
        .await
        .expect("the listing fails fast")
        .unwrap_err();
    assert_eq!(
        err,
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(120))
        }
    );
    // Not even the policy's first one-second wait.
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[tokio::test]
async fn listing_reports_latency_and_http_version() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&s)
        .await;
    let l = storage(&s).list(&RemotePath::root()).await.unwrap();
    assert_eq!(l.info.http_version, "HTTP/1.1");
    assert!(l.info.latency > Duration::ZERO, "{:?}", l.info.latency);
}

#[tokio::test]
async fn concurrent_lists_never_exceed_five() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("[]")
                .set_delay(Duration::from_millis(200)),
        )
        .expect(12)
        .mount(&s)
        .await;
    let storage = storage(&s);
    let root = RemotePath::root();
    let started = Instant::now();
    let results = join_all((0..12).map(|_| storage.list(&root))).await;
    let elapsed = started.elapsed();
    assert!(results.iter().all(Result::is_ok));
    // 12 requests through 5 slots is three waves of 200 ms.
    assert!(elapsed >= Duration::from_millis(590), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[tokio::test]
async fn capabilities_match_bunny_limits() {
    let s = MockServer::start().await;
    let default = BunnyStorage::new(config(&s)).unwrap();
    assert_eq!(
        default.capabilities(),
        &Capabilities {
            checksum_in_listing: Some(ChecksumKind::Sha256),
            server_side_copy: false,
            recursive_delete: true,
            batch_delete_max: 1,
            empty_folders: EmptyFolders::Unknown,
            list_concurrency: 5,
            delete_concurrency: 16,
            transfer_concurrency_max: 96,
            presign: false,
            read_only: false,
        }
    );

    let mut cfg = config(&s);
    cfg.delete_concurrency = 50;
    let capped = BunnyStorage::new(cfg).unwrap();
    assert_eq!(capped.capabilities().delete_concurrency, 30);
}

#[tokio::test]
async fn a_password_that_cannot_be_a_header_is_rejected_without_echoing_it() {
    let cfg = BunnyConfig::new("testzone", "storage.bunnycdn.com", "bad\npassword");
    match BunnyStorage::new(cfg) {
        Err(ProviderError::Permanent(m)) => assert!(!m.contains("bad"), "{m}"),
        Err(other) => panic!("expected Permanent, got {other:?}"),
        Ok(_) => panic!("expected an error"),
    }
}

#[tokio::test]
async fn a_listing_gives_up_after_its_own_timeout_and_retries_by_its_own_policy() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("[]")
                .set_delay(Duration::from_secs(2)),
        )
        .expect(2)
        .mount(&s)
        .await;
    // Uploads, downloads and deletes keep five retries; listings get one.
    let mut cfg = config(&s);
    cfg.list_timeout = Duration::from_millis(200);
    cfg.list_retry = Some(RetryPolicy::immediate(1));
    let started = Instant::now();
    let err = BunnyStorage::new(cfg)
        .unwrap()
        .list(&RemotePath::root())
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Network(_)), "{err:?}");
    // Two tries of 200 ms, not six of two seconds.
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[test]
fn listings_keep_the_long_policy_unless_told_otherwise() {
    let cfg = BunnyConfig::new("testzone", "storage.bunnycdn.com", "secret");
    assert_eq!(cfg.list_timeout, Duration::from_secs(120));
    assert_eq!(cfg.list_retry, None);
    assert_eq!(cfg.retry, RetryPolicy::default());
}
