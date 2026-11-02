//! HTTP-level tests of the S3 provider against a local mock server. Nothing here contacts a real
//! service.
//!
//! Every mock demands a request that a real S3 service would accept as signed ([`Signed`]): the
//! signature is recomputed from the request as it arrived, so a header that was signed but not
//! sent (or sent but not signed), a path or query that differs from the signed one, or a payload
//! hash that does not match the body all fail the test.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{NaiveDateTime, Utc};
use futures_util::StreamExt;
use httporter_core::{
    AuthHint, BodyMeter, ChecksumKind, EmptyFolders, Entry, HttpProtocol, MAX_RETRY_AFTER,
    ProviderError, ProxyMode, RemotePath, StorageProvider, TransferSettings, UploadOptions,
    UploadSource,
};
use httporter_s3::{Credentials, Endpoint, S3Config, S3Storage, SignInput, Url, sha256_hex, sign};
use md5::{Digest, Md5};
use sha2::Sha256;
use wiremock::matchers::{body_bytes, header, method, path, query_param, query_param_is_missing};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FQoGZXIvYXdzEXAMPLEtoken";
const BUCKET: &str = "site";
const REGION: &str = "eu-west-1";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

// ---------------------------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------------------------

fn creds() -> Credentials {
    Credentials {
        access_key_id: KEY_ID.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
    }
}

fn temporary_creds() -> Credentials {
    Credentials {
        session_token: Some(TOKEN.into()),
        ..creds()
    }
}

fn endpoint(server: &MockServer) -> Endpoint {
    Endpoint {
        base: Url::parse(&server.uri()).unwrap(),
        path_style: true,
        region: REGION.into(),
    }
}

/// Settings that never reach for the system proxy: the mock server is on this machine.
fn settings() -> TransferSettings {
    TransferSettings {
        proxy: ProxyMode::None,
        ..TransferSettings::default()
    }
}

fn config(server: &MockServer) -> S3Config {
    S3Config::for_site(BUCKET, endpoint(server), creds(), &settings())
}

fn provider(server: &MockServer) -> S3Storage {
    S3Storage::new(config(server)).unwrap()
}

fn p(text: &str) -> RemotePath {
    RemotePath::parse(text).unwrap()
}

fn md5_of(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

fn sha256_of(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------------------------
// A server that checks signatures
// ---------------------------------------------------------------------------------------------

/// Accepts a request only if its SigV4 `Authorization` header is valid for [`KEY_ID`],
/// [`SECRET`] and [`REGION`] when recomputed from the request as received.
struct Signed {
    creds: Credentials,
}

impl Signed {
    fn new() -> Self {
        Self { creds: creds() }
    }

    fn with_token() -> Self {
        Self {
            creds: temporary_creds(),
        }
    }
}

impl Match for Signed {
    fn matches(&self, request: &Request) -> bool {
        match verify(request, &self.creds) {
            Ok(()) => true,
            Err(why) => {
                eprintln!(
                    "{} {}: not validly signed: {why}",
                    request.method, request.url
                );
                false
            }
        }
    }
}

fn header_of<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).and_then(|v| v.to_str().ok())
}

fn verify(request: &Request, creds: &Credentials) -> Result<(), String> {
    let auth = header_of(request, "authorization").ok_or("no authorization header")?;
    let rest = auth
        .strip_prefix("AWS4-HMAC-SHA256 Credential=")
        .ok_or("not an AWS4-HMAC-SHA256 header")?;
    let (credential, rest) = rest
        .split_once(", SignedHeaders=")
        .ok_or("no SignedHeaders")?;
    let (signed, signature) = rest.split_once(", Signature=").ok_or("no Signature")?;
    if signature.len() != 64 || !signature.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("the signature is not 64 hex digits".into());
    }

    let stamp = header_of(request, "x-amz-date").ok_or("no x-amz-date")?;
    let when = NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ")
        .map_err(|e| format!("bad x-amz-date: {e}"))?
        .and_utc();
    if (Utc::now() - when).num_seconds().abs() > 300 {
        return Err("x-amz-date is not now".into());
    }
    let scope = format!("{KEY_ID}/{}/{REGION}/s3/aws4_request", &stamp[..8]);
    if credential != scope {
        return Err(format!(
            "credential scope is {credential}, expected {scope}"
        ));
    }

    let signed_names: Vec<&str> = signed.split(';').collect();
    for required in ["host", "x-amz-date", "x-amz-content-sha256"] {
        if !signed_names.contains(&required) {
            return Err(format!("{required} is not signed"));
        }
    }
    // Everything the signature names was sent; what a service insists on is signed.
    let mut caller_headers = Vec::new();
    for name in &signed_names {
        let value = header_of(request, name).ok_or(format!("{name} is signed but not sent"))?;
        if ![
            "host",
            "x-amz-date",
            "x-amz-content-sha256",
            "x-amz-security-token",
        ]
        .contains(name)
        {
            caller_headers.push(((*name).to_owned(), value.to_owned()));
        }
    }
    for name in request.headers.keys() {
        let name = name.as_str();
        let must_be_signed =
            name == "content-md5" || name == "content-type" || name.starts_with("x-amz-");
        if must_be_signed && !signed_names.contains(&name) {
            return Err(format!("{name} is sent but not signed"));
        }
    }
    match (
        &creds.session_token,
        header_of(request, "x-amz-security-token"),
    ) {
        (Some(token), Some(sent)) if token == sent => {}
        (None, None) => {}
        _ => return Err("the session token is not what it should be".into()),
    }

    let payload = header_of(request, "x-amz-content-sha256").ok_or("no x-amz-content-sha256")?;
    if payload != "UNSIGNED-PAYLOAD" && payload != sha256_hex(&request.body) {
        return Err("x-amz-content-sha256 is not the hash of the body".into());
    }

    let host = header_of(request, "host").ok_or("no host header")?;
    let mut url = format!("http://{host}{}", request.url.path());
    if let Some(query) = request.url.query() {
        url.push('?');
        url.push_str(query);
    }
    let url = Url::parse(&url).map_err(|e| e.to_string())?;
    let input = SignInput {
        method: request.method.as_str(),
        url: &url,
        headers: &caller_headers,
        payload_sha256: payload,
    };
    let expected = sign(&input, creds, REGION, "s3", when);
    let expected = &expected
        .iter()
        .find(|(name, _)| name == "authorization")
        .expect("sign returns an authorization header")
        .1;
    if expected != auth {
        return Err("the signature does not match the request".into());
    }
    Ok(())
}

