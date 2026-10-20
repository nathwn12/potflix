//! Cinemeta provider tests: the catalog-then-meta round trip against a fake
//! server (including the title validation that the iTunes leg lacked),
//! plus one live round trip against the real endpoint.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::cinemeta::CinemetaSource;
use crate::resolver::Source;
use crate::testsupport::{FakeServer, Request, Response, TempDir, http_client, resolver_with};

const MOVIE_CATALOG: &str = r#"{"metas":[{"id":"tt1375666","name":"Inception","releaseInfo":"2010","poster":"https://images.metahub.space/poster/small/tt1375666/img"}]}"#;
const MOVIE_META: &str = r#"{"meta":{"name":"Inception","year":"2010","runtime":"148 min","genre":["Adventure","Sci-Fi","Thriller"],"description":"A skilled thief who steals secrets from deep within the subconscious is offered one last job.","poster":"https://images.metahub.space/poster/small/tt1375666/img"}}"#;
const SERIES_CATALOG: &str = r#"{"metas":[{"id":"tt0903747","name":"Breaking Bad","releaseInfo":"2008-2013","poster":"https://images.metahub.space/poster/small/tt0903747/img"}]}"#;
const SERIES_META: &str = r#"{"meta":{"name":"Breaking Bad","year":"2008","genre":["Drama","Crime"],"description":"A chemistry teacher turns to cooking crystal meth.","poster":"https://images.metahub.space/poster/small/tt0903747/img","videos":[{"name":"Pilot","season":1,"number":1,"overview":"Walter White learns he has cancer.","thumbnail":"https://episodes.metahub.space/tt0903747/s1e1/img"}]}}"#;

/// A fake Cinemeta addon serving the canned catalog/meta pairs, recording
/// every request path so a test can prove the meta leg was never touched.
fn fake_addon(paths: Arc<Mutex<Vec<String>>>) -> FakeServer {
    FakeServer::start(move |req: &Request| {
        paths.lock().unwrap().push(req.path.clone());
        if req.path.starts_with("/catalog/movie/") {
            return Response::json(MOVIE_CATALOG);
        }
        if req.path.starts_with("/catalog/series/") {
            return Response::json(SERIES_CATALOG);
        }
        match req.path.as_str() {
            "/meta/movie/tt1375666.json" => Response::json(MOVIE_META),
            "/meta/series/tt0903747.json" => Response::json(SERIES_META),
            _ => Response::error(404, "not found"),
        }
    })
}

fn movie_query(title: &str, year: i32) -> Query {
    Query {
        kind: Kind::MOVIE,
        title: title.to_string(),
        year,
        ..Query::default()
    }
}

/// The catalog-then-meta round trip maps name/year/runtime/genres/synopsis/
/// poster, and the resolver carries the movie answer.
#[test]
fn cinemeta_finds_movie_by_validated_title() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_addon(Arc::clone(&paths));
    let source = CinemetaSource::new(server.url(), http_client());
    let q = movie_query("Inception", 2010);
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Inception");
    assert_eq!(media.year, 2010);
    assert_eq!(media.kind, Kind::MOVIE);
    assert_eq!(media.source, "cinemeta");
    assert_eq!(media.source_id, "tt1375666");
    assert_eq!(media.length, "148 min");
    assert_eq!(media.genres, "Adventure \u{00b7} Sci-Fi \u{00b7} Thriller");
    assert!(
        !media.synopsis.is_empty(),
        "synopsis must survive the cap/strip helpers"
    );
    assert!(
        media.artwork.starts_with("https://"),
        "artwork = {:?}",
        media.artwork
    );
    assert_eq!(
        media.movie_url, "https://www.imdb.com/title/tt1375666/",
        "movie_url = {:?}",
        media.movie_url
    );
    assert!(
        media.episodes.is_empty(),
        "movie must have no episodes: {:?}",
        media.episodes
    );

    let dir = TempDir::new();
    let resolver = resolver_with(
        &dir.path(),
        vec![Arc::new(CinemetaSource::new(server.url(), http_client()))],
    );
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert!(
        res.found && res.is_movie && res.series == "Inception" && res.year == 2010,
        "resolution = {res:?}"
    );
    assert_eq!(res.length, "148 min");
    assert!(
        paths
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.starts_with("/meta/movie/")),
        "the meta leg must have been fetched: {paths:?}"
    );
}

