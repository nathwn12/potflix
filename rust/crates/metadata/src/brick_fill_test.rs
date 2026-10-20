//! T7-2 brick-fill tests: identity derivation, pure extraction, priority,
//! the fill engine, and batch persistence.
//!
//! Zero network: struct-literal [`Media`] fixtures plus [`FakeSource`]
//! (which answers without touching the wire) and `TempDir` scratch paths.
//! No test reads or writes the real `%LOCALAPPDATA%\Potflix\brick-cache.json`.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;

use crate::brick::{
    Brick, BrickCache, BrickField, BrickLookup, BrickSource, BrickValue, EntityKey, EpisodeKey,
    capacity, entity_key_for_query, episode_key_for_query, extract_field_bricks, fill_entity,
    priority, with_media_id,
};
use crate::cache::cache_key;
use crate::resolver::Source;
use crate::testsupport::{FakeSource, TempDir};
use crate::timefmt::parse_rfc3339;

fn series_query() -> Query {
    Query {
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        year: 2008,
        season: 1,
        episode: 1,
        absolute: false,
    }
}

fn series_key() -> EpisodeKey {
    EpisodeKey::Episode {
        season: 1,
        number: 1,
    }
}

fn scratch_cache(scratch: &TempDir, name: &str) -> BrickCache {
    BrickCache::empty_at(PathBuf::from(scratch.child(name)))
}

