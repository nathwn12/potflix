
use std::fmt;
use std::io::{self, Read};
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::Serialize;
use ureq::http::{Response, StatusCode, Uri};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::Body;

use crate::cancel::{Cancel, CancelError};
use crate::ratelimit::wait_for_host;

/// The shared `User-Agent` every provider sends (http.go:16).
pub const METADATA_USER_AGENT: &str = "potflix-metadata/1.0";
/// The shared `Accept` (http.go:106).
pub const ACCEPT_JSON: &str = "application/json";
/// The POST payload media type (http.go:124).
pub const CONTENT_TYPE_JSON: &str = "application/json";
/// Response body cap (http.go:17).
pub const MAX_BODY_BYTES: u64 = 4 << 20;
/// Upper bound on an honored `Retry-After` (http.go:18).
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);
/// Maximum 429 retries after the initial attempt: 1 initial + up to 3
/// retries = 4 requests worst case. Small and bounded: a persistently
/// rate-limited host hands its last 429 back instead of looping.
pub const MAX_429_RETRIES: u32 = 3;
/// Base for the exponential backoff when no usable `Retry-After` arrives:
/// 200 ms, doubling per retry (200/400/800 ms), capped by
/// [`MAX_RETRY_WAIT`].
pub const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);
/// Upper bound on the retry jitter. Every 429 wait adds 0..=100 ms of
/// std-only jitter so concurrent lookups spread instead of retrying in
/// lockstep.
pub const RETRY_JITTER_CAP: Duration = Duration::from_millis(100);
/// Post-decode drain cap that returns a connection to the pool (http.go:22).
pub const DRAIN_BODY_CAP: u64 = 512 << 10;
/// Pre-close drain cap (http.go:75, :133, :144).
const CLOSE_DRAIN_BYTES: u64 = 64 << 10;
/// `defaultHTTPTimeout` (http.go:25).
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// A transport, request-build, cancellation, or decode failure.
///
/// `Status` and `Decode` carry only the status line or the JSON error: a
/// response body must never reach an error message or a log.
#[derive(Debug)]
pub enum Error {
    /// `ctx.Err()`: the scope was cancelled or its deadline passed.
    Cancelled(CancelError),
    /// The request could not be built (an invalid URL).
    Url(String),
    /// The POST payload could not be serialized (`json.Marshal` failure).
    Encode(serde_json::Error),
    /// The request failed in transport (`client.Do` failure).
    Transport(ureq::Error),
    /// A non-2xx status (`decodeCapped`).
    Status(StatusCode),
    /// The JSON body did not decode (`json.Decode` failure).
    Decode(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Cancelled(reason) => write!(f, "metadata: {reason}"),
            Error::Url(reason) => write!(f, "metadata: invalid request URL: {reason}"),
            Error::Encode(err) => write!(f, "metadata: encode request: {err}"),
            Error::Transport(err) => write!(f, "metadata: request failed: {err}"),
            Error::Status(status) => {
                write!(f, "metadata: unexpected HTTP status {status}")
            }
            Error::Decode(err) => write!(f, "metadata: decode response: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Cancelled(reason) => Some(reason),
            Error::Encode(err) | Error::Decode(err) => Some(err),
            Error::Transport(err) => Some(err),
            Error::Url(_) | Error::Status(_) => None,
        }
    }
}

/// `newHTTPClient`: the shared client every provider fetches through. Cloning
/// it shares the connection pool, so clone it rather than building another
/// (http.go:27).
#[derive(Clone, Debug)]
pub struct HttpClient {
    agent: ureq::Agent,
}

impl HttpClient {
    /// A client whose whole-request budget is `timeout`; a zero timeout takes
    /// [`DEFAULT_HTTP_TIMEOUT`] (http.go:28).
    pub fn new(timeout: Duration) -> Self {
        let timeout = if timeout.is_zero() {
            DEFAULT_HTTP_TIMEOUT
        } else {
            timeout
        };
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .http_status_as_error(false)
            .build();
        HttpClient {
            agent: config.into(),
        }
    }

    /// `fetchGet`: a GET with the shared `User-Agent`/`Accept` and bounded
    /// 429 retries (http.go:99). GET is idempotent, so it retries.
    pub fn get(&self, cancel: &Cancel, url: &str) -> Result<Response<Body>, Error> {
        self.fetch(cancel, url, None, "", true)
    }

    /// `fetchGet` with a per-request `Accept` override (empty means the shared
    /// one). Kitsu's JSON:API answers `406 Not Acceptable` unless the request
    /// carries `Accept: application/vnd.api+json`, so that one provider opts
    /// out of the shared `application/json` here; pool, timeout, limiter and
    /// retry are unchanged.
    pub fn get_with_accept(
        &self,
        cancel: &Cancel,
        url: &str,
        accept: &str,
    ) -> Result<Response<Body>, Error> {
        self.fetch(cancel, url, None, accept, true)
    }

