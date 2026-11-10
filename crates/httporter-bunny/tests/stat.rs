// SPDX-License-Identifier: MIT OR Apache-2.0

//! `stat` on Bunny Storage against a local mock server (I4 plan T10): the parent's listing
//! entry with its `DateCreated`, and the copy and presign that Bunny can't do. Nothing here
//! reaches a real storage zone.

use std::time::Duration;

use chrono::{TimeZone, Utc};
use httporter_bunny::{BunnyConfig, BunnyStorage};
use httporter_core::{ProviderError, RemotePath, RetryPolicy, StorageProvider};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn storage(server: &MockServer) -> BunnyStorage {
    let mut cfg = BunnyConfig::new("testzone", "storage.bunnycdn.com", "secret");
    cfg.retry = RetryPolicy::immediate(5);
    cfg.base_url = Some(server.uri());
    BunnyStorage::new(cfg).unwrap()
}

fn rp(text: &str) -> RemotePath {
    RemotePath::parse(text).unwrap()
}

/// A listing element shaped like Bunny's, with the fields we don't read left in.
fn object(name: &str, is_directory: bool, created: Option<&str>) -> Value {
    json!({
        "Guid": "00000000-0000-0000-0000-000000000001",
        "StorageZoneName": "testzone",
        "Path": "/testzone/blog/",
        "ObjectName": name,
        "Length": if is_directory { 0 } else { 688 },
        "Checksum": if is_directory { Value::Null } else {
            json!("B952DEDC8ACB32567C6A1409077036C7D0F371C85A4D75BCD667E4740B75F991")
        },
        "ReplicatedZones": "LA,DE",
        "LastChanged": "2026-10-28T10:02:51.089",
        "ServerId": 0,
        "ArrayNumber": 0,
        "IsDirectory": is_directory,
        "UserId": "00000000-0000-0000-0000-000000000000",
        "ContentType": "text/html",
        "DateCreated": created,
        "StorageZoneId": 1,
    })
}

async fn serve_blog(server: &MockServer, objects: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/testzone/blog/"))
        .and(header("AccessKey", "secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(objects)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn stat_reads_the_parent_listing_and_its_date_created() {
    let server = MockServer::start().await;
    serve_blog(
        &server,
        vec![
            object("other.html", false, Some("2020-01-01T00:00:00")),
            object("index.html", false, Some("2026-09-14T08:30:05.5")),
        ],
    )
    .await;

    let info = storage(&server).stat(&rp("blog/index.html")).await.unwrap();

    assert_eq!(info.entry.path, rp("blog/index.html"));
    assert!(!info.entry.is_folder);
    assert_eq!(info.entry.size, 688);
    assert_eq!(
        info.entry.modified,
        Some(
            Utc.with_ymd_and_hms(2026, 10, 28, 10, 2, 51).unwrap()
                + chrono::Duration::milliseconds(89)
        )
    );
    assert_eq!(
        info.created,
        Some(
            Utc.with_ymd_and_hms(2026, 9, 14, 8, 30, 5).unwrap()
                + chrono::Duration::milliseconds(500)
        )
    );
    assert_eq!(info.content_type.as_deref(), Some("text/html"));
    assert_eq!(info.entry.replicated_regions, ["LA", "DE"]);
    assert!(info.entry.checksum.is_some());
    // Bunny keeps no metadata of its own.
    assert!(info.metadata.is_empty());
}

#[tokio::test]
async fn a_missing_or_unreadable_date_created_is_none() {
    let server = MockServer::start().await;
    serve_blog(
        &server,
        vec![
            object("a.html", false, None),
            object("b.html", false, Some("not a date")),
        ],
    )
    .await;
    let storage = storage(&server);

    assert_eq!(
        storage.stat(&rp("blog/a.html")).await.unwrap().created,
        None
    );
    assert_eq!(
        storage.stat(&rp("blog/b.html")).await.unwrap().created,
        None
    );
}

#[tokio::test]
async fn a_file_is_preferred_to_a_folder_of_the_same_name() {
    let server = MockServer::start().await;
    serve_blog(
        &server,
        vec![
            object("same", true, Some("2026-01-01T00:00:00")),
            object("same", false, Some("2026-02-02T00:00:00")),
        ],
    )
    .await;

    let info = storage(&server).stat(&rp("blog/same")).await.unwrap();
    assert!(!info.entry.is_folder);
    assert_eq!(
        info.created,
        Some(Utc.with_ymd_and_hms(2026, 2, 2, 0, 0, 0).unwrap())
    );
}

#[tokio::test]
async fn a_name_the_parent_does_not_list_and_the_root_are_not_found() {
    let server = MockServer::start().await;
    serve_blog(&server, vec![object("index.html", false, None)]).await;
    let storage = storage(&server);

    assert_eq!(
        storage.stat(&rp("blog/gone.html")).await.unwrap_err(),
        ProviderError::NotFound
    );
    assert_eq!(
        storage.stat(&RemotePath::root()).await.unwrap_err(),
        ProviderError::NotFound
    );
    // The root asked nothing.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn bunny_can_neither_copy_nor_presign_and_sends_nothing_trying() {
    let server = MockServer::start().await;
    let storage = storage(&server);

    let copy = storage.copy(&rp("a.html"), &rp("b.html")).await;
    assert!(matches!(copy, Err(ProviderError::Permanent(_))), "{copy:?}");
    let link = storage.presign(&rp("a.html"), Duration::from_secs(60));
    assert!(matches!(link, Err(ProviderError::Permanent(_))), "{link:?}");
    assert!(!storage.capabilities().presign);
    assert!(server.received_requests().await.unwrap().is_empty());
}