/// `Content-MD5` is the base64 MD5 of the body.
struct ContentMd5;

impl Match for ContentMd5 {
    fn matches(&self, request: &Request) -> bool {
        header_of(request, "content-md5") == Some(b64(&md5_of(&request.body)).as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------------------------

fn xml(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/xml")
        .set_body_string(body.into())
}

fn s3_error(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("content-type", "application/xml")
        .set_body_string(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code>\
             <Message>{message}</Message><RequestId>4442587FB7D0A2F9</RequestId></Error>"
        ))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn contents(key: &str, size: u64, etag: &str) -> String {
    format!(
        "<Contents><Key>{}</Key><LastModified>2024-05-06T07:08:09.000Z</LastModified>\
         <ETag>&quot;{etag}&quot;</ETag><Size>{size}</Size><StorageClass>STANDARD</StorageClass>\
         </Contents>",
        escape(key)
    )
}

fn common_prefix(prefix: &str) -> String {
    format!(
        "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
        escape(prefix)
    )
}

fn listing(parts: &[String], next_token: Option<&str>) -> String {
    let token = next_token
        .map(|t| format!("<NextContinuationToken>{t}</NextContinuationToken>"))
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>{BUCKET}</Name><Delimiter>/</Delimiter><MaxKeys>1000</MaxKeys>\
         <IsTruncated>{}</IsTruncated>{token}{}</ListBucketResult>",
        next_token.is_some(),
        parts.concat()
    )
}

fn root_listing(parts: &[String]) -> String {
    listing(parts, None)
}

const GOOD_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";

/// A listing mock for the folder `prefix` (empty for the root).
fn list_mock(prefix: &str, response: ResponseTemplate) -> Mock {
    let mock = Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("list-type", "2"))
        .and(query_param("delimiter", "/"))
        .and(Signed::new());
    if prefix.is_empty() {
        mock.and(query_param_is_missing("prefix"))
    } else {
        mock.and(query_param("prefix", prefix))
    }
    .and(query_param_is_missing("continuation-token"))
    .respond_with(response)
}

async fn entries(storage: &S3Storage, dir: &str) -> Vec<Entry> {
    storage.list(&p(dir)).await.unwrap().entries
}

fn named(entries: &[Entry]) -> Vec<(&str, bool)> {
    let mut named: Vec<_> = entries
        .iter()
        .map(|e| (e.path.as_str(), e.is_folder))
        .collect();
    named.sort_unstable();
    named
}

/// Counts what the meter is told.
#[derive(Default)]
struct CountingMeter {
    calls: AtomicUsize,
    bytes: AtomicU64,
}

#[async_trait]
impl BodyMeter for CountingMeter {
    async fn chunk(&self, len: usize) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.bytes.fetch_add(len as u64, Ordering::SeqCst);
    }
}

fn opts() -> UploadOptions {
    UploadOptions::new("application/octet-stream")
}

fn bytes(data: &[u8]) -> UploadSource {
    UploadSource::Bytes(data.to_vec().into())
}

// ---------------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn list_pages_through_continuation_tokens() {
    let server = MockServer::start().await;
    let page = |first: usize, count: usize| -> Vec<String> {
        (first..first + count)
            .map(|i| contents(&format!("file-{i:04}.txt"), i as u64, GOOD_MD5))
            .collect()
    };
    let paged = |token: Option<&'static str>| {
        Mock::given(method("GET"))
            .and(path(format!("/{BUCKET}")))
            .and(query_param("list-type", "2"))
            .and(query_param("delimiter", "/"))
            .and(Signed::new())
            .and(move |request: &Request| {
                request
                    .url
                    .query_pairs()
                    .find(|(name, _)| name == "continuation-token")
                    .map(|(_, value)| value.into_owned())
                    .as_deref()
                    == token
            })
    };
    paged(None)
        .respond_with(xml(listing(&page(0, 1000), Some("token one/+="))))
        .expect(1)
        .mount(&server)
        .await;
    paged(Some("token one/+="))
        .respond_with(xml(listing(&page(1000, 1000), Some("t2"))))
        .expect(1)
        .mount(&server)
        .await;
    paged(Some("t2"))
        .respond_with(xml(root_listing(&page(2000, 1))))
        .expect(1)
        .mount(&server)
        .await;

    let storage = provider(&server);
    let listing = storage.list(&RemotePath::root()).await.unwrap();
    assert_eq!(listing.entries.len(), 2001);
    assert_eq!(listing.entries[0].path, p("file-0000.txt"));
    assert_eq!(listing.entries[2000].path, p("file-2000.txt"));
    assert_eq!(listing.entries[1500].size, 1500);
    assert_eq!(listing.info.http_version, "HTTP/1.1");
}

#[tokio::test]
async fn common_prefixes_become_folders_and_own_marker_is_hidden() {
    let server = MockServer::start().await;
    list_mock(
        "docs/",
        xml(root_listing(&[
            contents("docs/", 0, GOOD_MD5),
            contents("docs/readme.md", 42, GOOD_MD5),
            common_prefix("docs/img/"),
            common_prefix("docs/zh 文档/"),
        ])),
    )
    .expect(1)
    .mount(&server)
    .await;

    let found = entries(&provider(&server), "docs").await;
    assert_eq!(
        named(&found),
        [
            ("docs/img", true),
            ("docs/readme.md", false),
            ("docs/zh 文档", true)
        ]
    );
    let readme = found.iter().find(|e| !e.is_folder).unwrap();
    assert_eq!(readme.size, 42);
    assert_eq!(readme.name(), "readme.md");
    assert_eq!(
        readme.modified.unwrap().to_rfc3339(),
        "2024-05-06T07:08:09+00:00"
    );
    let folder = found.iter().find(|e| e.is_folder).unwrap();
    assert_eq!(folder.size, 0);
    assert_eq!(folder.checksum, None);
}