fn tvmaze_media() -> Media {
    Media {
        source: "tvmaze".to_string(),
        source_id: "169".to_string(),
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        year: 2008,
        artwork: "https://example.com/bb-poster.jpg".to_string(),
        synopsis: "A teacher cooks.".to_string(),
        genres: "Drama \u{b7} Crime".to_string(),
        length: "45 min".to_string(),
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            synopsis: "Walter cooks.".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

fn anilist_media() -> Media {
    Media {
        source: "anilist".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        year: 2008,
        artwork: "https://example.com/bb-anilist.jpg".to_string(),
        synopsis: "A teacher cooks, animated.".to_string(),
        genres: "Drama \u{b7} Crime".to_string(),
        length: "45 min".to_string(),
        // AniList episode rows carry titles only: the synopsis stays empty,
        // exactly as the provider builds them.
        episodes: vec![Episode {
            season: 0,
            number: 1,
            title: "Pilot".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

fn movie_query() -> Query {
    Query {
        kind: Kind::MOVIE,
        title: "Dune".to_string(),
        year: 2021,
        ..Query::default()
    }
}

fn cinemeta_movie() -> Media {
    Media {
        source: "cinemeta".to_string(),
        source_id: "tt1160419".to_string(),
        kind: Kind::MOVIE,
        title: "Dune".to_string(),
        year: 2021,
        artwork: "https://example.com/dune-poster.jpg".to_string(),
        synopsis: "Desert power.".to_string(),
        genres: "Sci-Fi \u{b7} Adventure".to_string(),
        length: "2h 35m".to_string(),
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

fn found_value(
    cache: &BrickCache,
    entity: &EntityKey,
    episode: &EpisodeKey,
    field: BrickField,
) -> BrickValue {
    match cache.lookup(entity, episode, field) {
        BrickLookup::Found(got) => got.value.clone(),
        other => panic!("want Found for {field:?}, got {other:?}"),
    }
}

/// The target identity is show-level: `series|<title>|<year>` with no season
/// (every season shares one bucket); movies keep `movie|<title>|<year>`. The
/// prefix comes from the RESOLVED kind, not `q.kind`. The resolver's media
/// cache (`cache_key`, per show-season) is deliberately coarser-separated and
/// stays untouched.
#[test]
fn fill_identity_matches_cache_key_shape() {
    let series = series_query();
    let key = entity_key_for_query(&series, Kind::SERIES);
    assert_eq!(key.id, "series|breaking bad|2008");
    assert_eq!(key, EntityKey::new("series|breaking bad|2008"));
    // The brick bucket drops the season the media cache keeps.
    assert_eq!(cache_key(&series), "series|breaking bad|2008|1");
    assert_ne!(key.id, cache_key(&series));
    // An UNKNOWN query still buckets series-side once the caller resolves it.
    let mut unknown = series_query();
    unknown.kind = Kind::UNKNOWN;
    assert_eq!(
        entity_key_for_query(&unknown, Kind::SERIES).id,
        "series|breaking bad|2008"
    );

    let movie = movie_query();
    let key = entity_key_for_query(&movie, Kind::MOVIE);
    assert_eq!(key.id, cache_key(&movie));
    assert_eq!(key.id, "movie|dune|2021");
}

/// Provider IDs land in the right slot; slot-less providers absorb nothing;
/// an existing ID is never overwritten.
#[test]
fn fill_with_media_id_maps_known_slots() {
    let bare = EntityKey::new("series|breaking bad|2008");

    let cinemeta = Media {
        source: "cinemeta".to_string(),
        source_id: "tt0903747".to_string(),
        ..Media::default()
    };
    let enriched = with_media_id(&bare, &cinemeta);
    assert_eq!(enriched.imdb.as_deref(), Some("tt0903747"));
    assert!(enriched.tvmaze.is_none() && enriched.anilist.is_none());

    let tvmaze = Media {
        source: "tvmaze".to_string(),
        source_id: "169".to_string(),
        ..Media::default()
    };
    let enriched = with_media_id(&bare, &tvmaze);
    assert_eq!(enriched.tvmaze, Some(169));
    assert!(enriched.imdb.is_none() && enriched.anilist.is_none());

    let anilist = Media {
        source: "anilist".to_string(),
        source_id: "1".to_string(),
        ..Media::default()
    };
    assert_eq!(with_media_id(&bare, &anilist).anilist, Some(1));

    // Slot-less providers absorb nothing, however numeric their id looks.
    for source in [
        "tenrai",
        "kitsu",
        "episodate",
        "bangumi",
        "livechart",
        "ann",
        "wikipedia",
        "justwatch",
    ] {
        let media = Media {
            source: source.to_string(),
            source_id: "169".to_string(),
            ..Media::default()
        };
        assert_eq!(
            with_media_id(&bare, &media),
            bare,
            "provider {source} must absorb nothing"
        );
    }

    // Garbage ids teach nothing either.
    let bad_cinemeta = Media {
        source: "cinemeta".to_string(),
        source_id: "not-an-imdb-id".to_string(),
        ..Media::default()
    };
    assert_eq!(with_media_id(&bare, &bad_cinemeta), bare);
    let bad_tvmaze = Media {
        source: "tvmaze".to_string(),
        source_id: "tt0903747".to_string(),
        ..Media::default()
    };
    assert_eq!(with_media_id(&bare, &bad_tvmaze), bare);

    // Never overwrite: the held IDs win over the answer's.
    let held = EntityKey {
        id: bare.id.clone(),
        imdb: Some("tt0000001".to_string()),
        tvmaze: Some(1),
        anilist: Some(2),
        ..EntityKey::default()
    };
    assert_eq!(
        with_media_id(&held, &cinemeta).imdb.as_deref(),
        Some("tt0000001")
    );
    assert_eq!(with_media_id(&held, &tvmaze).tvmaze, Some(1));
    assert_eq!(with_media_id(&held, &anilist).anilist, Some(2));
}

/// Season/number vs absolute vs none: an absolute query must NOT become S00.
#[test]
fn fill_episode_key_from_query() {
    assert_eq!(episode_key_for_query(&series_query()), series_key());

    let mut absolute = series_query();
    absolute.absolute = true;
    absolute.season = 0;
    absolute.episode = 51;
    assert_eq!(
        episode_key_for_query(&absolute),
        EpisodeKey::Absolute { number: 51 }
    );
    assert_ne!(
        episode_key_for_query(&absolute),
        EpisodeKey::Episode {
            season: 0,
            number: 51
        },
        "absolute numbering must never become S00"
    );

    assert_eq!(episode_key_for_query(&movie_query()), EpisodeKey::None);

    let mut no_episode = series_query();
    no_episode.episode = 0;
    assert_eq!(episode_key_for_query(&no_episode), EpisodeKey::None);

    let mut no_season = series_query();
    no_season.season = 0;
    assert_eq!(
        episode_key_for_query(&no_season),
        EpisodeKey::None,
        "a season-0 non-absolute query addresses no episode"
    );
}

fn value_text(pairs: &[(BrickField, BrickValue)], field: BrickField) -> Option<String> {
    pairs
        .iter()
        .find(|(f, _)| *f == field)
        .and_then(|(_, v)| match v {
            BrickValue::Text { text } => Some(text.clone()),
            _ => None,
        })
}

fn has_field(pairs: &[(BrickField, BrickValue)], field: BrickField) -> bool {
    pairs.iter().any(|(f, _)| *f == field)
}

/// TVmaze extraction: episode title + synopsis for the exact episode, show
/// synopsis (never movie), genres split, length, year, artwork.
#[test]
fn extract_tvmaze_episode_title_and_synopsis() {
    let q = series_query();
    let pairs = extract_field_bricks(&tvmaze_media(), &q, Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Pilot")
    );
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeSynopsis).as_deref(),
        Some("Walter cooks.")
    );
    assert_eq!(
        value_text(&pairs, BrickField::ShowSynopsis).as_deref(),
        Some("A teacher cooks.")
    );
    assert!(
        !has_field(&pairs, BrickField::MovieSynopsis),
        "a series synopsis must never land in movie_synopsis"
    );
    assert_eq!(
        value_text(&pairs, BrickField::Artwork).as_deref(),
        Some("https://example.com/bb-poster.jpg")
    );
    match pairs.iter().find(|(f, _)| *f == BrickField::Genres) {
        Some((_, BrickValue::Genres { genres })) => {
            assert_eq!(genres, &["Drama".to_string(), "Crime".to_string()]);
        }
        other => panic!("want Genres payload, got {other:?}"),
    }
    assert_eq!(
        value_text(&pairs, BrickField::Length).as_deref(),
        Some("45 min")
    );
    match pairs.iter().find(|(f, _)| *f == BrickField::Year) {
        Some((_, BrickValue::Year { year })) => assert_eq!(*year, 2008),
        other => panic!("want Year payload, got {other:?}"),
    }
    assert!(
        !has_field(&pairs, BrickField::Title),
        "extraction never emits titles"
    );
}

/// AniList extraction: cover art plus the episode title, and NO episode
/// synopsis (its rows carry none). AniList numbers absolutely on season 0, so
/// an absolute query meets it; a season query the table does not carry yields
/// no episode facts at all.
#[test]
fn extract_anilist_cover_title_and_no_synopsis() {
    // AniList numbers absolutely on season 0: the absolute query meets it.
    let mut absolute = series_query();
    absolute.absolute = true;
    absolute.season = 0;
    absolute.episode = 1;
    let pairs = extract_field_bricks(&anilist_media(), &absolute, Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::Artwork).as_deref(),
        Some("https://example.com/bb-anilist.jpg")
    );
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Pilot")
    );
    assert!(
        !has_field(&pairs, BrickField::EpisodeSynopsis),
        "AniList rows carry no synopsis, so none may be fabricated"
    );
    // The same media under a season query the table does not carry yields no
    // episode facts at all.
    let q = series_query();
    let pairs = extract_field_bricks(&anilist_media(), &q, Kind::SERIES);
    assert!(!has_field(&pairs, BrickField::EpisodeTitle));
    assert!(!has_field(&pairs, BrickField::EpisodeSynopsis));
}

/// Cinemeta movie extraction: movie synopsis (never show), genres, length,
/// year.
#[test]
fn extract_cinemeta_movie_synopsis_genres_length_year() {
    let q = movie_query();
    let pairs = extract_field_bricks(&cinemeta_movie(), &q, Kind::MOVIE);
    assert_eq!(
        value_text(&pairs, BrickField::MovieSynopsis).as_deref(),
        Some("Desert power.")
    );
    assert!(
        !has_field(&pairs, BrickField::ShowSynopsis),
        "a movie synopsis must never land in show_synopsis"
    );
    match pairs.iter().find(|(f, _)| *f == BrickField::Genres) {
        Some((_, BrickValue::Genres { genres })) => {
            assert_eq!(genres, &["Sci-Fi".to_string(), "Adventure".to_string()]);
        }
        other => panic!("want Genres payload, got {other:?}"),
    }
    assert_eq!(
        value_text(&pairs, BrickField::Length).as_deref(),
        Some("2h 35m")
    );
    assert_eq!(
        value_text(&pairs, BrickField::Artwork).as_deref(),
        Some("https://example.com/dune-poster.jpg")
    );
    match pairs.iter().find(|(f, _)| *f == BrickField::Year) {
        Some((_, BrickValue::Year { year })) => assert_eq!(*year, 2021),
        other => panic!("want Year payload, got {other:?}"),
    }
}

/// Wikipedia movie extraction: the synopsis (plus the carried year), and
/// nothing else -- no genres, no length, no episode facts, no artwork when
/// the page carries none.
#[test]
fn extract_wikipedia_movie_synopsis_only() {
    let q = movie_query();
    let media = Media {
        source: "wikipedia".to_string(),
        source_id: "Dune".to_string(),
        kind: Kind::MOVIE,
        title: "Dune".to_string(),
        year: 2021,
        synopsis: "Desert power, encyclopedically.".to_string(),
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    let pairs = extract_field_bricks(&media, &q, Kind::MOVIE);
    assert_eq!(
        value_text(&pairs, BrickField::MovieSynopsis).as_deref(),
        Some("Desert power, encyclopedically.")
    );
    for field in [
        BrickField::ShowSynopsis,
        BrickField::Genres,
        BrickField::Length,
        BrickField::Artwork,
        BrickField::EpisodeTitle,
        BrickField::EpisodeSynopsis,
        BrickField::Title,
    ] {
        assert!(
            !has_field(&pairs, field),
            "wikipedia must not yield {field:?}"
        );
    }
}

/// JustWatch movie extraction: artwork, synopsis, length, year -- and no
/// genres (its machine tags are deliberately dropped upstream).
#[test]
fn extract_justwatch_movie_fields() {
    let q = movie_query();
    let media = Media {
        source: "justwatch".to_string(),
        source_id: "tm123".to_string(),
        kind: Kind::MOVIE,
        title: "Dune".to_string(),
        year: 2021,
        artwork: "https://example.com/dune-jw.jpg".to_string(),
        synopsis: "Desert power, streaming.".to_string(),
        length: "2h 35m".to_string(),
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    let pairs = extract_field_bricks(&media, &q, Kind::MOVIE);
    assert_eq!(
        value_text(&pairs, BrickField::MovieSynopsis).as_deref(),
        Some("Desert power, streaming.")
    );
    assert_eq!(
        value_text(&pairs, BrickField::Artwork).as_deref(),
        Some("https://example.com/dune-jw.jpg")
    );
    assert_eq!(
        value_text(&pairs, BrickField::Length).as_deref(),
        Some("2h 35m")
    );
    assert!(has_field(&pairs, BrickField::Year));
    assert!(
        !has_field(&pairs, BrickField::Genres),
        "justwatch carries no display genres"
    );
}

/// Tenrai/Kitsu/Episodate extraction: the series legs carry artwork, show
/// synopsis and episode facts; Tenrai and Kitsu carry length, Episodate
/// carries episode titles without synopses and no length.
#[test]
fn extract_tenrai_kitsu_episodate_fields() {
    let q = Query {
        kind: Kind::SERIES,
        title: "Cowboy Bebop".to_string(),
        year: 1998,
        season: 1,
        episode: 1,
        absolute: false,
    };
    let episode = Episode {
        season: 1,
        number: 1,
        title: "Asteroid Blues".to_string(),
        synopsis: "Bebop bounty.".to_string(),
        ..Episode::default()
    };
    let base = Media {
        kind: Kind::SERIES,
        title: "Cowboy Bebop".to_string(),
        year: 1998,
        artwork: "https://example.com/bebop.jpg".to_string(),
        synopsis: "Space bounty.".to_string(),
        genres: "Action \u{b7} Sci-Fi".to_string(),
        length: "24 min".to_string(),
        episodes: vec![episode.clone()],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };

    let mut tenrai = base.clone();
    tenrai.source = "tenrai".to_string();
    tenrai.source_id = "1".to_string();
    let pairs = extract_field_bricks(&tenrai, &q, Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Asteroid Blues")
    );
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeSynopsis).as_deref(),
        Some("Bebop bounty.")
    );
    assert_eq!(
        value_text(&pairs, BrickField::ShowSynopsis).as_deref(),
        Some("Space bounty.")
    );
    assert_eq!(
        value_text(&pairs, BrickField::Length).as_deref(),
        Some("24 min")
    );
    assert!(has_field(&pairs, BrickField::Genres));

    let mut kitsu = base.clone();
    kitsu.source = "kitsu".to_string();
    kitsu.source_id = "1".to_string();
    kitsu.genres = String::new();
    let pairs = extract_field_bricks(&kitsu, &q, Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Asteroid Blues")
    );
    assert_eq!(
        value_text(&pairs, BrickField::Length).as_deref(),
        Some("24 min")
    );
    assert!(
        !has_field(&pairs, BrickField::Genres),
        "kitsu carries no genres"
    );

    let mut episodate = base.clone();
    episodate.source = "episodate".to_string();
    episodate.source_id = "cowboy-bebop".to_string();
    episodate.genres = String::new();
    episodate.length = String::new();
    episodate.episodes = vec![Episode {
        synopsis: String::new(),
        ..episode
    }];
    let pairs = extract_field_bricks(&episodate, &q, Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Asteroid Blues")
    );
    assert!(
        !has_field(&pairs, BrickField::EpisodeSynopsis),
        "episodate rows carry no synopsis"
    );
    assert!(
        !has_field(&pairs, BrickField::Length),
        "episodate carries no length"
    );
}

/// Absolute vs season addressing meet the right rows: `#51` meets the
/// absolute table, `S01E01` meets the season table, and neither fabricates
/// the other's title. The lookup IS `resolver::match_episode` (parity by
/// construction): the flattened-table fallback and the season-0 specials
/// guard below pin that wiring with hard-coded expectations the old
/// number-exact lookup failed.
#[test]
fn extract_absolute_vs_season_episode_key() {
    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "169".to_string(),
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        year: 2008,
        episodes: vec![
            Episode {
                season: 0,
                number: 51,
                title: "Absolute Fifty-One".to_string(),
                ..Episode::default()
            },
            Episode {
                season: 1,
                number: 1,
                title: "Pilot".to_string(),
                ..Episode::default()
            },
        ],
        ..Media::default()
    };
    let mut absolute = series_query();
    absolute.absolute = true;
    absolute.season = 0;
    absolute.episode = 51;
    let pairs = extract_field_bricks(&media, &absolute, Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Absolute Fifty-One")
    );
    let pairs = extract_field_bricks(&media, &series_query(), Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Pilot")
    );
    let mut missing = series_query();
    missing.absolute = true;
    missing.season = 0;
    missing.episode = 7;
    let pairs = extract_field_bricks(&media, &missing, Kind::SERIES);
    assert!(
        !has_field(&pairs, BrickField::EpisodeTitle),
        "an uncarried absolute number yields no title"
    );
}

/// Parity with `resolver::match_episode`: a provider that numbers a later
/// season by its TOTAL number is still matchable (the season-aware absolute
/// fallback). The call below IS the resolver's function, so the fallback is
/// pinned with a hard-coded expectation the old number-exact lookup failed.
/// (The season-0 specials guard is resolver-inherited and covered by the
/// resolver's own tests; a guard case yields `None` under both the old and
/// the new lookup, so it cannot pin this wiring and is not repeated here.)
#[test]
fn extract_episode_matching_parity_with_resolver() {
    // Flattened table: S1 carries 1..2, S2's opener rides as number 3 with
    // season 0. S02E01 wants total 2 + 1 = 3 and meets it.
    let flattened = Media {
        source: "tvmaze".to_string(),
        source_id: "169".to_string(),
        kind: Kind::SERIES,
        title: "Show".to_string(),
        year: 2008,
        episodes: vec![
            Episode {
                season: 1,
                number: 1,
                title: "S1E1".to_string(),
                ..Episode::default()
            },
            Episode {
                season: 1,
                number: 2,
                title: "S1E2".to_string(),
                ..Episode::default()
            },
            Episode {
                season: 0,
                number: 3,
                title: "Total Three".to_string(),
                ..Episode::default()
            },
        ],
        ..Media::default()
    };
    let mut q = series_query();
    q.title = "Show".to_string();
    q.season = 2;
    q.episode = 1;
    let pairs = extract_field_bricks(&flattened, &q, Kind::SERIES);
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Total Three"),
        "the resolver fallback must be wired into extraction"
    );
}

/// The priority tables are the contract: policy order filtered through
/// capability (no entry names an incapable source), split by kind.
#[test]
fn priority_tables_match_contract() {
    use BrickSource::*;
    assert_eq!(
        priority(Kind::SERIES, BrickField::Artwork),
        &[Anilist, Tvmaze, Tenrai, Kitsu, Cinemeta, Episodate, Bangumi, Livechart, Ann]
    );
    assert_eq!(
        priority(Kind::MOVIE, BrickField::Artwork),
        &[Cinemeta, JustWatch, Wikipedia]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::EpisodeTitle),
        &[Tvmaze, Tenrai, Kitsu, Cinemeta, Episodate, Anilist]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::EpisodeSynopsis),
        &[Tvmaze, Tenrai, Kitsu, Cinemeta]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::ShowSynopsis),
        &[
            Tvmaze, Anilist, Tenrai, Kitsu, Cinemeta, Episodate, Bangumi, Livechart, Ann
        ]
    );
    assert_eq!(
        priority(Kind::MOVIE, BrickField::MovieSynopsis),
        &[Wikipedia, Cinemeta, JustWatch]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::Genres),
        &[Tvmaze, Anilist, Tenrai, Cinemeta]
    );
    assert_eq!(
        priority(Kind::MOVIE, BrickField::Genres),
        &[Cinemeta, JustWatch]
    );
    // Length series drops Cinemeta (no series runtime): capability filters the
    // policy entry.
    assert_eq!(
        priority(Kind::SERIES, BrickField::Length),
        &[Tvmaze, Anilist, Tenrai, Kitsu, Ann]
    );
    assert_eq!(
        priority(Kind::MOVIE, BrickField::Length),
        &[Cinemeta, JustWatch]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::Year),
        &[
            Cinemeta, Tvmaze, Anilist, Tenrai, Kitsu, Episodate, Bangumi, Livechart, Ann
        ]
    );
    assert_eq!(
        priority(Kind::MOVIE, BrickField::Year),
        &[Cinemeta, Wikipedia, JustWatch]
    );
    assert_eq!(priority(Kind::SERIES, BrickField::Title), &[]);
    assert_eq!(priority(Kind::MOVIE, BrickField::Title), &[]);
    // Wrong-kind synopsis/episode slots answer empty: a movie never walks
    // episode/show slots and a series never walks the movie slot.
    assert_eq!(priority(Kind::MOVIE, BrickField::EpisodeTitle), &[]);
    assert_eq!(priority(Kind::MOVIE, BrickField::EpisodeSynopsis), &[]);
    assert_eq!(priority(Kind::MOVIE, BrickField::ShowSynopsis), &[]);
    assert_eq!(priority(Kind::SERIES, BrickField::MovieSynopsis), &[]);
    // Unknown kinds fill like series (only movies take the movie lists).
    assert_eq!(
        priority(Kind::UNKNOWN, BrickField::Artwork),
        priority(Kind::SERIES, BrickField::Artwork)
    );
    assert_eq!(
        priority(Kind::UNKNOWN, BrickField::Genres),
        priority(Kind::SERIES, BrickField::Genres)
    );
}

