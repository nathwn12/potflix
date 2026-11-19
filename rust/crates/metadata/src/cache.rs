//! `metadata::cache` -- mirrors `go\internal\metadata\cache.go`.
//!
//! ONE store file per cache directory (`<dir>.idx`, so
//! `%LOCALAPPDATA%\Potflix\meta.idx` for the default `meta` dir) behind one
//! handle held for the process lifetime, so a read is a map lookup. One entry
//! per key (`series|<show>|<year>|<season>` or `movie|<title>|<year>`);
//! not-found titles are cached as NEGATIVE entries for 30 days, and media
//! answers as positive entries for 30 days ([`POSITIVE_TTL`]): a bounded
//! read-through cache, never indefinite storage.
//! `migrateLegacyCache` imports the retired per-key `meta\*.json` directory
//! once, when the store is empty, and keeps it as the recovery copy;
//! `ClearCache` is the only code that removes it.
//!
//! # Serialization decision (made, not relitigated)
//!
//! Go's `json.Marshal` HTML-escapes `<`, `>` and `&`; `serde_json` does not.
//! This crate accepts serde_json's output for cache records: both directions
//! parse either form, so interop with existing Go-written caches holds, and no
//! byte-identical-refresh test exists. Nothing here depends on the escaping.
//!
//! # Schema gate
//!
//! [`CACHE_SCHEMA`] versions the on-disk envelope; a record whose `schema`
//! differs is a whole-record MISS so art/synopsis are refetched. `CacheHas`
//! deliberately does NOT check the schema -- it is a presence probe, and the
//! caller that needs the schema reads it from the record itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use metadata_core::types::{Kind, Media, Query};
use store::{Db, Options as StoreOptions};

use crate::Error;
use crate::timefmt::{now_rfc3339, parse_rfc3339};

/// `cacheSchema`: the on-disk envelope version.
///
/// Bump this whenever the matching or display-title logic changes what a key
/// SHOULD resolve to (schema 4: alias-aware anime matching, English-first
/// display titles -- pre-fix positives like `Shingeki no Kyojin: Picture
/// Drama` for a romaji AoT query must not keep serving as live hits;
/// schema 5: year-preference coverage judged across aliases, synonyms never
/// displayed as English -- pre-fix positives like `Sailor Moon` for a 2014
/// Crystal query or a synonym arc-name card for `Gintama` must re-resolve;
/// schema 6: year-override coverage judged across canonical titles only --
/// pre-fix positives like the `Golden Kamuy` OVA for a `Monster`/2019 query
/// (admitted by a `"Monster"` synonym) must re-resolve;
/// schema 7: an alias may extend year-override identity only with canonical
/// corroboration -- pre-fix misses like the 1977 Yamato movie for a `Space
/// Battleship Yamato`/1974 query (the corroborated `1650` alias was refused)
/// must re-resolve;
/// schema 8: that corroboration is judged by content token, not token length
/// -- pre-fix misses like the 2009 `Dragon Ball Kai` for a `DBZ Kai`/2014
/// query (the `kai` corroboration of `20635` was rejected by the length
/// floor) must re-resolve;
/// schema 9: season-aware absolute episode matching (S2E1 == 26) plus a
/// Cinemeta catalog seed for a romaji/junk title -- pre-fix misses like
/// `series|shingeki no kyojin|0|2` (and positives answered by the wrong
/// season) must re-resolve).
/// A record whose `schema` differs is a whole-record MISS, a `not_found`
/// record included: a repair that changes what a key SHOULD resolve to ships
/// a bump, so no stale negative outlives the repair (the 30-day
/// [`NEGATIVE_TTL`] only bounds how long a fresh miss stays quiet, it is not
/// the repair clock), and the scan-index skip (`library::scanindex`, gated on
/// `cache_schema()` + `cache_live`) follows the same version, so one bump
/// re-resolves and re-scans together.
pub const CACHE_SCHEMA: i32 = 9;

