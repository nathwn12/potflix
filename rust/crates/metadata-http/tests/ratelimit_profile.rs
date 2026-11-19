
use std::time::Duration;

use metadata_http::{
    current_rate_profile, normalize_rate_profile, rate_interval_for_host, set_host_interval,
    set_rate_profile,
};

/// Proves the three named profiles map onto the known hosts with the
/// published intervals as the floor (asserted on the five published ones): turbo is exactly the published values,
/// balanced is 2x, gentle is 4x. Turbo is the default: unknown or empty
/// profiles yield turbo, and an unknown host stays ungated in every profile.
#[test]
fn rate_profile_mapping() {
    let cases = [
        (
            "turbo",
            [
                ("api.tvmaze.com", Duration::from_millis(500)),
                ("graphql.anilist.co", Duration::from_millis(2000)),
                ("api.jikan.moe", Duration::from_millis(350)),
                ("itunes.apple.com", Duration::from_millis(3000)),
                ("en.wikipedia.org", Duration::from_millis(1000)),
            ],
        ),
        (
            "balanced",
            [
                ("api.tvmaze.com", Duration::from_millis(1000)),
                ("graphql.anilist.co", Duration::from_millis(4000)),
                ("api.jikan.moe", Duration::from_millis(700)),
                ("itunes.apple.com", Duration::from_millis(6000)),
                ("en.wikipedia.org", Duration::from_millis(2000)),
            ],
        ),
        (
            "gentle",
            [
                ("api.tvmaze.com", Duration::from_millis(2000)),
                ("graphql.anilist.co", Duration::from_millis(8000)),
                ("api.jikan.moe", Duration::from_millis(1400)),
                ("itunes.apple.com", Duration::from_millis(12000)),
                ("en.wikipedia.org", Duration::from_millis(4000)),
            ],
        ),
    ];

    for (profile, expected) in cases {
        set_rate_profile(profile);
        assert_eq!(current_rate_profile(), profile, "profile {profile:?}");
        for (host, want) in expected {
            let got = rate_interval_for_host(host);
            assert_eq!(got, want, "profile {profile:?} host {host:?}");
        }
        assert_eq!(
            rate_interval_for_host("unknown-host-127-0-0-1.example"),
            Duration::ZERO,
            "profile {profile:?} unknown host must stay ungated"
        );
    }

    for profile in ["", "  ", "ludicrous", "TURBO-fast"] {
        set_rate_profile(profile);
        assert_eq!(current_rate_profile(), "turbo", "profile {profile:?}");
        for (host, want) in cases[0].1 {
            let got = rate_interval_for_host(host);
            assert_eq!(got, want, "profile {profile:?} host {host:?} must be turbo");
        }
        assert_eq!(
            rate_interval_for_host("unknown-host-127-0-0-1.example"),
            Duration::ZERO,
            "profile {profile:?} unknown host must stay ungated"
        );
    }

    set_rate_profile("Turbo");
    assert_eq!(
        rate_interval_for_host("api.tvmaze.com"),
        Duration::from_millis(500),
        "profile Turbo"
    );
    set_rate_profile("GENTLE");
    assert_eq!(
        rate_interval_for_host("api.tvmaze.com"),
        Duration::from_millis(2000),
        "profile GENTLE"
    );

    set_host_interval("test-ratelimit-profile.example", Duration::from_millis(120));
    set_rate_profile("gentle");
    assert_eq!(
        rate_interval_for_host("test-ratelimit-profile.example"),
        Duration::from_millis(120),
        "an injected host must survive a profile switch"
    );
    set_host_interval("test-ratelimit-profile.example", Duration::ZERO);

    assert_eq!(normalize_rate_profile(" Balanced "), "balanced");
    assert_eq!(normalize_rate_profile("GENTLE"), "gentle");
    assert_eq!(normalize_rate_profile("nope"), "turbo");

    set_rate_profile("turbo");
}
