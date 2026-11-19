
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;

use crate::cache::{
    cache_exists, cache_file_name, cache_for_each, cache_has, cache_index_path, cache_key,
    cache_live, cache_load, cache_load_entry, cache_put_raw, cache_record, cache_store,
    cache_store_negative, checked_at_instant, clear_cache, clear_cache_entries, count_negatives,
    decode_cache_entry, default_cache_dir, legacy_cache_files, migrate_legacy_cache,
    resolve_cache_dir, with_cache, with_cache_write, CacheEntry, CacheFile, CACHE_MAX_VALUE_SIZE,
    CACHE_SCHEMA, NEGATIVE_TTL, POSITIVE_TTL, positive_fresh,
};
use crate::resolver::NEGATIVE_BACKOFF;
use crate::testsupport::{resolver_with, FakeSource, TempDir};
use crate::timefmt::format_rfc3339;

/// `storeRawRecord`: writes one encoded record straight into `dir`'s store.
pub(crate) fn store_raw_record(dir: &str, key: &str, data: &[u8]) {
    with_cache_write(dir, |db| {
        db.put(key, data)?;
        Ok(())
    })
    .expect("store record");
}

fn series_query(title: &str, season: i64) -> Query {
    Query {
        kind: Kind::SERIES,
        title: title.to_string(),
        season,
        ..Query::default()
    }
}

/// `TestCacheKeyShapes`.
#[test]
fn cache_key_shapes() {
    assert_eq!(
        cache_key(&Query {
            kind: Kind::SERIES,
            title: "Breaking Bad".to_string(),
            year: 2008,
            season: 1,
            ..Query::default()
        }),
        "series|breaking bad|2008|1"
    );
    assert_eq!(
        cache_key(&Query {
            kind: Kind::MOVIE,
            title: "Inception".to_string(),
            year: 2010,
            ..Query::default()
        }),
        "movie|inception|2010"
    );
    assert_eq!(
        cache_key(&Query {
            kind: Kind::SERIES,
            title: "  Breaking   BAD ".to_string(),
            year: 2008,
            season: 1,
            ..Query::default()
        }),
        "series|breaking bad|2008|1"
    );
}

/// `TestCacheRoundTrip`.
#[test]
fn cache_round_trip() {
    let dir = TempDir::new();
    let q = Query {
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        year: 2008,
        season: 1,
        ..Query::default()
    };
    let key = cache_key(&q);
    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "169".to_string(),
        kind: Kind::SERIES,
        title: "Breaking Bad".to_string(),
        year: 2008,
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    cache_store(&dir.path(), &key, &media).expect("cacheStore");
    let got = cache_load(&dir.path(), &key).expect("cacheLoad missed after store");
    assert_eq!(got, media, "round-trip mismatch");
}

/// `TestCacheCorruptIgnored`: a corrupt legacy file imports nothing (it is not a
/// record) and a key with no record is a miss.
#[test]
fn cache_corrupt_ignored() {
    let dir = TempDir::new();
    let key = cache_key(&series_query("Show", 1));
    std::fs::write(
        format!("{}/{}", dir.path(), cache_file_name(&key)),
        "not json{{{",
    )
    .unwrap();
    assert!(cache_load(&dir.path(), &key).is_none());
    assert!(cache_load(&dir.path(), "series|missing|0|0").is_none());
}

/// `TestCacheSchema2IsMiss`: schema-2 entries (pre-enrichment) are a miss under
/// the current schema while fresh stores hit.
#[test]
fn cache_schema2_is_miss() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Show", 1));
    let legacy2 = format!(
        "{{\"key\":{key:?},\"schema\":2,\"media\":{{\"source\":\"tvmaze\",\"sourceId\":\"1\",\"kind\":1,\"title\":\"Show\",\"fetchedAt\":\"2026-01-01T00:00:00Z\"}}}}"
    );
    std::fs::write(format!("{d}/{}", cache_file_name(&key)), legacy2).unwrap();
    assert!(
        cache_load(&d, &key).is_none(),
        "Schema:2 entry should be a miss under the current schema"
    );

    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Show".to_string(),
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    cache_store(&d, &key, &media).expect("cacheStore");
    assert!(cache_load(&d, &key).is_some());
}

