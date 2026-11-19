
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::cancel_link::CancelLink;
use crate::index::{
    FileRecord, Index, MAX_REPAIR_ATTEMPTS, STATE_NEEDS_REPAIR, STATE_PENDING, STATE_RESOLVED,
    index_store_path, now_unix_nanos, shared_index,
};
use crate::pathutil::{dir as path_dir, join as path_join, normalize_scan_dir};
use crate::{
    Cancel, Error, Progress, ProgressFn, dedupe_key, fetch_all, file_exists, new_resolver,
};

/// `maxRepairAttempts` is how many failed lookups an entry gets before it is
/// parked as needs_repair.
pub(crate) const REPAIR_COOLDOWN: Duration = Duration::from_secs(6 * 60 * 60);
/// `catchUpConcurrency` bounds concurrent lookups in the background worker.
pub(crate) const CATCH_UP_CONCURRENCY: usize = 3;
/// `catchUpCommitInterval` bounds how long an applied verdict sits uncommitted.
pub(crate) const CATCH_UP_COMMIT_INTERVAL: Duration = Duration::from_secs(15);

/// `CatchUpConfig` is one catch-up request.
#[derive(Clone, Default)]
pub struct CatchUpConfig {
    pub dirs: Vec<String>,
    pub cache_dir: String,
    pub index_path: String,
    pub force: bool,
    pub app_version: String,
    pub ctx: Option<Cancel>,
}

/// `CatchUpStats` reports the worker's cumulative work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CatchUpStats {
    pub passes: i64,
    pub lookups: i64,
    pub resolved: i64,
    pub missed: i64,
    pub failed: i64,
    pub needs_repair: i64,
    pub files_applied: i64,
}

#[derive(Default)]
struct CatchUpInner {
    cfg: CatchUpConfig,
    running: bool,
    cancel: Option<Cancel>,
    done: Option<JoinHandle<()>>,
    cb: Option<ProgressFn>,
    passes: i64,
    lookups: i64,
    resolved: i64,
    missed: i64,
    failed: i64,
    repair: i64,
    applied: i64,
}

/// One store's catch-up worker.
pub struct CatchUp {
    mu: Mutex<CatchUpInner>,
}

static CATCH_LIVE: Mutex<Option<HashMap<String, Arc<CatchUp>>>> = Mutex::new(None);

fn catch_live() -> std::sync::MutexGuard<'static, Option<HashMap<String, Arc<CatchUp>>>> {
    CATCH_LIVE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `StartCatchUp` ensures the catch-up worker for `cfg.index_path` is running.
pub fn start_catch_up(cfg: CatchUpConfig, progress_cb: Option<ProgressFn>) -> Option<Arc<CatchUp>> {
    if cfg.index_path.is_empty() {
        return None;
    }
    let key = index_store_path(&cfg.index_path);
    let mut guard = catch_live();
    let map = guard.get_or_insert_with(HashMap::new);
    let c = map
        .entry(key)
        .or_insert_with(|| {
            Arc::new(CatchUp {
                mu: Mutex::new(CatchUpInner::default()),
            })
        })
        .clone();
    let mut inner = c.mu.lock().unwrap();
    inner.cfg = cfg.clone();
    if let Some(cb) = progress_cb {
        inner.cb = Some(cb);
    }
    if inner.running {
        drop(inner);
        drop(guard);
        return Some(c);
    }
    inner.running = true;
    let pass_cancel = Cancel::background();
    inner.cancel = Some(pass_cancel.clone());
    let c2 = Arc::clone(&c);
    let ctx = cfg.ctx.clone();
    let handle = std::thread::spawn(move || {
        let link = ctx.as_ref().map(|parent| {
            let link = CancelLink::new(parent);
            link.add(pass_cancel.clone());
            link
        });
        c2.pass(&pass_cancel);
        if let Some(link) = link {
            link.finish();
        }
        let mut inner = c2.mu.lock().unwrap();
        inner.running = false;
        inner.cancel = None;
    });
    inner.done = Some(handle);
    drop(inner);
    drop(guard);
    Some(c)
}

impl CatchUp {
    /// Blocks until the worker's current pass finishes.
    pub fn wait(&self) {
        let handle = self.mu.lock().unwrap().done.take();
        if let Some(h) = handle {
            let _ = h.join();
        }
    }

    /// The worker's cumulative counters.
    pub fn stats(&self) -> CatchUpStats {
        let inner = self.mu.lock().unwrap();
        CatchUpStats {
            passes: inner.passes,
            lookups: inner.lookups,
            resolved: inner.resolved,
            missed: inner.missed,
            failed: inner.failed,
            needs_repair: inner.repair,
            files_applied: inner.applied,
        }
    }

