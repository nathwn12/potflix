//! T7-1 brick-cache tests: persistence round-trip, key-checked inserts,
//! Found-fixed vs Miss-retry semantics, force clears, corrupt/missing files,
//! atomic-failure safety, and unrelated-entity preservation.
//!
//! Every case runs against a `TempDir` scratch path (`load_from`/`empty_at`);
//! no test reads or writes the real `%LOCALAPPDATA%\Potflix\brick-cache.json`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::brick::{
    BRICK_FILE_NAME, Brick, BrickCache, BrickField, BrickLookup, BrickSource, BrickStatus,
    BrickValue, EntityKey, EpisodeKey, MISS_RETRY_AFTER, default_path, load_from,
};
use crate::testsupport::TempDir;
use crate::timefmt::{format_rfc3339, parse_rfc3339};

fn scratch_cache(scratch: &TempDir, name: &str) -> BrickCache {
    BrickCache::empty_at(PathBuf::from(scratch.child(name)))
}

fn show_entity() -> EntityKey {
    EntityKey {
        id: "series|breaking bad|2008".to_string(),
        tvmaze: Some(169),
        imdb: Some("tt0903747".to_string()),
        ..EntityKey::default()
    }
}

fn other_entity() -> EntityKey {
    EntityKey::new("series|better call saul|2015")
}

fn episode() -> EpisodeKey {
    EpisodeKey::Episode {
        season: 1,
        number: 1,
    }
}

fn found_title(entity: &EntityKey) -> Brick {
    Brick::found(
        BrickField::EpisodeTitle,
        BrickValue::Text {
            text: "Pilot".to_string(),
        },
        BrickSource::Tvmaze,
        entity.clone(),
        episode(),
        "tvmaze show 169 S01E01",
    )
}

fn miss_artwork(entity: &EntityKey) -> Brick {
    Brick::miss(
        BrickField::Artwork,
        BrickSource::Tvmaze,
        entity.clone(),
        EpisodeKey::None,
        "tvmaze show 169 has no poster yet",
    )
}

/// Round-trip: a Found brick and a Miss brick survive save + reload with
/// field, value, source, identity, status, timestamps, and provenance intact.
#[test]
fn brick_round_trip_persistence() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let found = found_title(&entity);
    let miss = miss_artwork(&entity);
    let mut cache = BrickCache::empty_at(path.clone());
    cache.insert(&entity, found.clone()).expect("insert found");
    cache.insert(&entity, miss.clone()).expect("insert miss");
    assert_eq!(cache.entity_count(), 1);
    assert_eq!(cache.brick_count(), 2);

    let reloaded = load_from(&path).expect("load_from");
    assert_eq!(reloaded.entity_count(), 1);
    assert_eq!(reloaded.brick_count(), 2);
    match reloaded.lookup(&entity, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => assert_eq!(got, &found, "found brick mismatch"),
        other => panic!("want Found, got {other:?}"),
    }
    match reloaded.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::MissWaiting(got) => assert_eq!(got, &miss, "miss brick mismatch"),
        other => panic!("want MissWaiting, got {other:?}"),
    }
    // The external IDs rode along: the merge in T7-2 keys off them.
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    let stored = &raw["entities"][0]["entity"];
    assert_eq!(stored["tvmaze"], 169);
    assert_eq!(stored["imdb"], "tt0903747");
}

/// Same-entity inserts are accepted, whatever the episode scope.
#[test]
fn brick_same_entity_accept() {
    let scratch = TempDir::new();
    let entity = show_entity();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    cache
        .insert(&entity, found_title(&entity))
        .expect("episode-scoped insert");
    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("entity-scoped insert");
    assert_eq!(cache.brick_count(), 2);
}

/// A brick whose entity id does not match the cache key is rejected, with no
/// partial insert. The byte check below is only a consistency check (an
/// unchanged re-save is byte-identical too): the falsifiable no-write proof
/// for refused paths lives in `brick_refused_overwrite_needs_no_save`, which
/// refuses while saving is impossible.
#[test]
fn brick_mismatched_entity_reject() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(&entity, found_title(&entity))
        .expect("seed insert");
    let before = std::fs::read(&path).expect("read seeded file");

    let foreign = found_title(&other_entity());
    let err = cache
        .insert(&entity, foreign)
        .expect_err("cross-entity insert must fail");
    assert!(
        err.message().contains("entity mismatch"),
        "unexpected error: {err}"
    );
    assert_eq!(cache.brick_count(), 1, "rejected brick leaked into memory");
    assert_eq!(
        std::fs::read(&path).expect("read file after reject"),
        before,
        "rejected brick rewrote the file"
    );
    // Case and whitespace differences are the SAME entity, not a mismatch.
    let mut alias = show_entity();
    alias.id = "  Series|BREAKING Bad|2008 ".to_string();
    cache
        .insert(&alias, miss_artwork(&alias))
        .expect("normalised-id insert");
    assert_eq!(cache.brick_count(), 2);
}

/// A brick whose episode scope does not fit its field is rejected: an
/// episode field needs an episode key, an entity field needs none.
#[test]
fn brick_episode_scope_mismatch_reject() {
    let scratch = TempDir::new();
    let entity = show_entity();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    let mut episode_field_without_episode = found_title(&entity);
    episode_field_without_episode.episode = EpisodeKey::None;
    assert!(
        cache
            .insert(&entity, episode_field_without_episode)
            .is_err(),
        "episode field with EpisodeKey::None must fail"
    );
    let mut entity_field_with_episode = miss_artwork(&entity);
    entity_field_with_episode.episode = episode();
    assert!(
        cache.insert(&entity, entity_field_with_episode).is_err(),
        "entity field with an episode key must fail"
    );
    assert_eq!(cache.brick_count(), 0);
}

