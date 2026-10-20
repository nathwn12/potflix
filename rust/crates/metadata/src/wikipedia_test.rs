//! Ports `go\internal\metadata\wikipedia_test.go`.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use metadata_core::types::{Kind, Query};
use metadata_http::cancel::Cancel;

use crate::resolver::{Options, Resolver, Source};
use crate::testsupport::{FakeServer, Request, Response, TempDir, dead_url, wiki_client};
use crate::wikipedia::{
    WIKIPEDIA_USER_AGENT, WikipediaSource, strip_disambiguation, wiki_pick_poster,
    wiki_sanitize_cause, wiki_transient, wiki_year,
};

const WIKI_HIT_SEARCH: &str =
    r#"{"query":{"search":[{"title":"Dune (2021 film)"},{"title":"Dune (novel)"}]}}"#;
const WIKI_HIT_DETAIL: &str = r#"{"query":{"pages":{"12345":{"pageid":12345,"title":"Dune (2021 film)","extract":"Dune is a 2021 American epic science fiction film directed by Denis Villeneuve.","thumbnail":{"source":"https://upload.wikimedia.org/wikipedia/en/thumb/8/8e/Dune_poster.jpg","width":600,"height":900}}}}}"#;
const WIKI_NO_POSTER_DETAIL: &str = r#"{"query":{"pages":{"12345":{"pageid":12345,"title":"Dune (2021 film)","extract":"Dune is a 2021 American epic science fiction film directed by Denis Villeneuve."}}}}"#;

/// `wikiFake`: canned search+detail JSON, plus the hits and the User-Agent it
/// saw, so the tests prove the wire shape without the network.
struct WikiFake {
    hits: Arc<AtomicUsize>,
    saw_ua: Arc<Mutex<String>>,
    detail_query: Arc<Mutex<String>>,
    images_query: Arc<Mutex<String>>,
    search: String,
    detail: String,
    images: String,
    imageinfo: String,
    fail_mode: String,
}

impl WikiFake {
    fn new() -> WikiFake {
        WikiFake {
            hits: Arc::new(AtomicUsize::new(0)),
            saw_ua: Arc::new(Mutex::new(String::new())),
            detail_query: Arc::new(Mutex::new(String::new())),
            images_query: Arc::new(Mutex::new(String::new())),
            search: String::new(),
            detail: String::new(),
            images: String::new(),
            imageinfo: String::new(),
            fail_mode: String::new(),
        }
    }

    fn with(search: &str, detail: &str) -> WikiFake {
        WikiFake {
            search: search.to_string(),
            detail: detail.to_string(),
            ..WikiFake::new()
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn saw_ua(&self) -> String {
        self.saw_ua.lock().unwrap().clone()
    }

    fn detail_query(&self) -> String {
        self.detail_query.lock().unwrap().clone()
    }

    fn images_query(&self) -> String {
        self.images_query.lock().unwrap().clone()
    }

    fn server(&self) -> FakeServer {
        let hits = Arc::clone(&self.hits);
        let saw_ua = Arc::clone(&self.saw_ua);
        let detail_query = Arc::clone(&self.detail_query);
        let images_query = Arc::clone(&self.images_query);
        let search = self.search.clone();
        let detail = self.detail.clone();
        let images = self.images.clone();
        let imageinfo = self.imageinfo.clone();
        let fail_mode = self.fail_mode.clone();
        FakeServer::start(move |req: &Request| {
            hits.fetch_add(1, Ordering::SeqCst);
            *saw_ua.lock().unwrap() = req.header("user-agent");
            if req.query("list").as_deref() == Some("search") {
                if fail_mode == "search500" {
                    return Response::error(500, "boom");
                }
                if fail_mode == "badjson" {
                    return Response::json("{not json");
                }
                let body: &str = if search.is_empty() {
                    r#"{"query":{"search":[]}}"#
                } else {
                    &search
                };
                return Response::json(body);
            }
            if req.query("prop").as_deref() == Some("images") {
                *images_query.lock().unwrap() = req.raw_query.clone();
                if fail_mode == "images500" {
                    return Response::error(500, "boom");
                }
                let body: &str = if images.is_empty() {
                    r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[]}}}}"#
                } else {
                    &images
                };
                return Response::json(body);
            }
            if req.query("prop").unwrap_or_default().contains("imageinfo") {
                if fail_mode == "imageinfo500" {
                    return Response::error(500, "boom");
                }
                let body: &str = if imageinfo.is_empty() {
                    r#"{"query":{"pages":{"-1":{"missing":true}}}}"#
                } else {
                    &imageinfo
                };
                return Response::json(body);
            }
            if fail_mode == "detail500" {
                return Response::error(500, "boom");
            }
            *detail_query.lock().unwrap() = req.raw_query.clone();
            Response::json(&detail)
        })
    }

    fn source(&self) -> (FakeServer, WikipediaSource) {
        let server = self.server();
        let source = WikipediaSource::new(server.url(), wiki_client());
        (server, source)
    }
}

