//! Tenrai provider tests: the search-then-episodes round trip against a fake
//! server (including the title validation the iTunes leg lacked), plus
//! one live round trip against the real endpoint.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::resolver::Source;
use crate::tenrai::TenraiSource;
use crate::testsupport::{FakeServer, Request, Response, TempDir, http_client, resolver_with};

const SEARCH: &str = r#"{"pagination":{"last_visible_page":1,"has_next_page":false,"current_page":1},"data":[{"mal_id":20,"url":"https://myanimelist.net/anime/20/Naruto","title":"Naruto","title_english":"Naruto","year":2002,"synopsis":"A young ninja chases his dream.","duration":"23 min per ep","images":{"jpg":{"image_url":"https://cdn.myanimelist.net/images/anime/20.jpg","large_image_url":"https://cdn.myanimelist.net/images/anime/20l.jpg"}},"genres":[{"name":"Action"}],"members":3000000,"episodes":220}]}"#;
const EPISODES: &str = r#"{"pagination":{"last_visible_page":1,"has_next_page":false},"data":[{"mal_id":1,"title":"Enter: Naruto Uzumaki!","synopsis":"The pilot.","url":"https://myanimelist.net/anime/20/Naruto/episode/1","duration":1427},{"mal_id":2,"title":"My Name is Konohamaru!","synopsis":"The second.","url":"https://myanimelist.net/anime/20/Naruto/episode/2","duration":1414}]}"#;

/// A fake Tenrai API serving the canned search/episodes pair, recording every
/// request path so a test can prove the episode leg was never touched.
fn fake_api(paths: Arc<Mutex<Vec<String>>>) -> FakeServer {
    FakeServer::start(move |req: &Request| {
        paths.lock().unwrap().push(req.path.clone());
        if req.path == "/v1/anime" {
            return Response::json(SEARCH);
        }
        match req.path.as_str() {
            "/v1/anime/20/episodes" => Response::json(EPISODES),
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

/// The search-then-episodes round trip maps title/year/genres/poster, and the
/// resolver carries the number-exact episode title.
#[test]
fn tenrai_finds_anime_by_validated_title() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_api(Arc::clone(&paths));
    let source = TenraiSource::new(server.url(), http_client());
    let q = anime_query("Naruto", 2002, 1);
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Naruto");
    assert_eq!(media.year, 2002);
    assert_eq!(media.kind, Kind::SERIES);
    assert_eq!(media.source, "tenrai");
    assert_eq!(media.source_id, "20");
    assert_eq!(media.length, "23 min");
    assert_eq!(media.genres, "Action");
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
        vec![Arc::new(TenraiSource::new(server.url(), http_client()))],
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
fn tenrai_rejects_wrong_title_without_episode_fetch() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&paths);
    let server = FakeServer::start(move |req: &Request| {
        recorded.lock().unwrap().push(req.path.clone());
        if req.path == "/v1/anime" {
            return Response::json(
                r#"{"pagination":{"last_visible_page":1,"has_next_page":false},"data":[{"mal_id":999,"url":"https://myanimelist.net/anime/999/Ring_of_Fire","title":"Ring of Fire","title_english":"Ring of Fire","year":2012,"members":10,"episodes":12}]}"#,
            );
        }
        Response::error(404, "not found")
    });
    let source = TenraiSource::new(server.url(), http_client());
    let found = source
        .find(&Cancel::background(), &anime_query("Naruto", 0, 1))
        .expect("Find");
    assert!(found.is_none(), "a wrong title must miss, got {found:?}");
    assert!(
        !paths.lock().unwrap().iter().any(|p| p.contains("/episodes")),
        "no episode fetch for a rejected title: {paths:?}"
    );
}

/// Year NEVER rejects: a known query year that disagrees with the pick still
/// answers with the essentials -- the episode leg IS fetched and the pick's
/// year rides along as a display field.
#[test]
fn tenrai_year_mismatch_still_answers() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_api(Arc::clone(&paths));
    let source = TenraiSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &anime_query("Naruto", 2000, 1))
        .expect("Find")
        .expect("a year mismatch must still answer, got nil");
    assert_eq!(media.title, "Naruto");
    assert_eq!(media.year, 2002, "the pick's year rides along: {media:?}");
    assert!(
        paths.lock().unwrap().iter().any(|p| p.contains("/episodes")),
        "the episode leg must be fetched despite the mismatch: {paths:?}"
    );
}