/// A Found brick is fixed: it serves forever (even with an ancient
/// `fetched_at`), carries no retry clock, and survives reload byte for byte.
#[test]
fn brick_found_survives_reload() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut ancient = found_title(&entity);
    ancient.fetched_at = format_rfc3339(SystemTime::now() - Duration::from_secs(365 * 24 * 3600));
    assert!(
        ancient.retry_after.is_none(),
        "a Found brick must carry no retry clock"
    );
    assert!(
        !ancient.retry_due(SystemTime::now()),
        "a Found brick is never retry-due"
    );
    let mut cache = BrickCache::empty_at(path.clone());
    cache.insert(&entity, ancient.clone()).expect("insert");
    match cache.lookup(&entity, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => assert_eq!(got, &ancient),
        other => panic!("ancient Found must still serve, got {other:?}"),
    }
    let reloaded = load_from(&path).expect("load_from");
    match reloaded.lookup(&entity, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => {
            assert_eq!(got.value, ancient.value);
            assert_eq!(got.source, BrickSource::Tvmaze);
            assert_eq!(got.provenance, "tvmaze show 169 S01E01");
            assert_eq!(got.fetched_at, ancient.fetched_at);
        }
        other => panic!("Found did not survive reload, got {other:?}"),
    }
}

/// Miss retry: a fresh miss waits, an overdue (or clockless) miss is due, a
/// Found answer recovers the slot, and a later miss never evicts a Found
/// brick.
#[test]
fn brick_miss_retry() {
    let scratch = TempDir::new();
    let entity = show_entity();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");

    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("insert miss");
    match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::MissWaiting(_) => {}
        other => panic!("fresh miss must wait, got {other:?}"),
    }

    // Backdate the miss past its retry clock: now it is due.
    let mut overdue = miss_artwork(&entity);
    let past = SystemTime::now() - MISS_RETRY_AFTER - Duration::from_secs(3600);
    overdue.fetched_at = format_rfc3339(past);
    overdue.retry_after = Some(format_rfc3339(
        past.checked_add(MISS_RETRY_AFTER).expect("add retry"),
    ));
    assert!(overdue.retry_due(SystemTime::now()));
    cache.insert(&entity, overdue).expect("insert overdue miss");
    assert!(
        matches!(
            cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::RetryDue
        ),
        "overdue miss must be retry-due"
    );

    // A clockless miss fails open to a retry, never pins quiet forever.
    let mut clockless = miss_artwork(&entity);
    clockless.retry_after = None;
    assert!(clockless.retry_due(SystemTime::now()));

    // Recovery: a Found answer replaces the miss and serves.
    let found = Brick::found(
        BrickField::Artwork,
        BrickValue::Text {
            text: "https://example.com/poster.jpg".to_string(),
        },
        BrickSource::Tvmaze,
        entity.clone(),
        EpisodeKey::None,
        "tvmaze show 169 poster",
    );
    cache.insert(&entity, found).expect("found recovers miss");
    assert!(
        matches!(
            cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::Found(_)
        ),
        "found must recover a missed slot"
    );

    // A later miss for the same slot is a silent no-op: the value stands.
    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("miss after found");
    match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "https://example.com/poster.jpg".to_string(),
            },
            "a miss evicted a Found brick"
        ),
        other => panic!("Found must survive a later miss, got {other:?}"),
    }
}

/// Never-seen slots and empty entity ids are `Absent`, never an error.
#[test]
fn brick_absent_lookup() {
    let scratch = TempDir::new();
    let entity = show_entity();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    cache
        .insert(&entity, found_title(&entity))
        .expect("seed insert");
    assert!(matches!(
        cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork),
        BrickLookup::Absent
    ));
    assert!(matches!(
        cache.lookup(&other_entity(), &episode(), BrickField::EpisodeTitle),
        BrickLookup::Absent
    ));
    assert!(matches!(
        cache.lookup(&EntityKey::new(""), &episode(), BrickField::EpisodeTitle),
        BrickLookup::Absent
    ));
    // Same key on both sides, so the empty-id guard is the only possible
    // rejection (not an entity mismatch): the error must name the empty id.
    let empty = EntityKey::new("  ");
    let empty_brick = found_title(&empty);
    let err = cache
        .insert(&empty, empty_brick)
        .expect_err("empty entity id must fail");
    assert!(
        err.message().contains("empty entity id"),
        "unexpected error: {err}"
    );
}