fn movie_query(title: &str, year: i32) -> Query {
    Query {
        kind: Kind::MOVIE,
        title: title.to_string(),
        year,
        ..Query::default()
    }
}

/// `TestWikipediaHit`: the two-call path yields title, year from "(2021 film)",
/// the thumbnail poster and the extract synopsis. The page title's
/// disambiguation suffix is stripped (`Dune`, not `Dune (2021 film)`): the live
/// card showed the raw suffix and doubled the year in the movie line.
#[test]
fn wikipedia_hit() {
    let fake = WikiFake::with(WIKI_HIT_SEARCH, WIKI_HIT_DETAIL);
    let (server, source) = fake.source();
    let q = movie_query("Dune", 2021);
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Dune");
    assert_eq!(media.year, 2021);
    assert_eq!(media.kind, Kind::MOVIE);
    assert!(
        media.artwork.starts_with("https://upload.wikimedia.org/"),
        "artwork = {:?}",
        media.artwork
    );
    assert!(
        media
            .synopsis
            .contains("2021 American epic science fiction film"),
        "synopsis = {:?}",
        media.synopsis
    );
    assert_eq!(media.source, "wikipedia");
    assert!(media.episodes.is_empty());
    assert_eq!(fake.hits(), 2, "hits must be exactly search+detail");
    assert!(
        fake.saw_ua().contains("Potflix"),
        "User-Agent = {:?}, want the Wikimedia policy identifier",
        fake.saw_ua()
    );
    assert!(
        fake.saw_ua().contains(WIKIPEDIA_USER_AGENT),
        "the Wikipedia wire must send the descriptive UA, not the shared one"
    );

    let dir = TempDir::new();
    let resolver = resolver_with_wiki(&dir.path(), server.url());
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert!(
        res.found && res.is_movie && res.year == 2021,
        "resolution = {res:?}"
    );
    assert!(
        !res.artwork.is_empty() && !res.synopsis.is_empty(),
        "resolution lacks card fields: {res:?}"
    );
}

fn resolver_with_wiki(dir: &str, base: String) -> Resolver {
    Resolver::new(Options {
        dir: dir.to_string(),
        timeout: Duration::from_secs(5),
        sources: Some(vec![Arc::new(WikipediaSource::new(base, wiki_client()))]),
        ..Options::default()
    })
}

/// Among several search hits, a known query year prefers the hit whose
/// parenthesized year matches it (`Dune (1984 film)` ranking above
/// `Dune (2021 film)` must not win a 2021 query); a year-less query keeps
/// the first hit.
#[test]
fn wikipedia_prefers_year_matching_hit() {
    const SEARCH: &str = r#"{"query":{"search":[{"title":"Dune (1984 film)"},{"title":"Dune (2021 film)"}]}}"#;
    const DETAIL_2021: &str = r#"{"query":{"pages":{"12345":{"pageid":12345,"title":"Dune (2021 film)","extract":"Dune is a 2021 American epic science fiction film directed by Denis Villeneuve.","thumbnail":{"source":"https://upload.wikimedia.org/wikipedia/en/thumb/8/8e/Dune_poster.jpg","width":600,"height":900}}}}}"#;
    const DETAIL_1984: &str = r#"{"query":{"pages":{"67890":{"pageid":67890,"title":"Dune (1984 film)","extract":"Dune is a 1984 American science fiction film directed by David Lynch.","thumbnail":{"source":"https://upload.wikimedia.org/wikipedia/en/thumb/8/8e/Dune_1984_poster.jpg","width":600,"height":900}}}}}"#;

    let fake = WikiFake::with(SEARCH, DETAIL_2021);
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 2021))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Dune");
    assert_eq!(media.year, 2021, "the 2021 hit must win a 2021 query");
    assert!(
        fake.detail_query().contains("2021"),
        "the detail call must address the 2021 page, query = {:?}",
        fake.detail_query()
    );

    let fake = WikiFake::with(SEARCH, DETAIL_1984);
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.year, 1984, "a year-less query keeps the first hit");
}

