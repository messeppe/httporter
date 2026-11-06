// SPDX-License-Identifier: MIT OR Apache-2.0

//! Helpers shared by the HTTP-level tests of the S3 provider: configuration against a mock
//! server, the request signature check ([`Signed`]) and canned responses. Nothing here contacts a
//! real service.
//!
//! Every mock demands a request that a real S3 service would accept as signed ([`Signed`]): the
//! signature is recomputed from the request as it arrived, so a header that was signed but not
//! sent (or sent but not signed), a path or query that differs from the signed one, or a payload
//! hash that does not match the body all fail the test.

// Each test binary uses its own part of this module.
#![allow(dead_code)]

use base64::Engine as _;
use chrono::{NaiveDateTime, Utc};
use httporter_core::{ProxyMode, RemotePath, TransferSettings, UploadOptions};
use httporter_s3::{Credentials, Endpoint, S3Config, S3Storage, SignInput, Url, sha256_hex, sign};
use md5::{Digest, Md5};
use sha2::Sha256;
use wiremock::{Match, MockServer, Request, ResponseTemplate};

pub const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
pub const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
pub const TOKEN: &str = "FQoGZXIvYXdzEXAMPLEtoken";
pub const BUCKET: &str = "site";
pub const REGION: &str = "eu-west-1";
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

// ---------------------------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------------------------

pub fn creds() -> Credentials {
    Credentials {
        access_key_id: KEY_ID.into(),
        secret_access_key: SECRET.into(),
        session_token: None,
    }
}

pub fn temporary_creds() -> Credentials {
    Credentials {
        session_token: Some(TOKEN.into()),
        ..creds()
    }
}

pub fn endpoint(server: &MockServer) -> Endpoint {
    Endpoint {
        base: Url::parse(&server.uri()).unwrap(),
        path_style: true,
        region: REGION.into(),
    }
}

/// Settings that never reach for the system proxy: the mock server is on this machine.
pub fn settings() -> TransferSettings {
    TransferSettings {
        proxy: ProxyMode::None,
        ..TransferSettings::default()
    }
}

pub fn config(server: &MockServer) -> S3Config {
    S3Config::for_site(BUCKET, endpoint(server), creds(), &settings())
}

pub fn provider(server: &MockServer) -> S3Storage {
    S3Storage::new(config(server)).unwrap()
}

pub fn p(text: &str) -> RemotePath {
    RemotePath::parse(text).unwrap()
}

pub fn md5_of(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

pub fn sha256_of(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------------------------
// A server that checks signatures
// ---------------------------------------------------------------------------------------------

/// Accepts a request only if its SigV4 `Authorization` header is valid for [`KEY_ID`],
/// [`SECRET`] and [`REGION`] when recomputed from the request as received.
pub struct Signed {
    creds: Credentials,
}

impl Signed {
    pub fn new() -> Self {
        Self { creds: creds() }
    }

    pub fn with_token() -> Self {
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

pub fn header_of<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.headers.get(name).and_then(|v| v.to_str().ok())
}

pub fn verify(request: &Request, creds: &Credentials) -> Result<(), String> {
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
pub struct ContentMd5;

impl Match for ContentMd5 {
    fn matches(&self, request: &Request) -> bool {
        header_of(request, "content-md5") == Some(b64(&md5_of(&request.body)).as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------------------------

pub fn xml(body: impl Into<String>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/xml")
        .set_body_string(body.into())
}

pub fn s3_error(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("content-type", "application/xml")
        .set_body_string(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code>\
             <Message>{message}</Message><RequestId>4442587FB7D0A2F9</RequestId></Error>"
        ))
}

pub fn opts() -> UploadOptions {
    UploadOptions::new("application/octet-stream")
}
