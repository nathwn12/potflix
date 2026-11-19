
use std::sync::Arc;

use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;

use crate::artwork::{MAX_SYNOPSIS_UNITS, cap_synopsis, first_non_empty, strip_html};
use crate::cache::{cache_file_name, cache_key, cache_load, cache_store};
use crate::resolver::Source;
use crate::testsupport::{FakeServer, FakeSource, Request, Response, TempDir, resolver_with};
use crate::tvmaze::TvmazeSource;

/// `TestStripHTML`.
#[test]
fn strip_html_drops_tags_and_decodes_entities() {
    assert_eq!(
        strip_html("<p>Hello <b>World</b></p><br>next line"),
        "Hello World next line"
    );
    assert_eq!(
        strip_html("A &amp; B &lt;C&gt; &quot;Q&quot; &#39;S&#39;&nbsp;&nbsp;end"),
        "A & B <C> \"Q\" 'S' end"
    );
    assert_eq!(
        strip_html("  line one\n\nline   two\r\nline three  "),
        "line one line two line three"
    );
    assert_eq!(strip_html(""), "");
    assert_eq!(strip_html("a < b"), "a < b");
}

/// `TestFirstNonEmpty`.
#[test]
fn first_non_empty_trims_and_skips_blanks() {
    assert_eq!(first_non_empty(&["", "  ", "b", "c"]), "b");
    assert_eq!(first_non_empty(&["", " "]), "");
}

/// `TestCapSynopsis`.
#[test]
fn cap_synopsis_caps_at_a_sentence_or_word_boundary() {
    assert_eq!(
        MAX_SYNOPSIS_UNITS, 128,
        "maxSynopsisUnits must stay 128 (Discord 128-unit asset-text cap)"
    );
    let short = "A short synopsis.";
    assert_eq!(cap_synopsis(short), short);

    let ascii_at_cap = "a".repeat(MAX_SYNOPSIS_UNITS);
    assert_eq!(cap_synopsis(&ascii_at_cap), ascii_at_cap);

    let long = "word ".repeat(100);
    let long = long.trim();
    let got = cap_synopsis(long);
    assert!(
        got.encode_utf16().count() <= MAX_SYNOPSIS_UNITS,
        "capSynopsis units = {}",
        got.encode_utf16().count()
    );
    assert!(
        !got.ends_with("wor") && !got.ends_with('w'),
        "capSynopsis cut mid-word: {got:?}"
    );
    assert!(got.ends_with("word"), "capSynopsis word boundary = {got:?}");

    let sentences = format!(
        "First sentence ends here. {}",
        "filler text here. ".repeat(30)
    );
    let got = cap_synopsis(&sentences);
    assert!(got.encode_utf16().count() <= MAX_SYNOPSIS_UNITS);
    assert!(
        got.ends_with('.'),
        "capSynopsis sentence boundary = {got:?}"
    );

    let emoji = "\u{1F600}".repeat(MAX_SYNOPSIS_UNITS);
    let got = cap_synopsis(&emoji);
    assert!(
        got.encode_utf16().count() <= MAX_SYNOPSIS_UNITS,
        "capSynopsis emoji units = {}",
        got.encode_utf16().count()
    );
    assert_eq!(got, "\u{1F600}".repeat(MAX_SYNOPSIS_UNITS / 2));
}

/// `TestCacheSchemaMismatchIsMiss`: pre-art entries (Schema 1 or absent) are a
/// MISS while current-schema entries still hit.
#[test]
fn cache_schema_mismatch_is_miss() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = Query {
        kind: Kind::SERIES,
        title: "Show".to_string(),
        season: 1,
        ..Query::default()
    };
    let key = cache_key(&q);

    let legacy = format!(
        "{{\"key\":{key:?},\"media\":{{\"source\":\"tvmaze\",\"sourceId\":\"1\",\"kind\":1,\"title\":\"Show\",\"fetchedAt\":\"2026-01-01T00:00:00Z\"}}}}"
    );
    std::fs::write(format!("{d}/{}", cache_file_name(&key)), legacy).unwrap();
    assert!(
        cache_load(&d, &key).is_none(),
        "schema-less legacy entry should be a miss"
    );

    let schema1 = format!(
        "{{\"key\":{key:?},\"schema\":1,\"media\":{{\"source\":\"tvmaze\",\"sourceId\":\"1\",\"kind\":1,\"title\":\"Show\",\"fetchedAt\":\"2026-01-01T00:00:00Z\"}}}}"
    );
    std::fs::write(format!("{d}/{}", cache_file_name(&key)), schema1).unwrap();
    assert!(
        cache_load(&d, &key).is_none(),
        "Schema:1 entry should be a miss"
    );

    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Show".to_string(),
        artwork: "https://example.com/poster.jpg".to_string(),
        synopsis: "A show.".to_string(),
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    cache_store(&d, &key, &media).expect("cacheStore");
    let got = cache_load(&d, &key).expect("current-schema entry should hit");
    assert_eq!(got.artwork, media.artwork);
    assert_eq!(got.synopsis, media.synopsis);
}