/// `TestWikipediaDefinitiveMiss`: every "no such page" shape is a definitive
/// miss, never the transient sentinel.
#[test]
fn wikipedia_definitive_miss() {
    let cases: Vec<(&str, &str, &str)> = vec![
        ("empty search results", "", WIKI_HIT_DETAIL),
        (
            "blank search title",
            r#"{"query":{"search":[{"title":"  "}]}}"#,
            WIKI_HIT_DETAIL,
        ),
        (
            "missing page",
            WIKI_HIT_SEARCH,
            r#"{"query":{"pages":{"-1":{"title":"Dune (2021 film)","missing":true}}}}"#,
        ),
        ("no pages", WIKI_HIT_SEARCH, r#"{"query":{"pages":{}}}"#),
    ];
    for (name, search, detail) in cases {
        let fake = WikiFake::with(search, detail);
        let (_server, source) = fake.source();
        let media = source
            .find(&Cancel::background(), &movie_query("Xyzzy Qqq", 0))
            .unwrap_or_else(|err| panic!("{name}: Find = Err({err}), want (nil, nil)"));
        assert!(media.is_none(), "{name}: Find = {media:?}, want None");
    }
}

/// `TestWikipediaTransientIsSentinel`: transport and API failures are
/// distinguishable from a miss.
#[test]
fn wikipedia_transient_is_sentinel() {
    for fail in ["search500", "detail500", "badjson"] {
        let fake = WikiFake {
            fail_mode: fail.to_string(),
            ..WikiFake::with(WIKI_HIT_SEARCH, WIKI_HIT_DETAIL)
        };
        let (_server, source) = fake.source();
        let err = source
            .find(&Cancel::background(), &movie_query("Dune", 0))
            .expect_err("Find must be transient");
        assert!(
            err.is_lookup_unavailable(),
            "{fail}: err = {err}, want ErrLookupUnavailable"
        );
    }

    // Dead server (connection refused) is transient too.
    let source = WikipediaSource::new(dead_url(), wiki_client());
    let err = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect_err("dead-server Find must be transient");
    assert!(err.is_lookup_unavailable(), "err = {err}");
}

/// `TestWikipediaKindGate`: a series key never reaches the wire, and a blank
/// title gates the same way. A movie query DOES reach the wire.
#[test]
fn wikipedia_kind_gate() {
    let fake = WikiFake::with(WIKI_HIT_SEARCH, WIKI_HIT_DETAIL);
    let (_server, source) = fake.source();
    for q in [
        Query {
            kind: Kind::SERIES,
            title: "Breaking Bad".to_string(),
            season: 1,
            episode: 1,
            ..Query::default()
        },
        Query {
            kind: Kind::MOVIE,
            title: String::new(),
            ..Query::default()
        },
    ] {
        let media = source
            .find(&Cancel::background(), &q)
            .unwrap_or_else(|err| panic!("gated query {q:?}: Find = Err({err})"));
        assert!(media.is_none(), "gated query {q:?} returned media");
    }
    assert_eq!(fake.hits(), 0, "gated queries reached the wire");

    // Positive control: a movie query DOES reach the wire.
    source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("movie Find");
    assert!(fake.hits() > 0, "the movie positive control made no calls");
}

/// `TestWikipediaNoPosterStillFound`: a poster-less page still yields a FOUND
/// movie with title, year and synopsis, Artwork honestly empty. The title is
/// the stripped page title (`Dune`): the suffix never reaches the card.
#[test]
fn wikipedia_no_poster_still_found() {
    let fake = WikiFake::with(WIKI_HIT_SEARCH, WIKI_NO_POSTER_DETAIL);
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.artwork, "", "artwork must stay empty (no thumbnail)");
    assert_eq!(media.title, "Dune");
    assert_eq!(media.year, 2021);
    assert!(!media.synopsis.is_empty());
    assert!(
        media.movie_url.starts_with("https://"),
        "movieURL = {:?}",
        media.movie_url
    );
}

