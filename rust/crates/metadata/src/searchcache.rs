//! `metadata::searchcache` -- the Fix-match picker's result cache.
//!
//! NEW SURFACE: `go\` has no search-result cache, so nothing here is diffed
//! against a ported file; the contract is stated plainly instead. A search
//! costs one request per provider leg, and the picker answers the same typed
//! name again and again, so the ranked, merged rows ([`crate::search`]) are
//! remembered under the picker's own key: [`search_cache_put`] records them,
//! [`search_cache_get`] serves them back. A PARTIAL answer keeps both halves --
//! the rows that arrived and the provider legs that failed -- so a restart can
//! retry just the legs that did not answer. There is NO TTL: a stored key is
//! served forever -- its rows are identities, not dates, and the picker's next
//! search of that key is what replaces them.
//!
//! # Store file
//!
//! `%LOCALAPPDATA%\Potflix\search-cache.json` ([`default_path`]), the same
//! directory convention as [`crate::overrides`]'s latch store: the path is
//! derived from that store's own path, so the two can never disagree about the
//! config directory. The shape is
//! `{"version":1,"entries":[{"key","rows","failed"}]}` and
//! [`SearchCache::save`] always writes version 1. `failed` carries the provider
//! names whose leg errored; an entry written before that field existed loads
//! with an empty list (`#[serde(default)]`), so old caches stay readable.
//! Entries are written in key order, so the same cache produces the same bytes.
//!
//! # A broken file is never fatal
//!
//! The cache is best-effort by design: an absent, unreadable or malformed file
//! loads as an empty cache, and an entry that does not decode as a
//! `{key, rows, failed}` pair is dropped on its own while its neighbours still
//! load. Nothing here reports an error and loading never rewrites the bad file;
//! the next successful [`SearchCache::put`] replaces it with a clean one.
//!
//! # What is never stored
//!
//! A blank key, a row carrying no identity at all (no provider, no provider id
//! and no title) and an answer that is BOTH empty and failure-free are all
//! dropped: a search that found nothing and lost nothing is a miss, not a
//! cached answer, and it never clears an entry the key already holds. A partial
//! entry (rows, failed legs, or both) IS stored, and a later clean put replaces
//! it so the stale failures are cleared.
//!
//! # Atomic saves
//!
//! [`SearchCache::save`] follows the repo's existing atomic-write convention
//! (the latch store's [`crate::overrides`] writer): marshal, create a uniquely
//! named `.tmp-<pid>-<nanos>-<seq>.json` beside the target, write, close,
//! rename over the target. A failed write or rename removes the temp file and
//! leaves the previous file exactly as it was.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::search::Candidate;

/// The cache file's name, beside the latch store in the config directory.
pub const SEARCH_CACHE_FILE_NAME: &str = "search-cache.json";

/// The on-disk envelope version [`SearchCache::save`] writes. A file carrying
/// any other version is discarded on load, never guessed at.
pub const SEARCH_CACHE_VERSION: i32 = 1;

/// One cached answer: the picker's key, the rows it was answered with, and the
/// provider legs that failed. `failed` is serde-defaulted so an entry written
/// before partial answers existed loads with no failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    #[serde(default)]
    key: String,
    #[serde(default)]
    rows: Vec<Candidate>,
    #[serde(default)]
    failed: Vec<String>,
}

/// A cached search answer: the ranked rows the legs offered plus the provider
/// names whose leg failed. `rows` may be empty (a failed-only entry, retried
/// once the provider recovers) and `failed` may be empty (a clean answer).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSearch {
    /// The rows the key was answered with, empty on a failed-only entry.
    pub rows: Vec<Candidate>,
    /// The provider names that failed, in provider order.
    pub failed: Vec<String>,
}

/// The read side of the envelope: `entries` stays untyped here so one
/// malformed entry can be dropped without losing its neighbours.
#[derive(Debug, Deserialize)]
struct CacheFile {
    #[serde(default)]
    version: i32,
    #[serde(default)]
    entries: Vec<serde_json::Value>,
}

