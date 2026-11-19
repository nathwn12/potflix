
use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;

pub(crate) use crate::pathutil::normalize_scan_dir;

use crate::Error;
use crate::index::{Index, now_unix_nanos, shared_index};
use crate::sha256::{digest, hex};

/// `FolderRecord` is one indexed folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderRecord {
    pub fingerprint: String,
    pub names_hash: String,
    pub files: i64,
    pub schema: i32,
    pub keys: Vec<String>,
    pub scanned_at_nanos: i64,
    pub per_file: bool,
}

/// Whether two records carry the same content (ScannedAt excluded).
pub(crate) fn same_scan_record(a: &FolderRecord, b: &FolderRecord) -> bool {
    a.fingerprint == b.fingerprint
        && a.names_hash == b.names_hash
        && a.files == b.files
        && a.schema == b.schema
        && a.per_file == b.per_file
        && a.keys == b.keys
}

/// One grandchild entry's hashed fields (name + size only).
#[derive(Clone, Debug)]
pub(crate) struct FpSub {
    pub name: String,
    pub size: i64,
}

/// One directory entry's hashed fields for the tier-2 fingerprint.
#[derive(Clone, Debug)]
pub(crate) struct FpEntry {
    pub name: String,
    pub size: i64,
    pub mtime_nanos: i64,
    pub is_dir: bool,
    pub sub: Vec<FpSub>,
}

/// The tier-1 hash core.
pub(crate) fn hash_names(mod_nanos: i64, count: usize, names: &[String]) -> String {
    let mut s = format!("{mod_nanos}\n{count}\n");
    for n in names {
        s.push_str(n);
        s.push('\n');
    }
    hex(&digest(s.as_bytes()))
}

/// The tier-2 hash core over entries sorted by name.
pub(crate) fn hash_fingerprint(mod_nanos: i64, entries: &[FpEntry]) -> String {
    let mut s = format!("{mod_nanos}\n{}\n", entries.len());
    for e in entries {
        s.push_str(&format!("{}\n{}\n{}\n", e.name, e.size, e.mtime_nanos));
        if e.is_dir {
            s.push_str(&format!("{}\n", e.sub.len()));
            for sub in &e.sub {
                s.push_str(&format!("{}\n{}\n", sub.name, sub.size));
            }
        }
    }
    hex(&digest(s.as_bytes()))
}

