
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::pathutil::{dir as path_dir, join as path_join, normalize_scan_dir};
use crate::scanindex::{read_legacy_scan_index, same_scan_record, FolderRecord, LegacyCounts};
use crate::{file_exists, Error};

/// The store file lives beside the legacy index path.
pub(crate) const INDEX_STORE_NAME: &str = "library.idx";
pub(crate) const LEGACY_BAK_SUFFIX: &str = ".bak";

pub(crate) const REC_KEY_FILE: &str = "f|";
pub(crate) const REC_KEY_FOLDER: &str = "d|";
pub(crate) const REC_KEY_META: &str = "m|index";

pub(crate) const ENC_FILE_VERSION: u8 = 1;
pub(crate) const ENC_FOLDER_VERSION: u8 = 1;

/// `maxRepairAttempts` is how many failed lookups an entry gets before it is
/// parked as needs_repair.
pub(crate) const MAX_REPAIR_ATTEMPTS: i64 = 3;

/// Per-file metadata state, exactly the strings the store records use.
pub const STATE_RESOLVED: &str = "resolved";
pub const STATE_PENDING: &str = "pending";
pub const STATE_NEEDS_REPAIR: &str = "needs_repair";

pub(crate) const FILE_KIND_UNKNOWN: &str = "unknown";
pub(crate) const FILE_KIND_SERIES: &str = "series";
pub(crate) const FILE_KIND_MOVIE: &str = "movie";

/// The store options: no per-write fsync, an 8 MiB delta budget before a
/// self-compacting rewrite, and a generous value cap.
pub(crate) fn store_options() -> store::Options {
    store::Options {
        sync_on_commit: false,
        compact_threshold: 8 << 20,
        max_value_size: 4 << 20,
    }
}

/// One indexed video file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FileRecord {
    pub path: String,
    pub name: String,
    pub ext: String,
    pub size: i64,
    pub mtime: i64,
    pub kind: String,
    pub title: String,
    /// Go `Season int` (`go\internal\library\index.go:127`): 64-bit, so a store
    /// record written by Go with a value above `i32::MAX` decodes unchanged.
    pub season: i64,
    pub episode: i64,
    pub year: i64,
    pub absolute: bool,
    pub meta_key: String,
    pub state: String,
    pub attempts: i64,
    pub last_error: String,
    pub last_try: i64,
    pub next_try: i64,
}

impl FileRecord {
    /// Rebuilds the query this record was written from, exactly as the live
    /// path builds it: [`metadata_core::context::resolve_query`] over the stored
    /// filename plus the stored full path, so a folder-only show/season is
    /// recovered at repair time and the current parser replaces stale stored
    /// columns (a scan leaves unchanged files' columns untouched, so an index
    /// written by an older parser would otherwise never repair).
    ///
    /// The stored columns are the fallback only when the path is unusable
    /// (empty); no schema is changed and no rescan is needed.
    pub fn query(&self) -> metadata::Query {
        if !self.path.trim().is_empty() {
            return metadata_core::context::resolve_query(&self.name, &self.path);
        }
        self.stored_query()
    }

    /// The query from the stored columns alone: the fallback for a record whose
    /// path is unusable (see [`FileRecord::query`]).
    fn stored_query(&self) -> metadata::Query {
        let kind = match self.kind.as_str() {
            FILE_KIND_SERIES => metadata::Kind::SERIES,
            FILE_KIND_MOVIE => metadata::Kind::MOVIE,
            _ => metadata::Kind::UNKNOWN,
        };
        metadata::Query {
            kind,
            title: self.title.clone(),
            year: self.year as i32,
            season: self.season,
            episode: self.episode as i32,
            absolute: self.absolute,
        }
    }
}

/// The index-level record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct IndexMeta {
    pub schema: i32,
    pub migrated_at: i64,
    pub migrated_folders: i64,
}

/// A point-in-time tally over the index's records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexState {
    pub folders: i64,
    pub files: i64,
    pub keys: i64,
    pub files_resolved: i64,
    pub files_no_key: i64,
    pub files_pending: i64,
    pub files_needs_repair: i64,
    pub keys_resolved: i64,
}

