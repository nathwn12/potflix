
use std::sync::atomic::Ordering;
use std::sync::Arc;

use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;

use crate::cache::{cache_key, cache_load_entry};
use crate::testsupport::{resolver_with, FakeSource, TempDir};

const WINNER_TITLE: &str = "Dragon Ball Super: Beerus";
const STABLE_COVER: &str =
    "https://s4.anilist.co/file/anilistcdn/media/anime/cover/large/bx206814-AbCdEfGh.jpg";

/// A 396-char signed Backblaze-style URL, mirroring live Kitsu id 50497
/// (`posterImage.original` with `Expires=900`).
fn signed_artwork() -> String {
    let mut url = String::from(
        "https://f000.backblazeb2.com/file/kitsu-prod/50497.jpg?Expires=900&X-Amz-Signature=",
    );
    while url.len() < 396 {
        url.push('a');
    }
    assert_eq!(
        url.len(),
        396,
        "backfill fixture must mirror the 396-char live URL"
    );
    url
}

fn beerus_query() -> Query {
    Query {
        kind: Kind::SERIES,
        title: WINNER_TITLE.to_string(),
        episode: 1,
        ..Query::default()
    }
}

/// A complete (episode + synopsis) winner, so the chain settles on it
/// immediately. The source is named `tvmaze` so it outranks `anilist` and the
/// chain never consults the AniList fake: every AniList call below is the
/// backfill itself.
fn winner_media(artwork: String) -> Media {
    Media {
        source: "tvmaze".to_string(),
        source_id: "50497".to_string(),
        kind: Kind::SERIES,
        title: WINNER_TITLE.to_string(),
        artwork,
        synopsis: "Beerus awakens.".to_string(),
        episodes: vec![Episode {
            number: 1,
            title: "Episode 1".to_string(),
            synopsis: "Beerus awakens.".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

fn winner_source(artwork: String) -> FakeSource {
    FakeSource::with_media("tvmaze", winner_media(artwork))
}

fn anilist_hit(title: &str, artwork: &str) -> Media {
    Media {
        source: "anilist".to_string(),
        source_id: "206814".to_string(),
        kind: Kind::SERIES,
        title: title.to_string(),
        artwork: artwork.to_string(),
        ..Media::default()
    }
}

fn lookup(
    dir: &TempDir,
    winner: FakeSource,
    anilist: FakeSource,
) -> (Query, crate::Resolution, Arc<std::sync::atomic::AtomicI32>) {
    let anilist_calls = anilist.counter();
    let resolver = resolver_with(
        &dir.path(),
        vec![winner.shared(), anilist.shared()],
    );
    let q = beerus_query();
    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    (q, res, anilist_calls)
}

/// Signed 396-char winner artwork + an exact-title AniList hit: the
/// resolution carries the stable cover and the persisted entry stores it.
#[test]
fn backfill_replaces_signed_artwork_with_stable_cover() {
    let dir = TempDir::new();
    let signed = signed_artwork();
    let (q, res, anilist_calls) = lookup(
        &dir,
        winner_source(signed.clone()),
        FakeSource::with_media("anilist", anilist_hit(WINNER_TITLE, STABLE_COVER)),
    );
    assert!(res.found, "lookup = {res:?}, want found");
    assert_eq!(res.artwork, STABLE_COVER, "resolution artwork");
    assert_eq!(
        anilist_calls.load(Ordering::SeqCst),
        1,
        "exactly one AniList call (the backfill)"
    );
    let entry = cache_load_entry(&dir.path(), &cache_key(&q)).expect("cache entry");
    assert_eq!(
        entry.media.expect("cached media").artwork,
        STABLE_COVER,
        "persisted artwork is the stable cover"
    );
}

/// A signed winner with an AniList miss, and one with a different-title hit:
/// artwork stays untouched in both cases.
#[test]
fn backfill_miss_or_wrong_title_leaves_artwork_untouched() {
    for (name, anilist) in [
        ("miss", FakeSource::new("anilist")),
        (
            "wrong title",
            FakeSource::with_media(
                "anilist",
                anilist_hit("Dragon Ball Super", STABLE_COVER),
            ),
        ),
        (
            "absent cover",
            FakeSource::with_media("anilist", anilist_hit(WINNER_TITLE, "")),
        ),
    ] {
        let dir = TempDir::new();
        let signed = signed_artwork();
        let (_, res, _) = lookup(&dir, winner_source(signed.clone()), anilist);
        assert!(res.found, "{name}: lookup found");
        assert_eq!(res.artwork, signed, "{name}: artwork untouched");
    }
}

/// A usable winner (short, unsigned http cover): no AniList call at all.
#[test]
fn backfill_usable_artwork_makes_no_anilist_call() {
    let dir = TempDir::new();
    let usable = "https://example.com/poster.jpg".to_string();
    let (_, res, anilist_calls) = lookup(
        &dir,
        winner_source(usable.clone()),
        FakeSource::with_media("anilist", anilist_hit(WINNER_TITLE, STABLE_COVER)),
    );
    assert!(res.found, "lookup = {res:?}, want found");
    assert_eq!(res.artwork, usable, "artwork untouched");
    assert_eq!(
        anilist_calls.load(Ordering::SeqCst),
        0,
        "usable artwork must not consult AniList"
    );
}
