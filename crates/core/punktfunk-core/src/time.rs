//! UTC timestamps for log lines and file stems, without a date crate.

/// `YYYY-MM-DDTHH:MM:SSZ` for unix milliseconds, with `.mmm` before the `Z` when `millis`.
pub fn utc_rfc3339(unix_ms: u64, millis: bool) -> String {
    let secs = (unix_ms / 1000) as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let frac = if millis {
        format!(".{:03}", unix_ms % 1000)
    } else {
        String::new()
    };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{frac}Z")
}

/// Civil (year, month, day) from days since the Unix epoch: Howard Hinnant's
/// `civil_from_days`. `719_468` shifts the epoch to a March-based year; `146_097`
/// days is one 400-year Gregorian era.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

#[cfg(test)]
mod tests {
    use super::utc_rfc3339;

    #[test]
    fn epoch_leap_day_and_millis() {
        assert_eq!(utc_rfc3339(0, false), "1970-01-01T00:00:00Z");
        assert_eq!(utc_rfc3339(0, true), "1970-01-01T00:00:00.000Z");
        // 2024-02-29 23:59:59.042 UTC.
        assert_eq!(
            utc_rfc3339(1_709_251_199_042, true),
            "2024-02-29T23:59:59.042Z"
        );
        assert_eq!(
            utc_rfc3339(1_709_251_200_000, false),
            "2024-03-01T00:00:00Z"
        );
        assert_eq!(utc_rfc3339(951_782_400_000, false), "2000-02-29T00:00:00Z");
    }
}