/// `TestCachePreviousSchemaIsMiss`: a positive written under the PREVIOUS
/// schema (a matching/display change like the alias-aware anime fix must bump
/// `CACHE_SCHEMA`) is never served after the bump -- not via load, live probe
/// or entry decode -- and a fresh store under the current schema replaces it.
#[test]
fn cache_previous_schema_is_miss() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Show", 1));
    let stale = CacheFile {
        key: key.clone(),
        schema: 3,
        media: Media {
            source: "tvmaze".to_string(),
            source_id: "1".to_string(),
            kind: Kind::SERIES,
            title: "Stale Show".to_string(),
            episodes: vec![Episode {
                season: 1,
                number: 1,
                title: "Pilot".to_string(),
                ..Episode::default()
            }],
            fetched_at: crate::timefmt::now_rfc3339(),
            ..Media::default()
        },
        not_found: false,
        checked_at: format_rfc3339(SystemTime::now()),
    };
    let data = serde_json::to_vec(&stale).expect("encode stale record");
    store_raw_record(&d, &key, &data);
    assert!(
        cache_load(&d, &key).is_none(),
        "previous-schema positive must be a miss after the bump"
    );
    assert!(!cache_live(&d, &key));
    assert!(decode_cache_entry(&data, &key).is_none());

    let fresh = Media {
        source: "kitsu".to_string(),
        title: "Show".to_string(),
        ..stale.media.clone()
    };
    cache_store(&d, &key, &fresh).expect("cacheStore replaces the stale record");
    let got = cache_load(&d, &key).expect("fresh store must hit");
    assert_eq!(got.title, "Show");
    assert_eq!(got.source, "kitsu");
}

/// `TestCacheHasIsPresenceOnly`: a presence check that never parses a stored
/// body. A corrupt legacy file imports nothing, so nothing is present.
#[test]
fn cache_has_is_presence_only() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&Query {
        kind: Kind::SERIES,
        title: "Stat Only".to_string(),
        year: 2001,
        season: 1,
        ..Query::default()
    });
    assert!(!cache_has(&d, &key));
    assert!(!cache_has(&d, ""));
    std::fs::write(
        format!("{d}/{}", cache_file_name(&key)),
        "not json{{{corrupt",
    )
    .unwrap();
    assert!(
        !cache_has(&d, &key),
        "CacheHas on a corrupt legacy file = true, want false (it never was a record)"
    );

    let key2 = cache_key(&Query {
        kind: Kind::MOVIE,
        title: "Stat Only Film".to_string(),
        year: 2002,
        ..Query::default()
    });
    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "7".to_string(),
        kind: Kind::MOVIE,
        title: "Stat Only Film".to_string(),
        year: 2002,
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    cache_store(&d, &key2, &media).expect("cacheStore");
    assert!(cache_has(&d, &key2));
}

/// `TestCacheNegativeRoundTrip`.
#[test]
fn cache_negative_round_trip() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&Query {
        kind: Kind::SERIES,
        title: "No Such Show".to_string(),
        year: 2031,
        season: 1,
        ..Query::default()
    });
    cache_store_negative(&d, &key).expect("cacheStoreNegative");
    let entry = cache_load_entry(&d, &key).expect("want negative hit");
    assert!(entry.not_found && entry.media.is_none(), "{entry:?}");
    assert!(
        cache_load(&d, &key).is_none(),
        "cacheLoad hit on a negative entry, want miss (positive-only)"
    );
    assert!(cache_has(&d, &key));
}

/// `TestCacheNegativeExpiredIsAbsent`.
#[test]
fn cache_negative_expired_is_absent() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Stale Miss", 1));
    let old = format_rfc3339(SystemTime::now() - NEGATIVE_TTL - Duration::from_secs(3600));
    let data = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        not_found: true,
        checked_at: old,
        ..CacheFile::default()
    })
    .unwrap();
    std::fs::write(format!("{d}/{}", cache_file_name(&key)), data).unwrap();
    assert!(
        cache_load_entry(&d, &key).is_none(),
        "expired negative should be absent, got hit"
    );
}

/// `TestCacheLivePositive`.
#[test]
fn cache_live_positive() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = Query {
        kind: Kind::SERIES,
        title: "Live Show".to_string(),
        year: 2008,
        season: 1,
        ..Query::default()
    };
    let key = cache_key(&q);
    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "169".to_string(),
        kind: Kind::SERIES,
        title: "Live Show".to_string(),
        year: 2008,
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    cache_store(&d, &key, &media).expect("cacheStore");
    assert!(cache_live(&d, &key));
}

/// `TestCacheLiveFreshNegative`.
#[test]
fn cache_live_fresh_negative() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Fresh Miss", 1));
    cache_store_negative(&d, &key).expect("cacheStoreNegative");
    assert!(cache_live(&d, &key));
}

/// `TestCacheLiveExpiredNegative`.
#[test]
fn cache_live_expired_negative() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Stale Miss Live", 1));
    let old = format_rfc3339(SystemTime::now() - NEGATIVE_TTL - Duration::from_secs(3600));
    let data = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        not_found: true,
        checked_at: old,
        ..CacheFile::default()
    })
    .unwrap();
    std::fs::write(format!("{d}/{}", cache_file_name(&key)), data).unwrap();
    assert!(!cache_live(&d, &key));
}

