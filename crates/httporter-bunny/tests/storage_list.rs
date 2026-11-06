// SPDX-License-Identifier: MIT OR Apache-2.0

mod support;

use std::time::{Duration, Instant};

use futures_util::future::join_all;
use httporter_bunny::{BunnyConfig, BunnyStorage};
use httporter_core::{
    AuthHint, Capabilities, ChecksumKind, EmptyFolders, HttpProtocol, ProviderError, ProxyMode,
    RemotePath, RetryPolicy, StorageProvider, TransferSettings, UploadOptions, UploadSource,
};
use support::{ScriptedServer, answer, chunked_answer, settle};
use wiremock::matchers::{any, header, method, path};
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

/// The client a site uses (`for_site`), pointed at a local server.
fn site(base_url: &str, settings: &TransferSettings) -> BunnyStorage {
    let mut cfg = BunnyConfig::for_site("testzone", "storage.bunnycdn.com", "secret", settings);
    cfg.base_url = Some(base_url.to_owned());
    BunnyStorage::new(cfg).unwrap()
}

fn default_site(base_url: &str) -> BunnyStorage {
    site(base_url, &TransferSettings::default())
}

fn rp(path: &str) -> RemotePath {
    RemotePath::parse(path).unwrap()
}

fn is_server(result: &Result<impl Sized, ProviderError>, code: u16) -> bool {
    matches!(result, Err(ProviderError::Server { status, .. }) if *status == code)
}

#[test]
fn for_site_follows_the_transfer_settings() {
    let settings = TransferSettings {
        simultaneous: 200,
        protocol: HttpProtocol::Http1Only,
        keep_alive: false,
        // Below the floor: the accessor makes it 5 s.
        request_timeout_secs: 2,
        proxy: ProxyMode::Custom("http://proxy.test:8080".into()),
        ..TransferSettings::default()
    };
    let cfg = BunnyConfig::for_site("testzone", "sg.storage.bunnycdn.com", "secret", &settings);
    assert_eq!(cfg.zone, "testzone");
    assert_eq!(cfg.host, "sg.storage.bunnycdn.com");
    assert_eq!(cfg.password, "secret");
    // Callers own retries: one attempt per call.
    assert_eq!(cfg.retry, RetryPolicy::immediate(0));
    assert_eq!(cfg.list_retry, None);
    assert_eq!(cfg.list_timeout, Duration::from_secs(20));
    assert_eq!(cfg.transfer_concurrency, 96, "capped at Bunny's limit");
    assert_eq!(cfg.request_timeout, Duration::from_secs(5));
    assert_eq!(cfg.transfer_idle_timeout, Duration::from_secs(5));
    assert!(cfg.http1_only);
    assert!(!cfg.keep_alive);
    assert_eq!(
        cfg.proxy,
        ProxyMode::Custom("http://proxy.test:8080".into())
    );
    assert_eq!(cfg.base_url, None);
    assert_eq!(cfg.delete_concurrency, 16);

    let cfg = BunnyConfig::for_site(
        "testzone",
        "sg.storage.bunnycdn.com",
        "secret",
        &TransferSettings::default(),
    );
    assert_eq!(cfg.transfer_concurrency, 32);
    assert_eq!(cfg.request_timeout, Duration::from_secs(120));
    assert_eq!(cfg.transfer_idle_timeout, Duration::from_secs(120));
    assert!(!cfg.http1_only);
    assert!(cfg.keep_alive);
    assert_eq!(cfg.proxy, ProxyMode::System);
}

