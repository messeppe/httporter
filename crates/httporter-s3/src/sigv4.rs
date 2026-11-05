//! AWS Signature Version 4 for S3 and S3-compatible services: header authorization
//! ([`sign`]) and pre-signed links ([`presign_get`]).

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{
    AsciiSet, NON_ALPHANUMERIC, percent_decode, percent_decode_str, percent_encode,
};
use sha2::{Digest, Sha256};
use url::Url;

/// `x-amz-content-sha256` value that tells S3 the body is not covered by the signature.
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

/// SHA-256 of zero bytes, hex: the payload hash of a request without a body.
pub const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// An access key pair, plus the session token when the keys are temporary.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Shows the access key id (it is not a secret and helps in logs) but never the secret key or
/// the session token.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The request to sign.
#[derive(Debug, Clone, Copy)]
pub struct SignInput<'a> {
    /// `GET`, `PUT`, … Signed in upper case, so `"get"` signs like `"GET"`; the caller must send
    /// the verb in upper case, as HTTP clients do.
    pub method: &'a str,
    /// The full request URL, exactly as it will be sent.
    pub url: &'a Url,
    /// The headers the caller will send and wants covered by the signature
    /// (`content-type`, `range`, `content-md5`, `x-amz-*`, …).
    pub headers: &'a [(String, String)],
    /// Lower-case hex SHA-256 of the body, or [`UNSIGNED_PAYLOAD`].
    pub payload_sha256: &'a str,
}

/// Signs a request and returns the headers to add to it, all with lower-case names: `authorization`,
/// `x-amz-date`, `x-amz-content-sha256` (S3 only) and `x-amz-security-token` (temporary
/// credentials only).
///
/// The caller must send the request to exactly `input.url`, with exactly the values in
/// `input.headers` plus the returned headers. The `Host` header is derived from the URL (so a
/// non-default port is signed), and these caller headers are ignored because the signer owns
/// or must not pin them: `host`, `authorization`, `x-amz-date`, `x-amz-content-sha256`,
/// `x-amz-security-token`, and the ones proxies or the HTTP stack rewrite in transit
/// (`user-agent`, `connection`, `keep-alive`, `transfer-encoding`, `te`, `trailer`, `upgrade`,
/// `expect`, `proxy-*`, `x-amzn-trace-id`).
///
/// Canonicalisation is S3's: path segments are percent-encoded once and the path is not
/// normalised. Other services (`iam`, `ec2`, …) sign correctly only when their path is `/`; they
/// expect a double-encoded, normalised path that is not implemented. `x-amz-content-sha256` is
/// an S3 header, so it is added only when `service` is `"s3"`.
pub fn sign(
    input: &SignInput,
    creds: &Credentials,
    region: &str,
    service: &str,
    now: DateTime<Utc>,
) -> Vec<(String, String)> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let day = now.format("%Y%m%d").to_string();

    let mut added = vec![("x-amz-date".to_owned(), amz_date.clone())];
    if service == "s3" {
        added.push((
            "x-amz-content-sha256".to_owned(),
            input.payload_sha256.to_owned(),
        ));
    }
    if let Some(token) = &creds.session_token {
        added.push(("x-amz-security-token".to_owned(), token.clone()));
    }

    let headers = signed_header_map(input, &added);
    let (request, signed_names) = canonical_request(input, &headers);
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        sha256_hex(request.as_bytes())
    );
    let key = signing_key(&creds.secret_access_key, &day, region, service);
    let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));

    let mut out = vec![(
        "authorization".to_owned(),
        format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}",
            creds.access_key_id
        ),
    )];
    out.extend(added);
    out
}

/// The longest a pre-signed link may stay valid: seven days, SigV4's limit.
pub const PRESIGN_MAX_SECS: u64 = 604_800;