/// No priority entry fails capability: every listed source can answer the
/// slot for the kind.
#[test]
fn priority_entries_are_all_capable() {
    let kinds = [Kind::SERIES, Kind::MOVIE, Kind::UNKNOWN];
    let fields = [
        BrickField::Artwork,
        BrickField::EpisodeTitle,
        BrickField::EpisodeSynopsis,
        BrickField::ShowSynopsis,
        BrickField::MovieSynopsis,
        BrickField::Genres,
        BrickField::Length,
        BrickField::Year,
        BrickField::Title,
    ];
    for kind in kinds {
        for field in fields {
            for source in priority(kind, field) {
                assert!(
                    capacity(*source, kind).contains(&field),
                    "priority({kind:?}, {field:?}) names incapable {source:?}"
                );
            }
        }
    }
}

/// Every applicable field has at least one capable provider for each kind:
/// movie slots, series episode slots, and series show-level slots.
#[test]
fn every_applicable_field_has_a_capable_provider() {
    use BrickSource::*;
    let all: &[BrickSource] = &[
        Anilist, Tvmaze, Tenrai, Kitsu, Cinemeta, Wikipedia, JustWatch, Episodate, Bangumi,
        Livechart, Ann,
    ];
    let capable = |kind: Kind, field: BrickField| -> Vec<BrickSource> {
        all.iter()
            .copied()
            .filter(|s| capacity(*s, kind).contains(&field))
            .collect()
    };
    for field in [
        BrickField::Artwork,
        BrickField::MovieSynopsis,
        BrickField::Genres,
        BrickField::Length,
        BrickField::Year,
    ] {
        assert!(
            !capable(Kind::MOVIE, field).is_empty(),
            "movie {field:?} has no capable provider"
        );
    }
    for field in [
        BrickField::Artwork,
        BrickField::EpisodeTitle,
        BrickField::EpisodeSynopsis,
        BrickField::ShowSynopsis,
        BrickField::Genres,
        BrickField::Length,
        BrickField::Year,
    ] {
        assert!(
            !capable(Kind::SERIES, field).is_empty(),
            "series {field:?} has no capable provider"
        );
    }
    // The frozen matrix, spot-checked: the exclusions are the contract.
    assert!(!capacity(BrickSource::Anilist, Kind::SERIES).contains(&BrickField::EpisodeSynopsis));
    assert!(!capacity(BrickSource::Kitsu, Kind::SERIES).contains(&BrickField::Genres));
    assert!(!capacity(BrickSource::Cinemeta, Kind::SERIES).contains(&BrickField::Length));
    assert!(capacity(BrickSource::Cinemeta, Kind::MOVIE).contains(&BrickField::Length));
    assert!(capacity(BrickSource::JustWatch, Kind::MOVIE).contains(&BrickField::Genres));
    assert!(capacity(BrickSource::Wikipedia, Kind::MOVIE).contains(&BrickField::Artwork));
    assert!(capacity(BrickSource::Wikipedia, Kind::SERIES).is_empty());
    assert!(capacity(BrickSource::JustWatch, Kind::SERIES).is_empty());
    assert!(capacity(BrickSource::Tvmaze, Kind::MOVIE).is_empty());
    assert!(capacity(BrickSource::Anilist, Kind::MOVIE).is_empty());
    assert!(capacity(BrickSource::Tmdb, Kind::SERIES).is_empty());
    assert!(capacity(BrickSource::Tmdb, Kind::MOVIE).is_empty());
    assert!(capacity(BrickSource::Manual, Kind::SERIES).is_empty());
    assert!(capacity(BrickSource::Manual, Kind::MOVIE).is_empty());
}

