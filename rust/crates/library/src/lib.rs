//! `library` -- mirrors the Go package `go\internal\library`.
//!
//! The shared walk + diff loop behind the tray window's Scan / Refresh, the
//! background metadata catch-up, and `metawarm`. The index is the store-backed
//! `library.idx` (`index.rs`: one record per video file plus a rollup per
//! folder); an indexed scan is enumerate + a `(size, mtime)` diff and performs
//! zero network fetches.
//!
//! This file is the module body for `go\internal\library\library.go`.

#![allow(
    clippy::collapsible_if,
    clippy::cloned_ref_to_slice_refs,
    clippy::collapsible_match
)]

pub mod catchup;
pub mod index;
pub mod scan;
pub mod scanindex;

mod cancel_link;
mod pathutil;
mod sha256;

#[cfg(test)]
mod catchup_test;
#[cfg(test)]
mod index_test;
#[cfg(test)]
mod library_test;
#[cfg(test)]
mod scanindex_test;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use metadata::Query;
pub use metadata_http::cancel::Cancel;

use cancel_link::CancelLink;
use pathutil::{
    base as path_base, dir as path_dir, ext as path_ext, join as path_join, normalize_scan_dir,
};
use scanindex::{
    FpEntry, FpSub, ScanIndex, folder_fingerprint as scan_folder_fingerprint, hash_fingerprint,
    hash_names,
};

pub(crate) use scanindex::load_scan_index;

pub(crate) const LOOKUP_TIMEOUT: Duration = Duration::from_secs(120);
/// `defaultConcurrency` bounds concurrent fetches.
pub(crate) const DEFAULT_CONCURRENCY: usize = 3;

pub(crate) const CHECKPOINT_EVERY_FOLDERS: usize = 50;
pub(crate) const CHECKPOINT_EVERY_INTERVAL: Duration = Duration::from_secs(2);

/// The video extensions the walk collects.
pub(crate) fn is_video_ext(ext: &str) -> bool {
    matches!(
        ext.to_lowercase().as_str(),
        ".mkv"
            | ".mp4"
            | ".avi"
            | ".m4v"
            | ".mov"
            | ".wmv"
            | ".ts"
            | ".flv"
            | ".webm"
            | ".mpg"
            | ".mpeg"
    )
}

pub(crate) fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

/// The crate's error: a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}


/// A resolver factory; tests replace the production one to inject fake sources.
pub(crate) type ResolverFactory = Arc<dyn Fn(&str) -> metadata::Resolver + Send + Sync>;

static RESOLVER_FACTORY: Mutex<Option<ResolverFactory>> = Mutex::new(None);

/// Builds the scan resolver; tests may replace the factory.
pub(crate) fn new_resolver(cache_dir: &str) -> metadata::Resolver {
    if let Some(f) = RESOLVER_FACTORY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        return f(cache_dir);
    }
    metadata::Resolver::new(metadata::Options {
        dir: cache_dir.to_string(),
        ..Default::default()
    })
}

#[cfg(test)]
pub(crate) fn set_resolver_factory(f: Option<ResolverFactory>) -> Option<ResolverFactory> {
    std::mem::replace(
        &mut *RESOLVER_FACTORY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        f,
    )
}

/// The per-file parser seam; tests replace it to count re-parses.
pub(crate) type ParseFn = fn(&str, &str) -> Query;

static PARSE_OVERRIDE: Mutex<Option<ParseFn>> = Mutex::new(None);

fn parse_query_default(raw: &str, path: &str) -> Query {
    metadata_core::context::resolve_query(raw, path)
}

pub(crate) fn current_parse() -> ParseFn {
    PARSE_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unwrap_or(parse_query_default as ParseFn)
}

#[cfg(test)]
pub(crate) fn set_parse_override(f: Option<ParseFn>) -> Option<ParseFn> {
    std::mem::replace(
        &mut *PARSE_OVERRIDE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        f,
    )
}

/// One walked entry's filesystem identity (the fields the library reads).
#[derive(Clone, Debug, Default)]
pub struct WalkEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime_nanos: i64,
    pub info_ok: bool,
}

/// What a walk callback asks the walker to do.
pub enum WalkResult {
    Continue,
    SkipDir,
    Fail(std::io::Error),
}

/// The walk callback: `(path, entry, err)` for each visited entry.
pub(crate) type WalkCb<'a> =
    dyn FnMut(&str, Option<&WalkEntry>, Option<&std::io::Error>) -> WalkResult + 'a;

pub(crate) type WalkFn = dyn Fn(&str, &mut WalkCb<'_>) -> std::io::Result<()> + Send + Sync;

static WALK_OVERRIDE: Mutex<Option<Arc<WalkFn>>> = Mutex::new(None);

pub(crate) fn walk_dir(root: &str, cb: &mut WalkCb<'_>) -> std::io::Result<()> {
    let f = WALK_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if let Some(f) = f {
        return f(root, cb);
    }
    real_walk_dir(root, cb)
}

#[cfg(test)]
pub(crate) fn set_walk_override(f: Option<Arc<WalkFn>>) -> Option<Arc<WalkFn>> {
    std::mem::replace(
        &mut *WALK_OVERRIDE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        f,
    )
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

fn entry_from_metadata(name: &str, is_dir: bool, md: &std::fs::Metadata) -> WalkEntry {
    WalkEntry {
        name: name.to_string(),
        is_dir,
        size: md.len() as i64,
        mtime_nanos: mtime_nanos(md),
        info_ok: true,
    }
}

fn cancelled_io() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")
}

/// The real walker: `filepath.WalkDir` semantics, entries sorted by name.
pub(crate) fn real_walk_dir(root: &str, cb: &mut WalkCb<'_>) -> std::io::Result<()> {
    match std::fs::symlink_metadata(root) {
        Err(e) => match cb(root, None, Some(&e)) {
            WalkResult::Fail(e2) => Err(e2),
            _ => Ok(()),
        },
        Ok(md) => {
            let name = path_base(root);
            let entry = entry_from_metadata(&name, md.is_dir(), &md);
            walk_rec(root, &entry, cb)
        }
    }
}

fn walk_rec(path: &str, entry: &WalkEntry, cb: &mut WalkCb<'_>) -> std::io::Result<()> {
    match cb(path, Some(entry), None) {
        WalkResult::Fail(e) => return Err(e),
        WalkResult::SkipDir => return Ok(()),
        WalkResult::Continue => {}
    }
    if !entry.is_dir {
        return Ok(());
    }
    let entries = match std::fs::read_dir(path) {
        Ok(iter) => match iter.collect::<Result<Vec<_>, _>>() {
            Ok(v) => {
                let mut v = v;
                v.sort_by_key(|e| e.file_name());
                v
            }
            Err(e) => {
                if let WalkResult::Fail(e2) = cb(path, Some(entry), Some(&e)) {
                    return Err(e2);
                }
                return Ok(());
            }
        },
        Err(e) => {
            if let WalkResult::Fail(e2) = cb(path, Some(entry), Some(&e)) {
                return Err(e2);
            }
            return Ok(());
        }
    };
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let child = path_join(&[path, &name]);
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let child_entry = match e.metadata() {
            Ok(md) => entry_from_metadata(&name, is_dir, &md),
            Err(_) => WalkEntry {
                name,
                is_dir,
                ..WalkEntry::default()
            },
        };
        walk_rec(&child, &child_entry, cb)?;
    }
    Ok(())
}