/// A movie query is self-gated before the wire: no request, definitive miss.
#[test]
fn tenrai_gates_movies_without_network() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_api(Arc::clone(&paths));
    let source = TenraiSource::new(server.url(), http_client());
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

/// DBS 2026 regression (measured 2026-10-15): the search ranks mal30694
/// (2015, far more members) first for `dragon ball super`, but a 2026 query
/// must land on mal63367.
#[test]
fn tenrai_year_prefers_beerus_over_2015_series() {
    let server = FakeServer::start(|req: &Request| {
        if req.path == "/v1/anime" {
            return Response::json(
                r#"{"pagination":{"last_visible_page":1,"has_next_page":false},"data":[{"mal_id":30694,"url":"https://myanimelist.net/anime/30694/Dragon_Ball_Super","title":"Dragon Ball Super","title_english":"Dragon Ball Super","year":2015,"synopsis":"The 2015 series.","duration":"24 min per ep","members":900000,"episodes":131},{"mal_id":63367,"url":"https://myanimelist.net/anime/63367/Dragon_Ball_Super_Beerus","title":"Dragon Ball Super: Beerus","title_english":"Dragon Ball Super: Beerus","year":2026,"synopsis":"The Beerus arc.","duration":"24 min per ep","members":1000,"episodes":1}]}"#,
            );
        }
        match req.path.as_str() {
            "/v1/anime/63367/episodes" => Response::json(
                r#"{"pagination":{"has_next_page":false},"data":[{"mal_id":1,"title":"Beerus Appears","synopsis":"The pilot.","duration":1440}]}"#,
            ),
            _ => Response::error(404, "not found"),
        }
    });
    let source = TenraiSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &anime_query("dragon ball super", 2026, 1))
        .expect("Find")
        .expect("the 2026 query must answer, got nil");
    assert_eq!(media.source_id, "63367", "media = {media:?}");
    assert_eq!(media.title, "Dragon Ball Super: Beerus");
    assert_eq!(media.year, 2026);
}

/// LIVE: the real Tenrai endpoint answers `Naruto` with year, genres, poster
/// and the absolute episode-1 title. Real network, real input; a mock proves
/// nothing here.
#[test]
#[ignore = "live network; run with --ignored"]
fn tenrai_live_naruto_round_trip() {
    let source = TenraiSource::new("", http_client());
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
        .expect("the live table must carry absolute episode 1");
    assert_eq!(
        first.title, "Enter: Naruto Uzumaki!",
        "episode 1 = {first:?}"
    );
}

/// AoT regression (measured 2026-10-15): ranking by the english-first display
/// name alone scores the main series ("Attack on Titan") 0 against a romaji
/// query, so a romaji-named special wins. The `title` alias must count.
#[test]
fn tenrai_romaji_query_selects_main_series() {
    let server = FakeServer::start(|req: &Request| {
        if req.path == "/v1/anime" {
            return Response::json(
                r#"{"pagination":{"last_visible_page":1,"has_next_page":false},"data":[{"mal_id":16498,"url":"https://myanimelist.net/anime/16498/Shingeki_no_Kyojin","title":"Shingeki no Kyojin","title_english":"Attack on Titan","year":2013,"synopsis":"Humanity fights for survival.","duration":"24 min per ep","members":900000,"episodes":25},{"mal_id":99999,"url":"https://myanimelist.net/anime/99999/Picture_Drama","title":"Shingeki no Kyojin: Picture Drama","title_english":"","year":2013,"synopsis":"Chibi comedy shorts.","duration":"2 min per ep","members":100,"episodes":32}]}"#,
            );
        }
        Response::error(404, "not found")
    });
    let source = TenraiSource::new(server.url(), http_client());
    let media = source
        .find(
            &Cancel::background(),
            &Query {
                kind: Kind::SERIES,
                title: "Shingeki no Kyojin".to_string(),
                absolute: true,
                ..Query::default()
            },
        )
        .expect("Find")
        .expect("the romaji query must answer, got nil");
    assert_eq!(media.source_id, "16498", "a special must not win: {media:?}");
    assert_eq!(media.title, "Attack on Titan", "media = {media:?}");
}
