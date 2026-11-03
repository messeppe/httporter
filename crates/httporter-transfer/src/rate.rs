//! Throughput over a sliding window, and the time left.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// Rates are measured over this much recent time.
pub const RATE_WINDOW: Duration = Duration::from_secs(5);

/// The window is kept in this many slots.
const SLOTS: u32 = 50;

/// Rates never divide by less than this, so the first file done doesn't read as a burst of
/// thousands per second.
const MIN_SPAN: Duration = Duration::from_secs(1);

/// Files and bytes done over the last [`RATE_WINDOW`] (or since activity began, when that is
/// more recent).
#[derive(Debug, Clone)]
pub struct RateWindow {
    window: Duration,
    slot: Duration,
    slots: VecDeque<Slot>,
    /// When activity began; `None` before anything was recorded.
    since: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Slot {
    start: Instant,
    files: u64,
    bytes: u64,
}

impl Default for RateWindow {
    fn default() -> Self {
        Self::new(RATE_WINDOW)
    }
}

impl RateWindow {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            slot: window / SLOTS,
            slots: VecDeque::new(),
            since: None,
        }
    }

    /// Forgets everything: activity begins again at `now`.
    pub fn restart(&mut self, now: Instant) {
        self.slots.clear();
        self.since = Some(now);
    }

    /// `files` more files and `bytes` more bytes were done at `now`.
    pub fn record(&mut self, now: Instant, files: u64, bytes: u64) {
        self.since.get_or_insert(now);
        match self.slots.back_mut() {
            Some(slot) if now < slot.start + self.slot => {
                slot.files += files;
                slot.bytes += bytes;
            }
            _ => self.slots.push_back(Slot {
                start: now,
                files,
                bytes,
            }),
        }
        // Slots that have left the window are of no further use.
        if let Some(cutoff) = now.checked_sub(self.window) {
            while self
                .slots
                .front()
                .is_some_and(|slot| slot.start + self.slot <= cutoff)
            {
                self.slots.pop_front();
            }
        }
    }

    /// Files per second and bytes per second at `now`.
    pub fn per_second(&self, now: Instant) -> (f64, f64) {
        let Some(since) = self.since else {
            return (0.0, 0.0);
        };
        let cutoff = now.checked_sub(self.window);
        let (files, bytes) = self
            .slots
            .iter()
            .filter(|slot| cutoff.is_none_or(|cutoff| slot.start > cutoff))
            .fold((0u64, 0u64), |(f, b), slot| {
                (f + slot.files, b + slot.bytes)
            });
        let span = now
            .saturating_duration_since(since)
            .clamp(MIN_SPAN.min(self.window), self.window)
            .as_secs_f64();
        (files as f64 / span, bytes as f64 / span)
    }
}

/// The time left: the longer of the estimate from the files left and the one from the bytes
/// left. A part with nothing left takes no time. A part with work left but no measured rate
/// (no file has finished yet) gives no estimate, and the other part's stands alone if it has
/// work left too. `None` when the time can't be told.
pub fn eta(
    remaining_files: u64,
    remaining_bytes: u64,
    files_per_s: f64,
    bytes_per_s: f64,
) -> Option<Duration> {
    fn estimate(remaining: u64, rate: f64) -> Option<f64> {
        if remaining == 0 {
            Some(0.0)
        } else if rate.is_finite() && rate > 0.0 {
            Some(remaining as f64 / rate)
        } else {
            None
        }
    }
    let seconds = match (
        estimate(remaining_files, files_per_s),
        estimate(remaining_bytes, bytes_per_s),
    ) {
        (Some(files), Some(bytes)) => files.max(bytes),
        // Only a part with work left can speak for the whole.
        (Some(one), None) | (None, Some(one)) if one > 0.0 => one,
        _ => return None,
    };
    Duration::try_from_secs_f64(seconds).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, seconds: f64) -> Instant {
        start + Duration::from_secs_f64(seconds)
    }

    #[test]
    fn nothing_recorded_is_zero() {
        let rates = RateWindow::default();
        assert_eq!(rates.per_second(Instant::now()), (0.0, 0.0));
    }

    #[test]
    fn steady_work_reads_at_its_rate() {
        let t0 = Instant::now();
        let mut rates = RateWindow::default();
        rates.restart(t0);
        // 10 files and 1,000 bytes a second, for 20 seconds, in steps of 100 ms.
        for step in 1..=200 {
            rates.record(at(t0, f64::from(step) / 10.0), 1, 100);
        }
        let (files, bytes) = rates.per_second(at(t0, 20.0));
        assert!((files - 10.0).abs() < 0.3, "{files}");
        assert!((bytes - 1000.0).abs() < 30.0, "{bytes}");
        assert!(
            rates.slots.len() <= SLOTS as usize + 2,
            "{}",
            rates.slots.len()
        );
    }

    #[test]
    fn only_the_last_five_seconds_count() {
        let t0 = Instant::now();
        let mut rates = RateWindow::default();
        rates.restart(t0);
        // A burst of 100 files at the start, then 1 a second.
        rates.record(at(t0, 0.5), 100, 0);
        for s in 1..=10 {
            rates.record(at(t0, f64::from(s)), 1, 0);
        }
        // At 10.5 s the window (5.5, 10.5] holds the files at 6, 7, 8, 9 and 10.
        let (files, _) = rates.per_second(at(t0, 10.5));
        assert!((files - 1.0).abs() < 1e-9, "{files}");
        // Nothing within the window: zero.
        let (files, bytes) = rates.per_second(at(t0, 16.0));
        assert_eq!((files, bytes), (0.0, 0.0));
    }

    #[test]
    fn early_rates_divide_by_the_time_so_far() {
        let t0 = Instant::now();
        let mut rates = RateWindow::default();
        rates.restart(t0);
        rates.record(at(t0, 1.0), 4, 4000);
        rates.record(at(t0, 2.0), 4, 4000);
        assert_eq!(rates.per_second(at(t0, 2.0)), (4.0, 4000.0));
        // …but never by less than a second.
        let mut fresh = RateWindow::default();
        fresh.restart(t0);
        fresh.record(at(t0, 0.01), 1, 10);
        assert_eq!(fresh.per_second(at(t0, 0.01)), (1.0, 10.0));
    }

    #[test]
    fn restart_forgets_the_past() {
        let t0 = Instant::now();
        let mut rates = RateWindow::default();
        rates.record(at(t0, 0.0), 50, 50);
        rates.restart(at(t0, 1.0));
        assert_eq!(rates.per_second(at(t0, 2.0)), (0.0, 0.0));
    }
}