/// Force clears: one field, one episode (entity-level bricks survive), one
/// entity -- each durable across reload, each leaving the rest alone.
#[test]
fn brick_force_clear() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache.insert(&entity, found_title(&entity)).expect("title");
    let mut synopsis = found_title(&entity);
    synopsis.field = BrickField::EpisodeSynopsis;
    synopsis.value = BrickValue::Text {
        text: "A teacher cooks.".to_string(),
    };
    cache.insert(&entity, synopsis).expect("synopsis");
    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("artwork");
    assert_eq!(cache.brick_count(), 3);

    // One field: only that slot goes.
    assert!(
        cache
            .clear_field(&entity, &episode(), BrickField::EpisodeSynopsis)
            .expect("clear field")
    );
    assert!(
        matches!(
            cache.lookup(&entity, &episode(), BrickField::EpisodeSynopsis),
            BrickLookup::Absent
        ),
        "cleared field must read absent"
    );
    assert!(
        matches!(
            cache.lookup(&entity, &episode(), BrickField::EpisodeTitle),
            BrickLookup::Found(_)
        ),
        "sibling field must survive a field clear"
    );
    assert!(
        !cache
            .clear_field(&entity, &episode(), BrickField::EpisodeSynopsis)
            .expect("second clear"),
        "clearing an absent slot must report false"
    );

    // One episode: episode bricks go, the entity-level artwork miss stays.
    assert_eq!(
        cache
            .clear_episode(&entity, &episode())
            .expect("clear episode"),
        1
    );
    assert!(matches!(
        cache.lookup(&entity, &episode(), BrickField::EpisodeTitle),
        BrickLookup::Absent
    ));
    assert!(
        matches!(
            cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::MissWaiting(_)
        ),
        "entity-level bricks must survive an episode clear"
    );
    assert_eq!(
        cache
            .clear_episode(&entity, &episode())
            .expect("second episode clear"),
        0
    );

    // Reload mid-way: the clears so far are durable.
    let reloaded = load_from(&path).expect("load_from");
    assert_eq!(reloaded.brick_count(), 1);

    // One entity: everything under it goes.
    let mut cache = reloaded;
    assert_eq!(cache.clear_entity(&entity).expect("clear entity"), 1);
    assert_eq!(cache.brick_count(), 0);
    assert_eq!(cache.clear_entity(&entity).expect("second entity clear"), 0);
    let reloaded = load_from(&path).expect("load_from after entity clear");
    assert_eq!(reloaded.brick_count(), 0);
    assert!(matches!(
        reloaded.lookup(&entity, &EpisodeKey::None, BrickField::Artwork),
        BrickLookup::Absent
    ));
}

/// Missing, corrupt, and foreign-version files all load as an empty cache --
/// never an error, never a touch of the real store.
#[test]
fn brick_corrupt_or_missing_file_is_empty() {
    let scratch = TempDir::new();
    let missing = PathBuf::from(scratch.child("no-such-file.json"));
    let cache = load_from(&missing).expect("missing file");
    assert_eq!(cache.entity_count(), 0);
    assert_eq!(cache.brick_count(), 0);

    let bad_path = PathBuf::from(scratch.child("bad.json"));
    std::fs::write(&bad_path, "not json{{{").expect("write garbage");
    let cache = load_from(&bad_path).expect("corrupt file");
    assert_eq!(cache.brick_count(), 0);
    std::fs::write(&bad_path, "{\"key\":").expect("write truncation");
    let cache = load_from(&bad_path).expect("truncated file");
    assert_eq!(cache.brick_count(), 0);

    std::fs::write(&bad_path, "{\"version\":999,\"entities\":[]}").expect("write future version");
    let cache = load_from(&bad_path).expect("future version");
    assert_eq!(cache.brick_count(), 0);

    // A foreign-version file carrying entities still reads empty (refused,
    // never misread), and the next save replaces it with version 1.
    std::fs::write(
        &bad_path,
        "{\"version\":999,\"entities\":[{\"entity\":{\"id\":\"x\"},\"bricks\":[]}]}",
    )
    .expect("write foreign entities");
    let mut cache = load_from(&bad_path).expect("foreign entities");
    assert_eq!(cache.entity_count(), 0);
    let entity = show_entity();
    cache
        .insert(&entity, found_title(&entity))
        .expect("insert after foreign");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&bad_path).expect("read")).expect("decode");
    assert_eq!(raw["version"], 1);
}

/// A failed atomic write leaves the prior file byte-identical, rolls the
/// in-memory change back, and leaves no temp file behind.
///
/// Windows-only: a read-only target makes Windows' replace-rename fail, which
/// is the real write failure this probes. On Unix the rename-over step
/// succeeds despite the read-only bit (permissions live on the directory), so
/// the failure never happens and the test would prove nothing.
#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
#[test]
fn brick_atomic_failure_leaves_prior_file() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(&entity, found_title(&entity))
        .expect("seed insert");
    let before = std::fs::read(&path).expect("read seeded file");

    // Read-only target: the rename-over step fails on Windows, after the temp
    // file was staged beside it -- exactly the midpoint an atomic write must
    // survive.
    let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).expect("make read-only");
    let failed = cache.insert(&entity, miss_artwork(&entity));
    let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).expect("make writable");
    assert!(
        failed.is_err(),
        "insert over a read-only file must fail, else this proves nothing"
    );
    assert_eq!(
        cache.brick_count(),
        1,
        "a failed save must roll memory back"
    );
    assert_eq!(
        std::fs::read(&path).expect("read file after failure"),
        before,
        "a failed save must leave the prior file byte-identical"
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
}

/// Clearing one entity never touches another, in memory or after reload.
#[test]
fn brick_unrelated_entities_preserved() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let first = show_entity();
    let second = other_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache.insert(&first, found_title(&first)).expect("first");
    let mut other_title = found_title(&second);
    other_title.value = BrickValue::Text {
        text: "Uno".to_string(),
    };
    cache.insert(&second, other_title.clone()).expect("second");
    assert_eq!(cache.entity_count(), 2);

    assert_eq!(cache.clear_entity(&first).expect("clear first"), 1);
    assert_eq!(cache.entity_count(), 1);
    match cache.lookup(&second, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => assert_eq!(got.value, other_title.value),
        other => panic!("unrelated entity must survive the clear, got {other:?}"),
    }
    let reloaded = load_from(&path).expect("load_from");
    assert_eq!(reloaded.entity_count(), 1);
    match reloaded.lookup(&second, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(got) => assert_eq!(got.value, other_title.value),
        other => panic!("unrelated entity must survive reload, got {other:?}"),
    }
}