/// `negativeTTL` bounds how long a persisted not-found entry counts as a cached
/// miss: long enough that rescans stay fast, short enough that a provider
/// improvement is picked up within a month. `force` always re-attempts.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// `positiveTTL` bounds how long a persisted media answer counts as a live
/// hit: the same shape and order as [`NEGATIVE_TTL`], so the store is a
/// bounded read-through cache and never indefinite storage. An expired
/// positive is re-fetched, never re-served.
pub const POSITIVE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// `cacheClockSkew` tolerates a small future drift in a stored `checkedAt`
/// (NTP step, dual-boot clock jump). Further than this in the future is
/// expired, never fresh.
pub const CACHE_CLOCK_SKEW: Duration = Duration::from_secs(5 * 60);

/// `cacheMaxValueSize` bounds one stored record (the largest entry measured on
/// the owner's machine is 231,757 bytes).
pub const CACHE_MAX_VALUE_SIZE: i64 = 1 << 20;

/// `cacheIndexSuffix`: the cache directory plus `.idx`.
pub const CACHE_INDEX_SUFFIX: &str = ".idx";

/// `cacheKeyTitle` lowercases and collapses whitespace for stable cache keys.
/// (Named apart from `match_.rs`'s `normalizeTitle`, which additionally folds
/// punctuation/diacritics for cross-title similarity.)
pub fn cache_key_title(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `cacheKey`: per (show, season) for series, per (movie, year) for movies.
pub fn cache_key(q: &Query) -> String {
    let title = cache_key_title(&q.title);
    if q.kind == Kind::MOVIE {
        format!("movie|{title}|{}", q.year)
    } else {
        format!("series|{title}|{}|{}", q.year, q.season)
    }
}

/// `cacheFileName`: the LEGACY one-file-per-key name, `sha256(key)[:16]` hex
/// plus `.json`. The store keys entries by the raw cache key instead, but this
/// name stays the stable identity of a key for the migration and for the
/// exported [`cache_file_name_for`] (Settings bundle export/import).
pub fn cache_file_name(key: &str) -> String {
    let digest = sha256(key.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{hex}.json")
}

/// `CacheSchema`: the current on-disk cache schema.
pub fn cache_schema() -> i32 {
    CACHE_SCHEMA
}

/// `CacheKeyFor`: the on-disk cache identity for one query.
pub fn cache_key_for(q: &Query) -> String {
    cache_key(q)
}

/// `CacheFileNameFor`: the legacy cache file base name for a cache key.
pub fn cache_file_name_for(key: &str) -> String {
    cache_file_name(key)
}

/// `defaultCacheDir`: `os.UserCacheDir() + \Potflix\meta`, falling back to
/// `<dir of the executable>\meta`.
pub fn default_cache_dir() -> String {
    if let Ok(dir) = std::env::var("LOCALAPPDATA")
        && !dir.is_empty()
    {
        return Path::new(&dir)
            .join("Potflix")
            .join("meta")
            .to_string_lossy()
            .into_owned();
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        return parent.join("meta").to_string_lossy().into_owned();
    }
    Path::new(".").join("meta").to_string_lossy().into_owned()
}

/// `resolveCacheDir`: an empty or blank dir means the default.
pub fn resolve_cache_dir(dir: &str) -> String {
    if dir.trim().is_empty() {
        default_cache_dir()
    } else {
        dir.to_string()
    }
}

/// `cacheIndexPath`: the store file backing the cache directory `dir`.
pub fn cache_index_path(dir: &str) -> String {
    format!("{dir}{CACHE_INDEX_SUFFIX}")
}

/// `cacheFile`: one stored cache record. Field names and `omitempty` set are the
/// wire format (Go's tags verbatim); `checkedAt` is RFC3339 text, not a parsed
/// instant, so a Go-written record round-trips through this crate untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheFile {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub schema: i32,
    #[serde(default)]
    pub media: Media,
    #[serde(default, skip_serializing_if = "is_false")]
    pub not_found: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checked_at: String,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `cacheEntry`: the three-state result of a cache probe: a positive hit
/// carries `media`, a negative hit carries `not_found`, and
/// absent/corrupt/expired is reported by `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub media: Option<Media>,
    pub not_found: bool,
    pub checked_at: String,
}

/// `cacheHandle`: ONE store handle for a cache directory, opened lazily and held
/// for the process lifetime. The mutex covers a whole operation, not one store
/// call, because a caller like `ClearCache` spans several.
pub(crate) struct CacheHandle {
    pub(crate) db: Mutex<Db>,
}

static CACHE_HANDLES: OnceLock<Mutex<HashMap<String, Arc<CacheHandle>>>> = OnceLock::new();

fn cache_handles() -> &'static Mutex<HashMap<String, Arc<CacheHandle>>> {
    CACHE_HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `cacheHandleFor`: `dir`'s held handle, opening the store on first use.
///
/// Opening runs under the registry lock: a second concurrent open of the same
/// file would hand out two handles to one file, and the loser's append would
/// replay a stale tail.
pub(crate) fn cache_handle_for(dir: &str) -> Result<Arc<CacheHandle>, Error> {
    let mut registry = cache_handles()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(handle) = registry.get(dir) {
        return Ok(Arc::clone(handle));
    }
    let index = cache_index_path(dir);
    if let Some(parent) = Path::new(&index).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = store::open(
        &index,
        StoreOptions {
            max_value_size: CACHE_MAX_VALUE_SIZE,
            ..StoreOptions::default()
        },
    )?;
    let handle = Arc::new(CacheHandle { db: Mutex::new(db) });
    registry.insert(dir.to_string(), Arc::clone(&handle));
    Ok(handle)
}

/// `cacheHandleHeld`: whether `dir`'s store is already open in this process.
pub(crate) fn cache_handle_held(dir: &str) -> bool {
    cache_handles()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(dir)
}

/// `closeCacheHandles`: closes and forgets every held handle, so a temporary
/// cache directory can be removed. Test-side only: production never closes a
/// handle. A handle reopens lazily, so the directory stays usable afterwards.
pub fn close_cache_handles() {
    let mut registry = cache_handles()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let handles: Vec<(String, Arc<CacheHandle>)> = registry.drain().collect();
    drop(registry);
    for (_, handle) in handles {
        let db = handle
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = db.close();
    }
}

/// `withCache`: runs `f` against `dir`'s held store, creating the cache
/// directory if missing and opening the store on first use. The READ shape; a
/// mutation uses [`with_cache_write`].
pub(crate) fn with_cache<T>(
    dir: &str,
    f: impl FnOnce(&Db) -> Result<T, Error>,
) -> Result<T, Error> {
    let handle = cache_handle_for(dir)?;
    let db = handle
        .db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The legacy directory stays watched while the store is empty: a caller that
    // seeds legacy files after the store was first touched still sees them
    // imported. On a populated store this is a `len` check, not a directory read.
    if migrate_legacy(&db, dir) > 0 {
        db.commit()?;
    }
    f(&db)
}

/// `withCacheWrite`: [`with_cache`] for a mutation. It commits (flush + fsync)
/// before returning, so a stored record is durable exactly as it was when every
/// call ended in a close that flushed and fsynced.
pub(crate) fn with_cache_write<T>(
    dir: &str,
    f: impl FnOnce(&Db) -> Result<T, Error>,
) -> Result<T, Error> {
    with_cache(dir, |db| {
        let result = f(db);
        let commit = db.commit();
        match result {
            Err(err) => Err(err),
            Ok(value) => commit.map(|()| value).map_err(Error::from),
        }
    })
}

/// `CacheForEach`: calls `f` once per entry in the live cache under `dir`
/// (`""` resolves to the default cache dir), in unspecified order. `f` returns
/// false to stop early, and `value` is the record's stored bytes, byte for byte.
///
/// A directory with no cache on disk calls `f` zero times and is not an error,
/// and a read never creates a cache that was not there. `f` runs with the
/// directory's store locked, so it must not call back into the cache for the
/// same directory. `None` is Go's nil callback (a no-op).
pub fn cache_for_each<F>(dir: &str, f: Option<F>) -> Result<(), Error>
where
    F: FnMut(&str, &[u8]) -> bool,
{
    let Some(mut f) = f else {
        return Ok(());
    };
    let d = resolve_cache_dir(dir);
    if d.is_empty() || !cache_exists(&d) {
        return Ok(());
    }
    with_cache(&d, |db| {
        db.for_each(|key, value| f(&String::from_utf8_lossy(key), value))?;
        Ok(())
    })
}

/// `CachePutRaw`: stores one already-encoded record under `key`, creating the
/// cache if this is its first write, and commits before returning. The bytes are
/// stored exactly as given -- no key, schema or size check beyond the store's own
/// cap, and no re-encoding -- so the CALLER owns that contract. It never touches
/// the legacy per-key directory.
pub fn cache_put_raw(dir: &str, key: &str, value: &[u8]) -> Result<(), Error> {
    if dir.is_empty() {
        return Err(Error::other(
            "metadata: cache put raw without a cache directory",
        ));
    }
    if key.is_empty() {
        return Err(Error::other("metadata: cache put raw with an empty key"));
    }
    with_cache_write(dir, |db| {
        db.put(key, value)
            .map_err(|err| Error::other(format!("metadata: cache put raw {key:?}: {err}")))
    })
}

/// `migrateLegacy`: imports the legacy per-key files while the store is empty,
/// and reports how many records it added. Only an empty store imports.
fn migrate_legacy(db: &Db, dir: &str) -> usize {
    if !db.is_empty() {
        return 0;
    }
    migrate_legacy_cache(db, &legacy_cache_files(dir))
}

/// `cacheExists`: whether `dir` has a cache on disk -- an open handle, or the
/// store index, or legacy per-key `.json` files migration has not imported yet.
/// Read paths use it so a probe never creates a cache that was never there.
pub(crate) fn cache_exists(dir: &str) -> bool {
    if cache_handle_held(dir) {
        return true;
    }
    if let Ok(meta) = std::fs::metadata(cache_index_path(dir))
        && !meta.is_dir()
    {
        return true;
    }
    !legacy_cache_files(dir).is_empty()
}

/// `cacheRecord`: the stored record for `key`. A cache that is not on disk is a
/// miss and is never created by a read.
pub(crate) fn cache_record(dir: &str, key: &str) -> Option<Arc<[u8]>> {
    if dir.is_empty() || key.is_empty() || !cache_exists(dir) {
        return None;
    }
    let mut data = None;
    let result = with_cache(dir, |db| {
        data = db.get(key);
        Ok(())
    });
    match result {
        Ok(()) => data,
        Err(_) => None,
    }
}

/// `legacyCacheFiles`: the legacy per-key cache files in `dir`, ignoring the
/// `.tmp-` files the old atomic writer could leave behind. A missing dir lists
/// none, never an error.
pub(crate) fn legacy_cache_files(dir: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".json") || name.starts_with(".tmp-") {
            continue;
        }
        out.push(entry.path());
    }
    out.sort();
    out
}