#[derive(Default)]
pub(crate) struct IndexInner {
    pub folders: HashMap<String, FolderRecord>,
    pub files: HashMap<String, FileRecord>,
    pub meta: IndexMeta,
    pub dirty_folders: HashMap<String, bool>,
    pub dirty_files: HashMap<String, bool>,
    pub gone_files: HashMap<String, bool>,
    pub dirty_meta: bool,
    pub scratch: Vec<u8>,
}

impl IndexInner {
    fn dirty(&self) -> bool {
        !self.dirty_folders.is_empty()
            || !self.dirty_files.is_empty()
            || !self.gone_files.is_empty()
            || self.dirty_meta
    }
}

/// The store-backed index. Records live in memory; the store file is opened
/// only for a commit.
pub struct Index {
    pub(crate) path: String,
    pub(crate) legacy: String,
    pub(crate) inner: Mutex<IndexInner>,
    pub(crate) open_err: Mutex<Option<Error>>,
}

/// `IndexStorePath` maps a legacy index path to the store file replacing it.
pub fn index_store_path(index_path: &str) -> String {
    if index_path.is_empty() {
        return String::new();
    }
    path_join(&[&path_dir(index_path), INDEX_STORE_NAME])
}


static LIVE_INDEXES: Mutex<Option<HashMap<String, Arc<Index>>>> = Mutex::new(None);

fn live() -> std::sync::MutexGuard<'static, Option<HashMap<String, Arc<Index>>>> {
    LIVE_INDEXES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The process-wide index for `index_path`, opened on first use.
pub(crate) fn shared_index(index_path: &str) -> Result<Arc<Index>, Error> {
    let sp = index_store_path(index_path);
    if sp.is_empty() {
        return Err(Error::new("library: index: empty index path"));
    }
    let mut guard = live();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(ix) = map.get(&sp) {
        return Ok(Arc::clone(ix));
    }
    let ix = Arc::new(open_index(index_path));
    map.insert(sp, Arc::clone(&ix));
    Ok(ix)
}

/// Drops the index for `index_path`, so the next use reopens and re-reads it.
pub fn close_index(index_path: &str) -> Result<(), Error> {
    let sp = index_store_path(index_path);
    if sp.is_empty() {
        return Err(Error::new("library: index: empty index path"));
    }
    live().as_mut().map(|m| m.remove(&sp));
    Ok(())
}

/// Forgets and deletes the store-backed index for `index_path`, returning the
/// folder-record count it held.
pub fn clear_store_index(index_path: &str) -> Result<i64, Error> {
    let sp = index_store_path(index_path);
    if sp.is_empty() {
        return Err(Error::new("library: index: empty index path"));
    }
    let folders = shared_index(index_path)
        .map(|ix| ix.folder_count())
        .unwrap_or(0);
    close_index(index_path)?;
    match std::fs::remove_file(&sp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(Error::new(format!("library: index: remove {sp}: {e}"))),
    }
    Ok(folders)
}

fn open_index(index_path: &str) -> Index {
    let path = index_store_path(index_path);
    let ix = Index {
        path: path.clone(),
        legacy: index_path.to_string(),
        inner: Mutex::new(IndexInner::default()),
        open_err: Mutex::new(None),
    };
    if let Some(parent) = std::path::Path::new(&path).parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            *ix.open_err.lock().unwrap() = Some(Error::new(format!(
                "library: index: mkdir {}: {e}",
                parent.display()
            )));
            return ix;
        }
    }
    let mut db = match store::open(&path, store_options()) {
        Ok(db) => Some(db),
        Err(err) => {
            let quarantine = format!("{path}.corrupt");
            if std::fs::rename(&path, &quarantine).is_ok() {
                eprintln!(
                    "library: index {path:?}: {err} (quarantined as {})",
                    std::path::Path::new(&quarantine)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                );
                store::open(&path, store_options()).ok()
            } else {
                None
            }
        }
    };
    let db = match db.take() {
        Some(db) => db,
        None => {
            *ix.open_err.lock().unwrap() = Some(Error::new(format!(
                "library: index: open {path}: unreadable"
            )));
            return ix;
        }
    };
    ix.decode_all(&db);
    let empty = db.is_empty();
    if let Err(e) = db.close() {
        let mut oe = ix.open_err.lock().unwrap();
        if oe.is_none() {
            *oe = Some(Error::new(format!("library: index: close {path}: {e}")));
        }
    }
    if empty {
        ix.import_legacy();
    }
    ix
}