/// A pre-signed `GET` link to `url` (an object's address, without a query): the signature
/// travels in the query instead of in headers, so anyone with the link can download the object
/// until `expires_secs` (1 s to [`PRESIGN_MAX_SECS`]) after `now`.
///
/// The query carries `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`,
/// `X-Amz-SignedHeaders=host`, `X-Amz-Security-Token` for temporary credentials, and last
/// `X-Amz-Signature`. Only `host` is signed and the payload is `UNSIGNED-PAYLOAD`, so the link
/// works from any browser. The secret key never appears in it; the access key id does, as it
/// must.
pub fn presign_get(
    url: &Url,
    creds: &Credentials,
    region: &str,
    now: DateTime<Utc>,
    expires_secs: u64,
) -> Url {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let day = now.format("%Y%m%d").to_string();
    let scope = format!("{day}/{region}/s3/aws4_request");
    let mut params = vec![
        ("X-Amz-Algorithm", ALGORITHM.to_owned()),
        (
            "X-Amz-Credential",
            format!("{}/{scope}", creds.access_key_id),
        ),
        ("X-Amz-Date", amz_date.clone()),
        (
            "X-Amz-Expires",
            expires_secs.clamp(1, PRESIGN_MAX_SECS).to_string(),
        ),
        ("X-Amz-SignedHeaders", "host".to_owned()),
    ];
    if let Some(token) = &creds.session_token {
        params.push(("X-Amz-Security-Token", token.clone()));
    }
    let query = params
        .iter()
        .map(|(name, value)| format!("{name}={}", percent_encode(value.as_bytes(), ENCODE)))
        .collect::<Vec<_>>()
        .join("&");
    let mut out = url.clone();
    out.set_query(Some(&query));

    let input = SignInput {
        method: "GET",
        url: &out,
        headers: &[],
        payload_sha256: UNSIGNED_PAYLOAD,
    };
    let headers = BTreeMap::from([("host".to_owned(), host_header(&out))]);
    let (request, _) = canonical_request(&input, &headers);
    let string_to_sign = format!(
        "{ALGORITHM}\n{amz_date}\n{scope}\n{}",
        sha256_hex(request.as_bytes())
    );
    let key = signing_key(&creds.secret_access_key, &day, region, "s3");
    let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));
    out.set_query(Some(&format!("{query}&X-Amz-Signature={signature}")));
    out
}

/// The canonical URI of a raw (not yet percent-encoded) path: every segment encoded once, the
/// slashes, empty segments and trailing slash kept, and a leading slash added when missing.
/// The empty path is `/`.
pub fn canonical_uri(path: &str) -> String {
    let path = path.strip_prefix('/').unwrap_or(path);
    let mut out = String::with_capacity(path.len() + 1);
    for segment in path.split('/') {
        out.push('/');
        out.extend(percent_encode(segment.as_bytes(), ENCODE));
    }
    out
}

/// Lower-case hex SHA-256 of `bytes`, the form `x-amz-content-sha256` takes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// Everything except the RFC 3986 unreserved characters (`A-Z a-z 0-9 - _ . ~`) is encoded, in
/// upper-case hex: the rule S3 canonicalisation uses for paths and query strings alike.
const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Headers the signer sets itself; copies from the caller are dropped.
const SIGNER_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "x-amz-date",
    "x-amz-content-sha256",
    "x-amz-security-token",
];

