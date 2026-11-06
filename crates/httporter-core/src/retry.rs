// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared retry helper: wraps one fallible async operation in a bounded
//! backoff loop. Whoever makes a request owns its retries and picks the
//! policy: someone browsing waits briefly, the transfer engine longer.

use std::future::Future;
use std::time::Duration;

use crate::ProviderError;

/// The longest `Retry-After` any policy honours, so a hostile or broken
/// server cannot park a request for hours.
///
/// Providers clamp a server's Retry-After to `MAX_RETRY_AFTER` before
/// returning `RateLimited` (spec: honoured up to 300 s), so the default
/// policy waits at most 300 s; a policy whose `max_retry_after` is below
/// that (interactive: 5 s) gives up instead of waiting.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);

/// The waits before each retry, in seconds: up to five retries.
const STANDARD_DELAYS: [u64; 5] = [1, 2, 4, 8, 10];

/// How long to wait before each retry. `delays.len()` is the number of
/// retries, so an operation runs at most `delays.len() + 1` times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub delays: Vec<Duration>,
    /// The longest `Retry-After` worth waiting for. A rate limit asking for
    /// more ends the retries at once.
    pub max_retry_after: Duration,
}

impl RetryPolicy {
    /// `retries` retries with no waiting between them, for tests.
    pub fn immediate(retries: usize) -> Self {
        Self {
            delays: vec![Duration::ZERO; retries],
            max_retry_after: MAX_RETRY_AFTER,
        }
    }

    /// The first `n` of the standard waits (1, 2, 4, 8 and 10 s): `n`
    /// retries, at most five. The Retries setting offers 5, 3 and 0.
    pub fn with_retries(n: u8) -> Self {
        Self {
            delays: STANDARD_DELAYS
                .into_iter()
                .take(usize::from(n))
                .map(Duration::from_secs)
                .collect(),
            max_retry_after: MAX_RETRY_AFTER,
        }
    }

    /// For a request someone is waiting on, such as a folder listing: two
    /// retries, 1 s and 2 s apart, and no waiting out a `Retry-After` of
    /// more than 5 s.
    pub fn interactive() -> Self {
        Self {
            delays: vec![Duration::from_secs(1), Duration::from_secs(2)],
            max_retry_after: Duration::from_secs(5),
        }
    }
}

impl Default for RetryPolicy {
    /// 1, 2, 4, 8 and 10 seconds: five retries, six attempts in all.
    fn default() -> Self {
        Self::with_retries(5)
    }
}