/// The real paths are the contract, and the store is never touched: every
/// test above runs through `load_from`/`empty_at` on a `TempDir`.
#[test]
fn brick_paths_are_local_app_data_potflix() {
    let path = default_path();
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(BRICK_FILE_NAME)
    );
    // An unset or empty LOCALAPPDATA falls back to the executable directory
    // (the `overrides::default_path` convention), whose parent is not
    // necessarily Potflix: only assert the Potflix parent when the env root
    // actually governs the path.
    match std::env::var("LOCALAPPDATA") {
        Ok(root) if !root.is_empty() => {
            let parent = path.parent().expect("cache parent");
            assert_eq!(
                parent.file_name().and_then(|name| name.to_str()),
                Some("Potflix")
            );
            assert_eq!(
                path,
                PathBuf::from(root).join("Potflix").join(BRICK_FILE_NAME)
            );
        }
        _ => assert!(path.ends_with(BRICK_FILE_NAME)),
    }
}

/// A brick filed under the wrong bucket inside the file is foreign data, not
/// ours to serve: it loads dropped, never misattributed.
#[test]
fn brick_foreign_bucket_brick_is_dropped_on_load() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let stray = found_title(&other_entity());
    let file = serde_json::json!({
        "version": 1,
        "entities": [
            {"entity": {"id": entity.id}, "bricks": [stray]},
        ],
    });
    std::fs::write(&path, serde_json::to_vec(&file).expect("encode")).expect("write");
    let cache = load_from(&path).expect("load_from");
    assert_eq!(
        cache.brick_count(),
        0,
        "a brick under the wrong bucket must not be served"
    );
}

/// `BrickStatus` round-trips through its wire names, and every source variant
/// has a distinct one (the T7-2 merge logs these verbatim).
#[test]
fn brick_status_and_source_wire_names() {
    assert_eq!(
        serde_json::to_value(BrickStatus::Found).expect("encode"),
        serde_json::Value::String("found".to_string())
    );
    assert_eq!(
        serde_json::to_value(BrickStatus::Miss).expect("encode"),
        serde_json::Value::String("miss".to_string())
    );
    let sources = [
        BrickSource::Anilist,
        BrickSource::Tvmaze,
        BrickSource::Tmdb,
        BrickSource::Wikipedia,
        BrickSource::Cinemeta,
        BrickSource::Manual,
    ];
    let mut names = std::collections::HashSet::new();
    for source in sources {
        let name = serde_json::to_value(source).expect("encode");
        assert!(name.is_string(), "source must encode as a string");
        assert!(names.insert(name), "duplicate source wire name");
    }
    assert_eq!(names.len(), sources.len());
}

/// An entity slot already Found still learns external IDs from a later Miss:
/// the refused brick's value is dropped, its IDs are kept -- in memory and
/// across reload -- and a new slot from a still-newer key teaches further IDs
/// without forking a second bucket.
#[test]
fn brick_insert_absorbs_external_ids() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    // The entity starts bare: no external IDs at all.
    let bare = EntityKey::new("series|breaking bad|2008");
    let mut cache = BrickCache::empty_at(path.clone());
    let poster = Brick::found(
        BrickField::Artwork,
        BrickValue::Text {
            text: "https://example.com/poster.jpg".to_string(),
        },
        BrickSource::Tvmaze,
        bare.clone(),
        EpisodeKey::None,
        "tvmaze show 169 poster",
    );
    cache.insert(&bare, poster).expect("seed found");
    assert_eq!(cache.entity_count(), 1);

    // A later Miss for the same slot carries the IDs the seeder lacked: the
    // value is refused (the Found poster stands) but the IDs are absorbed.
    let informed = show_entity();
    cache
        .insert(&informed, miss_artwork(&informed))
        .expect("miss teaches ids");
    match cache.lookup(&informed, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "https://example.com/poster.jpg".to_string(),
            },
            "a miss must never evict a Found brick"
        ),
        other => panic!("Found must survive a later miss, got {other:?}"),
    }

    // A new slot from a still-newer key teaches further IDs down the same path.
    let mut tmdb_key = show_entity();
    tmdb_key.tmdb = Some(1396);
    cache
        .insert(&tmdb_key, found_title(&tmdb_key))
        .expect("new slot with newer ids");
    assert_eq!(cache.entity_count(), 1, "ids must not fork a bucket");

    let reloaded = load_from(&path).expect("load_from");
    assert_eq!(reloaded.entity_count(), 1);
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    let stored = &raw["entities"][0]["entity"];
    assert_eq!(stored["tvmaze"], 169);
    assert_eq!(stored["imdb"], "tt0903747");
    assert_eq!(stored["tmdb"], 1396);
}