impl Index {
    /// Rebuilds the in-memory maps from an open store. A value that fails to decode
    /// is dropped and counted to stderr.
    fn decode_all(&self, db: &store::Db) {
        let mut bad = 0;
        let mut inner = self.inner.lock().unwrap();
        let _ = db.for_each(|k, v| {
            if k.len() > 2 && &k[..2] == REC_KEY_FILE.as_bytes() {
                match decode_file_record(v) {
                    Some(rec) => {
                        inner
                            .files
                            .insert(String::from_utf8_lossy(&k[2..]).into_owned(), rec);
                    }
                    None => bad += 1,
                }
            } else if k.len() > 2 && &k[..2] == REC_KEY_FOLDER.as_bytes() {
                match decode_folder_record(v) {
                    Some(rec) => {
                        inner
                            .folders
                            .insert(String::from_utf8_lossy(&k[2..]).into_owned(), rec);
                    }
                    None => bad += 1,
                }
            } else if k == REC_KEY_META.as_bytes() {
                if let Some(m) = decode_index_meta(v) {
                    inner.meta = m;
                }
            }
            true
        });
        if bad > 0 {
            eprintln!(
                "library: index {:?}: dropped {bad} undecodable record(s)",
                self.path
            );
        }
    }

    /// Imports the legacy JSON index once, when the store is empty and the JSON
    /// exists, then renames the JSON to `<name>.bak` - never deletes it.
    fn import_legacy(&self) {
        if self.legacy.is_empty() {
            return;
        }
        match std::fs::metadata(&self.legacy) {
            Ok(md) if !md.is_dir() => {}
            _ => return,
        }
        let (raw, counts) = match read_legacy_scan_index(&self.legacy) {
            Ok(v) => v,
            Err(err) => {
                eprintln!(
                    "library: index migrate {:?}: {err} (importing nothing)",
                    self.legacy
                );
                (HashMap::new(), LegacyCounts::default())
            }
        };
        {
            let mut inner = self.inner.lock().unwrap();
            for (dir, rec) in raw {
                let key = normalize_scan_dir(&dir);
                let mut rec = rec;
                rec.per_file = false; // the legacy file has no per-file records
                inner.folders.insert(key.clone(), rec);
                inner.dirty_folders.insert(key, true);
            }
            inner.meta = IndexMeta {
                schema: metadata::cache::cache_schema(),
                migrated_at: now_unix_nanos(),
                migrated_folders: counts.folders,
            };
            inner.dirty_meta = true;
            if let Err(err) = self.commit_locked(&mut inner) {
                eprintln!("library: index migrate {:?}: commit: {err}", self.legacy);
                return;
            }
        }
        let bak = format!("{}{}", self.legacy, LEGACY_BAK_SUFFIX);
        if let Err(err) = std::fs::rename(&self.legacy, &bak) {
            eprintln!(
                "library: index migrate {:?}: rename to {bak:?}: {err}",
                self.legacy
            );
            return;
        }
        eprintln!(
            "library: index migrated {} folder records ({} files, {} keys) into {:?} from {:?}; per-file records are absent in the legacy file, so the next scan enumerates files normally (legacy file renamed {:?})",
            counts.folders,
            counts.files,
            counts.keys,
            self.path,
            self.legacy,
            std::path::Path::new(&bak)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
    }

    /// Flushes staged records and fsyncs them.
    pub(crate) fn commit(&self) -> Result<(), Error> {
        let mut inner = self.inner.lock().unwrap();
        self.commit_locked(&mut inner)
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.inner.lock().unwrap().dirty()
    }

    fn commit_locked(&self, inner: &mut IndexInner) -> Result<(), Error> {
        if !inner.dirty() {
            return Ok(());
        }
        let db = self.open_store_locked()?;
        let res = write_all(inner, &db, false)
            .and_then(|()| db.commit().map_err(store_err))
            .and_then(|()| db.close().map_err(store_err));
        match res {
            Ok(()) => {
                clear_dirty(inner);
                Ok(())
            }
            Err(err) => {
                let _ = db.close();
                Err(err)
            }
        }
    }

    fn open_store_locked(&self) -> Result<store::Db, Error> {
        match store::open(&self.path, store_options()) {
            Ok(db) => {
                *self.open_err.lock().unwrap() = None;
                Ok(db)
            }
            Err(err) => {
                let e = Error::new(format!("library: index: open {}: {err}", self.path));
                *self.open_err.lock().unwrap() = Some(e.clone());
                Err(e)
            }
        }
    }
}

fn write_all(inner: &mut IndexInner, db: &store::Db, all: bool) -> Result<(), Error> {
    let put_files: Vec<String> = if all {
        inner.files.keys().cloned().collect()
    } else {
        inner.dirty_files.keys().cloned().collect()
    };
    for k in &put_files {
        let Some(rec) = inner.files.get(k) else {
            continue;
        };
        let mut buf = std::mem::take(&mut inner.scratch);
        buf.clear();
        encode_file_record(&mut buf, rec);
        let res = db.put(format!("{REC_KEY_FILE}{k}").as_bytes(), &buf);
        inner.scratch = buf;
        res.map_err(store_err)?;
    }
    for k in inner.gone_files.keys() {
        db.delete(format!("{REC_KEY_FILE}{k}").as_bytes())
            .map_err(store_err)?;
    }
    let put_folders: Vec<String> = if all {
        inner.folders.keys().cloned().collect()
    } else {
        inner.dirty_folders.keys().cloned().collect()
    };
    for k in &put_folders {
        match inner.folders.get(k) {
            Some(rec) => {
                let mut buf = std::mem::take(&mut inner.scratch);
                buf.clear();
                encode_folder_record(&mut buf, rec);
                let res = db.put(format!("{REC_KEY_FOLDER}{k}").as_bytes(), &buf);
                inner.scratch = buf;
                res.map_err(store_err)?;
            }
            None => {
                db.delete(format!("{REC_KEY_FOLDER}{k}").as_bytes())
                    .map_err(store_err)?;
            }
        }
    }
    if inner.dirty_meta || all {
        let mut buf = std::mem::take(&mut inner.scratch);
        buf.clear();
        encode_index_meta(&mut buf, inner.meta);
        let res = db.put(REC_KEY_META.as_bytes(), &buf);
        inner.scratch = buf;
        res.map_err(store_err)?;
    }
    Ok(())
}

fn clear_dirty(inner: &mut IndexInner) {
    inner.dirty_folders.clear();
    inner.dirty_files.clear();
    inner.gone_files.clear();
    inner.dirty_meta = false;
}

fn store_err(e: store::Error) -> Error {
    Error::new(format!("library: index: store: {e}"))
}

impl Index {
    /// Persists the index into the store derived from `legacy_path`. When that is
    /// the index's own store, only staged records are written; otherwise every
    /// record is written there with a dedicated handle.
    pub(crate) fn flush_to(&self, legacy_path: &str) -> Result<(), Error> {
        let sp = index_store_path(legacy_path);
        if sp.is_empty() {
            return Err(Error::new("library: index: empty index path"));
        }
        let mut inner = self.inner.lock().unwrap();
        if sp == self.path {
            return self.commit_locked(&mut inner);
        }
        if let Some(parent) = std::path::Path::new(&sp).parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Err(Error::new(format!(
                    "library: index: mkdir {}: {e}",
                    parent.display()
                )));
            }
        }
        let db = store::open(&sp, store_options()).map_err(store_err)?;
        let res = write_all(&mut inner, &db, true).and_then(|()| db.commit().map_err(store_err));
        match res {
            Ok(()) => {
                clear_dirty(&mut inner);
                db.close().map_err(store_err)
            }
            Err(err) => {
                let _ = db.close();
                Err(err)
            }
        }
    }


    pub(crate) fn state(&self) -> IndexState {
        state_of(&self.inner.lock().unwrap())
    }
}

