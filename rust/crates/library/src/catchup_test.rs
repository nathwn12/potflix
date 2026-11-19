
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::catchup::{
    CATCH_UP_CONCURRENCY, CatchUpConfig, REPAIR_COOLDOWN, set_overrides_path_for_test,
    start_catch_up, stop_catch_up, write_inventory,
};
use crate::index::{
    FileRecord, Index, IndexInner, MAX_REPAIR_ATTEMPTS, STATE_NEEDS_REPAIR, STATE_PENDING,
    STATE_RESOLVED,
};
use crate::library_test::{
    FakeSource, FirstNThenBlockSource, GateSource, HonestSource, MissSource, ResolverScope,
    SlowCancelSource, SplitSpeedSource, index_scan_opts, meta_cache_dir, resolver_with, temp_dir,
};
use crate::pathutil::{join as path_join, normalize_scan_dir};
use crate::{Options, scan_with_options};
use metadata_http::cancel::Cancel;

fn catch_up_corpus(n: usize) -> (crate::library_test::TempDir, Vec<String>) {
    let root = temp_dir("catchup");
    let mut names = Vec::new();
    for i in 0..n {
        let name = format!("Show{}.S01E01.mkv", char::from(b'A' + i as u8));
        std::fs::write(path_join(&[&root.path, &name]), "x").unwrap();
        names.push(name);
    }
    (root, names)
}

#[test]
fn catch_up_idempotent_start() {
    let rx = ResolverScope::new();
    let (root, names) = catch_up_corpus(3);
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(
        &Cancel::background(),
        Options {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            use_index: true,
            skip_catch_up: true,
            ..Default::default()
        },
    )
    .unwrap();
    let cfg = CatchUpConfig {
        dirs: vec![root.path.clone()],
        cache_dir: cache.path.clone(),
        index_path: index.clone(),
        ..Default::default()
    };
    let c1 = start_catch_up(cfg.clone(), None).unwrap();
    let c2 = start_catch_up(cfg.clone(), None).unwrap();
    assert!(Arc::ptr_eq(&c1, &c2));
    c1.wait();
    c2.wait();
    assert_eq!(fake.calls.load(Ordering::SeqCst), names.len() as i32);
    let first = c1.stats();
    assert_eq!(first.passes, 1);
    assert_eq!(first.lookups, names.len() as i64);
    let c3 = start_catch_up(cfg.clone(), None).unwrap();
    c3.wait();
    let settled = c3.stats();
    assert_eq!(settled.lookups, first.lookups);
    assert_eq!(settled.missed, first.missed);
    assert_eq!(settled.failed, first.failed);
    assert_eq!(fake.calls.load(Ordering::SeqCst), names.len() as i32);
    let state = crate::scanindex::load_scan_index(&index).ix.state();
    assert_eq!(state.keys_resolved, state.keys);
}

#[test]
fn catch_up_needs_repair_after_attempts() {
    let rx = ResolverScope::new();
    let (root, _names) = catch_up_corpus(1);
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let ctx = Cancel::background();
    let miss = Arc::new(MissSource::new());
    let m = Arc::clone(&miss);
    rx.set(move |dir| resolver_with(m.clone(), dir));
    let path = path_join(&[&root.path, "ShowA.S01E01.mkv"]);
    scan_with_options(
        &ctx,
        Options {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            use_index: true,
            skip_catch_up: true,
            ..Default::default()
        },
    )
    .unwrap();
    let cfg = CatchUpConfig {
        dirs: vec![root.path.clone()],
        cache_dir: cache.path.clone(),
        index_path: index.clone(),
        ..Default::default()
    };
    for i in 1..=MAX_REPAIR_ATTEMPTS {
        start_catch_up(cfg.clone(), None).unwrap().wait();
        let rec = crate::scanindex::load_scan_index(&index).ix.file(&path);
        let rec = rec.unwrap_or_else(|| panic!("record vanished"));
        assert_eq!(rec.attempts, i, "pass {i} attempts");
        if i < MAX_REPAIR_ATTEMPTS {
            assert_ne!(rec.state, STATE_NEEDS_REPAIR, "parked early");
        }
        if i == MAX_REPAIR_ATTEMPTS {
            assert_eq!(rec.state, STATE_NEEDS_REPAIR);
            assert!(!rec.last_error.is_empty());
            assert!(rec.next_try > crate::index::now_unix_nanos());
        }
    }
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    start_catch_up(cfg.clone(), None).unwrap().wait();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    let rec = crate::scanindex::load_scan_index(&index)
        .ix
        .file(&path)
        .unwrap();
    assert_eq!(rec.state, STATE_NEEDS_REPAIR);
    assert_eq!(rec.attempts, MAX_REPAIR_ATTEMPTS);
    let _ = REPAIR_COOLDOWN;

    let ix = Arc::clone(&crate::scanindex::load_scan_index(&index).ix);
    let mut rec = ix.file(&path).unwrap();
    rec.next_try = crate::index::now_unix_nanos() - 60_000_000_000;
    ix.put_file(rec);
    ix.commit().unwrap();
    let forced = CatchUpConfig {
        force: true,
        ..cfg.clone()
    };
    start_catch_up(forced, None).unwrap().wait();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    let after = crate::scanindex::load_scan_index(&index)
        .ix
        .file(&path)
        .unwrap();
    assert_eq!(after.state, STATE_RESOLVED);
    assert_eq!(after.attempts, 0);
}

