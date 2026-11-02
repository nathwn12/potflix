//! Kitsu provider tests: the anime-then-episodes round trip against a fake
//! server (including the title validation the iTunes leg lacked -- year never
//! gates), plus one live round trip against the real endpoint.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::kitsu::KitsuSource;
use crate::resolver::Source;
use crate::testsupport::{FakeServer, Request, Response, TempDir, http_client, resolver_with};

const SEARCH: &str = r#"{"data":[{"id":"11","attributes":{"slug":"naruto","canonicalTitle":"Naruto","titles":{"en":"Naruto","en_jp":"Naruto"},"startDate":"2002-10-03","synopsis":"A young ninja chases his dream.","posterImage":{"large":"https://media.kitsu.io/anime/poster_images/11/large.jpg","original":"https://media.kitsu.io/anime/poster_images/11/original.jpg"},"episodeLength":23,"episodeCount":220,"userCount":300000}}]}"#;
const EPISODES: &str = r#"{"data":[{"attributes":{"canonicalTitle":"Enter: Naruto Uzumaki!","seasonNumber":1,"number":1,"synopsis":"The pilot.","length":23,"thumbnail":{"original":"https://media.kitsu.io/anime/11/episodes/1/thumbnail.jpg"}}},{"attributes":{"canonicalTitle":"My Name is Konohamaru!","seasonNumber":1,"number":2,"synopsis":"The second.","length":23,"thumbnail":{"original":"https://media.kitsu.io/anime/11/episodes/2/thumbnail.jpg"}}}]}"#;

/// A fake Kitsu API serving the canned anime/episodes pair, recording every
/// request path so a test can prove the episode leg was never touched.
fn fake_api(paths: Arc<Mutex<Vec<String>>>) -> FakeServer {
    FakeServer::start(move |req: &Request| {
        paths.lock().unwrap().push(req.path.clone());
        if req.path == "/anime" {
            return Response::json(SEARCH);
        }
        match req.path.as_str() {
            "/episodes" => Response::json(EPISODES),
            _ => Response::error(404, "not found"),
        }
    })
}

fn anime_query(title: &str, year: i32, episode: i32) -> Query {
    Query {
        kind: Kind::SERIES,
        title: title.to_string(),
        year,
        episode,
        absolute: true,
        ..Query::default()
    }
}

/// The anime-then-episodes round trip maps title/year/poster, and the
/// resolver carries the number-exact episode title.
#[test]
fn kitsu_finds_anime_by_validated_title() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_api(Arc::clone(&paths));
    let source = KitsuSource::new(server.url(), http_client());
    let q = anime_query("Naruto", 2002, 1);
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Naruto");
    assert_eq!(media.year, 2002);
    assert_eq!(media.kind, Kind::SERIES);
    assert_eq!(media.source, "kitsu");
    assert_eq!(media.source_id, "11");
    assert_eq!(media.length, "23 min");
    assert!(
        !media.synopsis.is_empty(),
        "synopsis must survive the cap/strip helpers"
    );
    assert!(
        media.artwork.starts_with("https://"),
        "artwork = {:?}",
        media.artwork
    );
    assert_eq!(media.episodes.len(), 2);
    assert_eq!(media.episodes[0].number, 1);
    assert_eq!(media.episodes[0].title, "Enter: Naruto Uzumaki!");
    assert_eq!(media.episodes[0].length, "23 min");

    let dir = TempDir::new();
    let resolver = resolver_with(
        &dir.path(),
        vec![Arc::new(KitsuSource::new(server.url(), http_client()))],
    );
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert!(
        res.found && res.series == "Naruto" && res.episode == "Enter: Naruto Uzumaki!",
        "resolution = {res:?}"
    );
}

/// The iTunes failure, inverted: a search whose best title does not match the
/// query is a definitive miss, and the episode leg is never fetched.
#[test]
fn kitsu_rejects_wrong_title_without_episode_fetch() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&paths);
    let server = FakeServer::start(move |req: &Request| {
        recorded.lock().unwrap().push(req.path.clone());
        if req.path == "/anime" {
            return Response::json(
                r#"{"data":[{"id":"999","attributes":{"slug":"ring-of-fire","canonicalTitle":"Ring of Fire","startDate":"2012-01-01","userCount":10,"episodeCount":12}}]}"#,
            );
        }
        Response::error(404, "not found")
    });
    let source = KitsuSource::new(server.url(), http_client());
    let found = source
        .find(&Cancel::background(), &anime_query("Naruto", 0, 1))
        .expect("Find");
    assert!(found.is_none(), "a wrong title must miss, got {found:?}");
    assert!(
        !paths.lock().unwrap().iter().any(|p| p == "/episodes"),
        "no episode fetch for a rejected title: {paths:?}"
    );
}