/// The cache path: `%LOCALAPPDATA%\Potflix\search-cache.json`, the latch
/// store's own directory with this module's file name -- including that
/// store's executable-directory fallback.
pub fn default_path() -> PathBuf {
    crate::overrides::default_path().with_file_name(SEARCH_CACHE_FILE_NAME)
}

/// The comparison form of one picker key: trimmed and lowercased, so the same
/// typed search always hits the same entry. Empty stays empty (never stored).
fn cache_key(key: &str) -> String {
    key.trim().to_lowercase()
}

/// Whether a row carries no identity at all, so the cache never holds it.
fn row_is_empty(cand: &Candidate) -> bool {
    cand.provider.trim().is_empty()
        && cand.provider_id.trim().is_empty()
        && cand.title.trim().is_empty()
}

/// The rows a cache keeps: the empty ones dropped, the order kept.
fn kept_rows(rows: &[Candidate]) -> Vec<Candidate> {
    rows.iter()
        .filter(|cand| !row_is_empty(cand))
        .cloned()
        .collect()
}

/// The failed-leg names a cache keeps: trimmed, the blank ones dropped, a
/// repeated name kept once, the first-seen order kept.
fn kept_failed(failed: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(failed.len());
    for name in failed {
        let name = name.trim();
        if name.is_empty() || kept.iter().any(|kept| kept == name) {
            continue;
        }
        kept.push(name.to_string());
    }
    kept
}

/// The cache: the stored answers plus the file they live in.
#[derive(Debug, Clone)]
pub struct SearchCache {
    path: PathBuf,
    entries: BTreeMap<String, CachedSearch>,
}

/// The picker's answer for `key`, from the default store: `None` on a miss,
/// an absent file or a broken one (all three are the same "not cached").
pub fn search_cache_get(key: &str) -> Option<CachedSearch> {
    SearchCache::load().get(key)
}

/// Serializes every put (and the load it merges with): `put` is
/// load→insert→save of the whole file, so two threads or two processes
/// racing without this lock lose each other's keys.
static CACHE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Records `rows` and `failed` under `key` in the default store, best-effort:
/// an answer that is both empty and failure-free, a blank key or a failed
/// write leaves the store exactly as it was, and the picker's search stands
/// regardless -- there is nothing to report. The load, merge and save run
/// under one lock so concurrent puts keep each other's keys.
pub fn search_cache_put(key: &str, rows: &[Candidate], failed: &[String]) {
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cache = SearchCache::load_from(&default_path());
    let _ = cache.put_inner(key, rows, failed);
}

impl SearchCache {
    /// Loads the cache from [`default_path`].
    pub fn load() -> SearchCache {
        SearchCache::load_from(&default_path())
    }

    /// The explicit-path core of [`load`], so a caller with a custom location
    /// -- and every test -- never touches the real cache. Every failure is an
    /// empty cache, never an error: an absent file, a bad envelope, a
    /// non-array `entries`, a wrong version, or one entry that does not decode
    /// (that entry alone is skipped). A wholly unparsable or wrong-version
    /// file is quarantined beside the cache (once) before returning empty, so
    /// no evidence is lost; loading never rewrites the target itself.
    pub fn load_from(path: &Path) -> SearchCache {
        let mut cache = SearchCache {
            path: path.to_path_buf(),
            entries: BTreeMap::new(),
        };
        let Ok(data) = std::fs::read(path) else {
            return cache;
        };
        let Ok(file) = serde_json::from_slice::<CacheFile>(&data) else {
            let _ = quarantine_corrupt_file(path);
            return cache;
        };
        if file.version != SEARCH_CACHE_VERSION {
            let _ = quarantine_corrupt_file(path);
            return cache;
        }
        for value in file.entries {
            let Ok(entry) = serde_json::from_value::<CacheEntry>(value) else {
                continue;
            };
            let key = cache_key(&entry.key);
            let rows = kept_rows(&entry.rows);
            let failed = kept_failed(&entry.failed);
            if key.is_empty() || (rows.is_empty() && failed.is_empty()) {
                continue;
            }
            cache.entries.insert(key, CachedSearch { rows, failed });
        }
        cache
    }

