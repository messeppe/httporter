//! Turns what an S3 service answered, or failed to answer, into the [`ProviderError`] the
//! engine acts on.
//!
//! S3 reports most failures twice: an HTTP status and an XML `<Error>` document whose `<Code>`
//! is the precise reason. The code wins where it is known; the status is the fallback for
//! services that answer with a bare status or a proxy's HTML page.
//!
//! Nothing here echoes more of the server's text than a short, sanitised `Code: Message`. The
//! error documents of the signature failures carry the string that was signed and the access
//! key; those are never copied into a message.

use std::error::Error as _;
use std::time::Duration;

use chrono::{DateTime, Utc};
use httporter_core::{AuthHint, MAX_RETRY_AFTER, ProviderError};
use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};

use crate::xml;

/// How much of an error response body is ever read. S3 errors are a few hundred bytes; the
/// rest of a larger body (an HTML error page, say) tells nothing more.
pub(crate) const ERROR_BODY_MAX: usize = 16 * 1024;

/// The longest `Message` that ends up in an error.
const MESSAGE_MAX: usize = 200;

/// The error for an unsuccessful answer: `status`, the `Retry-After` it carried (already
/// clamped, see [`retry_after`]) and its body (at most [`ERROR_BODY_MAX`] bytes).
pub(crate) fn from_response(
    status: u16,
    retry_after: Option<Duration>,
    body: &[u8],
) -> ProviderError {
    let doc = xml::parse_error(body);
    from_parts(
        status,
        doc.code.as_deref(),
        doc.message.as_deref(),
        retry_after,
    )
}

/// The error for an S3 `Code` and `Message`. `status` is the HTTP status of the answer, or any
/// non-error status (use 200) for an error that came inside a successful answer, such as one
/// key of a `DeleteObjects` request.
pub(crate) fn from_parts(
    status: u16,
    code: Option<&str>,
    message: Option<&str>,
    retry_after: Option<Duration>,
) -> ProviderError {
    code.and_then(|code| by_code(code, status, retry_after))
        .unwrap_or_else(|| by_status(status, code, message, retry_after))
}

/// The meaning of the codes that name a reason precisely; `None` for the rest.
fn by_code(code: &str, status: u16, retry_after: Option<Duration>) -> Option<ProviderError> {
    let auth = |hint| ProviderError::Auth { hint };
    Some(match code {
        "InvalidAccessKeyId" => auth(AuthHint::AccessKey),
        "SignatureDoesNotMatch" => auth(AuthHint::Secret),
        // Signed for another region. `PermanentRedirect` is the same mistake seen from the
        // other side: the bucket lives at another endpoint, so the region or address chosen
        // is wrong, and every later request will fail the same way.
        "AuthorizationHeaderMalformed" | "PermanentRedirect" => auth(AuthHint::Region),
        "AccessDenied"
        | "AllAccessDisabled"
        | "AccountProblem"
        | "ExpiredToken"
        | "InvalidToken"
        | "InvalidSecurity"
        | "NotSignedUp"
        | "TokenRefreshRequired" => auth(AuthHint::Unknown),
        "RequestTimeTooSkewed" => ProviderError::ClockSkew,
        "SlowDown"
        | "ServiceUnavailable"
        | "Throttling"
        | "ThrottlingException"
        | "RequestLimitExceeded"
        | "TooManyRequests"
        | "BandwidthLimitExceeded" => ProviderError::RateLimited { retry_after },
        "NoSuchBucket" | "NoSuchKey" | "NoSuchUpload" => ProviderError::NotFound,
        "BadDigest" | "XAmzContentSHA256Mismatch" => ProviderError::ChecksumMismatch,
        "RequestTimeout" => {
            ProviderError::Network("the server gave up waiting for the request".into())
        }
        "InternalError" => server(if status >= 500 { status } else { 500 }, code),
        "OperationAborted" => server(409, code),
        // Right after a bucket is created, before its address is known everywhere.
        "TemporaryRedirect" => server(307, code),
        _ => return None,
    })
}