#[tokio::test]
async fn etag_without_dash_is_md5_with_dash_is_multipart() {
    let server = MockServer::start().await;
    let plain = "9E107D9D372BB6826BD81D3542A419D6"; // upper case in the listing, stored lower
    let multipart = "d41d8cd98f00b204e9800998ecf8427e-3";
    let sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709"; // B2 may report a SHA-1
    list_mock(
        "",
        xml(root_listing(&[
            contents("plain.txt", 1, plain),
            contents("multi.bin", 2, multipart),
            contents("sha1.bin", 3, sha1),
            contents("kms.bin", 4, "opaque-kms-value"),
            contents("empty-etag.bin", 5, ""),
        ])),
    )
    .expect(1)
    .mount(&server)
    .await;

    let found = entries(&provider(&server), "").await;
    let checksum = |name: &str| {
        found
            .iter()
            .find(|e| e.name() == name)
            .unwrap()
            .checksum
            .clone()
    };
    let plain = checksum("plain.txt").unwrap();
    assert_eq!(plain.kind, ChecksumKind::Md5);
    assert_eq!(plain.hex, "9e107d9d372bb6826bd81d3542a419d6");
    let multi = checksum("multi.bin").unwrap();
    assert_eq!(multi.kind, ChecksumKind::MultipartEtag);
    assert_eq!(multi.hex, multipart);
    // Only 32 hex digits are an MD5; anything else says nothing about the content.
    assert_eq!(checksum("sha1.bin"), None);
    assert_eq!(checksum("kms.bin"), None);
    assert_eq!(checksum("empty-etag.bin"), None);
}

#[tokio::test]
async fn keys_remote_path_cannot_hold_are_skipped_and_counted() {
    let server = MockServer::start().await;
    // Keys: "ok.txt", "..", "/lead.txt", "./x", "real/y", "a//b" (seen from the root and
    // from `a/`).
    list_mock(
        "",
        xml(root_listing(&[
            contents("ok.txt", 1, GOOD_MD5),
            contents("..", 1, GOOD_MD5),
            common_prefix("/"),
            common_prefix("./"),
            common_prefix("real/"),
            common_prefix("a/"),
        ])),
    )
    .expect(1)
    .mount(&server)
    .await;
    list_mock(
        "a/",
        xml(root_listing(&[
            contents("a/fine.txt", 1, GOOD_MD5),
            common_prefix("a//"),
        ])),
    )
    .expect(1)
    .mount(&server)
    .await;

    let storage = provider(&server);
    assert_eq!(storage.skipped_keys(), 0);
    let root = entries(&storage, "").await;
    assert_eq!(
        named(&root),
        [("a", true), ("ok.txt", false), ("real", true)]
    );
    // "..", "/" and "./" cannot be addressed.
    assert_eq!(storage.skipped_keys(), 3);
    let a = entries(&storage, "a").await;
    assert_eq!(named(&a), [("a/fine.txt", false)]);
    // The count is cumulative: "a//" is one more.
    assert_eq!(storage.skipped_keys(), 4);
}

#[tokio::test]
async fn white_space_in_keys_is_kept() {
    let server = MockServer::start().await;
    list_mock(
        "",
        xml(root_listing(&[
            contents(" lead.txt", 1, GOOD_MD5),
            contents("trail.txt ", 1, GOOD_MD5),
            contents("in  side.txt", 1, GOOD_MD5),
        ])),
    )
    .expect(1)
    .mount(&server)
    .await;
    let found = entries(&provider(&server), "").await;
    assert_eq!(
        named(&found),
        [
            (" lead.txt", false),
            ("in  side.txt", false),
            ("trail.txt ", false)
        ]
    );
}

#[tokio::test]
async fn encoded_keys_are_decoded_only_when_the_service_says_it_encoded_them() {
    let server = MockServer::start().await;
    // The request asks for URL-encoded keys (control characters cannot be written in XML).
    let encoded = "<EncodingType>url</EncodingType>";
    let body = root_listing(&[
        contents("my+file+%C3%BC.txt", 1, GOOD_MD5), // "my file ü.txt"
        contents("a%2Bb.txt", 1, GOOD_MD5),          // "a+b.txt"
        contents("100%25.txt", 1, GOOD_MD5),         // "100%.txt"
        contents("bad%FF.txt", 1, GOOD_MD5),         // not UTF-8
        common_prefix("caf%C3%A9/"),                 // "café"
    ])
    .replace("<IsTruncated>", &format!("{encoded}<IsTruncated>"));
    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("encoding-type", "url"))
        .and(Signed::new())
        .respond_with(xml(body))
        .expect(1)
        .mount(&server)
        .await;
    let storage = provider(&server);
    let found = entries(&storage, "").await;
    assert_eq!(
        named(&found),
        [
            ("100%.txt", false),
            ("a+b.txt", false),
            ("café", true),
            ("my file ü.txt", false)
        ]
    );
    assert_eq!(storage.skipped_keys(), 1, "the key that is not UTF-8");

    // A service that ignores `encoding-type` returns the keys as they are, `+` and `%` included.
    let plain = MockServer::start().await;
    list_mock(
        "",
        xml(root_listing(&[
            contents("a+b.txt", 1, GOOD_MD5),
            contents("100%25.txt", 1, GOOD_MD5),
        ])),
    )
    .expect(1)
    .mount(&plain)
    .await;
    let found = entries(&provider(&plain), "").await;
    assert_eq!(named(&found), [("100%25.txt", false), ("a+b.txt", false)]);
}

#[tokio::test]
async fn a_missing_folder_is_not_found_but_the_root_may_be_empty() {
    let server = MockServer::start().await;
    list_mock("", xml(root_listing(&[])))
        .expect(1)
        .mount(&server)
        .await;
    list_mock("gone/", xml(root_listing(&[])))
        .expect(1)
        .mount(&server)
        .await;
    // Only its marker: an empty folder that exists.
    list_mock(
        "empty/",
        xml(root_listing(&[contents("empty/", 0, GOOD_MD5)])),
    )
    .expect(1)
    .mount(&server)
    .await;
    let storage = provider(&server);
    assert!(entries(&storage, "").await.is_empty());
    assert_eq!(
        storage.list(&p("gone")).await.map(|l| l.entries.len()),
        Err(ProviderError::NotFound)
    );
    assert!(entries(&storage, "empty").await.is_empty());
}