/// `TestTVMazeArtworkFixture`: a provider fixture yields non-empty
/// Artwork+Synopsis at both show and episode level, and the Resolution carries
/// the SERIES poster plus the episode-preferred synopsis.
#[test]
fn tvmaze_artwork_fixture() {
    let server = FakeServer::start(|req: &Request| match req.path.as_str() {
        "/search/shows" => Response::json(
            r#"[{"score":1.0,"show":{"id":169,"name":"Breaking Bad","premiered":"2008-01-20","weight":99,"image":{"medium":"https://example.com/show-medium.jpg","original":"https://example.com/show-original.jpg"},"summary":"<p>A chemistry teacher turns cook.</p>"}}]"#,
        ),
        "/shows/169/seasons" => Response::json(r#"[{"id":753,"number":1}]"#),
        "/seasons/753/episodes" => Response::json(
            r#"[{"id":12192,"name":"Pilot","season":1,"number":1,"image":{"medium":"https://example.com/ep1-medium.jpg","original":"https://example.com/ep1-original.jpg"},"summary":"<p>Walter faces episode one.</p>"},{"id":12193,"name":"Second","season":1,"number":2}]"#,
        ),
        _ => Response::error(404, "not found"),
    });
    let q = Query {
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let source = TvmazeSource::new(server.url(), crate::testsupport::http_client());
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.artwork, "https://example.com/show-original.jpg");
    assert!(
        !media.synopsis.is_empty() && !media.synopsis.contains(['<', '>']),
        "show synopsis not cleaned: {:?}",
        media.synopsis
    );
    assert_eq!(
        media.episodes.first().map(|ep| ep.artwork.clone()),
        Some("https://example.com/ep1-original.jpg".to_string())
    );
    assert!(
        !media.episodes[0].synopsis.is_empty(),
        "episode synopsis empty: {:?}",
        media.episodes[0]
    );

    let dir = TempDir::new();
    let resolver = resolver_with(
        &dir.path(),
        vec![Arc::new(TvmazeSource::new(
            server.url(),
            crate::testsupport::http_client(),
        ))],
    );
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert!(res.found, "resolution = {res:?}, want found");
    assert_eq!(res.artwork, "https://example.com/show-original.jpg");
    assert!(
        res.synopsis.contains("episode one"),
        "resolution synopsis = {:?}, want episode-level text",
        res.synopsis
    );
}

/// `TestResolutionFallsBackToSeriesArt`: the series-level pair is used when the
/// matched episode carries none.
#[test]
fn resolution_falls_back_to_series_art() {
    let fake = FakeSource::with_media(
        "fake",
        Media {
            source: "fake".to_string(),
            source_id: "1".to_string(),
            kind: Kind::SERIES,
            title: "Show".to_string(),
            artwork: "https://example.com/show.jpg".to_string(),
            synopsis: "<p>Show summary.</p>".to_string(),
            episodes: vec![Episode {
                season: 1,
                number: 2,
                title: "Pilot".to_string(),
                ..Episode::default()
            }],
            fetched_at: crate::timefmt::now_rfc3339(),
            ..Media::default()
        },
    );
    let dir = TempDir::new();
    let resolver = resolver_with(&dir.path(), vec![fake.shared()]);
    let res = resolver
        .lookup_now(
            &Cancel::background(),
            &Query {
                kind: Kind::SERIES,
                title: "Show".to_string(),
                season: 1,
                episode: 2,
                ..Query::default()
            },
        )
        .expect("LookupNow");
    assert!(
        res.found && res.artwork == "https://example.com/show.jpg",
        "resolution = {res:?}, want series art fallback"
    );
    assert_eq!(res.synopsis, "Show summary.");
}