/// The iTunes failure, inverted: a catalog whose best title does not match
/// the query is a definitive miss, and the meta leg is never fetched.
#[test]
fn cinemeta_rejects_wrong_title_without_meta_fetch() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&paths);
    let server = FakeServer::start(move |req: &Request| {
        recorded.lock().unwrap().push(req.path.clone());
        if req.path.starts_with("/catalog/") {
            return Response::json(
                r#"{"metas":[{"id":"tt9999999","name":"Ring of Fire","releaseInfo":"2012"}]}"#,
            );
        }
        Response::error(404, "not found")
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let found = source
        .find(&Cancel::background(), &movie_query("Inception", 0))
        .expect("Find");
    assert!(found.is_none(), "a wrong title must miss, got {found:?}");
    assert!(
        !paths.lock().unwrap().iter().any(|p| p.starts_with("/meta/")),
        "no meta fetch for a rejected title: {paths:?}"
    );
}

/// Year NEVER rejects: a known query year that disagrees with the pick's
/// release year still answers with the essentials -- the meta leg IS fetched
/// and the pick's year rides along as a display field.
#[test]
fn cinemeta_year_mismatch_still_answers() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_addon(Arc::clone(&paths));
    let source = CinemetaSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &movie_query("Inception", 2000))
        .expect("Find")
        .expect("a year mismatch must still answer, got nil");
    assert_eq!(media.title, "Inception");
    assert_eq!(media.year, 2010, "the pick's year rides along: {media:?}");
    assert!(
        paths.lock().unwrap().iter().any(|p| p.starts_with("/meta/")),
        "the meta leg must be fetched despite the mismatch: {paths:?}"
    );
}

/// Series round trip: the episode table arrives in the meta call and the
/// resolver carries the number-exact episode title.
#[test]
fn cinemeta_resolves_series_episode() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let server = fake_addon(Arc::clone(&paths));
    let source = CinemetaSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Breaking Bad");
    assert_eq!(media.year, 2008);
    assert_eq!(media.genres, "Drama \u{00b7} Crime");
    assert_eq!(media.episodes.len(), 1);
    assert_eq!(media.episodes[0].title, "Pilot");

    // A movie query is served by the movie leg, never the series table.
    let movie = source
        .find(&Cancel::background(), &movie_query("Inception", 2010))
        .expect("movie Find")
        .expect("expected movie media");
    assert_eq!(movie.kind, Kind::MOVIE);

    let dir = TempDir::new();
    let resolver = resolver_with(
        &dir.path(),
        vec![Arc::new(CinemetaSource::new(server.url(), http_client()))],
    );
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert!(
        res.found && res.series == "Breaking Bad" && res.episode == "Pilot",
        "resolution = {res:?}"
    );
}