/// `Stats` tallies one Scan run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub keys: i64,
    pub keys_cached: i64,
    pub keys_fetched: i64,
    pub keys_not_found: i64,
    pub keys_failed: i64,
    pub keys_done: i64,
    pub cancelled: bool,
    pub files: i64,
    pub skipped: i64,
    pub unique: i64,
    pub resolved: i64,
    pub cached: i64,
    pub fetched: i64,
    pub not_found: i64,
    pub errors: i64,
    pub denied: i64,
    pub folders_scanned: i64,
    pub folders_skipped: i64,
    pub files_skipped: i64,
    pub unchanged: i64,
    pub parsed: i64,
    pub changed: i64,
    pub pruned: i64,
    pub pending: i64,
    pub needs_repair: i64,
}

/// `Progress` is one live scan update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: i64,
    pub total: i64,
    pub resolved: i64,
    pub cached: i64,
    pub fetched: i64,
    pub keys_total: i64,
    pub keys_done: i64,
    pub keys_cached: i64,
    pub keys_fetched: i64,
    pub keys_not_found: i64,
    pub keys_failed: i64,
    pub last: String,
    pub folders_scanned: i64,
    pub folders_skipped: i64,
}

/// The live progress callback (invoked from worker threads).
pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// `Options` configures `scan_with_options`.
#[derive(Clone, Default)]
pub struct Options {
    pub dirs: Vec<String>,
    pub force: bool,
    pub cache_dir: String,
    pub index_path: String,
    pub use_index: bool,
    pub progress: Option<ProgressFn>,
    pub skip_catch_up: bool,
    pub app_version: String,
}


/// `CollectFiles` walks each root recursively for video files.
pub fn collect_files(roots: &[String], limit: i32) -> (Vec<String>, i64, Option<Error>) {
    let (files, denied, _cancelled, err) = collect_files_generic(None, roots, limit);
    (files, denied, err)
}

/// `CollectFilesWithContext`.
pub fn collect_files_with_context(
    cancel: &Cancel,
    roots: &[String],
    limit: i32,
) -> (Vec<String>, i64, bool, Option<Error>) {
    let (files, denied, cancelled, err) = collect_files_generic(Some(cancel), roots, limit);
    (files, denied, cancelled, err)
}

fn collect_files_generic(
    cancel: Option<&Cancel>,
    roots: &[String],
    limit: i32,
) -> (Vec<String>, i64, bool, Option<Error>) {
    let mut files: Vec<String> = Vec::new();
    let mut denied = 0;
    let mut ok_roots = 0;
    let mut first_err: Option<Error> = None;
    let mut cancelled = false;
    for root in roots {
        if cancel.is_some_and(|c| c.err().is_some()) {
            cancelled = true;
            break;
        }
        let md = match std::fs::metadata(root) {
            Ok(md) => md,
            Err(e) => {
                eprintln!("library: skipping unreadable path {root:?}: {e}");
                denied += 1;
                if first_err.is_none() {
                    first_err = Some(Error::new(format!("unwalkable root {root:?}: {e}")));
                }
                continue;
            }
        };
        if !md.is_dir() {
            eprintln!("library: skipping unreadable path {root:?}: not a directory");
            denied += 1;
            if first_err.is_none() {
                first_err = Some(Error::new(format!(
                    "unwalkable root {root:?}: not a directory"
                )));
            }
            continue;
        }
        let res = walk_dir(root, &mut |path, entry, err| {
            if cancel.is_some_and(|c| c.err().is_some()) {
                return WalkResult::Fail(cancelled_io());
            }
            if let Some(err) = err {
                if path == root {
                    return WalkResult::Fail(std::io::Error::new(err.kind(), err.to_string()));
                }
                eprintln!("library: skipping unreadable path {path:?}: {err}");
                denied += 1;
                if entry.map(|e| e.is_dir).unwrap_or(false) {
                    return WalkResult::SkipDir;
                }
                return WalkResult::Continue;
            }
            let Some(entry) = entry else {
                return WalkResult::Continue;
            };
            if entry.is_dir {
                return WalkResult::Continue;
            }
            if is_video_ext(&path_ext(path)) {
                files.push(path.to_string());
            }
            WalkResult::Continue
        });
        if let Err(e) = res {
            if cancel.is_some_and(|c| c.err().is_some())
                || e.kind() == std::io::ErrorKind::Interrupted
            {
                cancelled = true;
                break;
            }
            eprintln!("library: skipping unreadable path {root:?}: {e}");
            denied += 1;
            if first_err.is_none() {
                first_err = Some(Error::new(format!("unwalkable root {root:?}: {e}")));
            }
            continue;
        }
        ok_roots += 1;
    }
    if cancel.is_some_and(|c| c.err().is_some()) {
        cancelled = true;
    }
    if ok_roots == 0 && first_err.is_some() && !cancelled {
        return (Vec::new(), denied, cancelled, first_err);
    }
    if limit > 0 && files.len() > limit as usize {
        files.truncate(limit as usize);
    }
    (files, denied, cancelled, None)
}

/// `Walk` calls `f` once per video file under `dirs`, in lexical walk order.
pub fn walk(dirs: &[String], mut f: impl FnMut(&str) -> Result<(), Error>) -> Result<(), Error> {
    let (files, _, err) = collect_files(dirs, 0);
    if let Some(err) = err {
        return Err(err);
    }
    for file in &files {
        f(file)?;
    }
    Ok(())
}