/// An incapable supplied source is never called and yields Absent, not Miss:
/// JustWatch gates series with `Ok(None)`, but capability skips it before any
/// call, so no 24 h Miss guards the slot.
#[test]
fn fill_incapable_source_is_never_called_and_stays_absent() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let jw_media = Media {
        source: "justwatch".to_string(),
        source_id: "tm1".to_string(),
        kind: Kind::MOVIE,
        title: "Breaking Bad".to_string(),
        year: 2008,
        artwork: "https://example.com/bb-jw.jpg".to_string(),
        synopsis: "Streaming synopsis.".to_string(),
        length: "45 min".to_string(),
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    let jw = FakeSource::with_media("justwatch", jw_media);
    let jw_calls = jw.counter();
    let sources = vec![jw.shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert_eq!(
        jw_calls.load(Ordering::SeqCst),
        0,
        "an incapable source must never be called"
    );
    assert!(
        !report.missed.contains(&BrickField::Artwork),
        "zero capable sources must never record a miss, missed={:?}",
        report.missed
    );
    assert!(
        report.not_supplied.contains(&BrickField::Artwork),
        "a matrix-covered but unsupplied field must report not_supplied, report={report:?}"
    );
    assert!(
        report.unsupported.is_empty(),
        "the matrix covers series artwork: nothing is unsupported, report={report:?}"
    );
    assert!(
        matches!(
            cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::Absent
        ),
        "zero capable supplied sources must leave the slot Absent"
    );
}

/// Every name from the production chain maps to a brick source, and unknown
/// names are never consulted: a foreign injected source files nothing.
#[test]
fn fill_default_chain_names_all_map_and_unknown_is_skipped() {
    for source in crate::resolver::default_sources() {
        assert!(
            BrickSource::for_provider(source.name()).is_some(),
            "default chain name {:?} maps to no brick source",
            source.name()
        );
    }
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let bogus = FakeSource::with_media(
        "bogus",
        Media {
            title: "Breaking Bad".to_string(),
            year: 2008,
            artwork: "https://example.com/bogus.jpg".to_string(),
            ..tvmaze_media()
        },
    );
    let bogus_calls = bogus.counter();
    let sources = vec![bogus.shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert_eq!(
        bogus_calls.load(Ordering::SeqCst),
        0,
        "an unknown source name must never be called"
    );
    assert!(
        report.found.is_empty() && report.missed.is_empty() && report.errored.is_empty(),
        "an unknown source must change nothing, report={report:?}"
    );
    assert_eq!(
        report.unmapped,
        vec!["bogus".to_string()],
        "the foreign name must be reported once, report={report:?}"
    );
    assert!(
        !report.not_supplied.is_empty(),
        "with no known source every attempted field is not_supplied"
    );
    assert!(
        report.unsupported.is_empty(),
        "the matrix covers every attempted field: nothing is unsupported"
    );
    assert_eq!(cache.brick_count(), 0);
}

/// A provider error is recorded even when a lower-priority provider later
/// wins the field: the outage stays visible instead of being dropped.
#[test]
fn fill_error_is_kept_when_a_later_provider_wins() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    // TVmaze sorts ahead of Kitsu in policy order: it errors while Kitsu wins.
    let mut kitsu_media = tvmaze_media();
    kitsu_media.source = "kitsu".to_string();
    kitsu_media.source_id = "1".to_string();
    let sources = vec![
        FakeSource::with_error("tvmaze", "boom").shared(),
        FakeSource::with_media("kitsu", kitsu_media).shared(),
    ];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(
        report.found.contains(&BrickField::Artwork),
        "kitsu must still win artwork, found={:?}",
        report.found
    );
    assert!(
        report
            .errored
            .iter()
            .any(|(f, message)| *f == BrickField::Artwork && message.contains("boom")),
        "the tvmaze error must survive the kitsu win, errored={:?}",
        report.errored
    );
}

/// Implausible years yield no Year brick: 0, 1899 and 2101 are out (the
/// `wikipedia::wiki_year` 1900..=2100 bound); the edges 1900 and 2100 are in.
#[test]
fn extract_implausible_years_yield_nothing() {
    let q = series_query();
    for year in [0, 1899, 2101] {
        let mut media = tvmaze_media();
        media.year = year;
        let pairs = extract_field_bricks(&media, &q, Kind::SERIES);
        assert!(
            !has_field(&pairs, BrickField::Year),
            "year {year} must yield no Year brick"
        );
    }
    for year in [1900, 2008, 2100] {
        let mut media = tvmaze_media();
        media.year = year;
        let pairs = extract_field_bricks(&media, &q, Kind::SERIES);
        match pairs.iter().find(|(f, _)| *f == BrickField::Year) {
            Some((_, BrickValue::Year { year: got })) => assert_eq!(*got, year),
            other => panic!("year {year} must yield a Year brick, got {other:?}"),
        }
    }
}

/// A policy-tail capable source is still consulted: TVmaze answers without
/// Length, so the walk reaches Ann (policy-last for Length series) and Ann
/// wins. The same tail path serves a future capable source the policy never
/// named (appended last, in supplied order).
#[test]
fn fill_capable_source_omitted_from_policy_is_consulted_last() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    // TVmaze answers with no Length; Ann answers Length. Policy order for
    // Length series is TVmaze first, Ann last: Ann must still be consulted
    // and win.
    let mut bare_tvmaze = tvmaze_media();
    bare_tvmaze.length = String::new();
    let mut ann_media = tvmaze_media();
    ann_media.source = "ann".to_string();
    ann_media.source_id = "1".to_string();
    ann_media.length = "24 min".to_string();
    let tvmaze = FakeSource::with_media("tvmaze", bare_tvmaze);
    let ann = FakeSource::with_media("ann", ann_media);
    let ann_calls = ann.counter();
    let sources = vec![tvmaze.shared(), ann.shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert_eq!(ann_calls.load(Ordering::SeqCst), 1, "policy-last Ann must be consulted");
    match cache.lookup(&report.key, &EpisodeKey::None, BrickField::Length) {
        BrickLookup::Found(got) => {
            assert_eq!(got.source, BrickSource::Ann);
            assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "24 min".to_string(),
                }
            );
        }
        other => panic!("Ann must win Length after TVmaze lacks it, got {other:?}"),
    }
}

/// Miss requires a consulted capable source: a definitive `Ok(None)` from a
/// capable provider records a Miss, while zero capable sources leave Absent.
#[test]
fn fill_miss_requires_a_consulted_capable_source() {
    // Definitive None from a capable source: Miss.
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let fake = FakeSource::new("tvmaze");
    let sources = vec![fake.shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(report.missed.contains(&BrickField::Artwork));
    assert!(matches!(
        cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork),
        BrickLookup::MissWaiting(_)
    ));

    // Zero capable sources (only the movie-only JustWatch supplied for a
    // series query): Absent, never Miss.
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let jw = FakeSource::new("justwatch");
    let sources = vec![jw.shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(
        !report.missed.contains(&BrickField::Artwork),
        "no capable source must record no miss"
    );
    assert!(
        report.not_supplied.contains(&BrickField::Artwork),
        "a matrix-covered but unsupplied field must report not_supplied, report={report:?}"
    );
    assert!(matches!(
        cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork),
        BrickLookup::Absent
    ));
    assert_eq!(cache.brick_count(), 0, "an Absent-only pass stores nothing");
}

