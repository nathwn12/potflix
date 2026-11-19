//! `library_test` -- mirrors `go\internal\library\library_test.go`, plus the
//! shared fixtures the other three test modules use. Every test takes the
//! process-wide lock (`ResolverScope`) so the resolver/parse/walk seams are
//! safe against the parallel test runner.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use metadata::{Kind, Media, Query, Resolution};
use metadata_http::cancel::Cancel;

use crate::catchup::{start_catch_up, stop_catch_up, CatchUpConfig};
use crate::pathutil::{join as path_join, normalize_scan_dir};
use crate::{
    collect_files, collect_files_with_context, collect_indexed_public,
    collect_indexed_with_context, fetch_all, load_scan_index, record_scan, scan, scan_with_options,
    FilePlan, Options, Progress, ProgressFn, WalkCb, WalkEntry, WalkFn, WalkResult,
};


static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn test_guard() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Holds the process-wide test lock and restores the resolver factory on drop.
pub(crate) struct ResolverScope {
    _serial: MutexGuard<'static, ()>,
}

impl ResolverScope {
    pub(crate) fn new() -> ResolverScope {
        let serial = test_guard();
        crate::set_resolver_factory(None);
        ResolverScope { _serial: serial }
    }

    pub(crate) fn set(&self, make: impl Fn(&str) -> metadata::Resolver + Send + Sync + 'static) {
        crate::set_resolver_factory(Some(Arc::new(make)));
    }
}

impl Drop for ResolverScope {
    fn drop(&mut self) {
        crate::set_resolver_factory(None);
    }
}

/// Holds the process-wide test lock and restores the walk override on drop.
pub(crate) struct WalkScope {
    prev: Option<Arc<WalkFn>>,
}

impl WalkScope {
    pub(crate) fn new() -> WalkScope {
        WalkScope {
            prev: crate::set_walk_override(None),
        }
    }

    pub(crate) fn set(&self, f: Arc<WalkFn>) {
        crate::set_walk_override(Some(f));
    }
}

impl Drop for WalkScope {
    fn drop(&mut self) {
        crate::set_walk_override(self.prev.take());
    }
}

/// Holds the process-wide test lock and restores the parser on drop.
pub(crate) struct ParseScope {
    prev: Option<crate::ParseFn>,
}

impl ParseScope {
    pub(crate) fn new() -> ParseScope {
        ParseScope {
            prev: crate::set_parse_override(None),
        }
    }

    pub(crate) fn set(&self, f: crate::ParseFn) {
        crate::set_parse_override(Some(f));
    }
}

impl Drop for ParseScope {
    fn drop(&mut self) {
        crate::set_parse_override(self.prev.take());
    }
}


static TEMP_SEQ: AtomicI32 = AtomicI32::new(0);

/// A unique temp directory removed (and its cache handles closed) on drop.
pub(crate) struct TempDir {
    pub path: String,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        metadata::cache::close_cache_handles();
        let _ = std::fs::remove_dir_all(&self.path);
        let _ = std::fs::remove_file(format!("{}.idx", self.path));
    }
}

pub(crate) fn temp_dir(tag: &str) -> TempDir {
    let base = std::env::temp_dir();
    loop {
        let n = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let path = base.join(format!("potflix-lib-{tag}-{}-{n}", std::process::id()));
        if std::fs::create_dir_all(&path).is_ok() {
            return TempDir {
                path: path.to_string_lossy().into_owned(),
            };
        }
    }
}

/// A metadata cache directory OUTSIDE any test root (the cache crate holds its
/// store open for the process, so cleanup closes the handles first).
pub(crate) fn meta_cache_dir() -> TempDir {
    temp_dir("meta")
}


fn now_fetched_at() -> String {
    repair::time::Time::now().rfc3339_nanos()
}

pub(crate) fn media_for(q: &Query, source: &str, tag: &str) -> Media {
    let mut m = Media {
        source: source.to_string(),
        source_id: "1".to_string(),
        kind: q.kind,
        title: q.title.clone(),
        year: q.year,
        fetched_at: now_fetched_at(),
        ..Media::default()
    };
    if q.kind != Kind::MOVIE {
        m.episodes = vec![metadata::Episode {
            season: i32::try_from(q.season).expect("test query season fits i32"),
            number: q.episode,
            title: tag.to_string(),
            ..metadata::Episode::default()
        }];
    }
    m
}

/// `fakeSource`: answers every query with media carrying `tag` as the title.
pub(crate) struct FakeSource {
    pub name: &'static str,
    pub tag: &'static str,
    pub calls: Arc<AtomicI32>,
}

impl FakeSource {
    pub(crate) fn new(tag: &'static str) -> FakeSource {
        FakeSource {
            name: "fake",
            tag,
            calls: Arc::new(AtomicI32::new(0)),
        }
    }
}

impl metadata::Source for FakeSource {
    fn name(&self) -> &str {
        self.name
    }

    fn find(&self, _cancel: &Cancel, q: &Query) -> Result<Option<Media>, metadata::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(media_for(q, "fake", self.tag)))
    }
}

/// `missSource`: every `find` is a definitive miss.
pub(crate) struct MissSource {
    pub calls: Arc<AtomicI32>,
}

impl MissSource {
    pub(crate) fn new() -> MissSource {
        MissSource {
            calls: Arc::new(AtomicI32::new(0)),
        }
    }
}

impl metadata::Source for MissSource {
    fn name(&self) -> &str {
        "miss"
    }

    fn find(&self, _cancel: &Cancel, _q: &Query) -> Result<Option<Media>, metadata::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

/// `honestSource`: one title resolves, "Missing" is a miss, "Unreachable"
/// fails transiently.
pub(crate) struct HonestSource;

impl metadata::Source for HonestSource {
    fn name(&self) -> &str {
        "honest"
    }

    fn find(&self, _cancel: &Cancel, q: &Query) -> Result<Option<Media>, metadata::Error> {
        if q.title.contains("Unreachable") {
            Err(metadata::Error::other("boom"))
        } else if q.title.contains("Missing") {
            Ok(None)
        } else {
            Ok(Some(media_for(q, "honest", "Found")))
        }
    }
}

fn block_until(cancel: &Cancel, release: &Arc<AtomicBool>) {
    loop {
        if cancel.err().is_some() || release.load(Ordering::SeqCst) {
            return;
        }
        let _ = cancel.wait(Duration::from_millis(2));
    }
}

/// `slowCancelSource`: blocks each lookup until cancelled or released.
pub(crate) struct SlowCancelSource {
    pub started: Arc<AtomicBool>,
    pub release: Arc<AtomicBool>,
    pub calls: Arc<AtomicI32>,
}

impl SlowCancelSource {
    pub(crate) fn new() -> SlowCancelSource {
        SlowCancelSource {
            started: Arc::new(AtomicBool::new(false)),
            release: Arc::new(AtomicBool::new(false)),
            calls: Arc::new(AtomicI32::new(0)),
        }
    }
}

impl metadata::Source for SlowCancelSource {
    fn name(&self) -> &str {
        "fake"
    }

    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, metadata::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.store(true, Ordering::SeqCst);
        block_until(cancel, &self.release);
        if let Some(err) = cancel.err() {
            return Err(metadata::Error::Cancelled(err));
        }
        Ok(Some(media_for(q, "fake", "V1")))
    }
}

