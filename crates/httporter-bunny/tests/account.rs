// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Bunny account API against local mock servers; nothing here reaches api.bunny.net.
//! The JSON is shaped like live responses (spec 5.2), with the fields we don't read left in.

use std::time::Duration;

use httporter_bunny::{BunnyAccount, PullZoneInfo, UnusableZone, ZoneInfo};
use httporter_core::{AuthHint, ProviderError};
use serde_json::{Value, json};
use wiremock::matchers::{any, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "acct-key-7f3a9c1e-0b52-4d68-a1c4-5e9d2b8f6a30";

fn account(server: &MockServer) -> BunnyAccount {
    BunnyAccount::with_base_url(KEY.to_owned(), server.uri()).unwrap()
}

/// A storage zone as the account API returns it. The password is `pw-{name}` and the read-only
/// one `ro-{name}`.
fn storage_zone(id: u64, name: &str, region: &str, hostname: Value) -> Value {
    json!({
        "Id": id,
        "UserId": "9c1e1d6e-0000-4a2b-8c53-000000000000",
        "Name": name,
        "Password": format!("pw-{name}"),
        "ReadOnlyPassword": format!("ro-{name}"),
        "DateModified": "2026-10-30T09:12:44.123Z",
        "Deleted": false,
        "StorageUsed": 1_234_567,
        "FilesStored": 42,
        "Region": region,
        "ReplicationRegions": ["DE", "NY"],
        "StorageHostname": hostname,
        // Whatever this lists, the pull zones come from /pullzone.
        "PullZones": [{ "Id": 9001, "Name": "stale-entry" }],
        "Rewrite404To200": false,
        "Custom404FilePath": null,
        "ZoneTier": 0,
    })
}

/// A pull zone; `hostnames` are `(value, is_system)`.
fn pull_zone(
    id: u64,
    name: &str,
    origin_type: u8,
    storage_zone_id: u64,
    hostnames: &[(&str, bool)],
) -> Value {
    let hostnames: Vec<Value> = hostnames
        .iter()
        .enumerate()
        .map(|(i, (value, system))| {
            json!({
                "Id": id * 100 + i as u64,
                "Value": value,
                "ForceSSL": false,
                "IsSystemHostname": system,
                "HasCertificate": true,
            })
        })
        .collect();
    json!({
        "Id": id,
        "Name": name,
        "OriginUrl": "",
        "Enabled": true,
        "OriginType": origin_type,
        "StorageZoneId": storage_zone_id,
        "Hostnames": hostnames,
        "MonthlyBandwidthUsed": 0,
    })
}

/// Serves the two lists to a client that sends [`KEY`].
async fn serve(server: &MockServer, zones: Value, pull_zones: Value) {
    Mock::given(method("GET"))
        .and(path("/storagezone"))
        .and(query_param("page", "0"))
        .and(header("accesskey", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(zones))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pullzone"))
        .and(header("accesskey", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(pull_zones))
        .mount(server)
        .await;
}

fn pz(id: u64, name: &str, hostnames: &[&str]) -> PullZoneInfo {
    PullZoneInfo {
        id,
        name: name.to_owned(),
        hostnames: hostnames.iter().map(|h| (*h).to_owned()).collect(),
    }
}

#[tokio::test]
async fn lists_zones_with_pull_zone_hostnames() {
    let server = MockServer::start().await;
    serve(
        &server,
        json!([
            storage_zone(1, "site-sg", "SG", json!("sg.storage.bunnycdn.com")),
            // No hostname: Frankfurt lives on the bare storage hostname.
            storage_zone(2, "site-de", "DE", json!("")),
            // No hostname at all: the regional one.
            storage_zone(3, "site-syd", "SYD", Value::Null),
        ]),
        json!([
            pull_zone(
                10,
                "site-sg-cdn",
                2,
                1,
                &[("site-sg-cdn.b-cdn.net", true), ("www.example.com", false)]
            ),
            // A URL origin: not a storage zone's.
            pull_zone(11, "elsewhere", 0, 0, &[("elsewhere.b-cdn.net", true)]),
            pull_zone(12, "site-sg-second", 2, 1, &[("cdn.example.org", false)]),
            // Still points at zone 1, but no longer has a storage origin.
            pull_zone(13, "was-storage", 0, 1, &[("was-storage.b-cdn.net", true)]),
            // Another zone's.
            pull_zone(14, "other-zone", 2, 99, &[("other.b-cdn.net", true)]),
            pull_zone(15, "site-syd-cdn", 2, 3, &[]),
            // Without the origin fields (they are optional).
            json!({ "Id": 16, "Name": "bare", "Hostnames": [{ "Value": "bare.b-cdn.net" }] }),
        ]),
    )
    .await;

    let list = account(&server).list_zones().await.unwrap();

    assert!(list.unusable.is_empty(), "{:?}", list.unusable);
    assert_eq!(
        list.zones,
        vec![
            ZoneInfo {
                id: 1,
                name: "site-sg".into(),
                region: "sg".into(),
                host: "sg.storage.bunnycdn.com".into(),
                password: "pw-site-sg".into(),
                read_only_password: "ro-site-sg".into(),
                // In the order the API gave them.
                pull_zones: vec![
                    pz(
                        10,
                        "site-sg-cdn",
                        &["site-sg-cdn.b-cdn.net", "www.example.com"]
                    ),
                    pz(12, "site-sg-second", &["cdn.example.org"]),
                ],
            },
            ZoneInfo {
                id: 2,
                name: "site-de".into(),
                region: "de".into(),
                host: "storage.bunnycdn.com".into(),
                password: "pw-site-de".into(),
                read_only_password: "ro-site-de".into(),
                pull_zones: vec![],
            },
            ZoneInfo {
                id: 3,
                name: "site-syd".into(),
                region: "syd".into(),
                host: "syd.storage.bunnycdn.com".into(),
                password: "pw-site-syd".into(),
                read_only_password: "ro-site-syd".into(),
                pull_zones: vec![pz(15, "site-syd-cdn", &[])],
            },
        ]
    );

    // Zones first, then pull zones; two requests for any number of zones.
    let requests = server.received_requests().await.unwrap();
    let seen: Vec<(String, String)> = requests
        .iter()
        .map(|r| (r.method.to_string(), r.url.path().to_owned()))
        .collect();
    assert_eq!(
        seen,
        [("GET", "/storagezone"), ("GET", "/pullzone")].map(|(m, p)| (m.to_owned(), p.to_owned()))
    );
    assert_eq!(requests[0].url.query(), Some("page=0"));
}

#[tokio::test]
async fn an_account_without_zones_is_an_empty_list() {
    let server = MockServer::start().await;
    serve(&server, json!([]), json!([])).await;

    let list = account(&server).list_zones().await.unwrap();
    assert!(list.zones.is_empty() && list.unusable.is_empty());
    // With no zones there is nothing to look up pull zones for.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn read_only_password_is_optional() {
    let server = MockServer::start().await;
    let mut zone = storage_zone(1, "site", "UK", json!("uk.storage.bunnycdn.com"));
    zone.as_object_mut().unwrap().remove("ReadOnlyPassword");
    serve(&server, json!([zone]), json!([])).await;

    let zones = account(&server).list_zones().await.unwrap().zones;

    assert_eq!(zones[0].password, "pw-site");
    assert_eq!(zones[0].read_only_password, "");
}

#[tokio::test]
async fn account_key_sent_as_accesskey_header_only_to_api_host() {
    let api = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    // A redirect to another host: following it would carry the key there.
    Mock::given(method("GET"))
        .and(path("/storagezone"))
        .respond_with(ResponseTemplate::new(307).insert_header(
            "location",
            format!("{}/storagezone?page=0", elsewhere.uri()),
        ))
        .mount(&api)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&elsewhere)
        .await;

    let err = account(&api).list_zones().await.unwrap_err();

    assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains(KEY));
    let sent = api.received_requests().await.unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].headers.get("accesskey").unwrap(), KEY);
    assert!(
        elsewhere.received_requests().await.unwrap().is_empty(),
        "the redirect was followed"
    );
}

