
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::cancel::{Cancel, CancelError};

/// The published intervals (ratelimit.go:17-30), except AniList: the docs state
/// a normal 90/min ceiling but the API is currently degraded to 30/min, so the
/// limiter holds the degraded figure (2000 ms) until the docs say otherwise.
/// A fixed interval fits this limiter's design (one spacing per host, plus the
/// bounded `Retry-After` retry in `http.rs`); adopting `X-RateLimit-Limit`
/// would need response headers plumbed back into the limiter, a new subsystem
/// for no functional gain in a one-title-at-a-time desktop app.
const DEFAULT_HOST_INTERVALS: &[(&str, Duration)] = &[
    ("api.tvmaze.com", Duration::from_millis(500)),
    ("graphql.anilist.co", Duration::from_millis(2000)),
    ("api.jikan.moe", Duration::from_millis(350)),
    ("itunes.apple.com", Duration::from_millis(3000)),
    ("en.wikipedia.org", Duration::from_millis(1000)),
    ("v3-cinemeta.strem.io", Duration::from_millis(1000)),
    ("api.tenrai.org", Duration::from_millis(500)),
    ("kitsu.io", Duration::from_millis(1000)),
    ("apis.justwatch.com", Duration::from_millis(2000)),
    ("www.episodate.com", Duration::from_millis(2000)),
    ("api.bgm.tv", Duration::from_millis(2000)),
    ("www.livechart.me", Duration::from_millis(2000)),
    ("cdn.animenewsnetwork.com", Duration::from_millis(2000)),
];

/// Turbo: the published intervals exactly. The default, and the fastest.
pub const SCAN_SPEED_TURBO: &str = "turbo";
/// Balanced: 2x the published intervals.
pub const SCAN_SPEED_BALANCED: &str = "balanced";
/// Gentle: 4x the published intervals.
pub const SCAN_SPEED_GENTLE: &str = "gentle";

/// One host's spacing gate: the instant of the last acquisition (ratelimit.go:111).
type Gate = Arc<Mutex<Option<Instant>>>;

/// Process-global limiter state (ratelimit.go:32-44).
struct Limiter {
    intervals: HashMap<String, Duration>,
    gates: HashMap<String, Gate>,
    enabled: bool,
    profile: &'static str,
}

impl Limiter {
    fn new() -> Self {
        let mut intervals = HashMap::with_capacity(DEFAULT_HOST_INTERVALS.len());
        for (host, interval) in DEFAULT_HOST_INTERVALS {
            intervals.insert((*host).to_string(), *interval);
        }
        Limiter {
            intervals,
            gates: HashMap::new(),
            enabled: true,
            profile: SCAN_SPEED_TURBO,
        }
    }
}

static LIMITER: OnceLock<Mutex<Limiter>> = OnceLock::new();

fn lock() -> MutexGuard<'static, Limiter> {
    LIMITER
        .get_or_init(|| Mutex::new(Limiter::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Maps any input onto a known profile: turbo, balanced, and gentle pass
/// through (case-insensitive, trimmed); empty and unknown yield turbo
/// (ratelimit.go:58).
pub fn normalize_rate_profile(p: &str) -> &'static str {
    match p.trim().to_lowercase().as_str() {
        SCAN_SPEED_BALANCED => SCAN_SPEED_BALANCED,
        SCAN_SPEED_GENTLE => SCAN_SPEED_GENTLE,
        _ => SCAN_SPEED_TURBO,
    }
}

/// Maps a named profile onto the thirteen known hosts, leaving any other
/// (test/fake) host entry untouched so `set_host_interval` keeps working.
/// Unknown/empty profiles yield turbo (ratelimit.go:72).
pub fn set_rate_profile(profile: &str) {
    let profile = normalize_rate_profile(profile);
    let multiplier = match profile {
        SCAN_SPEED_BALANCED => 2,
        SCAN_SPEED_GENTLE => 4,
        _ => 1,
    };
    let mut limiter = lock();
    for (host, interval) in DEFAULT_HOST_INTERVALS {
        limiter
            .intervals
            .insert((*host).to_string(), *interval * multiplier);
    }
    limiter.profile = profile;
}

/// The last profile applied via [`set_rate_profile`] (turbo until the first
/// call) (ratelimit.go:94).
pub fn current_rate_profile() -> &'static str {
    lock().profile
}