/// `splitSpeedSource`: answers ShowA fast, blocks ShowB until cancelled or
/// released.
pub(crate) struct SplitSpeedSource {
    pub release: Arc<AtomicBool>,
}

impl metadata::Source for SplitSpeedSource {
    fn name(&self) -> &str {
        "fake"
    }

    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, metadata::Error> {
        if !q.title.to_lowercase().contains("showa") {
            block_until(cancel, &self.release);
        }
        if let Some(err) = cancel.err() {
            return Err(metadata::Error::Cancelled(err));
        }
        Ok(Some(media_for(q, "fake", "V1")))
    }
}

/// `gateSource`: resolves only a still-live lookup, and only after `proceed`.
pub(crate) struct GateSource {
    pub started: Arc<AtomicBool>,
    pub proceed: Arc<AtomicBool>,
}

impl metadata::Source for GateSource {
    fn name(&self) -> &str {
        "fake"
    }

    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, metadata::Error> {
        self.started.store(true, Ordering::SeqCst);
        block_until(cancel, &self.proceed);
        if let Some(err) = cancel.err() {
            return Err(metadata::Error::Cancelled(err));
        }
        Ok(Some(media_for(q, "fake", "V1")))
    }
}

/// `firstNThenBlockSource`: resolves the first `n` lookups, blocks the rest.
pub(crate) struct FirstNThenBlockSource {
    pub n: i32,
    pub calls: Arc<AtomicI32>,
    pub release: Arc<AtomicBool>,
}

impl metadata::Source for FirstNThenBlockSource {
    fn name(&self) -> &str {
        "fake"
    }

    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, metadata::Error> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 > self.n {
            block_until(cancel, &self.release);
        }
        if let Some(err) = cancel.err() {
            return Err(metadata::Error::Cancelled(err));
        }
        Ok(Some(media_for(q, "fake", "V1")))
    }
}

pub(crate) fn resolver_with(source: Arc<dyn metadata::Source>, dir: &str) -> metadata::Resolver {
    metadata::Resolver::new(metadata::Options {
        dir: dir.to_string(),
        sources: Some(vec![source]),
        ..Default::default()
    })
}


fn write_file(path: &str, content: &str) {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// `seedLib`: the same episode twice, a second episode, a KindUnknown trailer,
/// a movie, and a non-video file.
pub(crate) fn seed_lib() -> TempDir {
    let root = temp_dir("seed");
    for f in [
        path_join(&[&root.path, "A", "Breaking.Bad.S01E01.mkv"]),
        path_join(&[&root.path, "B", "Breaking.Bad.S01E01.mkv"]),
        path_join(&[&root.path, "B", "Breaking.Bad.S01E02.mkv"]),
        path_join(&[&root.path, "ShowName", "Extras", "SomeShow trailer.mkv"]),
        path_join(&[&root.path, "Movies", "The.Matrix.1999.mkv"]),
    ] {
        write_file(&f, "x");
    }
    write_file(&path_join(&[&root.path, "notes.txt"]), "x");
    root
}

pub(crate) fn seed_scan_lib() -> TempDir {
    let root = temp_dir("scanlib");
    for f in [
        path_join(&[&root.path, "A", "ShowA.S01E01.mkv"]),
        path_join(&[&root.path, "B", "ShowA.S01E01.mkv"]),
        path_join(&[&root.path, "B", "ShowB.S01E01.mkv"]),
    ] {
        write_file(&f, "x");
    }
    root
}

pub(crate) fn seed_index_corpus() -> (Vec<String>, TempDir) {
    let parent = temp_dir("corpus");
    let mut dirs = Vec::new();
    let seeds: [(&str, &[&str]); 3] = [
        ("d1", &["ShowA.S01E01.mkv", "ShowA.S01E02.mkv"]),
        ("d2", &["ShowB.S01E01.mkv"]),
        ("d3", &["ShowC.S01E01.mkv", "ShowCtrailer.mkv"]),
    ];
    for (d, names) in seeds {
        for n in names {
            write_file(&path_join(&[&parent.path, d, n]), "x");
        }
        dirs.push(path_join(&[&parent.path, d]));
    }
    (dirs, parent)
}

pub(crate) fn index_scan_opts(dirs: &[String], cache: &str, index: &str, force: bool) -> Options {
    Options {
        dirs: dirs.to_vec(),
        force,
        cache_dir: cache.to_string(),
        index_path: index.to_string(),
        use_index: true,
        skip_catch_up: true,
        ..Default::default()
    }
}

pub(crate) fn pad3(i: i32) -> String {
    format!("{i:03}")
}

pub(crate) fn index_path_in(dir: &TempDir) -> String {
    path_join(&[&dir.path, "scanindex.json"])
}


#[test]
fn build_plan_skips_unknown_and_dedupes() {
    let _rx = ResolverScope::new();
    let root = seed_lib();
    let (files, denied, err) = collect_files(&[root.path.clone()], 0);
    assert!(err.is_none());
    assert_eq!(denied, 0);
    assert_eq!(files.len(), 5);
    let (plans, groups) = crate::build_plan(&files);
    let skipped: Vec<&FilePlan> = plans.iter().filter(|p| p.skipped).collect();
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].path.ends_with("SomeShow trailer.mkv"));
    assert_eq!(groups.len(), 3);
    for g in &groups {
        if g.paths.len() == 2 {
            assert!(g.paths[0].contains("S01E01"));
        }
    }
}