/// `migrateLegacyCache`: imports every legacy per-key record into the store and
/// returns how many records it added. Additive and never destructive: the files
/// stay exactly where they are, as the recovery copy. Only a body that decodes
/// as a cache record with a non-empty key is imported, so junk, truncation and
/// foreign `.json` files import nothing. The record's own key is authoritative
/// and its stored bytes are the file's bytes, byte for byte.
pub(crate) fn migrate_legacy_cache(db: &Db, files: &[PathBuf]) -> usize {
    let mut imported = 0;
    for path in files {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<CacheFile>(&data) else {
            continue;
        };
        if record.key.is_empty() {
            continue;
        }
        if db.put(&record.key, &data).is_err() {
            continue;
        }
        imported += 1;
    }
    imported
}

/// `ClearCache`: drops every cached entry under `dir` and removes the legacy
/// one-file-per-key directory if it is still there. THE way to invalidate a
/// cache entry. Idempotent: an empty or absent cache reports no error.
pub fn clear_cache(dir: &str) -> Result<(), Error> {
    let d = resolve_cache_dir(dir);
    if d.is_empty() {
        return Ok(());
    }
    clear_cache_entries(&d)?;
    match std::fs::remove_dir_all(&d) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::from(err)),
    }
}

/// `ClearCacheEntries`: drops every entry in the live cache under `dir` and
/// reports how many it cleared. It is `ClearCache`'s entry half: the store file
/// survives as an empty snapshot and the legacy per-key directory is left
/// exactly where it is.
pub fn clear_cache_entries(dir: &str) -> Result<i32, Error> {
    let d = resolve_cache_dir(dir);
    if d.is_empty() || !cache_exists(&d) {
        return Ok(0);
    }
    let mut cleared = 0;
    with_cache_write(&d, |db| {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        db.for_each(|key, _| {
            keys.push(key.to_vec());
            true
        })?;
        for key in &keys {
            db.delete(key)?;
        }
        cleared = keys.len() as i32;
        // Tombstones alone would leave the file's bytes on disk: rewrite it as
        // an empty snapshot.
        db.compact()?;
        Ok(())
    })?;
    Ok(cleared)
}