/// The effective spacing for `host`: zero when the host is ungated, unknown, or
/// rate limiting is disabled (ratelimit.go:106).
pub fn rate_interval_for_host(host: &str) -> Duration {
    let host = normalize_rate_host(host);
    if host.is_empty() {
        return Duration::ZERO;
    }
    let limiter = lock();
    if !limiter.enabled {
        return Duration::ZERO;
    }
    limiter
        .intervals
        .get(&host)
        .copied()
        .unwrap_or(Duration::ZERO)
}

/// Injects or overrides one host's spacing (tests use a fake host). A
/// non-positive `d` removes the entry, leaving the host ungated
/// (ratelimit.go:148).
pub fn set_host_interval(host: &str, d: Duration) {
    let host = normalize_rate_host(host);
    if host.is_empty() {
        return;
    }
    let mut limiter = lock();
    if d.is_zero() {
        limiter.intervals.remove(&host);
        return;
    }
    limiter.intervals.insert(host, d);
}

/// Toggles all per-host gating (tests may disable it outright). Unknown hosts
/// are ungated either way (ratelimit.go:167).
pub fn set_rate_limiting_enabled(enabled: bool) {
    lock().enabled = enabled;
}

/// Clears per-host last-fire times (timing state only; configured intervals are
/// kept) (ratelimit.go:175). A test seam, like the Go original.
#[doc(hidden)]
pub fn reset_rate_limiter() {
    lock().gates.clear();
}

/// Lowercases, trims, and strips any `:port` (including `127.0.0.1:port` and
/// bracketed IPv6) plus one trailing dot, so map lookups compare bare
/// hostnames (ratelimit.go:119).
pub fn normalize_rate_host(h: &str) -> String {
    let h = h.trim();
    if h.is_empty() {
        return String::new();
    }
    let h = split_host_port(h).map_or(h, |(host, _port)| host);
    h.strip_suffix('.').unwrap_or(h).to_lowercase()
}

/// `net.SplitHostPort`: splits on the last colon, `[v6]:port` for bracketed
/// literals; `None` when the address carries no port or is an unbracketed IPv6
/// literal (the Go function's "too many colons" error).
fn split_host_port(h: &str) -> Option<(&str, &str)> {
    if let Some(rest) = h.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        return after.strip_prefix(':').map(|port| (host, port));
    }
    let (host, port) = h.rsplit_once(':')?;
    if host.contains(':') {
        return None;
    }
    Some((host, port))
}

fn gate_for_host(limiter: &mut Limiter, host: &str) -> Gate {
    Arc::clone(limiter.gates.entry(host.to_string()).or_default())
}

/// Waits until `host`'s interval has elapsed since the last acquisition, then
/// records now. Unknown hosts and disabled limiting return immediately. A
/// cancelled or expired scope aborts the wait (ratelimit.go:197).
///
/// Concurrent callers for the same host serialize on that host's gate while
/// other hosts proceed independently.
pub fn wait_for_host(cancel: &Cancel, host: &str) -> Result<(), CancelError> {
    let host = normalize_rate_host(host);
    if host.is_empty() {
        return Ok(());
    }
    let (interval, gate) = {
        let mut limiter = lock();
        if !limiter.enabled {
            return Ok(());
        }
        let interval = limiter
            .intervals
            .get(&host)
            .copied()
            .unwrap_or(Duration::ZERO);
        if interval.is_zero() {
            return Ok(());
        }
        (interval, gate_for_host(&mut limiter, &host))
    };
    let mut last = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(previous) = *last {
        let elapsed = previous.elapsed();
        if elapsed < interval {
            cancel.wait(interval - elapsed)?;
        }
    }
    *last = Some(Instant::now());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_host_strips_port_brackets_and_dot() {
        assert_eq!(normalize_rate_host("API.TVmaze.com:443"), "api.tvmaze.com");
        assert_eq!(normalize_rate_host("[::1]:8080"), "::1");
        assert_eq!(normalize_rate_host("Example.COM."), "example.com");
        assert_eq!(normalize_rate_host("  example.com:80  "), "example.com");
        assert_eq!(normalize_rate_host("[::1]"), "[::1]");
        assert_eq!(normalize_rate_host("::1"), "::1");
        assert_eq!(normalize_rate_host(":8080"), "");
    }

    #[test]
    fn split_host_port_matches_net_split_host_port() {
        assert_eq!(
            split_host_port("example.com:80"),
            Some(("example.com", "80"))
        );
        assert_eq!(split_host_port("example.com"), None);
        assert_eq!(split_host_port("[::1]:80"), Some(("::1", "80")));
        assert_eq!(split_host_port("[::1]"), None);
        assert_eq!(split_host_port("a:b:c"), None);
        assert_eq!(split_host_port(":80"), Some(("", "80")));
    }
}