    fn post(&self, p: Progress) {
        let sink = self.mu.lock().unwrap().cb.clone();
        if let Some(cb) = sink {
            cb(p);
        }
    }

    /// One catch-up run.
    fn pass(&self, cancel: &Cancel) {
        let cfg = self.mu.lock().unwrap().cfg.clone();
        let idx = match shared_index(&cfg.index_path) {
            Ok(ix) => ix,
            Err(err) => {
                eprintln!(
                    "library: catch-up {:?}: {err} (pass continues in memory)",
                    cfg.index_path
                );
                return;
            }
        };
        if let Some(err) = idx.open_err.lock().unwrap().clone() {
            eprintln!(
                "library: catch-up {:?}: {err} (pass continues in memory)",
                cfg.index_path
            );
        }
        let resolver = new_resolver(&cfg.cache_dir);
        let overrides = load_overrides();
        let now = now_unix_nanos();

        let mut live: HashMap<String, bool> = HashMap::new();
        let mut probe = |key: &str| -> bool {
            if let Some(v) = live.get(key) {
                return *v;
            }
            let v = resolver.probe_cache(key);
            live.insert(key.to_string(), v);
            v
        };

        let records = idx.files_snapshot();
        let mut t = PassTally {
            files: records.len() as i64,
            ..PassTally::default()
        };
        let mut aggs: HashMap<String, KeyAgg> = HashMap::new();
        let mut by_path: HashMap<String, String> = HashMap::new();
        let mut pruned = 0;
        for rec in &records {
            if rec.meta_key.is_empty() {
                continue;
            }
            if rec.state != STATE_RESOLVED {
                if !under_roots(&cfg.dirs, &rec.path) {
                    continue;
                }
                if !file_exists(&rec.path) {
                    if idx.delete_file(&rec.path) {
                        pruned += 1;
                    }
                    continue;
                }
            }
            if overrides
                .resolve_for(&rec.path)
                .is_some()
            {
                apply_latch_verdict(&idx, rec, now);
                continue;
            }
            if is_extra(rec) {
                continue;
            }
            let agg = aggs.entry(rec.meta_key.clone()).or_insert_with(|| KeyAgg {
                query: rec.query(),
                paths: Vec::new(),
                unsettled: false,
                next_try: 0,
            });
            agg.paths.push(rec.path.clone());
            by_path.insert(normalize_scan_dir(&rec.path), rec.meta_key.clone());
            if rec.state == STATE_NEEDS_REPAIR && rec.next_try > agg.next_try {
                agg.next_try = rec.next_try;
            }
            if rec.state == STATE_RESOLVED && probe(&rec.meta_key) {
                continue;
            }
            agg.unsettled = true;
        }

        t.keys = aggs.len() as i64;
        t.files -= pruned;
        let mut groups: Vec<crate::Group> = Vec::new();
        for (key, agg) in &aggs {
            if !agg.unsettled {
                t.keys_resolved += 1;
                continue;
            }
            if !cfg.force && agg.next_try > 0 && now < agg.next_try {
                continue;
            }
            groups.push(crate::Group {
                key: key.clone(),
                query: agg.query.clone(),
                paths: agg.paths.clone(),
            });
        }
        if groups.is_empty() {
            self.finish_pass(&idx, &cfg, &t, &overrides);
            return;
        }

        {
            let mut inner = self.mu.lock().unwrap();
            inner.passes += 1;
            inner.lookups += groups.len() as i64;
        }

        let t_mu = Mutex::new(t);
        let src: Mutex<HashMap<String, bool>> = Mutex::new(HashMap::new());
        let lookup =
            |lctx: &Cancel, q: &metadata::Query| -> Result<metadata::Resolution, metadata::Error> {
                match resolver.lookup_now_force_src(lctx, q, cfg.force) {
                    Ok((res, from_cache)) => {
                        src.lock().unwrap().insert(dedupe_key(q), from_cache);
                        Ok(res)
                    }
                    Err(e) => Err(e),
                }
            };
        let lookup_ref: &crate::LookupFn<'_> = &lookup;
        let last_commit = Mutex::new(Instant::now());
        let apply = |g: &crate::Group, fr: crate::FetchResult| {
            let agg = &aggs[&g.key];
            let from_cache = src
                .lock()
                .unwrap()
                .get(&dedupe_key(&agg.query))
                .copied()
                .unwrap_or(false);
            let cancelled_lookup =
                fr.err.as_ref().is_some_and(is_cancelled_err) || cancel.err().is_some();
            if fr.err.is_some() && cancelled_lookup {
                let t = t_mu.lock().unwrap();
                let p = t.progress();
                drop(t);
                self.post(p);
                return;
            }
            {
                let mut t = t_mu.lock().unwrap();
                match &fr.err {
                    Some(_) => t.failed += 1,
                    None => {
                        if !resolved_usable(&fr.res) {
                            t.missed += 1;
                        } else if from_cache {
                            t.cached += 1;
                            t.keys_resolved += 1;
                        } else {
                            t.fetched += 1;
                            t.keys_resolved += 1;
                        }
                    }
                }
            }
            let (applied, repair_n, path) =
                idx.apply_verdict(&g.key, &by_path, &fr.res, fr.err.as_ref(), now_unix_nanos());
            let p = {
                let mut t = t_mu.lock().unwrap();
                t.applied += applied;
                t.repair += repair_n;
                if !path.is_empty() {
                    t.last_path = path;
                }
                t.progress()
            };
            self.post(p);
            let mut gate = last_commit.lock().unwrap();
            if gate.elapsed() > CATCH_UP_COMMIT_INTERVAL {
                *gate = Instant::now();
                drop(gate);
                if let Err(e) = idx.commit() {
                    eprintln!("library: catch-up commit: {e}");
                }
            }
        };
        let apply_ref: &crate::OnDone<'_> = &apply;

        fetch_all(
            cancel,
            &groups,
            lookup_ref,
            CATCH_UP_CONCURRENCY,
            Duration::ZERO,
            Some(apply_ref),
        );

        let t = t_mu.into_inner().unwrap();
        {
            let mut inner = self.mu.lock().unwrap();
            inner.resolved += t.keys_resolved;
            inner.missed += t.missed;
            inner.failed += t.failed;
            inner.applied += t.applied;
            inner.repair += t.repair;
        }

        if cancel.err().is_some() {
            if let Err(e) = idx.commit() {
                eprintln!("library: catch-up commit: {e}");
            }
            return;
        }
        self.finish_pass(&idx, &cfg, &t, &overrides);
    }