#[test]
fn catch_up_writes_inventory_report() {
    let rx = ResolverScope::new();
    let (root, _names) = catch_up_corpus(1);
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let ctx = Cancel::background();
    rx.set(|dir| resolver_with(Arc::new(HonestSource), dir));
    std::fs::rename(
        path_join(&[&root.path, "ShowA.S01E01.mkv"]),
        path_join(&[&root.path, "Missing.S01E01.mkv"]),
    )
    .unwrap();
    scan_with_options(
        &ctx,
        Options {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            use_index: true,
            skip_catch_up: true,
            ..Default::default()
        },
    )
    .unwrap();
    let cfg = CatchUpConfig {
        dirs: vec![root.path.clone()],
        cache_dir: cache.path.clone(),
        index_path: index.clone(),
        app_version: "test".into(),
        ..Default::default()
    };
    for _ in 0..MAX_REPAIR_ATTEMPTS {
        start_catch_up(cfg.clone(), None).unwrap().wait();
    }
    let report_path = path_join(&[
        &crate::pathutil::dir(&index),
        "inventory",
        "needs-repair.json",
    ]);
    let r = repair::report::read_report(&report_path).unwrap();
    assert_eq!(r.app_version, "test");
    assert_eq!((r.totals.files, r.totals.indexed), (1, 1));
    assert_eq!(r.entries.len(), 1);
    let e = &r.entries[0];
    assert_eq!(e.path, path_join(&[&root.path, "Missing.S01E01.mkv"]));
    assert_eq!(e.attempts, MAX_REPAIR_ATTEMPTS);
    assert!(!e.last_error.is_empty());
    assert_eq!(e.reason, repair::report::REASON_NO_METADATA);
    write_inventory(&index, "test").unwrap();
}