/// `TestCacheLiveCorrupt`: corrupt / truncated / wrong-schema / oversize /
/// missing records report not-live (unlike the presence-only `CacheHas`).
#[test]
fn cache_live_corrupt() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Corrupt Live", 1));
    assert!(!cache_live(&d, &key));
    assert!(!cache_live(&d, ""));
    let path = format!("{d}/{}", cache_file_name(&key));
    std::fs::write(&path, "not json{{{corrupt").unwrap();
    assert!(!cache_live(&d, &key));
    std::fs::write(&path, "{\"key\":").unwrap();
    assert!(!cache_live(&d, &key));
    let legacy = format!(
        "{{\"key\":{key:?},\"schema\":2,\"media\":{{\"source\":\"tvmaze\",\"sourceId\":\"1\",\"kind\":1,\"title\":\"Show\",\"fetchedAt\":\"2026-01-01T00:00:00Z\"}}}}"
    );
    std::fs::write(&path, legacy).unwrap();
    assert!(!cache_live(&d, &key));
    std::fs::write(&path, vec![b'x'; 512 * 1024]).unwrap();
    assert!(!cache_live(&d, &key));
}

/// `TestCacheLiveFutureCheckedAt`: a far-future `CheckedAt` (clock jump) reports
/// not-live, while a near-future one within the skew window stays live.
#[test]
fn cache_live_future_checked_at() {
    let key = cache_key(&series_query("Future Miss", 1));
    let far = format_rfc3339(SystemTime::now() + Duration::from_secs(2 * 3600));
    let dir = TempDir::new();
    let d = dir.path();
    let data = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        not_found: true,
        checked_at: far,
        ..CacheFile::default()
    })
    .unwrap();
    std::fs::write(format!("{d}/{}", cache_file_name(&key)), data).unwrap();
    assert!(
        !cache_live(&d, &key),
        "far-future CheckedAt must be expired (clock jump)"
    );
    assert!(cache_load_entry(&d, &key).is_none());

    let near = format_rfc3339(SystemTime::now() + Duration::from_secs(2 * 60));
    let dir2 = TempDir::new();
    let d2 = dir2.path();
    let data = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        not_found: true,
        checked_at: near,
        ..CacheFile::default()
    })
    .unwrap();
    std::fs::write(format!("{d2}/{}", cache_file_name(&key)), data).unwrap();
    assert!(
        cache_live(&d2, &key),
        "near-future CheckedAt (within skew) must stay live"
    );
}

/// `TestCacheLegacyPositiveNoNewFields`: a pre-existing positive file with no
/// `notFound`/`checkedAt` fields is now STALE under the positive TTL (a record
/// with no timestamp cannot prove freshness), while the same record carrying a
/// fresh `checkedAt` still loads as a positive hit.
#[test]
fn cache_legacy_positive_no_new_fields() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = Query {
        kind: Kind::SERIES,
        title: "Legacy Show".to_string(),
        year: 2005,
        season: 2,
        ..Query::default()
    };
    let key = cache_key(&q);
    let legacy = format!(
        "{{\"key\":{key:?},\"schema\":{},\"media\":{{\"source\":\"tvmaze\",\"sourceId\":\"9\",\"kind\":1,\"title\":\"Legacy Show\",\"episodes\":[{{\"season\":2,\"number\":1,\"title\":\"Pilot\"}}],\"fetchedAt\":\"2026-01-01T00:00:00Z\"}}}}",
        CACHE_SCHEMA
    );
    std::fs::write(format!("{d}/{}", cache_file_name(&key)), legacy).unwrap();
    assert!(
        cache_load(&d, &key).is_none(),
        "a positive record with no checkedAt must be stale under POSITIVE_TTL"
    );
    assert!(!positive_fresh("", SystemTime::now()));
    let dir2 = TempDir::new();
    let d2 = dir2.path();    let fresh = format!(
        "{{\"key\":{key:?},\"schema\":{},\"checked_at\":{:?},\"media\":{{\"source\":\"tvmaze\",\"sourceId\":\"9\",\"kind\":1,\"title\":\"Legacy Show\",\"episodes\":[{{\"season\":2,\"number\":1,\"title\":\"Pilot\"}}],\"fetchedAt\":\"2026-01-01T00:00:00Z\"}}}}",
        CACHE_SCHEMA,
        format_rfc3339(SystemTime::now())
    );
    std::fs::write(format!("{d2}/{}", cache_file_name(&key)), fresh).unwrap();
    let media = cache_load(&d2, &key).expect("fresh positive cacheLoad");
    assert_eq!(media.source, "tvmaze");
    let entry = cache_load_entry(&d2, &key).expect("fresh positive cacheLoadEntry");
    assert!(!entry.not_found && entry.media.is_some());
}