    /// `fetchPostJSON`: a JSON POST with the shared `User-Agent`/`Accept` and
    /// `Content-Type: application/json` (http.go:112).
    ///
    /// NEVER retried: POST is not idempotent in general, so a 429 (or any
    /// other status) is handed straight back. Read-only GraphQL search callers
    /// that can safely retry use [`HttpClient::post_json_idempotent`] instead,
    /// which documents the opt-in at the call site.
    pub fn post_json<T: Serialize>(
        &self,
        cancel: &Cancel,
        url: &str,
        payload: &T,
    ) -> Result<Response<Body>, Error> {
        let body = serde_json::to_vec(payload).map_err(Error::Encode)?;
        self.fetch(cancel, url, Some(body), "", false)
    }

    /// A JSON POST for read-only search queries (AniList / JustWatch GraphQL
    /// `search`): same wire shape as [`HttpClient::post_json`], but the
    /// caller asserts the payload is a side-effect-free search, so 429s retry
    /// with the shared backoff+jitter policy exactly like GET. Any future
    /// mutating POST must keep using [`HttpClient::post_json`] (no retry).
    pub fn post_json_idempotent<T: Serialize>(
        &self,
        cancel: &Cancel,
        url: &str,
        payload: &T,
    ) -> Result<Response<Body>, Error> {
        let body = serde_json::to_vec(payload).map_err(Error::Encode)?;
        self.fetch(cancel, url, Some(body), "", true)
    }

    /// `fetchWithRetry`: runs the request once, and on HTTP 429 retries an
    /// idempotent request up to [`MAX_429_RETRIES`] times (http.go:56).
    /// Non-idempotent calls never retry.
    ///
    /// Each retry waits for the server's `Retry-After` when present and
    /// usable (bounded by [`MAX_RETRY_WAIT`]), else the exponential
    /// [`RETRY_BASE_DELAY`] backoff, plus 0..=[`RETRY_JITTER_CAP`] of jitter
    /// so concurrent lookups do not retry in lockstep. Every attempt is spaced
    /// by the per-host limiter (unknown hosts go ungated) (http.go:56). A
    /// persistently rate-limited host hands its last 429 back for
    /// `decode_capped` to report; 429 is a response, never a transport error.
    fn fetch(
        &self,
        cancel: &Cancel,
        url: &str,
        payload: Option<Vec<u8>>,
        accept: &str,
        idempotent: bool,
    ) -> Result<Response<Body>, Error> {
        let uri: Uri = url
            .parse()
            .map_err(|err: ureq::http::uri::InvalidUri| Error::Url(err.to_string()))?;
        let mut attempt: u32 = 0;
        loop {
            let response = self.attempt(cancel, &uri, payload.as_deref(), accept)?;
            if response.status().as_u16() != StatusCode::TOO_MANY_REQUESTS.as_u16() {
                return Ok(response);
            }
            if !idempotent || attempt >= MAX_429_RETRIES {
                return Ok(response);
            }
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let server_wait = parse_retry_after(retry_after, SystemTime::now());
            let wait = retry_wait_for_attempt(server_wait, attempt);
            drain_and_close(response);
            cancel.wait(wait).map_err(Error::Cancelled)?;
            attempt += 1;
        }
    }

    fn attempt(
        &self,
        cancel: &Cancel,
        uri: &Uri,
        payload: Option<&[u8]>,
        accept: &str,
    ) -> Result<Response<Body>, Error> {
        wait_for_host(cancel, uri.host().unwrap_or_default()).map_err(Error::Cancelled)?;
        if let Some(reason) = cancel.err() {
            return Err(Error::Cancelled(reason));
        }
        let accept = if accept.is_empty() { ACCEPT_JSON } else { accept };
        let result = match payload {
            Some(body) => self
                .agent
                .post(uri.clone())
                .header("User-Agent", METADATA_USER_AGENT)
                .header("Accept", accept)
                .header("Content-Type", CONTENT_TYPE_JSON)
                .send(body),
            None => self
                .agent
                .get(uri.clone())
                .header("User-Agent", METADATA_USER_AGENT)
                .header("Accept", accept)
                .call(),
        };
        result.map_err(Error::Transport)
    }
}

/// Exponential backoff for retry `attempt` (0-based) when the server sent no
/// usable `Retry-After`: [`RETRY_BASE_DELAY`] doubling per retry
/// (200/400/800 ms for attempts 0/1/2), capped by [`MAX_RETRY_WAIT`].
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    let shift = attempt.min(6);
    RETRY_BASE_DELAY
        .saturating_mul(1 << shift)
        .min(MAX_RETRY_WAIT)
}