#[tokio::test]
async fn a_listing_with_temporary_credentials_sends_the_session_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .and(header("x-amz-security-token", TOKEN))
        .and(Signed::with_token())
        .respond_with(xml(root_listing(&[contents("a.txt", 1, GOOD_MD5)])))
        .expect(1)
        .mount(&server)
        .await;
    let storage = S3Storage::new(S3Config::for_site(
        BUCKET,
        endpoint(&server),
        temporary_creds(),
        &settings(),
    ))
    .unwrap();
    assert_eq!(entries(&storage, "").await.len(), 1);
}

#[tokio::test]
async fn a_truncated_listing_without_a_token_is_an_error_not_a_short_listing() {
    let server = MockServer::start().await;
    let body = root_listing(&[contents("a.txt", 1, GOOD_MD5)])
        .replace("<IsTruncated>false", "<IsTruncated>true");
    list_mock("", xml(body)).expect(1).mount(&server).await;
    assert!(matches!(
        provider(&server).list(&RemotePath::root()).await,
        Err(ProviderError::Permanent(m)) if m.contains("continuation")
    ));
}

#[tokio::test]
async fn a_listing_that_repeats_its_token_is_an_error_not_a_loop() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .respond_with(xml(listing(
            &[contents("a.txt", 1, GOOD_MD5)],
            Some("same"),
        )))
        .expect(2)
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server).list(&RemotePath::root()).await,
        Err(ProviderError::Permanent(_))
    ));
}

#[tokio::test]
async fn an_answer_that_is_not_a_listing_is_an_error_not_an_empty_folder() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("<html><body>Please sign in</body></html>"),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server).list(&RemotePath::root()).await,
        Err(ProviderError::Permanent(_))
    ));
}