/// `DedupeKey` is the resolver's cache identity for one query.
pub fn dedupe_key(q: &Query) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        q.kind.value(),
        q.title.to_lowercase(),
        q.year,
        q.season,
        q.absolute,
        q.episode
    )
}

/// One walked file's parsed query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilePlan {
    pub path: String,
    pub query: Query,
    pub key: String,
    pub skipped: bool,
}

/// One unique query shared by one or more files.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Group {
    pub key: String,
    pub query: Query,
    pub paths: Vec<String>,
}

/// Parses every file and dedupes non-Unknown queries into groups.
pub fn build_plan(files: &[String]) -> (Vec<FilePlan>, Vec<Group>) {
    let mut plans = Vec::with_capacity(files.len());
    let mut groups: Vec<Group> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for path in files {
        let q = metadata_core::context::resolve_query(&path_base(path), path);
        if q.kind == metadata::Kind::UNKNOWN {
            plans.push(FilePlan {
                path: path.clone(),
                query: q,
                skipped: true,
                ..Default::default()
            });
            continue;
        }
        let key = dedupe_key(&q);
        match by_key.get(&key) {
            Some(&i) => groups[i].paths.push(path.clone()),
            None => {
                by_key.insert(key.clone(), groups.len());
                groups.push(Group {
                    key: key.clone(),
                    query: q.clone(),
                    paths: vec![path.clone()],
                });
            }
        }
        plans.push(FilePlan {
            path: path.clone(),
            query: q,
            key,
            skipped: false,
        });
    }
    (plans, groups)
}


/// `LookupFunc` resolves one unique query.
pub type LookupFn<'a> =
    dyn Fn(&Cancel, &Query) -> Result<metadata::Resolution, metadata::Error> + Send + Sync + 'a;

/// The per-key completion callback `FetchAll` invokes from a worker.
pub type OnDone<'a> = dyn Fn(&Group, FetchResult) + Send + Sync + 'a;

/// The per-file listener an indexed walk installs on its enumeration.
pub(crate) type OnFile<'a> = dyn Fn(&str, i64, i64) + 'a;

/// One unique key's verdict.
#[derive(Clone, Debug, Default)]
pub struct FetchResult {
    pub res: metadata::Resolution,
    pub err: Option<metadata::Error>,
    pub from_cache: bool,
}

struct RateGate {
    mu: Mutex<Instant>,
    interval: Duration,
}

impl RateGate {
    fn new(interval: Duration) -> RateGate {
        RateGate {
            mu: Mutex::new(Instant::now()),
            interval,
        }
    }

    fn wait(&self, cancel: &Cancel) {
        let mut last = self.mu.lock().unwrap();
        let target = *last + self.interval;
        if let Some(d) = target.checked_duration_since(Instant::now()) {
            if !d.is_zero() {
                let _ = cancel.wait(d);
            }
        }
        *last = Instant::now();
    }
}

/// `FetchAll` calls `lookup` once per unique group with bounded concurrency.
pub fn fetch_all(
    cancel: &Cancel,
    groups: &[Group],
    lookup: &LookupFn<'_>,
    concurrency: usize,
    gate_interval: Duration,
    on_done: Option<&OnDone<'_>>,
) -> HashMap<String, FetchResult> {
    let out: Mutex<HashMap<String, FetchResult>> = Mutex::new(HashMap::new());
    let gate = if gate_interval > Duration::ZERO {
        Some(RateGate::new(gate_interval))
    } else {
        None
    };
    let link = CancelLink::new(cancel);
    let next = AtomicUsize::new(0);
    let concurrency = concurrency.max(1);
    std::thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                loop {
                    if cancel.err().is_some() {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= groups.len() {
                        break;
                    }
                    if cancel.err().is_some() {
                        break;
                    }
                    let g = &groups[i];
                    if let Some(gate) = &gate {
                        gate.wait(cancel);
                    }
                    if cancel.err().is_some() {
                        break;
                    }
                    let child = Cancel::with_deadline(LOOKUP_TIMEOUT);
                    link.add(child.clone());
                    let result = lookup(&child, &g.query);
                    child.cancel();
                    let fr = match result {
                        Ok(res) => FetchResult {
                            res,
                            err: None,
                            from_cache: false,
                        },
                        Err(e) => FetchResult {
                            res: metadata::Resolution::default(),
                            err: Some(e),
                            from_cache: false,
                        },
                    };
                    out.lock().unwrap().insert(g.key.clone(), fr.clone());
                    if let Some(cb) = on_done {
                        cb(g, fr);
                    }
                }
            });
        }
    });
    link.finish();
    out.into_inner().unwrap()
}

/// `Scan` walks dirs for video files and resolves each unique query.
pub fn scan(
    cancel: &Cancel,
    dirs: &[String],
    force: bool,
    cache_dir: &str,
    progress: Option<ProgressFn>,
) -> Result<Stats, Error> {
    scan_with_options(
        cancel,
        Options {
            dirs: dirs.to_vec(),
            force,
            cache_dir: cache_dir.to_string(),
            progress,
            ..Default::default()
        },
    )
}


/// A fresh tier-1 names hash (one `read_dir`, names sorted).
fn fresh_names_hash(dir: &str, mod_nanos: i64) -> std::io::Result<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(hash_names(mod_nanos, names.len(), &names))
}

/// A fresh tier-2 fingerprint (one `read_dir` plus fresh stats).
fn fresh_fingerprint(dir: &str, mod_nanos: i64) -> std::io::Result<String> {
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    let mut fps = Vec::with_capacity(entries.len());
    for e in &entries {
        let info = e.metadata()?;
        let is_dir = e.file_type()?.is_dir();
        let mut fe = FpEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            size: info.len() as i64,
            mtime_nanos: mtime_nanos(&info),
            is_dir,
            sub: Vec::new(),
        };
        if is_dir {
            let sub_path = std::path::Path::new(dir).join(e.file_name());
            let mut sub: Vec<std::fs::DirEntry> =
                std::fs::read_dir(sub_path)?.collect::<Result<_, _>>()?;
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
    Ok(hash_fingerprint(mod_nanos, &fps))
}

/// One visited directory's walk-time hash identities.
#[derive(Clone, Debug, Default)]
pub(crate) struct WalkDirInfo {
    pub names_hash: String,
    pub fingerprint: String,
}

/// One collect pass's per-directory hash state, keyed by normalized dir.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScanWalk {
    pub dirs: HashMap<String, WalkDirInfo>,
}

