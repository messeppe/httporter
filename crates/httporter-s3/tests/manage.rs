// SPDX-License-Identifier: MIT OR Apache-2.0

//! The manage calls of the S3 provider (I4 plan T10) against a local mock server: `stat`
//! (HeadObject) and its metadata, `copy` (CopyObject, including a 200 that carries an error),
//! the metadata and `Cache-Control` an upload sends, and pre-signed links, one of them checked
//! against the example in AWS's documentation. Nothing here contacts a real service.

use std::time::Duration;

use chrono::{TimeZone, Utc};
use httporter_core::{
    ChecksumKind, META_PART_SIZE, META_SHA256, ProviderError, RemotePath, StorageProvider,
    UploadOptions, UploadSource,
};
use httporter_s3::{Credentials, PRESIGN_MAX_SECS, Url, presign_get};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

mod common;

use common::*;

fn bytes(data: &[u8]) -> UploadSource {
    UploadSource::Bytes(data.to_vec().into())
}

// ---------------------------------------------------------------------------------------------
// stat (HeadObject)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stat_reads_size_date_etag_type_and_the_httporter_metadata() {
    let server = MockServer::start().await;
    let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    Mock::given(method("HEAD"))
        .and(path("/site/blog/caf%C3%A9.html"))
        .and(Signed::new())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(vec![b'x'; 688])
                .insert_header("content-type", "text/html; charset=utf-8")
                .insert_header("last-modified", "Wed, 28 Oct 2026 10:02:51 GMT")
                .insert_header("etag", "\"9E107D9D372BB6826BD81D3542A419D6\"")
                .insert_header("cache-control", "public, max-age=3600")
                .insert_header(format!("x-amz-meta-{META_SHA256}").as_str(), sha)
                .insert_header(
                    format!("x-amz-meta-{META_PART_SIZE}").as_str(),
                    " 16777216 ",
                )
                .insert_header("x-amz-meta-Author", "someone"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let info = provider(&server).stat(&p("blog/café.html")).await.unwrap();

    assert_eq!(info.entry.path, p("blog/café.html"));
    assert!(!info.entry.is_folder);
    assert_eq!(info.entry.size, 688);
    assert_eq!(
        info.entry.modified,
        Some(Utc.with_ymd_and_hms(2026, 10, 28, 10, 2, 51).unwrap())
    );
    let checksum = info.entry.checksum.unwrap();
    assert_eq!(checksum.kind, ChecksumKind::Md5);
    assert_eq!(checksum.hex, "9e107d9d372bb6826bd81d3542a419d6");
    assert_eq!(
        info.content_type.as_deref(),
        Some("text/html; charset=utf-8")
    );
    // S3 keeps no creation date.
    assert_eq!(info.created, None);
    // Without the prefix, names in lower case, values trimmed, sorted.
    assert_eq!(
        info.metadata,
        [
            ("author".to_owned(), "someone".to_owned()),
            (META_PART_SIZE.to_owned(), "16777216".to_owned()),
            (META_SHA256.to_owned(), sha.to_owned()),
        ]
    );
}

#[tokio::test]
async fn stat_of_a_multipart_object_reports_its_multipart_etag() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/site/big.bin"))
        .and(Signed::new())
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"d41d8cd98f00b204e9800998ecf8427e-5\""),
        )
        .mount(&server)
        .await;

    let info = provider(&server).stat(&p("big.bin")).await.unwrap();
    let checksum = info.entry.checksum.unwrap();
    assert_eq!(checksum.kind, ChecksumKind::MultipartEtag);
    assert!(info.metadata.is_empty());
}

#[tokio::test]
async fn stat_of_a_missing_key_or_the_root_is_not_found() {
    let server = MockServer::start().await;
    // A HEAD answer has no body, so only the status says why.
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let storage = provider(&server);

    assert_eq!(
        storage.stat(&p("gone.txt")).await.unwrap_err(),
        ProviderError::NotFound
    );
    assert_eq!(
        storage.stat(&RemotePath::root()).await.unwrap_err(),
        ProviderError::NotFound
    );
}

