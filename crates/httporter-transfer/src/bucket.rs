//! The global bandwidth limit, shared by every transfer.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use httporter_core::BodyMeter;
use tokio::sync::Notify;
use tokio::time::Instant;

/// How far behind the clock the schedule may fall. A sender that wakes a little late (timers
/// fire on whole milliseconds, and a busy runtime is later still) keeps its place instead of
/// the schedule restarting from "now" and drifting below the rate; after a pause this is also
/// the most that can go at once.
const SLACK: Duration = Duration::from_millis(50);

/// Lets bytes through at no more than a set rate, across every caller together.
///
/// Each [`take`](Self::take) books its bytes into the schedule and waits until they are paid
/// for, so the bytes let through by time `t` never exceed `rate × (t + 50 ms)`. Time spent
/// idle earns no credit beyond those 50 ms. A wait dropped before it ends gives its booking
/// back, so a paused or cancelled transfer costs the others nothing; a new rate applies at
/// once, to the waits under way too.
#[derive(Debug)]
pub struct TokenBucket {
    state: Mutex<Schedule>,
    /// Wakes every wait when the rate changes, to book again at the new rate.
    changed: Notify,
}

#[derive(Debug)]
struct Schedule {
    /// Bytes per second; `None` lets everything through at once.
    rate: Option<u64>,
    /// When every byte booked so far is paid for.
    paid_until: Option<Instant>,
    /// Goes up with every rate change. A booking made before the last change is not given
    /// back: the change already dropped it from the schedule.
    epoch: u64,
}

/// Bytes booked into the schedule and not yet paid for. Dropped unpaid, it gives its time back.
struct Booking<'a> {
    bucket: &'a TokenBucket,
    due: Instant,
    cost: Duration,
    epoch: u64,
    paid: bool,
}

impl Drop for Booking<'_> {
    fn drop(&mut self) {
        if !self.paid {
            self.bucket.refund(self.cost, self.epoch);
        }
    }
}

impl TokenBucket {
    /// A limit of `bytes_per_s`; `None` or `Some(0)` is no limit.
    pub fn new(bytes_per_s: Option<u64>) -> Self {
        Self {
            state: Mutex::new(Schedule {
                rate: bytes_per_s.filter(|&rate| rate > 0),
                paid_until: None,
                epoch: 0,
            }),
            changed: Notify::new(),
        }
    }

    /// Changes the limit at once: bytes booked and not yet paid for are dropped from the
    /// schedule, and their waits book again at the new rate.
    pub fn set_rate(&self, bytes_per_s: Option<u64>) {
        {
            let mut state = self.lock();
            state.rate = bytes_per_s.filter(|&rate| rate > 0);
            state.epoch += 1;
            let now = Instant::now();
            state.paid_until = match state.rate {
                Some(_) => state.paid_until.map(|paid| paid.min(now)),
                None => None,
            };
        }
        self.changed.notify_waiters();
    }

    /// Waits until `n` more bytes may go. Dropping the wait part way gives its bytes back.
    pub async fn take(&self, n: usize) {
        loop {
            // Made before booking, so a rate change after the booking is never missed.
            let changed = self.changed.notified();
            let Some(mut booking) = self.book(n) else {
                return;
            };
            tokio::select! {
                () = tokio::time::sleep_until(booking.due) => {
                    booking.paid = true;
                    return;
                }
                // The rate changed: book again at the new one.
                () = changed => {}
            }
        }
    }

    /// Books `n` bytes; `None` when there is no limit.
    fn book(&self, n: usize) -> Option<Booking<'_>> {
        let mut state = self.lock();
        let rate = state.rate?;
        let now = Instant::now();
        let earliest = now.checked_sub(SLACK).unwrap_or(now);
        let start = state.paid_until.map_or(earliest, |paid| paid.max(earliest));
        let cost = Duration::from_secs_f64(n as f64 / rate as f64);
        let due = start + cost;
        state.paid_until = Some(due);
        Some(Booking {
            bucket: self,
            due,
            cost,
            epoch: state.epoch,
            paid: false,
        })
    }

    /// Gives back `cost` of schedule booked at `epoch` and never used.
    fn refund(&self, cost: Duration, epoch: u64) {
        let mut state = self.lock();
        if state.epoch != epoch {
            return;
        }
        if let Some(paid) = state.paid_until {
            state.paid_until = Some(paid.checked_sub(cost).unwrap_or(paid));
        }
    }

    fn lock(&self) -> MutexGuard<'_, Schedule> {
        // Every update leaves the schedule whole, so a poisoned lock is still usable.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl BodyMeter for TokenBucket {
    async fn chunk(&self, len: usize) {
        self.take(len).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn no_limit_never_waits() {
        for bucket in [TokenBucket::new(None), TokenBucket::new(Some(0))] {
            let start = Instant::now();
            bucket.take(10_000_000).await;
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[tokio::test(start_paused = true)]
    async fn bytes_pass_at_the_rate() {
        let bucket = TokenBucket::new(Some(1000));
        let start = Instant::now();
        for _ in 0..4 {
            bucket.take(500).await;
        }
        // 2,000 bytes at 1,000 a second, less the slack.
        assert_eq!(start.elapsed(), ms(2000) - SLACK);
    }

    #[tokio::test(start_paused = true)]
    async fn callers_share_the_rate() {
        let bucket = TokenBucket::new(Some(1000));
        let start = Instant::now();
        futures_util::future::join_all((0..4).map(|_| bucket.take(1000))).await;
        assert_eq!(start.elapsed(), ms(4000) - SLACK);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_time_earns_no_more_than_the_slack() {
        let bucket = TokenBucket::new(Some(1000));
        bucket.take(1000).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
        let start = Instant::now();
        bucket.take(1000).await;
        assert_eq!(start.elapsed(), ms(1000) - SLACK);
    }

    /// Timers fire late (on whole milliseconds, or later on a busy runtime). A sender that
    /// wakes late keeps its place, so the rate holds over many small pieces.
    #[tokio::test(start_paused = true)]
    async fn waking_late_does_not_slow_the_rate() {
        let bucket = TokenBucket::new(Some(1_000_000));
        let start = Instant::now();
        // 65,536 bytes is 65.536 ms: every wait ends between two millisecond ticks.
        for _ in 0..100 {
            bucket.take(65_536).await;
        }
        let expected = Duration::from_micros(6_553_600) - SLACK;
        let took = start.elapsed();
        assert!(took >= expected && took <= expected + ms(1), "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_rate_can_change() {
        let bucket = TokenBucket::new(Some(1000));
        let start = Instant::now();
        bucket.take(1000).await;
        bucket.set_rate(Some(2000));
        bucket.take(1000).await;
        assert_eq!(start.elapsed(), ms(1500) - SLACK);
        bucket.set_rate(None);
        bucket.take(1_000_000).await;
        assert_eq!(start.elapsed(), ms(1500) - SLACK);
    }

    #[tokio::test(start_paused = true)]
    async fn it_meters_upload_bodies() {
        let bucket = TokenBucket::new(Some(1000));
        let meter: &dyn BodyMeter = &bucket;
        let start = Instant::now();
        meter.chunk(250).await;
        assert_eq!(start.elapsed(), ms(250) - SLACK);
    }
}