/// Year NEVER rejects: a known query year that disagrees with the pick still
/// answers with the essentials -- the episode leg IS fetched and the pick's
/// year rides along as a display field.
#[test]
fn kitsu_year_mismatch_still_answers() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_api(Arc::clone(&paths));
    let source = KitsuSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &anime_query("Naruto", 2000, 1))
        .expect("Find")
        .expect("a year mismatch must still answer, got nil");
    assert_eq!(media.title, "Naruto");
    assert_eq!(media.year, 2002, "the pick's year rides along: {media:?}");
    assert!(
        paths.lock().unwrap().iter().any(|p| p == "/episodes"),
        "the episode leg must be fetched despite the mismatch: {paths:?}"
    );
}

/// A movie query is self-gated before the wire: no request, definitive miss.
#[test]
fn kitsu_gates_movies_without_network() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_api(Arc::clone(&paths));
    let source = KitsuSource::new(server.url(), http_client());
    let found = source
        .find(
            &Cancel::background(),
            &Query {
                kind: Kind::MOVIE,
                title: "Naruto".to_string(),
                ..Query::default()
            },
        )
        .expect("Find");
    assert!(found.is_none(), "a movie query must miss, got {found:?}");
    assert!(
        paths.lock().unwrap().is_empty(),
        "no request for a gated kind: {paths:?}"
    );
}

/// LIVE: the real Kitsu endpoint answers `Naruto` with year, poster and the
/// absolute episode-1 entry. Real network, real input; a mock proves nothing
/// here.
#[test]
#[ignore = "live network; run with --ignored"]
fn kitsu_live_naruto_round_trip() {
    let source = KitsuSource::new("", http_client());
    let cancel = Cancel::with_deadline(Duration::from_secs(25));
    let media = source
        .find(&cancel, &anime_query("Naruto", 2002, 1))
        .expect("live Find")
        .expect("the live search must answer Naruto");
    assert_eq!(media.title, "Naruto", "media = {media:?}");
    assert_eq!(media.year, 2002, "media = {media:?}");
    assert!(
        !media.synopsis.is_empty(),
        "the live synopsis must be non-empty"
    );
    assert!(
        media.artwork.starts_with("https://"),
        "artwork = {:?}",
        media.artwork
    );
    let first = media
        .episodes
        .iter()
        .find(|episode| episode.number == 1)
        .expect("the live page must carry absolute episode 1");
    assert!(
        !first.title.is_empty(),
        "the live episode 1 must be titled: {first:?}"
    );
}

/// DBS 2026 regression (measured 2026-10-15): same treatment as the other
/// picks -- a 2026 query must land on the Beerus entry, not the 2015 series.
#[test]
fn kitsu_year_prefers_beerus_over_2015_series() {
    let server = FakeServer::start(|req: &Request| {
        if req.path == "/anime" {
            return Response::json(
                r#"{"data":[{"id":"101","attributes":{"slug":"dragon-ball-super","canonicalTitle":"Dragon Ball Super","startDate":"2015-07-05","synopsis":"The 2015 series.","episodeLength":24,"episodeCount":131,"userCount":500000}},{"id":"202","attributes":{"slug":"dragon-ball-super-beerus","canonicalTitle":"Dragon Ball Super: Beerus","startDate":"2026-10-11","synopsis":"The Beerus arc.","episodeLength":24,"episodeCount":1,"userCount":100}}]}"#,
            );
        }
        match req.path.as_str() {
            "/episodes" => Response::json(EPISODES),
            _ => Response::error(404, "not found"),
        }
    });
    let source = KitsuSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &anime_query("dragon ball super", 2026, 1))
        .expect("Find")
        .expect("the 2026 query must answer, got nil");
    assert_eq!(media.source_id, "202", "media = {media:?}");
    assert_eq!(media.title, "Dragon Ball Super: Beerus");
    assert_eq!(media.year, 2026);
}

