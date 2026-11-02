//! Episodate provider tests: the search-then-details round trip against a
//! fake server (including the title validation), plus one live round trip
//! against the real endpoint.

use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::episodate::EpisodateSource;
use crate::resolver::Source;
use crate::testsupport::{FakeServer, Request, Response, http_client};

const EP_SEARCH: &str = r#"{"total":2,"page":1,"pages":1,"tv_shows":[{"id":65859,"name":"Re:ZERO -Starting Life in Another World-","permalink":"re-zero-kara-hajimeru-isekai-seikatsu","start_date":"2016-04-04","country":"JP","network":"AT-X","status":"Running","image_thumbnail_path":"https://static.episodate.com/images/tv-show/thumbnail/65859.jpg"},{"id":91471,"name":"Re:Zero kara Hajimeru Break Time","permalink":"re-zero-break-time","start_date":"2016-04-04","country":"JP","network":"TV Tokyo","status":"Ended","image_thumbnail_path":"https://static.episodate.com/images/tv-show/thumbnail/91471.jpg"}]}"#;
const EP_DETAILS: &str = r#"{"tvShow":{"id":65859,"name":"Re:ZERO -Starting Life in Another World-","permalink":"re-zero-kara-hajimeru-isekai-seikatsu","url":"https://www.episodate.com/tv-show/re-zero-kara-hajimeru-isekai-seikatsu","description":"Subaru Natsuki is summoned to another world and gains the power to rewind time by dying.","start_date":"2016-04-04","country":"JP","status":"Running","runtime":30,"network":"AT-X","image_path":"https://static.episodate.com/images/tv-show/full/65859.jpg","image_thumbnail_path":"https://static.episodate.com/images/tv-show/thumbnail/65859.jpg","episodes":[{"season":1,"episode":1,"name":"The End of the Beginning and the Beginning of the End","air_date":"2016-04-04 13:30:00"},{"season":1,"episode":2,"name":"Reunion with the Witch","air_date":"2016-04-11 13:30:00"}]}}"#;

fn series_query(title: &str, season: i64, episode: i32) -> Query {
    Query {
        kind: Kind::SERIES,
        title: title.to_string(),
        season,
        episode,
        ..Query::default()
    }
}

/// The search-then-details round trip maps name/year/cover/synopsis and the
/// season-qualified episode table.
#[test]
fn episodate_finds_series_by_validated_title() {
    let server = FakeServer::start(|req: &Request| match req.path.as_str() {
        "/api/search" => Response::json(EP_SEARCH),
        "/api/show-details" => Response::json(EP_DETAILS),
        _ => Response::error(404, "not found"),
    });
    let source = EpisodateSource::new(server.url(), http_client());
    let media = source
        .find(
            &Cancel::background(),
            &series_query("Re:ZERO -Starting Life in Another World-", 1, 1),
        )
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Re:ZERO -Starting Life in Another World-");
    assert_eq!(media.year, 2016);
    assert_eq!(media.kind, Kind::SERIES);
    assert_eq!(media.source, "episodate");
    assert_eq!(
        media.source_id, "re-zero-kara-hajimeru-isekai-seikatsu",
        "source_id = {:?}",
        media.source_id
    );
    assert_eq!(
        media.artwork,
        "https://static.episodate.com/images/tv-show/full/65859.jpg"
    );
    assert!(
        !media.synopsis.is_empty(),
        "synopsis must survive the cap/strip helpers"
    );
    assert_eq!(media.episodes.len(), 2, "episodes = {:?}", media.episodes);
    assert_eq!(media.episodes[0].title, "The End of the Beginning and the Beginning of the End");
}

/// A low-confidence best is a definitive miss, never a wrong show.
#[test]
fn episodate_low_confidence_is_miss() {
    let server = FakeServer::start(|req: &Request| match req.path.as_str() {
        "/api/search" => Response::json(EP_SEARCH),
        _ => Response::json(EP_DETAILS),
    });
    let source = EpisodateSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::SERIES,
        title: "Xyzzy Qqq".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let got = source.find(&Cancel::background(), &q).expect("Find");
    assert!(got.is_none(), "got = {got:?}");
}

/// LIVE: the real Episodate endpoint answers the full Re:ZERO title with the
/// main series record, its synopsis, its cover and a named episode table.
/// Real network, real input; a mock proves nothing here.
///
/// Named reading: the short query `re zero` title-validates against the
/// `Break Time` spinoff (a genuine Re:Zero entry the matcher ranks first),
/// so the live round trip uses the full title, which only the main series
/// fully covers.
#[test]
#[ignore = "live network; run with --ignored"]
fn episodate_live_rezero_round_trip() {
    let source = EpisodateSource::new("", http_client());
    let cancel = Cancel::with_deadline(Duration::from_secs(25));
    let q = Query {
        kind: Kind::SERIES,
        title: "Re:ZERO -Starting Life in Another World-".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let media = source
        .find(&cancel, &q)
        .expect("live Find")
        .expect("the live endpoint must answer re zero");
    assert_eq!(
        media.title, "Re:ZERO -Starting Life in Another World-",
        "media = {media:?}"
    );
    assert_eq!(media.year, 2016, "media = {media:?}");
    assert!(
        !media.synopsis.is_empty(),
        "the live synopsis must be non-empty"
    );
    assert!(
        media.artwork.starts_with("https://static.episodate.com/"),
        "artwork = {:?}",
        media.artwork
    );
    assert!(
        media.episodes.iter().any(|ep| ep.number == 1 && !ep.title.is_empty()),
        "the live episode table must name episode 1: {:?}",
        media.episodes.first()
    );
}