    fn finish_pass(
        &self,
        idx: &Index,
        cfg: &CatchUpConfig,
        t: &PassTally,
        overrides: &metadata::overrides::Store,
    ) {
        let state = idx.state();
        if let Err(e) = idx.commit() {
            eprintln!("library: catch-up commit: {e}");
        }
        self.post(Progress {
            done: t.files,
            total: t.files,
            resolved: state.keys_resolved,
            keys_total: t.keys,
            keys_done: state.keys_resolved + t.missed + t.failed,
            keys_cached: state.keys_resolved - t.fetched,
            keys_fetched: t.fetched,
            keys_not_found: t.missed,
            keys_failed: t.failed,
            last: t.last_path.clone(),
            ..Progress::default()
        });
        if state.files == 0 {
            return;
        }
        if let Err(e) = idx.write_inventory(&cfg.index_path, &cfg.app_version, overrides) {
            eprintln!("library: inventory report: {e}");
        }
    }
}

/// `StopCatchUp` cancels the running pass for `index_path` and waits for it.
pub fn stop_catch_up(index_path: &str) {
    let targets: Vec<Arc<CatchUp>> = {
        let guard = catch_live();
        match guard.as_ref() {
            None => Vec::new(),
            Some(map) => {
                if index_path.is_empty() {
                    map.values().cloned().collect()
                } else {
                    map.get(&index_store_path(index_path))
                        .cloned()
                        .into_iter()
                        .collect()
                }
            }
        }
    };
    for c in targets {
        let cancel = c.mu.lock().unwrap().cancel.clone();
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        c.wait();
    }
}

/// Whether the parser classifies a file as an extra rather than a metadata
/// gap: its path sits under an extras leaf ("Featurettes", "Deleted Scenes",
/// ...) or its rebuilt query is UNKNOWN (themes, filename-level extras, bare
/// names with no usable folder context). The live poll path never fetches
/// UNKNOWN kinds (`app\src\main.rs`), and providers will never answer for an
/// extras leaf, so the repair path must not park either as needs-repair.
pub(crate) fn is_extra(rec: &FileRecord) -> bool {
    metadata_core::context::under_extras_folder(&rec.path)
        || rec.query().kind == metadata::Kind::UNKNOWN
}

/// One metadata key's files for a pass.
struct KeyAgg {
    query: metadata::Query,
    paths: Vec<String>,
    unsettled: bool,
    next_try: i64,
}