    /// The stored answer for `key`: a clone, so the caller owns its answer.
    pub fn get(&self, key: &str) -> Option<CachedSearch> {
        self.entries.get(&cache_key(key)).cloned()
    }

    /// Stores `rows` and `failed` under `key` and saves in the same call. An
    /// answer that is both empty and failure-free, or a blank key, is a no-op:
    /// nothing is written and an existing entry is left alone. A clean answer
    /// REPLACES a partial one, so a successful retry clears the stale failures.
    /// A failed save rolls the in-memory change back, so memory always matches
    /// the last good file. The whole put runs under the cache lock and merges
    /// the on-disk state first, so concurrent puts keep each other's keys.
    pub fn put(&mut self, key: &str, rows: &[Candidate], failed: &[String]) -> Result<(), Error> {
        let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.merge_disk_locked();
        self.put_inner(key, rows, failed)
    }

    /// The on-disk entries this instance has not seen yet (a concurrent put
    /// saved after this instance loaded). Called with the cache lock held.
    fn merge_disk_locked(&mut self) {
        let Ok(data) = std::fs::read(&self.path) else {
            return;
        };
        let Ok(file) = serde_json::from_slice::<CacheFile>(&data) else {
            return;
        };
        if file.version != SEARCH_CACHE_VERSION {
            return;
        }
        for value in file.entries {
            let Ok(entry) = serde_json::from_value::<CacheEntry>(value) else {
                continue;
            };
            let key = cache_key(&entry.key);
            let rows = kept_rows(&entry.rows);
            let failed = kept_failed(&entry.failed);
            if key.is_empty() || (rows.is_empty() && failed.is_empty()) {
                continue;
            }
            self.entries
                .entry(key)
                .or_insert(CachedSearch { rows, failed });
        }
    }

    /// The unlocked core of [`put`]: insert plus save. Callers hold
    /// [`CACHE_LOCK`] across the load/merge/save so the file never loses a
    /// concurrent key.
    fn put_inner(
        &mut self,
        key: &str,
        rows: &[Candidate],
        failed: &[String],
    ) -> Result<(), Error> {
        let key = cache_key(key);
        let rows = kept_rows(rows);
        let failed = kept_failed(failed);
        if key.is_empty() || (rows.is_empty() && failed.is_empty()) {
            return Ok(());
        }
        let previous = self
            .entries
            .insert(key.clone(), CachedSearch { rows, failed });
        match self.save_inner() {
            Ok(()) => Ok(()),
            Err(err) => {
                match previous {
                    Some(previous) => {
                        self.entries.insert(key, previous);
                    }
                    None => {
                        self.entries.remove(&key);
                    }
                }
                Err(err)
            }
        }
    }

    /// Writes the cache to its path, atomically: marshal, stage a
    /// uniquely-named temp file beside the target, rename over it. A failed
    /// write or rename removes the temp file and leaves the previous file
    /// exactly as it was. The rename is retried on transient sharing
    /// violations; the temp file is fsynced before the rename and the
    /// directory after, so a power cut cannot commit a torn file.
    pub fn save(&self) -> Result<(), Error> {
        let _guard = CACHE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.save_inner()
    }