/// S01E01 and S02E03 of one show share one entity bucket, carry distinct
/// episode keys, and reuse show-level Found bricks across episodes. The fill
/// derives both keys from the query: callers pass no episode key, so no
/// caller can file an `S00` slot or address another season's bucket.
#[test]
fn fill_show_level_bricks_are_shared_across_seasons() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let mut first = series_query();
    first.season = 1;
    first.episode = 1;
    let mut second = series_query();
    second.season = 2;
    second.episode = 3;
    let first_episode = EpisodeKey::Episode {
        season: 1,
        number: 1,
    };
    let second_episode = EpisodeKey::Episode {
        season: 2,
        number: 3,
    };
    assert_ne!(first_episode, second_episode);
    assert_eq!(
        entity_key_for_query(&first, Kind::SERIES),
        entity_key_for_query(&second, Kind::SERIES)
    );
    assert_eq!(
        entity_key_for_query(&first, Kind::SERIES).id,
        "series|breaking bad|2008"
    );

    // The second episode's title rides along so both episode slots can fill.
    let mut media = tvmaze_media();
    media.episodes.push(Episode {
        season: 2,
        number: 3,
        title: "Bit by a Dead Bee".to_string(),
        synopsis: "Walt meets Saul.".to_string(),
        ..Episode::default()
    });
    let sources = vec![FakeSource::with_media("tvmaze", media).shared()];
    let refs = fill_sources(&sources);
    let first_report =
        fill_entity(&mut cache, &first, Kind::SERIES, &refs, &Cancel::background())
            .expect("fill S01E01");
    assert_eq!(cache.entity_count(), 1);
    // The season-2 query lands in the season-2 episode key of the shared
    // bucket: the derived key is {2,3}, never S00, never S01.
    let second_report =
        fill_entity(&mut cache, &second, Kind::SERIES, &refs, &Cancel::background())
            .expect("fill S02E03");
    assert_eq!(
        cache.entity_count(),
        1,
        "both seasons must share one entity bucket"
    );
    assert_eq!(first_report.key, second_report.key);
    // Show-level bricks filed by S01E01 serve S02E03: the second pass finds
    // no new show-level fact.
    assert!(!second_report.found.contains(&BrickField::ShowSynopsis));
    assert!(!second_report.found.contains(&BrickField::Artwork));
    match cache.lookup(&second_report.key, &EpisodeKey::None, BrickField::ShowSynopsis) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "A teacher cooks.".to_string(),
            }
        ),
        other => panic!("show synopsis must be shared across seasons, got {other:?}"),
    }
    // Episode slots stay distinct.
    match cache.lookup(&second_report.key, &second_episode, BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "Bit by a Dead Bee".to_string(),
            }
        ),
        other => panic!("S02E03 title must be its own, got {other:?}"),
    }
    match cache.lookup(&second_report.key, &first_episode, BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "Pilot".to_string(),
            }
        ),
        other => panic!("S01E01 title must survive, got {other:?}"),
    }
}

/// No query can file an `S00` slot: a season-0 query addresses no episode, so
/// the fill attempts no episode fields and files nothing under any
/// season-0 key.
#[test]
fn fill_never_files_an_s00_slot() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let mut q = series_query();
    q.season = 0;
    q.episode = 5;
    assert_eq!(
        episode_key_for_query(&q),
        EpisodeKey::None,
        "a season-0 non-absolute query addresses no episode"
    );
    let sources = vec![FakeSource::with_media("tvmaze", tvmaze_media()).shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    for field in [BrickField::EpisodeTitle, BrickField::EpisodeSynopsis] {
        assert!(
            !report.found.contains(&field)
                && !report.missed.contains(&field)
                && !report.errored.iter().any(|(f, _)| *f == field)
                && !report.unsupported.contains(&field),
            "a season-0 query must not attempt {field:?}"
        );
        assert!(
            matches!(
                cache.lookup(
                    &report.key,
                    &EpisodeKey::Episode {
                        season: 0,
                        number: 5
                    },
                    field
                ),
                BrickLookup::Absent
            ),
            "no S00 slot may ever be filed for {field:?}"
        );
    }
    // Entity-level facts still fill from the same pass.
    assert!(report.found.contains(&BrickField::ShowSynopsis));
}

fn fill_sources(sources: &[std::sync::Arc<dyn Source>]) -> Vec<&dyn Source> {
    sources.iter().map(|s| s.as_ref()).collect()
}

/// AniList wins artwork over TVmaze: both answer, the first priority source
/// takes the slot.
#[test]
fn fill_anilist_wins_artwork_over_tvmaze() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let sources = vec![
        FakeSource::with_media("tvmaze", tvmaze_media()).shared(),
        FakeSource::with_media("anilist", anilist_media()).shared(),
    ];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(report.found.contains(&BrickField::Artwork));
    assert_eq!(
        found_value(&cache, &report.key, &EpisodeKey::None, BrickField::Artwork),
        BrickValue::Text {
            text: "https://example.com/bb-anilist.jpg".to_string(),
        }
    );
    match cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(got.source, BrickSource::Anilist),
        other => panic!("want Found artwork, got {other:?}"),
    }
}

/// TVmaze wins the episode title over AniList (whose absolute tables are
/// mislabeled downstream): AniList's S00 row must not take the S01E01 slot.
#[test]
fn fill_tvmaze_wins_episode_title_over_anilist() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let sources = vec![
        FakeSource::with_media("anilist", anilist_media()).shared(),
        FakeSource::with_media("tvmaze", tvmaze_media()).shared(),
    ];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(report.found.contains(&BrickField::EpisodeTitle));
    match cache.lookup(&report.key, &series_key(), BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => {
            assert_eq!(got.source, BrickSource::Tvmaze);
            assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "Pilot".to_string(),
                }
            );
        }
        other => panic!("want Found episode title, got {other:?}"),
    }
}

/// Wikipedia wins the movie synopsis over Cinemeta.
#[test]
fn fill_wikipedia_wins_movie_synopsis_over_cinemeta() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = movie_query();
    let wikipedia = Media {
        source: "wikipedia".to_string(),
        source_id: "Dune".to_string(),
        kind: Kind::MOVIE,
        title: "Dune".to_string(),
        year: 2021,
        synopsis: "Desert power, encyclopedically.".to_string(),
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    let sources = vec![
        FakeSource::with_media("cinemeta", cinemeta_movie()).shared(),
        FakeSource::with_media("wikipedia", wikipedia).shared(),
    ];
    let refs = fill_sources(&sources);
    let report = fill_entity(
        &mut cache,
        &q,
        Kind::MOVIE,
        &refs,
        &Cancel::background(),
    )
    .expect("fill");
    assert!(report.found.contains(&BrickField::MovieSynopsis));
    match cache.lookup(&report.key, &EpisodeKey::None, BrickField::MovieSynopsis) {
        BrickLookup::Found(got) => {
            assert_eq!(got.source, BrickSource::Wikipedia);
            assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "Desert power, encyclopedically.".to_string(),
                }
            );
        }
        other => panic!("want Found movie synopsis, got {other:?}"),
    }
}

/// Fallthrough: the first priority source answers but lacks the field, so
/// the next source with it wins -- per field, independently.
#[test]
fn fill_falls_through_when_first_lacks_field() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    // TVmaze answers with no artwork (but a real episode title); AniList
    // answers with artwork but its S00 row cannot take the S01E01 title.
    let mut bare_tvmaze = tvmaze_media();
    bare_tvmaze.artwork = String::new();
    let sources = vec![
        FakeSource::with_media("tvmaze", bare_tvmaze).shared(),
        FakeSource::with_media("anilist", anilist_media()).shared(),
    ];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    match cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(got.source, BrickSource::Anilist),
        other => panic!("artwork must fall through to anilist, got {other:?}"),
    }
    match cache.lookup(&report.key, &series_key(), BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => assert_eq!(got.source, BrickSource::Tvmaze),
        other => panic!("episode title stays with tvmaze, got {other:?}"),
    }
}

/// A full answer persists every applicable field, and the second pass makes
/// ZERO source calls: one provider is called exactly once per pass.
#[test]
fn fill_found_persists_and_second_pass_is_quiet() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let fake = FakeSource::with_media("tvmaze", tvmaze_media());
    let calls = fake.counter();
    let sources = vec![fake.shared()];
    let refs = fill_sources(&sources);

    let report = fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background())
        .expect("first fill");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one provider, one call");
    for field in [
        BrickField::Artwork,
        BrickField::EpisodeTitle,
        BrickField::EpisodeSynopsis,
        BrickField::ShowSynopsis,
        BrickField::Genres,
        BrickField::Length,
        BrickField::Year,
    ] {
        assert!(
            report.found.contains(&field),
            "first pass must find {field:?}, found={:?}",
            report.found
        );
    }
    assert!(report.missed.is_empty());
    assert!(report.errored.is_empty());
    assert!(report.unsupported.is_empty());
    // The win is keyed to the target identity, with the provider id absorbed.
    assert_eq!(report.key.id, "series|breaking bad|2008");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scratch.child("brick-cache.json")).expect("read"))
            .expect("decode");
    assert_eq!(raw["entities"][0]["entity"]["tvmaze"], 169);

    let second =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("refill");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "second pass must fetch nothing"
    );
    assert!(second.found.is_empty());
    assert!(second.missed.is_empty());
    assert!(second.errored.is_empty());
}

/// Two full providers: the first in priority order takes every shared slot,
/// and the loser is never even asked for the slots it lost... beyond what
/// the winner lacked. Here TVmaze covers everything, so AniList stays at
/// zero calls for the whole pass.
#[test]
fn fill_winner_takes_all_and_loser_rests() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    // AniList's absolute row cannot serve the season title/synopsis, so
    // TVmaze wins those outright; give TVmaze the full media so it wins the
    // rest too.
    let tvmaze = FakeSource::with_media("tvmaze", tvmaze_media());
    let anilist = FakeSource::with_media("anilist", anilist_media());
    let tvmaze_calls = tvmaze.counter();
    let anilist_calls = anilist.counter();
    let sources = vec![tvmaze.shared(), anilist.shared()];
    let refs = fill_sources(&sources);
    fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert_eq!(tvmaze_calls.load(Ordering::SeqCst), 1);
    // AniList is consulted only for artwork (its priority lead): one call
    // total for the whole pass, never one per field.
    assert_eq!(anilist_calls.load(Ordering::SeqCst), 1);
}