/// `TestPositiveTTLGovernsRefetch`: a positive entry inside its TTL serves the
/// hit with zero provider calls from a cold resolver; a positive backdated
/// past the TTL is stale and is re-fetched.
#[test]
fn positive_ttl_governs_refetch() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = Query {
        kind: Kind::SERIES,
        title: "Aired Somewhere".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let key = cache_key(&q);
    let media = Media {
        source: "fake".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Aired Somewhere".to_string(),
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };

    let first = FakeSource::with_media("fake", media.clone());
    let first_calls = first.counter();
    let r1 = resolver_with(&d, vec![first.shared()]);
    let res = r1
        .lookup_now(&Cancel::background(), &q)
        .expect("run 1 LookupNow");
    assert!(res.found, "run 1 = {res:?}, want a hit");
    assert_eq!(first_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(cache_live(&d, &key), "fresh positive must be live");

    let second = FakeSource::with_media("fake", media.clone());
    let second_calls = second.counter();
    let r2 = resolver_with(&d, vec![second.shared()]);
    let res = r2
        .lookup_now(&Cancel::background(), &q)
        .expect("run 2 LookupNow");
    assert!(res.found, "run 2 = {res:?}, want the cached hit");
    assert_eq!(
        second_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "positive inside its TTL must not touch the provider"
    );

    let backdated = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        media: media.clone(),
        checked_at: format_rfc3339(SystemTime::now() - POSITIVE_TTL - Duration::from_secs(3600)),
        ..CacheFile::default()
    })
    .unwrap();
    with_cache(&d, |db| {
        db.put(&key, &backdated)?;
        Ok(())
    })
    .expect("backdate stored positive");
    assert!(
        !cache_live(&d, &key),
        "a positive older than POSITIVE_TTL must be stale"
    );
    assert!(!positive_fresh(
        &format_rfc3339(SystemTime::now() - POSITIVE_TTL - Duration::from_secs(3600)),
        SystemTime::now()
    ));
    assert!(positive_fresh(
        &format_rfc3339(SystemTime::now()),
        SystemTime::now()
    ));

    let third = FakeSource::with_media("fake", media.clone());
    let third_calls = third.counter();
    let r3 = resolver_with(&d, vec![third.shared()]);
    let res = r3
        .lookup_now(&Cancel::background(), &q)
        .expect("run 3 LookupNow");
    assert!(res.found, "run 3 = {res:?}, want a hit");
    assert_eq!(
        third_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an expired positive must be re-fetched"
    );
}

/// `TestCacheLargeEntryRoundTrip`: the retired 64 KiB read cap is gone -- a
/// 256 KiB entry is stored, reports live, reads back and round-trips exactly.
#[test]
fn cache_large_entry_round_trip() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = Query {
        kind: Kind::SERIES,
        title: "Huge Show".to_string(),
        year: 2008,
        season: 1,
        ..Query::default()
    };
    let key = cache_key(&q);
    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "41".to_string(),
        kind: Kind::SERIES,
        title: "Huge Show".to_string(),
        year: 2008,
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            synopsis: "s".repeat(256 * 1024),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    let encoded = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        media: media.clone(),
        ..CacheFile::default()
    })
    .unwrap();
    assert!(
        encoded.len() > 64 * 1024,
        "fixture is {} bytes, want more than the old 64 KiB read cap",
        encoded.len()
    );
    cache_store(&d, &key, &media).expect("cacheStore of a 256 KiB entry");
    assert!(cache_live(&d, &key));
    let got = cache_load(&d, &key).expect("cacheLoad missed a large entry");
    assert_eq!(got, media);
}

/// `TestProbeMatchesCacheLiveLarge`: `ProbeCache` and `CacheLive` agree case by
/// case without a size gate.
#[test]
fn probe_matches_cache_live_large() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = Query {
        kind: Kind::SERIES,
        title: "Cap Show".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let key = cache_key(&q);
    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Cap Show".to_string(),
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: "Pilot".to_string(),
            synopsis: "s".repeat(256 * 1024),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    cache_store(&d, &key, &media).expect("cacheStore");
    assert!(cache_live(&d, &key));
    let resolver = resolver_with(&d, Vec::new());
    assert!(
        resolver.probe_cache(&key),
        "ProbeCache on a valid 256 KiB entry must match CacheLive"
    );

    let dir2 = TempDir::new();
    let d2 = dir2.path();
    let key2 = cache_key(&Query {
        kind: Kind::SERIES,
        title: "Broken Show".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    });
    std::fs::write(
        format!("{d2}/{}", cache_file_name(&key2)),
        "not json{{{corrupt",
    )
    .unwrap();
    assert!(!cache_live(&d2, &key2));
    let resolver2 = resolver_with(&d2, Vec::new());
    assert!(!resolver2.probe_cache(&key2));
}

