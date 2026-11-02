//! ANN provider tests: the XML report parsing against a canned body
//! (including the title validation and the colon retry), plus one live round
//! trip against the real endpoint.

use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::ann::AnnSource;
use crate::resolver::Source;
use crate::testsupport::{FakeServer, Request, Response, http_client};

const ANN_BEBOP: &str = r#"<ann><anime id="13" gid="34004783" type="TV" name="Cowboy Bebop" precision="TV" generated-on="2026-10-15T01:39:10Z"><info gid="2484932293" type="Picture" src="https://cdn.animenewsnetwork.com/thumbnails/fit200x200/encyc/A13-15.jpg" width="168" height="200"><img src="https://cdn.animenewsnetwork.com/thumbnails/fit200x200/encyc/A13-15.jpg" width="168" height="200"/></info><info gid="3686620137" type="Main title" lang="EN">Cowboy Bebop</info><info gid="3713150771" type="Alternative title" lang="JA">カウボーイビバップ</info><info gid="2458207658" type="Genres">action</info><info gid="2493109372" type="Plot Summary">Spike Spiegel, a bounty hunter with a dark past, hunts criminals across the solar system.</info><info gid="1706618398" type="Running time">25</info><info gid="120839903" type="Number of episodes">26</info><info gid="1533445328" type="Vintage">1998-04-03 to 1998-06-19</info></anime></ann>"#;
const ANN_EMPTY: &str = r#"<ann><warning>no results</warning></ann>"#;

fn anime_query(title: &str, episode: i32) -> Query {
    Query {
        kind: Kind::SERIES,
        title: title.to_string(),
        episode,
        absolute: true,
        ..Query::default()
    }
}

/// The XML round trip maps Main title/vintage/cover/synopsis/runtime and
/// synthesizes numbered absolute stubs from the episode count.
#[test]
fn ann_finds_anime_by_validated_title() {
    let server = FakeServer::start(|req: &Request| {
        assert!(req.path.starts_with("/encyclopedia/api.xml"));
        Response {
            status: 200,
            content_type: "text/xml".to_string(),
            body: ANN_BEBOP.as_bytes().to_vec(),
        }
    });
    let source = AnnSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &anime_query("Cowboy Bebop", 5))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Cowboy Bebop");
    assert_eq!(media.year, 1998);
    assert_eq!(media.kind, Kind::SERIES);
    assert_eq!(media.source, "ann");
    assert_eq!(media.source_id, "13");
    assert_eq!(
        media.artwork,
        "https://cdn.animenewsnetwork.com/thumbnails/fit200x200/encyc/A13-15.jpg"
    );
    assert!(
        media.synopsis.contains("Spike Spiegel"),
        "synopsis = {:?}",
        media.synopsis
    );
    assert_eq!(media.length, "25 min", "length = {:?}", media.length);
    assert_eq!(media.episodes.len(), 26, "one stub per counted episode");
    assert!(
        media.episodes.iter().all(|ep| ep.season == 0 && ep.title.is_empty()),
        "stubs carry numbers only, never fabricated titles"
    );
}

/// A warning body is a definitive miss, and a low-confidence best never
/// answers a wrong show.
#[test]
fn ann_warning_and_low_confidence_are_miss() {
    let server = FakeServer::start(|_: &Request| Response {
        status: 200,
        content_type: "text/xml".to_string(),
        body: ANN_EMPTY.as_bytes().to_vec(),
    });
    let source = AnnSource::new(server.url(), http_client());
    let got = source
        .find(&Cancel::background(), &anime_query("Cowboy Bebop", 5))
        .expect("Find");
    assert!(got.is_none(), "got = {got:?}");

    let server = FakeServer::start(|_: &Request| Response {
        status: 200,
        content_type: "text/xml".to_string(),
        body: ANN_BEBOP.as_bytes().to_vec(),
    });
    let source = AnnSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::SERIES,
        title: "Xyzzy Qqq".to_string(),
        episode: 5,
        absolute: true,
        ..Query::default()
    };
    let got = source.find(&Cancel::background(), &q).expect("Find");
    assert!(got.is_none(), "got = {got:?}");
}

/// LIVE: the real ANN endpoint answers `cowboy bebop` with the 1998 series,
/// its cover, its episode count and its synopsis. Real network, real input;
/// a mock proves nothing here.
#[test]
#[ignore = "live network; run with --ignored"]
fn ann_live_bebop_round_trip() {
    let source = AnnSource::new("", http_client());
    let cancel = Cancel::with_deadline(Duration::from_secs(25));
    let media = source
        .find(&cancel, &anime_query("cowboy bebop", 5))
        .expect("live Find")
        .expect("the live endpoint must answer cowboy bebop");
    assert_eq!(media.title, "Cowboy Bebop", "media = {media:?}");
    assert_eq!(media.year, 1998, "media = {media:?}");
    assert!(
        media.artwork.starts_with("https://cdn.animenewsnetwork.com/"),
        "artwork = {:?}",
        media.artwork
    );
    assert_eq!(media.episodes.len(), 26, "media = {media:?}");
    assert!(
        !media.synopsis.is_empty(),
        "the live synopsis must be non-empty"
    );
}