    /// The unlocked core of [`save`]; callers hold [`CACHE_LOCK`].
    fn save_inner(&self) -> Result<(), Error> {
        let dir = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir)?;
        let entries: Vec<CacheEntry> = self
            .entries
            .iter()
            .map(|(key, answer)| CacheEntry {
                key: key.clone(),
                rows: answer.rows.clone(),
                failed: answer.failed.clone(),
            })
            .collect();
        let data = serde_json::to_vec(&serde_json::json!({
            "version": SEARCH_CACHE_VERSION,
            "entries": entries,
        }))?;
        let (temp_path, mut temp) = create_temp(&dir)?;
        if let Err(err) = temp.write_all(&data) {
            drop(temp);
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        if let Err(err) = temp.sync_all() {
            drop(temp);
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        drop(temp);
        if let Err(err) = rename_with_retry(&temp_path, &self.path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        fsync_dir(&dir);
        Ok(())
    }
}

/// Renames with a few short retries: on Windows the target can be briefly
/// held by a concurrent reader (sharing violation), and dropping the write
/// instead of retrying would silently lose the cached answer.
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut wait_ms = 10u64;
    for attempt in 0..6 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(err) if attempt + 1 >= 6 => return Err(err),
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(wait_ms));
                wait_ms = (wait_ms * 2).min(200);
            }
        }
    }
    Err(std::io::Error::other("rename retries exhausted"))
}

/// Best-effort directory fsync: persists the rename. Failures are ignored;
/// the file sync is the load-bearing half.
fn fsync_dir(dir: &Path) {
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
}

