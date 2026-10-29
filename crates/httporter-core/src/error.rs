use std::time::Duration;

/// Why a storage operation failed, in terms the transfer engine can act on.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
    /// Credentials were rejected.
    #[error("authentication failed")]
    Auth,
    #[error("not found")]
    NotFound,
    /// The service asked us to slow down, optionally saying for how long.
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("checksum mismatch")]
    ChecksumMismatch,
    /// The request was understood but must not be performed (for example it
    /// would clobber a folder with a file).
    #[error("refused: {0}")]
    Refused(String),
    /// A failure that may succeed if tried again.
    #[error("transient error: {0}")]
    Transient(String),
    /// A failure that retrying will not fix.
    #[error("permanent error: {0}")]
    Permanent(String),
}

impl ProviderError {
    /// Only errors that can clear up on their own are worth retrying.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::RateLimited { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transient_and_rate_limited_retry() {
        assert!(ProviderError::Transient("x".into()).is_retryable());
        assert!(ProviderError::RateLimited { retry_after: None }.is_retryable());
        for e in [
            ProviderError::Auth,
            ProviderError::NotFound,
            ProviderError::ChecksumMismatch,
            ProviderError::Refused("x".into()),
            ProviderError::Permanent("x".into()),
        ] {
            assert!(!e.is_retryable(), "{e:?}");
        }
    }
}
