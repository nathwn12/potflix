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
    entity_key_for_query, episode_key_for_query, extract_field_bricks, fill_entity, priority,
    with_media_id,
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

/// The target identity is the cache-key shape with no external IDs.
#[test]
fn fill_identity_matches_cache_key_shape() {
    let series = series_query();
    let key = entity_key_for_query(&series);
    assert_eq!(key.id, cache_key(&series));
    assert_eq!(key.id, "series|breaking bad|2008|1");
    assert_eq!(key, EntityKey::new("series|breaking bad|2008|1"));

    let movie = movie_query();
    let key = entity_key_for_query(&movie);
    assert_eq!(key.id, cache_key(&movie));
    assert_eq!(key.id, "movie|dune|2021");
}

/// Provider IDs land in the right slot; slot-less providers absorb nothing;
/// an existing ID is never overwritten.
#[test]
fn fill_with_media_id_maps_known_slots() {
    let bare = EntityKey::new("series|breaking bad|2008|1");

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
    let pairs = extract_field_bricks(BrickSource::Tvmaze, &tvmaze_media(), &q, series_key());
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
/// synopsis (its rows carry none).
#[test]
fn extract_anilist_cover_title_and_no_synopsis() {
    let q = series_query();
    // AniList numbers absolutely on season 0: the absolute key meets it.
    let key = EpisodeKey::Absolute { number: 1 };
    let pairs = extract_field_bricks(BrickSource::Anilist, &anilist_media(), &q, key);
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
    // The same media under a season key the table does not carry yields no
    // episode facts at all.
    let pairs = extract_field_bricks(BrickSource::Anilist, &anilist_media(), &q, series_key());
    assert!(!has_field(&pairs, BrickField::EpisodeTitle));
    assert!(!has_field(&pairs, BrickField::EpisodeSynopsis));
}

/// Cinemeta movie extraction: movie synopsis (never show), genres, length,
/// year.
#[test]
fn extract_cinemeta_movie_synopsis_genres_length_year() {
    let q = movie_query();
    let pairs = extract_field_bricks(
        BrickSource::Cinemeta,
        &cinemeta_movie(),
        &q,
        EpisodeKey::None,
    );
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
    let pairs = extract_field_bricks(BrickSource::Wikipedia, &media, &q, EpisodeKey::None);
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
    let pairs = extract_field_bricks(BrickSource::JustWatch, &media, &q, EpisodeKey::None);
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
    let key = EpisodeKey::Episode {
        season: 1,
        number: 1,
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
    let pairs = extract_field_bricks(BrickSource::Tenrai, &tenrai, &q, key);
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
    let pairs = extract_field_bricks(BrickSource::Kitsu, &kitsu, &q, key);
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
    let pairs = extract_field_bricks(BrickSource::Episodate, &episodate, &q, key);
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
/// the other's title.
#[test]
fn extract_absolute_vs_season_episode_key() {
    let q = series_query();
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
    let pairs = extract_field_bricks(
        BrickSource::Tvmaze,
        &media,
        &q,
        EpisodeKey::Absolute { number: 51 },
    );
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Absolute Fifty-One")
    );
    let pairs = extract_field_bricks(BrickSource::Tvmaze, &media, &q, series_key());
    assert_eq!(
        value_text(&pairs, BrickField::EpisodeTitle).as_deref(),
        Some("Pilot")
    );
    let pairs = extract_field_bricks(
        BrickSource::Tvmaze,
        &media,
        &q,
        EpisodeKey::Absolute { number: 7 },
    );
    assert!(
        !has_field(&pairs, BrickField::EpisodeTitle),
        "an uncarried absolute number yields no title"
    );
}

/// The priority tables are the contract: exact order per kind and field.
#[test]
fn priority_tables_match_contract() {
    use BrickSource::*;
    assert_eq!(
        priority(Kind::SERIES, BrickField::Artwork),
        &[
            Anilist, Tvmaze, Tenrai, Kitsu, Cinemeta, JustWatch, Episodate, Bangumi, Livechart, Ann
        ]
    );
    assert_eq!(
        priority(Kind::MOVIE, BrickField::Artwork),
        &[Cinemeta, JustWatch, Wikipedia, Anilist]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::EpisodeTitle),
        &[Tvmaze, Tenrai, Kitsu, Cinemeta, Episodate, Anilist]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::EpisodeSynopsis),
        &[Tvmaze, Tenrai, Kitsu, Cinemeta, Anilist, Episodate]
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
        priority(Kind::SERIES, BrickField::Length),
        &[Tvmaze, Anilist, Tenrai, Kitsu, Cinemeta, JustWatch, Ann]
    );
    assert_eq!(
        priority(Kind::SERIES, BrickField::Year),
        &[
            Cinemeta, Tvmaze, Anilist, Tenrai, Kitsu, Wikipedia, JustWatch, Episodate, Bangumi,
            Livechart, Ann
        ]
    );
    assert_eq!(priority(Kind::SERIES, BrickField::Title), &[]);
    assert_eq!(priority(Kind::MOVIE, BrickField::Title), &[]);
    // Unknown kinds fill like series (only movies take the movie lists).
    assert_eq!(
        priority(Kind::UNKNOWN, BrickField::Artwork),
        priority(Kind::SERIES, BrickField::Artwork)
    );
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
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
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
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
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
        EpisodeKey::None,
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
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
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

    let report = fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background())
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
    // The win is keyed to the target identity, with the provider id absorbed.
    assert_eq!(report.key.id, "series|breaking bad|2008|1");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scratch.child("brick-cache.json")).expect("read"))
            .expect("decode");
    assert_eq!(raw["entities"][0]["entity"]["tvmaze"], 169);

    let second =
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("refill");
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
    fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
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
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
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
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("refill");
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
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
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

/// Cross-entity pollution is rejected: a wrong-title answer and a
/// same-title wrong-year answer file nothing and read as misses.
#[test]
fn fill_rejects_wrong_title_and_wrong_year() {
    for (label, media) in [
        (
            "wrong title",
            Media {
                title: "Better Call Saul".to_string(),
                year: 2008,
                ..tvmaze_media()
            },
        ),
        (
            "wrong year",
            Media {
                title: "Breaking Bad".to_string(),
                year: 1999,
                ..tvmaze_media()
            },
        ),
    ] {
        let scratch = TempDir::new();
        let mut cache = scratch_cache(&scratch, "brick-cache.json");
        let q = series_query();
        let sources = vec![FakeSource::with_media("tvmaze", media).shared()];
        let refs = fill_sources(&sources);
        let report =
            fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
        assert!(
            !report.found.contains(&BrickField::Artwork),
            "{label}: a rejected answer must file no artwork"
        );
        assert!(
            report.missed.contains(&BrickField::Artwork),
            "{label}: a definitive rejection reads as a miss"
        );
        assert!(
            matches!(
                cache.lookup(&report.key, &EpisodeKey::None, BrickField::Artwork),
                BrickLookup::MissWaiting(_)
            ),
            "{label}: the slot must hold a miss, never the wrong work's art"
        );
    }
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
        fill_entity(&mut cache, &q, series_key(), &refs, &Cancel::background()).expect("fill");
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
        id: "series|breaking bad|2008|1".to_string(),
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

    // A bad brick fails the batch with nothing applied -- and no write.
    let bad = vec![
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