/// The quarantine backup beside `path`, if one was already taken:
/// `<file-name>.corrupt-<ms>`.
pub fn corrupt_backup_for(path: &Path) -> Option<PathBuf> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let prefix = format!("{name}.corrupt-");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|candidate| {
            candidate
                .file_name()
                .is_some_and(|file| file.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// Moves a wholly unparsable cache file aside once, returning the backup path.
/// A missing file is not corrupt (`None`); when a backup already exists the
/// first one is kept and returned, so repeated loads never stack renames.
fn quarantine_corrupt_file(path: &Path) -> Option<PathBuf> {
    if let Some(existing) = corrupt_backup_for(path) {
        return Some(existing);
    }
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let backup = parent.join(format!("{name}.corrupt-{millis}"));
    std::fs::rename(path, &backup).ok()?;
    Some(backup)
}

/// `os.CreateTemp(dir, ".tmp-*.json")`: a uniquely named file in `dir`,
/// created exclusively and retried on collision, in the same
/// `.tmp-<pid>-<nanos>-<seq>.json` shape the latch store uses, so an atomic
/// write here looks like every other atomic write in the repo.
fn create_temp(dir: &Path) -> std::io::Result<(PathBuf, File)> {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..64 {
        let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos())
            .unwrap_or(0);
        let name = format!(".tmp-{}-{nanos}-{sequence}.json", std::process::id());
        let candidate = dir.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique temp file for the atomic write",
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use metadata_core::types::Kind;

    use super::*;
    use crate::testsupport::TempDir;

    /// One search row, with every field that is not `provider`/`provider_id`/
    /// `title` left empty: the shape a picker stores.
    fn cand(provider: &str, id: &str, title: &str) -> Candidate {
        Candidate {
            kind: Kind::SERIES,
            title: title.to_string(),
            year: 2008,
            provider: provider.to_string(),
            provider_id: id.to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        }
    }

    /// The cache file inside a fresh temp directory.
    fn cache_path(dir: &TempDir) -> PathBuf {
        PathBuf::from(dir.child(SEARCH_CACHE_FILE_NAME))
    }

    #[test]
    fn the_store_lives_beside_the_latch_store() {
        let path = default_path();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(SEARCH_CACHE_FILE_NAME)
        );
        assert_eq!(
            path.parent(),
            crate::overrides::default_path().parent(),
            "one config directory, whichever fallback it resolved to"
        );
    }

    #[test]
    fn round_trip_stores_and_serves_the_rows() {
        let dir = TempDir::new();
        let path = cache_path(&dir);
        let mut cache = SearchCache::load_from(&path);
        assert!(cache.get("Show|2008|series").is_none(), "a fresh cache is empty");

        let rows = vec![cand("tvmaze", "169", "Show")];
        cache.put("Show|2008|series", &rows, &[]).expect("put");
        assert_eq!(
            cache.get("  show|2008|SERIES "),
            Some(CachedSearch {
                rows: rows.clone(),
                failed: Vec::new(),
            }),
            "keys compare trimmed and case-insensitively"
        );

        // The bytes on disk are the versioned envelope, in key order.
        let raw = std::fs::read_to_string(&path).expect("read the cache");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("the envelope is JSON");
        assert_eq!(value["version"], SEARCH_CACHE_VERSION);
        assert_eq!(value["entries"][0]["key"], "show|2008|series");
        assert_eq!(
            value["entries"][0]["rows"][0]["provider_id"],
            "169",
            "the row travels whole"
        );
        assert_eq!(
            value["entries"][0]["failed"],
            serde_json::json!([]),
            "a clean answer writes an empty failed list"
        );

        // A fresh load serves the same answer: no TTL, no re-search.
        let reloaded = SearchCache::load_from(&path);
        assert_eq!(
            reloaded.get("Show|2008|series"),
            Some(CachedSearch {
                rows,
                failed: Vec::new(),
            })
        );
    }

    #[test]
    fn a_broken_file_loads_empty_and_the_next_put_rewrites_it() {
        let dir = TempDir::new();
        let path = cache_path(&dir);

        // Not JSON at all: a miss, never a panic.
        std::fs::write(&path, "{not json").expect("seed a broken file");
        let mut cache = SearchCache::load_from(&path);
        assert!(cache.get("Show|2008|series").is_none());
        let rows = vec![cand("tvmaze", "169", "Show")];
        cache
            .put("Show|2008|series", &rows, &[])
            .expect("put over a broken file");
        assert_eq!(
            SearchCache::load_from(&path).get("Show|2008|series"),
            Some(CachedSearch {
                rows,
                failed: Vec::new(),
            }),
            "the next put replaces the broken file with a clean one"
        );

        // One malformed ENTRY (and one with neither rows nor failed legs)
        // drops only itself.
        std::fs::write(&path, "{not json").expect("break it again");
        let good = serde_json::json!({
            "key": "Show|2009|series",
            "rows": [serde_json::to_value(cand("tvmaze", "170", "Show")).expect("row")],
        });
        let file = serde_json::json!({
            "version": SEARCH_CACHE_VERSION,
            "entries": [
                {"key": 7, "rows": "nope"},
                {"key": "Show", "rows": [], "failed": []},
                good,
            ],
        });
        std::fs::write(&path, serde_json::to_vec(&file).expect("json")).expect("write");
        let loaded = SearchCache::load_from(&path);
        assert!(
            loaded.get("Show|2009|series").is_some(),
            "the sane entry survives its broken neighbours"
        );
        assert!(
            loaded.get("Show").is_none(),
            "an entry with no rows and no failed legs is dropped"
        );

        // A wrong envelope version is not ours to read.
        std::fs::write(&path, r#"{"version":99,"entries":[]}"#).expect("write v99");
        assert!(SearchCache::load_from(&path).get("Show|2009|series").is_none());
    }

    #[test]
    fn an_empty_result_is_never_cached() {
        let dir = TempDir::new();
        let path = cache_path(&dir);
        let mut cache = SearchCache::load_from(&path);

        cache.put("Show|2008|series", &[], &[]).expect("put empty");
        assert!(cache.get("Show|2008|series").is_none(), "a miss is not an answer");
        assert!(!path.exists(), "an empty result writes no file");

        // A row with no identity at all is an empty result too.
        let blank = Candidate {
            kind: Kind::SERIES,
            title: String::new(),
            year: 0,
            provider: String::new(),
            provider_id: String::new(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        cache
            .put("Show|2008|series", std::slice::from_ref(&blank), &[])
            .expect("put blanks");
        assert!(!path.exists(), "blank rows are not a cached answer");

        // A blank key is never stored.
        cache
            .put("   ", &[cand("tvmaze", "169", "Show")], &[])
            .expect("blank key");
        assert!(!path.exists());

        // Empty rows are dropped from a real result, never stored beside it.
        cache
            .put(
                "Show|2008|series",
                &[blank, cand("tvmaze", "169", "Show")],
                &[],
            )
            .expect("put mixed");
        let stored = cache.get("Show|2008|series").expect("stored");
        assert_eq!(
            stored.rows.len(),
            1,
            "the blank row is dropped, the real one kept"
        );
        assert_eq!(stored.rows[0].provider_id, "169");

        // An empty result never clears what the key already holds.
        cache.put("Show|2008|series", &[], &[]).expect("put empty again");
        assert_eq!(
            cache.get("Show|2008|series").map(|answer| answer.rows.len()),
            Some(1),
            "an empty result never clears a stored answer"
        );
    }

    #[test]
    fn a_partial_answer_keeps_its_failed_legs_and_a_clean_retry_clears_them() {
        let dir = TempDir::new();
        let path = cache_path(&dir);
        let mut cache = SearchCache::load_from(&path);

        let rows = vec![cand("tvmaze", "169", "Show")];
        cache
            .put(
                "Show|2008|series",
                &rows,
                &["kitsu".to_string(), " kitsu ".to_string(), String::new()],
            )
            .expect("put partial");
        let stored = cache.get("Show|2008|series").expect("a partial answer is stored");
        assert_eq!(stored.rows, rows);
        assert_eq!(
            stored.failed,
            vec!["kitsu".to_string()],
            "one trimmed name, once"
        );

        // The failed legs survive the disk: a restart retries just them.
        let stored = SearchCache::load_from(&path)
            .get("show|2008|series")
            .expect("the partial answer reloads");
        assert_eq!(stored.rows.len(), 1);
        assert_eq!(stored.failed, vec!["kitsu".to_string()]);
        let raw = std::fs::read_to_string(&path).expect("read the cache");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(value["entries"][0]["failed"], serde_json::json!(["kitsu"]));

        // A successful retry stores the same rows with no failure: the stale
        // failure is CLEARED, not carried.
        let retried = vec![cand("tvmaze", "169", "Show"), cand("kitsu", "k1", "Show")];
        cache
            .put("Show|2008|series", &retried, &[])
            .expect("retry");
        let stored = SearchCache::load_from(&path)
            .get("Show|2008|series")
            .expect("the retry is stored");
        assert_eq!(stored.failed, Vec::<String>::new(), "a clean retry clears failed");
        assert_eq!(stored.rows, retried);
    }

    #[test]
    fn a_failed_only_answer_is_stored_and_served() {
        let dir = TempDir::new();
        let path = cache_path(&dir);
        let mut cache = SearchCache::load_from(&path);

        cache
            .put("Show|2008|series", &[], &["tvmaze".to_string()])
            .expect("put failed-only");
        assert!(path.exists(), "a failure is an answer worth remembering");
        let stored = cache.get("Show|2008|series").expect("stored");
        assert!(stored.rows.is_empty());
        assert_eq!(stored.failed, vec!["tvmaze".to_string()]);
        assert_eq!(
            SearchCache::load_from(&path)
                .get("Show|2008|series")
                .map(|answer| answer.failed),
            Some(vec!["tvmaze".to_string()])
        );

        // ...and an answer with neither half is still a no-op.
        cache.put("Show|2009|series", &[], &[]).expect("put nothing");
        assert!(cache.get("Show|2009|series").is_none());
    }

    #[test]
    fn an_entry_written_before_failed_existed_loads_with_no_failures() {
        let dir = TempDir::new();
        let path = cache_path(&dir);
        let file = serde_json::json!({
            "version": SEARCH_CACHE_VERSION,
            "entries": [{
                "key": "Show|2008|series",
                "rows": [serde_json::to_value(cand("tvmaze", "169", "Show")).expect("row")],
            }],
        });
        std::fs::write(&path, serde_json::to_vec(&file).expect("json")).expect("write");
        let stored = SearchCache::load_from(&path)
            .get("Show|2008|series")
            .expect("an old entry still loads");
        assert_eq!(stored.rows.len(), 1);
        assert!(
            stored.failed.is_empty(),
            "the missing field defaults to no failures"
        );
    }
}
