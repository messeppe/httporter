//! Human-readable file sizes and Finder-style relative dates for the file panes.
//!
//! The numbers are worked out here; their words (the units, "Today", the months) are `copy.rs`'s
//! (ruling Q36).

use chrono::{Datelike, NaiveDateTime, Timelike};

use crate::copy::{
    self, BYTES_UNIT as BYTES, SIZE_UNITS as UNITS, day_month_year, today_at, yesterday_at,
};

/// Size in 1024-based units (`B`, `KB`, `MB`, `GB`), one decimal above bytes: `383.4 KB`.
/// `GB` is the largest unit, so a terabyte reads `1024.0 GB`.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        return copy::size_in(&bytes.to_string(), BYTES);
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // 1023.95 and above would be shown as "1024.0", so it moves up to the next unit instead.
    while unit + 1 < UNITS.len() && value >= 1023.95 {
        value /= 1024.0;
        unit += 1;
    }
    copy::size_in(&format!("{value:.1}"), UNITS[unit])
}

/// A count with thousands separators: `13,793`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A transfer rate without its unit: whole numbers from ten up (`96`), one decimal below (`6.1`).
/// A rate that is not a number, or is negative, reads `0.0`.
pub fn format_rate(rate: f64) -> String {
    if !rate.is_finite() || rate <= 0.0 {
        return "0.0".to_owned();
    }
    let one_decimal = format!("{rate:.1}");
    // 9.96 would read "10.0" with a decimal; it is ten.
    if rate < 10.0 && one_decimal != "10.0" {
        one_decimal
    } else {
        format!("{}", rate.round_ties_even() as u64)
    }
}

/// How far a transfer has got in bytes, both numbers in the unit of the total: `527 of 865 MB`.
/// Whole numbers once the total is 100 or more in that unit, one decimal below; under 1 KB it is
/// bytes. Units roll over as in [`format_size`], so `GB` is the largest.
pub fn format_progress(done: u64, total: u64) -> String {
    if total < 1024 {
        return copy::progress_in(&done.to_string(), &total.to_string(), BYTES);
    }
    let mut divisor = 1024.0;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && total as f64 / divisor >= 1023.95 {
        divisor *= 1024.0;
        unit += 1;
    }
    let (done, total) = (done as f64 / divisor, total as f64 / divisor);
    if total >= 100.0 {
        copy::progress_in(&format!("{done:.0}"), &format!("{total:.0}"), UNITS[unit])
    } else {
        copy::progress_in(&format!("{done:.1}"), &format!("{total:.1}"), UNITS[unit])
    }
}

/// `Today HH:MM`, `Yesterday HH:MM`, otherwise `D Mon YYYY` (English month abbreviations).
/// `now` is a parameter so the result is deterministic. "Today" and "Yesterday" are calendar
/// days, not 24-hour windows.
pub fn format_date(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let day = t.date();
    let today = now.date();
    if day == today {
        today_at(t.hour(), t.minute())
    } else if today.pred_opt() == Some(day) {
        yesterday_at(t.hour(), t.minute())
    } else {
        day_month_year(t.day(), t.month0(), t.year())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(857), "857 B");
        assert_eq!(format_size(392_641), "383.4 KB");
        assert_eq!(format_size(6_725_584), "6.4 MB");
        assert_eq!(format_size(2_147_483_648), "2.0 GB");
    }

    #[test]
    fn size_unit_boundaries() {
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1_048_576), "1.0 MB");
        // 1023.999 KB would round to "1024.0 KB"; it must roll over to the next unit.
        assert_eq!(format_size(1_048_575), "1.0 MB");
        // GB is the largest unit.
        assert_eq!(format_size(1_099_511_627_776), "1024.0 GB");
        assert_eq!(format_size(u64::MAX), "17179869184.0 GB");
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(8_412), "8,412");
        assert_eq!(format_count(13_793), "13,793");
        assert_eq!(format_count(1_204_000), "1,204,000");
        assert_eq!(format_count(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn rates_are_whole_from_ten_and_have_one_decimal_below() {
        assert_eq!(format_rate(96.0), "96");
        assert_eq!(format_rate(96.4), "96");
        assert_eq!(format_rate(6.1), "6.1");
        assert_eq!(format_rate(0.0), "0.0");
        assert_eq!(format_rate(9.94), "9.9");
        // 9.96 would read "10.0" with a decimal; it is ten.
        assert_eq!(format_rate(9.96), "10");
        assert_eq!(format_rate(10.0), "10");
        assert_eq!(format_rate(1234.5), "1234");
        // Nothing sensible to show for a rate that is not a number or is negative.
        assert_eq!(format_rate(f64::NAN), "0.0");
        assert_eq!(format_rate(-3.0), "0.0");
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn progress_sizes_share_the_unit_of_the_total() {
        // Whole numbers once the total is 100 or more in its unit.
        assert_eq!(format_progress(527 * MB, 865 * MB), "527 of 865 MB");
        assert_eq!(format_progress(0, 865 * MB), "0 of 865 MB");
        assert_eq!(format_progress(50 * 1024, 100 * 1024), "50 of 100 KB");
        // One decimal below that.
        assert_eq!(format_progress(5_347_738, 12_897_485), "5.1 of 12.3 MB");
        assert_eq!(
            format_progress(3 * 1024 * MB, 10 * 1024 * MB),
            "3.0 of 10.0 GB"
        );
        // The unit follows the total, not the part done.
        assert_eq!(format_progress(300 * 1024, 5 * MB), "0.3 of 5.0 MB");
        assert_eq!(format_progress(300, 900), "300 of 900 B");
        // GB is the largest unit.
        assert_eq!(
            format_progress(1024 * 1024 * MB, 2560 * 1024 * MB),
            "1024 of 2560 GB"
        );
        // 1023.999 KB moves up to MB, as `format_size` does.
        assert_eq!(format_progress(0, 1_048_575), "0.0 of 1.0 MB");
    }

    #[test]
    fn dates() {
        let now = dt("2026-10-29 12:00");
        assert_eq!(format_date(dt("2026-10-29 10:02"), now), "Today 10:02");
        assert_eq!(format_date(dt("2026-10-28 17:02"), now), "Yesterday 17:02");
        assert_eq!(format_date(dt("2026-03-12 09:00"), now), "12 Mar 2026");
    }

    #[test]
    fn date_day_is_not_zero_padded_and_months_are_english() {
        let now = dt("2026-10-29 12:00");
        assert_eq!(format_date(dt("2025-01-05 09:00"), now), "5 Jan 2025");
        assert_eq!(format_date(dt("2024-12-31 23:59"), now), "31 Dec 2024");
    }

    #[test]
    fn yesterday_follows_the_calendar_day_not_24_hours() {
        // Only 2 minutes apart, but different calendar days.
        assert_eq!(
            format_date(dt("2026-10-28 23:59"), dt("2026-10-29 00:01")),
            "Yesterday 23:59"
        );
        // Across a month boundary.
        assert_eq!(
            format_date(dt("2026-10-31 08:05"), dt("2026-11-01 08:00")),
            "Yesterday 08:05"
        );
        // 23 hours apart but the same calendar day is still "Today".
        assert_eq!(
            format_date(dt("2026-10-29 00:30"), dt("2026-10-29 23:30")),
            "Today 00:30"
        );
        // Two days ago is a plain date.
        assert_eq!(
            format_date(dt("2026-10-27 12:00"), dt("2026-10-29 12:00")),
            "27 Oct 2026"
        );
    }
}