#[test]
fn a_site_config_hides_its_secrets_when_printed() {
    let settings = TransferSettings {
        proxy: ProxyMode::Custom("http://proxyuser:proxy-pw@proxy.test:8080".into()),
        ..TransferSettings::default()
    };
    let cfg = BunnyConfig::for_site("testzone", "storage.bunnycdn.com", "hunter2", &settings);
    let text = format!("{cfg:?}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("proxy-pw"), "{text}");
    assert!(text.contains("keep_alive: true"), "{text}");
}

/// Callers own retries: the client a site uses tries each request once, whatever the operation.
#[tokio::test]
async fn for_site_is_single_attempt() {
    let s = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(503))
        .expect(4)
        .mount(&s)
        .await;
    let storage = default_site(&s.uri());
    assert!(is_server(&storage.list(&RemotePath::root()).await, 503));
    let upload = storage
        .upload(
            &rp("a.txt"),
            UploadSource::Bytes("hello".into()),
            &UploadOptions::new("text/plain"),
        )
        .await;
    assert!(is_server(&upload, 503), "{upload:?}");
    assert!(is_server(&storage.download(&rp("a.txt")).await, 503));
    assert!(is_server(&storage.delete_file(&rp("a.txt")).await, 503));
    assert_eq!(s.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn busy_400_maps_to_server_400() {
    let s = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(400).set_body_string("Unable to process request"))
        .expect(2)
        .mount(&s)
        .await;
    let storage = default_site(&s.uri());
    let busy = ProviderError::Server {
        status: 400,
        detail: "busy".into(),
    };
    assert_eq!(storage.list(&RemotePath::root()).await, Err(busy.clone()));
    assert_eq!(storage.delete_file(&rp("a.txt")).await, Err(busy.clone()));
    assert!(busy.is_retryable() && busy.is_slow_down());
}

/// Ruling Q17: the provider clamps a server's Retry-After before returning it; the caller's
/// policy decides whether to wait.
#[tokio::test]
async fn retry_after_capped_at_300s() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "99999"))
        .expect(1)
        .mount(&s)
        .await;
    let err = default_site(&s.uri())
        .list(&RemotePath::root())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(300))
        }
    );
}

/// Ledger 143: an error answer left unread can't be followed by another request on the same
/// connection, so every busy retry paid for a new connection (and, for real, a TLS handshake).
/// The answers here come in chunks, which a dropped answer leaves behind.
#[tokio::test]
async fn error_bodies_are_read_so_the_connection_is_reused() {
    let busy = || chunked_answer("503 Service Unavailable", &["Service ", "busy, ", "retry"]);
    let server = ScriptedServer::start(vec![busy(), busy(), busy(), answer("200 OK", "[]")]).await;
    let storage = default_site(&server.uri());
    for _ in 0..3 {
        assert!(is_server(&storage.list(&RemotePath::root()).await, 503));
        settle().await;
    }
    let listing = storage.list(&RemotePath::root()).await.unwrap();
    assert!(listing.entries.is_empty());
    assert_eq!(server.requests().len(), 4);
    assert_eq!(server.connections(), 1, "{:?}", server.requests());
}

#[tokio::test]
async fn keep_alive_off_uses_a_fresh_connection_per_request() {
    for (keep_alive, expected) in [(false, 3), (true, 1)] {
        let server = ScriptedServer::start(vec![answer("200 OK", "[]"); 3]).await;
        let storage = site(
            &server.uri(),
            &TransferSettings {
                keep_alive,
                ..TransferSettings::default()
            },
        );
        for _ in 0..3 {
            storage.list(&RemotePath::root()).await.unwrap();
            settle().await;
        }
        assert_eq!(server.requests().len(), 3);
        assert_eq!(
            server.connections(),
            expected,
            "keep_alive {keep_alive}: {:?}",
            server.requests()
        );
    }
}

/// `reachable` asks only whether the server answers: any status will do, and the probe carries
/// no password.
#[tokio::test]
async fn reachable_ok_on_any_status() {
    for code in [200, 301, 401, 403, 404, 500, 503] {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(code).insert_header("Location", "/elsewhere"))
            .expect(1)
            .mount(&s)
            .await;
        assert_eq!(default_site(&s.uri()).reachable().await, Ok(()), "{code}");
        let requests = s.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "{code}: redirects are not followed");
        assert!(
            requests[0].headers.get("accesskey").is_none(),
            "{code}: the probe sends no AccessKey"
        );
    }
}

