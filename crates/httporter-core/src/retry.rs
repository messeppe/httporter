//! Shared retry helper: wraps one fallible async operation in a bounded
//! backoff loop. Provider clients use it around every HTTP request.

use std::future::Future;
use std::time::Duration;

use crate::ProviderError;

/// How long to wait before each retry. `delays.len()` is the number of
/// retries, so an operation runs at most `delays.len() + 1` times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub delays: Vec<Duration>,
}

impl RetryPolicy {
    /// `retries` retries with no waiting between them, for tests.
    pub fn immediate(retries: usize) -> Self {
        Self {
            delays: vec![Duration::ZERO; retries],
        }
    }
}

impl Default for RetryPolicy {
    /// 1, 2, 4, 8 and 10 seconds: five retries, six attempts in all.
    fn default() -> Self {
        Self {
            delays: [1, 2, 4, 8, 10].map(Duration::from_secs).to_vec(),
        }
    }
}

/// Runs `op` until it succeeds, fails with a non-retryable error, or the
/// policy runs out of delays. `op` receives the 0-based attempt number.
///
/// After a [`ProviderError::RateLimited`] the wait is the longer of the
/// policy's delay and the server's `retry_after`.
pub async fn with_retry<T, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T, ProviderError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut attempt: u32 = 0;
    loop {
        let err = match op(attempt).await {
            Ok(value) => return Ok(value),
            Err(err) => err,
        };
        if !err.is_retryable() {
            return Err(err);
        }
        let Some(&delay) = policy.delays.get(attempt as usize) else {
            return Err(err);
        };
        let wait = match &err {
            ProviderError::RateLimited {
                retry_after: Some(retry_after),
            } => delay.max(*retry_after),
            _ => delay,
        };
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderError;
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_six_attempts() {
        let calls = AtomicU32::new(0);
        let start = tokio::time::Instant::now();
        let r: Result<(), _> = with_retry(&RetryPolicy::default(), |_| {
            calls.fetch_add(1, SeqCst);
            async { Err(ProviderError::Transient("reset".into())) }
        })
        .await;
        assert!(matches!(r, Err(ProviderError::Transient(_))));
        assert_eq!(calls.load(SeqCst), 6);
        assert_eq!(start.elapsed(), Duration::from_secs(25));
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_policy_retries_without_waiting() {
        let calls = AtomicU32::new(0);
        let start = tokio::time::Instant::now();
        let r: Result<(), _> = with_retry(&RetryPolicy::immediate(2), |_| {
            calls.fetch_add(1, SeqCst);
            async { Err(ProviderError::Transient("reset".into())) }
        })
        .await;
        assert!(matches!(r, Err(ProviderError::Transient(_))));
        assert_eq!(calls.load(SeqCst), 3);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn non_retryable_returns_immediately() {
        let calls = AtomicU32::new(0);
        let start = tokio::time::Instant::now();
        let r: Result<(), _> = with_retry(&RetryPolicy::default(), |_| {
            calls.fetch_add(1, SeqCst);
            async { Err(ProviderError::Auth) }
        })
        .await;
        assert!(matches!(r, Err(ProviderError::Auth)));
        assert_eq!(calls.load(SeqCst), 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn honors_retry_after_when_longer() {
        let start = tokio::time::Instant::now();
        let r = with_retry(&RetryPolicy::default(), |attempt| async move {
            if attempt == 0 {
                Err(ProviderError::RateLimited {
                    retry_after: Some(Duration::from_secs(30)),
                })
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(r, Ok(7));
        assert_eq!(start.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn succeeds_after_transient() {
        let start = tokio::time::Instant::now();
        let r = with_retry(&RetryPolicy::default(), |attempt| async move {
            if attempt == 0 {
                Err(ProviderError::Transient("blip".into()))
            } else {
                Ok("done")
            }
        })
        .await;
        assert_eq!(r, Ok("done"));
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }
}