/// Headers that proxies, load balancers or the HTTP stack add or rewrite after signing, so
/// pinning them breaks the signature.
const VOLATILE_HEADERS: &[&str] = &[
    "user-agent",
    "x-amzn-trace-id",
    "expect",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

/// The path of `url` as S3 canonicalises it. The `url` crate leaves characters such as `+`, `$`
/// and `&` raw in a path, so each segment is decoded and encoded again; an existing `%20` stays
/// `%20` rather than becoming `%2520`. Works on bytes, so a segment that is not UTF-8 survives.
pub(crate) fn canonical_path(url: &Url) -> String {
    let mut out = String::with_capacity(url.path().len() + 1);
    for (i, segment) in url.path().split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        let decoded: Vec<u8> = percent_decode_str(segment).collect();
        out.extend(percent_encode(&decoded, ENCODE));
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// The query as S3 canonicalises it: names and values decoded (`+` is a space), encoded once,
/// then sorted by name and value. A bare `acl` is `acl=`.
fn canonical_query(url: &Url) -> String {
    let Some(query) = url.query() else {
        return String::new();
    };
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (encode_query_part(name), encode_query_part(value))
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_query_part(raw: &str) -> String {
    let spaced: Vec<u8> = raw
        .bytes()
        .map(|b| if b == b'+' { b' ' } else { b })
        .collect();
    let decoded: Vec<u8> = percent_decode(&spaced).collect();
    percent_encode(&decoded, ENCODE).to_string()
}

/// The `Host` header an HTTP client sends for `url`: the host, plus the port when it is not the
/// scheme's default.
fn host_header(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// `Trim()` and collapse runs of white space to one space.
fn normalise_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The headers that go into the signature: `host`, the caller's headers (minus the ones the
/// signer owns or must not pin) and `added`, by lower-case name. Repeated names join with `,`.
fn signed_header_map(input: &SignInput, added: &[(String, String)]) -> BTreeMap<String, String> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    grouped.insert("host".to_owned(), vec![host_header(input.url)]);
    for (name, value) in input.headers {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty()
            || SIGNER_HEADERS.contains(&name.as_str())
            || VOLATILE_HEADERS.contains(&name.as_str())
        {
            continue;
        }
        grouped
            .entry(name)
            .or_default()
            .push(normalise_value(value));
    }
    for (name, value) in added {
        grouped.insert(name.to_ascii_lowercase(), vec![normalise_value(value)]);
    }
    grouped
        .into_iter()
        .map(|(name, values)| (name, values.join(",")))
        .collect()
}

/// The canonical request and its `SignedHeaders` list.
fn canonical_request(input: &SignInput, headers: &BTreeMap<String, String>) -> (String, String) {
    let signed_names = headers
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(";");
    let mut canonical_headers = String::new();
    for (name, value) in headers {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value);
        canonical_headers.push('\n');
    }
    let request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_names}\n{}",
        input.method.to_ascii_uppercase(),
        canonical_path(input.url),
        canonical_query(input.url),
        input.payload_sha256
    );
    (request, signed_names)
}

