// SPDX-License-Identifier: MIT OR Apache-2.0

//! The zone tools of the Bunny account API (I6, §8.8.4–8.8.5) against local mock servers:
//! creating a storage zone and a pull zone, reading and changing a zone's settings. Nothing here
//! reaches api.bunny.net, and no zone is created anywhere. The JSON is shaped like the
//! `StorageZoneModel` of Bunny's reference, with fields we don't read left in.

use httporter_bunny::{
    BunnyAccount, NewZone, PULL_ZONE_NAME_TAKEN, ZONE_NAME_TAKEN, ZoneNameProblem, ZoneRef,
    ZoneSettings, pull_zone_name_problem, zone_name_problem,
};
use httporter_core::{AuthHint, ProviderError};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "acct-key-7f3a9c1e-0b52-4d68-a1c4-5e9d2b8f6a30";

fn account(server: &MockServer) -> BunnyAccount {
    BunnyAccount::with_base_url(KEY.to_owned(), server.uri()).unwrap()
}

/// A `StorageZoneModel`; the password is `pw-{name}`, or absent with `password: false`.
fn storage_zone(id: u64, name: &str, region: &str, password: bool) -> Value {
    let mut zone = json!({
        "Id": id,
        "UserId": "9c1e1d6e-0000-4a2b-8c53-000000000000",
        "Name": name,
        "Password": format!("pw-{name}"),
        "ReadOnlyPassword": format!("ro-{name}"),
        "DateModified": "2026-11-06T09:12:44.123Z",
        "Deleted": false,
        "StorageUsed": 0,
        "FilesStored": 0,
        "Region": region,
        "ReplicationRegions": ["DE"],
        "PullZones": [],
        "Rewrite404To200": false,
        "Custom404FilePath": null,
        "StorageHostname": format!("{}.storage.bunnycdn.com", region.to_ascii_lowercase()),
        "S3Hostname": null,
        "ZoneTier": 0,
        "ReplicationChangeInProgress": false,
        "PriceOverride": 0.0,
        "Discount": 0,
        "StorageZoneType": 0,
    });
    if !password {
        zone["Password"] = Value::Null;
    }
    zone
}

fn pull_zone(id: u64, name: &str, storage_zone_id: u64) -> Value {
    json!({
        "Id": id,
        "Name": name,
        "OriginUrl": "",
        "Enabled": true,
        "OriginType": 2,
        "StorageZoneId": storage_zone_id,
        "Hostnames": [
            { "Id": 1, "Value": format!("{name}.b-cdn.net"), "ForceSSL": false,
              "IsSystemHostname": true, "HasCertificate": true },
        ],
    })
}

/// Answers the availability checks for both kinds of zone.
async fn available(server: &MockServer, storage: bool, pull: bool) {
    for (path_, free) in [
        ("/storagezone/checkavailability", storage),
        ("/pullzone/checkavailability", pull),
    ] {
        Mock::given(method("POST"))
            .and(path(path_))
            .and(header("accesskey", KEY))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Available": free })))
            .mount(server)
            .await;
    }
}

fn new_zone(pull_zone: Option<&str>) -> NewZone {
    NewZone {
        name: "httporter-site".into(),
        region: "sg".into(),
        replication: vec!["de".into(), "ny".into(), "sg".into()],
        pull_zone: pull_zone.map(str::to_owned),
    }
}

// ---- Names ----

#[test]
fn zone_names_are_checked_before_anything_is_sent() {
    assert_eq!(zone_name_problem("my-site"), None);
    assert_eq!(zone_name_problem(&"a".repeat(63)), None);
    assert_eq!(
        zone_name_problem("abc"),
        Some(ZoneNameProblem::Length { min: 4, max: 63 })
    );
    assert_eq!(
        zone_name_problem(&"a".repeat(64)),
        Some(ZoneNameProblem::Length { min: 4, max: 63 })
    );
    for bad in ["My-Site", "my_site", "my site", "my.site", "sité"] {
        assert_eq!(
            zone_name_problem(bad),
            Some(ZoneNameProblem::Characters),
            "{bad}"
        );
    }
    assert_eq!(zone_name_problem("-site"), Some(ZoneNameProblem::Hyphen));
    assert_eq!(zone_name_problem("site-"), Some(ZoneNameProblem::Hyphen));

    assert_eq!(pull_zone_name_problem("cdn"), None);
    assert_eq!(
        pull_zone_name_problem(""),
        Some(ZoneNameProblem::Length { min: 1, max: 63 })
    );
    assert_eq!(
        pull_zone_name_problem("my.cdn"),
        Some(ZoneNameProblem::Characters)
    );
}

// ---- Creating ----