#[tokio::test]
async fn every_request_carries_the_key_in_the_header_and_nowhere_else() {
    let server = MockServer::start().await;
    serve(
        &server,
        json!([storage_zone(1, "site", "DE", json!("storage.bunnycdn.com"))]),
        json!([]),
    )
    .await;

    account(&server).list_zones().await.unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests {
        let keys: Vec<_> = request.headers.get_all("accesskey").iter().collect();
        assert_eq!(keys, [KEY], "{}", request.url.path());
        assert!(!request.url.as_str().contains(KEY));
        assert!(request.body.is_empty());
    }
}

#[tokio::test]
async fn whitespace_around_a_pasted_key_is_ignored() {
    let server = MockServer::start().await;
    serve(&server, json!([]), json!([])).await;
    let account =
        BunnyAccount::with_base_url(format!("  {KEY}\r\n"), server.uri()).expect("a pasted key");

    assert!(account.list_zones().await.unwrap().zones.is_empty());
}

#[test]
fn a_key_that_cannot_be_a_header_is_refused_without_echoing_it() {
    let err = BunnyAccount::new("not\na-valid-key".to_owned()).unwrap_err();

    assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("a-valid-key"));
}

#[tokio::test]
async fn wrong_key_is_auth_unknown() {
    let server = MockServer::start().await;
    // The right key gets its answer; any other gets Bunny's 401.
    serve(&server, json!([]), json!([])).await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"HttpCode":401,"Message":"Authorization has been denied for this request."}"#,
        ))
        .with_priority(10)
        .mount(&server)
        .await;
    let wrong = BunnyAccount::with_base_url("the-wrong-key".into(), server.uri()).unwrap();

    let err = wrong.list_zones().await.unwrap_err();

    assert_eq!(
        err,
        ProviderError::Auth {
            hint: AuthHint::Unknown
        }
    );
    assert!(!err.is_retryable());
    assert!(!format!("{err} {err:?}").contains("the-wrong-key"));
    // The first answer ended it.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn failures_map_like_the_storage_api_ones() {
    let cases = [
        (
            ResponseTemplate::new(403),
            ProviderError::Auth {
                hint: AuthHint::Unknown,
            },
        ),
        (
            ResponseTemplate::new(429).insert_header("retry-after", "7"),
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
        (
            ResponseTemplate::new(503),
            ProviderError::Server {
                status: 503,
                detail: "Service Unavailable".into(),
            },
        ),
    ];
    for (template, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(template)
            .mount(&server)
            .await;

        assert_eq!(account(&server).list_zones().await.unwrap_err(), expected);
    }
}