/// Std-only jitter for retry `attempt`: 0..=[`RETRY_JITTER_CAP`], mixed from
/// the wall clock and the attempt number so concurrent lookups spread instead
/// of retrying in lockstep. No third-party RNG: the workspace pins exact
/// versions and `--locked` is the build norm, so randomness here is
/// deliberately `std`-only. Bounds are exact; the value within them is not.
fn jitter_for_attempt(attempt: u32) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0x9E37_79B9, |elapsed| elapsed.subsec_nanos());
    let mixed = nanos
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add(attempt.wrapping_mul(0x85EB_CA6B));
    Duration::from_nanos((mixed % (RETRY_JITTER_CAP.as_nanos() as u32 + 1)) as u64)
}

/// The 429 wait for retry `attempt`: the server's `Retry-After` when present
/// and positive (bounded by [`MAX_RETRY_WAIT`]), else
/// [`backoff_for_attempt`], plus `jitter`. The total never exceeds
/// `MAX_RETRY_WAIT + RETRY_JITTER_CAP`. Pure for tests: production callers
/// pass [`jitter_for_attempt`] via [`retry_wait_for_attempt`].
pub fn compute_retry_wait(server_wait: Duration, attempt: u32, jitter: Duration) -> Duration {
    let jitter = jitter.min(RETRY_JITTER_CAP);
    let base = if server_wait.is_zero() {
        backoff_for_attempt(attempt)
    } else {
        server_wait.min(MAX_RETRY_WAIT)
    };
    (base + jitter).min(MAX_RETRY_WAIT + RETRY_JITTER_CAP)
}

/// [`compute_retry_wait`] with internally sampled [`jitter_for_attempt`]:
/// the single policy every idempotent 429 retry uses, here and in
/// `metadata::wikipedia`'s header-divergent client.
pub fn retry_wait_for_attempt(server_wait: Duration, attempt: u32) -> Duration {
    compute_retry_wait(server_wait, attempt, jitter_for_attempt(attempt))
}

/// `parseRetryAfter`: seconds, or an HTTP-date, or zero when the value is
/// empty, malformed, or already in the past (http.go:34).
pub fn parse_retry_after(value: &str, now: SystemTime) -> Duration {
    let value = value.trim();
    if value.is_empty() {
        return Duration::ZERO;
    }
    if let Ok(secs) = value.parse::<i64>() {
        return if secs < 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(secs as u64)
        };
    }
    if let Some(at) = parse_http_date(value)
        && let Ok(wait) = at.duration_since(now)
    {
        return wait;
    }
    Duration::ZERO
}

/// `drainAndClose`: reads a bounded prefix of the body and drops the response,
/// so a body the caller did not consume does not cost the pooled connection
/// (http.go:129).
pub fn drain_and_close(response: Response<Body>) {
    let mut body = response.into_body();
    drain(&mut body, CLOSE_DRAIN_BYTES);
}

/// `decodeCapped`: decodes a JSON body capped at [`MAX_BODY_BYTES`]. A non-2xx
/// status is an error; the body is never logged. The remainder is drained
/// (bounded by [`DRAIN_BODY_CAP`]) before the response is dropped so the
/// connection is returned to the pool; a body past the cap is legitimately
/// discarded (http.go:141).
pub fn decode_capped<T: DeserializeOwned>(response: Response<Body>) -> Result<T, Error> {
    let status = response.status();
    let mut body = response.into_body();
    if !status.is_success() {
        drain(&mut body, CLOSE_DRAIN_BYTES);
        return Err(Error::Status(status));
    }
    let decoded = {
        let reader = body.as_reader().take(MAX_BODY_BYTES);
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        T::deserialize(&mut deserializer)
    };
    drain(&mut body, DRAIN_BODY_CAP);
    decoded.map_err(Error::Decode)
}

fn drain(body: &mut Body, cap: u64) {
    let mut reader = body.as_reader().take(cap);
    let _ = io::copy(&mut reader, &mut io::sink());
}

/// `http.ParseTime`: RFC 1123 (`Sun, 06 Nov 1994 08:49:37 GMT`), RFC 850
/// (`Sunday, 06-Nov-94 08:49:37 GMT`) and ANSI C asctime
/// (`Sun Nov  6 08:49:37 1994`).
///
/// Named reading: Go tries the three layouts with `time.Parse` and also
/// accepts a named zone in place of `GMT`; this accepts a `GMT`/`UTC` zone
/// only, which is what RFC 7231 requires of a `Retry-After` date.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    if !value.is_ascii() {
        return None;
    }
    let normalized = value.replace([',', '-'], " ");
    let fields: Vec<&str> = normalized.split_whitespace().collect();
    let (year, month, day, time) = match fields.as_slice() {
        [_weekday, day, month, year, time, zone] => {
            if !is_gmt(zone) {
                return None;
            }
            (*year, *month, *day, *time)
        }
        [_weekday, month, day, time, year] => (*year, *month, *day, *time),
        _ => return None,
    };

    let year: i64 = match year.len() {
        4 => year.parse().ok()?,
        2 => {
            let short: i64 = year.parse().ok()?;
            if short >= 69 {
                1900 + short
            } else {
                2000 + short
            }
        }
        _ => return None,
    };
    let month = month_number(month)?;
    let day: i64 = day.parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let (hour, minute, second) = parse_clock(time)?;
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    if secs < 0 {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

fn is_gmt(zone: &str) -> bool {
    zone.eq_ignore_ascii_case("GMT") || zone.eq_ignore_ascii_case("UTC")
}

fn month_number(name: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| month.eq_ignore_ascii_case(name))
        .map(|index| index as i64 + 1)
}

