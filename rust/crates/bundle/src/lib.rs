
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use metadata::cache;

#[cfg(not(windows))]
mod reparse_other;
#[cfg(windows)]
mod reparse_windows;

#[cfg(not(windows))]
use reparse_other::dir_has_reparse_attribute;
#[cfg(windows)]
use reparse_windows::dir_has_reparse_attribute;

/// `FormatName` identifies bundle payloads; `FormatVersion` is the bundle
/// envelope version (distinct from the cache schema carried per entry).
pub const FORMAT_NAME: &str = "potflix-metadata";

/// The current bundle envelope version (`bundle.go:39`).
pub const FORMAT_VERSION: i32 = 1;

/// `maxBundleBytes` caps how much `import` will read: a larger bundle is
/// refused so a hostile or huge file cannot exhaust memory (`bundle.go:43`).
const MAX_BUNDLE_BYTES: usize = 64 << 20; // 64 MB

/// `maxBundleTotalBytes` caps the aggregate bytes `export` will carry: beyond
/// it `export` fails rather than growing the bundle without bound
/// (`bundle.go:50`).
const MAX_BUNDLE_TOTAL_BYTES: u64 = 64 << 20; // 64 MB

/// `NoteText` is the export header note; it states the privacy scope of a
/// bundle in prose (`bundle.go:54`).
pub const NOTE_TEXT: &str = "This bundle contains metadata only: titles, synopsis, genres, length, button URLs and poster URLs. It contains no media files, no library paths, and no credentials.";

/// The bundle envelope header (`bundle.go:57`). Field order is the wire order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub version: i32,
    #[serde(default)]
    pub schema: i32,
    #[serde(default)]
    pub app_version: String,
    /// Go's `time.Time`, carried as its RFC3339 JSON text.
    #[serde(default)]
    pub exported_at: String,
    #[serde(default)]
    pub count: i32,
    #[serde(default)]
    pub note: String,
}

/// One cache file carried inside a bundle (`bundle.go:69`). `media` is opaque:
/// it is passed through without interpretation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// The 16-hex cache filename, e.g. `"ab12cd34ef56ab78.json"`.
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub schema: i32,
    /// Opaque passthrough (Go's `json.RawMessage`).
    #[serde(default)]
    pub media: Option<Box<RawValue>>,
    /// Carried verbatim: the record's freshness stamp rides the bundle so an
    /// export/import round-trip preserves the entry's remaining TTL instead of
    /// resetting it. Absent in bundles written before the positive TTL, which
    /// still parse (`default`) and import as undated — and therefore stale —
    /// records.
    #[serde(default)]
    pub checked_at: String,
}

/// The export/import envelope (`bundle.go:77`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    #[serde(default)]
    pub header: Header,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// Reports an `export` call (`bundle.go:83`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportStats {
    pub entries: i32,
    pub skipped: i32,
    pub bytes: i64,
}

/// Reports an `import` call (`bundle.go:90`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportStats {
    pub added: i32,
    pub present: i32,
    pub skipped_schema: i32,
    pub invalid: i32,
    pub total: i32,
}