/// `TestWikipediaPosterFromImageList`: a resolved page with no PageImages
/// thumbnail lists the article files, picks the poster-like file, and resolves
/// it via `prop=imageinfo` -- exactly 4 calls. The fixture mixes in a logo and
/// an svg "poster" to prove the pick rule rejects them. The title assertion
/// pins the stripped page title alongside the artwork pick.
#[test]
fn wikipedia_poster_from_image_list() {
    let fake = WikiFake {
        search: WIKI_HIT_SEARCH.to_string(),
        detail: WIKI_NO_POSTER_DETAIL.to_string(),
        images: r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[{"title":"File:Dune logo.png"},{"title":"File:Dune poster guidelines.svg"},{"title":"File:Dune theatrical poster.jpg"}]}}}}"#.to_string(),
        imageinfo: r#"{"query":{"pages":{"999":{"pageid":999,"title":"File:Dune theatrical poster.jpg","imageinfo":[{"url":"https://upload.wikimedia.org/wikipedia/en/8/8e/Dune_theatrical_poster.jpg"}]}}}}"#.to_string(),
        ..WikiFake::new()
    };
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(
        media.artwork,
        "https://upload.wikimedia.org/wikipedia/en/8/8e/Dune_theatrical_poster.jpg"
    );
    assert_eq!(media.title, "Dune");
    assert_eq!(media.year, 2021);
    assert!(!media.synopsis.is_empty());
    assert_eq!(fake.hits(), 4, "search+detail+images+imageinfo");
}

/// `TestWikipediaPosterAbsentStillFound`: a poster-less image list keeps
/// today's behavior -- found, with Artwork honestly empty -- and the stripped
/// title (`Dune`).
#[test]
fn wikipedia_poster_absent_still_found() {
    let fake = WikiFake {
        search: WIKI_HIT_SEARCH.to_string(),
        detail: WIKI_NO_POSTER_DETAIL.to_string(),
        images: r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[]}}}}"#.to_string(),
        ..WikiFake::new()
    };
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.artwork, "", "no poster-like file means empty artwork");
    assert_eq!(media.title, "Dune");
    assert_eq!(media.year, 2021);
    assert!(!media.synopsis.is_empty());
    assert_eq!(fake.hits(), 3, "search+detail+images, no imageinfo call");
}

/// `TestWikipediaNonPosterImageIgnored`: a non-poster image (a logo, a cast
/// photo) is never substituted, and the title is still the stripped page title.
#[test]
fn wikipedia_non_poster_image_ignored() {
    let fake = WikiFake {
        search: WIKI_HIT_SEARCH.to_string(),
        detail: WIKI_NO_POSTER_DETAIL.to_string(),
        images: r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[{"title":"File:Dune logo.png"},{"title":"File:Dune cast at premiere.jpg"}]}}}}"#.to_string(),
        imageinfo: r#"{"query":{"pages":{"999":{"pageid":999,"title":"File:Dune logo.png","imageinfo":[{"url":"https://upload.wikimedia.org/wikipedia/en/9/9e/Dune_logo.png"}]}}}}"#.to_string(),
        ..WikiFake::new()
    };
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.artwork, "", "a logo/cast photo must never substitute");
    assert_eq!(media.title, "Dune");
    assert!(!media.synopsis.is_empty());
    assert_eq!(fake.hits(), 3, "no imageinfo call for a non-poster list");
}

/// `TestWikipediaPosterFailureStillFound`: the fallback is best-effort -- a 500
/// on either poster call still yields the resolved movie with empty Artwork,
/// never the transient sentinel.
#[test]
fn wikipedia_poster_failure_still_found() {
    for mode in ["images500", "imageinfo500"] {
        let fake = WikiFake {
            search: WIKI_HIT_SEARCH.to_string(),
            detail: WIKI_NO_POSTER_DETAIL.to_string(),
            images: r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[{"title":"File:Dune theatrical poster.jpg"}]}}}}"#.to_string(),
            imageinfo: r#"{"query":{"pages":{"999":{"pageid":999,"title":"File:Dune theatrical poster.jpg","imageinfo":[{"url":"https://upload.wikimedia.org/wikipedia/en/8/8e/Dune_theatrical_poster.jpg"}]}}}}"#.to_string(),
            fail_mode: mode.to_string(),
            ..WikiFake::new()
        };
        let (_server, source) = fake.source();
        let media = source
            .find(&Cancel::background(), &movie_query("Dune", 0))
            .unwrap_or_else(|err| panic!("{mode}: Find err = {err}, want nil (best-effort)"));
        let media = media.unwrap_or_else(|| panic!("{mode}: expected media, got nil"));
        assert_eq!(media.artwork, "", "{mode}: artwork must stay empty");
    }
}