#[test]
fn fetch_all_looks_up_once_per_unique_key() {
    let _rx = ResolverScope::new();
    let root = seed_lib();
    let (files, _, _) = collect_files(&[root.path.clone()], 0);
    let (_, groups) = crate::build_plan(&files);
    let calls = Arc::new(AtomicI32::new(0));
    let calls2 = Arc::clone(&calls);
    let lookup = move |_c: &Cancel, _q: &Query| -> Result<Resolution, metadata::Error> {
        calls2.fetch_add(1, Ordering::SeqCst);
        Ok(Resolution {
            found: true,
            ..Resolution::default()
        })
    };
    let lookup_ref: &crate::LookupFn<'_> = &lookup;
    let results = fetch_all(
        &Cancel::background(),
        &groups,
        lookup_ref,
        3,
        Duration::ZERO,
        None,
    );
    assert_eq!(calls.load(Ordering::SeqCst), groups.len() as i32);
    assert_eq!(results.len(), groups.len());
}

#[test]
fn collect_files_skips_denied_subdir() {
    let _rx = ResolverScope::new();
    let root = temp_dir("denied");
    let good = path_join(&[&root.path, "good.mkv"]);
    let other = path_join(&[&root.path, "other.mp4"]);
    let bad_dir = path_join(&[&root.path, "bad"]);
    let inner = path_join(&[&bad_dir, "hidden.mkv"]);

    let root_path = root.path.clone();
    let ws = WalkScope::new();
    let root_for_walk = root.path.clone();
    let good_c = good.clone();
    let other_c = other.clone();
    let bad_dir_c = bad_dir.clone();
    let inner_c = inner.clone();
    ws.set(Arc::new(
        move |walk_root: &str, cb: &mut WalkCb<'_>| -> std::io::Result<()> {
            assert_eq!(walk_root, root_path);
            let re = WalkEntry {
                name: crate::pathutil::base(walk_root),
                is_dir: true,
                ..WalkEntry::default()
            };
            if let WalkResult::Fail(e) = cb(walk_root, Some(&re), None) {
                return Err(e);
            }
            let ge = WalkEntry {
                name: "good.mkv".to_string(),
                ..WalkEntry::default()
            };
            if let WalkResult::Fail(e) = cb(&good_c, Some(&ge), None) {
                return Err(e);
            }
            let bd = WalkEntry {
                name: "bad".to_string(),
                is_dir: true,
                ..WalkEntry::default()
            };
            if let WalkResult::Fail(e) = cb(&bad_dir_c, Some(&bd), None) {
                return Err(e);
            }
            let perr = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "open bad");
            if let WalkResult::Fail(e) = cb(&bad_dir_c, Some(&bd), Some(&perr)) {
                return Err(e);
            }
            let _ = &inner_c;
            let oe = WalkEntry {
                name: "other.mp4".to_string(),
                ..WalkEntry::default()
            };
            if let WalkResult::Fail(e) = cb(&other_c, Some(&oe), None) {
                return Err(e);
            }
            let _ = root_for_walk;
            Ok(())
        },
    ));

    let (files, denied, err) = collect_files(&[root.path.clone()], 0);
    assert!(err.is_none());
    assert_eq!(denied, 1);
    assert_eq!(files.len(), 2);
    assert!(files.contains(&good));
    assert!(files.contains(&other));
    for f in &files {
        assert_ne!(f, &inner);
        assert!(!f.starts_with(&format!("{bad_dir}\\")));
    }
}

#[test]
fn walk_filters_and_tolerates_unreadable_root() {
    let _rx = ResolverScope::new();
    let root = temp_dir("walk");
    let video = path_join(&[&root.path, "Show.S01E01.mkv"]);
    write_file(&video, "x");
    write_file(&path_join(&[&root.path, "notes.txt"]), "x");
    let missing = path_join(&[&temp_dir("missing").path, "no-such-dir"]);

    let mut got: Vec<String> = Vec::new();
    let res = crate::walk(&[root.path.clone(), missing.clone()], |p| {
        got.push(p.to_string());
        Ok(())
    });
    assert!(res.is_ok());
    assert_eq!(got, vec![video]);

    assert!(crate::walk(&[missing], |_| Ok(())).is_err());
}

#[test]
fn scan_incremental_and_force() {
    let rx = ResolverScope::new();
    let root = seed_scan_lib();
    let cache = meta_cache_dir();
    let ctx = Cancel::background();

    let fake1 = Arc::new(FakeSource::new("V1"));
    let f1 = Arc::clone(&fake1);
    let cache1 = cache.path.clone();
    rx.set(move |dir| resolver_with(f1.clone(), dir));
    let _ = cache1;

    let last: Arc<Mutex<Option<Progress>>> = Arc::new(Mutex::new(None));
    let last2 = Arc::clone(&last);
    let pf: ProgressFn = Arc::new(move |p| *last2.lock().unwrap() = Some(p));
    let st1 = scan(&ctx, &[root.path.clone()], false, &cache.path, Some(pf)).unwrap();
    assert_eq!(st1.files, 3);
    assert_eq!(st1.skipped, 0);
    assert_eq!(st1.unique, 2);
    assert_eq!(st1.resolved, 3);
    assert_eq!(st1.cached, 0);
    assert_eq!(st1.fetched, 3);
    assert_eq!(fake1.calls.load(Ordering::SeqCst), 2);
    let lp = last.lock().unwrap().clone().unwrap();
    assert_eq!((lp.done, lp.total, lp.resolved), (3, 3, 3));
    assert_eq!((lp.cached, lp.fetched), (0, 3));

    let q = metadata_core::context::resolve_query(
        "ShowA.S01E01.mkv",
        &path_join(&[&root.path, "A", "ShowA.S01E01.mkv"]),
    );
    let offline = metadata::Resolver::new(metadata::Options {
        dir: cache.path.clone(),
        offline: true,
        ..Default::default()
    });
    let res = offline.lookup_now(&ctx, &q).unwrap();
    assert!(res.found);
    assert_eq!(res.episode, "V1");
    for n in ["ShowA.S01E01.mkv", "ShowB.S01E01.mkv"] {
        let key = metadata::cache::cache_key_for(&metadata_core::context::resolve_query(
            n,
            &path_join(&[&root.path, "A", n]),
        ));
        assert!(metadata::cache::cache_has(&cache.path, &key));
    }

    // Run 2 (non-force): no fetch, served from cache.
    let fake2 = Arc::new(FakeSource::new("V2-should-never-fetch"));
    let f2 = Arc::clone(&fake2);
    rx.set(move |dir| resolver_with(f2.clone(), dir));
    let st2 = scan(&ctx, &[root.path.clone()], false, &cache.path, None).unwrap();
    assert_eq!(st2.resolved, st1.resolved);
    assert_eq!((st2.cached, st2.fetched), (3, 0));
    assert_eq!(fake2.calls.load(Ordering::SeqCst), 0);
    for n in ["ShowA.S01E01.mkv", "ShowB.S01E01.mkv"] {
        let key = metadata::cache::cache_key_for(&metadata_core::context::resolve_query(
            n,
            &path_join(&[&root.path, "A", n]),
        ));
        assert!(metadata::cache::cache_live(&cache.path, &key));
    }

    // Run 3 (force): refetch, rewrite.
    std::thread::sleep(Duration::from_millis(60));
    let fake3 = Arc::new(FakeSource::new("V2"));
    let f3 = Arc::clone(&fake3);
    rx.set(move |dir| resolver_with(f3.clone(), dir));
    let st3 = scan(&ctx, &[root.path.clone()], true, &cache.path, None).unwrap();
    assert_eq!((st3.cached, st3.fetched), (0, 3));
    assert_eq!(fake3.calls.load(Ordering::SeqCst), 2);
    let offline3 = metadata::Resolver::new(metadata::Options {
        dir: cache.path.clone(),
        offline: true,
        ..Default::default()
    });
    let res = offline3.lookup_now(&ctx, &q).unwrap();
    assert!(res.found);
    assert_eq!(res.episode, "V2");
}