fn parse_clock(time: &str) -> Option<(i64, i64, i64)> {
    let mut parts = time.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=60).contains(&second) {
        return None;
    }
    Some((hour, minute, second))
}

/// Days from 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`), verified against the epoch arithmetic of Go's `time`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400; // [0, 399]
    let month_index = (month + 9) % 12; // Mar = 0
    let day_of_year = (153 * month_index + 2) / 5 + day - 1; // [0, 365]
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year; // [0, 146096]
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds_dates_and_junk() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(parse_retry_after("", now), Duration::ZERO);
        assert_eq!(parse_retry_after("   ", now), Duration::ZERO);
        assert_eq!(parse_retry_after("5", now), Duration::from_secs(5));
        assert_eq!(parse_retry_after(" 12 ", now), Duration::from_secs(12));
        assert_eq!(parse_retry_after("-3", now), Duration::ZERO);
        assert_eq!(parse_retry_after("soon", now), Duration::ZERO);
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        assert_eq!(
            parse_retry_after("Sun, 09 Sep 2001 01:46:40 GMT", at),
            Duration::ZERO
        );
        assert_eq!(
            parse_retry_after(
                "Sun, 09 Sep 2001 01:46:40 GMT",
                at - Duration::from_secs(30)
            ),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_retry_after(
                "Sunday, 09-Sep-01 01:46:40 GMT",
                at - Duration::from_secs(30)
            ),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_retry_after("Sun Sep  9 01:46:40 2001", at - Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_retry_after(
                "Sun, 09 Sep 2001 01:46:40 PST",
                at - Duration::from_secs(30)
            ),
            Duration::ZERO
        );
    }

    #[test]
    fn backoff_doubles_per_attempt_and_caps() {
        assert_eq!(backoff_for_attempt(0), Duration::from_millis(200));
        assert_eq!(backoff_for_attempt(1), Duration::from_millis(400));
        assert_eq!(backoff_for_attempt(2), Duration::from_millis(800));
        assert_eq!(backoff_for_attempt(3), Duration::from_millis(1600));
        assert_eq!(backoff_for_attempt(100), MAX_RETRY_WAIT);
    }

    #[test]
    fn compute_retry_wait_honors_retry_after_and_caps() {
        let jitter = Duration::from_millis(50);
        assert_eq!(
            compute_retry_wait(Duration::ZERO, 0, jitter),
            Duration::from_millis(250)
        );
        assert_eq!(
            compute_retry_wait(Duration::ZERO, 1, jitter),
            Duration::from_millis(450)
        );
        assert_eq!(
            compute_retry_wait(Duration::from_secs(2), 0, jitter),
            Duration::from_millis(2050)
        );
        assert_eq!(
            compute_retry_wait(Duration::from_secs(3600), 0, jitter),
            MAX_RETRY_WAIT + jitter
        );
        assert_eq!(
            compute_retry_wait(Duration::ZERO, 100, jitter),
            MAX_RETRY_WAIT + jitter
        );
        assert_eq!(
            compute_retry_wait(Duration::ZERO, 0, Duration::from_secs(60)),
            RETRY_BASE_DELAY + RETRY_JITTER_CAP
        );
    }

    #[test]
    fn retry_wait_for_attempt_stays_in_bounds() {
        for attempt in 0..=MAX_429_RETRIES {
            let wait = retry_wait_for_attempt(Duration::ZERO, attempt);
            let base = backoff_for_attempt(attempt);
            assert!(
                wait >= base,
                "attempt {attempt}: {wait:?} must carry the {base:?} backoff floor"
            );
            assert!(
                wait <= base + RETRY_JITTER_CAP,
                "attempt {attempt}: {wait:?} must not exceed backoff + jitter"
            );
        }
        let honoring = retry_wait_for_attempt(Duration::from_secs(3), 0);
        assert!(
            honoring >= Duration::from_secs(3)
                && honoring <= Duration::from_secs(3) + RETRY_JITTER_CAP,
            "a 3 s Retry-After must be honored plus jitter, got {honoring:?}"
        );
    }
}