fn state_of(inner: &IndexInner) -> IndexState {
    let mut s = IndexState {
        folders: inner.folders.len() as i64,
        files: inner.files.len() as i64,
        ..IndexState::default()
    };
    let mut unresolved: HashMap<String, bool> = HashMap::new();
    for r in inner.files.values() {
        match r.state.as_str() {
            STATE_RESOLVED => s.files_resolved += 1,
            STATE_NEEDS_REPAIR => s.files_needs_repair += 1,
            _ => s.files_pending += 1,
        }
        if r.meta_key.is_empty() {
            if r.state == STATE_RESOLVED {
                s.files_resolved -= 1;
            } else {
                s.files_pending -= 1;
            }
            s.files_no_key += 1;
            continue;
        }
        if r.state != STATE_RESOLVED {
            unresolved.insert(r.meta_key.clone(), true);
        } else {
            unresolved.entry(r.meta_key.clone()).or_insert(false);
        }
    }
    for bad in unresolved.values() {
        s.keys += 1;
        if !bad {
            s.keys_resolved += 1;
        }
    }
    s
}


impl Index {
    pub(crate) fn folder(&self, dir: &str) -> Option<FolderRecord> {
        self.inner
            .lock()
            .unwrap()
            .folders
            .get(&normalize_scan_dir(dir))
            .cloned()
    }

