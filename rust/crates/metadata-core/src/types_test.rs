//! Wire tests for the `types.go` JSON tags, ported from the `cache_test.go`
//! cases that pin them: `TestCacheRoundTrip`, `TestCacheSchema2IsMiss`,
//! `TestCacheLegacyPositiveNoNewFields`, `TestCacheLargeEntryRoundTrip`.
//!
//! The cache record envelope (`key`/`schema`/`notFound`/`checkedAt`) and the
//! store file belong to `metadata::cache`; what is pinned here is the `media`
//! object those records carry, against the exact bytes Go wrote. The Go
//! round-trip fixtures use `time.Now().UTC().Truncate(time.Millisecond)`; the
//! port uses a fixed RFC3339 instant, which tests the same property (the
//! timestamp is carried verbatim) without a clock.

use crate::types::{
    Episode, Kind, Media, join_genres, millis_length, normalize_url, runtime_length,
};

/// The `media` object of the schema-2 record in `TestCacheSchema2IsMiss`.
const LEGACY_SCHEMA2: &str = r#"{"source":"tvmaze","sourceId":"1","kind":1,"title":"Show","fetchedAt":"2026-01-01T00:00:00Z"}"#;

/// The schema-3 record of `TestCacheLegacyPositiveNoNewFields`: a pre-existing
/// positive file with no `notFound`/`checkedAt` and no enriched fields.
const LEGACY_POSITIVE: &str = r#"{"source":"tvmaze","sourceId":"9","kind":1,"title":"Legacy Show","episodes":[{"season":2,"number":1,"title":"Pilot"}],"fetchedAt":"2026-01-01T00:00:00Z"}"#;

#[test]
fn cache_round_trip() {
    let media = Media {
        source: "tvmaze".into(),
        source_id: "169".into(),
        kind: Kind::SERIES,
        title: "Breaking Bad".into(),
        year: 2008,
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".into(),
            ..Episode::default()
        }],
        fetched_at: "2026-01-01T00:00:00Z".into(),
        ..Media::default()
    };
    let encoded = serde_json::to_string(&media).expect("encode");
    assert_eq!(
        encoded,
        r#"{"source":"tvmaze","sourceId":"169","kind":1,"title":"Breaking Bad","year":2008,"episodes":[{"season":1,"number":1,"title":"Pilot"}],"fetchedAt":"2026-01-01T00:00:00Z"}"#
    );
    let got: Media = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(got, media, "round-trip mismatch");
}

/// A schema-2 record's `media` object still decodes field for field; the schema
/// gate that makes it a MISS is the cache's, not the type's.
#[test]
fn schema_2_legacy_media_object() {
    let m: Media = serde_json::from_str(LEGACY_SCHEMA2).expect("decode");
    assert_eq!(m.source, "tvmaze");
    assert_eq!(m.source_id, "1");
    assert_eq!(m.kind, Kind::SERIES);
    assert_eq!(m.kind.value(), 1, "Kind is a plain JSON integer");
    assert_eq!(m.title, "Show");
    assert_eq!(m.year, 0);
    assert!(m.episodes.is_empty());
    assert_eq!(m.fetched_at, "2026-01-01T00:00:00Z");
    assert_eq!(serde_json::to_string(&m).unwrap(), LEGACY_SCHEMA2);
}

#[test]
fn legacy_positive_no_new_fields() {
    let m: Media = serde_json::from_str(LEGACY_POSITIVE).expect("decode");
    assert_eq!(m.source, "tvmaze");
    assert_eq!(m.source_id, "9");
    assert_eq!(m.kind, Kind::SERIES);
    assert_eq!(m.title, "Legacy Show");
    assert_eq!(m.episodes.len(), 1);
    let ep = &m.episodes[0];
    assert_eq!((ep.season, ep.number, ep.title.as_str()), (2, 1, "Pilot"));
    // The pre-enrichment record carries none of the newer per-episode fields.
    assert_eq!(
        (
            ep.artwork.as_str(),
            ep.synopsis.as_str(),
            ep.url.as_str(),
            ep.length.as_str()
        ),
        ("", "", "", "")
    );
    assert_eq!(serde_json::to_string(&m).unwrap(), LEGACY_POSITIVE);
}

/// The retired 64 KiB reader cap: a 256 KiB synopsis round-trips exactly.
#[test]
fn large_entry_round_trip() {
    let synopsis = "s".repeat(256 * 1024);
    let media = Media {
        source: "tvmaze".into(),
        source_id: "41".into(),
        kind: Kind::SERIES,
        title: "Huge Show".into(),
        year: 2008,
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".into(),
            synopsis: synopsis.clone(),
            ..Episode::default()
        }],
        fetched_at: "2026-01-01T00:00:00Z".into(),
        ..Media::default()
    };
    let encoded = serde_json::to_string(&media).expect("encode");
    assert!(
        encoded.len() > 64 * 1024,
        "fixture is {} bytes, want more than the old 64 KiB read cap",
        encoded.len()
    );
    let got: Media = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(got, media, "large round-trip mismatch");
    assert_eq!(got.episodes[0].synopsis.len(), 256 * 1024);
}

