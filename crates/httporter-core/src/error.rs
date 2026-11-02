use std::time::Duration;

/// Which part of the credentials a rejection points at, when the service says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthHint {
    /// The service did not say why (Bunny always; S3 `AccessDenied`).
    Unknown,
    /// The key ID is not known (S3 `InvalidAccessKeyId`).
    AccessKey,
    /// The secret does not match the key ID (S3 `SignatureDoesNotMatch`).
    Secret,
    /// The request was signed for another region (S3 `AuthorizationHeaderMalformed`).
    Region,
}

/// Why a storage operation failed, in terms the transfer engine can act on.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
    /// Credentials were rejected.
    #[error("authentication failed")]
    Auth { hint: AuthHint },
    #[error("not found")]
    NotFound,
    /// The service asked us to slow down, optionally saying for how long.
    /// Providers clamp `retry_after` to [`crate::MAX_RETRY_AFTER`] (spec:
    /// honoured up to 300 s); a policy with a lower limit gives up instead.
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("checksum mismatch")]
    ChecksumMismatch,
    /// The request was understood but must not be performed (for example it
    /// would clobber a folder with a file).
    #[error("refused: {0}")]
    Refused(String),
    /// No answer: DNS, connect, TLS, reset, timeout.
    #[error("network error: {0}")]
    Network(String),
    /// The server answered with a temporary failure (5xx, Bunny "busy" 400).
    #[error("server error (HTTP {status}){}", detail_suffix(detail))]
    Server { status: u16, detail: String },
    /// The PC clock is too far off (S3 RequestTimeTooSkewed).
    #[error("the computer's clock is too far off")]
    ClockSkew,
    /// A failure that retrying will not fix.
    #[error("permanent error: {0}")]
    Permanent(String),
}

/// `": {detail}"`, or nothing when there is no detail.
fn detail_suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

impl ProviderError {
    /// Only errors that can clear up on their own are worth retrying: no answer, a server
    /// that failed for now, or one that asked us to wait.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Network(_) | Self::Server { .. } | Self::RateLimited { .. }
        )
    }

    /// The server is pushing back on the load, so fewer requests at once would help: a 429,
    /// a 503 (S3 `SlowDown`) or Bunny's "busy" 400. Only Bunny's busy answer may become
    /// `Server { status: 400 }`; any other 400 is mapped to something else.
    pub fn is_slow_down(&self) -> bool {
        match self {
            Self::RateLimited { .. } => true,
            Self::Server { status, .. } => matches!(status, 400 | 503),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_and_slow_down_sets() {
        assert!(ProviderError::Network("x".into()).is_retryable());
        assert!(
            ProviderError::Server {
                status: 502,
                detail: "".into()
            }
            .is_retryable()
        );
        assert!(
            ProviderError::Server {
                status: 503,
                detail: "".into()
            }
            .is_slow_down()
        );
        assert!(
            !ProviderError::Server {
                status: 500,
                detail: "".into()
            }
            .is_slow_down()
        );
        assert!(
            !ProviderError::Auth {
                hint: AuthHint::Unknown
            }
            .is_retryable()
        );
        assert!(!ProviderError::ClockSkew.is_retryable());
    }

    #[test]
    fn every_variant_is_placed_in_both_sets() {
        let server = |status| ProviderError::Server {
            status,
            detail: "busy".into(),
        };
        // (error, retryable, slow down)
        let cases = [
            (ProviderError::RateLimited { retry_after: None }, true, true),
            (ProviderError::Network("reset".into()), true, false),
            (server(400), true, true),
            (server(503), true, true),
            (server(500), true, false),
            (server(504), true, false),
            (
                ProviderError::Auth {
                    hint: AuthHint::Secret,
                },
                false,
                false,
            ),
            (ProviderError::NotFound, false, false),
            (ProviderError::ChecksumMismatch, false, false),
            (ProviderError::Refused("x".into()), false, false),
            (ProviderError::ClockSkew, false, false),
            (ProviderError::Permanent("x".into()), false, false),
        ];
        for (error, retryable, slow_down) in cases {
            assert_eq!(error.is_retryable(), retryable, "{error:?}");
            assert_eq!(error.is_slow_down(), slow_down, "{error:?}");
        }
    }

    #[test]
    fn a_server_error_says_its_status_and_any_detail() {
        let server = |detail: &str| ProviderError::Server {
            status: 503,
            detail: detail.into(),
        };
        assert_eq!(server("busy").to_string(), "server error (HTTP 503): busy");
        assert_eq!(server("").to_string(), "server error (HTTP 503)");
    }
}