#[test]
fn scan_cached_vs_fetched() {
    let rx = ResolverScope::new();
    let root = seed_scan_lib();
    let cache = meta_cache_dir();
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));

    let fill = scan(&ctx, &[root.path.clone()], false, &cache.path, None).unwrap();
    assert_eq!((fill.resolved, fill.cached, fill.fetched), (3, 0, 3));
    let rescan = scan(&ctx, &[root.path.clone()], false, &cache.path, None).unwrap();
    assert_eq!((rescan.files, rescan.unique, rescan.resolved), (3, 2, 3));
    assert_eq!((rescan.cached, rescan.fetched), (3, 0));
    let refresh = scan(&ctx, &[root.path.clone()], true, &cache.path, None).unwrap();
    assert_eq!(refresh.resolved, 3);
    assert_eq!((refresh.cached, refresh.fetched), (0, 3));
}

#[test]
fn index_second_scan_skips_unchanged() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));

    let st1 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st1.folders_skipped, st1.files_skipped), (0, 0));
    assert_eq!(st1.folders_scanned, 3);
    let total_files = st1.files;

    let visits = Arc::new(AtomicI32::new(0));
    let v2 = Arc::clone(&visits);
    let ws = WalkScope::new();
    ws.set(Arc::new(
        move |root: &str, cb: &mut WalkCb<'_>| -> std::io::Result<()> {
            let v2 = Arc::clone(&v2);
            crate::real_walk_dir(root, &mut |p, d, e| {
                if e.is_none() && d.map(|d| !d.is_dir).unwrap_or(false) {
                    v2.fetch_add(1, Ordering::SeqCst);
                }
                cb(p, d, e)
            })
        },
    ));

    let last: Arc<Mutex<Option<Progress>>> = Arc::new(Mutex::new(None));
    let last2 = Arc::clone(&last);
    let pf: ProgressFn = Arc::new(move |p| *last2.lock().unwrap() = Some(p));
    let mut opts = index_scan_opts(&dirs, &cache.path, &index, false);
    opts.progress = Some(pf);
    let st2 = scan_with_options(&ctx, opts).unwrap();
    assert_eq!(st2.folders_scanned, 0);
    assert_eq!(st2.folders_skipped, 3);
    assert_eq!(st2.files, 0);
    assert_eq!(st2.files_skipped, total_files);
    assert_eq!(visits.load(Ordering::SeqCst), 0);
    let lp = last.lock().unwrap().clone().unwrap();
    assert_eq!((lp.folders_scanned, lp.folders_skipped), (0, 3));
}

#[test]
fn index_add_file_rescans_only_that_folder() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    write_file(&path_join(&[&dirs[0], "ShowA.S01E03.mkv"]), "x");
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st2.folders_scanned, st2.folders_skipped), (1, 2));
}

#[test]
fn index_rename_is_not_skipped() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let trailer = path_join(&[&dirs[2], "ShowCtrailer.mkv"]);
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    std::fs::rename(&trailer, path_join(&[&dirs[2], "ShowCfeaturette.mkv"])).unwrap();
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st2.folders_scanned, st2.folders_skipped), (1, 2));
}

#[test]
fn index_stale_tier1_refreshes_via_tier2() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    let ix = load_scan_index(&index);
    let rec = ix.lookup(&dirs[0]).unwrap();
    ix.record_full(
        &dirs[0],
        "stale-names-hash",
        &rec.fingerprint,
        rec.schema,
        &rec.keys,
        rec.files,
    );
    ix.save(&index).unwrap();
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st2.folders_scanned, st2.folders_skipped), (0, 3));
    let now = crate::scanindex::folder_names_hash(&dirs[0]).unwrap();
    let got = load_scan_index(&index).lookup(&dirs[0]).unwrap();
    assert_eq!(got.names_hash, now);
}

#[test]
fn index_missing_cache_key_disables_skip() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fast = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fast);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    start_catch_up(
        CatchUpConfig {
            dirs: dirs.clone(),
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    let before = fast.calls.load(Ordering::SeqCst);
    assert!(before > 0);
    metadata::cache::clear_cache(&cache.path).unwrap();
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st2.folders_scanned, st2.folders_skipped), (0, 3));
    start_catch_up(
        CatchUpConfig {
            dirs: dirs.clone(),
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    let after = fast.calls.load(Ordering::SeqCst);
    assert_eq!(after - before, before);
}

#[test]
fn index_force_rewrites_without_skipping() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    let before = load_scan_index(&index).lookup(&dirs[0]).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    let st = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, true)).unwrap();
    assert_eq!(st.folders_skipped, 0);
    assert_eq!(st.folders_scanned, 3);
    let after = load_scan_index(&index).lookup(&dirs[0]).unwrap();
    assert!(crate::scanindex::same_scan_record(&before, &after));
    std::fs::remove_file(path_join(&[&dirs[0], "ShowA.S01E02.mkv"])).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, true)).unwrap();
    let rec = load_scan_index(&index).lookup(&dirs[0]).unwrap();
    assert_eq!(rec.files, 1);
}

#[test]
fn index_schema_bump_skips_nothing() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    let ix = load_scan_index(&index);
    for d in &dirs {
        let rec = ix.lookup(d).unwrap();
        ix.record_full(
            d,
            &rec.names_hash,
            &rec.fingerprint,
            metadata::cache::cache_schema() + 1,
            &rec.keys,
            rec.files,
        );
    }
    ix.save(&index).unwrap();
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st2.folders_scanned, st2.folders_skipped), (3, 0));
}