/// Runs `op` until it succeeds, fails with a non-retryable error, or the
/// policy runs out of delays. `op` receives the 0-based attempt number.
///
/// After a [`ProviderError::RateLimited`] the wait is the longer of the
/// policy's delay and the server's `retry_after`; a `retry_after` beyond the
/// policy's `max_retry_after` is returned at once instead of waited out.
///
/// Providers clamp a server's Retry-After to [`MAX_RETRY_AFTER`] before
/// returning `RateLimited` (spec: honoured up to 300 s), so the default
/// policy waits at most 300 s; a policy whose `max_retry_after` is below
/// that (interactive: 5 s) gives up instead of waiting.
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
            } => {
                if *retry_after > policy.max_retry_after {
                    return Err(err);
                }
                delay.max(*retry_after)
            }
            _ => delay,
        };
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuthHint;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use tokio::time::Instant;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn limited(seconds: u64) -> ProviderError {
        ProviderError::RateLimited {
            retry_after: Some(secs(seconds)),
        }
    }

    #[test]
    fn the_policies_have_the_contract_values() {
        assert_eq!(MAX_RETRY_AFTER, secs(300));
        assert_eq!(
            RetryPolicy::default(),
            RetryPolicy {
                delays: [1, 2, 4, 8, 10].map(secs).to_vec(),
                max_retry_after: MAX_RETRY_AFTER,
            }
        );
        assert_eq!(
            RetryPolicy::interactive(),
            RetryPolicy {
                delays: vec![secs(1), secs(2)],
                max_retry_after: secs(5),
            }
        );
        assert_eq!(
            RetryPolicy::immediate(2),
            RetryPolicy {
                delays: vec![Duration::ZERO; 2],
                max_retry_after: MAX_RETRY_AFTER,
            }
        );
    }

    #[test]
    fn with_retries_takes_the_first_n_delays() {
        assert_eq!(
            RetryPolicy::with_retries(3).delays,
            [1, 2, 4].map(Duration::from_secs).to_vec()
        );
        assert!(RetryPolicy::with_retries(0).delays.is_empty());
        assert_eq!(RetryPolicy::with_retries(5), RetryPolicy::default());
        // There are only five waits to take from.
        assert_eq!(RetryPolicy::with_retries(9), RetryPolicy::default());
        assert_eq!(
            RetryPolicy::with_retries(3).max_retry_after,
            MAX_RETRY_AFTER
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_six_attempts() {
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        let r: Result<(), _> = with_retry(&RetryPolicy::default(), |_| {
            calls.fetch_add(1, SeqCst);
            async { Err(ProviderError::Network("reset".into())) }
        })
        .await;
        assert!(matches!(r, Err(ProviderError::Network(_))));
        assert_eq!(calls.load(SeqCst), 6);
        assert_eq!(start.elapsed(), Duration::from_secs(25));
    }

    /// Ledger 118: the waits come in the policy's order, not just to the right total.
    #[tokio::test(start_paused = true)]
    async fn waits_follow_the_delays_in_order() {
        let start = Instant::now();
        let seen = Mutex::new(Vec::new());
        let r: Result<(), _> = with_retry(&RetryPolicy::default(), |_| {
            seen.lock().unwrap().push(start.elapsed());
            async {
                Err(ProviderError::Server {
                    status: 502,
                    detail: String::new(),
                })
            }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(
            seen.into_inner().unwrap(),
            [0, 1, 3, 7, 15, 25].map(secs).to_vec()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_policy_retries_without_waiting() {
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        let r: Result<(), _> = with_retry(&RetryPolicy::immediate(2), |_| {
            calls.fetch_add(1, SeqCst);
            async { Err(ProviderError::Network("reset".into())) }
        })
        .await;
        assert!(matches!(r, Err(ProviderError::Network(_))));
        assert_eq!(calls.load(SeqCst), 3);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn non_retryable_returns_immediately() {
        let auth = ProviderError::Auth {
            hint: AuthHint::Unknown,
        };
        for error in [auth, ProviderError::ClockSkew] {
            let calls = AtomicU32::new(0);
            let start = Instant::now();
            let r: Result<(), _> = with_retry(&RetryPolicy::default(), |_| {
                calls.fetch_add(1, SeqCst);
                let error = error.clone();
                async move { Err(error) }
            })
            .await;
            assert_eq!(r, Err(error));
            assert_eq!(calls.load(SeqCst), 1);
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
    }

    /// Ruling F4: a listing someone is waiting for does not sit out a long `Retry-After`.
    #[tokio::test(start_paused = true)]
    async fn interactive_gives_up_on_long_retry_after() {
        let calls = AtomicU32::new(0);
        let start = Instant::now();
        let r: Result<(), _> = with_retry(&RetryPolicy::interactive(), |_| {
            calls.fetch_add(1, SeqCst);
            async { Err(limited(60)) }
        })
        .await;
        assert_eq!(r, Err(limited(60)));
        assert_eq!(calls.load(SeqCst), 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_up_to_the_limit_is_waited_out() {
        let start = Instant::now();
        let r = with_retry(&RetryPolicy::interactive(), |attempt| async move {
            if attempt == 0 { Err(limited(5)) } else { Ok(1) }
        })
        .await;
        assert_eq!(r, Ok(1));
        assert_eq!(start.elapsed(), secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn any_policy_returns_a_retry_after_beyond_its_limit_at_once() {
        let calls = AtomicU32::new(0);
        let r: Result<(), _> = with_retry(&RetryPolicy::default(), |_| {
            calls.fetch_add(1, SeqCst);
            async { Err(limited(301)) }
        })
        .await;
        assert_eq!(r, Err(limited(301)));
        assert_eq!(calls.load(SeqCst), 1);
    }

    /// Ledger 117: the wait is the longer of the two, so a short (or missing) `Retry-After`
    /// still waits the policy's delay.
    #[tokio::test(start_paused = true)]
    async fn retry_after_shorter_than_delay_waits_the_delay() {
        for retry_after in [Some(Duration::from_millis(200)), None] {
            let start = Instant::now();
            let r = with_retry(&RetryPolicy::default(), |attempt| async move {
                if attempt == 0 {
                    Err(ProviderError::RateLimited { retry_after })
                } else {
                    Ok(())
                }
            })
            .await;
            assert_eq!(r, Ok(()));
            assert_eq!(start.elapsed(), secs(1), "{retry_after:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn honors_retry_after_when_longer() {
        let start = Instant::now();
        let r = with_retry(&RetryPolicy::default(), |attempt| async move {
            if attempt == 0 {
                Err(limited(30))
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(r, Ok(7));
        assert_eq!(start.elapsed(), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn succeeds_after_a_network_error() {
        let start = Instant::now();
        let r = with_retry(&RetryPolicy::default(), |attempt| async move {
            if attempt == 0 {
                Err(ProviderError::Network("blip".into()))
            } else {
                Ok("done")
            }
        })
        .await;
        assert_eq!(r, Ok("done"));
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }
}
