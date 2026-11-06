// SPDX-License-Identifier: MIT OR Apache-2.0

//! Adaptive concurrency: fewer requests at once when a server pushes back, then back up again.

/// Successes in a row that earn one more request at once.
const RECOVER_AFTER: u32 = 20;

/// How many requests a site may have in flight: the configured number, halved each time the
/// server asks to slow down (never below 1), and raised by one again after every
/// [`RECOVER_AFTER`] successes in a row (never above the configured number).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveLimiter {
    configured: usize,
    limit: usize,
    successes: u32,
}

impl AdaptiveLimiter {
    /// A limiter at full speed. A `configured` of 0 is treated as 1.
    pub fn new(configured: usize) -> Self {
        let configured = configured.max(1);
        Self {
            configured,
            limit: configured,
            successes: 0,
        }
    }

    /// The requests allowed in flight now.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// A request succeeded.
    pub fn on_success(&mut self) {
        self.successes += 1;
        if self.successes >= RECOVER_AFTER {
            self.successes = 0;
            if self.limit < self.configured {
                self.limit += 1;
            }
        }
    }

    /// The server asked to slow down: halve the limit and start counting successes again.
    pub fn on_slow_down(&mut self) {
        self.limit = (self.limit / 2).max(1);
        self.successes = 0;
    }

    /// The configured number changed. A limiter at full speed follows it; a slowed-down one
    /// keeps its lower limit, coming down to the new number if that is lower still.
    pub fn set_configured(&mut self, n: usize) {
        let n = n.max(1);
        if self.limit == self.configured || self.limit > n {
            self.limit = n;
        }
        self.configured = n;
    }

    /// Back to full speed, forgetting any slow-down.
    pub fn reset(&mut self) {
        self.limit = self.configured;
        self.successes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_returns_to_the_configured_value() {
        let mut limiter = AdaptiveLimiter::new(16);
        limiter.on_slow_down();
        limiter.on_slow_down();
        assert_eq!(limiter.limit(), 4);
        limiter.reset();
        assert_eq!(limiter.limit(), 16);
        // The success count starts again too.
        for _ in 0..19 {
            limiter.on_success();
        }
        limiter.on_slow_down();
        for _ in 0..19 {
            limiter.on_success();
        }
        assert_eq!(limiter.limit(), 8);
    }
}
