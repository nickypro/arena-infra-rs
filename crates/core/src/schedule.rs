//! Date math for the autocommit week/day labels, with no external date crate.
//!
//! The ARENA iteration has a start date; a backup is labelled `wNdM` where the start
//! date itself is **w0d1**, the next day w0d2, … day 8 rolls to w1d1. Everything here
//! is pure integer math over "days since the Unix epoch", so it's fully unit-tested;
//! the only impurity (reading the wall clock) stays in the caller.

/// Days since 1970-01-01 for a proleptic-Gregorian civil date — Howard Hinnant's
/// well-known `days_from_civil` algorithm.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // Mar=0 .. Feb=11
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`]: a days-from-epoch count back to `(year, month, day)`
/// — Howard Hinnant's `civil_from_days`. Used to display a resolved plan date.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format a days-from-epoch count as `YYYY-MM-DD`.
pub fn ymd_string(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Parse a `YYYY-MM-DD` date into `(year, month, day)`.
pub fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let mut it = s.trim().split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// `(week, day)` for `today_days` relative to `start_days` (both days-from-epoch),
/// where the start date is **w0d1**. Dates before the start clamp to w0d1.
pub fn week_day(start_days: i64, today_days: i64) -> (u32, u32) {
    let diff = (today_days - start_days).max(0);
    ((diff / 7) as u32, (diff % 7) as u32 + 1)
}

/// Days-from-epoch for a Unix timestamp in seconds.
pub fn days_from_unix(unix_secs: u64) -> i64 {
    (unix_secs / 86_400) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
        assert_eq!(days_from_civil(2026, 6, 1), 20_605);
    }

    #[test]
    fn civil_from_days_round_trips() {
        for &(y, m, d) in &[(1970, 1, 1), (2000, 1, 1), (2026, 6, 2), (2026, 12, 31), (1999, 2, 28)] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d));
        }
        assert_eq!(ymd_string(days_from_civil(2026, 6, 2)), "2026-06-02");
    }

    #[test]
    fn parse_ymd_ok_and_bad() {
        assert_eq!(parse_ymd("2026-05-26"), Some((2026, 5, 26)));
        assert_eq!(parse_ymd(" 2026-05-26 "), Some((2026, 5, 26)));
        assert_eq!(parse_ymd("2026-13-01"), None); // bad month
        assert_eq!(parse_ymd("2026-05"), None); // too few parts
        assert_eq!(parse_ymd("nope"), None);
    }

    #[test]
    fn week_day_starts_at_w0d1_and_rolls() {
        let start = days_from_civil(2026, 5, 26);
        assert_eq!(week_day(start, start), (0, 1)); // start day
        assert_eq!(week_day(start, start + 1), (0, 2));
        assert_eq!(week_day(start, start + 6), (0, 7));
        assert_eq!(week_day(start, start + 7), (1, 1)); // day 8 -> w1d1
        assert_eq!(week_day(start, start + 8), (1, 2));
        assert_eq!(week_day(start, start - 5), (0, 1)); // before start clamps
    }
}
