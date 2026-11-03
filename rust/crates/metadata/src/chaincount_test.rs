//! Ports `go\internal\metadata\chaincount_test.go`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use metadata_core::context::resolve_query;
use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;

use crate::resolver::{Options, Resolver, Source};
use crate::testsupport::{failing_server, http_client, wiki_client, TempDir};
use crate::Error;
use crate::{anilist::AnilistSource, cinemeta::CinemetaSource};
use crate::{ann::AnnSource, bangumi::BangumiSource, episodate::EpisodateSource};
use crate::{justwatch::JustWatchSource, livechart::LivechartSource};
use crate::{kitsu::KitsuSource, tenrai::TenraiSource};
use crate::{tvmaze::TvmazeSource, wikipedia::WikipediaSource};

/// The answering callback shared by every fake provider in this module.
type FindFn = Arc<dyn Fn(&str, &Query) -> Option<Media> + Send + Sync>;

/// `countingSource`: a fake [`Source`] that records every `find` call by source
/// name so the resolver chain cost can be asserted per query kind.
struct CountingSource {
    name: String,
    counts: Arc<Mutex<HashMap<String, i32>>>,
    find: FindFn,
}

impl Source for CountingSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn find(&self, _cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        let name = self.name.clone();
        *self.counts.lock().unwrap().entry(name.clone()).or_insert(0) += 1;
        Ok((self.find)(&name, q))
    }
}

/// `countingResolver`: a Resolver whose providers are fakes named exactly like
/// the defaults, so `orderedSources` ranks them as in production.
fn counting_resolver(
    dir: &str,
    counts: &Arc<Mutex<HashMap<String, i32>>>,
    find: impl Fn(&str, &Query) -> Option<Media> + Send + Sync + 'static,
) -> Resolver {
    let find: FindFn = Arc::new(find);
    let make = |name: &str| {
        Arc::new(CountingSource {
            name: name.to_string(),
            counts: Arc::clone(counts),
            find: Arc::clone(&find),
        }) as Arc<dyn Source>
    };
    Resolver::new(Options {
        dir: dir.to_string(),
        timeout: Duration::from_secs(8),
        sources: Some(vec![
            make("tvmaze"),
            make("anilist"),
            make("tenrai"),
            make("kitsu"),
            make("cinemeta"),
            make("wikipedia"),
            make("justwatch"),
            make("episodate"),
            make("bangumi"),
            make("livechart"),
            make("ann"),
        ]),
        ..Options::default()
    })
}

/// `lookupCounts`: clears the tally, forces one lookup, and returns the counts.
fn lookup_counts(
    resolver: &Resolver,
    q: &Query,
    counts: &Arc<Mutex<HashMap<String, i32>>>,
) -> HashMap<String, i32> {
    counts.lock().unwrap().clear();
    resolver
        .lookup_now_force(&Cancel::background(), q, true)
        .expect("LookupNowForce");
    counts.lock().unwrap().clone()
}

/// `seriesAnswer`: an answering series media for the query's own episode. It
/// carries a synopsis (episode and show) so the hit is complete and settles
/// the chain at the answering provider; the walk-on for a bare match is
/// covered by the completeness test in `resolver_test`.
fn series_answer(q: &Query) -> Media {
    Media {
        title: q.title.clone(),
        synopsis: "A complete record with a synopsis.".to_string(),
        episodes: vec![Episode {
            season: i32::try_from(q.season).expect("test query season fits i32"),
            number: q.episode,
            title: "Pilot".to_string(),
            synopsis: "The pilot episode.".to_string(),
            ..Episode::default()
        }],
        ..Media::default()
    }
}

fn count(counts: &HashMap<String, i32>, name: &str) -> i32 {
    counts.get(name).copied().unwrap_or(0)
}