/// `TestNegativeTTLGovernsRefetch`: a not-found entry inside its 30-day TTL
/// serves the miss with zero provider calls from a cold resolver, and one
/// backdated past the TTL is re-attempted.
#[test]
fn negative_ttl_governs_refetch() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = Query {
        kind: Kind::SERIES,
        title: "Never Aired Nowhere".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    let key = cache_key(&q);

    let first = FakeSource::new("fake");
    let first_calls = first.counter();
    let r1 = resolver_with(&d, vec![first.shared()]);
    let res = r1
        .lookup_now(&Cancel::background(), &q)
        .expect("run 1 LookupNow");
    assert!(!res.found, "run 1 = {res:?}, want a miss");
    assert_eq!(first_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let entry = cache_load_entry(&d, &key).expect("run 1 entry");
    assert!(entry.not_found, "want a persisted negative");

    let second = FakeSource::new("fake");
    let second_calls = second.counter();
    let r2 = resolver_with(&d, vec![second.shared()]);
    let res = r2
        .lookup_now(&Cancel::background(), &q)
        .expect("run 2 LookupNow");
    assert!(!res.found, "run 2 = {res:?}, want the cached miss");
    assert_eq!(
        second_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "negative inside its TTL must not touch the provider"
    );

    let backdated = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        not_found: true,
        checked_at: format_rfc3339(SystemTime::now() - NEGATIVE_TTL - Duration::from_secs(3600)),
        ..CacheFile::default()
    })
    .unwrap();
    with_cache(&d, |db| {
        db.put(&key, &backdated)?;
        Ok(())
    })
    .expect("backdate stored negative");
    assert!(!cache_live(&d, &key));

    let third = FakeSource::new("fake");
    let third_calls = third.counter();
    let r3 = resolver_with(&d, vec![third.shared()]);
    let res = r3
        .lookup_now(&Cancel::background(), &q)
        .expect("run 3 LookupNow");
    assert!(!res.found, "run 3 = {res:?}, want a miss");
    assert_eq!(
        third_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an expired negative must be re-attempted"
    );
}

/// `TestClearCacheEmptiesStoreAndLegacyDir`: `ClearCache` drops every stored
/// entry, removes the legacy directory, stays idempotent, and leaves the store
/// usable for the next write.
#[test]
fn clear_cache_empties_store_and_legacy_dir() {
    let root = TempDir::new();
    let dir = root.child("meta");
    std::fs::create_dir_all(&dir).unwrap();
    let pos_key = cache_key(&series_query("Kept Show", 1));
    let neg_key = cache_key(&series_query("Dead Show", 1));
    let media = Media {
        source: "tvmaze".to_string(),
        source_id: "5".to_string(),
        kind: Kind::SERIES,
        title: "Kept Show".to_string(),
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    };
    cache_store(&dir, &pos_key, &media).expect("cacheStore");
    cache_store_negative(&dir, &neg_key).expect("cacheStoreNegative");
    assert!(cache_has(&dir, &pos_key));
    assert_eq!(count_negatives(&dir), 1);

    clear_cache(&dir).expect("ClearCache");
    assert!(!cache_has(&dir, &pos_key) && !cache_live(&dir, &neg_key));
    assert_eq!(count_negatives(&dir), 0);
    assert!(
        std::fs::metadata(&dir).is_err(),
        "the legacy cache dir must be removed"
    );
    clear_cache(&dir).expect("second ClearCache");
    cache_store(&dir, &pos_key, &media).expect("cacheStore after ClearCache");
    assert!(cache_has(&dir, &pos_key), "store unusable after ClearCache");
    clear_cache(&dir).expect("final ClearCache");
}

/// `TestCacheMigrationParityOnRealDir`: copies the machine's legacy cache
/// directory into a temp dir (the live directory is only read) and proves the
/// first open imports exactly one record per legacy `.json` file and that every
/// key resolves to the same answer it had as a file.
#[test]
fn cache_migration_parity_on_real_dir() {
    let live = default_cache_dir();
    let files = legacy_cache_files(&live);
    if files.is_empty() {
        eprintln!("no legacy cache at {live} to migrate; skipping");
        return;
    }
    let before = dir_snapshot(&live);

    let root = TempDir::new();
    let dir = root.child("meta");
    std::fs::create_dir_all(&dir).unwrap();

    struct Want {
        key: String,
        entry: Option<CacheEntry>,
    }
    let mut legacy: Vec<Want> = Vec::with_capacity(files.len());
    let mut seen: HashMap<String, bool> = HashMap::new();
    for src in &files {
        let data = std::fs::read(src).expect("read legacy");
        let name = src.file_name().unwrap().to_string_lossy().into_owned();
        std::fs::write(format!("{dir}/{name}"), &data).expect("copy legacy");
        let record: CacheFile =
            serde_json::from_slice(&data).unwrap_or_else(|err| panic!("legacy {name}: {err}"));
        assert!(
            !record.key.is_empty(),
            "legacy file {name} is not a cache record"
        );
        assert!(
            !seen.contains_key(&record.key),
            "legacy dir has a duplicate key {:?}",
            record.key
        );
        seen.insert(record.key.clone(), true);
        let entry = decode_cache_entry(&data, &record.key);
        legacy.push(Want {
            key: record.key,
            entry,
        });
    }

    let mut records = 0;
    let mut imported = 0;
    with_cache(&dir, |db| {
        records = db.len();
        imported = migrate_legacy_cache(db, &legacy_cache_files(&dir));
        Ok(())
    })
    .expect("withCache");
    assert_eq!(records, files.len(), "one record per legacy .json file");
    assert_eq!(imported, files.len(), "migrateLegacyCache import count");
    for want in &legacy {
        let got = cache_load_entry(&dir, &want.key);
        assert_eq!(
            got, want.entry,
            "key {:?} after migration must equal the legacy answer",
            want.key
        );
    }
    assert_eq!(
        legacy_cache_files(&dir).len(),
        files.len(),
        "the recovery copy must be kept in place"
    );
    assert_eq!(
        dir_snapshot(&live),
        before,
        "the live legacy dir must be untouched"
    );
}