/// Duplicate exact titles: the live catalog carries two string-exact
/// "Inception" rows, and the year-less one sorts first. A known query year
/// must still land on the right id.
#[test]
fn cinemeta_year_disambiguates_duplicate_exact_titles() {
    let server = FakeServer::start(|req: &Request| {
        if req.path.starts_with("/catalog/") {
            return Response::json(
                r#"{"metas":[{"id":"tt5581256","name":"Inception"},{"id":"tt1375666","name":"Inception","releaseInfo":"2010"}]}"#,
            );
        }
        if req.path.as_str() == "/meta/movie/tt1375666.json" {
            return Response::json(MOVIE_META);
        }
        Response::error(404, "not found")
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &movie_query("Inception", 2010))
        .expect("Find")
        .expect("the year-matching exact must answer");
    assert_eq!(media.source_id, "tt1375666");
    assert_eq!(media.year, 2010);
}

/// No meta year veto: details that disagree with a known query year are still
/// a card, never a miss -- the essentials win over the nice-to-have year.
#[test]
fn cinemeta_meta_year_disagreement_still_answers() {
    let server = FakeServer::start(|req: &Request| {
        if req.path.starts_with("/catalog/") {
            return Response::json(
                r#"{"metas":[{"id":"tt1375666","name":"Inception","releaseInfo":"2010"}]}"#,
            );
        }
        Response::json(
            r#"{"meta":{"name":"Inception","year":"2000","runtime":"148 min","genre":["Sci-Fi"],"description":"Wrong decade.","poster":"https://example.com/p.jpg"}}"#,
        )
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &movie_query("Inception", 2010))
        .expect("Find")
        .expect("a meta year disagreement must still answer, got nil");
    assert_eq!(media.title, "Inception");
}

/// LIVE: the real Cinemeta endpoint answers `Inception` -- the title the old
/// iTunes leg mismatched -- with year, runtime, genres, synopsis and poster.
/// Real network, real input; a mock proves nothing here.
#[test]
fn cinemeta_live_inception_round_trip() {
    let source = CinemetaSource::new("", http_client());
    let cancel = Cancel::with_deadline(Duration::from_secs(25));
    let media = source
        .find(&cancel, &movie_query("Inception", 2010))
        .expect("live Find")
        .expect("the live catalog must answer Inception");
    assert_eq!(media.title, "Inception", "media = {media:?}");
    assert_eq!(media.year, 2010, "media = {media:?}");
    assert_eq!(media.length, "148 min", "media = {media:?}");
    assert!(
        media.genres.contains("Sci-Fi"),
        "genres = {:?}",
        media.genres
    );
    assert!(
        !media.synopsis.is_empty(),
        "the live synopsis must be non-empty"
    );
    assert!(
        media.artwork.starts_with("https://"),
        "artwork = {:?}",
        media.artwork
    );
}

/// DBS 2026 regression (measured 2026-10-15): the catalog ranks the perfect-
/// title 2015 series first, but a 2026 query must land on Beerus.
#[test]
fn cinemeta_year_prefers_beerus_over_2015_series() {
    let server = FakeServer::start(|req: &Request| {
        if req.path.starts_with("/catalog/") {
            return Response::json(
                r#"{"metas":[{"id":"tt4644488","name":"Dragon Ball Super","releaseInfo":"2015-2018"},{"id":"tt39395275","name":"Dragon Ball Super: Beerus","releaseInfo":"2026-"}]}"#,
            );
        }
        match req.path.as_str() {
            "/meta/series/tt39395275.json" => Response::json(
                r#"{"meta":{"name":"Dragon Ball Super: Beerus","year":"2026","genre":["Animation"],"description":"The Beerus arc.","poster":"https://example.com/beerus.jpg","videos":[]}}"#,
            ),
            "/meta/series/tt4644488.json" => Response::json(
                r#"{"meta":{"name":"Dragon Ball Super","year":"2015","genre":["Animation"],"description":"The 2015 series.","poster":"https://example.com/dbs.jpg","videos":[]}}"#,
            ),
            _ => Response::error(404, "not found"),
        }
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let media = source
        .find(
            &Cancel::background(),
            &Query {
                kind: Kind::SERIES,
                title: "dragon ball super".to_string(),
                year: 2026,
                ..Query::default()
            },
        )
        .expect("Find")
        .expect("the 2026 query must answer, got nil");
    assert_eq!(media.source_id, "tt39395275", "media = {media:?}");
    assert_eq!(media.title, "Dragon Ball Super: Beerus");
    assert_eq!(media.year, 2026);
}

/// A romaji query the token scorer cannot verify against the English display
/// name: the title winner is a look-alike special, and the catalog's own top
/// row (`Attack on Titan` for `Shingeki no Kyojin`) answers only because the
/// query year corroborates it AND its episode table carries the wanted S2E1
/// -- a genuinely different show cannot.
#[test]
fn cinemeta_series_catalog_top_row_answers_romaji_query() {
    let server = FakeServer::start(|req: &Request| {
        if req.path.starts_with("/catalog/series/") {
            return Response::json(
                r#"{"metas":[
                    {"id":"tt2560140","name":"Attack on Titan","releaseInfo":"2013-2023"},
                    {"id":"tt6811662","name":"Shingeki no Kyojin: Picture Drama","releaseInfo":"2013-2014"}
                ]}"#,
            );
        }
        match req.path.as_str() {
            "/meta/series/tt6811662.json" => Response::json(
                r#"{"meta":{"name":"Shingeki no Kyojin: Picture Drama","year":"2013","genre":["Animation"],"description":"Specials.","poster":"https://example.com/pd.jpg","videos":[{"name":"Day 1","season":1,"number":1,"overview":"x"}]}}"#,
            ),
            "/meta/series/tt2560140.json" => Response::json(
                r#"{"meta":{"name":"Attack on Titan","year":"2013","genre":["Animation"],"description":"The series.","poster":"https://example.com/aot.jpg","videos":[{"name":"To You, 2,000 Years in the Future","season":1,"number":1,"overview":"S1"},{"name":"Beast Titan","season":2,"number":1,"overview":"S2"}]}}"#,
            ),
            _ => Response::error(404, "not found"),
        }
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::SERIES,
        title: "Shingeki no Kyojin".to_string(),
        year: 2013,
        season: 2,
        episode: 1,
        ..Query::default()
    };
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("the catalog top row must answer, got nil");
    assert_eq!(media.source_id, "tt2560140", "media = {media:?}");
    assert_eq!(media.title, "Attack on Titan");
    assert_eq!(media.episodes.len(), 2);

    let dir = TempDir::new();
    let resolver = resolver_with(
        &dir.path(),
        vec![Arc::new(CinemetaSource::new(server.url(), http_client()))],
    );
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert!(
        res.found
            && res.series == "Attack on Titan"
            && res.season == 2
            && res.number == 1
            && res.episode == "Beast Titan",
        "resolution = {res:?}"
    );
}

/// The other side of the year-corroborated top row: the same romaji setup
/// with a year the top row does not carry answers nothing -- the fallback
/// never fires on an unknown or differing year.
#[test]
fn cinemeta_series_top_row_needs_year_corroboration() {
    for (label, year) in [("unknown year", 0), ("differing year", 2015)] {
        let server = FakeServer::start(|req: &Request| {
            if req.path.starts_with("/catalog/series/") {
                return Response::json(
                    r#"{"metas":[
                        {"id":"tt2560140","name":"Attack on Titan","releaseInfo":"2013-2023"},
                        {"id":"tt6811662","name":"Shingeki no Kyojin: Picture Drama","releaseInfo":"2013-2014"}
                    ]}"#,
                );
            }
            match req.path.as_str() {
                "/meta/series/tt6811662.json" => Response::json(
                    r#"{"meta":{"name":"Shingeki no Kyojin: Picture Drama","year":"2013","genre":["Animation"],"description":"Specials.","poster":"https://example.com/pd.jpg","videos":[{"name":"Day 1","season":1,"number":1,"overview":"x"}]}}"#,
                ),
                "/meta/series/tt2560140.json" => Response::json(
                    r#"{"meta":{"name":"Attack on Titan","year":"2013","genre":["Animation"],"description":"The series.","poster":"https://example.com/aot.jpg","videos":[{"name":"To You, 2,000 Years in the Future","season":1,"number":1,"overview":"S1"},{"name":"Beast Titan","season":2,"number":1,"overview":"S2"}]}}"#,
                ),
                _ => Response::error(404, "not found"),
            }
        });
        let source = CinemetaSource::new(server.url(), http_client());
        let q = Query {
            kind: Kind::SERIES,
            title: "Shingeki no Kyojin".to_string(),
            year,
            season: 2,
            episode: 1,
            ..Query::default()
        };
        let found = source.find(&Cancel::background(), &q).expect("Find");
        assert!(
            found.is_none(),
            "{label}: the top row must not answer without year corroboration, got {found:?}"
        );
    }
}

/// The other side of the top-row rule: a genuinely different show loses.
/// Neither row's table carries the wanted episode, so the source is a
/// definitive miss -- never a wrong card.
#[test]
fn cinemeta_series_wrong_rows_still_lose() {
    let server = FakeServer::start(|req: &Request| {
        if req.path.starts_with("/catalog/series/") {
            return Response::json(
                r#"{"metas":[
                    {"id":"tt0000001","name":"Some Other Show","releaseInfo":"2016"},
                    {"id":"tt6811662","name":"Shingeki no Kyojin: Picture Drama","releaseInfo":"2013-2014"}
                ]}"#,
            );
        }
        match req.path.as_str() {
            "/meta/series/tt6811662.json" | "/meta/series/tt0000001.json" => Response::json(
                r#"{"meta":{"name":"Shingeki no Kyojin: Picture Drama","year":"2013","genre":["Animation"],"description":"Specials.","poster":"https://example.com/pd.jpg","videos":[{"name":"Day 1","season":1,"number":1,"overview":"x"}]}}"#,
            ),
            _ => Response::error(404, "not found"),
        }
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::SERIES,
        title: "Shingeki no Kyojin".to_string(),
        season: 2,
        episode: 1,
        ..Query::default()
    };
    let found = source.find(&Cancel::background(), &q).expect("Find");
    assert!(
        found.is_none(),
        "a table without the wanted episode must miss, got {found:?}"
    );
}