/// Manual precedence backstop: a provider Found never overwrites a Manual
/// Found; a Manual Found overwrites a provider one freely -- in memory and
/// across reload. The refused overwrite needs no save: it stays `Ok` with
/// memory unchanged even while saving is impossible (a byte-identical re-save
/// would look untouched too, so the blocked path is the falsifiable proof).
#[test]
fn brick_manual_found_beats_provider() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(
            &entity,
            Brick::found(
                BrickField::Artwork,
                BrickValue::Text {
                    text: "https://example.com/provider.jpg".to_string(),
                },
                BrickSource::Tvmaze,
                entity.clone(),
                EpisodeKey::None,
                "tvmaze show 169 poster",
            ),
        )
        .expect("provider seed");

    // The hand correction replaces the provider answer.
    cache
        .insert(
            &entity,
            Brick::found(
                BrickField::Artwork,
                BrickValue::Text {
                    text: "https://example.com/manual.jpg".to_string(),
                },
                BrickSource::Manual,
                entity.clone(),
                EpisodeKey::None,
                "hand-picked poster",
            ),
        )
        .expect("manual overwrites provider");
    match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => {
            assert_eq!(got.source, BrickSource::Manual);
            assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "https://example.com/manual.jpg".to_string(),
                }
            );
        }
        other => panic!("manual must overwrite provider, got {other:?}"),
    }

    // A later provider answer is refused with no save: the same key carries no
    // new IDs, so there is nothing to absorb. The blocked path proves no write
    // was attempted -- the refuse stays `Ok` and memory is unchanged even
    // though any save would fail.
    let before = break_save_with_dir(&path);
    cache
        .insert(
            &entity,
            Brick::found(
                BrickField::Artwork,
                BrickValue::Text {
                    text: "https://example.com/provider-v2.jpg".to_string(),
                },
                BrickSource::Tmdb,
                entity.clone(),
                EpisodeKey::None,
                "tmdb poster",
            ),
        )
        .expect("refused provider is Ok even while saving is impossible");
    match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => {
            assert_eq!(got.source, BrickSource::Manual, "provider overwrote manual");
            assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "https://example.com/manual.jpg".to_string(),
                }
            );
        }
        other => panic!("manual must survive a later provider, got {other:?}"),
    }
    assert_no_temp_files(&scratch);
    repair_save_with_file(&path, &before);
    let reloaded = load_from(&path).expect("load_from");
    match reloaded.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(got.source, BrickSource::Manual),
        other => panic!("manual must survive reload, got {other:?}"),
    }
}

/// A refused Miss-over-Found (same IDs, nothing new to absorb) likewise needs
/// no save: it stays `Ok` with the Found value intact while saving is
/// impossible.
#[test]
fn brick_refused_overwrite_needs_no_save() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    let poster = Brick::found(
        BrickField::Artwork,
        BrickValue::Text {
            text: "https://example.com/poster.jpg".to_string(),
        },
        BrickSource::Tvmaze,
        entity.clone(),
        EpisodeKey::None,
        "tvmaze show 169 poster",
    );
    cache.insert(&entity, poster).expect("seed found");

    let before = break_save_with_dir(&path);
    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("refused miss is Ok even while saving is impossible");
    match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "https://example.com/poster.jpg".to_string(),
            },
            "a miss evicted a Found brick"
        ),
        other => panic!("Found must survive a refused miss, got {other:?}"),
    }
    assert_no_temp_files(&scratch);
    repair_save_with_file(&path, &before);
    let reloaded = load_from(&path).expect("load_from");
    match reloaded.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(_) => {}
        other => panic!("Found must survive repair + reload, got {other:?}"),
    }
}