// ---------------------------------------------------------------------------------------------
// copy (CopyObject)
// ---------------------------------------------------------------------------------------------

const COPY_RESULT: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CopyObjectResult>\
     <LastModified>2026-11-10T09:00:00.000Z</LastModified>\
     <ETag>\"9e107d9d372bb6826bd81d3542a419d6\"</ETag></CopyObjectResult>";

#[tokio::test]
async fn copy_puts_the_target_naming_the_source_and_keeps_the_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/site/new%20name/caf%C3%A9.html"))
        .and(header("x-amz-copy-source", "/site/old/caf%C3%A9.html"))
        .and(header("x-amz-metadata-directive", "COPY"))
        .and(|request: &Request| request.body.is_empty())
        .and(Signed::new())
        .respond_with(xml(COPY_RESULT))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server)
        .copy(&p("old/café.html"), &p("new name/café.html"))
        .await
        .unwrap();
}

/// S3 may answer 200 and fail while it copies: the `<Error>` in the body is the answer.
#[tokio::test]
async fn a_200_with_an_error_body_is_that_error() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/site/b.txt"))
        .and(Signed::new())
        .respond_with(xml(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>InternalError</Code>\
             <Message>We encountered an internal error. Please try again.</Message>\
             <RequestId>656c76696e6727732072657175657374</RequestId></Error>",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let err = provider(&server)
        .copy(&p("a.txt"), &p("b.txt"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProviderError::Server { status: 500, .. }),
        "{err:?}"
    );
    assert!(err.is_retryable());
}

#[tokio::test]
async fn a_200_with_access_denied_in_the_body_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(Signed::new())
        .respond_with(xml(
            "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
        ))
        .mount(&server)
        .await;

    let err = provider(&server)
        .copy(&p("a.txt"), &p("b.txt"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Refused(_)), "{err:?}");
}

#[tokio::test]
async fn copy_failures_keep_their_meaning() {
    let cases = [
        (
            s3_error(
                400,
                "InvalidRequest",
                "The specified copy source is larger than the maximum allowable size for a copy \
                 source: 5368709120",
            ),
            ProviderError::Permanent(httporter_core::copy::COPY_TOO_LARGE.to_owned()),
        ),
        (
            s3_error(404, "NoSuchKey", "The specified key does not exist."),
            ProviderError::NotFound,
        ),
    ];
    for (answer, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(Signed::new())
            .respond_with(answer)
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            provider(&server)
                .copy(&p("a.bin"), &p("b.bin"))
                .await
                .unwrap_err(),
            expected
        );
    }
}