/// The crate's error: Go's `error` return, carrying the same message text so a
/// caller can assert the limit or the refusal the way the Go tests did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<metadata::Error> for Error {
    fn from(err: metadata::Error) -> Error {
        Error::new(err.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::new(format!("bundle: io: {err}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Error {
        Error::new(format!("bundle: json: {err}"))
    }
}

/// `cacheRecord` mirrors the cache envelope the store carries: the record's own
/// key, schema, media and freshness stamp. Media stays raw so its bytes survive
/// an export/import round-trip unchanged (`bundle.go:102`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheRecord {
    #[serde(default)]
    key: String,
    #[serde(default)]
    schema: i32,
    #[serde(default)]
    media: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    checked_at: String,
}

/// `requireDir` refuses an empty cache directory (`bundle.go:112`): every
/// function here takes the directory explicitly, and an empty one would
/// otherwise resolve to the metadata package's DEFAULT cache directory.
fn require_dir(dir: &str) -> Result<(), Error> {
    if dir.trim().is_empty() {
        return Err(Error::new("bundle: no cache directory given"));
    }
    Ok(())
}

/// `validCacheName` reports whether `name` is a cache filename: 16 lowercase
/// hex chars plus `".json"` (`bundle.go:124`).
fn valid_cache_name(name: &str) -> bool {
    const SUFFIX: &str = ".json";
    if name.len() != 16 + SUFFIX.len() || !name.ends_with(SUFFIX) {
        return false;
    }
    name.as_bytes()[..16]
        .iter()
        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// `checkBundleDir` refuses a cache directory that is itself a reparse point
/// (`bundle.go:144`); a missing directory is not a refusal.
fn check_bundle_dir(dir: &str) -> Result<(), Error> {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() || dir_has_reparse_attribute(Path::new(dir)) {
        return Err(Error::new(format!(
            "bundle: refusing to operate on a reparse-point directory {dir:?}"
        )));
    }
    Ok(())
}

/// `isReparsePoint` reports whether `path` is itself a reparse point, checked
/// without following it (`bundle.go:161`).
fn is_reparse_point(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.file_type().is_symlink() || dir_has_reparse_attribute(path)
}

/// `Export` reads every entry in the live cache under `dir` and writes one
/// indented bundle to `w` (`bundle.go:182`). Entries are sorted by key so two
/// exports of the same cache are byte-identical over their entries.
pub fn export<W: Write>(
    dir: &str,
    w: &mut W,
    schema: i32,
    app_version: &str,
) -> Result<ExportStats, Error> {
    let mut st = ExportStats::default();
    require_dir(dir)?;
    check_bundle_dir(dir)?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut total: u64 = 0;
    let mut cap_err: Option<Error> = None;
    cache::cache_for_each(
        dir,
        Some(|_key: &str, value: &[u8]| -> bool {
            total += value.len() as u64;
            if total > MAX_BUNDLE_TOTAL_BYTES {
                cap_err = Some(Error::new(
                    "bundle: refusing export larger than 64 MB (aggregate entry bytes exceed the cap)",
                ));
                return false;
            }
            match serde_json::from_slice::<CacheRecord>(value) {
                Ok(rec) if !rec.key.is_empty() => entries.push(Entry {
                    file: cache::cache_file_name_for(&rec.key),
                    key: rec.key,
                    schema: rec.schema,
                    media: rec.media,
                    checked_at: rec.checked_at,
                }),
                _ => st.skipped += 1,
            }
            true
        }),
    )?;
    if let Some(err) = cap_err {
        return Err(err);
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    let count = entries.len() as i32;
    let b = Bundle {
        header: Header {
            format: FORMAT_NAME.to_string(),
            version: FORMAT_VERSION,
            schema,
            app_version: app_version.to_string(),
            exported_at: metadata::timefmt::now_rfc3339(),
            count,
            note: NOTE_TEXT.to_string(),
        },
        entries,
    };
    let mut data = serde_json::to_vec_pretty(&b)?;
    data.push(b'\n');
    st.bytes = w.write(&data)? as i64;
    st.entries = count;
    Ok(st)
}

/// `Import` merges a bundle from `r` into the live cache under `dir`:
/// fill-missing-only (`bundle.go:260`). It NEVER replaces an entry the cache
/// already holds.
pub fn import<R: Read>(dir: &str, r: &mut R, schema: i32) -> Result<ImportStats, Error> {
    let mut st = ImportStats::default();
    require_dir(dir)?;
    check_bundle_dir(dir)?;
    let mut data = Vec::new();
    let limit = MAX_BUNDLE_BYTES as u64 + 1;
    r.take(limit)
        .read_to_end(&mut data)
        .map_err(|err| Error::new(format!("bundle: read bundle: {err}")))?;
    if data.len() > MAX_BUNDLE_BYTES {
        return Err(Error::new(format!(
            "bundle: refusing bundle larger than 64 MB ({} bytes read)",
            data.len()
        )));
    }
    let b: Bundle = serde_json::from_slice(&data)
        .map_err(|err| Error::new(format!("bundle: invalid bundle JSON: {err}")))?;
    if b.header.format != FORMAT_NAME || b.header.version != FORMAT_VERSION {
        return Err(Error::new(format!(
            "bundle: unsupported bundle header: format {:?} version {} (want {FORMAT_NAME:?} version {FORMAT_VERSION})",
            b.header.format, b.header.version
        )));
    }
    st.total = b.entries.len() as i32;
    for e in &b.entries {
        if !valid_cache_name(&e.file) || e.key.is_empty() {
            st.invalid += 1;
            continue;
        }
        if e.schema != schema {
            st.skipped_schema += 1;
            continue;
        }
        if cache::cache_has(dir, &e.key) {
            st.present += 1;
            continue;
        }
        let rec = CacheRecord {
            key: e.key.clone(),
            schema: e.schema,
            media: e.media.clone(),
            checked_at: e.checked_at.clone(),
        };
        let payload = match serde_json::to_vec(&rec) {
            Ok(payload) => payload,
            Err(_) => {
                st.invalid += 1;
                continue;
            }
        };
        cache::cache_put_raw(dir, &e.key, &payload)?;
        st.added += 1;
    }
    Ok(st)
}

/// `Count` returns how many entries the live cache under `dir` holds
/// (`bundle.go:322`).
pub fn count(dir: &str) -> Result<i32, Error> {
    require_dir(dir)?;
    let mut n = 0i32;
    cache::cache_for_each(
        dir,
        Some(|_key: &str, _value: &[u8]| -> bool {
            n += 1;
            true
        }),
    )?;
    Ok(n)
}

/// `Clear` empties the live cache under `dir` and removes the legacy per-key
/// cache files the store replaced, plus `.tmp-` orphans, directly inside `dir`
/// (`bundle.go:347`). It never recurses and never removes `dir` itself.
pub fn clear(dir: &str) -> Result<i32, Error> {
    require_dir(dir)?;
    check_bundle_dir(dir)?;
    let cleared = cache::clear_cache_entries(dir)?;
    remove_legacy_cache_files(dir)?;
    Ok(cleared)
}

/// `removeLegacyCacheFiles` deletes the legacy per-key cache files directly
/// inside `dir` -- never another file, never a subdirectory, and never through
/// a reparse point (`bundle.go:370`).
fn remove_legacy_cache_files(dir: &str) -> Result<(), Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(Error::from(err)),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !valid_cache_name(&name) && !name.starts_with(".tmp-") {
            continue;
        }
        let full = entry.path();
        if is_reparse_point(&full) {
            continue; // never delete through a reparse point
        }
        match std::fs::remove_file(&full) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(Error::from(err)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