#[tokio::test]
async fn create_storage_zone_sends_the_documented_fields() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .and(header("accesskey", KEY))
        .and(header("content-type", "application/json"))
        // Upper-case codes, and the main region left out of the replication regions.
        .and(body_json(json!({
            "Name": "httporter-site",
            "Region": "SG",
            "ReplicationRegions": ["DE", "NY"],
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(storage_zone(
            501,
            "httporter-site",
            "SG",
            true,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let zone = account(&server)
        .create_storage_zone(&new_zone(None))
        .await
        .unwrap();

    assert_eq!(zone.id, 501);
    assert_eq!(zone.name, "httporter-site");
    assert_eq!(zone.region, "sg");
    assert_eq!(zone.host, "sg.storage.bunnycdn.com");
    assert_eq!(zone.password, "pw-httporter-site");
    assert_eq!(zone.read_only_password, "ro-httporter-site");
    assert!(zone.pull_zones.is_empty());
    // The password stays out of the zone's Debug.
    assert!(!format!("{zone:?}").contains("pw-httporter-site"));
}

#[tokio::test]
async fn no_replication_regions_sends_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .and(body_json(
            json!({ "Name": "httporter-site", "Region": "DE" }),
        ))
        .respond_with(ResponseTemplate::new(201).set_body_json(storage_zone(
            7,
            "httporter-site",
            "DE",
            true,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let zone = account(&server)
        .create_storage_zone(&NewZone {
            name: "httporter-site".into(),
            region: "de".into(),
            ..NewZone::default()
        })
        .await
        .unwrap();
    assert_eq!(zone.host, "de.storage.bunnycdn.com");
}

/// When the answer leaves the password out, the zone is read again for it.
#[tokio::test]
async fn a_created_zone_without_its_password_is_read_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .respond_with(ResponseTemplate::new(201).set_body_json(storage_zone(
            88,
            "httporter-site",
            "SG",
            false,
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storagezone/88"))
        .and(header("accesskey", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(storage_zone(
            88,
            "httporter-site",
            "SG",
            true,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let zone = account(&server)
        .create_storage_zone(&new_zone(None))
        .await
        .unwrap();
    assert_eq!(zone.password, "pw-httporter-site");
}

#[tokio::test]
async fn bunnys_own_words_come_back_from_a_rejected_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ErrorKey": "storagezone.validation",
            "Field": "Region",
            "Message": "The region\u{202E} is not\n valid.",
        })))
        .mount(&server)
        .await;

    let err = account(&server)
        .create_storage_zone(&new_zone(None))
        .await
        .unwrap_err();
    // Cleaned of control and bidi characters, and nothing we sent.
    assert_eq!(
        err,
        ProviderError::Refused("The region is not valid.".into())
    );
}

#[tokio::test]
async fn a_rejected_key_and_a_busy_api_are_told_apart() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let err = account(&server)
        .create_storage_zone(&new_zone(None))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ProviderError::Auth {
            hint: AuthHint::Unknown
        }
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_string("{\"HttpCode\":400,\"Message\":\"Unable to process request\"}"),
        )
        .mount(&server)
        .await;
    let err = account(&server)
        .create_storage_zone(&new_zone(None))
        .await
        .unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
}

#[tokio::test]
async fn create_pull_zone_links_the_storage_zone_as_its_origin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/pullzone"))
        .and(header("accesskey", KEY))
        .and(body_json(json!({
            "Name": "httporter-site",
            "OriginType": 2,
            "StorageZoneId": 501,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull_zone(
            9100,
            "httporter-site",
            501,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let pull = account(&server)
        .create_pull_zone("httporter-site", 501)
        .await
        .unwrap();
    assert_eq!(pull.id, 9100);
    assert_eq!(pull.name, "httporter-site");
    assert_eq!(pull.hostnames, ["httporter-site.b-cdn.net"]);
}

/// Bunny's reference gives the 201 no body: the pull zone is then found in the list.
#[tokio::test]
async fn a_pull_zone_answer_without_a_body_is_looked_up() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/pullzone"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/pullzone"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            pull_zone(1, "httporter-site", 999),
            pull_zone(2, "other", 501),
            pull_zone(3, "httporter-site", 501),
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let pull = account(&server)
        .create_pull_zone("httporter-site", 501)
        .await
        .unwrap();
    assert_eq!(pull.id, 3);
}

#[tokio::test]
async fn availability_is_read_leniently() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/storagezone/checkavailability"))
        .and(body_json(json!({ "Name": "taken-name" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Available": false })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/pullzone/checkavailability"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let account = account(&server);

    assert!(!account.zone_name_available("taken-name").await.unwrap());
    // An answer that doesn't say counts as available: creating will say otherwise.
    assert!(account.pull_zone_name_available("anything").await.unwrap());
}

#[tokio::test]
async fn create_zone_makes_both_and_returns_the_zone_with_its_pull_zone() {
    let server = MockServer::start().await;
    available(&server, true, true).await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .respond_with(ResponseTemplate::new(201).set_body_json(storage_zone(
            501,
            "httporter-site",
            "SG",
            true,
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/pullzone"))
        .and(body_json(json!({
            "Name": "httporter-cdn",
            "OriginType": 2,
            "StorageZoneId": 501,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull_zone(
            9100,
            "httporter-cdn",
            501,
        )))
        .expect(1)
        .mount(&server)
        .await;

    let zone = account(&server)
        .create_zone(&new_zone(Some("httporter-cdn")))
        .await
        .unwrap();
    assert_eq!(zone.id, 501);
    assert_eq!(zone.pull_zones.len(), 1);
    assert_eq!(zone.pull_zones[0].hostnames, ["httporter-cdn.b-cdn.net"]);
}

#[tokio::test]
async fn a_name_taken_creates_nothing() {
    for (storage, pull, expected) in [
        (false, true, ZONE_NAME_TAKEN),
        (true, false, PULL_ZONE_NAME_TAKEN),
    ] {
        let server = MockServer::start().await;
        available(&server, storage, pull).await;
        Mock::given(method("POST"))
            .and(path("/storagezone"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/pullzone"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&server)
            .await;

        let err = account(&server)
            .create_zone(&new_zone(Some("httporter-cdn")))
            .await
            .unwrap_err();
        assert_eq!(err, ProviderError::Refused(expected.into()));
    }
}

#[tokio::test]
async fn a_bad_name_is_refused_without_a_request() {
    let server = MockServer::start().await;
    let err = account(&server)
        .create_zone(&NewZone {
            name: "Bad Name".into(),
            region: "de".into(),
            ..NewZone::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Permanent(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_pull_zone_that_fails_after_the_storage_zone_says_so() {
    let server = MockServer::start().await;
    available(&server, true, true).await;
    Mock::given(method("POST"))
        .and(path("/storagezone"))
        .respond_with(ResponseTemplate::new(201).set_body_json(storage_zone(
            501,
            "httporter-site",
            "SG",
            true,
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/pullzone"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ErrorKey": "pullzone.name_taken",
            "Field": "Name",
            "Message": "The pull zone name is already taken.",
        })))
        .mount(&server)
        .await;

    let err = account(&server)
        .create_zone(&new_zone(Some("httporter-cdn")))
        .await
        .unwrap_err();
    let ProviderError::Permanent(message) = err else {
        panic!("{err:?}");
    };
    assert!(message.contains("httporter-site was created"), "{message}");
    assert!(message.contains("already taken"), "{message}");
}

// ---- Settings ----

#[tokio::test]
async fn a_zone_is_found_by_its_exact_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storagezone"))
        .and(query_param("page", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            storage_zone(1, "Site", "DE", true),
            storage_zone(2, "site", "SYD", true),
        ])))
        .mount(&server)
        .await;
    let account = account(&server);

    assert_eq!(
        account.find_zone("site").await.unwrap(),
        ZoneRef {
            id: 2,
            region: "syd".into()
        }
    );
    assert_eq!(
        account.find_zone("missing").await,
        Err(ProviderError::NotFound)
    );
}

#[tokio::test]
async fn zone_settings_are_read() {
    let server = MockServer::start().await;
    let mut zone = storage_zone(501, "httporter-site", "SG", true);
    zone["Custom404FilePath"] = json!(" /404.html ");
    zone["Rewrite404To200"] = json!(true);
    zone["ReplicationRegions"] = json!(["DE", "NY"]);
    Mock::given(method("GET"))
        .and(path("/storagezone/501"))
        .respond_with(ResponseTemplate::new(200).set_body_json(zone))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storagezone/502"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(storage_zone(502, "plain", "DE", true)),
        )
        .mount(&server)
        .await;
    let account = account(&server);

    assert_eq!(
        account.zone_settings(501).await.unwrap(),
        ZoneSettings {
            custom_404_path: Some("/404.html".into()),
            rewrite_404_to_200: true,
            replication: vec!["de".into(), "ny".into()],
        }
    );
    let plain = account.zone_settings(502).await.unwrap();
    assert_eq!(plain.custom_404_path, None);
    assert!(!plain.rewrite_404_to_200);
}

#[tokio::test]
async fn updating_adds_regions_and_never_drops_one() {
    let server = MockServer::start().await;
    let mut zone = storage_zone(501, "httporter-site", "SG", true);
    zone["ReplicationRegions"] = json!(["DE", "NY"]);
    Mock::given(method("GET"))
        .and(path("/storagezone/501"))
        .respond_with(ResponseTemplate::new(200).set_body_json(zone))
        .mount(&server)
        .await;
    // `ny` is left out of what is asked and `sg` is the main region: the zone keeps NY and
    // gets SYD, never SG.
    Mock::given(method("POST"))
        .and(path("/storagezone/501"))
        .and(header("accesskey", KEY))
        .and(body_json(json!({
            "Custom404FilePath": "/404.html",
            "Rewrite404To200": true,
            "ReplicationZones": ["DE", "NY", "SYD"],
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    account(&server)
        .update_zone_settings(
            501,
            &ZoneSettings {
                custom_404_path: Some("/404.html".into()),
                rewrite_404_to_200: true,
                replication: vec!["de".into(), "syd".into(), "sg".into()],
            },
        )
        .await
        .unwrap();
}

/// Without a region to add, the regions aren't sent; an empty 404 path clears it.
#[tokio::test]
async fn updating_without_new_regions_sends_only_the_404_settings() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storagezone/501"))
        .respond_with(ResponseTemplate::new(200).set_body_json(storage_zone(
            501,
            "httporter-site",
            "SG",
            true,
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/storagezone/501"))
        .and(body_json(json!({
            "Custom404FilePath": "",
            "Rewrite404To200": false,
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    account(&server)
        .update_zone_settings(
            501,
            &ZoneSettings {
                custom_404_path: None,
                rewrite_404_to_200: false,
                replication: vec!["de".into()],
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_zone_that_is_gone_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storagezone/404"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    assert_eq!(
        account(&server).zone_settings(404).await,
        Err(ProviderError::NotFound)
    );
}

// ---- Open questions of the I6 plan, checked against Bunny's API reference ----

/// The reference (`PullZoneAddModel.Name`) gives only `minLength: 1`, no pattern; the name
/// becomes the host name label `{name}.b-cdn.net`, where hyphens are allowed inside. The test
/// pull zone itself is called `httporter-test-sg`.
#[tokio::test]
async fn pull_zone_names_may_hold_hyphens_inside() {
    for good in ["httporter-test-sg", "a-b", "a--b", "x1-2-3"] {
        assert_eq!(pull_zone_name_problem(good), None, "{good}");
    }
    assert_eq!(
        pull_zone_name_problem("-cdn"),
        Some(ZoneNameProblem::Hyphen)
    );
    assert_eq!(
        pull_zone_name_problem("cdn-"),
        Some(ZoneNameProblem::Hyphen)
    );

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/pullzone"))
        .and(body_json(json!({
            "Name": "httporter-test-sg",
            "OriginType": 2,
            "StorageZoneId": 501,
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull_zone(
            9101,
            "httporter-test-sg",
            501,
        )))
        .expect(1)
        .mount(&server)
        .await;
    let pull = account(&server)
        .create_pull_zone("httporter-test-sg", 501)
        .await
        .unwrap();
    assert_eq!(pull.hostnames, ["httporter-test-sg.b-cdn.net"]);
}

/// Both checks send `{"Name": …}` (the reference's `ZoneAvailabilityRequestModel`); the
/// reference names no answer fields, and `Available` is what live answers carry.
#[tokio::test]
async fn availability_sends_name_and_reads_available() {
    let server = MockServer::start().await;
    for path_ in [
        "/storagezone/checkavailability",
        "/pullzone/checkavailability",
    ] {
        Mock::given(method("POST"))
            .and(path(path_))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({ "Name": "httporter-test-new" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "Available": true })))
            .expect(1)
            .mount(&server)
            .await;
    }
    let account = account(&server);
    assert!(
        account
            .zone_name_available("httporter-test-new")
            .await
            .unwrap()
    );
    assert!(
        account
            .pull_zone_name_available("httporter-test-new")
            .await
            .unwrap()
    );
}

/// The reference marks `Custom404FilePath` nullable and says nothing about clearing; the code
/// clears it by sending an empty string, also for a path of only white space.
#[tokio::test]
async fn a_blank_404_path_is_sent_as_an_empty_string() {
    let server = MockServer::start().await;
    let mut zone = storage_zone(501, "httporter-site", "SG", true);
    zone["Custom404FilePath"] = json!("/404.html");
    Mock::given(method("GET"))
        .and(path("/storagezone/501"))
        .respond_with(ResponseTemplate::new(200).set_body_json(zone))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/storagezone/501"))
        .and(body_json(json!({
            "Custom404FilePath": "",
            "Rewrite404To200": false,
        })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    account(&server)
        .update_zone_settings(
            501,
            &ZoneSettings {
                custom_404_path: Some("   ".into()),
                rewrite_404_to_200: false,
                replication: Vec::new(),
            },
        )
        .await
        .unwrap();
}