#[test]
fn index_missing_or_corrupt_is_full_walk() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    let st1 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st1.folders_scanned, st1.folders_skipped), (3, 0));
    crate::index::close_index(&index).unwrap();
    std::fs::write(
        crate::index::index_store_path(&index),
        "{{not a store file at all, forty bytes or so",
    )
    .unwrap();
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st2.folders_scanned, st2.folders_skipped), (3, 0));
    assert!(std::fs::metadata(format!(
        "{}.corrupt",
        crate::index::index_store_path(&index)
    ))
    .is_ok());
}

#[test]
fn index_all_negative_folder_skipped() {
    let rx = ResolverScope::new();
    let parent = temp_dir("neg");
    let folder = path_join(&[&parent.path, "missfolder"]);
    for n in ["ShowM.S01E01.mkv", "ShowN.S01E01.mkv"] {
        write_file(&path_join(&[&folder, n]), "x");
    }
    let dirs = vec![folder.clone()];
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();

    let miss1 = Arc::new(MissSource::new());
    let m1 = Arc::clone(&miss1);
    rx.set(move |dir| resolver_with(m1.clone(), dir));
    let st1 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!((st1.files, st1.pending, st1.resolved), (2, 2, 0));
    assert_eq!((st1.folders_scanned, st1.folders_skipped), (1, 0));
    start_catch_up(
        CatchUpConfig {
            dirs: dirs.clone(),
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    for n in ["ShowM.S01E01.mkv", "ShowN.S01E01.mkv"] {
        let q = metadata_core::context::resolve_query(n, &path_join(&[&folder, n]));
        assert!(metadata::cache::cache_has(
            &cache.path,
            &metadata::cache::cache_key_for(&q)
        ));
    }
    let miss2 = Arc::new(MissSource::new());
    let m2 = Arc::clone(&miss2);
    rx.set(move |dir| resolver_with(m2.clone(), dir));
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!(st2.folders_scanned, 0);
    assert_eq!(st2.folders_skipped, 1);
    assert_eq!(st2.files, 0);
    assert_eq!(st2.files_skipped, 2);
    assert_eq!(miss2.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn scan_keys_fully_cached() {
    let rx = ResolverScope::new();
    let root = seed_scan_lib();
    let cache = meta_cache_dir();
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan(&ctx, &[root.path.clone()], false, &cache.path, None).unwrap();

    let calls: Arc<Mutex<Vec<Progress>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = Arc::clone(&calls);
    let pf: ProgressFn = Arc::new(move |p| c2.lock().unwrap().push(p));
    let st = scan(&ctx, &[root.path.clone()], false, &cache.path, Some(pf)).unwrap();
    let calls = calls.lock().unwrap();
    assert!(!calls.is_empty());
    let first = &calls[0];
    assert_eq!((first.keys_total, first.keys_done), (2, 2));
    assert_eq!(first.keys_fetched, 0);
    for p in calls.iter() {
        assert_eq!(
            p.keys_done,
            p.keys_cached + p.keys_fetched + p.keys_not_found + p.keys_failed
        );
    }
    assert_eq!(st.keys, 2);
    assert_eq!((st.keys_cached, st.keys_done), (2, 2));
    assert_eq!(
        st.keys_done,
        st.keys_cached + st.keys_fetched + st.keys_not_found + st.keys_failed
    );
}

#[test]
fn scan_keys_partially_cached() {
    let rx = ResolverScope::new();
    let root = seed_scan_lib();
    let cache = meta_cache_dir();
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan(&ctx, &[root.path.clone()], false, &cache.path, None).unwrap();

    write_file(&path_join(&[&root.path, "C", "ShowC.S01E01.mkv"]), "x");
    let miss = Arc::new(MissSource::new());
    let m = Arc::clone(&miss);
    rx.set(move |dir| resolver_with(m.clone(), dir));

    let calls: Arc<Mutex<Vec<Progress>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = Arc::clone(&calls);
    let pf: ProgressFn = Arc::new(move |p| c2.lock().unwrap().push(p));
    let st = scan(&ctx, &[root.path.clone()], false, &cache.path, Some(pf)).unwrap();
    let calls = calls.lock().unwrap();
    let first = &calls[0];
    assert!(first.keys_cached > 0);
    assert_eq!(first.keys_done, first.keys_cached);
    assert_eq!(first.keys_total, 3);
    for p in calls.iter() {
        assert_eq!(
            p.keys_done,
            p.keys_cached + p.keys_fetched + p.keys_not_found + p.keys_failed
        );
    }
    assert_eq!(st.keys, 3);
    assert_eq!(st.keys_cached, 2);
    assert_eq!(st.keys_done, st.keys);
}

#[test]
fn scan_cancel_returns_partial_quickly() {
    let _rx = ResolverScope::new();
    let root = seed_scan_lib();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let seen = Arc::new(AtomicI32::new(0));
    let s2 = Arc::clone(&seen);
    let cancel_for = ctx.clone();
    let ws = WalkScope::new();
    ws.set(Arc::new(
        move |root: &str, cb: &mut WalkCb<'_>| -> std::io::Result<()> {
            let s2 = Arc::clone(&s2);
            let cancel_for = cancel_for.clone();
            crate::real_walk_dir(root, &mut |p, d, e| {
                if e.is_none() && d.map(|d| !d.is_dir).unwrap_or(false) {
                    if s2.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                        cancel_for.cancel();
                    }
                }
                if cancel_for.err().is_some() {
                    return WalkResult::Fail(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ));
                }
                cb(p, d, e)
            })
        },
    ));
    let start = Instant::now();
    let st = scan_with_options(
        &ctx,
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(st.cancelled);
    assert!(st.files > 0);
    let fi = std::fs::metadata(crate::index::index_store_path(&index)).unwrap();
    assert!(fi.len() > 0);
    assert_eq!(load_scan_index(&index).len(), 0);
}

#[test]
fn scan_resume_after_cancel_skips_cached_folders() {
    let rx = ResolverScope::new();
    let root = seed_scan_lib();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);

    let fast = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fast);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    let st1 = scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    assert!(!st1.cancelled);
    start_catch_up(
        CatchUpConfig {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();

    let resume = Arc::new(FakeSource::new("V1"));
    let r = Arc::clone(&resume);
    rx.set(move |dir| resolver_with(r.clone(), dir));
    let st2 = scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    assert!(!st2.cancelled);
    assert_eq!((st2.folders_scanned, st2.files_skipped), (0, 3));
    assert_eq!(resume.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn scan_checkpoint_exists_before_fetch_completes() {
    let rx = ResolverScope::new();
    let root = temp_dir("ckpt");
    for i in 0..60 {
        let f = path_join(&[
            &root.path,
            &format!("F{}", pad3(i)),
            &format!("Show{}.S01E01.mkv", pad3(i)),
        ]);
        write_file(&f, "x");
    }
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let slow = Arc::new(SlowCancelSource::new());
    let s = Arc::clone(&slow);
    rx.set(move |dir| resolver_with(s.clone(), dir));
    scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();

    let c = start_catch_up(
        CatchUpConfig {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !slow.started.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let fi = std::fs::metadata(crate::index::index_store_path(&index)).unwrap();
    assert!(fi.len() > 0);
    slow.release.store(true, Ordering::SeqCst);
    c.wait();
}

#[test]
fn scan_cancelled_walk_records_nothing() {
    let _rx = ResolverScope::new();
    let root = temp_dir("cancelwalk");
    let dir_a = path_join(&[&root.path, "A"]);
    let dir_b = path_join(&[&root.path, "B"]);
    for f in [
        path_join(&[&dir_a, "ShowA.S01E01.mkv"]),
        path_join(&[&dir_b, "ShowB.S01E01.mkv"]),
    ] {
        write_file(&f, "x");
    }
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let cancel_for = ctx.clone();
    let ws = WalkScope::new();
    ws.set(Arc::new(
        move |root: &str, cb: &mut WalkCb<'_>| -> std::io::Result<()> {
            let cancel_for = cancel_for.clone();
            crate::real_walk_dir(root, &mut |p, d, e| {
                if e.is_none()
                    && d.map(|d| d.is_dir).unwrap_or(false)
                    && crate::pathutil::base(p) == "B"
                {
                    cancel_for.cancel();
                    return WalkResult::Fail(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "cancelled",
                    ));
                }
                cb(p, d, e)
            })
        },
    ));
    let st = scan_with_options(
        &ctx,
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    assert!(st.cancelled);
    let ix = load_scan_index(&index);
    for d in [&root.path, &dir_a, &dir_b] {
        if let Some(rec) = ix.lookup(d) {
            panic!("recorded {d}: files={} keys={}", rec.files, rec.keys.len());
        }
    }
}

#[test]
fn record_index_never_stamps_stale_folder() {
    let _rx = ResolverScope::new();
    let folder = temp_dir("stale");
    write_file(&path_join(&[&folder.path, "ShowA.S01E01.mkv"]), "x");
    let (files, _, _) = collect_files(&[folder.path.clone()], 0);
    assert_eq!(files.len(), 1);
    let (plans, _) = crate::build_plan(&files);
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    write_file(&path_join(&[&folder.path, "ShowB.S01E01.mkv"]), "x");
    record_scan(&index, &[folder.path.clone()], &HashMap::new(), &plans);
    assert!(load_scan_index(&index).lookup(&folder.path).is_none());

    let (files2, _, _) = collect_files(&[folder.path.clone()], 0);
    let (plans2, _) = crate::build_plan(&files2);
    record_scan(&index, &[folder.path.clone()], &HashMap::new(), &plans2);
    let rec = load_scan_index(&index).lookup(&folder.path).unwrap();
    assert_eq!(rec.files, 2);
    assert_eq!(rec.keys.len(), 2);
}

#[test]
fn should_skip_validity_aware() {
    let rx = ResolverScope::new();
    let ctx = Cancel::background();
    let live = |cache: String| move |k: &str| metadata::cache::cache_live(&cache, k);
    let _ = live;

    let pos_dir = temp_dir("pos");
    write_file(&path_join(&[&pos_dir.path, "ShowP.S01E01.mkv"]), "x");
    let pos_cache = meta_cache_dir();
    let pos_idxdir = temp_dir("idxs");
    let pos_index = index_path_in(&pos_idxdir);
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(
        &ctx,
        index_scan_opts(&[pos_dir.path.clone()], &pos_cache.path, &pos_index, false),
    )
    .unwrap();
    start_catch_up(
        CatchUpConfig {
            dirs: vec![pos_dir.path.clone()],
            cache_dir: pos_cache.path.clone(),
            index_path: pos_index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    rx.set(|_| metadata::Resolver::new(metadata::Options::default()));
    let pos_fp = crate::scanindex::folder_fingerprint(&pos_dir.path).unwrap();
    let pos_cache2 = pos_cache.path.clone();
    assert!(load_scan_index(&pos_index).should_skip(
        &pos_dir.path,
        &pos_fp,
        metadata::cache::cache_schema(),
        Some(&|k: &str| metadata::cache::cache_live(&pos_cache2, k)),
    ));
    assert!(load_scan_index(&pos_index).should_skip(
        &pos_dir.path,
        &pos_fp,
        metadata::cache::cache_schema(),
        Some(&|_: &str| true),
    ));

    let neg_dir = temp_dir("neg2");
    let neg_path = path_join(&[&neg_dir.path, "ShowM.S01E01.mkv"]);
    write_file(&neg_path, "x");
    let neg_cache = meta_cache_dir();
    let neg_idxdir = temp_dir("idxs");
    let neg_index = index_path_in(&neg_idxdir);
    let miss = Arc::new(MissSource::new());
    let m = Arc::clone(&miss);
    rx.set(move |dir| resolver_with(m.clone(), dir));
    scan_with_options(
        &ctx,
        index_scan_opts(&[neg_dir.path.clone()], &neg_cache.path, &neg_index, false),
    )
    .unwrap();
    start_catch_up(
        CatchUpConfig {
            dirs: vec![neg_dir.path.clone()],
            cache_dir: neg_cache.path.clone(),
            index_path: neg_index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    rx.set(|_| metadata::Resolver::new(metadata::Options::default()));
    let neg_fp = crate::scanindex::folder_fingerprint(&neg_dir.path).unwrap();
    let neg_q = metadata_core::context::resolve_query("ShowM.S01E01.mkv", &neg_path);
    let neg_key = metadata::cache::cache_key_for(&neg_q);
    let neg_cache2 = neg_cache.path.clone();
    assert!(metadata::cache::cache_live(&neg_cache.path, &neg_key));
    assert!(load_scan_index(&neg_index).should_skip(
        &neg_dir.path,
        &neg_fp,
        metadata::cache::cache_schema(),
        Some(&|k: &str| metadata::cache::cache_live(&neg_cache2, k)),
    ));
    metadata::cache::clear_cache(&neg_cache.path).unwrap();
    let neg_cache3 = neg_cache.path.clone();
    assert!(!load_scan_index(&neg_index).should_skip(
        &neg_dir.path,
        &neg_fp,
        metadata::cache::cache_schema(),
        Some(&|k: &str| metadata::cache::cache_live(&neg_cache3, k)),
    ));
}

#[test]
fn collect_files_with_context_honours_cancel() {
    let _rx = ResolverScope::new();
    let root = seed_scan_lib();
    let ws = WalkScope::new();
    let started = Arc::new(AtomicBool::new(false));
    let ctx = Cancel::background();
    let started2 = Arc::clone(&started);
    let ctx2 = ctx.clone();
    ws.set(Arc::new(
        move |_root: &str, _cb: &mut WalkCb<'_>| -> std::io::Result<()> {
            started2.store(true, Ordering::SeqCst);
            let _ = ctx2.wait(Duration::from_secs(5));
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "cancelled",
            ))
        },
    ));
    let ctx3 = ctx.clone();
    let handle =
        std::thread::spawn(move || collect_files_with_context(&ctx3, &[root.path.clone()], 0));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(50));
    let cancel_at = Instant::now();
    ctx.cancel();
    let (_files, _denied, cancelled, err) = handle.join().unwrap();
    assert!(cancel_at.elapsed() < Duration::from_secs(2));
    assert!(err.is_none());
    assert!(cancelled);
}

#[test]
fn scan_cancelled_fetch_counts_no_error() {
    let rx = ResolverScope::new();
    let root = seed_scan_lib();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let slow = Arc::new(SlowCancelSource::new());
    let s = Arc::clone(&slow);
    rx.set(move |dir| resolver_with(s.clone(), dir));
    scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    let c = start_catch_up(
        CatchUpConfig {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !slow.started.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    stop_catch_up(&index);
    slow.release.store(true, Ordering::SeqCst);
    c.wait();
    let stats = c.stats();
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.missed, 0);
    assert_eq!(load_scan_index(&index).state().files_needs_repair, 0);
}

#[test]
fn scan_transient_failure_honest_counters() {
    let rx = ResolverScope::new();
    let ctx = Cancel::background();
    let root = temp_dir("honest");
    for n in [
        "Found.S01E01.mkv",
        "Missing.S01E01.mkv",
        "Unreachable.S01E01.mkv",
    ] {
        write_file(&path_join(&[&root.path, n]), "x");
    }
    let cache = meta_cache_dir();
    rx.set(|dir| resolver_with(Arc::new(HonestSource), dir));
    let calls: Arc<Mutex<Vec<Progress>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = Arc::clone(&calls);
    let pf: ProgressFn = Arc::new(move |p| c2.lock().unwrap().push(p));
    let st = scan(&ctx, &[root.path.clone()], false, &cache.path, Some(pf)).unwrap();
    assert_eq!(st.keys, 3);
    assert_eq!(
        (
            st.keys_fetched,
            st.keys_not_found,
            st.keys_failed,
            st.keys_cached
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(st.keys_done, 3);
    assert_eq!(st.resolved, 1);
    assert_eq!(st.not_found, 1);
    assert_eq!(st.errors, 1);
    for p in calls.lock().unwrap().iter() {
        assert_eq!(
            p.keys_done,
            p.keys_cached + p.keys_fetched + p.keys_not_found + p.keys_failed
        );
    }
    for (name, want) in [
        ("Found.S01E01.mkv", true),
        ("Missing.S01E01.mkv", true),
        ("Unreachable.S01E01.mkv", false),
    ] {
        let key = metadata::cache::cache_key_for(&metadata_core::context::resolve_query(
            name,
            &path_join(&[&root.path, name]),
        ));
        assert_eq!(
            metadata::cache::cache_has(&cache.path, &key),
            want,
            "{name}"
        );
    }
}

#[test]
fn scan_use_index_off_matches_plain_scan() {
    let rx = ResolverScope::new();
    let seed_unique = || {
        let root = temp_dir("uniq");
        for f in [
            path_join(&[&root.path, "A", "UniqShowA.S01E01.mkv"]),
            path_join(&[&root.path, "B", "UniqShowB.S02E03.mkv"]),
            path_join(&[&root.path, "B", "UniqShowC.S01E01.mkv"]),
            path_join(&[&root.path, "Movies", "UniqFilm.2001.mkv"]),
            path_join(&[&root.path, "Extras", "UniqShowA trailer.mkv"]),
        ] {
            write_file(&f, "x");
        }
        root
    };
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    let ctx = Cancel::background();

    let root1 = seed_unique();
    let cache1 = meta_cache_dir();
    let last1: Arc<Mutex<Option<Progress>>> = Arc::new(Mutex::new(None));
    let l1 = Arc::clone(&last1);
    let pf1: ProgressFn = Arc::new(move |p| *l1.lock().unwrap() = Some(p));
    let st_plain = scan(&ctx, &[root1.path.clone()], false, &cache1.path, Some(pf1)).unwrap();

    let root2 = seed_unique();
    let cache2 = meta_cache_dir();
    let last2: Arc<Mutex<Option<Progress>>> = Arc::new(Mutex::new(None));
    let l2 = Arc::clone(&last2);
    let pf2: ProgressFn = Arc::new(move |p| *l2.lock().unwrap() = Some(p));
    let st_opt = scan_with_options(
        &ctx,
        Options {
            dirs: vec![root2.path.clone()],
            cache_dir: cache2.path.clone(),
            progress: Some(pf2),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(st_plain, st_opt);
    let mut lp = last1.lock().unwrap().clone().unwrap();
    let mut lo = last2.lock().unwrap().clone().unwrap();
    lp.last = String::new();
    lo.last = String::new();
    assert_eq!(lp, lo);
}

#[test]
fn scan_indexed_first_pass_matches_plain_tallies() {
    let rx = ResolverScope::new();
    let ctx = Cancel::background();
    let root = temp_dir("idxfirst");
    for f in [
        path_join(&[&root.path, "A", "UniqShowA.S01E01.mkv"]),
        path_join(&[&root.path, "B", "UniqShowB.S02E03.mkv"]),
        path_join(&[&root.path, "B", "UniqShowC.S01E01.mkv"]),
        path_join(&[&root.path, "Movies", "UniqFilm.2001.mkv"]),
        path_join(&[&root.path, "Extras", "UniqShowA trailer.mkv"]),
    ] {
        write_file(&f, "x");
    }
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    let cache = meta_cache_dir();
    let st_plain = scan(&ctx, &[root.path.clone()], false, &cache.path, None).unwrap();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let st_idx = scan_with_options(
        &ctx,
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    assert_eq!(st_idx.files, st_plain.files);
    assert_eq!(st_idx.skipped, st_plain.skipped);
    assert_eq!(st_idx.folders_scanned, st_plain.folders_scanned);
    assert_eq!(st_idx.folders_skipped, st_plain.folders_skipped);
    assert_eq!(st_idx.pending, st_plain.files - st_plain.skipped);
    start_catch_up(
        CatchUpConfig {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    let state = load_scan_index(&index).state();
    assert_eq!(state.keys, st_plain.keys);
    assert_eq!(state.keys_resolved, st_plain.keys);
}

#[test]
fn collect_skipped_sets_indexed_second_pass() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    let st1 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    start_catch_up(
        CatchUpConfig {
            dirs: dirs.clone(),
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    let cr = collect_indexed_public(&dirs, &cache.path, &index, true, false).unwrap();
    assert!(cr.visited.is_empty());
    assert_eq!(cr.skipped.len(), 3);
    for d in &dirs {
        assert!(cr.skipped.contains_key(&normalize_scan_dir(d)));
    }
    assert_eq!(cr.files_skipped, st1.files);
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!(
        (st2.folders_scanned, st2.folders_skipped, st2.files_skipped),
        (0, 3, st1.files)
    );
}

#[test]
fn scan_cancel_fetch_phase_keeps_checkpoint() {
    let rx = ResolverScope::new();
    let (_dirs, parent) = seed_index_corpus();
    let root = parent.path.clone();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let block = Arc::new(FirstNThenBlockSource {
        n: crate::catchup::CATCH_UP_CONCURRENCY as i32,
        calls: Arc::new(AtomicI32::new(0)),
        release: Arc::new(AtomicBool::new(false)),
    });
    let b = Arc::clone(&block);
    rx.set(move |dir| resolver_with(b.clone(), dir));
    scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.clone()], &cache.path, &index, false),
    )
    .unwrap();
    let c = start_catch_up(
        CatchUpConfig {
            dirs: vec![root.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while load_scan_index(&index).state().keys_resolved == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    stop_catch_up(&index);
    block.release.store(true, Ordering::SeqCst);
    c.wait();
    let state = load_scan_index(&index).state();
    assert!(state.keys_resolved > 0);
    crate::index::close_index(&index).unwrap();
    let reopened = load_scan_index(&index).state();
    assert!(reopened.keys_resolved > 0);
    assert_eq!(reopened.files_needs_repair, 0);
}

#[test]
fn stale_record_scan_refuses_new_subdir() {
    let rx = ResolverScope::new();
    let ctx = Cancel::background();
    let root = temp_dir("stalesub");
    let a_path = path_join(&[&root.path, "ShowA.S01E01.mkv"]);
    write_file(&a_path, "x");
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(
        &ctx,
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    let (cr_force, _, err) =
        collect_indexed_with_context(&ctx, &[root.path.clone()], &cache.path, &index, true, true);
    assert!(err.is_none());
    let (plans, _) = crate::build_plan(&cr_force.files);
    let b_path = path_join(&[&root.path, "New", "ShowB.S01E01.mkv"]);
    write_file(&b_path, "x");
    record_scan(&index, &cr_force.visited, &cr_force.skipped, &plans);
    let (cr2, _, err2) =
        collect_indexed_with_context(&ctx, &[root.path.clone()], &cache.path, &index, true, false);
    assert!(err2.is_none());
    assert!(cr2.files.contains(&a_path));
    assert!(cr2.files.contains(&b_path));
}

#[test]
fn mid_walk_add_uses_fresh_decision() {
    let rx = ResolverScope::new();
    let ctx = Cancel::background();
    let root = temp_dir("midwalk");
    let child = path_join(&[&root.path, "Child"]);
    let a_path = path_join(&[&child, "ShowA.S01E01.mkv"]);
    write_file(&a_path, "x");
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(
        &ctx,
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    let child_mtime = std::fs::metadata(&child).unwrap().modified().unwrap();
    write_file(&path_join(&[&root.path, "notes.txt"]), "x");
    let b_path = path_join(&[&child, "ShowB.S01E01.mkv"]);
    let mutated = Arc::new(AtomicBool::new(false));
    let m2 = Arc::clone(&mutated);
    let restore_ok = Arc::new(AtomicBool::new(true));
    let r2 = Arc::clone(&restore_ok);
    let child_cmp = child.clone();
    let ws = WalkScope::new();
    ws.set(Arc::new(
        move |root: &str, cb: &mut WalkCb<'_>| -> std::io::Result<()> {
            let m2 = Arc::clone(&m2);
            let restore_ok = Arc::clone(&r2);
            let child_cmp = child_cmp.clone();
            let b_path = b_path.clone();
            crate::real_walk_dir(root, &mut |p, d, e| {
                if !m2.load(Ordering::SeqCst)
                    && e.is_none()
                    && d.map(|d| d.is_dir).unwrap_or(false)
                    && normalize_scan_dir(p) == normalize_scan_dir(&child_cmp)
                {
                    m2.store(true, Ordering::SeqCst);
                    write_file(&b_path, "x");
                    if filetime_set(&child_cmp, child_mtime).is_err() {
                        restore_ok.store(false, Ordering::SeqCst);
                    }
                }
                cb(p, d, e)
            })
        },
    ));
    let st2 = scan_with_options(
        &ctx,
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    assert!(mutated.load(Ordering::SeqCst));
    if !restore_ok.load(Ordering::SeqCst) {
        // The platform cannot restore the dir mtime exactly, so the hook's
        // precondition is unverifiable here (mirrors Go's t.Skipf).
        return;
    }
    assert_eq!(st2.files, 2);
}

/// Restores a path's mtime exactly (Windows needs FILE_WRITE_ATTRIBUTES plus
/// FILE_FLAG_BACKUP_SEMANTICS for a directory handle).
fn filetime_set(path: &str, mtime: std::time::SystemTime) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let f = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        f.set_modified(mtime)?;
        let md = std::fs::metadata(path)?;
        if md.modified()? == mtime {
            Ok(())
        } else {
            Err(std::io::Error::other("mtime not restored"))
        }
    }
    #[cfg(not(windows))]
    {
        let f = std::fs::OpenOptions::new().write(true).open(path)?;
        f.set_modified(mtime)?;
        Ok(())
    }
}

/// `CollectResult`'s counters are Go `int` (64-bit): two skipped rollups of
/// `1<<30` files each sum to `1<<31` without wrapping negative.
#[test]
fn files_skipped_accumulates_past_i32_max() {
    let mut cr = crate::CollectResult::default();
    for _ in 0..2 {
        cr.files_skipped += 1_i64 << 30;
    }
    assert_eq!(cr.files_skipped, 1_i64 << 31);
    assert!(cr.files_skipped > i64::from(i32::MAX));
}
