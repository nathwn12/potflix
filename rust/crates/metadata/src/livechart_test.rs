//! LiveChart.me provider tests: the keyless search round trip against a fake
//! server (including the title validation and the stub synthesis), plus one
//! live round trip against the real endpoint.

use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::livechart::LivechartSource;
use crate::resolver::Source;
use crate::testsupport::{FakeServer, Request, Response, http_client};

const LC_SEARCH: &str = r#"{"has_more":false,"items":[{"id":4047,"romaji_title":"Re:Petit kara Hajimeru Isekai Seikatsu","english_title":"Re:ZERO PETIT","native_title":"Re:プチから始める異世界生活","alternate_titles":["Re:Zero kara Hajimeru Break Time 2"],"synopsis":"A collection of comedic shorts featuring characters from Re:Zero.","premiere_date":"2016-06-23T15:00:00.000000000Z","premiere_date_precision":3,"episode_count":14,"poster":{"url":"https://img.livechart.me/poster/4047.jpg"}}]}"#;

fn anime_query(title: &str, episode: i32) -> Query {
    Query {
        kind: Kind::SERIES,
        title: title.to_string(),
        episode,
        absolute: true,
        ..Query::default()
    }
}

/// The search round trip maps titles/year/synopsis/poster and synthesizes
/// numbered absolute stubs from the episode count.
#[test]
fn livechart_finds_anime_by_validated_title() {
    let server = FakeServer::start(|req: &Request| {
        assert!(req.path.starts_with("/api/v1/anime"));
        Response::json(LC_SEARCH)
    });
    let source = LivechartSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &anime_query("Re:ZERO PETIT", 3))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Re:ZERO PETIT");
    assert_eq!(media.year, 2016);
    assert_eq!(media.kind, Kind::SERIES);
    assert_eq!(media.source, "livechart");
    assert_eq!(media.source_id, "4047");
    assert_eq!(media.artwork, "https://img.livechart.me/poster/4047.jpg");
    assert!(
        !media.synopsis.is_empty(),
        "synopsis must survive the cap/strip helpers"
    );
    assert_eq!(media.episodes.len(), 14, "one stub per counted episode");
    assert!(
        media.episodes.iter().all(|ep| ep.season == 0 && ep.title.is_empty()),
        "stubs carry numbers only, never fabricated titles"
    );
}

/// A low-confidence best is a definitive miss, never a wrong show.
#[test]
fn livechart_low_confidence_is_miss() {
    let server = FakeServer::start(|_: &Request| Response::json(LC_SEARCH));
    let source = LivechartSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::SERIES,
        title: "Xyzzy Qqq".to_string(),
        episode: 1,
        absolute: true,
        ..Query::default()
    };
    let got = source.find(&Cancel::background(), &q).expect("Find");
    assert!(got.is_none(), "got = {got:?}");
}

/// LIVE: the real LiveChart.me endpoint answers `re zero` with a synopsis.
/// Real network, real input; a mock proves nothing here.
#[test]
#[ignore = "live network; run with --ignored"]
fn livechart_live_rezero_round_trip() {
    let source = LivechartSource::new("", http_client());
    let cancel = Cancel::with_deadline(Duration::from_secs(25));
    let media = source
        .find(&cancel, &anime_query("re zero", 1))
        .expect("live Find")
        .expect("the live endpoint must answer re zero");
    assert!(
        !media.synopsis.is_empty(),
        "the live synopsis must be non-empty: {media:?}"
    );
    assert_eq!(media.source, "livechart", "media = {media:?}");
}