/// `TestWikiPickPoster`: "poster" case-insensitive plus a photo extension wins;
/// `.jpg`/`.jpeg` outrank `.png` outrank `.webp`; `.svg` and extensionless names
/// are rejected even with "poster" in them.
#[test]
fn wiki_pick_poster_ranks_extensions() {
    let titles = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    let cases: Vec<(&str, Vec<String>, &str)> = vec![
        (
            "jpg wins over png",
            titles(&["File:Film poster.png", "File:Film poster.jpg"]),
            "File:Film poster.jpg",
        ),
        (
            "case insensitive",
            titles(&["File:Film THEATRICAL POSTER.JPEG"]),
            "File:Film THEATRICAL POSTER.JPEG",
        ),
        (
            "png accepted",
            titles(&["File:Film poster.png"]),
            "File:Film poster.png",
        ),
        (
            "webp last resort",
            titles(&["File:Film poster.webp"]),
            "File:Film poster.webp",
        ),
        ("svg rejected", titles(&["File:Film poster.svg"]), ""),
        ("logo rejected", titles(&["File:Film logo.png"]), ""),
        (
            "cast photo rejected",
            titles(&["File:Film cast at premiere.jpg"]),
            "",
        ),
        ("empty list", Vec::new(), ""),
    ];
    for (name, list, want) in cases {
        assert_eq!(wiki_pick_poster(&list, "Dune (2021 film)"), want, "{name}");
    }
}

/// `TestWikiYear`: the paren year in the page title wins, else the first
/// plausible year in the extract, else empty (never invented).
#[test]
fn wiki_year_extraction() {
    let cases: Vec<(&str, &str, &str, i32)> = vec![
        (
            "paren film year",
            "Dune (2021 film)",
            "A science fiction film.",
            2021,
        ),
        ("paren bare year", "Heat (1995)", "Crime film.", 1995),
        (
            "extract fallback",
            "Some Film",
            "Released in 2019 to acclaim.",
            2019,
        ),
        (
            "no year stays empty",
            "Some Film",
            "A film of unknown vintage.",
            0,
        ),
        (
            "far future not a year",
            "Future Film",
            "Set in the year 2200.",
            0,
        ),
    ];
    for (name, title, extract, want) in cases {
        assert_eq!(wiki_year(title, extract), want, "{name}");
    }
}

/// `TestWikipediaMovieOrder`: for a movie query the default chain ranks
/// Cinemeta first with Wikipedia immediately after.
#[test]
fn wikipedia_movie_order() {
    let dir = TempDir::new();
    let resolver = Resolver::new(Options {
        dir: dir.path(),
        timeout: Duration::from_secs(5),
        ..Options::default()
    });
    let names: Vec<String> = resolver
        .ordered_sources(&movie_query("Dune", 0))
        .iter()
        .map(|source| source.name().to_string())
        .collect();
    assert!(
        names.len() >= 2 && names[0] == "cinemeta" && names[1] == "wikipedia",
        "movie order = {names:?}, want [cinemeta wikipedia ...]"
    );
    // Series order is untouched: wikipedia self-gates there anyway.
    let series_names: Vec<String> = resolver
        .ordered_sources(&Query {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            season: 1,
            episode: 1,
            ..Query::default()
        })
        .iter()
        .map(|source| source.name().to_string())
        .collect();
    assert!(series_names.contains(&"wikipedia".to_string()));
}

/// `TestWikipediaAPIErrorIsTransientNoNegative`: a 200 body carrying the API
/// error envelope is transient, and the resolver persists no negative entry.
#[test]
fn wikipedia_api_error_is_transient_no_negative() {
    const API_ERR: &str = r#"{"error":{"code":"maxlag","info":"Waiting for db"}}"#;
    for (search, detail) in [(API_ERR, WIKI_HIT_DETAIL), (WIKI_HIT_SEARCH, API_ERR)] {
        let fake = WikiFake::with(search, detail);
        let (_server, source) = fake.source();
        let err = source
            .find(&Cancel::background(), &movie_query("Dune", 0))
            .expect_err("an API error envelope must be transient");
        assert!(err.is_lookup_unavailable(), "err = {err}");
    }

    // Resolver honesty: an API-error lookup persists nothing.
    let fake = WikiFake::with(API_ERR, WIKI_HIT_DETAIL);
    let (server, _source) = fake.source();
    let dir = TempDir::new();
    let d = dir.path();
    let resolver = resolver_with_wiki(&d, server.url());
    let q = movie_query("Dune", 2021);
    let err = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect_err("LookupNow err = nil, want ErrLookupUnavailable");
    assert!(err.is_lookup_unavailable());
    let key = crate::cache::cache_key(&q);
    assert!(
        !crate::cache::cache_has(&d, &key),
        "API error stored a record"
    );
    assert!(
        !resolver.neg_suppressed(&key),
        "API error set in-memory suppression"
    );
}