/// `CacheHas`: whether `key` has an entry in the cache under `dir`. Presence in
/// the store is the existence check: the body is never read or parsed, so a
/// stored entry reports present whatever its schema.
pub fn cache_has(dir: &str, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let d = resolve_cache_dir(dir);
    if d.is_empty() {
        return false;
    }
    cache_record(&d, key).is_some()
}

/// `CacheLive`: whether `key` has a VALID LIVE answer in the cache under `dir`.
/// Unlike the presence-only [`cache_has`], this decodes and validates the stored
/// record: a positive entry is live only while its TTL has not expired; a
/// negative entry is live only while its TTL has not expired; corrupt /
/// wrong-schema / wrong-key / truncated records are not.
pub fn cache_live(dir: &str, key: &str) -> bool {
    cache_load_entry(dir, key).is_some()
}

/// `cacheLoadEntry`: distinguishes positive hits, fresh negative hits, and
/// absent/corrupt/expired entries in `dir`'s store (`""` is always a miss).
pub(crate) fn cache_load_entry(dir: &str, key: &str) -> Option<CacheEntry> {
    let data = cache_record(dir, key)?;
    decode_cache_entry(&data, key)
}

/// `decodeCacheEntry`: applies the cache's validity gates to one stored record:
/// the schema and key must match, a negative must still be fresh, and a positive
/// must still be fresh and carry a `source`. A positive Wikipedia record additionally heals its
/// title on decode: records written before the disambiguation strip carry
/// `Ready Player One (film)`, and normalizing here (no schema bump, no rewrite,
/// no refetch) lets the owner's existing cache heal in place. The normalizer
/// is deliberately narrow -- a real parenthesized title survives -- so healing
/// cannot corrupt a legitimate record.
pub(crate) fn decode_cache_entry(data: &[u8], key: &str) -> Option<CacheEntry> {
    let record: CacheFile = serde_json::from_slice(data).ok()?;
    if record.key != key || record.schema != CACHE_SCHEMA {
        return None;
    }
    if record.not_found {
        if !negative_fresh(&record.checked_at, SystemTime::now()) {
            return None;
        }
        return Some(CacheEntry {
            media: None,
            not_found: true,
            checked_at: record.checked_at,
        });
    }
    if record.media.source.is_empty() {
        return None;
    }
    if !positive_fresh(&record.checked_at, SystemTime::now()) {
        return None;
    }
    let mut media = record.media;
    if media.source == "wikipedia" {
        media.title = crate::wikipedia::strip_disambiguation(&media.title);
        media.source_id = crate::wikipedia::strip_disambiguation(&media.source_id);
    }
    Some(CacheEntry {
        media: Some(media),
        not_found: false,
        checked_at: record.checked_at,
    })
}