/// A junk-laden movie title below the title floor still answers when the
/// catalog's own top row carries the salvage year; the same row carrying a
/// different year is a definitive miss and the meta leg is never touched.
#[test]
fn cinemeta_movie_junk_title_year_seeds_top_row() {
    let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&paths);
    let server = FakeServer::start(move |req: &Request| {
        recorded.lock().unwrap().push(req.path.clone());
        if req.path.starts_with("/catalog/movie/") {
            return Response::json(
                r#"{"metas":[
                    {"id":"tt0185937","name":"The Blair Witch Project","releaseInfo":"1999"},
                    {"id":"tt7740342","name":"The Blair Witch Legacy","releaseInfo":"2018"}
                ]}"#,
            );
        }
        if req.path.as_str() == "/meta/movie/tt0185937.json" {
            return Response::json(
                r#"{"meta":{"name":"The Blair Witch Project","year":"1999","runtime":"81 min","genre":["Horror"],"description":"Three film students vanish.","poster":"https://example.com/bwp.jpg"}}"#,
            );
        }
        Response::error(404, "not found")
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let q = Query {
        kind: Kind::MOVIE,
        title: "The Blair Witch Project 1999 REMUX AVC MA 2 0".to_string(),
        year: 1999,
        ..Query::default()
    };
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("the year-confirmed top row must answer, got nil");
    assert_eq!(media.source_id, "tt0185937", "media = {media:?}");
    assert_eq!(media.year, 1999);

    paths.lock().unwrap().clear();
    let q2 = Query {
        year: 2001,
        ..q.clone()
    };
    let miss = source.find(&Cancel::background(), &q2).expect("Find");
    assert!(miss.is_none(), "a year mismatch must miss, got {miss:?}");
    assert!(
        !paths
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.starts_with("/meta/")),
        "no meta fetch without the year key: {paths:?}"
    );
}