/// A definitive miss on every provider stores a bounded Miss: quiet now,
/// due at the stored clock -- and the quiet pass fetches nothing.
#[test]
fn fill_miss_is_bounded_and_retried_at_stored_clock() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let fake = FakeSource::new("tvmaze");
    let calls = fake.counter();
    let sources = vec![fake.shared()];
    let refs = fill_sources(&sources);

    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(report.found.is_empty());
    assert!(report.errored.is_empty());
    assert!(
        report.missed.contains(&BrickField::Artwork),
        "a definitive miss must record the field"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Bounded by the STORED clock, never the wall clock: one second before
    // the stored bound the miss waits, at it the miss is due.
    let stored_after = match cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::MissWaiting(got) => parse_rfc3339(
            got.retry_after
                .as_deref()
                .expect("a stored miss carries retry_after"),
        )
        .expect("stored retry_after parses"),
        other => panic!("fresh miss must wait, got {other:?}"),
    };
    assert!(
        matches!(
            cache.lookup_at(
                &report.key,
                &EpisodeKey::None,
                BrickField::Artwork,
                stored_after - Duration::from_secs(1),
            ),
            BrickLookup::MissWaiting(_)
        ),
        "one second before the stored bound the miss must still wait"
    );
    assert!(
        matches!(
            cache.lookup_at(
                &report.key,
                &EpisodeKey::None,
                BrickField::Artwork,
                stored_after,
            ),
            BrickLookup::RetryDue
        ),
        "at the stored bound the miss must be due"
    );

    // The quiet pass fetches nothing and reports nothing.
    let quiet =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("refill");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a waiting miss must not refetch"
    );
    assert!(quiet.found.is_empty() && quiet.missed.is_empty() && quiet.errored.is_empty());
}

/// A transient provider failure leaves the slot `Absent` -- never a Miss --
/// and names the field in the report.
#[test]
fn fill_transient_error_leaves_absent_not_miss() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let sources = vec![FakeSource::with_error("tvmaze", "boom").shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(report.found.is_empty());
    assert!(
        report.missed.is_empty(),
        "an error must never become a miss"
    );
    assert!(
        report
            .errored
            .iter()
            .any(|(f, _)| *f == BrickField::Artwork),
        "the failed field must report, errored={:?}",
        report.errored
    );
    assert!(
        matches!(
            cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::Absent
        ),
        "a transient failure persists nothing"
    );
    assert_eq!(cache.brick_count(), 0);
}

/// The provider's `find` is the match authority for titles: an
/// alternate/romaji title the provider returned is accepted -- the fill
/// engine never compares titles. A KNOWN year mismatch is the one exception:
/// the fill vetoes it as a different-work signal (stricter than the
/// resolver's soft policy, because a Found brick never expires). Alone, the
/// vetoed field stays `Absent` (never `Miss`, never a wrong Found); with a
/// correct provider down the chain, the correct answer wins.
#[test]
fn fill_accepts_aliases_but_vetoes_known_year_mismatch() {
    // Alternate title, same year: accepted.
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let alias = Media {
        title: "Breaking Bad: Pilot Special".to_string(),
        year: 2008,
        ..tvmaze_media()
    };
    let sources = vec![FakeSource::with_media("tvmaze", alias).shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(
        report.found.contains(&BrickField::Artwork),
        "a provider-matched alias must file artwork"
    );
    match cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(got.source, BrickSource::Tvmaze),
        other => panic!("want Found artwork, got {other:?}"),
    }

    // Differing known year, no other provider: skipped, Absent -- not Miss,
    // never a wrong Found.
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let remake = Media {
        title: "Breaking Bad".to_string(),
        year: 1999,
        ..tvmaze_media()
    };
    let sources = vec![FakeSource::with_media("tvmaze", remake).shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(
        report.found.is_empty(),
        "a year-mismatched answer must file nothing, found={:?}",
        report.found
    );
    assert!(
        !report.missed.contains(&BrickField::Artwork),
        "a veto teaches nothing: no miss, missed={:?}",
        report.missed
    );
    assert!(
        matches!(
            cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::Absent
        ),
        "a vetoed-only field stays Absent"
    );
    assert_eq!(cache.brick_count(), 0);

    // Differing known year with a correct provider down the chain: the
    // correct answer wins.
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let remake = Media {
        title: "Breaking Bad".to_string(),
        year: 1999,
        artwork: "https://example.com/bb-remake.jpg".to_string(),
        ..tvmaze_media()
    };
    let mut correct = tvmaze_media();
    correct.source = "tenrai".to_string();
    correct.source_id = "1".to_string();
    let sources = vec![
        FakeSource::with_media("tvmaze", remake).shared(),
        FakeSource::with_media("tenrai", correct).shared(),
    ];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    match cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => {
            assert_eq!(got.source, BrickSource::Tenrai);
            assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "https://example.com/bb-poster.jpg".to_string(),
                }
            );
        }
        other => panic!("the correct provider must win after the veto, got {other:?}"),
    }
}

/// A same-title answer of the opposite kind never files: series fills skip
/// movie-kind media and movie fills skip series-kind media, so artwork,
/// genres, length, year, and the synopsis slot stay wrong-kind-free.
#[test]
fn fill_skips_opposite_kind_answers() {
    // Series fill, movie-kind answer: nothing files, every attempted slot
    // misses (the capable provider was consulted and answered definitively).
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let mut movie_media = cinemeta_movie();
    movie_media.title = "Breaking Bad".to_string();
    movie_media.year = 2008;
    let sources = vec![FakeSource::with_media("cinemeta", movie_media).shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(
        report.found.is_empty(),
        "a movie-kind answer must file nothing into a series bucket, found={:?}",
        report.found
    );
    assert!(report.missed.contains(&BrickField::Artwork));
    assert!(report.missed.contains(&BrickField::ShowSynopsis));
    assert!(
        !report.found.contains(&BrickField::MovieSynopsis),
        "a series fill must never file a movie synopsis"
    );

    // Movie fill, series-kind answer from a movie-capable source: likewise
    // nothing files (the capable provider was consulted and skipped by kind).
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = movie_query();
    let series_media = Media {
        source: "cinemeta".to_string(),
        source_id: "tt1160419".to_string(),
        kind: Kind::SERIES,
        title: "Dune".to_string(),
        year: 2021,
        artwork: "https://example.com/dune-poster.jpg".to_string(),
        synopsis: "Desert power.".to_string(),
        genres: "Sci-Fi \u{b7} Adventure".to_string(),
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    let sources = vec![FakeSource::with_media("cinemeta", series_media).shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::MOVIE, &refs, &Cancel::background()).expect("fill");
    assert!(
        report.found.is_empty(),
        "a series-kind answer must file nothing into a movie bucket, found={:?}",
        report.found
    );
    assert!(report.missed.contains(&BrickField::Artwork));
    assert!(report.missed.contains(&BrickField::MovieSynopsis));
    assert!(
        !report.found.contains(&BrickField::ShowSynopsis),
        "a movie fill must never file a show synopsis"
    );
}

/// An unresolved kind is rejected: the caller resolves SERIES or MOVIE before
/// calling.
#[test]
fn fill_rejects_unresolved_kind() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let sources = vec![FakeSource::with_media("tvmaze", tvmaze_media()).shared()];
    let refs = fill_sources(&sources);
    let err = fill_entity(&mut cache, &q, Kind::UNKNOWN, &refs, &Cancel::background())
        .expect_err("UNKNOWN kind must fail");
    assert!(
        err.message().contains("unresolved kind"),
        "unexpected error: {err}"
    );
    assert_eq!(cache.brick_count(), 0, "a rejected fill stores nothing");
}

/// An answer whose provider ID contradicts the bucket's stored identity is
/// skipped as another entity's record: the stored brick survives, and the
/// vetoed field stays `Absent` (a veto teaches nothing, so no `Miss`).
#[test]
fn fill_skips_id_conflicting_answers_and_keeps_stored_bricks() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    // Seed the bucket: every field Found, tvmaze=169 absorbed into the
    // stored identity. Then clear Year so exactly one slot is open.
    let seed = vec![FakeSource::with_media("tvmaze", tvmaze_media()).shared()];
    let refs = fill_sources(&seed);
    fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("seed fill");
    assert!(
        cache
            .clear_field(&EntityKey::new("series|breaking bad|2008"), &EpisodeKey::None, BrickField::Year)
            .expect("clear year")
    );
    let stored = cache
        .stored_entity(&EntityKey::new("series|breaking bad|2008"))
        .expect("bucket holds an identity");
    assert_eq!(stored.tvmaze, Some(169));

    // Same title, year and kind -- but another show's tvmaze id.
    let mut impostor = tvmaze_media();
    impostor.source_id = "999".to_string();
    let sources = vec![FakeSource::with_media("tvmaze", impostor).shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(
        report.found.is_empty() && report.missed.is_empty() && report.errored.is_empty(),
        "a conflicted answer must change nothing, report={report:?}"
    );
    assert!(
        matches!(
            cache.lookup(
                &report.key,
                &EpisodeKey::None,
                BrickField::Year
            ),
            BrickLookup::Absent
        ),
        "the vetoed Year must stay Absent, never Miss, never Found"
    );
    match cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => {
            assert_eq!(got.source, BrickSource::Tvmaze);
            assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "https://example.com/bb-poster.jpg".to_string(),
                }
            );
        }
        other => panic!("the stored artwork must survive the conflict, got {other:?}"),
    }
}

