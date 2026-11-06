// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live checks of the bucket tools (I6, plan T10), on the user's S3 test service. Both are
//! `#[ignore]`d and run only when asked for:
//!
//! ```text
//! cargo test --locked -p httporter-s3 --test live_buckets -- --ignored --nocapture --test-threads 1
//! ```
//!
//! - `cors_set_get_clear_on_the_test_bucket`: on `HTTPORTER_TEST_S3_BUCKET` (which must start
//!   with `httporter-test`), sets a CORS rule, reads it back and clears it; whatever rules the
//!   bucket had before are put back at the end.
//! - `create_and_delete_a_test_bucket`: creates `httporter-test-<unix time>` and deletes it again
//!   (`DELETE` of the empty bucket, signed with this crate's [`sign`]).
//!
//! **Credentials** come from the git-ignored `.env.test`, as for `live.rs`
//! (`HTTPORTER_TEST_S3_PRESET`, `…_ACCOUNT_OR_ENDPOINT`, `…_REGION`, `…_BUCKET`, `…_KEY_ID`,
//! `…_SECRET`), read by this process alone. Nothing printed holds them, nor any URL (the account
//! ID is part of it).

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use futures_util::FutureExt;
use httporter_core::{S3Preset, TransferSettings};
use httporter_s3::{
    CorsRule, Credentials, EMPTY_PAYLOAD_SHA256, Endpoint, S3Config, S3Storage, SignInput,
    bucket_url, endpoint, sign,
};

const PREFIX: &str = "httporter-test";

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

fn required(values: &HashMap<String, String>, name: &str) -> String {
    match values.get(name).map(|v| v.trim()) {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => panic!("{name} is not set in .env.test"),
    }
}

fn assert_test_bucket(bucket: &str) {
    assert!(
        bucket.starts_with(PREFIX),
        "refusing to touch bucket {bucket:?}: live tests only use buckets starting with \
         {PREFIX:?}"
    );
}

fn preset(name: &str) -> S3Preset {
    match name.to_ascii_lowercase().as_str() {
        "r2" | "cloudflare-r2" | "cloudflarer2" => S3Preset::CloudflareR2,
        "aws" | "s3" | "aws-s3" => S3Preset::AwsS3,
        "b2" | "backblaze" => S3Preset::BackblazeB2,
        "wasabi" => S3Preset::Wasabi,
        "spaces" | "digitalocean" => S3Preset::DigitalOceanSpaces,
        "minio" => S3Preset::Minio,
        "other" => S3Preset::Other,
        _ => panic!("HTTPORTER_TEST_S3_PRESET {name:?} is not a known preset"),
    }
}

/// The test bucket's name, the endpoint and the credentials.
fn setup() -> (String, Endpoint, Credentials) {
    let values = test_env();
    let bucket = required(&values, "HTTPORTER_TEST_S3_BUCKET");
    assert_test_bucket(&bucket);
    let preset = preset(&required(&values, "HTTPORTER_TEST_S3_PRESET"));
    let account = required(&values, "HTTPORTER_TEST_S3_ACCOUNT_OR_ENDPOINT");
    let region = values
        .get("HTTPORTER_TEST_S3_REGION")
        .map(|r| r.trim().to_owned())
        .unwrap_or_default();
    let endpoint = endpoint(preset, &region, &account, &account, true)
        .unwrap_or_else(|message| panic!("no endpoint: {message}"));
    let creds = Credentials {
        access_key_id: required(&values, "HTTPORTER_TEST_S3_KEY_ID"),
        secret_access_key: required(&values, "HTTPORTER_TEST_S3_SECRET"),
        session_token: None,
    };
    (bucket, endpoint, creds)
}

fn storage(bucket: &str, endpoint: &Endpoint, creds: &Credentials) -> S3Storage {
    S3Storage::new(S3Config::for_site(
        bucket,
        endpoint.clone(),
        creds.clone(),
        &TransferSettings::default(),
    ))
    .unwrap_or_else(|_| panic!("could not build the provider (credentials unusable)"))
}

/// What a result was, without the text of an error (which may name the endpoint).
fn kind<T>(result: &Result<T, httporter_core::ProviderError>) -> String {
    use httporter_core::ProviderError::*;
    match result {
        Ok(_) => "ok",
        Err(Auth { .. }) => "auth",
        Err(NotFound) => "not found",
        Err(RateLimited { .. }) => "rate limited",
        Err(ChecksumMismatch) => "checksum mismatch",
        Err(Refused(_)) => "refused",
        Err(Network(_)) => "network",
        Err(Server { .. }) => "server",
        Err(ClockSkew) => "clock skew",
        Err(Permanent(_)) => "permanent",
    }
    .to_owned()
}

#[tokio::test]
#[ignore = "live: needs .env.test and the S3 test bucket"]
async fn cors_set_get_clear_on_the_test_bucket() {
    let (bucket, endpoint, creds) = setup();
    let s3 = storage(&bucket, &endpoint, &creds);
    let before = s3.cors().await.unwrap_or_else(|e| panic!("cors: {e:?}"));
    println!("rules before: {}", before.len());

    let steps = AssertUnwindSafe(async {
        let rule = CorsRule {
            origins: vec!["https://httporter-test.example".into()],
            methods: vec!["GET".into(), "HEAD".into()],
            headers: vec!["*".into()],
            max_age: Some(600),
            ..CorsRule::default()
        };
        s3.set_cors(std::slice::from_ref(&rule)).await.unwrap();
        let read = s3.cors().await.unwrap();
        println!("read back: {read:?}");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].origins, rule.origins);
        assert_eq!(read[0].methods, rule.methods);
        s3.set_cors(&[]).await.unwrap();
        assert!(s3.cors().await.unwrap().is_empty());
        println!("website: {}", kind(&s3.website().await));
    })
    .catch_unwind()
    .await;

    let restored = s3.set_cors(&before).await;
    println!("rules restored: {}", kind(&restored));
    if let Err(panic) = steps {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "live: needs .env.test and the S3 test service; creates and deletes a bucket"]
async fn create_and_delete_a_test_bucket() {
    let (_, endpoint, creds) = setup();
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let name = format!("{PREFIX}-{unix}");
    assert_test_bucket(&name);
    let s3 = storage("", &endpoint, &creds);

    let created = s3.create_bucket(&name).await;
    println!("create {name}: {}", kind(&created));
    created.unwrap_or_else(|e| panic!("create_bucket: {e:?}"));
    // Again: one's own bucket is fine.
    println!("create again: {}", kind(&s3.create_bucket(&name).await));

    // `DELETE` the empty bucket, signed as the crate signs.
    assert_test_bucket(&name);
    let url = bucket_url(&endpoint, &name);
    let headers = sign(
        &SignInput {
            method: "DELETE",
            url: &url,
            headers: &[],
            payload_sha256: EMPTY_PAYLOAD_SHA256,
        },
        &creds,
        &endpoint.region,
        "s3",
        Utc::now(),
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut request = client.delete(url.as_str());
    for (header, value) in &headers {
        request = request.header(header.as_str(), value.as_str());
    }
    match request.send().await {
        Ok(response) => println!("delete {name}: {}", response.status()),
        Err(error) => println!("delete {name}: no answer ({})", error.without_url()),
    }
}