/// One collect pass's outcome.
#[derive(Default)]
pub(crate) struct CollectOutcome {
    pub files: Vec<String>,
    pub visited: Vec<String>,
    pub skipped: HashMap<String, scanindex::FolderRecord>,
    pub walk: Option<ScanWalk>,
    pub folders_scanned: i64,
    pub folders_skipped: i64,
    pub files_skipped: i64,
    pub denied: i64,
    pub first_err: Option<Error>,
    pub cancelled: bool,
}

/// `collectIndexed` walks each root for video files, consulting `ix` when
/// enabled.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_indexed(
    cancel: &Cancel,
    roots: &[String],
    ix: Option<&ScanIndex>,
    use_index: bool,
    schema: i32,
    key_present: Option<&dyn Fn(&str) -> bool>,
    force: bool,
    index_path: &str,
    on_file: Option<&OnFile<'_>>,
) -> CollectOutcome {
    let mut skipped: HashMap<String, scanindex::FolderRecord> = HashMap::new();
    let enabled = use_index && !force && ix.is_some();
    let indexed = use_index && !index_path.is_empty() && ix.is_some();
    let mut walk = if indexed {
        Some(ScanWalk::default())
    } else {
        None
    };
    let mut files: Vec<String> = Vec::new();
    let mut visited: Vec<String> = Vec::new();
    let mut folders_scanned = 0;
    let mut folders_skipped = 0;
    let mut files_skipped = 0;
    let mut denied = 0;
    let mut first_err: Option<Error> = None;
    let mut cancelled = false;
    let mut ok_roots = 0;
    let mut last_flush = Instant::now();
    let mut last_flush_count = 0usize;
    for root in roots {
        if cancel.err().is_some() {
            cancelled = true;
            break;
        }
        let md = match std::fs::metadata(root) {
            Ok(md) => md,
            Err(e) => {
                eprintln!("library: skipping unreadable path {root:?}: {e}");
                denied += 1;
                if first_err.is_none() {
                    first_err = Some(Error::new(format!("unwalkable root {root:?}: {e}")));
                }
                continue;
            }
        };
        if !md.is_dir() {
            eprintln!("library: skipping unreadable path {root:?}: not a directory");
            denied += 1;
            if first_err.is_none() {
                first_err = Some(Error::new(format!(
                    "unwalkable root {root:?}: not a directory"
                )));
            }
            continue;
        }
        let res = walk_dir(root, &mut |path, entry, err| {
            if cancel.err().is_some() {
                return WalkResult::Fail(cancelled_io());
            }
            if let Some(err) = err {
                if path == root {
                    return WalkResult::Fail(std::io::Error::new(err.kind(), err.to_string()));
                }
                eprintln!("library: skipping unreadable path {path:?}: {err}");
                denied += 1;
                if entry.map(|e| e.is_dir).unwrap_or(false) {
                    return WalkResult::SkipDir;
                }
                return WalkResult::Continue;
            }
            let Some(entry) = entry else {
                return WalkResult::Continue;
            };
            if entry.is_dir {
                if enabled {
                    let ix = ix.unwrap();
                    let mut nh = String::new();
                    let mut fp = String::new();
                    if let Ok(fi) = std::fs::metadata(path) {
                        if fi.is_dir() {
                            if let Ok(h) = fresh_names_hash(path, mtime_nanos(&fi)) {
                                nh = h;
                                if ix.should_skip_names(path, &nh, schema, key_present) {
                                    if let Some(rec) = ix.lookup(path) {
                                        files_skipped += rec.files;
                                        skipped.insert(normalize_scan_dir(path), rec);
                                    }
                                    folders_skipped += 1;
                                    return WalkResult::SkipDir;
                                }
                            }
                            if let Ok(h) = fresh_fingerprint(path, mtime_nanos(&fi)) {
                                fp = h;
                                if ix.should_skip(path, &fp, schema, key_present) {
                                    if let Some(rec) = ix.lookup(path) {
                                        files_skipped += rec.files;
                                        skipped.insert(normalize_scan_dir(path), rec.clone());
                                        ix.record_full(
                                            path, &nh, &fp, schema, &rec.keys, rec.files,
                                        );
                                    }
                                    folders_skipped += 1;
                                    return WalkResult::SkipDir;
                                }
                            }
                        }
                    }
                    if let Some(walk) = walk.as_mut() {
                        walk.dirs.insert(
                            normalize_scan_dir(path),
                            WalkDirInfo {
                                names_hash: nh,
                                fingerprint: fp,
                            },
                        );
                    }
                }
                folders_scanned += 1;
                visited.push(path.to_string());
                if indexed
                    && (visited.len() - last_flush_count >= CHECKPOINT_EVERY_FOLDERS
                        || last_flush.elapsed() >= CHECKPOINT_EVERY_INTERVAL)
                {
                    last_flush = Instant::now();
                    last_flush_count = visited.len();
                    if let Some(ix) = ix {
                        if let Err(e) = ix.save_if_dirty(index_path) {
                            eprintln!("library: checkpoint scan index {index_path:?}: {e}");
                        }
                    }
                }
                return WalkResult::Continue;
            }
            if is_video_ext(&path_ext(path)) {
                files.push(path.to_string());
                if let Some(on_file) = on_file {
                    if entry.info_ok {
                        on_file(path, entry.size, entry.mtime_nanos);
                    }
                }
            }
            WalkResult::Continue
        });
        if let Err(e) = res {
            if cancel.err().is_some() || e.kind() == std::io::ErrorKind::Interrupted {
                cancelled = true;
                break;
            }
            eprintln!("library: skipping unreadable path {root:?}: {e}");
            denied += 1;
            if first_err.is_none() {
                first_err = Some(Error::new(format!("unwalkable root {root:?}: {e}")));
            }
            continue;
        }
        ok_roots += 1;
    }
    if cancel.err().is_some() {
        cancelled = true;
    }
    if ok_roots == 0 && first_err.is_some() && !cancelled {
        return CollectOutcome {
            files: Vec::new(),
            visited: Vec::new(),
            skipped,
            walk,
            folders_scanned,
            folders_skipped,
            files_skipped,
            denied,
            first_err,
            cancelled,
        };
    }
    CollectOutcome {
        files,
        visited,
        skipped,
        walk,
        folders_scanned,
        folders_skipped,
        files_skipped,
        denied,
        first_err: None,
        cancelled,
    }
}

