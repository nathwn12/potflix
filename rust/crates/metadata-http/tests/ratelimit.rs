
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use metadata_http::{
    rate_interval_for_host, reset_rate_limiter, set_host_interval, set_rate_limiting_enabled,
    wait_for_host, Cancel,
};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Proves the per-host limiter still throttles real provider hosts: N
/// acquisitions for a fake host with a known interval must be spaced by
/// (almost) the full interval.
#[test]
fn rate_limiter_spacing() {
    const HOST: &str = "test-ratelimit.example";
    const INTERVAL: Duration = Duration::from_millis(100);
    const N: usize = 4;
    const TOLERANCE: Duration = Duration::from_millis(25);

    let _guard = serial();
    set_host_interval(HOST, INTERVAL);
    reset_rate_limiter();

    let cancel = Cancel::background();
    let mut times = Vec::with_capacity(N);
    for i in 0..N {
        wait_for_host(&cancel, HOST).unwrap_or_else(|err| panic!("wait_for_host {i}: {err}"));
        times.push(Instant::now());
    }
    for i in 1..N {
        let gap = times[i].duration_since(times[i - 1]);
        assert!(
            gap >= INTERVAL - TOLERANCE,
            "gap {i} = {gap:?}, want >= {:?} (interval {INTERVAL:?})",
            INTERVAL - TOLERANCE
        );
    }
    let total = times[N - 1].duration_since(times[0]);
    let want = INTERVAL * (N as u32 - 1) - TOLERANCE;
    assert!(
        total >= want,
        "total = {total:?}, want >= {want:?} for {N} spaced acquisitions"
    );

    set_host_interval(HOST, Duration::ZERO);
    reset_rate_limiter();
}

/// Proves any host without a configured interval (tests, loopback fakes,
/// future sources) is never throttled.
#[test]
fn rate_limiter_unknown_host_ungated() {
    let _guard = serial();
    reset_rate_limiter();
    let cancel = Cancel::background();
    let start = Instant::now();
    for _ in 0..10 {
        wait_for_host(&cancel, "unknown-host-127-0-0-1.example").expect("wait_for_host");
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "10 ungated acquisitions took {elapsed:?}, want < 100ms"
    );
    reset_rate_limiter();
}

/// Proves the limiter can be switched off outright.
#[test]
fn rate_limiter_disabled() {
    const HOST: &str = "test-ratelimit-disabled.example";

    let _guard = serial();
    set_host_interval(HOST, Duration::from_millis(500));
    reset_rate_limiter();
    set_rate_limiting_enabled(false);

    let interval = rate_interval_for_host(HOST);
    assert_eq!(
        interval,
        Duration::ZERO,
        "rate_interval_for_host with the limiter disabled = {interval:?}, want 0"
    );

    let cancel = Cancel::background();
    let start = Instant::now();
    for _ in 0..3 {
        wait_for_host(&cancel, HOST).expect("wait_for_host");
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "3 disabled acquisitions took {elapsed:?}, want < 100ms"
    );

    set_rate_limiting_enabled(true);
    set_host_interval(HOST, Duration::ZERO);
    reset_rate_limiter();
}