fn signing_key(secret: &str, day: &str, region: &str, service: &str) -> Vec<u8> {
    let date_key = hmac_sha256(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    let region_key = hmac_sha256(&date_key, region.as_bytes());
    let service_key = hmac_sha256(&region_key, service.as_bytes());
    hmac_sha256(&service_key, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const AWS_S3_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    fn s3_docs_creds() -> Credentials {
        Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: AWS_S3_SECRET.into(),
            session_token: None,
        }
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// The header names only: the values hold the signature and the session token, which no
    /// assertion message may print.
    fn names(out: &[(String, String)]) -> Vec<&str> {
        out.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn get<'a>(out: &'a [(String, String)], name: &str) -> &'a str {
        out.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no {name} header in {:?}", names(out)))
    }

    fn has(out: &[(String, String)], name: &str) -> bool {
        out.iter().any(|(n, _)| n == name)
    }

    /// Signs one S3 docs example (us-east-1, 2013-05-24) and returns the headers to add.
    fn sign_s3_docs(
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        payload: &str,
    ) -> Vec<(String, String)> {
        let url = Url::parse(url).unwrap();
        let headers = pairs(headers);
        let input = SignInput {
            method,
            url: &url,
            headers: &headers,
            payload_sha256: payload,
        };
        sign(
            &input,
            &s3_docs_creds(),
            "us-east-1",
            "s3",
            at(2013, 5, 24, 0, 0, 0),
        )
    }

    fn authorization(signed_headers: &str, signature: &str) -> String {
        format!(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders={signed_headers}, Signature={signature}"
        )
    }

    /// AWS "Create a signed request" example: IAM ListUsers on 2015-08-30. IAM does not use
    /// `x-amz-content-sha256`, so only the S3 service name adds that header.
    #[test]
    fn aws_iam_example_signature() {
        let url =
            Url::parse("https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08").unwrap();
        let headers = pairs(&[(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        )]);
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let input = SignInput {
            method: "GET",
            url: &url,
            headers: &headers,
            payload_sha256: EMPTY_PAYLOAD_SHA256,
        };
        let out = sign(
            &input,
            &creds,
            "us-east-1",
            "iam",
            at(2015, 8, 30, 12, 36, 0),
        );
        assert_eq!(
            get(&out, "authorization"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
        assert_eq!(get(&out, "x-amz-date"), "20150830T123600Z");
        assert!(!has(&out, "x-amz-content-sha256"));
    }

    /// S3 docs, "Example: GET Object" with a Range header.
    #[test]
    fn s3_get_object_example_signature() {
        let out = sign_s3_docs(
            "GET",
            "https://examplebucket.s3.amazonaws.com/test.txt",
            &[("Range", "bytes=0-9")],
            EMPTY_PAYLOAD_SHA256,
        );
        assert_eq!(
            get(&out, "authorization"),
            authorization(
                "host;range;x-amz-content-sha256;x-amz-date",
                "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
            )
        );
        assert_eq!(get(&out, "x-amz-date"), "20130524T000000Z");
        assert_eq!(get(&out, "x-amz-content-sha256"), EMPTY_PAYLOAD_SHA256);
    }

    /// S3 docs, "Example: PUT Object": a `$` in the key, extra signed headers and a real
    /// payload hash.
    #[test]
    fn s3_put_object_example_signature() {
        let payload = sha256_hex(b"Welcome to Amazon S3.");
        assert_eq!(
            payload,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let out = sign_s3_docs(
            "PUT",
            "https://examplebucket.s3.amazonaws.com/test$file.text",
            &[
                ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ],
            &payload,
        );
        assert_eq!(
            get(&out, "authorization"),
            authorization(
                "date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class",
                "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
            )
        );
    }

    /// S3 docs, "Example: GET Bucket Lifecycle": a subresource query with no value.
    #[test]
    fn s3_subresource_query_example_signature() {
        let out = sign_s3_docs(
            "GET",
            "https://examplebucket.s3.amazonaws.com/?lifecycle",
            &[],
            EMPTY_PAYLOAD_SHA256,
        );
        assert_eq!(
            get(&out, "authorization"),
            authorization(
                "host;x-amz-content-sha256;x-amz-date",
                "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"
            )
        );
    }

    /// S3 docs, "Example: Get Bucket (List Objects)": two query parameters.
    #[test]
    fn s3_list_objects_example_signature() {
        let out = sign_s3_docs(
            "GET",
            "https://examplebucket.s3.amazonaws.com/?max-keys=2&prefix=J",
            &[],
            EMPTY_PAYLOAD_SHA256,
        );
        assert_eq!(
            get(&out, "authorization"),
            authorization(
                "host;x-amz-content-sha256;x-amz-date",
                "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            )
        );
    }

    /// The hash of the canonical request is printed in the S3 docs; checking it pins the
    /// canonical request itself, not just the final signature.
    #[test]
    fn canonical_request_matches_the_s3_docs() {
        let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
        let headers = pairs(&[("Range", "bytes=0-9")]);
        let input = SignInput {
            method: "GET",
            url: &url,
            headers: &headers,
            payload_sha256: EMPTY_PAYLOAD_SHA256,
        };
        let added = [
            ("x-amz-date".to_owned(), "20130524T000000Z".to_owned()),
            (
                "x-amz-content-sha256".to_owned(),
                EMPTY_PAYLOAD_SHA256.to_owned(),
            ),
        ];
        let signed = signed_header_map(&input, &added);
        let (request, names) = canonical_request(&input, &signed);
        assert_eq!(
            request,
            format!(
                "GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\n\
                 x-amz-content-sha256:{EMPTY_PAYLOAD_SHA256}\nx-amz-date:20130524T000000Z\n\n\
                 host;range;x-amz-content-sha256;x-amz-date\n{EMPTY_PAYLOAD_SHA256}"
            )
        );
        assert_eq!(names, "host;range;x-amz-content-sha256;x-amz-date");
        assert_eq!(
            sha256_hex(request.as_bytes()),
            "7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972"
        );
    }

    #[test]
    fn canonical_uri_encodes_each_segment_once() {
        assert_eq!(
            canonical_uri("/b/a b/c#d%e?f+g/ü.txt"),
            "/b/a%20b/c%23d%25e%3Ff%2Bg/%C3%BC.txt"
        );
    }

    #[test]
    fn canonical_uri_keeps_the_shape_of_the_path() {
        assert_eq!(canonical_uri(""), "/");
        assert_eq!(canonical_uri("/"), "/");
        assert_eq!(
            canonical_uri("b/k"),
            "/b/k",
            "a missing leading slash is added"
        );
        assert_eq!(canonical_uri("/b/folder/"), "/b/folder/");
        assert_eq!(canonical_uri("/b//k"), "/b//k", "S3 does not merge slashes");
    }

    /// S3 canonical encoding: every byte except `A-Z a-z 0-9 - _ . ~` is encoded, in upper-case
    /// hex. Also pins that `httporter-core`'s URL encoding (used to build request URLs) agrees.
    #[test]
    fn encoding_matches_the_s3_rules_and_core() {
        for byte in 0u8..=127 {
            let c = char::from(byte);
            if c == '/' {
                continue;
            }
            let expected = if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
                c.to_string()
            } else {
                format!("%{byte:02X}")
            };
            assert_eq!(
                canonical_uri(&format!("/{c}")),
                format!("/{expected}"),
                "{byte}"
            );
            assert_eq!(
                httporter_core::encode_segment(&c.to_string()),
                expected,
                "core, byte {byte}"
            );
        }
        assert_eq!(canonical_uri("/😀"), "/%F0%9F%98%80");
        assert_eq!(httporter_core::encode_segment("😀"), "%F0%9F%98%80");
    }

    #[test]
    fn credentials_debug_hides_the_secret_and_the_token() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "super-secret-key".into(),
            session_token: Some("session-token-value".into()),
        };
        let shown = format!("{creds:?}");
        assert!(shown.contains("AKIDEXAMPLE"), "{shown}");
        assert!(!shown.contains("super-secret-key"), "{shown}");
        assert!(!shown.contains("session-token-value"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        let no_token = Credentials {
            session_token: None,
            ..creds
        };
        assert!(format!("{no_token:?}").contains("session_token: None"));
    }

    #[test]
    fn session_token_is_signed_and_returned() {
        let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
        let input = SignInput {
            method: "GET",
            url: &url,
            headers: &[],
            payload_sha256: EMPTY_PAYLOAD_SHA256,
        };
        let plain = s3_docs_creds();
        let temporary = Credentials {
            session_token: Some("FQoGZXIvYXdzEXAMPLE".into()),
            ..s3_docs_creds()
        };
        let now = at(2013, 5, 24, 0, 0, 0);
        let without = sign(&input, &plain, "us-east-1", "s3", now);
        let with = sign(&input, &temporary, "us-east-1", "s3", now);
        assert!(!has(&without, "x-amz-security-token"));
        // Not assert_eq!: a failure would print the token.
        assert!(
            get(&with, "x-amz-security-token") == "FQoGZXIvYXdzEXAMPLE",
            "the returned session token is not the one given"
        );
        assert!(
            get(&with, "authorization").contains(
                "SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token,"
            ),
            "headers returned: {:?}",
            names(&with)
        );
        assert_ne!(get(&with, "authorization"), get(&without, "authorization"));
    }

    #[test]
    fn unsigned_payload_is_signed_and_sent_verbatim() {
        let out = sign_s3_docs(
            "PUT",
            "https://examplebucket.s3.amazonaws.com/big.bin",
            &[("Content-Type", "application/octet-stream")],
            UNSIGNED_PAYLOAD,
        );
        assert_eq!(get(&out, "x-amz-content-sha256"), "UNSIGNED-PAYLOAD");
        assert_ne!(
            get(&out, "authorization"),
            get(
                &sign_s3_docs(
                    "PUT",
                    "https://examplebucket.s3.amazonaws.com/big.bin",
                    &[("Content-Type", "application/octet-stream")],
                    EMPTY_PAYLOAD_SHA256,
                ),
                "authorization"
            )
        );
    }

    #[test]
    fn returned_headers_are_lower_case_and_in_a_fixed_order() {
        let out = sign_s3_docs(
            "GET",
            "https://examplebucket.s3.amazonaws.com/test.txt",
            &[],
            EMPTY_PAYLOAD_SHA256,
        );
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["authorization", "x-amz-date", "x-amz-content-sha256"]
        );
    }

    fn canonical_for(url: &str, headers: &[(&str, &str)], added: &[(&str, &str)]) -> String {
        let url = Url::parse(url).unwrap();
        let headers = pairs(headers);
        let input = SignInput {
            method: "GET",
            url: &url,
            headers: &headers,
            payload_sha256: EMPTY_PAYLOAD_SHA256,
        };
        let added: Vec<(String, String)> = pairs(added);
        canonical_request(&input, &signed_header_map(&input, &added)).0
    }

    #[test]
    fn header_values_are_trimmed_collapsed_and_duplicates_joined() {
        let request = canonical_for(
            "https://h.example/b/k",
            &[
                ("X-Amz-Meta-Note", "  two   spaces\tand a tab  "),
                ("x-amz-meta-note", "second"),
                ("Content-Type", "text/plain"),
            ],
            &[],
        );
        assert!(
            request.contains(
                "content-type:text/plain\nhost:h.example\nx-amz-meta-note:two spaces and a tab,second\n"
            ),
            "{request}"
        );
    }

    #[test]
    fn headers_the_signer_owns_or_proxies_rewrite_are_not_taken_from_the_caller() {
        let request = canonical_for(
            "https://h.example/b/k",
            &[
                ("Host", "evil.example"),
                ("Authorization", "old"),
                ("User-Agent", "HTTPorter"),
                ("Connection", "keep-alive"),
                ("Transfer-Encoding", "chunked"),
                ("x-amz-date", "19700101T000000Z"),
                ("Range", "bytes=0-9"),
            ],
            &[("x-amz-date", "20130524T000000Z")],
        );
        assert!(
            request.contains(
                "\nhost:h.example\nrange:bytes=0-9\nx-amz-date:20130524T000000Z\n\nhost;range;x-amz-date\n"
            ),
            "{request}"
        );
    }

    #[test]
    fn host_header_carries_a_non_default_port() {
        let request = canonical_for("http://localhost:9000/b/k", &[], &[]);
        assert!(request.contains("\nhost:localhost:9000\n"), "{request}");
        let request = canonical_for("https://h.example:443/b/k", &[], &[]);
        assert!(request.contains("\nhost:h.example\n"), "{request}");
        let request = canonical_for("http://[::1]:9000/b/k", &[], &[]);
        assert!(request.contains("\nhost:[::1]:9000\n"), "{request}");
    }

    #[test]
    fn query_is_decoded_encoded_once_and_sorted() {
        let request = canonical_for(
            "https://h.example/b?prefix=a+b%2Fc&list-type=2&delimiter=%2F&acl&Z=1&a=%C3%BC",
            &[],
            &[],
        );
        assert!(
            request.starts_with(
                "GET\n/b\nZ=1&a=%C3%BC&acl=&delimiter=%2F&list-type=2&prefix=a%20b%2Fc\n"
            ),
            "{request}"
        );
    }

    #[test]
    fn query_values_sort_when_a_name_repeats() {
        let request = canonical_for("https://h.example/b?k=b&k=a&j=z", &[], &[]);
        assert!(request.starts_with("GET\n/b\nj=z&k=a&k=b\n"), "{request}");
    }

    /// The `url` crate leaves `+`, `$`, `&` … raw in a path; S3 wants them encoded, and an
    /// existing `%20` must not become `%2520`.
    #[test]
    fn the_path_is_re_encoded_from_the_url_not_double_encoded() {
        let request = canonical_for("https://h.example/b/a%20b/c+d/$e/%C3%BC", &[], &[]);
        assert!(
            request.starts_with("GET\n/b/a%20b/c%2Bd/%24e/%C3%BC\n"),
            "{request}"
        );
        let request = canonical_for("https://h.example/b/100%25/%FF", &[], &[]);
        assert!(request.starts_with("GET\n/b/100%25/%FF\n"), "{request}");
    }

    #[test]
    fn an_empty_path_is_a_slash() {
        let request = canonical_for("https://h.example", &[], &[]);
        assert!(request.starts_with("GET\n/\n\n"), "{request}");
    }

    #[test]
    fn the_method_is_signed_in_upper_case() {
        let url = "https://examplebucket.s3.amazonaws.com/test.txt";
        for (given, canonical) in [("get", "GET"), ("Put", "PUT"), ("dElEtE", "DELETE")] {
            let lower = sign_s3_docs(given, url, &[], EMPTY_PAYLOAD_SHA256);
            let upper = sign_s3_docs(canonical, url, &[], EMPTY_PAYLOAD_SHA256);
            assert_eq!(
                get(&lower, "authorization"),
                get(&upper, "authorization"),
                "{given}"
            );
        }
        // And the canonical request itself starts with the upper-case verb.
        let url = Url::parse(url).unwrap();
        let input = SignInput {
            method: "get",
            url: &url,
            headers: &[],
            payload_sha256: EMPTY_PAYLOAD_SHA256,
        };
        let request = canonical_request(&input, &signed_header_map(&input, &[])).0;
        assert!(request.starts_with("GET\n/test.txt\n"), "{request}");
    }

    /// A failed lookup reports which headers there were, never their values: they hold the
    /// signature and, with temporary credentials, the session token.
    #[test]
    #[should_panic(
        expected = r#"no x-amz-missing header in ["authorization", "x-amz-date", "x-amz-content-sha256"]"#
    )]
    fn a_missing_header_panic_names_the_headers_and_hides_the_values() {
        let out = sign_s3_docs(
            "GET",
            "https://examplebucket.s3.amazonaws.com/test.txt",
            &[],
            EMPTY_PAYLOAD_SHA256,
        );
        get(&out, "x-amz-missing");
    }

    #[test]
    fn the_signature_changes_with_every_input() {
        let url = Url::parse("https://examplebucket.s3.amazonaws.com/test.txt").unwrap();
        let input = SignInput {
            method: "GET",
            url: &url,
            headers: &[],
            payload_sha256: EMPTY_PAYLOAD_SHA256,
        };
        let creds = s3_docs_creds();
        let now = at(2013, 5, 24, 0, 0, 0);
        let base = sign(&input, &creds, "us-east-1", "s3", now);
        let signature = |out: &[(String, String)]| {
            get(out, "authorization")
                .rsplit("Signature=")
                .next()
                .unwrap()
                .to_owned()
        };
        let base_sig = signature(&base);
        assert_eq!(base_sig.len(), 64);
        assert_eq!(
            base_sig,
            signature(&sign(&input, &creds, "us-east-1", "s3", now))
        );
        let other_region = signature(&sign(&input, &creds, "auto", "s3", now));
        let later = signature(&sign(
            &input,
            &creds,
            "us-east-1",
            "s3",
            at(2013, 5, 24, 0, 0, 1),
        ));
        let put = signature(&sign(
            &SignInput {
                method: "PUT",
                ..input
            },
            &creds,
            "us-east-1",
            "s3",
            now,
        ));
        let other_secret = signature(&sign(
            &input,
            &Credentials {
                secret_access_key: "another".into(),
                ..creds.clone()
            },
            "us-east-1",
            "s3",
            now,
        ));
        for other in [other_region, later, put, other_secret] {
            assert_ne!(base_sig, other);
        }
    }
}