/// `cacheLoad`: the cached `Media` for `key`. A missing, corrupt, or mismatched
/// entry is a miss, never an error.
///
/// Test-only, exactly as in Go: `cache.go` defines it but only `*_test.go`
/// calls it (`cacheLoadEntry` is what production uses).
#[cfg(test)]
pub(crate) fn cache_load(dir: &str, key: &str) -> Option<Media> {
    let entry = cache_load_entry(dir, key)?;
    if entry.not_found {
        return None;
    }
    entry.media
}

/// `cacheStoreNegative`: persists a known-miss under the SAME key so the
/// presence probe keeps answering true for it with no change. The `checkedAt`
/// timestamp is clamped to now (never written in the future) so a clock jump
/// cannot pin a far-future miss.
pub(crate) fn cache_store_negative(dir: &str, key: &str) -> Result<(), Error> {
    if key.is_empty() {
        return Err(Error::other("metadata: cacheStoreNegative empty key"));
    }
    let record = CacheFile {
        key: key.to_string(),
        schema: CACHE_SCHEMA,
        media: Media::default(),
        not_found: true,
        checked_at: now_rfc3339(),
    };
    let data = serde_json::to_vec(&record)?;
    cache_put(dir, key, &data)
}

/// `cacheStore`: writes one record for `key` into the store. Entries are
/// additive; nothing here ever deletes. The record is stamped with now, which
/// starts its [`POSITIVE_TTL`] window.
pub(crate) fn cache_store(dir: &str, key: &str, media: &Media) -> Result<(), Error> {
    if key.is_empty() {
        return Err(Error::other("metadata: cacheStore empty key"));
    }
    let record = CacheFile {
        key: key.to_string(),
        schema: CACHE_SCHEMA,
        media: media.clone(),
        not_found: false,
        checked_at: now_rfc3339(),
    };
    let data = serde_json::to_vec(&record)?;
    cache_put(dir, key, &data)
}

