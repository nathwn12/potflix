//! Bangumi provider tests: the keyless search round trip against a fake
//! server (including the title validation and the stub synthesis), plus one
//! live round trip against the real endpoint.

use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::bangumi::BangumiSource;
use crate::resolver::Source;
use crate::testsupport::{FakeServer, Request, Response, http_client};

const BGM_TV: &str = r#"{"results":2,"list":[{"id":102689,"url":"http://bgm.tv/subject/102689","type":6,"name":"Better Call Saul Season 1","name_cn":"风骚律师 第一季","summary":"2002年，Jimmy McGill从一个无名小律师转变为游走在法律边缘的Saul的故事。","eps":10,"eps_count":10,"air_date":"2015-02-08","air_weekday":7,"images":{"large":"http://lain.bgm.tv/pic/cover/l/29/82/102689_T438e.jpg","common":"","medium":"","small":"","grid":""},"collection":{}},{"id":126800,"url":"http://bgm.tv/subject/126800","type":6,"name":"Better Call Saul Season 2","name_cn":"风骚律师 第二季","summary":"Saul的故事继续。","eps":10,"eps_count":10,"air_date":"2016-02-15","air_weekday":1,"images":{"large":"http://lain.bgm.tv/pic/cover/l/xx/yy/126800_x.jpg","common":"","medium":"","small":"","grid":""},"collection":{}}]}"#;
const BGM_EMPTY: &str = r#"{"results":0,"list":[]}"#;

fn tv_query(title: &str, year: i32, season: i64, episode: i32) -> Query {
    Query {
        kind: Kind::SERIES,
        title: title.to_string(),
        year,
        season,
        episode,
        ..Query::default()
    }
}

/// The search round trip maps titles/year/cover/synopsis, prefers the
/// year-carrying season, and synthesizes numbered season-1 stubs from the
/// episode count.
#[test]
fn bangumi_finds_series_by_validated_title() {
    let server = FakeServer::start(|req: &Request| {
        assert!(req.path.starts_with("/search/subject/"));
        Response::json(BGM_TV)
    });
    let source = BangumiSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &tv_query("Better Call Saul", 2015, 1, 1))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Better Call Saul Season 1");
    assert_eq!(media.year, 2015);
    assert_eq!(media.kind, Kind::SERIES);
    assert_eq!(media.source, "bangumi");
    assert_eq!(media.source_id, "102689");
    assert_eq!(
        media.artwork,
        "https://lain.bgm.tv/pic/cover/l/29/82/102689_T438e.jpg"
    );
    assert!(
        !media.synopsis.is_empty(),
        "synopsis must survive the cap/strip helpers"
    );
    assert_eq!(media.episodes.len(), 10, "one stub per counted episode");
    assert!(
        media.episodes.iter().all(|ep| ep.season == 1 && ep.title.is_empty()),
        "stubs carry numbers only, never fabricated titles"
    );
}

/// A low-confidence best is a definitive miss, never a wrong show.
#[test]
fn bangumi_low_confidence_is_miss() {
    let server = FakeServer::start(|_: &Request| Response::json(BGM_TV));
    let source = BangumiSource::new(server.url(), http_client());
    let got = source
        .find(&Cancel::background(), &tv_query("Xyzzy Qqq", 0, 1, 1))
        .expect("Find");
    assert!(got.is_none(), "got = {got:?}");
}

/// An empty result list is a definitive miss.
#[test]
fn bangumi_empty_list_is_miss() {
    let server = FakeServer::start(|_: &Request| Response::json(BGM_EMPTY));
    let source = BangumiSource::new(server.url(), http_client());
    let got = source
        .find(&Cancel::background(), &tv_query("Better Call Saul", 0, 1, 1))
        .expect("Find");
    assert!(got.is_none(), "got = {got:?}");
}

/// LIVE: the real Bangumi endpoint answers `better call saul` (2015) with
/// the season-1 record: cover, episode count and synopsis. Real network,
/// real input; a mock proves nothing here.
///
/// Named reading: Bangumi anime entries carry native-script names
/// (`Re:ゼロ...`), which a romaji query can never title-validate, so the
/// live anime leg is proven by the stub-shape fake above while the live
/// round trip runs where the names are English.
#[test]
#[ignore = "live network; run with --ignored"]
fn bangumi_live_bcs_round_trip() {
    let source = BangumiSource::new("", http_client());
    let cancel = Cancel::with_deadline(Duration::from_secs(30));
    let media = source
        .find(&cancel, &tv_query("better call saul", 2015, 1, 1))
        .expect("live Find")
        .expect("the live endpoint must answer better call saul");
    assert_eq!(media.title, "Better Call Saul Season 1", "media = {media:?}");
    assert_eq!(media.year, 2015, "media = {media:?}");
    assert!(
        media.artwork.starts_with("http"),
        "artwork = {:?}",
        media.artwork
    );
    assert_eq!(media.episodes.len(), 10, "media = {media:?}");
    assert!(
        !media.synopsis.is_empty(),
        "the live synopsis must be non-empty"
    );
}