/// What a status means when the code did not say.
fn by_status(
    status: u16,
    code: Option<&str>,
    message: Option<&str>,
    retry_after: Option<Duration>,
) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth {
            hint: AuthHint::Unknown,
        },
        404 => ProviderError::NotFound,
        408 => ProviderError::Network("the server gave up waiting for the request".into()),
        // 503 is S3's own "slow down"; a proxy in front of a service uses it the same way.
        429 | 503 => ProviderError::RateLimited { retry_after },
        500..=599 => server(status, code.unwrap_or_else(|| reason(status))),
        300..=399 => ProviderError::Permanent(format!(
            "the server redirected the request (HTTP {status}); check the endpoint address"
        )),
        _ => ProviderError::Permanent(describe(status, code, message)),
    }
}

fn server(status: u16, detail: &str) -> ProviderError {
    ProviderError::Server {
        status,
        detail: detail.to_owned(),
    }
}

fn reason(status: u16) -> &'static str {
    StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or_default()
}

/// `Code: Message` as one short line, or the status when the server said nothing usable.
fn describe(status: u16, code: Option<&str>, message: Option<&str>) -> String {
    let code = code.map(clean).filter(|c| !c.is_empty());
    let message = message.map(clean).filter(|m| !m.is_empty());
    match (code, message) {
        (Some(code), Some(message)) => truncate(format!("{code}: {message}")),
        (Some(code), None) => truncate(code),
        (None, Some(message)) => truncate(message),
        (None, None) => format!("unexpected response from server (HTTP {status})"),
    }
}

/// Server text with control characters removed and runs of white space made one space.
fn clean(text: &str) -> String {
    text.split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(text: String) -> String {
    if text.chars().count() <= MESSAGE_MAX {
        return text;
    }
    let mut short: String = text.chars().take(MESSAGE_MAX).collect();
    short.push_str("...");
    short
}

/// `Retry-After` as a wait, at most [`MAX_RETRY_AFTER`] (ruling Q17). Both forms are read: a
/// number of seconds, and an HTTP date (a date in the past is no wait). A value that is neither
/// is ignored, and one too large for a number is still a very long wait.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        let wait = value
            .parse::<u64>()
            .map_or(MAX_RETRY_AFTER, Duration::from_secs);
        return Some(wait.min(MAX_RETRY_AFTER));
    }
    let date = DateTime::parse_from_rfc2822(value).ok()?;
    let wait = (date.with_timezone(&Utc) - Utc::now())
        .to_std()
        .unwrap_or(Duration::ZERO);
    Some(wait.min(MAX_RETRY_AFTER))
}

/// A request that never produced a usable answer (no connection, a reset, a timeout): a
/// network error, worth another try, unless the request itself could not be built.
pub(crate) fn network_error(error: reqwest::Error) -> ProviderError {
    let mut causes = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    let builder_error = error.is_builder();
    // The URL names the bucket and key, which callers already know.
    let mut message = error.without_url().to_string();
    for cause in causes {
        message.push_str(": ");
        message.push_str(&cause);
    }
    if builder_error {
        ProviderError::Permanent(message)
    } else {
        ProviderError::Network(message)
    }
}

