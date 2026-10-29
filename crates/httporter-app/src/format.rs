//! Human-readable file sizes and Finder-style relative dates for the file panes.

use chrono::{Datelike, NaiveDateTime, Timelike};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Size in 1024-based units (`B`, `KB`, `MB`, `GB`), one decimal above bytes: `383.4 KB`.
/// `GB` is the largest unit, so a terabyte reads `1024.0 GB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    // 1023.95 and above would be shown as "1024.0", so it moves up to the next unit instead.
    while unit + 1 < UNITS.len() && value >= 1023.95 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `Today HH:MM`, `Yesterday HH:MM`, otherwise `D Mon YYYY` (English month abbreviations).
/// `now` is a parameter so the result is deterministic. "Today" and "Yesterday" are calendar
/// days, not 24-hour windows.
pub fn format_date(t: NaiveDateTime, now: NaiveDateTime) -> String {
    let day = t.date();
    let today = now.date();
    if day == today {
        format!("Today {:02}:{:02}", t.hour(), t.minute())
    } else if today.pred_opt() == Some(day) {
        format!("Yesterday {:02}:{:02}", t.hour(), t.minute())
    } else {
        format!("{} {} {}", t.day(), MONTHS[t.month0() as usize], t.year())
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