#[tokio::test]
async fn a_400_is_a_rejected_request_not_a_busy_server() {
    // Bunny's storage API answers 400 when it is overloaded. The account API has no such
    // answer, so retrying a 400 there would only repeat a bad request.
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(400).set_body_string("Unable to process request"))
        .mount(&server)
        .await;

    let err = account(&server).list_zones().await.unwrap_err();

    assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn a_failing_pull_zone_list_fails_the_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storagezone"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!([storage_zone(
                1,
                "site",
                "DE",
                json!("storage.bunnycdn.com")
            )])),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pullzone"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let err = account(&server).list_zones().await.unwrap_err();

    assert!(
        matches!(err, ProviderError::Server { status: 500, .. }),
        "{err:?}"
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn an_unreadable_answer_is_a_permanent_error_that_leaks_nothing() {
    let mut no_password = storage_zone(1, "site", "DE", json!("storage.bunnycdn.com"));
    no_password.as_object_mut().unwrap().remove("Password");
    let cases = [
        ("<html>maintenance</html>".to_owned(), "not JSON"),
        // The shape of a later page (`page > 0`); we only ask for page 0.
        (
            json!({ "Items": [], "HasMoreItems": false }).to_string(),
            "unrecognised",
        ),
        (json!([no_password]).to_string(), "Password"),
    ];
    for (body, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let err = account(&server).list_zones().await.unwrap_err();

        let ProviderError::Permanent(message) = &err else {
            panic!("{err:?}");
        };
        assert!(message.contains(expected), "{message}");
        assert!(
            !message.contains(KEY) && !message.contains("pw-"),
            "{message}"
        );
    }
}

fn unusable(name: &str, reason: &str) -> UnusableZone {
    UnusableZone {
        name: name.to_owned(),
        reason: reason.to_owned(),
    }
}

/// A zone with its password blanked.
fn without_password(id: u64, name: &str) -> Value {
    let mut zone = storage_zone(id, name, "DE", json!("storage.bunnycdn.com"));
    zone["Password"] = json!("");
    zone
}

#[tokio::test]
async fn a_bad_zone_among_good_ones_is_reported_and_the_others_listed() {
    let server = MockServer::start().await;
    serve(
        &server,
        json!([
            storage_zone(1, "good-one", "SG", json!("sg.storage.bunnycdn.com")),
            // A hostname that would send the zone password somewhere else.
            storage_zone(2, "odd-host", "DE", json!("evil.example/path@x")),
            storage_zone(3, "good-two", "DE", json!("")),
        ]),
        json!([
            pull_zone(10, "one-cdn", 2, 1, &[("one.b-cdn.net", true)]),
            // The unusable zone's pull zone is not offered anywhere.
            pull_zone(11, "odd-cdn", 2, 2, &[("odd.b-cdn.net", true)]),
            pull_zone(12, "two-cdn", 2, 3, &[("two.b-cdn.net", true)]),
        ]),
    )
    .await;

    let list = account(&server).list_zones().await.unwrap();

    let usable: Vec<(u64, &str, &[PullZoneInfo])> = list
        .zones
        .iter()
        .map(|z| (z.id, z.host.as_str(), z.pull_zones.as_slice()))
        .collect();
    assert_eq!(
        usable,
        [
            (
                1,
                "sg.storage.bunnycdn.com",
                [pz(10, "one-cdn", &["one.b-cdn.net"])].as_slice()
            ),
            (
                3,
                "storage.bunnycdn.com",
                [pz(12, "two-cdn", &["two.b-cdn.net"])].as_slice()
            ),
        ]
    );
    assert_eq!(
        list.unusable,
        [unusable("odd-host", "invalid storage hostname")]
    );
}

#[tokio::test]
async fn every_kind_of_unusable_zone_is_reported_with_its_reason_and_no_secret() {
    let mut cases = vec![
        (without_password(1, "no-password"), "no password"),
        // Neither a hostname nor a known region to derive one from.
        (
            storage_zone(2, "no-host", "ZZ", Value::Null),
            "unknown region",
        ),
        (
            storage_zone(3, "no-region", "", json!("  ")),
            "unknown region",
        ),
    ];
    // Anything that is not a plain hostname could send the password somewhere else.
    for (i, host) in [
        "evil.example/path@x",
        "https://evil.example",
        "evil.example:8080",
        "user@evil.example",
        "evil.example?x=1",
        ".evil.example",
        "evil example",
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("host-{i}");
        cases.push((
            storage_zone(10 + i as u64, &name, "DE", json!(host)),
            "invalid storage hostname",
        ));
    }
    for (zone, reason) in cases {
        let name = zone["Name"].as_str().unwrap().to_owned();
        let server = MockServer::start().await;
        serve(&server, json!([zone]), json!([])).await;

        let list = account(&server).list_zones().await.unwrap();

        assert!(list.zones.is_empty(), "{name}");
        assert_eq!(list.unusable, [unusable(&name, reason)]);
        let text = format!("{list:?}");
        assert!(
            !text.contains("pw-") && !text.contains("ro-") && !text.contains(KEY),
            "{text}"
        );
    }
}

#[tokio::test]
async fn when_every_zone_is_bad_none_are_listed_and_all_are_reported() {
    let server = MockServer::start().await;
    serve(
        &server,
        json!([
            without_password(1, "first"),
            // Server text is cleaned like any other before it is shown.
            storage_zone(2, "sec\u{202e}ond\n\u{0}zone", "ZZ", Value::Null),
            storage_zone(3, "third", "DE", json!("a/b")),
        ]),
        json!([]),
    )
    .await;

    let list = account(&server).list_zones().await.unwrap();

    assert!(list.zones.is_empty());
    // In the order the API gave them.
    assert_eq!(
        list.unusable,
        [
            unusable("first", "no password"),
            unusable("sec ond zone", "unknown region"),
            unusable("third", "invalid storage hostname"),
        ]
    );
    // With no zone to look pull zones up for, they were not asked for.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_network_error() {
    // A port that was open a moment ago and is not any more.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let account = BunnyAccount::with_base_url(KEY.to_owned(), url).unwrap();

    let err = account.list_zones().await.unwrap_err();

    assert!(matches!(err, ProviderError::Network(_)), "{err:?}");
    assert!(err.is_retryable());
    assert!(!format!("{err}").contains(KEY));
}

#[tokio::test]
async fn debug_never_shows_secrets() {
    let server = MockServer::start().await;
    serve(
        &server,
        json!([storage_zone(
            1,
            "site-sg",
            "SG",
            json!("sg.storage.bunnycdn.com")
        )]),
        json!([pull_zone(
            10,
            "site-sg-cdn",
            2,
            1,
            &[("www.example.com", false)]
        )]),
    )
    .await;
    let account = account(&server);
    let list = account.list_zones().await.unwrap();
    let zone = &list.zones[0];

    for text in [
        format!("{account:?}"),
        format!("{account:#?}"),
        format!("{zone:?}"),
        format!("{zone:#?}"),
        format!("{list:?}"),
        format!("{list:#?}"),
    ] {
        for secret in [KEY, &zone.password, &zone.read_only_password] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
    }
    // What is left still tells zones apart.
    let text = format!("{zone:?}");
    for shown in [
        "site-sg",
        "sg.storage.bunnycdn.com",
        "www.example.com",
        "<redacted>",
    ] {
        assert!(text.contains(shown), "{shown} missing from {text}");
    }
    assert!(format!("{account:?}").contains("<redacted>"));
}