/// No progress for `idle`: as good as no answer.
pub(crate) fn stall_error(what: &str, idle: Duration) -> ProviderError {
    ProviderError::Network(format!("the {what} stalled (no progress for {idle:?})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(retry_after: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(RETRY_AFTER, HeaderValue::from_str(retry_after).unwrap());
        map
    }

    fn code_body(code: &str) -> Vec<u8> {
        format!("<Error><Code>{code}</Code><Message>m</Message></Error>").into_bytes()
    }

    #[test]
    fn retry_after_reads_seconds_and_caps_them() {
        let secs = |v: &str| retry_after(&headers(v));
        assert_eq!(secs("7"), Some(Duration::from_secs(7)));
        assert_eq!(secs(" 120 "), Some(Duration::from_secs(120)));
        assert_eq!(secs("0"), Some(Duration::ZERO));
        assert_eq!(secs("300"), Some(MAX_RETRY_AFTER));
        assert_eq!(secs("301"), Some(MAX_RETRY_AFTER));
        assert_eq!(secs("3600"), Some(MAX_RETRY_AFTER));
        assert_eq!(secs("99999999999999999999999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(MAX_RETRY_AFTER, Duration::from_secs(300));
    }

    #[test]
    fn retry_after_ignores_what_it_cannot_read() {
        for junk in ["", "soon", "-5", "1.5", "5s", "0x10"] {
            assert_eq!(retry_after(&headers(junk)), None, "{junk:?}");
        }
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_after_reads_http_dates() {
        let date = |v: &str| retry_after(&headers(v));
        assert_eq!(date("Thu, 01 Jan 2099 00:00:00 GMT"), Some(MAX_RETRY_AFTER));
        assert_eq!(date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(Duration::ZERO));
    }

    #[test]
    fn the_code_decides_before_the_status() {
        // A 400 that is really a signature problem, a 500 that is really "slow down".
        assert_eq!(
            from_response(400, None, &code_body("AuthorizationHeaderMalformed")),
            ProviderError::Auth {
                hint: AuthHint::Region
            }
        );
        assert_eq!(
            from_response(500, None, &code_body("SlowDown")),
            ProviderError::RateLimited { retry_after: None }
        );
    }

    #[test]
    fn a_bare_status_is_mapped_by_its_class() {
        let bare = |status| from_response(status, None, b"");
        assert_eq!(
            bare(403),
            ProviderError::Auth {
                hint: AuthHint::Unknown
            }
        );
        assert_eq!(bare(404), ProviderError::NotFound);
        assert_eq!(bare(429), ProviderError::RateLimited { retry_after: None });
        assert_eq!(bare(503), ProviderError::RateLimited { retry_after: None });
        assert_eq!(
            bare(502),
            ProviderError::Server {
                status: 502,
                detail: "Bad Gateway".into()
            }
        );
        assert!(matches!(bare(301), ProviderError::Permanent(m) if m.contains("redirected")));
        assert_eq!(
            bare(418),
            ProviderError::Permanent("unexpected response from server (HTTP 418)".into())
        );
    }

    #[test]
    fn an_error_inside_a_successful_answer_keeps_its_meaning() {
        // One key of a DeleteObjects request: the HTTP status was 200.
        let per_key = |code: &str| from_parts(200, Some(code), Some("text"), None);
        assert_eq!(
            per_key("AccessDenied"),
            ProviderError::Auth {
                hint: AuthHint::Unknown
            }
        );
        assert_eq!(
            per_key("InternalError"),
            ProviderError::Server {
                status: 500,
                detail: "InternalError".into()
            }
        );
        assert_eq!(
            per_key("Weird"),
            ProviderError::Permanent("Weird: text".into())
        );
    }

    #[test]
    fn server_text_is_cleaned_and_capped() {
        let long = "x".repeat(500);
        let message = format!("  Bad\r\n\tthing\u{7} {long}  ");
        let ProviderError::Permanent(text) = from_parts(400, Some("Odd"), Some(&message), None)
        else {
            panic!("expected Permanent");
        };
        assert!(text.starts_with("Odd: Bad thing xxx"), "{text}");
        assert!(!text.contains(['\r', '\n', '\t', '\u{7}']), "{text:?}");
        assert!(text.ends_with("..."), "{text}");
        assert_eq!(text.chars().count(), MESSAGE_MAX + 3);
    }

    #[test]
    fn the_signature_documents_are_never_copied() {
        let body = b"<Error><Code>SignatureDoesNotMatch</Code><Message>The request signature we \
                     calculated does not match</Message><AWSAccessKeyId>AKIAEXAMPLE</AWSAccessKeyId>\
                     <StringToSign>AWS4-HMAC-SHA256 secret-ish</StringToSign></Error>";
        let error = from_response(403, None, body);
        let shown = format!("{error:?} {error}");
        assert!(!shown.contains("AKIAEXAMPLE"), "{shown}");
        assert!(!shown.contains("StringToSign"), "{shown}");
        assert!(!shown.contains("secret-ish"), "{shown}");
    }
}