/// The resolved kind reaches `provider.find`: a `q.kind == UNKNOWN` query
/// filled as `MOVIE` shows providers `MOVIE`, so a series-first provider
/// never answers with a series record the engine would only reject.
#[test]
fn fill_passes_resolved_kind_to_providers() {
    use std::sync::Mutex;

    struct KindRecordingSource {
        name: String,
        seen: std::sync::Arc<Mutex<Vec<Kind>>>,
        media: Option<Media>,
    }

    impl Source for KindRecordingSource {
        fn name(&self) -> &str {
            &self.name
        }

        fn find(&self, _cancel: &Cancel, q: &Query) -> Result<Option<Media>, crate::Error> {
            self.seen.lock().unwrap().push(q.kind);
            Ok(self.media.clone())
        }
    }

    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let mut q = movie_query();
    q.kind = Kind::UNKNOWN;
    let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
    let rec = KindRecordingSource {
        name: "cinemeta".to_string(),
        seen: std::sync::Arc::clone(&seen),
        media: Some(cinemeta_movie()),
    };
    let rec: std::sync::Arc<dyn Source> = std::sync::Arc::new(rec);
    let refs = vec![rec.as_ref()];
    let report =
        fill_entity(&mut cache, &q, Kind::MOVIE, &refs, &Cancel::background()).expect("fill");
    let seen = seen.lock().unwrap();
    assert!(
        !seen.is_empty(),
        "the provider must have been called"
    );
    assert!(
        seen.iter().all(|kind| *kind == Kind::MOVIE),
        "every find call must see the resolved MOVIE kind, saw={seen:?}"
    );
    assert!(
        report.found.contains(&BrickField::MovieSynopsis),
        "the movie fill must file, found={:?}",
        report.found
    );
}

/// Extraction chooses the synopsis slot from the resolved kind, never from
/// the answer or the query: resolved `SERIES` with `q.kind == MOVIE` yields
/// `ShowSynopsis`, not `MovieSynopsis`.
#[test]
fn extract_uses_resolved_kind_for_synopsis_slot() {
    let mut q = series_query();
    q.kind = Kind::MOVIE;
    let pairs = extract_field_bricks(&tvmaze_media(), &q, Kind::SERIES);
    assert!(
        has_field(&pairs, BrickField::ShowSynopsis),
        "resolved SERIES must yield a show synopsis"
    );
    assert!(
        !has_field(&pairs, BrickField::MovieSynopsis),
        "resolved SERIES must never yield a movie synopsis"
    );
    let pairs = extract_field_bricks(&cinemeta_movie(), &movie_query(), Kind::MOVIE);
    assert!(has_field(&pairs, BrickField::MovieSynopsis));
    assert!(!has_field(&pairs, BrickField::ShowSynopsis));
}

/// The three-way diagnostic split: `not_supplied` (matrix covers, chain
/// lacks), `unmapped` (foreign supplied names), `unsupported` (nothing in
/// the matrix could answer). A movie query supplied only `bogus` + `tvmaze`
/// (mapped but movie-incapable) reports every applicable field
/// `not_supplied`, the foreign name once, and nothing `unsupported`.
#[test]
fn fill_reports_not_supplied_unmapped_and_unsupported() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = movie_query();
    let sources = vec![
        FakeSource::new("bogus").shared(),
        FakeSource::new("tvmaze").shared(),
    ];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::MOVIE, &refs, &Cancel::background()).expect("fill");
    for field in [
        BrickField::Artwork,
        BrickField::MovieSynopsis,
        BrickField::Genres,
        BrickField::Length,
        BrickField::Year,
    ] {
        assert!(
            report.not_supplied.contains(&field),
            "movie {field:?} is matrix-covered but unsupplied, report={report:?}"
        );
    }
    assert_eq!(
        report.unmapped,
        vec!["bogus".to_string()],
        "report={report:?}"
    );
    assert!(
        report.unsupported.is_empty(),
        "the matrix covers every applicable movie field, report={report:?}"
    );
    assert!(
        report.found.is_empty() && report.missed.is_empty() && report.errored.is_empty(),
        "report={report:?}"
    );
    assert_eq!(cache.brick_count(), 0);
}

/// An answer whose table lacks the requested episode files entity facts but
/// no episode facts: the episode slots miss, the entity slots stand.
#[test]
fn fill_rejects_episode_key_mismatch() {
    let scratch = TempDir::new();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let q = series_query();
    let mut media = tvmaze_media();
    media.episodes = vec![Episode {
        season: 1,
        number: 2,
        title: "Cat's in the Bag".to_string(),
        ..Episode::default()
    }];
    let sources = vec![FakeSource::with_media("tvmaze", media).shared()];
    let refs = fill_sources(&sources);
    let report =
        fill_entity(&mut cache, &q, Kind::SERIES, &refs, &Cancel::background()).expect("fill");
    assert!(report.found.contains(&BrickField::Artwork));
    assert!(report.found.contains(&BrickField::ShowSynopsis));
    assert!(report.missed.contains(&BrickField::EpisodeTitle));
    assert!(report.missed.contains(&BrickField::EpisodeSynopsis));
    assert!(
        matches!(
            cache.lookup(&report.key, &series_key(), BrickField::EpisodeTitle),
            BrickLookup::MissWaiting(_)
        ),
        "the unmatched episode slot must miss, never take S01E02's title"
    );
}

/// The provider-name map is exhaustive over the eleven live providers, and
/// foreign names (including the reserved `tmdb` and the hand `manual`)
/// answer `None`.
#[test]
fn fill_provider_name_map_is_exhaustive() {
    use BrickSource::*;
    for (name, want) in [
        ("anilist", Anilist),
        ("tvmaze", Tvmaze),
        ("tenrai", Tenrai),
        ("kitsu", Kitsu),
        ("cinemeta", Cinemeta),
        ("wikipedia", Wikipedia),
        ("justwatch", JustWatch),
        ("episodate", Episodate),
        ("bangumi", Bangumi),
        ("livechart", Livechart),
        ("ann", Ann),
    ] {
        assert_eq!(BrickSource::for_provider(name), Some(want), "name {name}");
        assert_eq!(want.provider_name(), name, "round-trip {name}");
    }
    for foreign in ["tmdb", "manual", "bogus", "", "TVMAZE"] {
        assert_eq!(
            BrickSource::for_provider(foreign),
            None,
            "{foreign:?} must map to no source"
        );
    }
    // Every source variant (plus the reserved/manual slots) serialises to a
    // distinct wire name: the merge logs these verbatim.
    let all = [
        Anilist,
        Tvmaze,
        BrickSource::Tmdb,
        Wikipedia,
        Cinemeta,
        BrickSource::Manual,
        Tenrai,
        Kitsu,
        JustWatch,
        Episodate,
        Bangumi,
        Livechart,
        Ann,
    ];
    let mut names = std::collections::HashSet::new();
    for source in all {
        let name = serde_json::to_value(source).expect("encode");
        assert!(name.is_string(), "source must encode as a string");
        assert!(names.insert(name), "duplicate source wire name");
    }
}