/// Parser-classified extras are not metadata gaps: a needs-repair row under an
/// extras leaf must not be looked up and must be counted under `extras`, never
/// listed in the entries. The mixed fixture holds one normal record that IS
/// looked up (the lookup count is exactly 1) and one extras row whose cooldown
/// is already expired, so only the extras rule keeps it out of the pass.
#[test]
fn catch_up_skips_extras_and_partitions_the_report() {
    let rx = ResolverScope::new();
    let root = temp_dir("extrascs");
    let normal = path_join(&[&root.path, "Missing.S01E01.mkv"]);
    let extras = path_join(&[&root.path, "Featurettes", "Deleted Scenes", "scenes1.mkv"]);
    std::fs::write(&normal, "x").unwrap();
    std::fs::create_dir_all(std::path::Path::new(&extras).parent().unwrap()).unwrap();
    std::fs::write(&extras, "x").unwrap();
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);

    let miss = Arc::new(MissSource::new());
    let m = Arc::clone(&miss);
    rx.set(move |dir| resolver_with(m.clone(), dir));
    scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();

    let ix = Arc::clone(&crate::scanindex::load_scan_index(&index).ix);
    ix.put_file(FileRecord {
        path: extras.clone(),
        name: "scenes1.mkv".into(),
        kind: crate::index::FILE_KIND_UNKNOWN.into(),
        title: "Scenes".into(),
        meta_key: "series|scenes|0|1".into(),
        state: STATE_NEEDS_REPAIR.into(),
        attempts: MAX_REPAIR_ATTEMPTS,
        last_error: "no metadata found".into(),
        next_try: crate::index::now_unix_nanos() - 60_000_000_000,
        ..FileRecord::default()
    });
    ix.commit().unwrap();

    let c = start_catch_up(
        CatchUpConfig {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            app_version: "test".into(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    c.wait();

    assert_eq!(miss.calls.load(Ordering::SeqCst), 1, "lookups");
    assert_eq!(c.stats().lookups, 1);

    let report_path = path_join(&[
        &crate::pathutil::dir(&index),
        "inventory",
        "needs-repair.json",
    ]);
    let r = repair::report::read_report(&report_path).unwrap();
    assert!(
        r.entries.iter().all(|e| !e.path.contains("Featurettes")),
        "extras row listed in the entries: {:?}",
        r.entries
    );
    assert!(r.entries.is_empty(), "entries: {:?}", r.entries);
    assert_eq!(
        (
            r.totals.resolved,
            r.totals.needs_repair,
            r.totals.extras,
            r.totals.pinned,
            r.totals.pending
        ),
        (0, 0, 1, 0, 1),
        "extras were not counted separately: {:?}",
        r.totals
    );
    assert_eq!(r.totals.files, r.totals.indexed, "files != indexed");
    assert_eq!(
        r.totals.indexed,
        r.totals.resolved
            + r.totals.needs_repair
            + r.totals.extras
            + r.totals.pinned
            + r.totals.pending,
        "the partition does not hold: {:?}",
        r.totals
    );
    write_inventory(&index, "test").unwrap();
}

/// Restores the previous override-store fixture path on drop. The module's
/// shared test lock is held for each test's whole span, so a fixture never
/// leaks into a concurrent test.
struct OverridesFixture(Option<PathBuf>);

impl OverridesFixture {
    fn install(path: PathBuf) -> OverridesFixture {
        OverridesFixture(set_overrides_path_for_test(Some(path)))
    }
}

impl Drop for OverridesFixture {
    fn drop(&mut self) {
        set_overrides_path_for_test(self.0.take());
    }
}

/// The manual latch wins before any metadata work: a record parked as
/// needs-repair is resolved from the override store itself -- zero lookups, no
/// attempt, no error -- and the regenerated report counts it as `pinned`, never
/// as needs-repair. The positive control proves the parked row really is looked
/// up when no latch exists.
#[test]
fn catch_up_resolves_a_latched_record_without_any_lookup() {
    let rx = ResolverScope::new();
    let (root, _names) = catch_up_corpus(1);
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let miss = Arc::new(MissSource::new());
    let m = Arc::clone(&miss);
    rx.set(move |dir| resolver_with(m.clone(), dir));
    let path = path_join(&[&root.path, "ShowA.S01E01.mkv"]);
    scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    let cfg = CatchUpConfig {
        dirs: vec![root.path.clone()],
        cache_dir: cache.path.clone(),
        index_path: index.clone(),
        app_version: "test".into(),
        ..Default::default()
    };

    let ix = Arc::clone(&crate::scanindex::load_scan_index(&index).ix);
    let mut rec = ix.file(&path).unwrap();
    rec.state = STATE_NEEDS_REPAIR.into();
    rec.attempts = MAX_REPAIR_ATTEMPTS;
    rec.last_error = "no metadata found".into();
    rec.next_try = crate::index::now_unix_nanos() - 60_000_000_000;
    ix.put_file(rec);
    ix.commit().unwrap();

    start_catch_up(cfg.clone(), None).unwrap().wait();
    assert_eq!(
        miss.calls.load(Ordering::SeqCst),
        1,
        "positive control: the parked, expired row must be looked up"
    );

    let store_dir = temp_dir("latchcs");
    let store_path = PathBuf::from(&store_dir.path).join("overrides.json");
    let mut store = metadata::overrides::Store::empty_at(store_path.clone());
    store
        .put(metadata::overrides::Override {
            scope: metadata::overrides::Scope::Series,
            key: root.path.clone(),
            title: "Show A".into(),
            year: 2020,
            provider: "tvmaze".into(),
            provider_id: "1".into(),
            poster: String::new(),
            media: None,
            episodes: Vec::new(),
            created_at_ms: 1,
        })
        .expect("latch the show folder");
    let _fixture = OverridesFixture::install(store_path);

    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    start_catch_up(cfg.clone(), None).unwrap().wait();

    assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "latched lookup");
    let after = crate::scanindex::load_scan_index(&index)
        .ix
        .file(&path)
        .unwrap();
    assert_eq!(after.state, STATE_RESOLVED);
    assert_eq!(after.attempts, 0);
    assert!(
        after.last_error.is_empty(),
        "last_error: {:?}",
        after.last_error
    );
    assert_eq!(after.next_try, 0);

    let report_path = path_join(&[
        &crate::pathutil::dir(&index),
        "inventory",
        "needs-repair.json",
    ]);
    let r = repair::report::read_report(&report_path).unwrap();
    assert_eq!(r.totals.pinned, 1, "pinned count: {:?}", r.totals);
    assert_eq!(r.totals.needs_repair, 0, "needs-repair count: {:?}", r.totals);
    assert!(r.entries.is_empty(), "entries: {:?}", r.entries);
    assert_eq!(r.totals.files, r.totals.indexed, "files != indexed");
    assert_eq!(
        r.totals.indexed,
        r.totals.resolved
            + r.totals.needs_repair
            + r.totals.extras
            + r.totals.pinned
            + r.totals.pending,
        "the partition does not hold: {:?}",
        r.totals
    );
}

#[test]
fn catch_up_cancel_stops_pass() {
    let rx = ResolverScope::new();
    let root = temp_dir("cancelcs");
    for n in ["ShowA.S01E01.mkv", "ShowB.S01E01.mkv", "ShowC.S01E01.mkv"] {
        std::fs::write(path_join(&[&root.path, n]), "x").unwrap();
    }
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let split = Arc::new(SplitSpeedSource {
        release: Arc::new(AtomicBool::new(false)),
    });
    let s = Arc::clone(&split);
    rx.set(move |dir| resolver_with(s.clone(), dir));
    scan_with_options(
        &Cancel::background(),
        Options {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            use_index: true,
            skip_catch_up: true,
            ..Default::default()
        },
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
    while crate::scanindex::load_scan_index(&index)
        .ix
        .state()
        .keys_resolved
        == 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let idx = index.clone();
    let handle = std::thread::spawn(move || {
        stop_catch_up(&idx);
        let _ = tx.send(());
    });
    if rx.recv_timeout(Duration::from_secs(5)).is_err() {
        split.release.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        panic!("StopCatchUp did not return within 5s");
    }
    handle.join().unwrap();
    split.release.store(true, Ordering::SeqCst);
    c.wait();
    let stats = c.stats();
    assert_eq!((stats.failed, stats.missed), (0, 0));
    let state = crate::scanindex::load_scan_index(&index).ix.state();
    assert!(state.keys_resolved > 0);
    assert_eq!(state.files_needs_repair, 0);
    for rec in crate::scanindex::load_scan_index(&index)
        .ix
        .files_snapshot()
    {
        if rec.meta_key.is_empty() {
            continue;
        }
        assert_ne!(rec.state, STATE_NEEDS_REPAIR);
        assert!(!(rec.attempts > 0 && rec.state == STATE_PENDING));
    }
}

#[test]
fn catch_up_cancels_with_scan_context() {
    let rx = ResolverScope::new();
    let (root, _names) = catch_up_corpus(2);
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let slow = Arc::new(SlowCancelSource::new());
    let s = Arc::clone(&slow);
    rx.set(move |dir| resolver_with(s.clone(), dir));
    let ctx = Cancel::background();
    scan_with_options(
        &ctx,
        Options {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            use_index: true,
            skip_catch_up: true,
            ..Default::default()
        },
    )
    .unwrap();
    let c = start_catch_up(
        CatchUpConfig {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            ctx: Some(ctx.clone()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !slow.started.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ctx.cancel();
    let done = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&done);
    let c2 = Arc::clone(&c);
    let handle = std::thread::spawn(move || {
        c2.wait();
        d2.store(true, Ordering::SeqCst);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !done.load(Ordering::SeqCst) {
        slow.release.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        panic!("the scan's cancel did not stop the catch-up");
    }
    handle.join().unwrap();
    assert_eq!(c.stats().failed, 0);
}

#[test]
fn apply_verdict_transient_failure_is_not_a_resolution() {
    let _rx = ResolverScope::new();
    let rec = FileRecord {
        path: "D:\\lib\\Show.S01E01.mkv".into(),
        meta_key: "series|show|0|1".into(),
        state: STATE_PENDING.into(),
        ..FileRecord::default()
    };
    let key = normalize_scan_dir(&rec.path);
    let mut files: HashMap<String, FileRecord> = HashMap::new();
    files.insert(key.clone(), rec.clone());
    let ix = Index {
        path: String::new(),
        legacy: String::new(),
        inner: Mutex::new(IndexInner {
            files,
            ..IndexInner::default()
        }),
        open_err: Mutex::new(None),
    };
    let mut by_path: HashMap<String, String> = HashMap::new();
    by_path.insert(key.clone(), rec.meta_key.clone());
    let (applied, _repair, _last) = ix.apply_verdict(
        &rec.meta_key,
        &by_path,
        &metadata::Resolution::default(),
        Some(&metadata::Error::other("boom")),
        crate::index::now_unix_nanos(),
    );
    assert_eq!(applied, 1);
    let got = ix.file(&rec.path).unwrap();
    assert_eq!(got.last_error, "boom");
    assert_eq!(got.attempts, 1);
    assert_eq!(got.state, STATE_PENDING);
}

#[test]
fn scan_handoff_resolves_after_the_scan_context_ends() {
    let rx = ResolverScope::new();
    let (root, names) = catch_up_corpus(3);
    let cache = meta_cache_dir();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let gate = Arc::new(GateSource {
        started: Arc::new(AtomicBool::new(false)),
        proceed: Arc::new(AtomicBool::new(false)),
    });
    let g = Arc::clone(&gate);
    rx.set(move |dir| resolver_with(g.clone(), dir));
    let ctx = Cancel::background();
    let st = scan_with_options(
        &ctx,
        Options {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            use_index: true,
            app_version: "test".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(st.keys, names.len() as i64);
    assert_eq!(st.keys_done, 0);

    let deadline = Instant::now() + Duration::from_secs(10);
    while !gate.started.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    ctx.cancel();
    gate.proceed.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::scanindex::load_scan_index(&index)
        .ix
        .state()
        .keys_resolved
        < names.len() as i64
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let state = crate::scanindex::load_scan_index(&index).ix.state();
    stop_catch_up(&index);
    assert_eq!(state.keys_resolved, names.len() as i64);
    let cache_idx = format!("{}.idx", cache.path);
    let fi = std::fs::metadata(&cache_idx).expect("metadata cache store missing");
    assert!(fi.len() > 0);
}

#[test]
fn catch_up_inventory_report_needs_an_enumerated_index() {
    let rx = ResolverScope::new();
    let idxdir = crate::library_test::temp_dir("idxs");
    let index = crate::library_test::index_path_in(&idxdir);
    let cache = meta_cache_dir();
    let report_path = path_join(&[
        &crate::pathutil::dir(&index),
        "inventory",
        "needs-repair.json",
    ]);
    start_catch_up(
        CatchUpConfig {
            index_path: index.clone(),
            cache_dir: cache.path.clone(),
            app_version: "test".into(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    assert!(std::fs::metadata(&report_path).is_err());

    let (root, names) = catch_up_corpus(3);
    let fake = Arc::new(FakeSource::new("V1"));
    let f = Arc::clone(&fake);
    rx.set(move |dir| resolver_with(f.clone(), dir));
    scan_with_options(
        &Cancel::background(),
        index_scan_opts(&[root.path.clone()], &cache.path, &index, false),
    )
    .unwrap();
    start_catch_up(
        CatchUpConfig {
            dirs: vec![root.path.clone()],
            cache_dir: cache.path.clone(),
            index_path: index.clone(),
            app_version: "test".into(),
            ..Default::default()
        },
        None,
    )
    .unwrap()
    .wait();
    let r = repair::report::read_report(&report_path).unwrap();
    assert_eq!(
        (r.totals.files, r.totals.indexed),
        (names.len() as i64, names.len() as i64)
    );
    let _ = CATCH_UP_CONCURRENCY;
    let _ = FirstNThenBlockSource {
        n: 1,
        calls: Arc::new(AtomicI32::new(0)),
        release: Arc::new(AtomicBool::new(false)),
    };
}

/// The cumulative catch-up tallies are Go `int` (64-bit): two passes of
/// `1<<30` lookups sum to `1<<31` without wrapping negative or panicking.
#[test]
fn catchup_stats_accumulate_past_i32_max() {
    let mut st = crate::catchup::CatchUpStats::default();
    for _ in 0..2 {
        st.lookups += 1_i64 << 30;
    }
    assert_eq!(st.lookups, 1_i64 << 31);
    assert!(st.lookups > i64::from(i32::MAX));
}