/// One pass's counters.
#[derive(Default)]
struct PassTally {
    files: i64,
    keys: i64,
    keys_resolved: i64,
    cached: i64,
    fetched: i64,
    missed: i64,
    failed: i64,
    applied: i64,
    repair: i64,
    last_path: String,
}

impl PassTally {
    fn progress(&self) -> Progress {
        Progress {
            done: self.files,
            total: self.files,
            resolved: self.keys_resolved,
            keys_total: self.keys,
            keys_done: self.cached + self.fetched + self.missed + self.failed,
            keys_cached: self.cached,
            keys_fetched: self.fetched,
            keys_not_found: self.missed,
            keys_failed: self.failed,
            last: self.last_path.clone(),
            ..Progress::default()
        }
    }
}

impl Index {
    /// Writes the needs-repair report for this index into
    /// `<dir of indexPath>\inventory` and returns both report paths.
    ///
    /// The file partition is exact and needs no scan: `files = indexed =
    /// resolved + needs_repair + extras + pinned + pending`. `extras` are the
    /// rows the parser classifies as extras (`is_extra`): they never become
    /// needs-repair. `pinned` is counted from `overrides` HERE, at inventory
    /// time, so a latched file is counted once as pinned -- whatever else its
    /// record says -- and never listed as needs-repair. `pending` is every
    /// remaining file (still awaiting a verdict), and `needs_repair` equals
    /// `entries.len()`.
    pub(crate) fn write_inventory(
        &self,
        index_path: &str,
        app_version: &str,
        overrides: &metadata::overrides::Store,
    ) -> Result<(String, String), Error> {
        let mut r = repair::report::Report {
            generated_at: repair::time::Time::now(),
            app_version: app_version.to_string(),
            ..Default::default()
        };
        let records = self.files_snapshot();
        let mut resolved = 0i64;
        let mut pinned = 0i64;
        let mut extras = 0i64;
        let mut pending = 0i64;
        for rec in &records {
            if overrides
                .resolve_for(&rec.path)
                .is_some()
            {
                pinned += 1;
                continue;
            }
            if is_extra(rec) {
                extras += 1;
                continue;
            }
            match rec.state.as_str() {
                STATE_RESOLVED => resolved += 1,
                STATE_NEEDS_REPAIR => r.entries.push(repair::report::Entry {
                    path: rec.path.clone(),
                    name: rec.name.clone(),
                    kind: rec.kind.clone(),
                    reason: repair::report::REASON_NO_METADATA.to_string(),
                    attempts: rec.attempts,
                    last_error: rec.last_error.clone(),
                    last_try: nanos_to_time(rec.last_try),
                }),
                _ => pending += 1,
            }
        }
        let files = records.len() as i64;
        r.totals = repair::report::Totals {
            files,
            indexed: files,
            resolved,
            needs_repair: r.entries.len() as i64,
            extras,
            pinned,
            pending,
        };
        let dir = path_join(&[&path_dir(index_path), "inventory"]);
        let (json, txt) = repair::report::write_report(&dir, &r)
            .map_err(|e| Error::new(format!("library: inventory: {e}")))?;
        Ok((
            json.to_string_lossy().into_owned(),
            txt.to_string_lossy().into_owned(),
        ))
    }

    /// Writes one key's verdict onto every file record carrying it.
    pub(crate) fn apply_verdict(
        &self,
        key: &str,
        by_path: &HashMap<String, String>,
        res: &metadata::Resolution,
        lerr: Option<&metadata::Error>,
        now: i64,
    ) -> (i64, i64, String) {
        if key.is_empty() {
            return (0, 0, String::new());
        }
        let ok = lerr.is_none() && resolved_usable(res);
        let mut inner = self.inner.lock().unwrap();
        let mut applied = 0;
        let mut repair_n = 0;
        let mut last_path = String::new();
        for (norm, k) in by_path {
            if k != key {
                continue;
            }
            let Some(rec) = inner.files.get(norm) else {
                continue;
            };
            let mut next = rec.clone();
            next.last_try = now;
            if ok {
                next.state = STATE_RESOLVED.to_string();
                next.attempts = 0;
                next.last_error = String::new();
                next.next_try = 0;
            } else {
                next.attempts = rec.attempts + 1;
                next.last_error = verdict_error(lerr, res);
                if next.attempts >= MAX_REPAIR_ATTEMPTS {
                    next.state = STATE_NEEDS_REPAIR.to_string();
                    next.next_try = now + REPAIR_COOLDOWN.as_nanos() as i64;
                    repair_n += 1;
                } else {
                    next.state = STATE_PENDING.to_string();
                    next.next_try = 0;
                }
            }
            last_path = next.path.clone();
            crate::index::put_file_locked(&mut inner, next);
            applied += 1;
        }
        (applied, repair_n, last_path)
    }
}