/// `cachePut`: stores one encoded record, creating the cache if this is its
/// first write, and commits before returning.
fn cache_put(dir: &str, key: &str, data: &[u8]) -> Result<(), Error> {
    if dir.is_empty() {
        return Err(Error::other(
            "metadata: cache put without a cache directory",
        ));
    }
    with_cache_write(dir, |db| {
        db.put(key, data)
            .map_err(|err| Error::other(format!("metadata: cache put {key:?}: {err}")))
    })
}

/// `CountNegatives`: how many not-found (dead) entries the cache holds under
/// `dir`. Only well-formed current-schema negative records count.
pub fn count_negatives(dir: &str) -> i32 {
    let d = resolve_cache_dir(dir);
    if d.is_empty() || !cache_exists(&d) {
        return 0;
    }
    let mut count = 0;
    // `for_each` runs with the store locked: this callback only decodes a copy.
    let result = with_cache(&d, |db| {
        db.for_each(|_, value| {
            if let Ok(record) = serde_json::from_slice::<CacheFile>(value)
                && record.schema == CACHE_SCHEMA
                && record.not_found
            {
                count += 1;
            }
            true
        })?;
        Ok(())
    });
    match result {
        Ok(()) => count,
        Err(_) => 0,
    }
}

/// `positiveFresh`: whether a stored positive `checkedAt` still counts as a
/// live hit at `now`. Zero, TTL-expired, and far-future (beyond
/// [`CACHE_CLOCK_SKEW`]) timestamps are all expired; a near-future timestamp
/// within the skew window is fresh (clock wobble, not a jump).
pub(crate) fn positive_fresh(checked_at: &str, now: SystemTime) -> bool {
    let Some(at) = parse_rfc3339(checked_at) else {
        return false;
    };
    if at > now + CACHE_CLOCK_SKEW {
        return false;
    }
    if at > now {
        return true;
    }
    match now.duration_since(at) {
        Ok(age) => age <= POSITIVE_TTL,
        Err(_) => true,
    }
}

/// `negativeFresh`: whether a stored `checkedAt` still counts as a live miss at
/// `now`. Zero, TTL-expired, and far-future (beyond [`CACHE_CLOCK_SKEW`])
/// timestamps are all expired; a near-future timestamp within the skew window is
/// fresh (clock wobble, not a jump).
pub(crate) fn negative_fresh(checked_at: &str, now: SystemTime) -> bool {
    let Some(at) = parse_rfc3339(checked_at) else {
        return false;
    };
    if at > now + CACHE_CLOCK_SKEW {
        return false;
    }
    if at > now {
        return true;
    }
    match now.duration_since(at) {
        Ok(age) => age <= NEGATIVE_TTL,
        Err(_) => true,
    }
}

