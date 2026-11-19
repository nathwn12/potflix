//! `library::scan` -- mirrors `go\internal\library\scan.go`.
//!
//! The store-backed indexed scan: one walk, a per-file diff, and no network at
//! all. An indexed `Scan` performs ZERO fetches; it walks, diffs, prunes, stamps
//! rollups and hands the pending keys to the catch-up.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use crate::catchup::{start_catch_up, CatchUpConfig};
use crate::index::{
    shared_index, FileRecord, FILE_KIND_MOVIE, FILE_KIND_SERIES, FILE_KIND_UNKNOWN, STATE_PENDING,
    STATE_RESOLVED,
};
use crate::pathutil::{base as path_base, ext as path_ext, normalize_scan_dir};
use crate::scanindex::ScanIndex;
use crate::{collect_indexed, current_parse, record_index_keys, Error, Options, Progress, Stats};

/// One walked file's filesystem identity for the diff.
struct FileStamp {
    path: String,
    size: i64,
    mtime: i64,
}

/// The indexed scan: enumerate + diff, then hand the pending keys to the
/// catch-up worker.
pub(crate) fn scan_indexed(cancel: &crate::Cancel, opts: &Options) -> Result<Stats, Error> {
    let schema = metadata::cache::cache_schema();
    let idx = match shared_index(&opts.index_path) {
        Ok(ix) => ix,
        Err(err) => {
            eprintln!(
                "library: index {:?}: {err} (continuing without a persistent index)",
                opts.index_path
            );
            return Err(err);
        }
    };
    if let Some(err) = idx.open_err.lock().unwrap().clone() {
        eprintln!(
            "library: index {:?}: {err} (continuing without a persistent index)",
            opts.index_path
        );
    }
    let si = ScanIndex {
        ix: Arc::clone(&idx),
    };
    let key_present = |_: &str| true;

    let enumerated = RefCell::new(HashMap::<String, FileStamp>::new());
    let last = RefCell::new(String::new());
    let on_file = |path: &str, size: i64, mtime_nanos: i64| {
        enumerated.borrow_mut().insert(
            normalize_scan_dir(path),
            FileStamp {
                path: path.to_string(),
                size,
                mtime: mtime_nanos,
            },
        );
        *last.borrow_mut() = path.to_string();
    };
    let outcome = collect_indexed(
        cancel,
        &opts.dirs,
        Some(&si),
        true,
        schema,
        Some(&key_present),
        opts.force,
        &opts.index_path,
        Some(&on_file),
    );
    if let Some(err) = outcome.first_err {
        return Err(err);
    }
    if outcome.cancelled {
        let st = Stats {
            files: enumerated.borrow().len() as i64,
            denied: outcome.denied,
            cancelled: true,
            folders_scanned: outcome.folders_scanned,
            folders_skipped: outcome.folders_skipped,
            files_skipped: outcome.files_skipped,
            ..Stats::default()
        };
        return Ok(st);
    }

    let parse = current_parse();
    let mut keys: HashMap<String, String> = HashMap::new();
    let mut unchanged = 0;
    let mut parsed = 0;
    let mut changed = 0;
    for st in enumerated.borrow().values() {
        let rec = idx.file(&st.path);
        if let Some(rec) = &rec {
            if !opts.force && rec.size == st.size && rec.mtime == st.mtime {
                unchanged += 1;
                keys.insert(st.path.clone(), rec.meta_key.clone());
                continue;
            }
        }
        let q = parse(&path_base(&st.path), &st.path);
        let mut next = FileRecord {
            path: st.path.clone(),
            name: path_base(&st.path),
            ext: path_ext(&st.path).to_lowercase(),
            size: st.size,
            mtime: st.mtime,
            kind: record_kind(q.kind).to_string(),
            title: q.title.clone(),
            season: q.season,
            episode: q.episode as i64,
            year: q.year as i64,
            absolute: q.absolute,
            state: STATE_RESOLVED.to_string(),
            ..FileRecord::default()
        };
        if q.kind != metadata::Kind::UNKNOWN {
            next.meta_key = metadata::cache::cache_key_for(&q);
            next.state = STATE_PENDING.to_string();
        }
        if let Some(rec) = &rec {
            if !rec.meta_key.is_empty() && rec.meta_key == next.meta_key {
                next.state = rec.state.clone();
                next.attempts = rec.attempts;
                next.last_error = rec.last_error.clone();
                next.last_try = rec.last_try;
                next.next_try = rec.next_try;
            }
        } else {
            changed += 1;
        }
        parsed += 1;
        idx.put_file(next.clone());
        keys.insert(st.path.clone(), next.meta_key.clone());
        if let Some(progress) = &opts.progress {
            if parsed % 500 == 0 {
                progress(Progress {
                    done: keys.len() as i64,
                    folders_scanned: outcome.folders_scanned,
                    folders_skipped: outcome.folders_skipped,
                    last: st.path.clone(),
                    ..Progress::default()
                });
            }
        }
    }

    let mut enumerated_paths: HashMap<String, bool> = HashMap::new();
    for p in &outcome.files {
        enumerated_paths.insert(normalize_scan_dir(p), true);
    }
    let pruned = idx.prune_files(&visited_set(&outcome.visited), &enumerated_paths);

    record_index_keys(
        &si,
        &outcome.visited,
        &outcome.skipped,
        &keys,
        &opts.index_path,
        schema,
        outcome.walk.as_ref(),
    );

    let state = idx.state();
    let st = Stats {
        files: enumerated.borrow().len() as i64,
        unique: distinct_keys(&keys),
        keys: state.keys,
        keys_done: state.keys_resolved,
        keys_cached: state.keys_resolved,
        keys_not_found: state.keys - state.keys_resolved,
        resolved: state.files_resolved,
        cached: state.files_resolved,
        not_found: state.files_pending,
        skipped: state.files_no_key,
        denied: outcome.denied,
        folders_scanned: outcome.folders_scanned,
        folders_skipped: outcome.folders_skipped,
        files_skipped: outcome.files_skipped,
        unchanged,
        parsed,
        changed,
        pruned,
        pending: state.files_pending,
        needs_repair: state.files_needs_repair,
        ..Stats::default()
    };
    if let Some(progress) = &opts.progress {
        progress(Progress {
            done: state.files,
            total: state.files,
            resolved: state.keys_resolved,
            keys_total: state.keys,
            keys_done: state.keys_resolved,
            keys_cached: state.keys_resolved,
            keys_not_found: state.keys - state.keys_resolved,
            last: last.borrow().clone(),
            folders_scanned: outcome.folders_scanned,
            folders_skipped: outcome.folders_skipped,
            ..Progress::default()
        });
    }

    // The pass owns its own lifetime, deliberately (see catchup.rs).
    if !opts.skip_catch_up && cancel.err().is_none() {
        start_catch_up(
            CatchUpConfig {
                dirs: opts.dirs.clone(),
                cache_dir: opts.cache_dir.clone(),
                index_path: opts.index_path.clone(),
                force: opts.force,
                app_version: opts.app_version.clone(),
                ctx: None,
            },
            opts.progress.clone(),
        );
    }
    Ok(st)
}

/// Normalizes a visited-directory list into a set.
fn visited_set(visited: &[String]) -> HashMap<String, bool> {
    let mut set = HashMap::new();
    for v in visited {
        set.insert(normalize_scan_dir(v), true);
    }
    set
}

/// Counts the distinct non-empty metadata keys of one walk's file-key map.
fn distinct_keys(keys: &HashMap<String, String>) -> i64 {
    let mut seen: HashMap<&str, bool> = HashMap::new();
    for k in keys.values() {
        if !k.is_empty() {
            seen.insert(k.as_str(), true);
        }
    }
    seen.len() as i64
}

/// Maps a parsed query kind onto the stored kind string.
pub(crate) fn record_kind(kind: metadata::Kind) -> &'static str {
    if kind == metadata::Kind::MOVIE {
        FILE_KIND_MOVIE
    } else if kind == metadata::Kind::SERIES {
        FILE_KIND_SERIES
    } else {
        FILE_KIND_UNKNOWN
    }
}