/// The outcome of an index-aware walk.
#[derive(Default)]
pub struct CollectResult {
    pub files: Vec<String>,
    pub visited: Vec<String>,
    pub skipped: HashMap<String, scanindex::FolderRecord>,
    pub folders_scanned: i64,
    pub folders_skipped: i64,
    pub files_skipped: i64,
    pub denied: i64,
}

/// `CollectIndexedWithContext` is `CollectIndexed` with a cancellable context.
pub fn collect_indexed_with_context(
    cancel: &Cancel,
    roots: &[String],
    cache_dir: &str,
    index_path: &str,
    use_index: bool,
    force: bool,
) -> (CollectResult, bool, Option<Error>) {
    let mut out = CollectResult::default();
    let indexed = use_index && !index_path.is_empty();
    let ix = if indexed {
        Some(load_scan_index(index_path))
    } else {
        None
    };
    let schema = metadata::cache::cache_schema();
    let memo: Mutex<HashMap<String, bool>> = Mutex::new(HashMap::new());
    let key_present = |key: &str| -> bool {
        let mut memo = memo.lock().unwrap();
        if let Some(v) = memo.get(key) {
            return *v;
        }
        let v = metadata::cache::cache_live(cache_dir, key);
        memo.insert(key.to_string(), v);
        v
    };
    let key_present_ref: Option<&dyn Fn(&str) -> bool> =
        if indexed { Some(&key_present) } else { None };
    let outcome = collect_indexed(
        cancel,
        roots,
        ix.as_ref(),
        indexed,
        schema,
        key_present_ref,
        force,
        index_path,
        None,
    );
    if let Some(err) = outcome.first_err {
        return (CollectResult::default(), outcome.cancelled, Some(err));
    }
    out.files = outcome.files;
    out.visited = outcome.visited;
    out.skipped = outcome.skipped;
    out.folders_scanned = outcome.folders_scanned;
    out.folders_skipped = outcome.folders_skipped;
    out.files_skipped = outcome.files_skipped;
    out.denied = outcome.denied;
    (out, outcome.cancelled, None)
}

/// `CollectIndexed` walks roots with the two-tier index skip when `use_index`
/// is set.
pub fn collect_indexed_public(
    roots: &[String],
    cache_dir: &str,
    index_path: &str,
    use_index: bool,
    force: bool,
) -> Result<CollectResult, Error> {
    let (cr, _, err) = collect_indexed_with_context(
        &Cancel::background(),
        roots,
        cache_dir,
        index_path,
        use_index,
        force,
    );
    match err {
        Some(e) => Err(e),
        None => Ok(cr),
    }
}


/// `RecordScan` re-records every visited dir from an indexed walk.
pub fn record_scan(
    index_path: &str,
    visited: &[String],
    skipped: &HashMap<String, scanindex::FolderRecord>,
    plans: &[FilePlan],
) {
    if index_path.is_empty() {
        return;
    }
    let ix = load_scan_index(index_path);
    record_index(
        &ix,
        visited,
        skipped,
        plans,
        index_path,
        metadata::cache::cache_schema(),
        None,
    );
}

pub(crate) fn record_index(
    ix: &ScanIndex,
    visited: &[String],
    skipped: &HashMap<String, scanindex::FolderRecord>,
    plans: &[FilePlan],
    index_path: &str,
    schema: i32,
    walk: Option<&ScanWalk>,
) {
    let mut file_cache_key: HashMap<String, String> = HashMap::new();
    for p in plans {
        if p.skipped {
            continue;
        }
        file_cache_key.insert(p.path.clone(), metadata::cache::cache_key_for(&p.query));
    }
    record_index_keys(
        ix,
        visited,
        skipped,
        &file_cache_key,
        index_path,
        schema,
        walk,
    );
}