/// A failed batch save rolls every in-memory change back and leaves the
/// prior file byte-identical with no temp litter; the repaired batch then
/// stores with one save, and a bad brick fails the batch with nothing
/// applied.
#[test]
fn fill_insert_many_is_all_or_nothing() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = EntityKey {
        id: "series|breaking bad|2008".to_string(),
        tvmaze: Some(169),
        ..EntityKey::default()
    };
    let mut cache = BrickCache::empty_at(path.clone());
    let seed = Brick::found(
        BrickField::Artwork,
        BrickValue::Text {
            text: "https://example.com/poster.jpg".to_string(),
        },
        BrickSource::Tvmaze,
        entity.clone(),
        EpisodeKey::None,
        "tvmaze show 169 poster",
    );
    cache.insert(&entity, seed).expect("seed insert");
    assert_eq!(cache.brick_count(), 1);

    let batch = vec![
        Brick::found(
            BrickField::ShowSynopsis,
            BrickValue::Text {
                text: "A teacher cooks.".to_string(),
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "tvmaze show 169 synopsis",
        ),
        Brick::miss(
            BrickField::Length,
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "tvmaze show 169 has no runtime yet",
        ),
    ];

    // Block the save with a directory at the cache path (every platform).
    let before = std::fs::read(&path).expect("read seeded file");
    std::fs::remove_file(&path).expect("remove seeded file");
    std::fs::create_dir(&path).expect("dir at cache path");
    let failed = cache.insert_many(batch.clone());
    assert!(
        failed.is_err(),
        "a batch over a blocked file must fail, else this proves nothing"
    );
    assert_eq!(
        cache.brick_count(),
        1,
        "a failed batch must roll memory back"
    );
    match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(_) => {}
        other => panic!("rolled-back artwork must still resolve, got {other:?}"),
    }
    assert!(
        matches!(
            cache.lookup(&entity, &EpisodeKey::None, BrickField::ShowSynopsis),
            BrickLookup::Absent
        ),
        "the rolled-back batch must have stored nothing"
    );
    let leftovers: Vec<String> = std::fs::read_dir(scratch.path())
        .expect("read scratch dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );

    // Repair and store: one save for the whole batch.
    std::fs::remove_dir(&path).expect("remove blocking dir");
    std::fs::write(&path, &before).expect("restore seeded file");
    assert_eq!(cache.insert_many(batch).expect("repaired batch"), 2);
    assert_eq!(cache.brick_count(), 3);
    let reloaded = crate::brick::load_from(&path).expect("load_from");
    assert_eq!(reloaded.brick_count(), 3);

    // A bad brick fails the batch with nothing applied -- and no write. The
    // VALID brick comes first on purpose: a validate-as-you-mutate
    // implementation would already have stored it before failing on the
    // second, so the counts below prove the rollback, not just the refusal.
    let bad = vec![
        Brick::found(
            BrickField::Title,
            BrickValue::Text {
                text: "Breaking Bad".to_string(),
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "title",
        ),
        Brick::found(
            BrickField::Year,
            BrickValue::Text {
                text: "2008".to_string(),
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
    ];
    let bytes_before = std::fs::read(&path).expect("read file");
    let failed = cache.insert_many(bad);
    assert!(
        failed.is_err(),
        "a shape-mismatched batch must fail, else this proves nothing"
    );
    assert_eq!(cache.brick_count(), 3, "a refused batch applies nothing");
    assert_eq!(
        std::fs::read(&path).expect("read file after refusal"),
        bytes_before,
        "a refused batch must not rewrite the file"
    );

    // An empty batch stores nothing and writes nothing.
    assert_eq!(cache.insert_many(Vec::new()).expect("empty batch"), 0);
    assert_eq!(
        std::fs::read(&path).expect("read file after empty batch"),
        bytes_before,
        "an empty batch must not rewrite the file"
    );
}

/// An all-refused batch saves nothing and absorbs nothing: a Miss over a
/// Found slot carrying NEW external IDs is `Ok(0)` even while saving is
/// impossible (unlike `insert`, which can `Err` having changed nothing,
/// `insert_many` never fails when there is nothing to persist) -- and the
/// refused IDs never land in the stored identity.
#[test]
fn fill_insert_many_all_refused_needs_no_save() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let bare = EntityKey::new("series|breaking bad|2008");
    let informed = EntityKey {
        id: "series|breaking bad|2008".to_string(),
        tvmaze: Some(169),
        imdb: Some("tt0903747".to_string()),
        ..EntityKey::default()
    };
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(
            &bare,
            Brick::found(
                BrickField::Artwork,
                BrickValue::Text {
                    text: "https://example.com/poster.jpg".to_string(),
                },
                BrickSource::Tvmaze,
                bare.clone(),
                EpisodeKey::None,
                "tvmaze show 169 poster",
            ),
        )
        .expect("seed found");
    let before = std::fs::read(&path).expect("read seeded file");

    // Block the save: a refused batch must not even attempt it.
    std::fs::remove_file(&path).expect("remove seeded file");
    std::fs::create_dir(&path).expect("dir at cache path");
    let refused = vec![Brick::miss(
        BrickField::Artwork,
        BrickSource::Tvmaze,
        informed.clone(),
        EpisodeKey::None,
        "tvmaze show 169 still has no other poster",
    )];
    assert_eq!(
        cache
            .insert_many(refused)
            .expect("an all-refused batch is Ok even while saving is impossible"),
        0
    );
    match cache.lookup(&bare, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "https://example.com/poster.jpg".to_string(),
            },
            "a miss evicted a Found brick"
        ),
        other => panic!("Found must survive a refused batch, got {other:?}"),
    }
    let leftovers: Vec<String> = std::fs::read_dir(scratch.path())
        .expect("read scratch dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );

    // Repair: the file is byte-identical, the refused IDs never landed --
    // persisting the memory and reloading still shows the bare identity.
    std::fs::remove_dir(&path).expect("remove blocking dir");
    std::fs::write(&path, &before).expect("restore seeded file");
    cache.save().expect("save after repair");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    let stored = &raw["entities"][0]["entity"];
    assert_eq!(stored["id"], "series|breaking bad|2008");
    assert!(
        stored.get("tvmaze").is_none(),
        "refused IDs leaked tvmaze: {stored}"
    );
    assert!(
        stored.get("imdb").is_none(),
        "refused IDs leaked imdb: {stored}"
    );
    let reloaded = crate::brick::load_from(&path).expect("load_from");
    assert_eq!(reloaded.brick_count(), 1);
}

/// A failed batch rolls back absorbed IDs and every bucket it touched: seed
/// a BARE entity, batch a brick carrying new external IDs plus a brick for a
/// second bucket, break the save, and assert the IDs are still absent and
/// both buckets read as before.
#[test]
fn fill_insert_many_rollback_restores_ids_and_both_buckets() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let bare = EntityKey::new("series|breaking bad|2008");
    let other = EntityKey::new("series|better call saul|2015");
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(
            &bare,
            Brick::found(
                BrickField::Artwork,
                BrickValue::Text {
                    text: "https://example.com/poster.jpg".to_string(),
                },
                BrickSource::Tvmaze,
                bare.clone(),
                EpisodeKey::None,
                "tvmaze poster",
            ),
        )
        .expect("seed bare artwork");
    cache
        .insert(
            &other,
            Brick::found(
                BrickField::Artwork,
                BrickValue::Text {
                    text: "https://example.com/saul.jpg".to_string(),
                },
                BrickSource::Tvmaze,
                other.clone(),
                EpisodeKey::None,
                "tvmaze saul poster",
            ),
        )
        .expect("seed second bucket");
    assert_eq!(cache.entity_count(), 2);

    // The batch teaches the first bucket new IDs and files a new slot in the
    // second bucket: both must roll back together.
    let informed = EntityKey {
        id: "series|breaking bad|2008".to_string(),
        tvmaze: Some(169),
        imdb: Some("tt0903747".to_string()),
        ..EntityKey::default()
    };
    let batch = vec![
        Brick::found(
            BrickField::ShowSynopsis,
            BrickValue::Text {
                text: "A teacher cooks.".to_string(),
            },
            BrickSource::Tvmaze,
            informed.clone(),
            EpisodeKey::None,
            "tvmaze show 169 synopsis",
        ),
        Brick::found(
            BrickField::ShowSynopsis,
            BrickValue::Text {
                text: "A lawyer cooks.".to_string(),
            },
            BrickSource::Tvmaze,
            other.clone(),
            EpisodeKey::None,
            "tvmaze saul synopsis",
        ),
    ];
    let before = std::fs::read(&path).expect("read seeded file");
    std::fs::remove_file(&path).expect("remove seeded file");
    std::fs::create_dir(&path).expect("dir at cache path");
    assert!(
        cache.insert_many(batch).is_err(),
        "a batch over a blocked file must fail, else this proves nothing"
    );
    // Both buckets read as before the batch.
    assert_eq!(cache.entity_count(), 2);
    assert_eq!(cache.brick_count(), 2);
    assert!(
        matches!(
            cache.lookup(&bare, &EpisodeKey::None, BrickField::ShowSynopsis),
            BrickLookup::Absent
        ),
        "the rolled-back synopsis must be absent"
    );
    assert!(
        matches!(
            cache.lookup(&other, &EpisodeKey::None, BrickField::ShowSynopsis),
            BrickLookup::Absent
        ),
        "the rolled-back second bucket must be absent too"
    );

    // Repair, persist the rolled-back memory, reload: the absorbed IDs never
    // landed, in either bucket.
    std::fs::remove_dir(&path).expect("remove blocking dir");
    std::fs::write(&path, &before).expect("restore seeded file");
    cache.save().expect("save after repair");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    let entities = raw["entities"].as_array().expect("entities array");
    assert_eq!(entities.len(), 2);
    for stored in entities {
        assert!(
            stored["entity"].get("tvmaze").is_none(),
            "rolled-back absorb leaked tvmaze: {stored}"
        );
        assert!(
            stored["entity"].get("imdb").is_none(),
            "rolled-back absorb leaked imdb: {stored}"
        );
    }
    let reloaded = crate::brick::load_from(&path).expect("load_from");
    assert_eq!(reloaded.entity_count(), 2);
    assert_eq!(reloaded.brick_count(), 2);
}