/// Sailor Moon 2014 regression (review of the alias-aware change): query
/// `Bishoujo Senshi Sailor Moon` with year 2014 must land on Crystal (`7163`,
/// 2014), not the 1992 series (`489`). The 1992 entry wins the title pick via
/// its exact romaji alias, so only the year preference can swap to Crystal --
/// and Crystal's English display name (`Pretty Guardian Sailor Moon Crystal`)
/// does NOT cover the romaji query, while its aliases do. Coverage judged on
/// the display name alone keeps the 1992 series; judged across aliases it
/// selects Crystal. Live shape measured 2026-10-15 (trimmed to the fields the
/// pick reads).
#[test]
fn kitsu_year_prefers_crystal_over_1992_series_for_romaji_query() {
    let server = FakeServer::start(|req: &Request| {
        if req.path == "/anime" {
            return Response::json(
                r#"{"data":[
                    {"id":"489","attributes":{"slug":"bishoujo-senshi-sailor-moon","canonicalTitle":"Bishoujo Senshi Sailor Moon","titles":{"en":"Sailor Moon","en_jp":"Bishoujo Senshi Sailor Moon"},"startDate":"1992-03-07","synopsis":"The 1992 series.","episodeLength":24,"episodeCount":46,"userCount":300000}},
                    {"id":"7163","attributes":{"slug":"bishoujo-senshi-sailor-moon-crystal","canonicalTitle":"Bishoujo Senshi Sailor Moon Crystal","titles":{"en":"Pretty Guardian Sailor Moon Crystal","en_jp":"Bishoujo Senshi Sailor Moon Crystal"},"startDate":"2014-07-05","synopsis":"The 2014 remake.","episodeLength":24,"episodeCount":26,"userCount":14167}}
                ]}"#,
            );
        }
        match req.path.as_str() {
            "/episodes" => Response::json(EPISODES),
            _ => Response::error(404, "not found"),
        }
    });
    let source = KitsuSource::new(server.url(), http_client());
    let media = source
        .find(
            &Cancel::background(),
            &anime_query("Bishoujo Senshi Sailor Moon", 2014, 1),
        )
        .expect("Find")
        .expect("the 2014 query must answer, got nil");
    assert_eq!(media.source_id, "7163", "media = {media:?}");
    assert_eq!(media.title, "Pretty Guardian Sailor Moon Crystal");
    assert_eq!(media.year, 2014);
}
/// AoT regression (measured 2026-10-15): a romaji S01E03 query must land on
/// the main TV series with its ENGLISH title, never on a romaji-named
/// special. The live chain resolved `Shingeki no Kyojin S01E03` to the
/// "Picture Drama" entry with `Day 5/Day 6/Day 7` leaking into details.
#[test]
fn kitsu_romaji_query_selects_main_series_with_english_title() {
    let server = FakeServer::start(|req: &Request| {
        if req.path == "/anime" {
            return Response::json(
                r#"{"data":[
                    {"id":"1","attributes":{"slug":"attack-on-titan","canonicalTitle":"Attack on Titan","titles":{"en":"Attack on Titan","en_jp":"Shingeki no Kyojin"},"startDate":"2013-04-07","synopsis":"Humanity fights for survival.","episodeLength":24,"episodeCount":25,"userCount":500000}},
                    {"id":"2","attributes":{"slug":"shingeki-no-kyojin-picture-drama","canonicalTitle":"Shingeki no Kyojin: Picture Drama","titles":{"en":"","en_jp":"Shingeki no Kyojin: Picture Drama"},"startDate":"2013-08-06","synopsis":"Chibi comedy shorts.","episodeLength":2,"episodeCount":32,"userCount":100}}
                ]}"#,
            );
        }
        if req.path == "/episodes" {
            if req.query("filter[mediaId]").as_deref() == Some("1") {
                return Response::json(
                    r#"{"data":[{"attributes":{"canonicalTitle":"To You, in 2000 Years","seasonNumber":1,"number":3,"synopsis":"The fall of Shiganshina.","length":24,"thumbnail":{"original":"https://example.com/s1e3.jpg"}}}]}"#,
                );
            }
            return Response::json(
                r#"{"data":[{"attributes":{"canonicalTitle":"Day 5/Day 6/Day 7","seasonNumber":1,"number":3,"synopsis":"Chibi shorts.","length":2,"thumbnail":{"original":"https://example.com/pd3.jpg"}}}]}"#,
            );
        }
        Response::error(404, "not found")
    });
    let source = KitsuSource::new(server.url(), http_client());
    let media = source
        .find(
            &Cancel::background(),
            &Query {
                kind: Kind::SERIES,
                title: "Shingeki no Kyojin".to_string(),
                season: 1,
                episode: 3,
                ..Query::default()
            },
        )
        .expect("Find")
        .expect("the romaji query must answer, got nil");
    assert_eq!(media.source_id, "1", "a special must not win: {media:?}");
    assert_eq!(media.title, "Attack on Titan", "media = {media:?}");
    let ep = media
        .episodes
        .iter()
        .find(|episode| episode.season == 1 && episode.number == 3)
        .expect("S1E3 must be present");
    assert_eq!(ep.title, "To You, in 2000 Years", "episode = {ep:?}");
    assert_ne!(ep.title, "Day 5/Day 6/Day 7", "the special tell: {ep:?}");
}