fn mtime_nanos(md: &std::fs::Metadata) -> i64 {
    match md.modified() {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

/// A cheap identity for a directory's contents (tier 2).
pub(crate) fn folder_fingerprint(dir: &str) -> std::io::Result<String> {
    let md = std::fs::metadata(dir)?;
    if !md.is_dir() {
        return Err(std::io::Error::other(format!(
            "library: scanindex: not a directory {dir:?}"
        )));
    }
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    let mut fps = Vec::with_capacity(entries.len());
    for e in &entries {
        let info = e.metadata()?;
        let name = e.file_name().to_string_lossy().into_owned();
        let is_dir = e.file_type()?.is_dir();
        let mut fe = FpEntry {
            name,
            size: info.len() as i64,
            mtime_nanos: mtime_nanos(&info),
            is_dir,
            sub: Vec::new(),
        };
        if is_dir {
            let sub_path = std::path::Path::new(dir).join(e.file_name());
            let mut sub: Vec<std::fs::DirEntry> =
                std::fs::read_dir(&sub_path)?.collect::<Result<_, _>>()?;
            sub.sort_by_key(|s| s.file_name());
            for s in &sub {
                let sinfo = s.metadata()?;
                fe.sub.push(FpSub {
                    name: s.file_name().to_string_lossy().into_owned(),
                    size: sinfo.len() as i64,
                });
            }
        }
        fps.push(fe);
    }
    Ok(hash_fingerprint(mtime_nanos(&md), &fps))
}

/// The cheap tier-1 identity for a directory.
pub fn folder_names_hash(dir: &str) -> std::io::Result<String> {
    let md = std::fs::metadata(dir)?;
    if !md.is_dir() {
        return Err(std::io::Error::other(format!(
            "library: scanindex: not a directory {dir:?}"
        )));
    }
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(hash_names(mtime_nanos(&md), names.len(), &names))
}


const MAX_SCAN_INDEX_BYTES: u64 = 8 << 20;

/// `LegacyCounts` is the inventory of a legacy scanindex.json.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LegacyCounts {
    pub folders: i64,
    pub files: i64,
    pub keys: i64,
    pub key_refs: i64,
}

#[derive(Deserialize)]
struct LegacyFolderRecord {
    #[serde(default)]
    fingerprint: String,
    #[serde(default)]
    names_hash: String,
    #[serde(default)]
    files: i64,
    #[serde(default)]
    schema: i32,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    scanned_at: Option<String>,
}

/// Reads the retired JSON index, tolerating a missing, unreadable, oversized
/// or corrupt file as an empty map plus an error.
pub(crate) fn read_legacy_scan_index(
    path: &str,
) -> Result<(HashMap<String, FolderRecord>, LegacyCounts), Error> {
    let mut counts = LegacyCounts::default();
    let file = std::fs::File::open(path)
        .map_err(|e| Error::new(format!("library: scanindex: open {path:?}: {e}")))?;
    let md = file
        .metadata()
        .map_err(|e| Error::new(format!("library: scanindex: stat {path:?}: {e}")))?;
    if md.len() > MAX_SCAN_INDEX_BYTES {
        return Err(Error::new(format!(
            "legacy index {path:?} exceeds {MAX_SCAN_INDEX_BYTES} bytes"
        )));
    }
    use std::io::Read;
    let mut data = Vec::new();
    file.take(MAX_SCAN_INDEX_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|e| Error::new(format!("library: scanindex: read {path:?}: {e}")))?;
    if data.len() as u64 > MAX_SCAN_INDEX_BYTES {
        return Err(Error::new(format!(
            "legacy index {path:?} exceeds {MAX_SCAN_INDEX_BYTES} bytes"
        )));
    }
    let raw: HashMap<String, LegacyFolderRecord> = serde_json::from_slice(&data)
        .map_err(|e| Error::new(format!("library: scanindex: decode {path:?}: {e}")))?;
    let mut out = HashMap::with_capacity(raw.len());
    let mut keys: HashMap<String, bool> = HashMap::new();
    for (k, v) in raw {
        let scanned_at_nanos = v
            .scanned_at
            .as_deref()
            .and_then(repair::time::Time::parse_rfc3339)
            .map(|t| t.unix_seconds().saturating_mul(1_000_000_000) + t.subsec_nanos() as i64)
            .unwrap_or(0);
        counts.files += v.files;
        for key in &v.keys {
            if !key.is_empty() {
                keys.insert(key.clone(), true);
                counts.key_refs += 1;
            }
        }
        out.insert(
            normalize_scan_dir(&k),
            FolderRecord {
                fingerprint: v.fingerprint,
                names_hash: v.names_hash,
                files: v.files,
                schema: v.schema,
                keys: v.keys,
                scanned_at_nanos,
                per_file: false,
            },
        );
    }
    counts.folders = out.len() as i64;
    counts.keys = keys.len() as i64;
    Ok((out, counts))
}

/// Reports the folder/file/key totals of the legacy JSON index at `path`.
pub fn legacy_scan_index_counts(path: &str) -> Result<LegacyCounts, Error> {
    read_legacy_scan_index(path).map(|(_, counts)| counts)
}


/// The folder tier of the store-backed library index.
#[derive(Clone)]
pub struct ScanIndex {
    pub(crate) ix: Arc<Index>,
}

/// Opens the store-backed index that replaces `path` and imports a legacy
/// `scanindex.json` on first use of an empty store.
pub fn load_scan_index(path: &str) -> ScanIndex {
    match shared_index(path) {
        Ok(ix) => ScanIndex { ix },
        Err(err) => {
            eprintln!("library: index {path:?}: {err}");
            ScanIndex {
                ix: Arc::new(Index {
                    path: String::new(),
                    legacy: String::new(),
                    inner: std::sync::Mutex::new(crate::index::IndexInner::default()),
                    open_err: std::sync::Mutex::new(None),
                }),
            }
        }
    }
}

impl ScanIndex {
    /// Persists every staged record into the store derived from `path`.
    pub fn save(&self, path: &str) -> Result<(), Error> {
        self.ix.flush_to(path)
    }

    /// Persists the index only when records actually changed.
    pub fn save_if_dirty(&self, path: &str) -> Result<(), Error> {
        if !self.ix.is_dirty() {
            return Ok(());
        }
        self.ix.flush_to(path)
    }

    /// Whether `dir` can be skipped entirely (tier 2).
    pub fn should_skip(
        &self,
        dir: &str,
        fp: &str,
        schema: i32,
        key_present: Option<&dyn Fn(&str) -> bool>,
    ) -> bool {
        let Some(key_present) = key_present else {
            return false;
        };
        let Some(rec) = self.ix.folder(dir) else {
            return false;
        };
        if !rec.per_file || rec.fingerprint != fp || rec.schema != schema {
            return false;
        }
        if rec.files > 0 && rec.keys.is_empty() {
            return false;
        }
        if rec.files > 0 && rec.keys.len() as i64 > rec.files {
            return false;
        }
        for k in &rec.keys {
            if !key_present(k) {
                return false;
            }
        }
        true
    }

    /// The tier-1 skip check.
    pub fn should_skip_names(
        &self,
        dir: &str,
        names_hash: &str,
        schema: i32,
        key_present: Option<&dyn Fn(&str) -> bool>,
    ) -> bool {
        let Some(key_present) = key_present else {
            return false;
        };
        if names_hash.is_empty() {
            return false;
        }
        let Some(rec) = self.ix.folder(dir) else {
            return false;
        };
        if !rec.per_file || rec.names_hash.is_empty() || rec.names_hash != names_hash {
            return false;
        }
        if rec.schema != schema {
            return false;
        }
        if rec.files > 0 && rec.keys.is_empty() {
            return false;
        }
        if rec.files > 0 && rec.keys.len() as i64 > rec.files {
            return false;
        }
        for k in &rec.keys {
            if !key_present(k) {
                return false;
            }
        }
        true
    }

    /// A copy of the record for `dir`, if any.
    pub fn lookup(&self, dir: &str) -> Option<FolderRecord> {
        self.ix.folder(dir)
    }

    /// Stores (or replaces) the full two-tier record for `dir`.
    pub fn record_full(
        &self,
        dir: &str,
        names_hash: &str,
        fp: &str,
        schema: i32,
        keys: &[String],
        files: i64,
    ) {
        self.ix.put_folder(
            dir,
            FolderRecord {
                fingerprint: fp.to_string(),
                names_hash: names_hash.to_string(),
                files,
                schema,
                keys: keys.to_vec(),
                scanned_at_nanos: now_unix_nanos(),
                per_file: true,
            },
        );
    }

    /// Stores (or replaces) the record for `dir`, preserving any existing
    /// tier-1 fields.
    pub fn record(&self, dir: &str, fp: &str, schema: i32, keys: &[String]) {
        let prev = self.ix.folder(dir).unwrap_or_default();
        self.ix.put_folder(
            dir,
            FolderRecord {
                fingerprint: fp.to_string(),
                names_hash: prev.names_hash,
                files: prev.files,
                schema,
                keys: keys.to_vec(),
                scanned_at_nanos: now_unix_nanos(),
                per_file: true,
            },
        );
    }

    /// Counts indexed folders.
    pub fn len(&self) -> i64 {
        self.ix.folder_count()
    }

    /// The per-file/rollup tally behind this folder view.
    pub fn state(&self) -> crate::index::IndexState {
        self.ix.state()
    }

    /// Whether the index holds no folders.
    pub fn is_empty(&self) -> bool {
        self.ix.folder_count() == 0
    }

    /// Drops records whose directory no longer exists.
    pub fn prune_existing(&self) {
        self.ix.prune_folders();
    }
}