/// A caller slip never becomes a permanent empty fact: field/value shape
/// mismatches and empty Found values are rejected with no write, while Miss
/// bricks keep their placeholder behavior and well-shaped Found values store.
#[test]
fn brick_insert_rejects_mismatched_or_empty_found() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());

    // Shape mismatches: Artwork takes text, Year takes a year, Genres takes
    // genre names, an episode title takes text.
    let shape_slips = [
        Brick::found(
            BrickField::Artwork,
            BrickValue::Year { year: 2008 },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
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
        Brick::found(
            BrickField::Genres,
            BrickValue::Text {
                text: "Drama".to_string(),
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
        Brick::found(
            BrickField::Title,
            BrickValue::Genres {
                genres: vec!["Drama".to_string()],
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
        Brick::found(
            BrickField::EpisodeTitle,
            BrickValue::Year { year: 2008 },
            BrickSource::Tvmaze,
            entity.clone(),
            episode(),
            "slip",
        ),
    ];
    for slip in shape_slips {
        let err = cache
            .insert(&entity, slip)
            .expect_err("shape mismatch must fail");
        assert!(
            err.message().contains("value shape mismatch"),
            "unexpected error: {err}"
        );
    }

    // Empty Found values: blank text, the zero year, no usable genre name.
    let empty_slips = [
        Brick::found(
            BrickField::Artwork,
            BrickValue::Text {
                text: String::new(),
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
        Brick::found(
            BrickField::Title,
            BrickValue::Text {
                text: "   ".to_string(),
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
        Brick::found(
            BrickField::Year,
            BrickValue::Year { year: 0 },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
        Brick::found(
            BrickField::Genres,
            BrickValue::Genres { genres: vec![] },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
        Brick::found(
            BrickField::Genres,
            BrickValue::Genres {
                genres: vec!["".to_string(), "  ".to_string()],
            },
            BrickSource::Tvmaze,
            entity.clone(),
            EpisodeKey::None,
            "slip",
        ),
        Brick::found(
            BrickField::EpisodeTitle,
            BrickValue::Text {
                text: String::new(),
            },
            BrickSource::Tvmaze,
            entity.clone(),
            episode(),
            "slip",
        ),
    ];
    for slip in empty_slips {
        let err = cache
            .insert(&entity, slip)
            .expect_err("empty Found must fail");
        assert!(
            err.message().contains("empty Found value"),
            "unexpected error: {err}"
        );
    }
    assert_eq!(cache.brick_count(), 0, "rejected bricks leaked into memory");
    assert!(!path.exists(), "rejected bricks must not create the file");

    // Miss behavior is untouched: the placeholder still stores and waits.
    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("miss keeps its placeholder");
    assert!(
        matches!(
            cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::MissWaiting(_)
        ),
        "miss placeholder must still store"
    );

    // Well-shaped Found values still store: year, genres, and text.
    cache
        .insert(
            &entity,
            Brick::found(
                BrickField::Year,
                BrickValue::Year { year: 2008 },
                BrickSource::Tvmaze,
                entity.clone(),
                EpisodeKey::None,
                "tvmaze show 169 year",
            ),
        )
        .expect("year stores");
    cache
        .insert(
            &entity,
            Brick::found(
                BrickField::Genres,
                BrickValue::Genres {
                    genres: vec!["Drama".to_string(), "Crime".to_string()],
                },
                BrickSource::Tvmaze,
                entity.clone(),
                EpisodeKey::None,
                "tvmaze show 169 genres",
            ),
        )
        .expect("genres store");
    cache
        .insert(
            &entity,
            Brick::found(
                BrickField::Title,
                BrickValue::Text {
                    text: "Breaking Bad".to_string(),
                },
                BrickSource::Tvmaze,
                entity.clone(),
                EpisodeKey::None,
                "tvmaze show 169 title",
            ),
        )
        .expect("title stores");
    assert_eq!(cache.brick_count(), 4);
}

/// Replaces the seeded cache file with a directory of the same name, so the
/// next save stages its temp file and then fails the rename-over step on every
/// platform. Returns the seeded file bytes for later comparison/restore.
fn break_save_with_dir(path: &Path) -> Vec<u8> {
    let before = std::fs::read(path).expect("read seeded file");
    std::fs::remove_file(path).expect("remove seeded file");
    std::fs::create_dir(path).expect("dir at cache path");
    before
}

/// Undoes [`break_save_with_dir`]: removes the blocking directory and restores
/// the seeded bytes, so the test can then exercise the success path.
fn repair_save_with_file(path: &Path, before: &[u8]) {
    std::fs::remove_dir(path).expect("remove blocking dir");
    std::fs::write(path, before).expect("restore seeded file");
}

/// A scratch directory holds no staged temp files.
fn assert_no_temp_files(scratch: &TempDir) {
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
}

/// A failed `clear_entity` rolls memory back (the cleared bricks still
/// resolve), and the repaired clear is durable with matching entity counts.
#[test]
fn brick_clear_entity_save_failure_rolls_back() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(&entity, found_title(&entity))
        .expect("seed title");
    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("seed artwork");
    assert_eq!(cache.brick_count(), 2);

    let before = break_save_with_dir(&path);
    let failed = cache.clear_entity(&entity);
    assert!(
        failed.is_err(),
        "clear over a blocked file must fail, else this proves nothing"
    );
    match cache.lookup(&entity, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(_) => {}
        other => panic!("rolled-back title must still resolve, got {other:?}"),
    }
    assert!(
        matches!(
            cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork),
            BrickLookup::MissWaiting(_)
        ),
        "rolled-back miss must still resolve"
    );
    assert_eq!(cache.entity_count(), 1);
    assert_eq!(cache.brick_count(), 2);
    assert_no_temp_files(&scratch);

    repair_save_with_file(&path, &before);
    assert_eq!(cache.clear_entity(&entity).expect("clear entity"), 2);
    let reloaded = load_from(&path).expect("load_from");
    assert_eq!(cache.entity_count(), reloaded.entity_count());
    assert_eq!(reloaded.entity_count(), 0);
    assert_eq!(reloaded.brick_count(), 0);
}

/// A failed `clear_episode` rolls memory back (the cleared bricks still
/// resolve); the repaired clear drops the emptied bucket before the save, so
/// the file never keeps a zero-brick bucket and reload matches memory.
#[test]
fn brick_clear_episode_save_failure_rolls_back() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(&entity, found_title(&entity))
        .expect("seed title");
    let mut synopsis = found_title(&entity);
    synopsis.field = BrickField::EpisodeSynopsis;
    synopsis.value = BrickValue::Text {
        text: "A teacher cooks.".to_string(),
    };
    cache.insert(&entity, synopsis).expect("seed synopsis");

    let before = break_save_with_dir(&path);
    let failed = cache.clear_episode(&entity, &episode());
    assert!(
        failed.is_err(),
        "clear over a blocked file must fail, else this proves nothing"
    );
    match cache.lookup(&entity, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(_) => {}
        other => panic!("rolled-back title must still resolve, got {other:?}"),
    }
    match cache.lookup(&entity, &episode(), BrickField::EpisodeSynopsis) {
        BrickLookup::Found(_) => {}
        other => panic!("rolled-back synopsis must still resolve, got {other:?}"),
    }
    assert_eq!(cache.entity_count(), 1);
    assert_eq!(cache.brick_count(), 2);
    assert_no_temp_files(&scratch);

    repair_save_with_file(&path, &before);
    assert_eq!(
        cache
            .clear_episode(&entity, &episode())
            .expect("clear episode"),
        2
    );
    // The emptied bucket went before the save: the file holds no zero-brick
    // bucket and reload agrees with memory.
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    assert_eq!(raw["entities"], serde_json::Value::Array(vec![]));
    let reloaded = load_from(&path).expect("load_from");
    assert_eq!(cache.entity_count(), reloaded.entity_count());
    assert_eq!(reloaded.entity_count(), 0);
}

/// A failed `clear_field` rolls memory back (the cleared brick still
/// resolves); the repaired clear drops the emptied bucket before the save, so
/// the file never keeps a zero-brick bucket and reload matches memory.
#[test]
fn brick_clear_field_save_failure_rolls_back() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let entity = show_entity();
    let mut cache = BrickCache::empty_at(path.clone());
    cache
        .insert(&entity, found_title(&entity))
        .expect("seed title");

    let before = break_save_with_dir(&path);
    let failed = cache.clear_field(&entity, &episode(), BrickField::EpisodeTitle);
    assert!(
        failed.is_err(),
        "clear over a blocked file must fail, else this proves nothing"
    );
    match cache.lookup(&entity, &episode(), BrickField::EpisodeTitle) {
        BrickLookup::Found(_) => {}
        other => panic!("rolled-back title must still resolve, got {other:?}"),
    }
    assert_eq!(cache.entity_count(), 1);
    assert_eq!(cache.brick_count(), 1);
    assert_no_temp_files(&scratch);

    repair_save_with_file(&path, &before);
    assert!(
        cache
            .clear_field(&entity, &episode(), BrickField::EpisodeTitle)
            .expect("clear field")
    );
    // The emptied bucket went before the save: the file holds no zero-brick
    // bucket and reload agrees with memory.
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    assert_eq!(raw["entities"], serde_json::Value::Array(vec![]));
    let reloaded = load_from(&path).expect("load_from");
    assert_eq!(cache.entity_count(), reloaded.entity_count());
    assert_eq!(reloaded.entity_count(), 0);
    assert!(matches!(
        reloaded.lookup(&entity, &episode(), BrickField::EpisodeTitle),
        BrickLookup::Absent
    ));
}

/// The retry bound is one day, as a bound not a constant: the same stored miss
/// waits now and is due once the day has passed -- no clock hand-editing,
/// just the explicit-clock lookup. The literal pins the bound (changing
/// `MISS_RETRY_AFTER` alone fails this); the boundary probes use the stored
/// clock, not the constant, so both sides cannot drift together.
#[test]
fn brick_miss_retry_bound_is_behavior() {
    assert_eq!(
        MISS_RETRY_AFTER,
        Duration::from_secs(86_400),
        "the miss quiet day is exactly 86_400 s"
    );
    let scratch = TempDir::new();
    let entity = show_entity();
    let mut cache = scratch_cache(&scratch, "brick-cache.json");
    cache
        .insert(&entity, miss_artwork(&entity))
        .expect("insert miss");
    let now = SystemTime::now();
    assert!(
        matches!(
            cache.lookup_at(&entity, &EpisodeKey::None, BrickField::Artwork, now),
            BrickLookup::MissWaiting(_)
        ),
        "a fresh miss must wait"
    );
    // Boundary around the STORED retry clock: one second before it the miss
    // still waits, at it the miss is due. Literals, never `MISS_RETRY_AFTER`,
    // so a wrong constant cannot pass both sides.
    let (stored_fetched, stored_after) =
        match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
            BrickLookup::MissWaiting(got) => (
                parse_rfc3339(&got.fetched_at).expect("stored fetched_at parses"),
                parse_rfc3339(
                    got.retry_after
                        .as_deref()
                        .expect("a stored miss carries retry_after"),
                )
                .expect("stored retry_after parses"),
            ),
            other => panic!("fresh miss must wait, got {other:?}"),
        };
    assert!(
        matches!(
            cache.lookup_at(
                &entity,
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
                &entity,
                &EpisodeKey::None,
                BrickField::Artwork,
                stored_after
            ),
            BrickLookup::RetryDue
        ),
        "at the stored bound the miss must be due"
    );
    // And the literal day bound agrees with the stored clock on a SEEDED
    // clock: the stored `retry_after` is exactly one literal day after the
    // stored `fetched_at`, and both sides of that boundary probe read stored
    // values -- no wall-clock drift between the insert's stamp and `now` (a
    // slow CI) can flake them.
    assert_eq!(
        stored_after
            .duration_since(stored_fetched)
            .expect("retry_after sits after fetched_at"),
        Duration::from_secs(86_400),
        "the stored retry clock must sit exactly one literal day out"
    );
    assert!(
        matches!(
            cache.lookup_at(
                &entity,
                &EpisodeKey::None,
                BrickField::Artwork,
                stored_fetched + Duration::from_secs(86_400 - 1),
            ),
            BrickLookup::MissWaiting(_)
        ),
        "one second before the seeded bound the miss must still wait"
    );
    assert!(
        matches!(
            cache.lookup_at(
                &entity,
                &EpisodeKey::None,
                BrickField::Artwork,
                stored_fetched + Duration::from_secs(86_400),
            ),
            BrickLookup::RetryDue
        ),
        "at the seeded bound the miss must be due"
    );
}

/// `absorb_ids_only` saves -- and rolls back: a Found brick seeded on a bare
/// key, a blocked save, a Miss carrying new external IDs for the same slot.
/// The insert must `Err`, the in-memory identity must stay bare (delete the
/// rollback line and the repaired save persists the absorbed IDs, failing the
/// bare assertions below), and the repaired write + reload still shows bare.
#[test]
fn brick_absorb_ids_only_save_failure_rolls_back() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let bare = EntityKey::new("series|breaking bad|2008");
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
        .expect("seed found on bare key");

    let before = break_save_with_dir(&path);
    let informed = show_entity();
    let failed = cache.insert(&informed, miss_artwork(&informed));
    assert!(
        failed.is_err(),
        "absorb over a blocked file must fail, else this proves nothing"
    );
    // The Found value stands (the Miss was refused).
    match cache.lookup(&bare, &EpisodeKey::None, BrickField::Artwork) {
        BrickLookup::Found(got) => assert_eq!(
            got.value,
            BrickValue::Text {
                text: "https://example.com/poster.jpg".to_string(),
            }
        ),
        other => panic!("rolled-back Found must still resolve, got {other:?}"),
    }
    assert_no_temp_files(&scratch);

    // Repair, persist the (rolled-back) memory, reload: the stored identity is
    // still bare. Without the rollback the absorb would have stuck and this
    // save would persist tvmaze/imdb.
    repair_save_with_file(&path, &before);
    cache.save().expect("save after repair");
    let reloaded = load_from(&path).expect("load_from");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    let stored = &raw["entities"][0]["entity"];
    assert_eq!(stored["id"], "series|breaking bad|2008");
    assert!(
        stored.get("tvmaze").is_none(),
        "rolled-back absorb leaked tvmaze: {stored}"
    );
    assert!(
        stored.get("imdb").is_none(),
        "rolled-back absorb leaked imdb: {stored}"
    );
    assert_eq!(reloaded.entity_count(), 1);
    assert_eq!(reloaded.brick_count(), 1);
}

/// Load drops zero-brick buckets: a hand-written empty bucket never counts in
/// `entity_count()` and the next save does not re-save it.
#[test]
fn brick_load_drops_zero_brick_buckets() {
    let scratch = TempDir::new();
    let path = PathBuf::from(scratch.child("brick-cache.json"));
    let empty_entity = other_entity();
    let live_entity = show_entity();
    let live = found_title(&live_entity);
    let file = serde_json::json!({
        "version": 1,
        "entities": [
            {"entity": serde_json::to_value(&empty_entity).expect("encode"), "bricks": []},
            {"entity": serde_json::to_value(&live_entity).expect("encode"),
             "bricks": [serde_json::to_value(&live).expect("encode")]},
        ],
    });
    std::fs::write(&path, serde_json::to_vec(&file).expect("encode")).expect("write");
    let cache = load_from(&path).expect("load_from");
    assert_eq!(
        cache.entity_count(),
        1,
        "a zero-brick bucket must not count"
    );
    assert_eq!(cache.brick_count(), 1);

    cache.save().expect("save after load");
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read file")).expect("decode file");
    let entities = raw["entities"].as_array().expect("entities array");
    assert_eq!(entities.len(), 1, "zero-brick bucket was re-saved: {raw}");
    let reloaded = load_from(&path).expect("reload");
    assert_eq!(reloaded.entity_count(), 1);
    assert_eq!(reloaded.brick_count(), 1);
}

/// Duplicate-slot load collapse: Manual beats provider regardless of order;
/// same-source (and provider-vs-provider) duplicates stay deterministic
/// last-wins.
#[test]
fn brick_load_duplicate_slot_prefers_manual() {
    fn slot_file(entity: &EntityKey, first: &Brick, second: &Brick) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "entities": [
                {"entity": serde_json::to_value(entity).expect("encode"),
                 "bricks": [
                    serde_json::to_value(first).expect("encode"),
                    serde_json::to_value(second).expect("encode"),
                 ]},
            ],
        })
    }
    fn artwork(entity: &EntityKey, source: BrickSource, text: &str) -> Brick {
        Brick::found(
            BrickField::Artwork,
            BrickValue::Text {
                text: text.to_string(),
            },
            source,
            entity.clone(),
            EpisodeKey::None,
            "probe",
        )
    }

    // Provider-then-Manual: Manual wins despite coming... in either order.
    for (first_source, second_source, want) in [
        (BrickSource::Tvmaze, BrickSource::Manual, "manual"),
        (BrickSource::Manual, BrickSource::Tvmaze, "manual"),
    ] {
        let scratch = TempDir::new();
        let path = PathBuf::from(scratch.child("brick-cache.json"));
        let entity = show_entity();
        let first = artwork(&entity, first_source, "first");
        let second = artwork(&entity, second_source, "second");
        let want_value = if want == "manual" {
            if first_source == BrickSource::Manual {
                "first"
            } else {
                "second"
            }
        } else {
            "second"
        };
        std::fs::write(
            &path,
            serde_json::to_vec(&slot_file(&entity, &first, &second)).expect("encode"),
        )
        .expect("write");
        let cache = load_from(&path).expect("load_from");
        assert_eq!(cache.brick_count(), 1, "duplicates must collapse to one");
        match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
            BrickLookup::Found(got) => {
                assert_eq!(got.source, BrickSource::Manual, "manual must win");
                assert_eq!(
                    got.value,
                    BrickValue::Text {
                        text: want_value.to_string(),
                    }
                );
            }
            other => panic!("collapsed slot must serve Found, got {other:?}"),
        }
    }

    // Same-source duplicates: deterministic last-wins.
    {
        let scratch = TempDir::new();
        let path = PathBuf::from(scratch.child("brick-cache.json"));
        let entity = show_entity();
        let first = artwork(&entity, BrickSource::Tvmaze, "https://example.com/v1.jpg");
        let second = artwork(&entity, BrickSource::Tvmaze, "https://example.com/v2.jpg");
        std::fs::write(
            &path,
            serde_json::to_vec(&slot_file(&entity, &first, &second)).expect("encode"),
        )
        .expect("write");
        let cache = load_from(&path).expect("load_from");
        match cache.lookup(&entity, &EpisodeKey::None, BrickField::Artwork) {
            BrickLookup::Found(got) => assert_eq!(
                got.value,
                BrickValue::Text {
                    text: "https://example.com/v2.jpg".to_string(),
                },
                "same-source duplicates must be last-wins"
            ),
            other => panic!("collapsed slot must serve Found, got {other:?}"),
        }
    }
}

/// Saving under a file path (an uncreatable parent) reports the failure and
/// leaves the blocker intact with no temp litter -- the `overrides` parity
/// case.
#[test]
fn brick_save_under_an_uncreatable_parent_reports_and_leaves_the_blocker_intact() {
    let scratch = TempDir::new();
    let base = PathBuf::from(scratch.path());
    let blocker = base.join("not-a-dir");
    std::fs::write(&blocker, "keep me").expect("seed blocker");
    let cache = BrickCache::empty_at(blocker.join(BRICK_FILE_NAME));
    let err = cache.save().expect_err("save under a file path must fail");
    assert!(!err.to_string().is_empty(), "the failure is reported");
    assert_eq!(
        std::fs::read_to_string(&blocker).expect("read blocker"),
        "keep me"
    );
    let mut names: Vec<String> = std::fs::read_dir(&base)
        .expect("read scratch dir")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["not-a-dir".to_string()], "temp litter");
}
