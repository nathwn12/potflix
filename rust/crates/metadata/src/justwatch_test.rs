//! JustWatch provider tests: the GraphQL round trip against a fake server
//! (including the title validation and the poster-template expansion),
//! plus one live round trip against the real endpoint.

use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::justwatch::JustWatchSource;
use crate::resolver::Source;
use crate::testsupport::{FakeServer, Request, Response, http_client};

const JW_SEARCH: &str = r#"{"data":{"popularTitles":{"edges":[{"node":{"id":"tm421284","objectType":"MOVIE","content":{"title":"Re:ZERO -Starting Life in Another World- Memory Snow","posterUrl":"/poster/227979362/{profile}/re-zero-starting-life-in-another-world-memory-snow.{format}","shortDescription":"Subaru finally gets to take a breather on this snowy day.","originalReleaseYear":2018,"runtime":61,"genres":[{"shortName":"ani"}]}}},{"node":{"id":"tm999","objectType":"MOVIE","content":{"title":"Completely Unrelated Film","posterUrl":"","shortDescription":"Nothing to do with the query.","originalReleaseYear":2020,"runtime":90,"genres":[]}}}]}}}"#;

fn movie_query(title: &str, year: i32) -> Query {
    Query {
        kind: Kind::MOVIE,
        title: title.to_string(),
        year,
        ..Query::default()
    }
}

/// The search round trip maps title/year/runtime/synopsis/poster, expands the
/// poster template, and drops the machine-tag genres.
#[test]
fn justwatch_finds_movie_by_validated_title() {
    let server = FakeServer::start(|req: &Request| {
        assert_eq!(req.path, "/graphql");
        assert_eq!(req.method, "POST");
        Response::json(JW_SEARCH)
    });
    let source = JustWatchSource::new(server.url(), http_client());
    let media = source
        .find(
            &Cancel::background(),
            &movie_query("Re:ZERO Memory Snow", 2018),
        )
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(
        media.title,
        "Re:ZERO -Starting Life in Another World- Memory Snow"
    );
    assert_eq!(media.year, 2018);
    assert_eq!(media.kind, Kind::MOVIE);
    assert_eq!(media.source, "justwatch");
    assert_eq!(media.source_id, "tm421284");
    assert_eq!(media.length, "61 min");
    assert!(media.genres.is_empty(), "genres = {:?}", media.genres);
    assert!(
        !media.synopsis.is_empty(),
        "synopsis must survive the cap/strip helpers"
    );
    assert_eq!(
        media.artwork,
        "https://images.justwatch.com/poster/227979362/s718/re-zero-starting-life-in-another-world-memory-snow.jpg",
        "artwork = {:?}",
        media.artwork
    );
}

/// A low-confidence best is a definitive miss, never a wrong movie.
#[test]
fn justwatch_low_confidence_is_miss() {
    let server = FakeServer::start(|_: &Request| Response::json(JW_SEARCH));
    let source = JustWatchSource::new(server.url(), http_client());
    let got = source
        .find(&Cancel::background(), &movie_query("Xyzzy Qqq", 0))
        .expect("Find");
    assert!(got.is_none(), "got = {got:?}");
}

/// A series key never reaches the wire.
#[test]
fn justwatch_series_gates_before_wire() {
    let server = FakeServer::start(|_: &Request| Response::json(JW_SEARCH));
    let source = JustWatchSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::SERIES,
        title: "Re:ZERO".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let got = source.find(&Cancel::background(), &q).expect("Find");
    assert!(got.is_none(), "got = {got:?}");
}

/// LIVE: the real JustWatch endpoint answers `re zero` with `ts41257`, the
/// 2016 SHOW carrying a synopsis and a poster. Real network, real input; a
/// mock proves nothing here.
#[test]
#[ignore = "live network; run with --ignored"]
fn justwatch_live_rezero_round_trip() {
    let source = JustWatchSource::new("", http_client());
    let cancel = Cancel::with_deadline(Duration::from_secs(25));
    let q = Query {
        kind: Kind::UNKNOWN,
        title: "re zero".to_string(),
        ..Query::default()
    };
    let media = source
        .find(&cancel, &q)
        .expect("live Find")
        .expect("the live endpoint must answer re zero");
    assert_eq!(media.source_id, "ts41257", "media = {media:?}");
    assert_eq!(media.year, 2016, "media = {media:?}");
    assert!(
        !media.synopsis.is_empty(),
        "the live synopsis must be non-empty"
    );
    assert!(
        media.artwork.starts_with("https://images.justwatch.com/poster/"),
        "artwork = {:?}",
        media.artwork
    );
}