/// `TestChainCallCounts`: how many provider `Find` calls one lookup costs per
/// query kind. These are Find invocations, not network calls: each source
/// self-gates a ruled-out kind before touching the wire, so a series miss still
/// INVOKES cinemeta.Find but a movie miss still INVOKES the series-only fakes.
#[test]
fn chain_call_counts() {
    // NOTE: the fixtures below carry no artwork, so every non-AniList winner
    // also costs exactly one `anilist` backfill call (`fetch_media` fires it
    // for unusable artwork); an AniList winner and a miss cost none.
    let tv_q = resolve_query("Breaking Bad S01E01.mkv", "");
    assert_eq!(tv_q.kind, Kind::SERIES, "tv fixture kind");
    let anime_q = resolve_query("[Anime Time] Re Zero - 01.mkv", "");
    assert!(
        anime_q.kind == Kind::SERIES && anime_q.absolute,
        "anime fixture must be KindSeries absolute"
    );
    let movie_q = Query {
        kind: Kind::MOVIE,
        title: "Inception".to_string(),
        ..Query::default()
    };
    let miss_q = resolve_query("Xyzzy Qqq S01E01.mkv", "");
    assert_eq!(miss_q.kind, Kind::SERIES, "miss fixture kind");
    let bare_q = resolve_query("Sample.mkv", "");
    assert_eq!(bare_q.kind, Kind::UNKNOWN, "bare fixture kind");

    struct Case {
        name: &'static str,
        q: Query,
        answer_at: &'static str,
        want: Vec<(&'static str, i32)>,
    }
    let cases = vec![
        Case {
            name: "tv hit at first provider",
            q: tv_q.clone(),
            answer_at: "tvmaze",
            want: vec![("tvmaze", 1), ("anilist", 1)],
        },
        Case {
            name: "movie hit at cinemeta",
            q: movie_q.clone(),
            answer_at: "cinemeta",
            want: vec![("cinemeta", 1), ("anilist", 1)],
        },
        Case {
            name: "movie hit at justwatch walks the movie legs then the unranked tail",
            q: movie_q.clone(),
            answer_at: "justwatch",
            want: vec![
                ("cinemeta", 1),
                // Walked once on the way past, then once for the artwork
                // backfill: the winning fixture carries no cover.
                ("anilist", 2),
                ("tenrai", 1),
                ("kitsu", 1),
                ("wikipedia", 1),
                ("justwatch", 1),
            ],
        },
        Case {
            name: "movie definitive miss walks cinemeta, wikipedia, justwatch then the unranked tail",
            q: movie_q.clone(),
            answer_at: "",
            want: vec![
                ("cinemeta", 1),
                ("wikipedia", 1),
                ("justwatch", 1),
                ("tvmaze", 1),
                ("anilist", 1),
                ("tenrai", 1),
                ("kitsu", 1),
                ("episodate", 1),
                ("bangumi", 1),
                ("livechart", 1),
                ("ann", 1),
            ],
        },
        Case {
            name: "anime hit at anilist",
            q: anime_q.clone(),
            answer_at: "anilist",
            want: vec![("tvmaze", 1), ("anilist", 1)],
        },
        Case {
            name: "anime hit at tenrai walks past tvmaze and anilist",
            q: anime_q.clone(),
            answer_at: "tenrai",
            want: vec![("tvmaze", 1), ("anilist", 2), ("tenrai", 1)],
        },
        Case {
            name: "anime hit at kitsu walks past tvmaze, anilist and tenrai",
            q: anime_q.clone(),
            answer_at: "kitsu",
            want: vec![("tvmaze", 1), ("anilist", 2), ("tenrai", 1), ("kitsu", 1)],
        },
        Case {
            name: "series hit at episodate walks past the first five legs",
            q: anime_q.clone(),
            answer_at: "episodate",
            want: vec![
                ("tvmaze", 1),
                ("anilist", 2),
                ("tenrai", 1),
                ("kitsu", 1),
                ("cinemeta", 1),
                ("episodate", 1),
            ],
        },
        Case {
            name: "series hit at ann walks the whole series chain",
            q: anime_q.clone(),
            answer_at: "ann",
            want: vec![
                ("tvmaze", 1),
                ("anilist", 2),
                ("tenrai", 1),
                ("kitsu", 1),
                ("cinemeta", 1),
                ("episodate", 1),
                ("bangumi", 1),
                ("livechart", 1),
                ("ann", 1),
            ],
        },
        Case {
            name: "tv definitive miss walks the series chain",
            q: miss_q.clone(),
            answer_at: "",
            want: vec![
                ("tvmaze", 1),
                ("anilist", 1),
                ("tenrai", 1),
                ("kitsu", 1),
                ("cinemeta", 1),
                ("episodate", 1),
                ("bangumi", 1),
                ("livechart", 1),
                ("ann", 1),
            ],
        },
        Case {
            name: "unknown definitive miss keeps full chain",
            q: bare_q.clone(),
            answer_at: "",
            want: vec![
                ("tvmaze", 1),
                ("anilist", 1),
                ("tenrai", 1),
                ("kitsu", 1),
                ("cinemeta", 1),
                ("episodate", 1),
                ("bangumi", 1),
                ("livechart", 1),
                ("ann", 1),
            ],
        },
    ];

    for case in cases {
        let dir = TempDir::new();
        let counts: Arc<Mutex<HashMap<String, i32>>> = Arc::new(Mutex::new(HashMap::new()));
        let answer_at = case.answer_at;
        let resolver = counting_resolver(&dir.path(), &counts, move |name, q| {
            if name != answer_at {
                return None;
            }
            if q.kind == Kind::MOVIE {
                return Some(Media {
                    title: q.title.clone(),
                    synopsis: "A complete record with a synopsis.".to_string(),
                    ..Media::default()
                });
            }
            Some(series_answer(q))
        });
        let got = lookup_counts(&resolver, &case.q, &counts);
        for (name, expected) in &case.want {
            assert_eq!(
                count(&got, name),
                *expected,
                "{}: source {name} count (full table {got:?})",
                case.name
            );
        }
    }
}

/// `TestKindGateNoNetwork`: each source self-gates a ruled-out kind BEFORE
/// touching the wire -- against a dead server a ruled-out query returns
/// `(nil, nil)`, while an allowed query fails with a network error (the positive
/// control that proves the dead server would have been hit).
#[test]
fn kind_gate_no_network() {
    // The positive control answers immediately on loopback and records each
    // hit: an allowed query must surface a wire error AND leave a recorded
    // hit, while a ruled-out query must leave the counter untouched. A dead
    // port cost ~1 s per allowed query (11.1 s for the 11 sources); the mock
    // keeps the same proof without the timeout.
    let hits: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let mock = failing_server(&hits);
    let mock_url = mock.url();
    let series_q = Query {
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let movie_q = Query {
        kind: Kind::MOVIE,
        title: "Inception".to_string(),
        ..Query::default()
    };
    // Cinemeta serves both kinds, so its pre-wire gate is the empty title.
    let empty_q = Query {
        kind: Kind::MOVIE,
        ..Query::default()
    };

    let cases: Vec<(&str, Box<dyn Source>, &Query, &Query)> = vec![
        (
            "cinemeta",
            Box::new(CinemetaSource::new(mock_url.clone(), http_client())),
            &empty_q,
            &movie_q,
        ),
        (
            "wikipedia",
            Box::new(WikipediaSource::new(mock_url.clone(), wiki_client())),
            &series_q,
            &movie_q,
        ),
        (
            "tvmaze",
            Box::new(TvmazeSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
        (
            "anilist",
            Box::new(AnilistSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
        (
            "tenrai",
            Box::new(TenraiSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
        (
            "kitsu",
            Box::new(KitsuSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
        (
            "justwatch",
            Box::new(JustWatchSource::new(mock_url.clone(), http_client())),
            &series_q,
            &movie_q,
        ),
        (
            "episodate",
            Box::new(EpisodateSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
        (
            "bangumi",
            Box::new(BangumiSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
        (
            "livechart",
            Box::new(LivechartSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
        (
            "ann",
            Box::new(AnnSource::new(mock_url.clone(), http_client())),
            &movie_q,
            &series_q,
        ),
    ];
    for (name, source, ruled_out, allowed) in cases {
        let cancel = Cancel::with_deadline(Duration::from_secs(10));
        let before = hits.load(Ordering::SeqCst);
        match source.find(&cancel, ruled_out) {
            Ok(None) => {}
            other => panic!("{name} ruled-out query: got {other:?}, want Ok(None)"),
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            before,
            "{name}: ruled-out query must not touch the wire"
        );
        match source.find(&cancel, allowed) {
            Err(err) => {
                assert!(
                    !err.message().is_empty(),
                    "{name}: the allowed query must surface a wire error"
                );
                assert!(
                    err.message().contains("500"),
                    "{name}: the allowed query must surface the recording 500 mock, got {err:?}"
                );
            }
            Ok(other) => panic!("{name} allowed query: got {other:?}, want a network error"),
        }
        assert!(
            hits.load(Ordering::SeqCst) > before,
            "{name}: the allowed query must reach the wire"
        );
    }
}