/// `WriteInventory` writes the on-demand needs-repair report.
pub fn write_inventory(index_path: &str, app_version: &str) -> Result<(String, String), Error> {
    let idx = match shared_index(index_path) {
        Ok(ix) => ix,
        Err(err) => {
            eprintln!("library: inventory {index_path:?}: {err}");
            return Err(err);
        }
    };
    let overrides = load_overrides();
    idx.write_inventory(index_path, app_version, &overrides)
}

/// Loads the latch store a pass (and its report) honours: the launcher's
/// `%LOCALAPPDATA%\Potflix\overrides.json` ([`metadata::overrides::load`]).
/// A malformed store is reported and treated as empty -- the caller carries on
/// -- exactly as the store documents; the file is never rewritten here.
fn load_overrides() -> metadata::overrides::Store {
    let path = override_store_path();
    match metadata::overrides::load_from(&path) {
        Ok(store) => store,
        Err(err) => {
            eprintln!(
                "library: catch-up overrides {}: {err} (latch ignored until the store changes)",
                path.display()
            );
            metadata::overrides::Store::empty_at(path)
        }
    }
}

/// The store path [`load_overrides`] reads. Production honours the store's own
/// default location; a test reads its fixture path, or -- with no fixture -- a
/// path that never exists, so no test ever reads the owner's store.
fn override_store_path() -> PathBuf {
    #[cfg(test)]
    {
        OVERRIDES_PATH_TEST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("potflix-test-overrides-absent.json"))
    }
    #[cfg(not(test))]
    {
        metadata::overrides::default_path()
    }
}

#[cfg(test)]
static OVERRIDES_PATH_TEST: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Installs (or clears) the fixture store path the next pass reads; returns
/// the previous fixture. Module tests hold the shared test lock, so a fixture
/// never leaks into a concurrent test.
#[cfg(test)]
pub(crate) fn set_overrides_path_for_test(path: Option<PathBuf>) -> Option<PathBuf> {
    std::mem::replace(
        &mut *OVERRIDES_PATH_TEST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        path,
    )
}

/// Applies the latch's verdict to one record, resolved from the store alone:
/// `resolved`, never an attempt, no error, no cooldown. Returns whether the
/// record changed; an already-clean resolved record is left untouched.
fn apply_latch_verdict(idx: &Index, rec: &FileRecord, now: i64) -> bool {
    if rec.state == STATE_RESOLVED
        && rec.attempts == 0
        && rec.last_error.is_empty()
        && rec.next_try == 0
    {
        return false;
    }
    let mut next = rec.clone();
    next.state = STATE_RESOLVED.to_string();
    next.attempts = 0;
    next.last_error = String::new();
    next.next_try = 0;
    next.last_try = now;
    idx.put_file(next);
    true
}

fn is_cancelled_err(e: &metadata::Error) -> bool {
    matches!(e, metadata::Error::Cancelled(_))
}

/// The stored reason a lookup produced nothing usable.
fn verdict_error(lerr: Option<&metadata::Error>, res: &metadata::Resolution) -> String {
    if let Some(e) = lerr {
        return e.message().to_string();
    }
    if !res.found {
        return "no metadata found".to_string();
    }
    "metadata found but empty (no title, cover or synopsis)".to_string()
}

/// Whether a resolution carries anything worth keeping.
fn resolved_usable(r: &metadata::Resolution) -> bool {
    r.found
        && (!r.series.is_empty()
            || !r.episode.is_empty()
            || !r.artwork.is_empty()
            || !r.synopsis.is_empty())
}

/// Whether `path` lies under one of the configured roots.
fn under_roots(roots: &[String], path: &str) -> bool {
    if roots.is_empty() {
        return true;
    }
    let p = normalize_scan_dir(path);
    for root in roots {
        let r = normalize_scan_dir(root);
        if p == r {
            return true;
        }
        if p.len() > r.len()
            && p.get(r.len()..)
                .is_some_and(|rest| rest.starts_with(crate::pathutil::SEP))
        {
            return true;
        }
    }
    false
}

fn nanos_to_time(nanos: i64) -> repair::time::Time {
    let secs = nanos.div_euclid(1_000_000_000);
    let sub = nanos.rem_euclid(1_000_000_000);
    repair::time::Time::from_unix(secs, sub as u32)
}