#[tokio::test]
async fn a_listing_gives_up_after_the_list_timeout() {
    let server = MockServer::start().await;
    list_mock("", xml(root_listing(&[])).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let mut cfg = config(&server);
    cfg.list_timeout = Duration::from_millis(300);
    let storage = S3Storage::new(cfg).unwrap();
    let started = Instant::now();
    let result = storage.list(&RemotePath::root()).await;
    assert!(
        matches!(result, Err(ProviderError::Network(_))),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn nothing_but_listing_buckets_works_without_a_bucket() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(Signed::new())
        .respond_with(xml(
            "<ListAllMyBucketsResult><Buckets><Bucket><Name>one</Name></Bucket></Buckets>\
             </ListAllMyBucketsResult>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let storage = S3Storage::new(S3Config::for_site(
        "",
        endpoint(&server),
        creds(),
        &settings(),
    ))
    .unwrap();
    assert_eq!(storage.list_buckets().await.unwrap(), ["one"]);
    for result in [
        storage.list(&RemotePath::root()).await.map(|_| ()),
        storage.upload(&p("a"), bytes(b"x"), &opts()).await,
        storage.delete_file(&p("a")).await,
        storage.create_folder(&p("a")).await,
        storage.reachable().await,
    ] {
        assert!(
            matches!(result, Err(ProviderError::Permanent(ref m)) if m.contains("bucket")),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn list_buckets_parses_names() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(Signed::new())
        .respond_with(xml(
            "<?xml version=\"1.0\"?><ListAllMyBucketsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Owner><ID>abc</ID><DisplayName>me</DisplayName></Owner><Buckets>\
             <Bucket><Name>alpha</Name><CreationDate>2020-01-01T00:00:00.000Z</CreationDate></Bucket>\
             <Bucket><Name>beta.example.com</Name><CreationDate>2021-01-01T00:00:00.000Z</CreationDate></Bucket>\
             </Buckets></ListAllMyBucketsResult>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        provider(&server).list_buckets().await.unwrap(),
        ["alpha", "beta.example.com"]
    );

    let none = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(xml(
            "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>",
        ))
        .mount(&none)
        .await;
    assert!(provider(&none).list_buckets().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Uploads and downloads
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn upload_sends_content_type_md5_and_sha256_meta() {
    let server = MockServer::start().await;
    let data = b"<html><body>hello</body></html>";
    let sha = sha256_of(data);
    Mock::given(method("PUT"))
        .and(path("/site/blog/caf%C3%A9%20menu.html"))
        .and(header("content-type", "text/html"))
        .and(header("content-md5", b64(&md5_of(data)).as_str()))
        .and(header(
            "x-amz-meta-httporter-sha256",
            hex::encode(sha).as_str(),
        ))
        .and(header("x-amz-content-sha256", "UNSIGNED-PAYLOAD"))
        .and(header("content-length", data.len().to_string().as_str()))
        .and(body_bytes(data.to_vec()))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"abc\""))
        .expect(1)
        .mount(&server)
        .await;

    let options = UploadOptions {
        sha256: Some(sha),
        md5: Some(md5_of(data)),
        ..UploadOptions::new("text/html")
    };
    provider(&server)
        .upload(&p("blog/café menu.html"), bytes(data), &options)
        .await
        .unwrap();
}

#[tokio::test]
async fn upload_without_digests_sends_neither_header() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/site/a.bin"))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server)
        .upload(&p("a.bin"), bytes(b"abc"), &opts())
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("content-md5").is_none());
    assert!(
        requests[0]
            .headers
            .get("x-amz-meta-httporter-sha256")
            .is_none()
    );
    assert_eq!(
        header_of(&requests[0], "content-type"),
        Some("application/octet-stream")
    );
}

#[tokio::test]
async fn upload_of_an_empty_file_sends_a_zero_length_body() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/site/.nojekyll"))
        .and(header("content-length", "0"))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let meter = Arc::new(CountingMeter::default());
    let options = UploadOptions {
        meter: Some(meter.clone()),
        ..opts()
    };
    provider(&server)
        .upload(&p(".nojekyll"), bytes(b""), &options)
        .await
        .unwrap();
    assert_eq!(meter.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn upload_streams_a_file_through_the_meter() {
    let server = MockServer::start().await;
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.bin");
    std::fs::write(&file, &data).unwrap();
    Mock::given(method("PUT"))
        .and(path("/site/assets/big.bin"))
        .and(body_bytes(data.clone()))
        .and(header("content-length", "200000"))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let meter = Arc::new(CountingMeter::default());
    let options = UploadOptions {
        meter: Some(meter.clone()),
        ..opts()
    };
    provider(&server)
        .upload(
            &p("assets/big.bin"),
            UploadSource::File {
                path: file,
                len: data.len() as u64,
            },
            &options,
        )
        .await
        .unwrap();
    assert_eq!(meter.bytes.load(Ordering::SeqCst), 200_000);
    assert!(meter.calls.load(Ordering::SeqCst) >= 4, "64 KiB pieces");
}

#[tokio::test]
async fn upload_of_a_file_that_changed_is_permanent_and_sends_nothing() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.txt");
    std::fs::write(&file, b"12345").unwrap();
    let storage = provider(&server);
    let result = storage
        .upload(
            &p("f.txt"),
            UploadSource::File { path: file, len: 9 },
            &opts(),
        )
        .await;
    assert!(
        matches!(result, Err(ProviderError::Permanent(_))),
        "{result:?}"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn upload_bad_digest_is_checksum_mismatch() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(Signed::new())
        .respond_with(s3_error(
            400,
            "BadDigest",
            "The Content-MD5 you specified did not match what we received.",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let options = UploadOptions {
        md5: Some(md5_of(b"abc")),
        ..opts()
    };
    assert_eq!(
        provider(&server)
            .upload(&p("a.txt"), bytes(b"abd"), &options)
            .await,
        Err(ProviderError::ChecksumMismatch)
    );
}

#[tokio::test]
async fn an_upload_that_stops_moving_is_a_network_error() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let mut cfg = config(&server);
    cfg.transfer_idle_timeout = Duration::from_millis(400);
    let storage = S3Storage::new(cfg).unwrap();
    let started = Instant::now();
    let result = storage.upload(&p("a.txt"), bytes(b"abc"), &opts()).await;
    assert!(
        matches!(&result, Err(ProviderError::Network(m)) if m.contains("stalled")),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn download_streams_body() {
    let server = MockServer::start().await;
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
    Mock::given(method("GET"))
        .and(path("/site/media/clip%20one.mp4"))
        .and(header("x-amz-content-sha256", EMPTY_SHA256))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .expect(1)
        .mount(&server)
        .await;

    let mut stream = provider(&server)
        .download(&p("media/clip one.mp4"))
        .await
        .unwrap();
    let mut received = Vec::new();
    while let Some(chunk) = stream.next().await {
        received.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(received, data);
}

#[tokio::test]
async fn download_of_a_missing_key_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(s3_error(
            404,
            "NoSuchKey",
            "The specified key does not exist.",
        ))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        provider(&server).download(&p("nope.txt")).await,
        Err(ProviderError::NotFound)
    ));
}

#[tokio::test]
async fn files_are_not_confused_with_the_bucket_root() {
    let server = MockServer::start().await;
    let storage = provider(&server);
    let root = RemotePath::root();
    assert!(matches!(
        storage.upload(&root, bytes(b"x"), &opts()).await,
        Err(ProviderError::Refused(_))
    ));
    assert!(matches!(
        storage.download(&root).await,
        Err(ProviderError::Refused(_))
    ));
    assert!(matches!(
        storage.delete_file(&root).await,
        Err(ProviderError::Refused(_))
    ));
    assert!(matches!(
        storage.delete_folder(&root).await,
        Err(ProviderError::Refused(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Deletes and folders
// ---------------------------------------------------------------------------------------------

/// The `<Key>` texts of a `DeleteObjects` body, undone from XML escapes the test knows about.
fn delete_keys(request: &Request) -> Vec<String> {
    let body = std::str::from_utf8(&request.body).unwrap();
    body.split("<Key>")
        .skip(1)
        .map(|part| {
            let key = part.split("</Key>").next().unwrap();
            key.replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&")
        })
        .collect()
}

fn delete_mock() -> wiremock::MockBuilder {
    Mock::given(method("POST"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("delete", ""))
        .and(header("content-type", "application/xml"))
        .and(ContentMd5)
        .and(Signed::new())
}

#[tokio::test]
async fn delete_file_sends_one_delete() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/site/old/a%20b.txt"))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server)
        .delete_file(&p("old/a b.txt"))
        .await
        .unwrap();
}

#[tokio::test]
async fn delete_batch_posts_one_xml_per_1000_keys_with_content_md5() {
    let server = MockServer::start().await;
    delete_mock()
        .respond_with(xml("<DeleteResult/>"))
        .expect(3)
        .mount(&server)
        .await;
    let paths: Vec<RemotePath> = (0..2500)
        .map(|i| p(&format!("dir/file-{i:04}.txt")))
        .collect();

    let results = provider(&server).delete_batch(&paths).await;
    assert_eq!(results.len(), 2500);
    assert!(results.iter().all(Result::is_ok));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let mut sent: Vec<Vec<String>> = requests.iter().map(delete_keys).collect();
    sent.sort_by_key(|keys| std::cmp::Reverse(keys.len()));
    assert_eq!(
        sent.iter().map(Vec::len).collect::<Vec<_>>(),
        [1000, 1000, 500]
    );
    let mut all: Vec<String> = sent.concat();
    all.sort();
    let mut expected: Vec<String> = paths.iter().map(|p| p.as_str().to_owned()).collect();
    expected.sort();
    assert_eq!(all, expected);
    for request in &requests {
        let body = std::str::from_utf8(&request.body).unwrap();
        assert!(body.contains("<Quiet>true</Quiet>"));
        assert_eq!(
            header_of(request, "content-md5").unwrap(),
            b64(&md5_of(&request.body))
        );
    }
}

#[tokio::test]
async fn delete_batch_with_nothing_to_delete_sends_nothing() {
    let server = MockServer::start().await;
    assert!(provider(&server).delete_batch(&[]).await.is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn delete_batch_maps_per_key_errors() {
    let server = MockServer::start().await;
    delete_mock()
        .respond_with(xml(
            "<DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Deleted><Key>a.txt</Key></Deleted>\
             <Error><Key>b.txt</Key><Code>AccessDenied</Code><Message>Access Denied</Message></Error>\
             <Error><Key>c.txt</Key><Code>InternalError</Code><Message>We encountered an internal error.</Message></Error>\
             <Error><Key>d.txt</Key><Code>NoSuchBucket</Code><Message>x</Message></Error>\
             <Error><Key>e.txt</Key><Code>SlowDown</Code><Message>x</Message></Error>\
             <Error><Key>f.txt</Key><Code>Odd</Code><Message>something odd</Message></Error>\
             </DeleteResult>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let names = [
        "a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt", "g.txt",
    ];
    let paths: Vec<RemotePath> = names.iter().map(|n| p(n)).collect();
    let results = provider(&server).delete_batch(&paths).await;
    assert_eq!(
        results,
        [
            Ok(()),
            Err(ProviderError::Auth {
                hint: AuthHint::Unknown
            }),
            Err(ProviderError::Server {
                status: 500,
                detail: "InternalError".into()
            }),
            Err(ProviderError::NotFound),
            Err(ProviderError::RateLimited { retry_after: None }),
            Err(ProviderError::Permanent("Odd: something odd".into())),
            Ok(()),
        ]
    );
}

#[tokio::test]
async fn delete_batch_failing_as_a_whole_fails_every_key_of_that_request_only() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    delete_mock()
        .respond_with(move |_: &Request| {
            // The first request is refused, the second one goes through.
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                s3_error(403, "InvalidAccessKeyId", "The key does not exist")
            } else {
                xml("<DeleteResult/>")
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let paths: Vec<RemotePath> = (0..1500).map(|i| p(&format!("f{i}"))).collect();
    let results = provider(&server).delete_batch(&paths).await;
    assert_eq!(results.len(), 1500);
    let failed = results.iter().filter(|r| r.is_err()).count();
    let denied = ProviderError::Auth {
        hint: AuthHint::AccessKey,
    };
    assert!(
        failed == 1000 || failed == 500,
        "exactly one request's keys fail, not {failed}"
    );
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| *e == denied)
    );
}

#[tokio::test]
async fn delete_batch_with_an_unnamed_error_fails_the_keys_it_did_not_clear() {
    let server = MockServer::start().await;
    delete_mock()
        .respond_with(xml(
            "<DeleteResult><Error><Code>InternalError</Code><Message>x</Message></Error>\
             <Error><Key>b</Key><Code>AccessDenied</Code></Error></DeleteResult>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let results = provider(&server).delete_batch(&[p("a"), p("b")]).await;
    assert_eq!(
        results,
        [
            Err(ProviderError::Server {
                status: 500,
                detail: "InternalError".into()
            }),
            Err(ProviderError::Auth {
                hint: AuthHint::Unknown
            }),
        ]
    );
}

#[tokio::test]
async fn delete_batch_escapes_keys_and_deletes_the_unwritable_ones_alone() {
    let server = MockServer::start().await;
    delete_mock()
        .respond_with(xml("<DeleteResult/>"))
        .expect(1)
        .mount(&server)
        .await;
    // A key with a control character cannot be written in XML 1.0: it is deleted on its own.
    Mock::given(method("DELETE"))
        .and(path("/site/ctl%01.txt"))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let paths = [p("q&a/<b>.txt"), p("ctl\u{1}.txt"), p("plain.txt")];
    let results = provider(&server).delete_batch(&paths).await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    let requests = server.received_requests().await.unwrap();
    let batch = requests
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .unwrap();
    assert_eq!(delete_keys(batch), ["q&a/<b>.txt", "plain.txt"]);
}

#[tokio::test]
async fn delete_folder_walks_then_batch_deletes_including_marker() {
    let server = MockServer::start().await;
    list_mock(
        "site/",
        xml(root_listing(&[
            contents("site/", 0, GOOD_MD5),
            contents("site/index.html", 10, GOOD_MD5),
            common_prefix("site/css/"),
        ])),
    )
    .expect(1)
    .mount(&server)
    .await;
    list_mock(
        "site/css/",
        xml(root_listing(&[
            contents("site/css/", 0, GOOD_MD5),
            contents("site/css/a.css", 5, GOOD_MD5),
            common_prefix("site/css/fonts/"),
        ])),
    )
    .expect(1)
    .mount(&server)
    .await;
    list_mock(
        "site/css/fonts/",
        xml(root_listing(&[contents(
            "site/css/fonts/f.woff2",
            5,
            GOOD_MD5,
        )])),
    )
    .expect(1)
    .mount(&server)
    .await;
    delete_mock()
        .respond_with(xml("<DeleteResult/>"))
        .expect(1)
        .mount(&server)
        .await;

    provider(&server).delete_folder(&p("site")).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    let batch = requests
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .unwrap();
    let mut keys = delete_keys(batch);
    keys.sort();
    // The files, the markers of the folders below (there may or may not be one), and the
    // folder's own marker.
    assert_eq!(
        keys,
        [
            "site/",
            "site/css/",
            "site/css/a.css",
            "site/css/fonts/",
            "site/css/fonts/f.woff2",
            "site/index.html",
        ]
    );
}

#[tokio::test]
async fn delete_folder_reports_a_key_that_could_not_be_deleted() {
    let server = MockServer::start().await;
    list_mock(
        "locked/",
        xml(root_listing(&[contents("locked/a.txt", 1, GOOD_MD5)])),
    )
    .expect(1)
    .mount(&server)
    .await;
    delete_mock()
        .respond_with(xml(
            "<DeleteResult><Error><Key>locked/a.txt</Key><Code>AccessDenied</Code>\
             <Message>Object is locked</Message></Error></DeleteResult>",
        ))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        provider(&server).delete_folder(&p("locked")).await,
        Err(ProviderError::Auth {
            hint: AuthHint::Unknown
        })
    );
}

#[tokio::test]
async fn create_folder_puts_zero_byte_marker() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/site/photos/2024/"))
        .and(header("content-length", "0"))
        .and(header("x-amz-content-sha256", EMPTY_SHA256))
        .and(body_bytes(Vec::new()))
        .and(Signed::new())
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let storage = provider(&server);
    storage.create_folder(&p("photos/2024")).await.unwrap();
    // The root exists already: nothing is sent for it.
    storage.create_folder(&RemotePath::root()).await.unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------------------------

/// What `list(root)` returns when the service answers with `response`.
async fn list_error(response: ResponseTemplate) -> ProviderError {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(response)
        .expect(1) // one attempt, never a retry
        .mount(&server)
        .await;
    provider(&server)
        .list(&RemotePath::root())
        .await
        .map(|_| ())
        .unwrap_err()
}

#[tokio::test]
async fn auth_errors_name_the_field() {
    let auth = |hint| ProviderError::Auth { hint };
    let cases = [
        (403, "InvalidAccessKeyId", AuthHint::AccessKey),
        (403, "SignatureDoesNotMatch", AuthHint::Secret),
        (400, "AuthorizationHeaderMalformed", AuthHint::Region),
        (403, "AccessDenied", AuthHint::Unknown),
        (403, "ExpiredToken", AuthHint::Unknown),
        (301, "PermanentRedirect", AuthHint::Region),
    ];
    for (status, code, hint) in cases {
        let response = s3_error(status, code, "The server explains at length.");
        assert_eq!(list_error(response).await, auth(hint), "{code}");
    }
    // A bare 403 (no document at all) says nothing about why.
    assert_eq!(
        list_error(ResponseTemplate::new(403)).await,
        auth(AuthHint::Unknown)
    );
}

#[tokio::test]
async fn request_time_too_skewed_is_clock_skew() {
    let response = s3_error(
        403,
        "RequestTimeTooSkewed",
        "The difference between the request time and the current time is too large.",
    );
    assert_eq!(list_error(response).await, ProviderError::ClockSkew);
}

#[tokio::test]
async fn slow_down_and_503_are_rate_limited() {
    let limited = |seconds: Option<u64>| ProviderError::RateLimited {
        retry_after: seconds.map(Duration::from_secs),
    };
    let slow_down = || s3_error(503, "SlowDown", "Please reduce your request rate.");
    // Retry-After is honoured up to 300 s (ledger 120).
    assert_eq!(
        list_error(slow_down().insert_header("retry-after", "7")).await,
        limited(Some(7))
    );
    assert_eq!(
        list_error(slow_down().insert_header("retry-after", "300")).await,
        limited(Some(300))
    );
    assert_eq!(
        list_error(slow_down().insert_header("retry-after", "3600")).await,
        limited(Some(300))
    );
    assert_eq!(MAX_RETRY_AFTER, Duration::from_secs(300));
    assert_eq!(
        list_error(slow_down().insert_header("retry-after", "99999999999999999999999")).await,
        limited(Some(300))
    );
    assert_eq!(list_error(slow_down()).await, limited(None));
    assert_eq!(
        list_error(slow_down().insert_header("retry-after", "soon")).await,
        limited(None)
    );
    // A bare 503 (an overloaded proxy, a 503 with an HTML page) is the same thing.
    assert_eq!(
        list_error(ResponseTemplate::new(503).insert_header("retry-after", "9")).await,
        limited(Some(9))
    );
    assert_eq!(
        list_error(
            ResponseTemplate::new(503)
                .set_body_string("<html><body>Service Unavailable</body></html>")
        )
        .await,
        limited(None)
    );
    // So is a 429, whoever sends it.
    assert_eq!(
        list_error(ResponseTemplate::new(429).insert_header("retry-after", "2")).await,
        limited(Some(2))
    );
    // The code decides, whatever the status.
    assert_eq!(
        list_error(s3_error(500, "SlowDown", "x")).await,
        limited(None)
    );
    assert!(limited(None).is_slow_down() && limited(None).is_retryable());
}

#[tokio::test]
async fn no_such_bucket_is_not_found() {
    let response = s3_error(404, "NoSuchBucket", "The specified bucket does not exist");
    assert_eq!(list_error(response).await, ProviderError::NotFound);
    assert_eq!(
        list_error(ResponseTemplate::new(404)).await,
        ProviderError::NotFound
    );
}

#[tokio::test]
async fn other_failures_keep_their_class() {
    assert_eq!(
        list_error(s3_error(
            500,
            "InternalError",
            "We encountered an internal error."
        ))
        .await,
        ProviderError::Server {
            status: 500,
            detail: "InternalError".into()
        }
    );
    assert_eq!(
        list_error(ResponseTemplate::new(502).set_body_string("<html>Bad gateway</html>")).await,
        ProviderError::Server {
            status: 502,
            detail: "Bad Gateway".into()
        }
    );
    assert!(matches!(
        list_error(s3_error(
            400,
            "RequestTimeout",
            "Your socket connection timed out"
        ))
        .await,
        ProviderError::Network(_)
    ));
    assert_eq!(
        list_error(s3_error(
            400,
            "InvalidBucketName",
            "The specified bucket is not valid."
        ))
        .await,
        ProviderError::Permanent("InvalidBucketName: The specified bucket is not valid.".into())
    );
    // Retried calls are the caller's business, and these are worth retrying.
    for error in [
        list_error(ResponseTemplate::new(500)).await,
        list_error(ResponseTemplate::new(504)).await,
    ] {
        assert!(error.is_retryable(), "{error:?}");
    }
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/elsewhere"))
        .respond_with(xml(root_listing(&[])))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/elsewhere"))
        .expect(1)
        .mount(&server)
        .await;
    let error = provider(&server)
        .list(&RemotePath::root())
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Permanent(m) if m.contains("redirect")));
}

#[tokio::test]
async fn an_upload_is_attempted_once() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(s3_error(503, "SlowDown", "Slow down"))
        .expect(1)
        .mount(&server)
        .await;
    let result = provider(&server)
        .upload(&p("a.txt"), bytes(b"abc"), &opts())
        .await;
    assert_eq!(
        result,
        Err(ProviderError::RateLimited { retry_after: None })
    );
}

#[tokio::test]
async fn a_connection_that_cannot_be_made_is_a_network_error() {
    // A port nothing listens on: bind, note the address, close.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let ep = Endpoint {
        base: Url::parse(&format!("http://{addr}")).unwrap(),
        path_style: true,
        region: REGION.into(),
    };
    let storage = S3Storage::new(S3Config::for_site(BUCKET, ep, creds(), &settings())).unwrap();
    let error = storage.list(&RemotePath::root()).await.unwrap_err();
    assert!(matches!(error, ProviderError::Network(_)), "{error:?}");
    assert!(error.is_retryable());
}

#[tokio::test]
async fn nothing_secret_appears_in_errors_or_debug_output() {
    let server = MockServer::start().await;
    // A service that echoes the key and the secret back in its error document.
    Mock::given(method("GET"))
        .respond_with(s3_error(
            403,
            "SignatureDoesNotMatch",
            &format!("The signature for {KEY_ID} with {SECRET} and {TOKEN} is wrong"),
        ))
        .mount(&server)
        .await;
    let mut cfg = S3Config::for_site(
        BUCKET,
        endpoint(&server),
        temporary_creds(),
        &TransferSettings {
            proxy: ProxyMode::None,
            ..TransferSettings::default()
        },
    );
    let shown = format!("{cfg:?}");
    assert!(
        shown.contains(KEY_ID),
        "the key id is not a secret: {shown}"
    );
    assert!(!shown.contains(SECRET), "{shown}");
    assert!(!shown.contains(TOKEN), "{shown}");
    cfg.proxy = ProxyMode::Custom("http://user:hunter2@127.0.0.1:1".into());
    assert!(!format!("{cfg:?}").contains("hunter2"));
    cfg.proxy = ProxyMode::None;

    let error = S3Storage::new(cfg)
        .unwrap()
        .list(&RemotePath::root())
        .await
        .unwrap_err();
    let shown = format!("{error:?} / {error}");
    assert!(!shown.contains(SECRET) && !shown.contains(TOKEN), "{shown}");
}

// ---------------------------------------------------------------------------------------------
// Reachability, capabilities and configuration
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reachable_asks_for_one_key_and_reports_failures() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/{BUCKET}")))
        .and(query_param("list-type", "2"))
        .and(query_param("max-keys", "1"))
        .and(Signed::new())
        .respond_with(xml(root_listing(&[])))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server).reachable().await.unwrap();

    let refused = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(s3_error(403, "AccessDenied", "Access Denied"))
        .expect(1)
        .mount(&refused)
        .await;
    assert_eq!(
        provider(&refused).reachable().await,
        Err(ProviderError::Auth {
            hint: AuthHint::Unknown
        })
    );
}

#[tokio::test]
async fn capabilities_are_the_s3_contract() {
    let server = MockServer::start().await;
    let storage = provider(&server);
    let caps = storage.capabilities();
    assert_eq!(caps.checksum_in_listing, Some(ChecksumKind::Md5));
    assert!(caps.server_side_copy);
    assert!(!caps.recursive_delete);
    assert_eq!(caps.batch_delete_max, 1000);
    assert_eq!(caps.empty_folders, EmptyFolders::Markers);
    assert_eq!(caps.list_concurrency, 8);
    assert_eq!(caps.delete_concurrency, 8);
    assert_eq!(caps.transfer_concurrency_max, 128);
    assert!(!caps.presign);
    assert!(!caps.read_only);
}

#[tokio::test]
async fn endpoint_shows_where_the_bucket_is() {
    let server = MockServer::start().await;
    assert_eq!(
        provider(&server).endpoint(),
        format!("{}/{BUCKET}", server.uri())
    );
}

#[test]
fn for_site_follows_the_transfer_settings() {
    let ep = Endpoint {
        base: Url::parse("https://s3.eu-west-1.amazonaws.com/").unwrap(),
        path_style: false,
        region: REGION.into(),
    };
    let s = TransferSettings {
        simultaneous: 500,
        protocol: HttpProtocol::Http1Only,
        keep_alive: false,
        request_timeout_secs: 60,
        proxy: ProxyMode::Custom("http://proxy:8080".into()),
        ..TransferSettings::default()
    };
    let cfg = S3Config::for_site("bucket", ep.clone(), creds(), &s);
    assert_eq!(cfg.bucket, "bucket");
    assert_eq!(cfg.endpoint, ep);
    assert_eq!(cfg.creds, creds());
    assert_eq!(cfg.transfer_concurrency, 128, "capped at what S3 allows");
    assert!(cfg.http1_only);
    assert!(!cfg.keep_alive);
    assert_eq!(cfg.request_timeout, Duration::from_secs(60));
    assert_eq!(cfg.list_timeout, Duration::from_secs(60));
    assert_eq!(cfg.transfer_idle_timeout, Duration::from_secs(60));
    assert_eq!(cfg.proxy, ProxyMode::Custom("http://proxy:8080".into()));

    // The accessors apply their own floors: one transfer at least, five seconds at least.
    let tiny = TransferSettings {
        simultaneous: 0,
        request_timeout_secs: 1,
        ..TransferSettings::default()
    };
    let cfg = S3Config::for_site("bucket", ep, creds(), &tiny);
    assert_eq!(cfg.transfer_concurrency, 1);
    // Ruling Q26: the Request timeout setting governs both the short requests and how long a
    // stalled transfer may stay idle, floor included.
    assert_eq!(cfg.request_timeout, Duration::from_secs(5));
    assert_eq!(cfg.transfer_idle_timeout, Duration::from_secs(5));
    assert_eq!(cfg.list_timeout, Duration::from_secs(5));
    assert!(!cfg.http1_only);
    assert!(cfg.keep_alive);
}

#[test]
fn credentials_that_can_never_work_are_refused_up_front() {
    let ep = Endpoint {
        base: Url::parse("https://s3.eu-west-1.amazonaws.com/").unwrap(),
        path_style: false,
        region: REGION.into(),
    };
    let with = |creds: Credentials| {
        S3Storage::new(S3Config::for_site("b", ep.clone(), creds, &settings())).err()
    };
    let auth = |hint| Some(ProviderError::Auth { hint });
    for key in ["", "AKIA EXAMPLE", "AKIA\nEXAMPLE", "AKIAé"] {
        let creds = Credentials {
            access_key_id: key.into(),
            ..creds()
        };
        assert_eq!(with(creds), auth(AuthHint::AccessKey), "{key:?}");
    }
    let no_secret = Credentials {
        secret_access_key: String::new(),
        ..creds()
    };
    assert_eq!(with(no_secret), auth(AuthHint::Secret));
    let bad_token = Credentials {
        session_token: Some("tok\nen".into()),
        ..creds()
    };
    assert_eq!(with(bad_token), auth(AuthHint::Unknown));
    assert_eq!(with(creds()), None);
    assert_eq!(with(temporary_creds()), None);
}

#[test]
fn an_unusable_proxy_is_refused_without_echoing_it() {
    let ep = Endpoint {
        base: Url::parse("https://s3.eu-west-1.amazonaws.com/").unwrap(),
        path_style: false,
        region: REGION.into(),
    };
    let settings = TransferSettings {
        proxy: ProxyMode::Custom("http://user:hunter2@exa mple:80".into()),
        ..TransferSettings::default()
    };
    let Err(error) = S3Storage::new(S3Config::for_site("b", ep, creds(), &settings)) else {
        panic!("a proxy address with a space is not valid");
    };
    assert!(matches!(&error, ProviderError::Permanent(_)));
    assert!(!format!("{error:?}").contains("hunter2"));
}