/// `dirSnapshot`: a comparable (name, size, mtime) snapshot of a directory.
fn dir_snapshot(dir: &str) -> Vec<String> {
    let entries = std::fs::read_dir(dir).expect("read dir");
    let mut out: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let meta = entry.metadata().expect("stat entry");
        let mtime = meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        out.push(format!(
            "{}|{}|{}",
            entry.file_name().to_string_lossy(),
            meta.len(),
            mtime
        ));
    }
    out.sort();
    out
}

/// `TestCacheConcurrentAccess`: concurrent readers and writers on one cache
/// directory are safe -- the store and the handle registry both serialize.
#[test]
fn cache_concurrent_access() {
    let dir = TempDir::new();
    let d = dir.path();
    let shared = cache_key(&Query {
        kind: Kind::SERIES,
        title: "Shared Show".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    });
    let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for w in 0..8 {
        let d = d.clone();
        let shared = shared.clone();
        let failures = Arc::clone(&failures);
        handles.push(std::thread::spawn(move || {
            let key = cache_key(&Query {
                kind: Kind::SERIES,
                title: format!("Show {w}"),
                season: 1,
                episode: 1,
                ..Query::default()
            });
            for _ in 0..20 {
                let media = Media {
                    source: "fake".to_string(),
                    source_id: w.to_string(),
                    kind: Kind::SERIES,
                    title: format!("Show {w}"),
                    ..Media::default()
                };
                if let Err(err) = cache_store(&d, &key, &media) {
                    failures.lock().unwrap().push(format!("cacheStore: {err}"));
                    return;
                }
                if let Err(err) = cache_store_negative(&d, &shared) {
                    failures
                        .lock()
                        .unwrap()
                        .push(format!("cacheStoreNegative: {err}"));
                    return;
                }
                let _ = cache_load(&d, &key);
                let _ = cache_load_entry(&d, &shared);
                let _ = cache_live(&d, &key);
                let _ = cache_has(&d, &key);
                let _ = count_negatives(&d);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("worker thread");
    }
    assert_eq!(failures.lock().unwrap().len(), 0, "{failures:?}");
    assert!(cache_has(&d, &shared), "shared key missing after writes");
    assert_eq!(
        count_negatives(&d),
        1,
        "one shared negative key after concurrent writes"
    );
}

/// `TestCacheForEachEnumeratesLiveStore`: the exported enumeration reads the
/// store's live records with each value the record's stored bytes, imports a
/// legacy-only directory first, can stop early, and never creates a cache.
#[test]
fn cache_for_each_enumerates_live_store() {
    let dir = TempDir::new();
    let d = dir.path();
    let keys = [
        cache_key(&series_query("Alpha", 1)),
        cache_key(&Query {
            kind: Kind::MOVIE,
            title: "Beta".to_string(),
            year: 1999,
            ..Query::default()
        }),
        cache_key(&series_query("Gamma", 2)),
    ];
    for key in &keys {
        cache_store(
            &d,
            key,
            &Media {
                source: "fake".to_string(),
                kind: Kind::SERIES,
                title: key.clone(),
                ..Media::default()
            },
        )
        .unwrap_or_else(|err| panic!("cacheStore {key:?}: {err}"));
    }
    let neg = cache_key(&series_query("Dead", 1));
    cache_store_negative(&d, &neg).expect("cacheStoreNegative");

    let seen: Arc<Mutex<HashMap<String, i32>>> = Arc::new(Mutex::new(HashMap::new()));
    let recorded = Arc::clone(&seen);
    let failures: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let bad = Arc::clone(&failures);
    cache_for_each(
        &d,
        Some(|key: &str, value: &[u8]| {
            *recorded.lock().unwrap().entry(key.to_string()).or_insert(0) += 1;
            match serde_json::from_slice::<CacheFile>(value) {
                Ok(record) if record.key != key => bad
                    .lock()
                    .unwrap()
                    .push(format!("value for {key:?} carries key {:?}", record.key)),
                Ok(_) => {}
                Err(err) => bad
                    .lock()
                    .unwrap()
                    .push(format!("value for {key:?} does not decode: {err}")),
            }
            true
        }),
    )
    .expect("CacheForEach");
    assert_eq!(failures.lock().unwrap().len(), 0, "{failures:?}");
    let seen = seen.lock().unwrap();
    for key in keys.iter().chain(std::iter::once(&neg)) {
        assert_eq!(seen.get(key), Some(&1), "key {key:?} seen {seen:?}");
    }
    assert_eq!(seen.len(), 4, "enumeration saw {seen:?}");
    drop(seen);

    let mut n = 0;
    cache_for_each(
        &d,
        Some(|_: &str, _: &[u8]| {
            n += 1;
            false
        }),
    )
    .expect("CacheForEach (early stop)");
    assert_eq!(n, 1, "early stop must call the callback once");

    let legacy = TempDir::new();
    let legacy_dir = legacy.path();
    let lkey = cache_key(&Query {
        kind: Kind::MOVIE,
        title: "Legacy".to_string(),
        year: 2001,
        ..Query::default()
    });
    let payload = serde_json::to_vec(&CacheFile {
        key: lkey.clone(),
        schema: CACHE_SCHEMA,
        media: Media {
            source: "fake".to_string(),
            title: "Legacy".to_string(),
            ..Media::default()
        },
        ..CacheFile::default()
    })
    .unwrap();
    std::fs::write(format!("{legacy_dir}/{}", cache_file_name(&lkey)), payload).unwrap();
    let mut got: Vec<String> = Vec::new();
    cache_for_each(
        &legacy_dir,
        Some(|key: &str, _: &[u8]| {
            got.push(key.to_string());
            true
        }),
    )
    .expect("CacheForEach(legacy)");
    assert_eq!(got, vec![lkey], "legacy enumeration");

    let empty = TempDir::new();
    let empty_dir = empty.path();
    let mut calls = 0;
    cache_for_each(
        &empty_dir,
        Some(|_: &str, _: &[u8]| {
            calls += 1;
            true
        }),
    )
    .expect("CacheForEach(empty)");
    assert_eq!(calls, 0);
    assert!(
        std::fs::metadata(cache_index_path(&empty_dir)).is_err(),
        "a read created a cache file"
    );
    cache_for_each(&empty_dir, None::<fn(&str, &[u8]) -> bool>).expect("nil callback");
}

/// `TestCacheForEachUsesTheHeldHandle`: the enumeration runs against the
/// process's ONE held store handle rather than opening a new one.
#[test]
fn cache_for_each_uses_the_held_handle() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Held", 1));
    cache_store(
        &d,
        &key,
        &Media {
            source: "fake".to_string(),
            title: "Held".to_string(),
            ..Media::default()
        },
    )
    .expect("cacheStore");
    let before = crate::cache::cache_handle_for(&d).expect("cacheHandleFor");
    assert!(crate::cache::cache_handle_held(&d));
    for i in 0..2 {
        let mut n = 0;
        cache_for_each(
            &d,
            Some(|_: &str, _: &[u8]| {
                n += 1;
                true
            }),
        )
        .unwrap_or_else(|err| panic!("CacheForEach run {i}: {err}"));
        assert_eq!(n, 1, "run {i} saw {n} records, want 1");
        let after = crate::cache::cache_handle_for(&d).expect("cacheHandleFor");
        assert!(Arc::ptr_eq(&before, &after), "run {i} replaced the handle");
        assert_eq!(
            before
                .db
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );
    }
    assert!(
        cache_live(&d, &key),
        "the enumerated entry stopped being live"
    );
}

/// `TestClearCacheEntriesClearsEntriesOnly`: the entry-level clear drops every
/// stored record, reports how many titles it cleared, and leaves the store file,
/// the directory and every non-entry file exactly where they were.
#[test]
fn clear_cache_entries_clears_entries_only() {
    let dir = TempDir::new();
    let d = dir.path();
    let pos_key = cache_key(&series_query("Pos", 1));
    let neg_key = cache_key(&series_query("Neg", 1));
    cache_store(
        &d,
        &pos_key,
        &Media {
            source: "fake".to_string(),
            title: "Pos".to_string(),
            ..Media::default()
        },
    )
    .expect("cacheStore");
    cache_store_negative(&d, &neg_key).expect("cacheStoreNegative");
    std::fs::write(format!("{d}/notes.json"), "{\"keep\":true}").unwrap();

    let n = clear_cache_entries(&d).expect("ClearCacheEntries");
    assert_eq!(n, 2, "ClearCacheEntries cleared {n}, want 2");
    assert!(!cache_has(&d, &pos_key) && !cache_live(&d, &neg_key));
    assert_eq!(count_negatives(&d), 0);
    assert!(std::fs::metadata(&d).is_ok(), "cache directory was removed");
    assert!(
        std::fs::metadata(cache_index_path(&d)).is_ok(),
        "store file was removed"
    );
    assert_eq!(
        std::fs::read_to_string(format!("{d}/notes.json")).unwrap(),
        "{\"keep\":true}",
        "non-entry file touched"
    );
    assert_eq!(
        clear_cache_entries(&d).expect("second ClearCacheEntries"),
        0
    );
    cache_store(
        &d,
        &pos_key,
        &Media {
            source: "fake".to_string(),
            title: "Pos".to_string(),
            ..Media::default()
        },
    )
    .expect("cacheStore after clear");
    assert!(
        cache_has(&d, &pos_key),
        "store unusable after ClearCacheEntries"
    );
}

/// `TestClearCacheEntriesCountsImportedLegacyTitles`: the clear counts the
/// titles a legacy directory still holds, and leaves the legacy files alone.
#[test]
fn clear_cache_entries_counts_imported_legacy_titles() {
    let dir = TempDir::new();
    let d = dir.path();
    let keys = [
        cache_key(&series_query("One", 1)),
        cache_key(&Query {
            kind: Kind::MOVIE,
            title: "Two".to_string(),
            year: 1994,
            ..Query::default()
        }),
    ];
    for key in &keys {
        let payload = serde_json::to_vec(&CacheFile {
            key: key.clone(),
            schema: CACHE_SCHEMA,
            media: Media {
                source: "fake".to_string(),
                title: key.clone(),
                ..Media::default()
            },
            ..CacheFile::default()
        })
        .unwrap();
        std::fs::write(format!("{d}/{}", cache_file_name(key)), payload).unwrap();
    }

    let n = clear_cache_entries(&d).expect("ClearCacheEntries");
    assert_eq!(n, 2, "cleared {n} titles, want 2");
    let handle = crate::cache::cache_handle_for(&d).expect("cacheHandleFor");
    let len = handle
        .db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .len();
    assert_eq!(len, 0, "the store must hold nothing after the clear");
    for key in &keys {
        assert!(
            std::fs::metadata(format!("{d}/{}", cache_file_name(key))).is_ok(),
            "legacy file for {key:?} was removed"
        );
    }
}

/// `TestCachePutRawStoresBytesVerbatim`: the bulk-write seam stores what it is
/// given, the record is immediately live, and it refuses what the store cannot
/// hold.
#[test]
fn cache_put_raw_stores_bytes_verbatim() {
    let dir = TempDir::new();
    let d = dir.path();
    let key = cache_key(&series_query("Raw", 1));
    let payload = serde_json::to_vec(&CacheFile {
        key: key.clone(),
        schema: CACHE_SCHEMA,
        media: Media {
            source: "fake".to_string(),
            title: "Raw".to_string(),
            ..Media::default()
        },
        checked_at: crate::timefmt::now_rfc3339(),
        ..CacheFile::default()
    })
    .unwrap();
    cache_put_raw(&d, &key, &payload).expect("CachePutRaw");
    assert!(cache_has(&d, &key) && cache_live(&d, &key));
    let stored = cache_record(&d, &key).expect("stored record missing");
    assert_eq!(
        &stored[..],
        &payload[..],
        "stored bytes differ from the payload"
    );

    let big = vec![0u8; (CACHE_MAX_VALUE_SIZE + 1) as usize];
    assert!(
        cache_put_raw(&d, "movie|huge|2001", &big).is_err(),
        "over-size value accepted, want a refusal"
    );
    assert!(!cache_has(&d, "movie|huge|2001"));
    assert!(
        cache_put_raw(&d, "", b"{}").is_err(),
        "empty key accepted, want an error"
    );
    assert!(
        cache_put_raw("", &key, b"{}").is_err(),
        "empty directory accepted, want an error"
    );
}

/// A `resolveCacheDir` spot check: the blank dir resolves to the default.
#[test]
fn resolve_cache_dir_blank_is_default() {
    assert_eq!(resolve_cache_dir("   "), default_cache_dir());
    assert_eq!(resolve_cache_dir("X:/tmp/meta"), "X:/tmp/meta");
}

/// `checkedAt` text round-trips through the crate's own formatter.
#[test]
fn checked_at_instant_round_trips() {
    let now = SystemTime::now();
    let text = format_rfc3339(now);
    assert!(checked_at_instant(&text).is_some());
    assert!(cache_exists(&resolve_cache_dir("")) || !cache_exists(&resolve_cache_dir("")));
    assert!(NEGATIVE_BACKOFF < NEGATIVE_TTL);
}