    pub(crate) fn put_folder(&self, dir: &str, rec: FolderRecord) {
        let mut inner = self.inner.lock().unwrap();
        let key = normalize_scan_dir(dir);
        if let Some(prev) = inner.folders.get(&key) {
            if same_scan_record(prev, &rec) {
                return;
            }
        }
        inner.folders.insert(key.clone(), rec);
        inner.dirty_folders.insert(key, true);
    }

    pub fn folder_names(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<String> = inner.folders.keys().cloned().collect();
        out.sort();
        out
    }

    pub(crate) fn folder_count(&self) -> i64 {
        self.inner.lock().unwrap().folders.len() as i64
    }

    /// Drops rollups whose directory no longer exists; one stat per folder.
    pub(crate) fn prune_folders(&self) -> i64 {
        let mut inner = self.inner.lock().unwrap();
        let mut n = 0;
        let keys: Vec<String> = inner.folders.keys().cloned().collect();
        for k in keys {
            if !file_exists(&k) {
                inner.folders.remove(&k);
                inner.dirty_folders.insert(k, true);
                n += 1;
            }
        }
        n
    }


    pub(crate) fn file(&self, path: &str) -> Option<FileRecord> {
        self.inner
            .lock()
            .unwrap()
            .files
            .get(&normalize_scan_dir(path))
            .cloned()
    }

    pub(crate) fn put_file(&self, rec: FileRecord) {
        let mut inner = self.inner.lock().unwrap();
        put_file_locked(&mut inner, rec);
    }
}

pub(crate) fn put_file_locked(inner: &mut IndexInner, rec: FileRecord) {
    let key = normalize_scan_dir(&rec.path);
    inner.files.insert(key.clone(), rec);
    inner.dirty_files.insert(key.clone(), true);
    inner.gone_files.remove(&key);
}

impl Index {
    pub(crate) fn delete_file(&self, path: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        Self::delete_file_locked(&mut inner, path)
    }

    fn delete_file_locked(inner: &mut IndexInner, path: &str) -> bool {
        let key = normalize_scan_dir(path);
        if inner.files.remove(&key).is_none() {
            return false;
        }
        inner.dirty_files.remove(&key);
        inner.gone_files.insert(key, true);
        true
    }