/// Every optional tag, verbatim -- the tags `cache_test.go` never exercises.
/// The record carries `"kind":1` as an integer and the exact camelCase/lowercase
/// names the cache reads; a single wrong tag invalidates every cached record.
#[test]
fn every_optional_tag_verbatim() {
    let media = Media {
        source: "tvmaze".into(),
        source_id: "169".into(),
        kind: Kind::SERIES,
        title: "Breaking Bad".into(),
        year: 2008,
        artwork: "https://example.com/poster.jpg".into(),
        synopsis: "A chemistry teacher turns to crime.".into(),
        genres: "Drama \u{00b7} Crime".into(),
        length: "45 min".into(),
        show_url: "https://example.com/show".into(),
        episode_url: "https://example.com/episode".into(),
        movie_url: "https://example.com/movie".into(),
        trailer_url: "https://example.com/trailer".into(),
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".into(),
            artwork: "https://example.com/still.jpg".into(),
            synopsis: "The first one.".into(),
            url: "https://example.com/ep1".into(),
            length: "45 min".into(),
        }],
        episode_count: 0,
        fetched_at: "2026-01-01T00:00:00Z".into(),
    };
    let encoded = serde_json::to_string(&media).expect("encode");
    // U+00B7 is written raw (UTF-8), not escaped, by both serde_json and Go.
    let expected = format!(
        r#"{{"source":"tvmaze","sourceId":"169","kind":1,"title":"Breaking Bad","year":2008,"artwork":"https://example.com/poster.jpg","synopsis":"A chemistry teacher turns to crime.","genres":"Drama {dot} Crime","length":"45 min","showUrl":"https://example.com/show","episodeUrl":"https://example.com/episode","movieUrl":"https://example.com/movie","trailerUrl":"https://example.com/trailer","episodes":[{{"season":1,"number":1,"title":"Pilot","artwork":"https://example.com/still.jpg","synopsis":"The first one.","url":"https://example.com/ep1","length":"45 min"}}],"fetchedAt":"2026-01-01T00:00:00Z"}}"#,
        dot = '\u{00b7}'
    );
    assert_eq!(encoded, expected);
    let got: Media = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(got, media);
}

#[test]
fn media_without_an_episode_count_loads_zero() {
    let media: Media = serde_json::from_str(r#"{"source":"tvmaze","title":"Show"}"#).expect("decode");
    assert_eq!(
        media.episode_count, 0,
        "an old cache record has no episodeCount key"
    );
}

// Additions: no Go test covers these helpers directly, and the brief names each
// one's exact behaviour, so each is pinned here.

#[test]
fn join_genres_trims_and_drops_blanks() {
    let gs = vec![
        " Drama ".to_string(),
        String::new(),
        "   ".to_string(),
        "Crime".to_string(),
    ];
    assert_eq!(join_genres(&gs), "Drama \u{00b7} Crime");
    assert_eq!(join_genres(&[]), "");
}

#[test]
fn normalize_url_upgrades_and_rejects() {
    assert_eq!(normalize_url(""), "");
    assert_eq!(normalize_url("   "), "");
    assert_eq!(
        normalize_url("http://example.com/x"),
        "https://example.com/x"
    );
    assert_eq!(
        normalize_url("  http://example.com/x  "),
        "https://example.com/x"
    );
    assert_eq!(
        normalize_url("https://example.com/x"),
        "https://example.com/x"
    );
    // A bare scheme is rejected: the length must exceed "https://".
    assert_eq!(normalize_url("https://"), "");
    assert_eq!(normalize_url("http://"), "");
    assert_eq!(normalize_url("ftp://example.com/x"), "");
    assert_eq!(normalize_url("/wiki/Film"), "");
}

#[test]
fn millis_length_rounds_half_up() {
    assert_eq!(millis_length(0), "");
    assert_eq!(millis_length(-1), "");
    assert_eq!(millis_length(29_999), "");
    assert_eq!(millis_length(30_000), "1 min");
    assert_eq!(millis_length(45 * 60_000), "45 min");
    assert_eq!(millis_length(60 * 60_000), "1h");
    assert_eq!(millis_length(139 * 60_000), "2h 19m");
    // The +30000 rounds half up: 90 000 ms is 2 min, 89 999 ms is 1 min.
    assert_eq!(millis_length(90_000), "2 min");
    assert_eq!(millis_length(89_999), "1 min");
}

#[test]
fn runtime_length_formats_minutes() {
    assert_eq!(runtime_length(0), "");
    assert_eq!(runtime_length(-5), "");
    assert_eq!(runtime_length(45), "45 min");
}