/// `TestWikipediaTransientOmitsTitle`: a transport failure never leaks the owner
/// title into the error text. Go forces this with an injectable transport; here
/// the same guarantee is proven at the sanitizer (with a title-bearing URL) and
/// end to end against a dead server whose path carries the title.
#[test]
fn wikipedia_transient_omits_title() {
    const SECRET: &str = "UnlikelySecretTitleXYZ987";

    // The sanitizer is what keeps the title out, so prove it directly on the
    // shape ureq/Go produce: a transport message carrying the request URL.
    let cause = format!(
        "Get \"https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={SECRET}+film&srlimit=5&format=json\": connection refused"
    );
    let sanitized = wiki_sanitize_cause(&cause);
    assert!(!sanitized.contains(SECRET), "sanitizer leaks the title");
    assert!(!sanitized.contains("api.php") && !sanitized.contains("srsearch"));
    let wrapped = wiki_transient(&cause);
    assert!(wrapped.is_lookup_unavailable());
    assert!(!wrapped.message().contains(SECRET));
    assert!(!wrapped.message().contains("api.php"));
    assert!(!wrapped.message().contains("srsearch"));
    // URL-free diagnostics pass through verbatim.
    assert_eq!(wiki_sanitize_cause("  boom  "), "boom");
    assert_eq!(wiki_sanitize_cause("   "), "request failed (URL redacted)");

    // End to end: the dead server's own URL carries the title.
    let base = format!("{}/{SECRET}", dead_url());
    let source = WikipediaSource::new(base, wiki_client());
    let err = source
        .find(&Cancel::background(), &movie_query(SECRET, 0))
        .expect_err("Find err = nil, want the transient sentinel");
    assert!(err.is_lookup_unavailable(), "err = {err}");
    assert!(
        !err.message().contains(SECRET),
        "transient err leaks the title: {err:?}"
    );
    assert!(
        !err.message().contains("api.php") && !err.message().contains("srsearch"),
        "transient err leaks the URL/query: {err:?}"
    );
}