/// Never-a-veto: a query year matching NOTHING still returns the best title
/// match (the 2015 series) rather than `Ok(None)`.
#[test]
fn cinemeta_year_matching_nothing_keeps_best_title() {
    let server = FakeServer::start(|req: &Request| {
        if req.path.starts_with("/catalog/") {
            return Response::json(
                r#"{"metas":[{"id":"tt4644488","name":"Dragon Ball Super","releaseInfo":"2015-2018"},{"id":"tt39395275","name":"Dragon Ball Super: Beerus","releaseInfo":"2026-"}]}"#,
            );
        }
        match req.path.as_str() {
            "/meta/series/tt4644488.json" => Response::json(
                r#"{"meta":{"name":"Dragon Ball Super","year":"2015","genre":["Animation"],"description":"The 2015 series.","poster":"https://example.com/dbs.jpg","videos":[]}}"#,
            ),
            _ => Response::error(404, "not found"),
        }
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let media = source
        .find(
            &Cancel::background(),
            &Query {
                kind: Kind::SERIES,
                title: "dragon ball super".to_string(),
                year: 1999,
                ..Query::default()
            },
        )
        .expect("Find")
        .expect("an unmatched year must still answer, got nil");
    assert_eq!(media.source_id, "tt4644488", "media = {media:?}");
}

/// `Blade Runner 2049 (2017)` salvages the parenthesised 2017 (the bare 2049
/// belongs to the title) and the movie leg answers the 2049 row, never the
/// 1982 original.
#[test]
fn cinemeta_blade_runner_2049_parenthesised_year_matches() {
    use metadata_core::parse::parse_raw;
    let q = parse_raw("Blade Runner 2049 (2017)");
    assert_eq!(
        (q.title.as_str(), q.year),
        ("Blade Runner 2049", 2017),
        "parse must salvage the parenthesised year: {q:?}"
    );
    let server = FakeServer::start(|req: &Request| {
        if req.path.starts_with("/catalog/movie/") {
            return Response::json(
                r#"{"metas":[{"id":"tt1856101","name":"Blade Runner 2049","releaseInfo":"2017"},{"id":"tt0083658","name":"Blade Runner","releaseInfo":"1982"}]}"#,
            );
        }
        if req.path.as_str() == "/meta/movie/tt1856101.json" {
            return Response::json(
                r#"{"meta":{"name":"Blade Runner 2049","year":"2017","runtime":"164 min","genre":["Sci-Fi"],"description":"A young blade runner's discovery of a long-buried secret.","poster":"https://example.com/br2049.jpg"}}"#,
            );
        }
        Response::error(404, "not found")
    });
    let source = CinemetaSource::new(server.url(), http_client());
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("the 2049 row must answer, got nil");
    assert_eq!(media.source_id, "tt1856101", "media = {media:?}");
    assert_eq!(media.title, "Blade Runner 2049");
    assert_eq!(media.year, 2017);
}