    /// Copies every file record, ordered by path.
    pub(crate) fn files_snapshot(&self) -> Vec<FileRecord> {
        let inner = self.inner.lock().unwrap();
        let mut out: Vec<FileRecord> = inner.files.values().cloned().collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Deletes every record whose parent was visited but whose path was not
    /// enumerated in that visit.
    pub(crate) fn prune_files(
        &self,
        visited_dirs: &HashMap<String, bool>,
        enumerated: &HashMap<String, bool>,
    ) -> i64 {
        let mut inner = self.inner.lock().unwrap();
        let mut n = 0;
        let keys: Vec<String> = inner.files.keys().cloned().collect();
        for key in keys {
            if enumerated.contains_key(&key) {
                continue;
            }
            let rec = inner.files.get(&key).cloned().unwrap();
            if !visited_dirs.contains_key(&normalize_scan_dir(&path_dir(&rec.path))) {
                continue;
            }
            inner.files.remove(&key);
            inner.dirty_files.remove(&key);
            inner.gone_files.insert(key, true);
            n += 1;
        }
        n
    }
}


pub(crate) fn put_uvarint(dst: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        dst.push((v as u8) | 0x80);
        v >>= 7;
    }
    dst.push(v as u8);
}

pub(crate) fn get_uvarint(b: &[u8], n: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    let mut shift = 0;
    let mut i = n;
    while i < b.len() {
        let byte = b[i];
        if shift >= 64 {
            return None;
        }
        v |= ((byte & 0x7f) as u64) << shift;
        i += 1;
        if byte < 0x80 {
            return Some((v, i));
        }
        shift += 7;
    }
    None
}

pub(crate) fn put_string(dst: &mut Vec<u8>, s: &str) {
    put_uvarint(dst, s.len() as u64);
    dst.extend_from_slice(s.as_bytes());
}

pub(crate) fn get_string(b: &[u8], n: usize) -> Option<(String, usize)> {
    let (l, off) = get_uvarint(b, n)?;
    let l = l as usize;
    if off + l > b.len() {
        return None;
    }
    Some((
        String::from_utf8_lossy(&b[off..off + l]).into_owned(),
        off + l,
    ))
}

fn kind_code(kind: &str) -> u8 {
    match kind {
        FILE_KIND_MOVIE => 2,
        FILE_KIND_SERIES => 1,
        _ => 0,
    }
}

fn kind_from_code(c: u8) -> &'static str {
    match c {
        2 => FILE_KIND_MOVIE,
        1 => FILE_KIND_SERIES,
        _ => FILE_KIND_UNKNOWN,
    }
}

fn state_code(state: &str) -> u8 {
    match state {
        STATE_RESOLVED => 1,
        STATE_NEEDS_REPAIR => 2,
        _ => 0,
    }
}

fn state_from_code(c: u8) -> &'static str {
    match c {
        1 => STATE_RESOLVED,
        2 => STATE_NEEDS_REPAIR,
        _ => STATE_PENDING,
    }
}

pub(crate) fn encode_file_record(dst: &mut Vec<u8>, r: &FileRecord) {
    dst.push(ENC_FILE_VERSION);
    put_string(dst, &r.path);
    put_string(dst, &r.name);
    put_string(dst, &r.ext);
    put_uvarint(dst, r.size as u64);
    put_uvarint(dst, r.mtime as u64);
    dst.push(kind_code(&r.kind));
    put_string(dst, &r.title);
    put_uvarint(dst, r.season.max(0) as u64);
    put_uvarint(dst, r.episode.max(0) as u64);
    put_uvarint(dst, r.year.max(0) as u64);
    put_string(dst, &r.meta_key);
    dst.push(state_code(&r.state));
    put_uvarint(dst, r.attempts.max(0) as u64);
    put_string(dst, &r.last_error);
    put_uvarint(dst, r.last_try as u64);
    put_uvarint(dst, r.next_try as u64);
    dst.push(if r.absolute { 1 } else { 0 });
}

