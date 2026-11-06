// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live check of the zone tools (I6, plan T10): creates a storage zone and a pull zone named
//! `httporter-test-<unix time>`, reads and changes the zone's settings, then deletes both. It is
//! `#[ignore]`d and runs only when asked for:
//!
//! ```text
//! cargo test --locked -p httporter-bunny --test live_zones -- --ignored --nocapture
//! ```
//!
//! **Credentials.** `HTTPORTER_TEST_ACCOUNT_KEY` from the git-ignored `.env.test` in the
//! repository root (found by walking up from this crate), read by this process alone and never
//! printed. Every request names a zone starting with `httporter-test-` and checks it first; the
//! production zone and its pull zone are refused by name and id.
//!
//! **Cleanup.** The pull zone and the storage zone are deleted at the end (`DELETE /pullzone/{id}`,
//! `DELETE /storagezone/{id}`), also when a step fails.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::FutureExt;
use httporter_bunny::{BunnyAccount, NewZone, ZoneSettings};

/// The only names this test creates.
const PREFIX: &str = "httporter-test-";
/// Never touched, whatever the prefix check says.
const PRODUCTION_ZONE: &str = "exampleshop";
const PRODUCTION_PULL_ZONE: u64 = 4_495_693;

/// `HTTPORTER_TEST_*` of the nearest `.env.test` above this crate, parsed here and never loaded
/// into the environment; its errors never quote a line (which may hold a secret).
fn test_env() -> HashMap<String, String> {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let file = loop {
        let candidate = dir.join(".env.test");
        if candidate.is_file() {
            break candidate;
        }
        dir = dir
            .parent()
            .unwrap_or_else(|| panic!("no .env.test in any folder above this crate"));
    };
    let entries =
        dotenvy::from_path_iter(&file).unwrap_or_else(|_| panic!(".env.test could not be opened"));
    let mut values = HashMap::new();
    for entry in entries {
        match entry {
            Ok((key, value)) if key.starts_with("HTTPORTER_TEST_") => {
                values.insert(key, value);
            }
            Ok(_) => {}
            Err(_) => panic!("an entry of .env.test could not be read"),
        }
    }
    values
}

fn assert_test_name(name: &str) {
    assert!(
        name.starts_with(PREFIX) && name != PRODUCTION_ZONE,
        "refusing to touch {name:?}: this test only makes zones starting with {PREFIX:?}"
    );
}

/// `DELETE` on the account API, for the cleanup (the client itself never deletes a zone).
async fn delete(key: &str, path: &str) -> String {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = client
        .delete(format!("https://api.bunny.net{path}"))
        .header("AccessKey", key)
        .send()
        .await;
    match response {
        Ok(response) => response.status().to_string(),
        Err(error) => format!("no answer: {}", error.without_url()),
    }
}

#[tokio::test]
#[ignore = "live: needs .env.test with HTTPORTER_TEST_ACCOUNT_KEY; creates and deletes a zone"]
async fn create_change_and_delete_a_test_zone() {
    let values = test_env();
    let key = values
        .get("HTTPORTER_TEST_ACCOUNT_KEY")
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| panic!("HTTPORTER_TEST_ACCOUNT_KEY is not set in .env.test"));
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let name = format!("{PREFIX}{unix}");
    assert_test_name(&name);
    let account = BunnyAccount::new(key.clone()).unwrap();

    let zone = account
        .create_zone(&NewZone {
            name: name.clone(),
            region: "de".into(),
            replication: Vec::new(),
            pull_zone: Some(name.clone()),
        })
        .await
        .unwrap_or_else(|e| panic!("create_zone: {e}"));
    assert_test_name(&zone.name);
    println!(
        "created zone {} (id {}), host {}, pull zones {:?}",
        zone.name,
        zone.id,
        zone.host,
        zone.pull_zones
            .iter()
            .map(|p| (p.id, p.hostnames.clone()))
            .collect::<Vec<_>>()
    );

    let steps = AssertUnwindSafe(async {
        let found = account.find_zone(&name).await.unwrap();
        assert_eq!(found.id, zone.id);
        let before = account.zone_settings(zone.id).await.unwrap();
        println!("settings at first: {before:?}");
        account
            .update_zone_settings(
                zone.id,
                &ZoneSettings {
                    custom_404_path: Some("/404.html".into()),
                    rewrite_404_to_200: true,
                    replication: Vec::new(),
                },
            )
            .await
            .unwrap();
        let after = account.zone_settings(zone.id).await.unwrap();
        println!("settings after: {after:?}");
        assert_eq!(after.custom_404_path.as_deref(), Some("/404.html"));
        assert!(after.rewrite_404_to_200);
    })
    .catch_unwind()
    .await;

    for pull_zone in &zone.pull_zones {
        assert_ne!(pull_zone.id, PRODUCTION_PULL_ZONE);
        assert_test_name(&pull_zone.name);
        let status = delete(&key, &format!("/pullzone/{}", pull_zone.id)).await;
        println!("deleted pull zone {}: {status}", pull_zone.id);
    }
    assert_test_name(&zone.name);
    let status = delete(&key, &format!("/storagezone/{}", zone.id)).await;
    println!("deleted storage zone {}: {status}", zone.id);

    if let Err(panic) = steps {
        std::panic::resume_unwind(panic);
    }
}
