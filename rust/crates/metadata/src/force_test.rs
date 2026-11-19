
use std::time::{Duration, SystemTime};

use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;

use crate::cache::{cache_index_path, cache_key, cache_record};
use crate::testsupport::{FakeSource, TempDir, resolver_with};

/// `cacheEntryState`: the stored record for `key` and the store file's
/// modification time. The cache is ONE store file (`<dir>.idx`) keyed by the raw
/// cache key, so the record's own bytes are the per-key truth and the store
/// file's mtime is the write witness.
fn cache_entry_state(dir: &str, key: &str) -> (Vec<u8>, SystemTime) {
    let data =
        cache_record(dir, key).unwrap_or_else(|| panic!("cache record for {key:?} is missing"));
    let meta = std::fs::metadata(cache_index_path(dir)).expect("stat store file");
    (data.to_vec(), meta.modified().expect("mtime"))
}

fn show_query() -> Query {
    Query {
        kind: Kind::SERIES,
        title: "Show".to_string(),
        season: 1,
        episode: 1,
        ..Query::default()
    }
}

fn media_with(tag: &str) -> Media {
    Media {
        source: "fake".to_string(),
        source_id: "1".to_string(),
        kind: Kind::SERIES,
        title: "Show".to_string(),
        episodes: vec![Episode {
            season: 1,
            number: 1,
            title: tag.to_string(),
            ..Episode::default()
        }],
        fetched_at: crate::timefmt::now_rfc3339(),
        ..Media::default()
    }
}

/// `TestLookupNowForceRewritesCache`: the force path bypasses the cached entry
/// and rewrites it, while a non-force second run is a strict no-op (served from
/// the store: zero fetches, stored record untouched).
#[test]
fn lookup_now_force_rewrites_cache() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = show_query();

    let fake1 = FakeSource::with_media("fake", media_with("V1"));
    let calls1 = fake1.counter();
    let r1 = resolver_with(&d, vec![fake1.shared()]);
    let res1 = r1
        .lookup_now(&Cancel::background(), &q)
        .expect("run 1 LookupNow");
    assert!(
        res1.found && res1.episode == "V1",
        "run 1 = {res1:?}, want V1 found"
    );
    assert_eq!(calls1.load(std::sync::atomic::Ordering::SeqCst), 1);
    let (body1, mtime1) = cache_entry_state(&d, &cache_key(&q));

    let fake2 = FakeSource::with_media("fake", media_with("V2"));
    let calls2 = fake2.counter();
    let r2 = resolver_with(&d, vec![fake2.shared()]);
    let res2 = r2
        .lookup_now(&Cancel::background(), &q)
        .expect("run 2 LookupNow");
    assert!(
        res2.found && res2.episode == "V1",
        "run 2 (non-force) = {res2:?}, want cached V1"
    );
    assert_eq!(
        calls2.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "run 2 fetches must be 0 (store no-op)"
    );
    let (body, mtime) = cache_entry_state(&d, &cache_key(&q));
    assert_eq!(body, body1, "run 2 rewrote the cache entry without force");
    assert_eq!(mtime, mtime1, "run 2 rewrote the store file");

    std::thread::sleep(Duration::from_millis(60)); // separate mtimes
    let res3 = r2
        .lookup_now_force(&Cancel::background(), &q, true)
        .expect("run 3 force LookupNow");
    assert!(
        res3.found && res3.episode == "V2",
        "run 3 (force) = {res3:?}, want refetched V2"
    );
    assert_eq!(
        calls2.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "run 3 fetches must be 1 (forced refetch)"
    );
    let (body3, mtime3) = cache_entry_state(&d, &cache_key(&q));
    assert_ne!(body3, body1, "run 3 left the stored record unchanged");
    assert!(
        mtime3 > mtime1,
        "run 3 did not rewrite the cache entry ({mtime3:?} vs {mtime1:?})"
    );

    let res4 = r2
        .lookup_now_force(&Cancel::background(), &q, false)
        .expect("force=false LookupNow");
    assert!(
        res4.found && res4.episode == "V2",
        "force=false = {res4:?}, want V2"
    );
    assert_eq!(
        calls2.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "fetches after force=false must still be 1"
    );
}

/// `TestLookupNowForceSrcProvenance`: a first non-force lookup fetches
/// (fromCache=false), a repeat is served from cache (fromCache=true), and force
/// always fetches (fromCache=false), while the wrappers keep their results.
#[test]
fn lookup_now_force_src_provenance() {
    let dir = TempDir::new();
    let d = dir.path();
    let q = show_query();
    let fake = FakeSource::with_media("fake", media_with("V1"));
    let calls = fake.counter();
    let resolver = resolver_with(&d, vec![fake.shared()]);

    let (res1, cached1) = resolver
        .lookup_now_force_src(&Cancel::background(), &q, false)
        .expect("first lookup");
    assert!(
        res1.found && !cached1,
        "first lookup = {res1:?} cached={cached1}"
    );
    let (res2, cached2) = resolver
        .lookup_now_force_src(&Cancel::background(), &q, false)
        .expect("repeat lookup");
    assert!(
        res2.found && cached2,
        "repeat lookup = {res2:?} cached={cached2}"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the repeat must be served from cache"
    );
    let (_res3, cached3) = resolver
        .lookup_now_force_src(&Cancel::background(), &q, true)
        .expect("force lookup");
    assert!(!cached3, "force lookup must report cached=false");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the forced refetch must be counted"
    );

    let res = resolver
        .lookup_now(&Cancel::background(), &q)
        .expect("LookupNow");
    assert!(res.found);
    let res = resolver
        .lookup_now_force(&Cancel::background(), &q, false)
        .expect("LookupNowForce(false)");
    assert!(res.found);
}