/// `recordIndexKeys` re-records every visited dir, verifying fail-closed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_index_keys(
    ix: &ScanIndex,
    visited: &[String],
    skipped: &HashMap<String, scanindex::FolderRecord>,
    file_cache_key: &HashMap<String, String>,
    index_path: &str,
    schema: i32,
    walk: Option<&ScanWalk>,
) {
    if !index_path.is_empty() {
        ix.prune_existing();
    }
    if index_path.is_empty() || visited.is_empty() {
        if !index_path.is_empty() {
            if let Err(e) = ix.save_if_dirty(index_path) {
                eprintln!("library: save scan index {index_path:?}: {e}");
            }
        }
        return;
    }
    let mut norm_of: HashMap<String, String> = HashMap::new();
    let mut norms: Vec<String> = Vec::new();
    for v in visited {
        let nv = normalize_scan_dir(v);
        if !norm_of.contains_key(&nv) {
            norm_of.insert(nv.clone(), v.clone());
            norms.push(nv);
        }
    }
    struct DirAgg {
        files: i64,
        keys: HashMap<String, bool>,
        paths: Vec<String>,
    }
    let mut aggs: HashMap<String, DirAgg> = norms
        .iter()
        .map(|nv| {
            (
                nv.clone(),
                DirAgg {
                    files: 0,
                    keys: HashMap::new(),
                    paths: Vec::new(),
                },
            )
        })
        .collect();
    for (path, k) in file_cache_key {
        let d = normalize_scan_dir(&path_dir(path));
        let mut a = d;
        while !a.is_empty() {
            if let Some(agg) = aggs.get_mut(&a) {
                agg.files += 1;
                if !k.is_empty() {
                    agg.keys.insert(k.clone(), true);
                }
                agg.paths.push(path.clone());
            }
            a = parent_norm(&a);
        }
    }
    let mut depend: HashMap<String, Vec<String>> = HashMap::new();
    for (snorm, rec) in skipped {
        let mut a = snorm.clone();
        while !a.is_empty() {
            if let Some(agg) = aggs.get_mut(&a) {
                agg.files += rec.files;
                for k in &rec.keys {
                    if !k.is_empty() {
                        agg.keys.insert(k.clone(), true);
                    }
                }
            }
            a = parent_norm(&a);
        }
        let mut a = snorm.clone();
        while !a.is_empty() {
            if aggs.contains_key(&a) {
                depend.entry(a.clone()).or_default().push(snorm.clone());
            }
            a = parent_norm(&a);
        }
    }
    let mut dc = CachedDir::default();
    let mut fresh_names: HashMap<String, HashMap<String, bool>> = HashMap::new();
    let mut bad_list: HashMap<String, bool> = HashMap::new();
    for nv in &norms {
        match dc.list(&norm_of[nv]) {
            Ok(es) => {
                let set: HashMap<String, bool> =
                    es.into_iter().map(|(name, _)| (name, true)).collect();
                fresh_names.insert(nv.clone(), set);
            }
            Err(_) => {
                bad_list.insert(nv.clone(), true);
            }
        }
    }
    let mut fresh_rec: HashMap<String, i64> = HashMap::new();
    let mut poisoned: HashMap<String, bool> = HashMap::new();
    let mut memo_rec: HashMap<String, i64> = HashMap::new();
    for nv in &norms {
        if bad_list.contains_key(nv) {
            continue;
        }
        match rec_count(&mut dc, skipped, &mut memo_rec, nv, &norm_of[nv]) {
            Some(t) => {
                fresh_rec.insert(nv.clone(), t);
            }
            None => {
                poisoned.insert(nv.clone(), true);
            }
        }
    }
    let mut skip_fp: HashMap<String, String> = HashMap::new();
    for snorm in skipped.keys() {
        if let Ok(fp) = scan_folder_fingerprint(snorm) {
            skip_fp.insert(snorm.clone(), fp);
        }
    }
    for nv in &norms {
        let v = &norm_of[nv];
        let a = &aggs[nv];
        if bad_list.contains_key(nv) || poisoned.contains_key(nv) {
            continue;
        }
        if fresh_rec.get(nv).copied().unwrap_or(-1) != a.files {
            continue;
        }
        let mut verified = true;
        for p in &a.paths {
            let set = fresh_names.get(&normalize_scan_dir(&path_dir(p)));
            match set {
                Some(s) if s.contains_key(&path_base(p)) => {}
                _ => {
                    verified = false;
                    break;
                }
            }
        }
        if !verified {
            continue;
        }
        if let Some(deps) = depend.get(nv) {
            for sn in deps {
                let rec = &skipped[sn];
                match skip_fp.get(sn) {
                    Some(f) if f == &rec.fingerprint => {}
                    _ => {
                        verified = false;
                        break;
                    }
                }
            }
        }
        if !verified {
            continue;
        }
        let mut keys: Vec<String> = a.keys.keys().cloned().collect();
        keys.sort();
        let mut nh = String::new();
        let mut fp = String::new();
        if let Some(walk) = walk {
            if let Some(info) = walk.dirs.get(nv) {
                if !info.names_hash.is_empty() && !info.fingerprint.is_empty() {
                    nh = info.names_hash.clone();
                    fp = info.fingerprint.clone();
                }
            }
        }
        if nh.is_empty() || fp.is_empty() {
            let Ok(fi) = std::fs::metadata(v) else {
                continue;
            };
            if !fi.is_dir() {
                continue;
            }
            nh = match dc.names_hash(v, mtime_nanos(&fi)) {
                Ok(h) => h,
                Err(_) => continue,
            };
            fp = match dc.fingerprint(v, mtime_nanos(&fi)) {
                Ok(h) => h,
                Err(_) => continue,
            };
        }
        ix.record_full(v, &nh, &fp, schema, &keys, a.files);
    }
    if let Err(e) = ix.save_if_dirty(index_path) {
        eprintln!("library: save scan index {index_path:?}: {e}");
    }
}

fn parent_norm(nv: &str) -> String {
    let par = normalize_scan_dir(&path_dir(nv));
    if par == nv { String::new() } else { par }
}

fn rec_count(
    dc: &mut CachedDir,
    skipped: &HashMap<String, scanindex::FolderRecord>,
    memo: &mut HashMap<String, i64>,
    nv: &str,
    path: &str,
) -> Option<i64> {
    if let Some(v) = memo.get(nv) {
        return Some(*v);
    }
    let es = dc.list(path).ok()?;
    let mut total = 0;
    for (name, is_dir) in es {
        if is_dir {
            let child = path_join(&[path, &name]);
            let cn = normalize_scan_dir(&child);
            if let Some(rec) = skipped.get(&cn) {
                total += rec.files;
                continue;
            }
            total += rec_count(dc, skipped, memo, &cn, &child)?;
        } else if is_video_ext(&path_ext(&name)) {
            total += 1;
        }
    }
    memo.insert(nv.to_string(), total);
    Some(total)
}

/// `cachedDir` memoizes one `read_dir` per directory for one recordIndex pass.
#[derive(Default)]
struct CachedDir {
    lists: HashMap<String, Vec<(String, bool)>>,
    infos: HashMap<String, (i64, i64)>,
}

impl CachedDir {
    fn list(&mut self, path: &str) -> std::io::Result<Vec<(String, bool)>> {
        if let Some(es) = self.lists.get(path) {
            return Ok(es.clone());
        }
        let mut out = Vec::new();
        for e in std::fs::read_dir(path)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push((name, is_dir));
        }
        self.lists.insert(path.to_string(), out.clone());
        Ok(out)
    }

    fn info(&mut self, path: &str) -> std::io::Result<(i64, i64)> {
        if let Some(v) = self.infos.get(path) {
            return Ok(*v);
        }
        let md = std::fs::metadata(path)?;
        let v = (md.len() as i64, mtime_nanos(&md));
        self.infos.insert(path.to_string(), v);
        Ok(v)
    }

    fn names_hash(&mut self, path: &str, mod_nanos: i64) -> std::io::Result<String> {
        let mut names: Vec<String> = self.list(path)?.into_iter().map(|(name, _)| name).collect();
        names.sort();
        Ok(hash_names(mod_nanos, names.len(), &names))
    }

    fn fingerprint(&mut self, path: &str, mod_nanos: i64) -> std::io::Result<String> {
        let mut sorted = self.list(path)?;
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut fps = Vec::with_capacity(sorted.len());
        for (name, is_dir) in &sorted {
            let ep = path_join(&[path, name]);
            let info = self.info(&ep)?;
            let mut fe = FpEntry {
                name: name.clone(),
                size: info.0,
                mtime_nanos: info.1,
                is_dir: *is_dir,
                sub: Vec::new(),
            };
            if *is_dir {
                let mut sub = self.list(&ep)?;
                sub.sort_by(|a, b| a.0.cmp(&b.0));
                for (sn, _) in sub {
                    let sp = path_join(&[&ep, &sn]);
                    let sinfo = self.info(&sp)?;
                    fe.sub.push(FpSub {
                        name: sn,
                        size: sinfo.0,
                    });
                }
            }
            fps.push(fe);
        }
        Ok(hash_fingerprint(mod_nanos, &fps))
    }
}