/// Decodes one file record; `None` for a truncated or wrong-version payload or
/// a trailing byte.
pub(crate) fn decode_file_record(b: &[u8]) -> Option<FileRecord> {
    if b.is_empty() || b[0] != ENC_FILE_VERSION {
        return None;
    }
    let mut r = FileRecord::default();
    let mut n = 1;
    let (path, nn) = get_string(b, n)?;
    r.path = path;
    n = nn;
    let (name, nn) = get_string(b, n)?;
    r.name = name;
    n = nn;
    let (ext, nn) = get_string(b, n)?;
    r.ext = ext;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    r.size = v as i64;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    r.mtime = v as i64;
    n = nn;
    if n >= b.len() {
        return None;
    }
    r.kind = kind_from_code(b[n]).to_string();
    n += 1;
    let (title, nn) = get_string(b, n)?;
    r.title = title;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    r.season = v as i64;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    r.episode = v as i64;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    r.year = v as i64;
    n = nn;
    let (meta_key, nn) = get_string(b, n)?;
    r.meta_key = meta_key;
    n = nn;
    if n >= b.len() {
        return None;
    }
    r.state = state_from_code(b[n]).to_string();
    n += 1;
    let (v, nn) = get_uvarint(b, n)?;
    r.attempts = v as i64;
    n = nn;
    let (last_error, nn) = get_string(b, n)?;
    r.last_error = last_error;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    r.last_try = v as i64;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    r.next_try = v as i64;
    n = nn;
    if n >= b.len() {
        return None;
    }
    r.absolute = b[n] == 1;
    n += 1;
    if n != b.len() {
        return None;
    }
    Some(r)
}

pub(crate) fn encode_folder_record(dst: &mut Vec<u8>, r: &FolderRecord) {
    dst.push(ENC_FOLDER_VERSION);
    put_uvarint(dst, r.schema.max(0) as u64);
    put_uvarint(dst, r.scanned_at_nanos as u64);
    put_uvarint(dst, r.files.max(0) as u64);
    dst.push(if r.per_file { 1 } else { 0 });
    put_string(dst, &r.names_hash);
    put_string(dst, &r.fingerprint);
    put_uvarint(dst, r.keys.len() as u64);
    for k in &r.keys {
        put_string(dst, k);
    }
}

pub(crate) fn decode_folder_record(b: &[u8]) -> Option<FolderRecord> {
    if b.is_empty() || b[0] != ENC_FOLDER_VERSION {
        return None;
    }
    let mut n = 1;
    let (v, nn) = get_uvarint(b, n)?;
    let schema = v as i32;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    let scanned_at_nanos = v as i64;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    let files = v as i64;
    n = nn;
    if n >= b.len() {
        return None;
    }
    let per_file = b[n] == 1;
    n += 1;
    let (names_hash, nn) = get_string(b, n)?;
    n = nn;
    let (fingerprint, nn) = get_string(b, n)?;
    n = nn;
    let (v, nn) = get_uvarint(b, n)?;
    let count = v as usize;
    n = nn;
    if count > b.len() {
        return None;
    }
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        let (k, nn) = get_string(b, n)?;
        keys.push(k);
        n = nn;
    }
    if n != b.len() {
        return None;
    }
    Some(FolderRecord {
        fingerprint,
        names_hash,
        files,
        schema,
        keys,
        scanned_at_nanos,
        per_file,
    })
}

fn encode_index_meta(dst: &mut Vec<u8>, m: IndexMeta) {
    put_uvarint(dst, m.schema.max(0) as u64);
    put_uvarint(dst, m.migrated_at as u64);
    put_uvarint(dst, m.migrated_folders.max(0) as u64);
}

fn decode_index_meta(b: &[u8]) -> Option<IndexMeta> {
    let (v, n) = get_uvarint(b, 0)?;
    let schema = v as i32;
    let (v, n) = get_uvarint(b, n)?;
    let migrated_at = v as i64;
    let (v, n) = get_uvarint(b, n)?;
    let migrated_folders = v as i64;
    if n != b.len() {
        return None;
    }
    Some(IndexMeta {
        schema,
        migrated_at,
        migrated_folders,
    })
}

/// The current wall clock in unix nanoseconds, like Go's `time.Now().UnixNano()`.
pub(crate) fn now_unix_nanos() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(_) => 0,
    }
}