/// `TestWikipediaMissingStringIsDefinitiveMiss`: the live string shape
/// `{"missing":""}` is a definitive miss, and the resolver persists the
/// negative.
#[test]
fn wikipedia_missing_string_is_definitive_miss() {
    const MISSING: &str = r#"{"query":{"pages":{"-1":{"title":"Dune (2021 film)","missing":""}}}}"#;
    let fake = WikiFake::with(WIKI_HIT_SEARCH, MISSING);
    let (server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("string-missing Find");
    assert!(media.is_none(), "string missing must be (nil, nil)");

    let dir = TempDir::new();
    let d = dir.path();
    let resolver = resolver_with_wiki(&d, server.url());
    let q = movie_query("Dune", 2021);
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("definitive miss must have a nil error");
    assert!(!res.found, "LookupNow = {res:?}, want miss");
    let key = crate::cache::cache_key(&q);
    assert!(
        crate::cache::cache_record(&d, &key).is_some(),
        "a definitive miss must store one negative"
    );
    let entry = crate::cache::cache_load_entry(&d, &key).expect("negative entry");
    assert!(entry.not_found);
}

/// `TestWikiPickPosterGuidelinesRejected`: a "poster guidelines" doc never
/// becomes Artwork, while a genuine theatrical poster still is picked.
#[test]
fn wiki_pick_poster_guidelines_rejected() {
    let one = |name: &str| vec![name.to_string()];
    assert_eq!(
        wiki_pick_poster(&one("File:Dune poster guidelines.jpg"), "Dune (2021 film)"),
        ""
    );
    assert!(!wiki_pick_poster(&one("File:Dune theatrical poster.jpg"), "Dune (2021 film)").is_empty());
    for name in [
        "File:Dune poster logo.jpg",
        "File:Dune poster list.jpg",
        "File:Dune poster sheet.png",
        "File:Dune film poster style guide.jpg",
        "File:Posterize.jpg",
    ] {
        assert_eq!(wiki_pick_poster(&one(name), "Dune (2021 film)"), "", "blocked pick {name}");
    }
    // A genuine poster still wins over blocked docs (jpg preferred).
    let mixed = vec![
        "File:Dune poster guidelines.jpg".to_string(),
        "File:Dune theatrical poster.jpg".to_string(),
    ];
    assert_eq!(
        wiki_pick_poster(&mixed, "Dune (2021 film)"),
        "File:Dune theatrical poster.jpg",
        "the genuine poster must win"
    );
}

/// `TestWikipediaPageImageOriginal`: the page-image request carries
/// `piprop=thumbnail|original`, and a page image served ONLY as `original` still
/// lands in Artwork -- the poster-file fallback must NOT run when one exists.
#[test]
fn wikipedia_page_image_original() {
    let fake = WikiFake::with(
        WIKI_HIT_SEARCH,
        r#"{"query":{"pages":{"12345":{"pageid":12345,"title":"Dune (2021 film)","extract":"Dune is a 2021 American epic science fiction film directed by Denis Villeneuve.","pageimage":"Dune_poster.jpg","original":{"source":"https://upload.wikimedia.org/wikipedia/en/8/8e/Dune_original_poster.jpg","width":2000,"height":3000}}}}}"#,
    );
    let (server, source) = fake.source();
    let q = movie_query("Dune", 2021);
    let media = source
        .find(&Cancel::background(), &q)
        .expect("Find")
        .expect("expected media, got nil");
    let want = "https://upload.wikimedia.org/wikipedia/en/8/8e/Dune_original_poster.jpg";
    assert_eq!(media.artwork, want, "artwork must come from the page image");
    assert_eq!(
        fake.hits(),
        2,
        "search+detail: a page image needs no fallback"
    );
    let detail_query = fake.detail_query();
    assert!(
        detail_query.contains("piprop=thumbnail|original")
            || detail_query.contains("piprop=thumbnail%7Coriginal"),
        "detail query = {detail_query:?}"
    );
    assert_eq!(
        fake.images_query(),
        "",
        "prop=images was called despite a page image"
    );

    let dir = TempDir::new();
    let resolver = resolver_with_wiki(&dir.path(), server.url());
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert_eq!(res.artwork, want, "the record artwork must reach the card");
}

/// `TestWikipediaImageListAsksForEveryFile`: `prop=images` asks for the whole
/// file list (`imlimit=max`); the live poster is the 18th of 24 files.
#[test]
fn wikipedia_image_list_asks_for_every_file() {
    let fake = WikiFake {
        search: WIKI_HIT_SEARCH.to_string(),
        detail: r#"{"query":{"pages":{"12345":{"pageid":12345,"title":"Spider-Man: Brand New Day","extract":"Spider-Man: Brand New Day is a 2026 American superhero film."}}}}"#.to_string(),
        images: r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[{"title":"File:Commons-logo.svg"},{"title":"File:Movie set of Spider-Man Brand New Day (2025) (2).jpg"},{"title":"File:Spider-Man Brand New Day poster.jpg"}]}}}}"#.to_string(),
        imageinfo: r#"{"query":{"pages":{"999":{"pageid":999,"title":"File:Spider-Man Brand New Day poster.jpg","imageinfo":[{"url":"https://upload.wikimedia.org/wikipedia/en/9/9a/Spider-Man_Brand_New_Day_poster.jpg"}]}}}}"#.to_string(),
        ..WikiFake::new()
    };
    let (_server, source) = fake.source();
    let media = source
        .find(
            &Cancel::background(),
            &movie_query("Spider-Man: Brand New Day", 0),
        )
        .expect("Find")
        .expect("expected media, got nil");
    assert!(
        fake.images_query().contains("imlimit=max"),
        "prop=images query = {:?}, want imlimit=max",
        fake.images_query()
    );
    assert_eq!(
        media.artwork,
        "https://upload.wikimedia.org/wikipedia/en/9/9a/Spider-Man_Brand_New_Day_poster.jpg"
    );
}

/// `TestWikiStripDisambiguation`: only a trailing parenthetical that IS a media
/// disambiguation (`film`, `2021 film`, `TV series`, case-insensitive) is
/// dropped; a real parenthesized title (`Director's Cut`) and a bare year
/// (`Heat (1995)`, which carries no kind word) survive, because the strip must
/// never eat a legitimate title.
#[test]
fn wiki_strip_disambiguation_kinds_only() {
    let cases: Vec<(&str, &str)> = vec![
        ("Ready Player One (film)", "Ready Player One"),
        ("Dune (2021 film)", "Dune"),
        ("Backrooms (film)", "Backrooms"),
        ("Dune (Film)", "Dune"),
        ("Show (TV series)", "Show"),
        ("Show (2021 TV series)", "Show"),
        ("Show (limited series)", "Show"),
        ("Game (video game)", "Game"),
        ("Song (song)", "Song"),
        ("Nymphomaniac (Director's Cut)", "Nymphomaniac (Director's Cut)"),
        ("Heat (1995)", "Heat (1995)"),
        ("No Parens", "No Parens"),
        ("(film)", "(film)"),
    ];
    for (input, want) in cases {
        assert_eq!(strip_disambiguation(input), want, "input = {input:?}");
    }
}