struct Agg {
    done: i64,
    resolved: i64,
    cached: i64,
    fetched: i64,
    keys_cached: i64,
    keys_fetched: i64,
    keys_not_found: i64,
    keys_failed: i64,
    cached_hit: HashMap<String, bool>,
}

fn sub(a: i32, b: i32) -> i32 {
    a - b
}

/// `ScanWithOptions` is `Scan` with an opt-in folder index.
pub fn scan_with_options(cancel: &Cancel, opts: Options) -> Result<Stats, Error> {
    if opts.use_index && !opts.index_path.is_empty() {
        return scan::scan_indexed(cancel, &opts);
    }
    let indexed = opts.use_index && !opts.index_path.is_empty();
    let schema = metadata::cache::cache_schema();
    let ix = if indexed {
        Some(load_scan_index(&opts.index_path))
    } else {
        None
    };
    let live_memo: Mutex<HashMap<String, bool>> = Mutex::new(HashMap::new());
    let scan_resolver: Option<metadata::Resolver> = if indexed {
        Some(new_resolver(&opts.cache_dir))
    } else {
        None
    };
    let key_present = |key: &str| -> bool {
        if let Some(r) = &scan_resolver {
            let mut memo = live_memo.lock().unwrap();
            if let Some(v) = memo.get(key) {
                return *v;
            }
            let v = r.probe_cache(key);
            memo.insert(key.to_string(), v);
            v
        } else {
            metadata::cache::cache_live(&opts.cache_dir, key)
        }
    };
    let key_present_ref: Option<&dyn Fn(&str) -> bool> =
        if indexed { Some(&key_present) } else { None };
    let outcome = collect_indexed(
        cancel,
        &opts.dirs,
        ix.as_ref(),
        indexed,
        schema,
        key_present_ref,
        opts.force,
        &opts.index_path,
        None,
    );
    if let Some(err) = outcome.first_err {
        return Err(err);
    }
    let (plans, groups) = build_plan(&outcome.files);
    if outcome.cancelled || cancel.err().is_some() {
        return Ok(Stats {
            files: plans.len() as i64,
            unique: groups.len() as i64,
            denied: outcome.denied,
            folders_scanned: outcome.folders_scanned,
            folders_skipped: outcome.folders_skipped,
            files_skipped: outcome.files_skipped,
            keys: groups.len() as i64,
            cancelled: true,
            ..Stats::default()
        });
    }
    if let (Some(ix), true) = (&ix, indexed) {
        if cancel.err().is_none() {
            record_index(
                ix,
                &outcome.visited,
                &outcome.skipped,
                &plans,
                &opts.index_path,
                schema,
                outcome.walk.as_ref(),
            );
        }
    }
    let mut st = Stats {
        files: plans.len() as i64,
        unique: groups.len() as i64,
        denied: outcome.denied,
        folders_scanned: outcome.folders_scanned,
        folders_skipped: outcome.folders_skipped,
        files_skipped: outcome.files_skipped,
        keys: groups.len() as i64,
        ..Stats::default()
    };
    let keys_total = groups.len() as i64;
    let agg = Arc::new(Mutex::new(Agg {
        done: 0,
        resolved: 0,
        cached: 0,
        fetched: 0,
        keys_cached: 0,
        keys_fetched: 0,
        keys_not_found: 0,
        keys_failed: 0,
        cached_hit: HashMap::new(),
    }));
    if !opts.force {
        for g in &groups {
            let ck = metadata::cache::cache_key_for(&g.query);
            let live = if let Some(r) = &scan_resolver {
                let mut memo = live_memo.lock().unwrap();
                match memo.get(&ck) {
                    Some(v) => *v,
                    None => {
                        let v = r.probe_cache(&ck);
                        memo.insert(ck.clone(), v);
                        v
                    }
                }
            } else {
                metadata::cache::cache_live(&opts.cache_dir, &ck)
            };
            if live {
                let mut a = agg.lock().unwrap();
                a.keys_cached += 1;
                a.cached_hit.insert(g.key.clone(), true);
            }
        }
    }
    let keys_cached = agg.lock().unwrap().keys_cached;
    st.keys_cached = keys_cached;
    st.keys_done = keys_cached;
    if groups.is_empty() {
        if let Some(progress) = &opts.progress {
            progress(Progress {
                folders_scanned: outcome.folders_scanned,
                folders_skipped: outcome.folders_skipped,
                keys_total,
                keys_done: keys_cached,
                keys_cached,
                ..Progress::default()
            });
        }
    }
    let mut status_by_key: HashMap<String, String> = HashMap::new();
    let mut cached_by_key: HashMap<String, bool> = HashMap::new();
    if !groups.is_empty() {
        let r = match &scan_resolver {
            Some(r) => r.clone(),
            None => new_resolver(&opts.cache_dir),
        };
        let src_by_key: Mutex<HashMap<String, bool>> = Mutex::new(HashMap::new());
        let lookup = |lctx: &Cancel, q: &Query| -> Result<metadata::Resolution, metadata::Error> {
            let (res, from_cache, err) = match r.lookup_now_force_src(lctx, q, opts.force) {
                Ok((res, from_cache)) => (res, from_cache, None),
                Err(e) => (metadata::Resolution::default(), false, Some(e)),
            };
            src_by_key.lock().unwrap().insert(dedupe_key(q), from_cache);
            match err {
                Some(e) => Err(e),
                None => Ok(res),
            }
        };
        let lookup_ref: &LookupFn<'_> = &lookup;
        let total = plans.len() as i64;
        let agg2 = Arc::clone(&agg);
        let src_ref = &src_by_key;
        let progress = opts.progress.clone();
        let last_ckpt = Mutex::new(Instant::now());
        let ix_ck = ix.clone();
        let visited = outcome.visited.clone();
        let skipped_ck = outcome.skipped.clone();
        let plans_ck = plans.clone();
        let walk_ck = outcome.walk.clone();
        let index_path = opts.index_path.clone();
        let on_done = |g: &Group, fr: FetchResult| {
            let from_cache = src_ref
                .lock()
                .unwrap()
                .get(&g.key)
                .copied()
                .unwrap_or(false);
            let n = g.paths.len() as i64;
            let (add, add_cached, add_fetched) = if fr.err.is_none() && fr.res.found {
                if from_cache { (n, n, 0) } else { (n, 0, n) }
            } else {
                (0, 0, 0)
            };
            let mut a = agg2.lock().unwrap();
            a.done += n;
            a.resolved += add;
            a.cached += add_cached;
            a.fetched += add_fetched;
            if !a.cached_hit.get(&g.key).copied().unwrap_or(false) {
                match &fr.err {
                    Some(e) => {
                        if cancel.err().is_none() {
                            if e.is_lookup_unavailable() {
                                a.keys_failed += 1;
                            } else {
                                a.keys_not_found += 1;
                            }
                        }
                    }
                    None => {
                        if fr.res.found {
                            if !from_cache {
                                a.keys_fetched += 1;
                            } else {
                                a.keys_cached += 1;
                                a.cached_hit.insert(g.key.clone(), true);
                            }
                        } else {
                            a.keys_not_found += 1;
                        }
                    }
                }
            }
            let keys_done = a.keys_cached + a.keys_fetched + a.keys_not_found + a.keys_failed;
            let p = Progress {
                done: a.done,
                total,
                resolved: a.resolved,
                cached: a.cached,
                fetched: a.fetched,
                last: g.paths.last().cloned().unwrap_or_default(),
                folders_scanned: outcome.folders_scanned,
                folders_skipped: outcome.folders_skipped,
                keys_total,
                keys_done,
                keys_cached: a.keys_cached,
                keys_fetched: a.keys_fetched,
                keys_not_found: a.keys_not_found,
                keys_failed: a.keys_failed,
            };
            drop(a);
            // Mid-scan checkpoint (indexed only, context live).
            if indexed && cancel.err().is_none() {
                let mut last = last_ckpt.lock().unwrap();
                if last.elapsed() >= CHECKPOINT_EVERY_INTERVAL {
                    *last = Instant::now();
                    drop(last);
                    if let Some(ix) = &ix_ck {
                        record_index(
                            ix,
                            &visited,
                            &skipped_ck,
                            &plans_ck,
                            &index_path,
                            schema,
                            walk_ck.as_ref(),
                        );
                    }
                }
            }
            if let Some(progress) = &progress {
                progress(p);
            }
        };
        let on_done_ref: &(dyn Fn(&Group, FetchResult) + Send + Sync) = &on_done;
        if let Some(progress) = &opts.progress {
            let a = agg.lock().unwrap();
            progress(Progress {
                done: a.done,
                total,
                folders_scanned: outcome.folders_scanned,
                folders_skipped: outcome.folders_skipped,
                keys_total,
                keys_done: a.keys_cached,
                keys_cached: a.keys_cached,
                keys_fetched: a.keys_fetched,
                keys_not_found: a.keys_not_found,
                keys_failed: a.keys_failed,
                ..Progress::default()
            });
        }
        let mut results: HashMap<String, FetchResult> = HashMap::new();
        let mut miss_groups: Vec<Group> = Vec::new();
        for g in &groups {
            let pre = agg
                .lock()
                .unwrap()
                .cached_hit
                .get(&g.key)
                .copied()
                .unwrap_or(false);
            if !pre {
                miss_groups.push(g.clone());
                continue;
            }
            if cancel.err().is_some() {
                break;
            }
            if let Some(res) = r.resolve_cached(&g.query) {
                if res.found {
                    src_by_key.lock().unwrap().insert(g.key.clone(), true);
                    let fr = FetchResult {
                        res,
                        err: None,
                        from_cache: true,
                    };
                    results.insert(g.key.clone(), fr.clone());
                    on_done(g, fr);
                    continue;
                }
            }
            miss_groups.push(g.clone());
        }
        for (k, fr) in fetch_all(
            cancel,
            &miss_groups,
            lookup_ref,
            DEFAULT_CONCURRENCY,
            Duration::ZERO,
            Some(on_done_ref),
        ) {
            results.insert(k, fr);
        }
        let src = src_by_key.lock().unwrap();
        for (k, fr) in results.iter_mut() {
            fr.from_cache = src.get(k).copied().unwrap_or(false);
        }
        drop(src);
        for g in &groups {
            match results.get(&g.key) {
                None => {
                    status_by_key.insert(g.key.clone(), "error".into());
                }
                Some(fr) => {
                    if let Some(_e) = &fr.err {
                        if cancel.err().is_some() {
                            status_by_key.insert(g.key.clone(), "cancelled".into());
                        } else {
                            status_by_key.insert(g.key.clone(), "error".into());
                        }
                    } else if fr.res.found {
                        status_by_key.insert(g.key.clone(), "resolved".into());
                        cached_by_key.insert(g.key.clone(), fr.from_cache);
                    } else {
                        status_by_key.insert(g.key.clone(), "not-found".into());
                    }
                }
            }
        }
    }
    for p in &plans {
        let status = if p.skipped {
            "skipped"
        } else {
            status_by_key.get(&p.key).map(|s| s.as_str()).unwrap_or("")
        };
        match status {
            "skipped" => st.skipped += 1,
            "resolved" => {
                st.resolved += 1;
                if cached_by_key.get(&p.key).copied().unwrap_or(false) {
                    st.cached += 1;
                } else {
                    st.fetched += 1;
                }
            }
            "not-found" => st.not_found += 1,
            "error" => st.errors += 1,
            _ => {}
        }
    }
    let a = agg.lock().unwrap();
    st.keys_cached = a.keys_cached;
    st.keys_fetched = a.keys_fetched;
    st.keys_not_found = a.keys_not_found;
    st.keys_failed = a.keys_failed;
    st.keys_done = a.keys_cached + a.keys_fetched + a.keys_not_found + a.keys_failed;
    drop(a);
    if cancel.err().is_some() {
        st.cancelled = true;
    }
    if let (Some(ix), true) = (&ix, indexed) {
        if cancel.err().is_none() {
            record_index(
                ix,
                &outcome.visited,
                &outcome.skipped,
                &plans,
                &opts.index_path,
                schema,
                outcome.walk.as_ref(),
            );
        }
    }
    let _ = sub;
    Ok(st)
}