#[tokio::test]
async fn reachable_network_error_offline() {
    // A port nobody listens on.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let result = default_site(&format!("http://127.0.0.1:{port}"))
        .reachable()
        .await;
    assert!(
        matches!(result, Err(ProviderError::Network(_))),
        "{result:?}"
    );
}

/// A custom proxy carries every request, even to a local test endpoint.
#[tokio::test]
async fn a_custom_proxy_carries_the_requests() {
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/testzone/"))
        .and(header("AccessKey", "secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&proxy)
        .await;
    // Nothing answers at this address: only the proxy can.
    let storage = site(
        "http://bunny.invalid",
        &TransferSettings {
            proxy: ProxyMode::Custom(proxy.uri()),
            ..TransferSettings::default()
        },
    );
    let listing = storage.list(&RemotePath::root()).await.unwrap();
    assert!(listing.entries.is_empty());
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
    // The folder is empty, so its parent is asked whether it exists.
    Mock::given(method("GET"))
        .and(path("/testzone/"))
        .respond_with(listing(&[("x y#", true)]))
        .expect(1)
        .mount(&s)
        .await;
    let dir = RemotePath::parse("x y#").unwrap();
    let l = storage(&s).list(&dir).await.unwrap();
    assert!(l.entries.is_empty());
}

/// A listing answer with `(name, is_directory)` objects, shaped like Bunny's.
fn listing(objects: &[(&str, bool)]) -> ResponseTemplate {
    let objects: Vec<serde_json::Value> = objects
        .iter()
        .map(|(name, is_directory)| {
            serde_json::json!({
                "Guid": "00000000-0000-0000-0000-000000000001",
                "StorageZoneName": "testzone",
                "Path": "/testzone/",
                "ObjectName": name,
                "Length": 0,
                "LastChanged": "2026-11-03T07:49:00.859",
                "IsDirectory": is_directory,
                "Checksum": null,
                "ContentType": "",
                "ReplicatedZones": null,
            })
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(objects)
}

/// Live probe (Task 7): Bunny answers `200 []` for a folder that is not there, exactly as for
/// an empty one. The parent's listing tells them apart: only a folder it shows by that very
/// name exists (names are case-sensitive, and a file of that name is not the folder).
#[tokio::test]
async fn an_empty_answer_is_a_missing_folder_unless_the_parent_lists_it() {
    for (parent, exists) in [
        (vec![("b", true)], true),
        (vec![], false),
        (vec![("B", true)], false),
        (vec![("b", false)], false),
        (vec![("other", true), ("b", false), ("b", true)], true),
    ] {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/testzone/a/b/"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .expect(1)
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/testzone/a/"))
            .respond_with(listing(&parent))
            .expect(1)
            .mount(&s)
            .await;
        let result = storage(&s).list(&rp("a/b")).await;
        if exists {
            assert!(result.unwrap().entries.is_empty(), "{parent:?}");
        } else {
            assert_eq!(result.unwrap_err(), ProviderError::NotFound, "{parent:?}");
        }
    }
}

/// A folder with something in it, and the root, need no second listing.
#[tokio::test]
async fn a_listing_with_entries_or_of_the_root_asks_nothing_more() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/testzone/a/"))
        .respond_with(listing(&[("f.txt", false)]))
        .expect(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/testzone/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&s)
        .await;
    let storage = storage(&s);
    assert_eq!(storage.list(&rp("a")).await.unwrap().entries.len(), 1);
    assert!(
        storage
            .list(&RemotePath::root())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

/// The parent's listing is a listing like any other: its failures are the answer.
#[tokio::test]
async fn a_failing_parent_listing_fails_the_listing() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/testzone/a/b/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/testzone/a/"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&s)
        .await;
    let err = storage(&s).list(&rp("a/b")).await.unwrap_err();
    assert_eq!(
        err,
        ProviderError::Auth {
            hint: AuthHint::Unknown
        }
    );
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
            empty_folders: EmptyFolders::Supported,
            list_concurrency: 5,
            delete_concurrency: 16,
            transfer_concurrency_max: 96,
            presign: false,
            read_only: false,
            cut_uploads_delete_late: true,
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