/// `TestWikipediaDisambiguationStripped`: the live `Ready Player One (film)`
/// shape yields a clean `Media.title` with the year still derived from the raw
/// page title (2018).
#[test]
fn wikipedia_disambiguation_stripped() {
    for (page_title, want_title, want_year, extract) in [
        (
            "Ready Player One (film)",
            "Ready Player One",
            2018,
            "Ready Player One is a 2018 American science fiction action film directed by Steven Spielberg.",
        ),
        (
            "Backrooms (film)",
            "Backrooms",
            2026,
            "Backrooms is a 2026 American psychological horror film directed by Kane Parsons.",
        ),
    ] {
        let detail = format!(
            r#"{{"query":{{"pages":{{"12345":{{"pageid":12345,"title":"{page_title}","extract":"{extract}"}}}}}}}}"#
        );
        let search =
            format!(r#"{{"query":{{"search":[{{"title":"{page_title}"}}]}}}}"#);
        let fake = WikiFake::with(&search, &detail);
        let (_server, source) = fake.source();
        let media = source
            .find(&Cancel::background(), &movie_query(want_title, 0))
            .expect("Find")
            .expect("expected media, got nil");
        assert_eq!(media.title, want_title, "page = {page_title:?}");
        assert_eq!(media.year, want_year, "page = {page_title:?}");
        assert_eq!(media.source_id, want_title, "page = {page_title:?}");
    }
}

/// `TestWikipediaNondisambiguationParenSurvives`: a parenthetical that is part
/// of the real title reaches `Media` untouched.
#[test]
fn wikipedia_nondisambiguation_paren_survives() {
    const PAGE: &str = "Nymphomaniac (Director's Cut)";
    let search = r#"{"query":{"search":[{"title":"Nymphomaniac (Director's Cut)"}]}}"#;
    let detail = r#"{"query":{"pages":{"12345":{"pageid":12345,"title":"Nymphomaniac (Director's Cut)","extract":"Nymphomaniac Director's Cut is a 2013 drama film in two volumes."}}}}"#;
    let fake = WikiFake::with(search, detail);
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Nymphomaniac", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, PAGE);
    assert_eq!(media.year, 2013);
}

/// `TestWikipediaExactTitlePoster`: the Ready Player One live shape -- no page
/// image, and an image list whose only poster file is named exactly like the
/// article (`File:Ready Player One (film).png`, no "poster" word) -- resolves
/// that file via `prop=imageinfo` instead of falling back to `logo`.
#[test]
fn wikipedia_exact_title_poster() {
    let fake = WikiFake {
        search: r#"{"query":{"search":[{"title":"Ready Player One (film)"}]}}"#.to_string(),
        detail: r#"{"query":{"pages":{"12345":{"pageid":12345,"title":"Ready Player One (film)","extract":"Ready Player One is a 2018 American science fiction action film directed by Steven Spielberg."}}}}"#.to_string(),
        images: r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[{"title":"File:Commons-logo.svg"},{"title":"File:Ready Player One (film).png"},{"title":"File:Steven Spielberg by Gage Skidmore.jpg"}]}}}}"#.to_string(),
        imageinfo: r#"{"query":{"pages":{"999":{"pageid":999,"title":"File:Ready Player One (film).png","imageinfo":[{"url":"https://upload.wikimedia.org/wikipedia/en/7/74/Ready_Player_One_%28film%29.png"}]}}}}"#.to_string(),
        ..WikiFake::new()
    };
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Ready Player One", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.title, "Ready Player One");
    assert_eq!(
        media.artwork,
        "https://upload.wikimedia.org/wikipedia/en/7/74/Ready_Player_One_%28film%29.png"
    );
    // A same-named non-photo (an svg diagram of the article subject) is still
    // rejected: the extension guard survives the fallback.
    let fake = WikiFake {
        search: r#"{"query":{"search":[{"title":"Dune (2021 film)"}]}}"#.to_string(),
        detail: WIKI_NO_POSTER_DETAIL.to_string(),
        images: r#"{"query":{"pages":{"12345":{"pageid":12345,"images":[{"title":"File:Dune (2021 film).svg"}]}}}}"#.to_string(),
        ..WikiFake::new()
    };
    let (_server, source) = fake.source();
    let media = source
        .find(&Cancel::background(), &movie_query("Dune", 0))
        .expect("Find")
        .expect("expected media, got nil");
    assert_eq!(media.artwork, "", "an svg same-named file is not a poster");
}
