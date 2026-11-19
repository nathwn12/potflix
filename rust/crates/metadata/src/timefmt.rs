
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The oldest instant `SystemTime` represents on every supported platform:
/// Windows' FILETIME epoch, 1601-01-01T00:00:00Z.
const MIN_REPRESENTABLE_SECS: i64 = -11_644_473_600;

/// The instant a `checkedAt` string names, or `None` when it is not RFC3339 UTC
/// text (which includes the empty string a Rust-written positive record omits).
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let year: i64 = ascii_digits(&bytes[0..4])?;
    let month: i64 = ascii_digits(&bytes[5..7])?;
    let day: i64 = ascii_digits(&bytes[8..10])?;
    let hour: i64 = ascii_digits(&bytes[11..13])?;
    let minute: i64 = ascii_digits(&bytes[14..16])?;
    let second: i64 = ascii_digits(&bytes[17..19])?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let rest = &bytes[19..];
    let (nanos, zone) = match rest.first() {
        Some(b'.') => {
            let digits: Vec<u8> = rest[1..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .copied()
                .collect();
            if digits.is_empty() || digits.len() > 9 {
                return None;
            }
            let zone = &rest[1 + digits.len()..];
            let mut nanos: u32 = 0;
            for d in &digits {
                nanos = nanos * 10 + u32::from(d - b'0');
            }
            for _ in digits.len()..9 {
                nanos *= 10;
            }
            (nanos, zone)
        }
        _ => (0, rest),
    };
    if zone != b"Z" {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    let secs = secs.max(MIN_REPRESENTABLE_SECS);
    let base = if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))?
    };
    base.checked_add(Duration::from_nanos(u64::from(nanos)))
}

/// `time.RFC3339Nano` for a UTC instant.
pub fn format_rfc3339(at: SystemTime) -> String {
    let (secs, nanos) = match at.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(err) => {
            let d = err.duration();
            let secs = -(d.as_secs() as i64);
            let nanos = d.subsec_nanos();
            if nanos == 0 {
                (secs, 0)
            } else {
                (secs - 1, 1_000_000_000 - nanos)
            }
        }
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    if nanos == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        let mut frac = format!("{nanos:09}");
        while frac.ends_with('0') {
            frac.pop();
        }
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{frac}Z")
    }
}

/// `time.Now().UTC()` as its RFC3339Nano text.
pub fn now_rfc3339() -> String {
    format_rfc3339(SystemTime::now())
}

fn ascii_digits(bytes: &[u8]) -> Option<i64> {
    let mut value: i64 = 0;
    for b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(b - b'0');
    }
    Some(value)
}

/// Days from 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The proleptic Gregorian date for days from 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_epoch_and_a_known_instant() {
        assert_eq!(format_rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let at = UNIX_EPOCH + Duration::from_secs(1_791_857_643);
        assert_eq!(format_rfc3339(at), "2026-10-13T02:14:03Z");
        assert_eq!(parse_rfc3339("2026-10-13T02:14:03Z"), Some(at));
    }

    #[test]
    fn fractions_trim_like_rfc3339_nano() {
        let at = UNIX_EPOCH + Duration::from_millis(1_500);
        assert_eq!(format_rfc3339(at), "1970-01-01T00:00:01.5Z");
        assert_eq!(parse_rfc3339("1970-01-01T00:00:01.5Z"), Some(at));
        assert_eq!(
            parse_rfc3339("1970-01-01T00:00:01.500000000Z"),
            Some(UNIX_EPOCH + Duration::from_millis(1_500))
        );
        assert!(!format_rfc3339(now()).is_empty());
    }

    #[test]
    fn go_zero_time_is_pre_epoch_and_rejects_junk() {
        let zero = parse_rfc3339("0001-01-01T00:00:00Z").expect("Go's zero time parses");
        assert!(zero < UNIX_EPOCH);
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2026-10-13T02:14:03+02:00"), None);
        assert_eq!(parse_rfc3339("2026-10-13 02:14:03Z"), None);
    }

    fn now() -> SystemTime {
        SystemTime::now()
    }
}
