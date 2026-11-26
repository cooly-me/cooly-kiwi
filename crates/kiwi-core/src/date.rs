//! Civil dates from Unix time, without a calendar dependency.

/// Formats Unix seconds as `YYYY-MM-DD` (UTC).
pub fn from_unix(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Formats Unix seconds as an RFC 3339 UTC timestamp (`2026-11-23T08:15:00Z`).
pub fn timestamp(seconds: i64) -> String {
    let secs = seconds.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        from_unix(seconds),
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Today's date (UTC).
pub fn today() -> String {
    from_unix(now_unix())
}

/// Whether a string is a valid `YYYY-MM-DD` date.
pub fn is_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| value[range].parse::<u32>().ok();
    matches!(
        (digits(0..4), digits(5..7), digits(8..10)),
        (Some(_), Some(1..=12), Some(1..=31))
    )
}

// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(from_unix(0), "1970-01-01");
        assert_eq!(from_unix(1_700_000_000), "2023-11-14");
        assert_eq!(from_unix(951_782_400), "2000-02-29");
        assert_eq!(timestamp(1_700_000_000), "2023-11-14T22:13:20Z");
        assert!(is_date("2027-03-01"));
        assert!(!is_date("2027-13-01"));
        assert!(!is_date("27-03-01"));
    }
}