#[tokio::test]
async fn copy_from_or_to_the_root_is_refused_without_a_request() {
    let server = MockServer::start().await;
    let storage = provider(&server);
    let root = RemotePath::root();
    assert!(matches!(
        storage.copy(&root, &p("b.txt")).await,
        Err(ProviderError::Refused(_))
    ));
    assert!(matches!(
        storage.copy(&p("a.txt"), &root).await,
        Err(ProviderError::Refused(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Upload metadata and Cache-Control
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_upload_sends_its_cache_control_signed_with_the_sha256_meta() {
    let server = MockServer::start().await;
    let data = b"body { color: red }";
    let sha = sha256_of(data);
    Mock::given(method("PUT"))
        .and(path("/site/css/site.css"))
        .and(header("content-type", "text/css"))
        // wiremock's `header` splits values at commas, so the whole value is compared here.
        .and(|request: &Request| {
            header_of(request, "cache-control") == Some("public, max-age=3600")
        })
        .and(header(
            format!("x-amz-meta-{META_SHA256}").as_str(),
            hex::encode(sha).as_str(),
        ))
        .and(|request: &Request| {
            // Cache-Control is no x-amz-* header, but it is signed all the same.
            header_of(request, "authorization").is_some_and(|auth| auth.contains("cache-control"))
        })
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"abc\""))
        .expect(1)
        .mount(&server)
        .await;

    let options = UploadOptions {
        sha256: Some(sha),
        cache_control: Some("  public, max-age=3600 ".into()),
        ..UploadOptions::new("text/css")
    };
    provider(&server)
        .upload(&p("css/site.css"), bytes(data), &options)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_blank_cache_control_is_not_sent() {
    for blank in [None, Some(String::new()), Some("   ".to_owned())] {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(Signed::new())
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let options = UploadOptions {
            cache_control: blank.clone(),
            ..opts()
        };
        provider(&server)
            .upload(&p("a.bin"), bytes(b"abc"), &options)
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests[0].headers.get("cache-control").is_none(),
            "{blank:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Pre-signed links
// ---------------------------------------------------------------------------------------------

/// The example of "Authenticating Requests: Using Query Parameters (AWS Signature Version 4)"
/// in the Amazon S3 API reference: a GET of `examplebucket/test.txt`, valid for 86400 seconds
/// from 2013-05-24T00:00:00Z in us-east-1, with the documentation's example keys.
#[test]
fn presign_matches_the_aws_documented_example() {
    let creds = Credentials {
        access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
        secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        session_token: None,
    };
    let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
    let now = Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap();

    let link = presign_get(&url, &creds, "us-east-1", now, 86_400);

    assert_eq!(
        link.as_str(),
        "https://examplebucket.s3.amazonaws.com/test.txt\
         ?X-Amz-Algorithm=AWS4-HMAC-SHA256\
         &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
         &X-Amz-Date=20130524T000000Z\
         &X-Amz-Expires=86400\
         &X-Amz-SignedHeaders=host\
         &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
    );
}

fn query_of(link: &str) -> Vec<(String, String)> {
    Url::parse(link)
        .unwrap()
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(found, _)| found == name)
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn presign_makes_a_link_to_the_object_without_asking_the_service() {
    let server = MockServer::start().await;
    let storage = provider(&server);

    let link = storage
        .presign(&p("blog/café menu.html"), Duration::from_secs(60))
        .unwrap();

    assert!(
        link.starts_with(&format!(
            "{}/site/blog/caf%C3%A9%20menu.html?",
            server.uri()
        )),
        "{link}"
    );
    let pairs = query_of(&link);
    assert_eq!(param(&pairs, "X-Amz-Algorithm"), Some("AWS4-HMAC-SHA256"));
    assert_eq!(param(&pairs, "X-Amz-Expires"), Some("60"));
    assert_eq!(param(&pairs, "X-Amz-SignedHeaders"), Some("host"));
    let credential = param(&pairs, "X-Amz-Credential").unwrap();
    assert!(
        credential.starts_with(&format!("{KEY_ID}/")),
        "{credential}"
    );
    assert!(credential.ends_with(&format!("/{REGION}/s3/aws4_request")));
    let signature = param(&pairs, "X-Amz-Signature").unwrap();
    assert_eq!(signature.len(), 64);
    assert!(signature.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(pairs.last().unwrap().0, "X-Amz-Signature");
    assert_eq!(param(&pairs, "X-Amz-Security-Token"), None);
    assert!(!link.contains(SECRET));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn presign_clamps_the_lifetime_to_seven_days_and_carries_a_session_token() {
    let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
    let now = Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap();
    let creds = temporary_creds();

    let long = presign_get(&url, &creds, "us-east-1", now, 30 * 86_400);
    let pairs = query_of(long.as_str());
    assert_eq!(
        param(&pairs, "X-Amz-Expires"),
        Some(PRESIGN_MAX_SECS.to_string().as_str())
    );
    assert_eq!(param(&pairs, "X-Amz-Security-Token"), Some(TOKEN));

    let zero = presign_get(&url, &creds, "us-east-1", now, 0);
    assert_eq!(param(&query_of(zero.as_str()), "X-Amz-Expires"), Some("1"));
}

#[tokio::test]
async fn presign_of_the_root_is_refused() {
    let server = MockServer::start().await;
    assert!(matches!(
        provider(&server).presign(&RemotePath::root(), Duration::from_secs(60)),
        Err(ProviderError::Refused(_))
    ));
}