/// `negativeMemExpiry`: bounds the in-memory suppression so it never outlives
/// the stored TTL expiry: `min(checkedAt+negativeTTL, now+negativeBackoff)`.
/// Callers must only use it for a fresh `checkedAt`.
pub(crate) fn negative_mem_expiry(
    checked_at: &str,
    now: SystemTime,
    backoff: Duration,
) -> SystemTime {
    let ttl_expiry = parse_rfc3339(checked_at)
        .and_then(|at| at.checked_add(NEGATIVE_TTL))
        .unwrap_or(now);
    let backoff_expiry = now + backoff;
    ttl_expiry.min(backoff_expiry)
}

/// A cache record's `checkedAt` as an instant, for the tests and callers that
/// need to compare it.
pub fn checked_at_instant(checked_at: &str) -> Option<SystemTime> {
    parse_rfc3339(checked_at)
}


const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256, hand-rolled: the workspace pins no hashing crate, and the cache's
/// legacy file name (`sha256(key)[:16]`) is the one consumer. Table-driven IEEE
/// FIPS 180-4, the same posture the crash-safe store takes for CRC32.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(data.len() + 72);
    message.extend_from_slice(data);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let (chunks, _) = message.as_chunks::<64>();
    for chunk in chunks {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        digest[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timefmt::format_rfc3339;

    #[test]
    fn sha256_matches_the_published_digests() {
        let hex = |data: &[u8]| {
            sha256(data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(
            hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn legacy_file_name_is_sixteen_hex_chars() {
        let name = cache_file_name("series|breaking bad|2008|1");
        assert_eq!(name.len(), 21);
        assert!(name.ends_with(".json"));
        assert!(name[..16].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn decode_heals_wikipedia_disambiguation_title() {
        // A record written before the disambiguation strip (the owner's live
        // cache carries `Ready Player One (film)`) decodes to the clean title
        // with no schema bump, no rewrite and no refetch.
        let key = "movie|ready player one|2018";
        let record = CacheFile {
            key: key.to_string(),
            schema: CACHE_SCHEMA,
            media: Media {
                source: "wikipedia".to_string(),
                source_id: "Ready Player One (film)".to_string(),
                kind: Kind::MOVIE,
                title: "Ready Player One (film)".to_string(),
                year: 2018,
                ..Media::default()
            },
            not_found: false,
            checked_at: format_rfc3339(SystemTime::now()),
        };
        let data = serde_json::to_vec(&record).expect("encode record");
        let entry = decode_cache_entry(&data, key).expect("entry");
        let media = entry.media.expect("media");
        assert_eq!(media.title, "Ready Player One");
        assert_eq!(media.source_id, "Ready Player One");
        assert_eq!(media.year, 2018);
        // A real parenthesized title heals to itself, and other providers are
        // never rewritten by a Wikipedia convention.
        for (source, title) in [
            ("wikipedia", "Nymphomaniac (Director's Cut)"),
            ("cinemeta", "Dune (2021 film)"),
        ] {
            let mut stored = record.clone();
            stored.media.source = source.to_string();
            stored.media.title = title.to_string();
            stored.media.source_id = title.to_string();
            let data = serde_json::to_vec(&stored).expect("encode record");
            let entry = decode_cache_entry(&data, key).expect("entry");
            let media = entry.media.expect("media");
            assert_eq!(media.title, title, "source = {source}");
        }
    }

    #[test]
    fn negative_freshness_gates() {
        let now = SystemTime::now();
        assert!(!negative_fresh("", now));
        assert!(!negative_fresh("0001-01-01T00:00:00Z", now));
        assert!(negative_fresh(&format_rfc3339(now), now));
        assert!(negative_fresh(
            &format_rfc3339(now + Duration::from_secs(60)),
            now
        ));
        assert!(!negative_fresh(
            &format_rfc3339(now + Duration::from_secs(3600)),
            now
        ));
        assert!(!negative_fresh(
            &format_rfc3339(now - NEGATIVE_TTL - Duration::from_secs(3600)),
            now
        ));
    }
}
