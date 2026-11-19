
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// An instant, UTC, second + nanosecond resolution.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Time {
    secs: i64,
    nanos: u32,
}

impl Time {
    /// Go's `time.Time{}`: 0001-01-01T00:00:00Z.
    pub const ZERO: Time = Time {
        secs: -62_135_596_800,
        nanos: 0,
    };

    /// An instant from Unix seconds and sub-second nanoseconds (normalized).
    pub fn from_unix(secs: i64, nanos: u32) -> Time {
        let carry = (nanos / 1_000_000_000) as i64;
        Time {
            secs: secs + carry,
            nanos: nanos % 1_000_000_000,
        }
    }

    /// Unix seconds (may be negative).
    pub fn unix_seconds(&self) -> i64 {
        self.secs
    }

    /// Sub-second nanoseconds, `0..1_000_000_000`.
    pub fn subsec_nanos(&self) -> u32 {
        self.nanos
    }

    /// Go's `t.IsZero()`.
    pub fn is_zero(&self) -> bool {
        *self == Time::ZERO
    }

    /// The current wall-clock instant.
    pub fn now() -> Time {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => Time::from_unix(d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => Time::from_unix(-(e.duration().as_secs() as i64), 0),
        }
    }

    /// Go's `t.UTC().Format(time.RFC3339)` -- never a fractional second.
    pub fn rfc3339(&self) -> String {
        self.format(false)
    }

    /// Go's `time.Time` JSON form (`RFC3339Nano`, trailing zeros trimmed).
    pub fn rfc3339_nanos(&self) -> String {
        self.format(true)
    }

    /// Reads the RFC3339 form Go's `time.Time` writes (`Z` or `+HH:MM` offset,
    /// optional fraction). `None` for anything else.
    pub fn parse_rfc3339(s: &str) -> Option<Time> {
        let b = s.as_bytes();
        if b.len() < 20 {
            return None;
        }
        let year = digits(b, 0, 4)?;
        if b[4] != b'-' {
            return None;
        }
        let month = digits(b, 5, 2)?;
        if b[7] != b'-' {
            return None;
        }
        let day = digits(b, 8, 2)?;
        if b[10] != b'T' && b[10] != b't' {
            return None;
        }
        let hour = digits(b, 11, 2)?;
        if b[13] != b':' {
            return None;
        }
        let minute = digits(b, 14, 2)?;
        if b[16] != b':' {
            return None;
        }
        let second = digits(b, 17, 2)?;

        let mut i = 19;
        let mut nanos = 0u32;
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return None;
            }
            let mut scale = 100_000_000u32;
            for c in b[start..i].iter().take(9) {
                nanos += (c - b'0') as u32 * scale;
                scale /= 10;
            }
        }

        let offset_secs: i64 = if i < b.len() && (b[i] == b'Z' || b[i] == b'z') {
            i += 1;
            0
        } else if i + 1 < b.len() && (b[i] == b'+' || b[i] == b'-') {
            let sign: i64 = if b[i] == b'-' { -1 } else { 1 };
            let oh = digits(b, i + 1, 2)?;
            if i + 3 >= b.len() || b[i + 3] != b':' {
                return None;
            }
            let om = digits(b, i + 4, 2)?;
            i += 6;
            sign * (oh * 3600 + om * 60)
        } else {
            return None;
        };
        if i != b.len() {
            return None;
        }
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
            return None;
        }
        let secs = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
            - offset_secs;
        Some(Time::from_unix(secs, nanos))
    }

    fn format(&self, fraction: bool) -> String {
        let days = self.secs.div_euclid(86_400);
        let sod = self.secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
        let mut out = format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}");
        if fraction && self.nanos != 0 {
            let mut frac = format!(".{:09}", self.nanos);
            while frac.ends_with('0') {
                frac.pop();
            }
            out.push_str(&frac);
        }
        out.push('Z');
        out
    }
}

impl Default for Time {
    fn default() -> Time {
        Time::ZERO
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rfc3339())
    }
}

impl serde::Serialize for Time {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.rfc3339_nanos())
    }
}

impl<'de> serde::Deserialize<'de> for Time {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Time, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(d)?;
        Time::parse_rfc3339(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("repair: not an RFC3339 timestamp: {s:?}"))
        })
    }
}

fn digits(b: &[u8], at: usize, len: usize) -> Option<i64> {
    if at + len > b.len() {
        return None;
    }
    let mut v: i64 = 0;
    for &c in &b[at..at + len] {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    Some(v)
}

/// Days from 1970-01-01 to y-m-d, proleptic Gregorian (Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse: (year, month, day) for a day count from 1970-01-01 (Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn TestTimeRFC3339Format() {
        assert_eq!(Time::ZERO.rfc3339(), "0001-01-01T00:00:00Z");
        assert_eq!(
            Time::from_unix(1_791_840_600, 0).rfc3339(),
            "2026-10-12T21:30:00Z"
        );
        assert_eq!(Time::ZERO.rfc3339_nanos(), "0001-01-01T00:00:00Z");
    }

    #[test]
    fn TestTimeParseIsTheInverseOfFormat() {
        for s in [
            "0001-01-01T00:00:00Z",
            "2026-10-12T21:30:00Z",
            "2026-10-12T21:30:00.5Z",
            "1969-07-20T20:17:40Z",
        ] {
            let t = Time::parse_rfc3339(s).expect("parses");
            assert_eq!(t.rfc3339_nanos(), s, "round trip of {s}");
        }
        let offset = Time::parse_rfc3339("2026-10-12T23:30:00+02:00").expect("parses");
        assert_eq!(offset, Time::from_unix(1_791_840_600, 0));
        assert_eq!(offset.rfc3339(), "2026-10-12T21:30:00Z");
        assert_eq!(
            Time::from_unix(1_791_840_600, 500_000_000).rfc3339_nanos(),
            "2026-10-12T21:30:00.5Z"
        );
        for bad in [
            "",
            "not a time",
            "2026-10-12T21:30:00",
            "2026-10-12 21:30:00Z",
        ] {
            assert!(Time::parse_rfc3339(bad).is_none(), "{bad} must not parse");
        }
    }

    #[test]
    fn TestTimeJSONRoundTrip() {
        let t = Time::from_unix(1_791_840_600, 0);
        let json = serde_json::to_string(&t).expect("serializes");
        assert_eq!(json, "\"2026-10-12T21:30:00Z\"");
        let back: Time = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back, t);
    }
}
