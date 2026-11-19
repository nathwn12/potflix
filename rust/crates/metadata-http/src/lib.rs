
pub mod cancel;
pub mod http;
pub mod ratelimit;

pub use cancel::{Cancel, CancelError};
pub use http::{
    backoff_for_attempt, compute_retry_wait, decode_capped, drain_and_close, parse_retry_after,
    retry_wait_for_attempt, Error, HttpClient, ACCEPT_JSON, CONTENT_TYPE_JSON,
    DEFAULT_HTTP_TIMEOUT, DRAIN_BODY_CAP, MAX_429_RETRIES, MAX_BODY_BYTES, MAX_RETRY_WAIT,
    METADATA_USER_AGENT, RETRY_BASE_DELAY, RETRY_JITTER_CAP,
};
pub use ratelimit::{
    current_rate_profile, normalize_rate_host, normalize_rate_profile, rate_interval_for_host,
    reset_rate_limiter, set_host_interval, set_rate_limiting_enabled, set_rate_profile,
    wait_for_host, SCAN_SPEED_BALANCED, SCAN_SPEED_GENTLE, SCAN_SPEED_TURBO,
};
