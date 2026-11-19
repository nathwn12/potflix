
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;

use crate::index::{
    close_index, decode_file_record, decode_folder_record, encode_file_record,
    encode_folder_record, index_store_path, shared_index, FileRecord, LEGACY_BAK_SUFFIX,
    STATE_RESOLVED,
};
use crate::library_test::{
    index_path_in, index_scan_opts, meta_cache_dir, seed_index_corpus, temp_dir, ParseScope,
    ResolverScope,
};
use crate::scanindex::{load_scan_index, same_scan_record, FolderRecord};
use crate::{scan_with_options, Options};
use metadata_http::cancel::Cancel;

static PARSE_CALLS: AtomicI32 = AtomicI32::new(0);

fn counting_parse(raw: &str, path: &str) -> metadata::Query {
    PARSE_CALLS.fetch_add(1, Ordering::SeqCst);
    metadata_core::context::resolve_query(raw, path)
}

fn real_index_source() -> String {
    match std::env::var("LOCALAPPDATA") {
        Ok(d) if !d.is_empty() => crate::pathutil::join(&[&d, "Potflix", "scanindex.json"]),
        _ => String::new(),
    }
}

#[test]
fn migration_parity_real_index() {
    let _rx = ResolverScope::new();
    let src = real_index_source();
    if src.is_empty() {
        return;
    }
    let data = match std::fs::read(&src) {
        Ok(d) => d,
        Err(_) => return,
    };
    let dir = temp_dir("realidx");
    let copy = crate::pathutil::join(&[&dir.path, "scanindex.json"]);
    std::fs::write(&copy, &data).unwrap();

    let counts = crate::scanindex::legacy_scan_index_counts(&copy).unwrap();
    assert_eq!(
        (counts.folders, counts.files),
        (928, 23402),
        "legacy parity"
    );
    assert_eq!((counts.keys, counts.key_refs), (2310, 7218));
    let _ = Instant::now();
    let ix = load_scan_index(&copy);
    assert_eq!(ix.len(), counts.folders);
    assert!(std::fs::metadata(format!("{copy}{LEGACY_BAK_SUFFIX}")).is_ok());
    assert!(std::fs::metadata(&copy).is_err());
    for d in ix.ix.folder_names() {
        let rec = ix.lookup(&d).unwrap();
        assert!(!rec.per_file);
    }
    close_index(&copy).unwrap();
    let reopened = load_scan_index(&copy);
    assert_eq!(reopened.len(), counts.folders);
    assert!(std::fs::metadata(&src).is_ok());
}

#[test]
fn migration_imports_zero_and_renames() {
    let _rx = ResolverScope::new();
    let dir = temp_dir("badlegacy");
    let path = crate::pathutil::join(&[&dir.path, "scanindex.json"]);
    std::fs::write(&path, "{{not json").unwrap();
    let ix = load_scan_index(&path);
    assert_eq!(ix.len(), 0);
    assert!(std::fs::metadata(format!("{path}{LEGACY_BAK_SUFFIX}")).is_ok());
}

#[test]
fn unchanged_file_fast_path() {
    let rx = ResolverScope::new();
    let (dirs, parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = std::sync::Arc::new(crate::library_test::FakeSource::new("V1"));
    let f = std::sync::Arc::clone(&fake);
    rx.set(move |dir| crate::library_test::resolver_with(f.clone(), dir));

    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    let ps = ParseScope::new();
    PARSE_CALLS.store(0, Ordering::SeqCst);
    ps.set(counting_parse);

    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(3);
    let mut all = vec![parent.path.clone()];
    all.extend(dirs.iter().cloned());
    for d in &all {
        set_dir_mtime(d, later);
    }
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!(
        PARSE_CALLS.load(Ordering::SeqCst),
        0,
        "scan 2 re-parsed files"
    );
    assert_eq!(st2.parsed, 0);
    assert_eq!(st2.folders_skipped, 0);
    assert!(st2.files > 0);
    assert_eq!(st2.unchanged, st2.files);
}

/// Forces a directory's mtime forward (Windows: create a probe file then drop
/// it, which bumps the parent mtime).
fn set_dir_mtime(dir: &str, _t: std::time::SystemTime) {
    let probe = crate::pathutil::join(&[dir, ".probe-mtime"]);
    let _ = std::fs::write(&probe, "x");
    let _ = std::fs::remove_file(&probe);
}

#[test]
fn prune_deleted_file_disappears_from_store() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = std::sync::Arc::new(crate::library_test::FakeSource::new("V1"));
    let f = std::sync::Arc::clone(&fake);
    rx.set(move |dir| crate::library_test::resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    let gone = crate::pathutil::join(&[&dirs[0], "ShowA.S01E02.mkv"]);
    assert!(load_scan_index(&index).ix.file(&gone).is_some());
    std::fs::remove_file(&gone).unwrap();
    let st2 = scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    assert_eq!(st2.pruned, 1);
    assert!(load_scan_index(&index).ix.file(&gone).is_none());
    close_index(&index).unwrap();
    assert!(load_scan_index(&index).ix.file(&gone).is_none());
}

#[test]
fn torn_tail_keeps_index_readable() {
    let rx = ResolverScope::new();
    let (dirs, _parent) = seed_index_corpus();
    let cache = meta_cache_dir();
    let idxdir = temp_dir("idxs");
    let index = index_path_in(&idxdir);
    let ctx = Cancel::background();
    let fake = std::sync::Arc::new(crate::library_test::FakeSource::new("V1"));
    let f = std::sync::Arc::clone(&fake);
    rx.set(move |dir| crate::library_test::resolver_with(f.clone(), dir));
    scan_with_options(&ctx, index_scan_opts(&dirs, &cache.path, &index, false)).unwrap();
    let before = shared_index(&index).unwrap().state();
    assert!(before.files > 0 && before.keys > 0);
    let sp = index_store_path(&index);
    close_index(&index).unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&sp).unwrap();
        f.write_all(&[1u8, 60, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef, b'h', b'a', b'l'])
            .unwrap();
    }
    let torn = std::fs::metadata(&sp).unwrap().len();
    let after = shared_index(&index).unwrap().state();
    assert_eq!((after.files, after.keys), (before.files, before.keys));
    let rec = load_scan_index(&index).lookup(&dirs[0]).unwrap();
    assert!(rec.files > 0);
    let trimmed = std::fs::metadata(&sp).unwrap().len();
    assert!(trimmed < torn);
}

#[test]
fn file_record_query_round_trip() {
    let _rx = ResolverScope::new();
    for name in [
        "ShowA.S01E01.mkv",
        "ShowA.S01E02.mkv",
        "Movie.Title.1999.mkv",
        "[GRP] Anime Show 12 [1080p].mkv",
        "ShowCtrailer.mkv",
    ] {
        let p = crate::pathutil::join(&["D:\\lib", name]);
        let q = metadata_core::context::resolve_query(name, &p);
        let mut rec = FileRecord {
            path: p.clone(),
            name: name.to_string(),
            kind: crate::scan::record_kind(q.kind).to_string(),
            title: q.title.clone(),
            season: q.season,
            episode: q.episode as i64,
            year: q.year as i64,
            absolute: q.absolute,
            ..FileRecord::default()
        };
        if q.kind != metadata::Kind::UNKNOWN {
            rec.meta_key = metadata::cache::cache_key_for(&q);
            assert_eq!(
                metadata::cache::cache_key_for(&rec.query()),
                rec.meta_key,
                "{name}"
            );
        }
    }
}

/// The stored columns of a folder-context episode are a filename-only parse;
/// the rebuild must recover show/season from the stored path exactly as the live
/// path does (`resolve_query`), or an index written by an older parser never
/// repairs.
#[test]
fn file_record_query_rebuilds_folder_context() {
    let _rx = ResolverScope::new();
    let rec = FileRecord {
        path: crate::pathutil::join(&["D:\\lib", "Zentaro", "Season 4", "01.mkv"]),
        name: "01.mkv".into(),
        kind: crate::index::FILE_KIND_UNKNOWN.into(),
        ..FileRecord::default()
    };
    let q = rec.query();
    assert_eq!(q.kind, metadata::Kind::SERIES, "kind");
    assert_eq!(q.title, "Zentaro", "folder show");
    assert_eq!(q.season, 4, "folder season");
    assert_eq!(q.episode, 1, "bare episode number");
    assert!(!q.absolute, "a season-folder query is not absolute");
}

/// An unusable (empty or blank) stored path still reads the stored columns: the
/// rebuild never invents folder context it does not have.
#[test]
fn file_record_query_empty_path_falls_back_to_stored_columns() {
    let _rx = ResolverScope::new();
    for path in ["", "   "] {
        let rec = FileRecord {
            path: path.to_string(),
            name: "Show.S02E03.mkv".into(),
            kind: crate::index::FILE_KIND_SERIES.into(),
            title: "Stored Title".into(),
            season: 2,
            episode: 3,
            year: 1999,
            absolute: true,
            ..FileRecord::default()
        };
        let q = rec.query();
        assert_eq!(q.kind, metadata::Kind::SERIES, "path={path:?}");
        assert_eq!(q.title, "Stored Title", "path={path:?}");
        assert_eq!(q.season, 2, "path={path:?}");
        assert_eq!(q.episode, 3, "path={path:?}");
        assert_eq!(q.year, 1999, "path={path:?}");
        assert!(q.absolute, "path={path:?}");
    }
}

#[test]
fn file_record_encoding_budget() {
    let _rx = ResolverScope::new();
    let rec = FileRecord {
        path: "D:\\Media\\Series\\RealShapeShow123\\Season 02\\RealShapeShow123.S02E07.1080p.WEB-DL.x264-GRP.mkv".into(),
        name: "RealShapeShow123.S02E07.1080p.WEB-DL.x264-GRP.mkv".into(),
        ext: ".mkv".into(),
        size: 1 << 30,
        mtime: 1_750_000_000_000_000_000,
        kind: crate::index::FILE_KIND_SERIES.into(),
        title: "RealShapeShow123".into(),
        season: 2,
        episode: 7,
        meta_key: "series|realshapeshow123|0|2".into(),
        state: STATE_RESOLVED.into(),
        ..FileRecord::default()
    };
    let mut enc = Vec::new();
    encode_file_record(&mut enc, &rec);
    let js = serde_json::to_vec(&rec).unwrap();
    assert!(
        enc.len() < js.len(),
        "binary {} vs json {}",
        enc.len(),
        js.len()
    );
    assert!(enc.len() * 23402 <= 6 << 20);
    let got = decode_file_record(&enc).unwrap();
    assert_eq!(got, rec);
    assert!(decode_file_record(&enc[..enc.len() - 1]).is_none());
    let mut trailing = enc.clone();
    trailing.push(0);
    assert!(decode_file_record(&trailing).is_none());
}

#[test]
fn folder_record_encoding_round_trip() {
    let _rx = ResolverScope::new();
    let rec = FolderRecord {
        fingerprint: "fp".into(),
        names_hash: "nh".into(),
        files: 3,
        schema: 3,
        keys: vec!["series|a|0|1".into(), "movie|b|1999".into()],
        scanned_at_nanos: 1_700_000_000 * 1_000_000_000,
        per_file: true,
    };
    let mut enc = Vec::new();
    encode_folder_record(&mut enc, &rec);
    let got = decode_folder_record(&enc).unwrap();
    assert!(same_scan_record(&rec, &got));
    assert_eq!(got.per_file, rec.per_file);
    assert_eq!(got.scanned_at_nanos, rec.scanned_at_nanos);
}

#[test]
fn index_store_load_time() {
    let _rx = ResolverScope::new();
    const FILES: i32 = 23402;
    const KEYS: i32 = 2310;
    let dir = temp_dir("load");
    let index = index_path_in(&dir);
    let ix = shared_index(&index).unwrap();
    for i in 0..FILES {
        let k = i % KEYS;
        let title = format!("RealShapeShow{k:04}");
        let base = format!(
            "{title}.S{:02}E{:02}.1080p.WEB-DL.x264-GRP{i:05}.mkv",
            k % 5 + 1,
            i % 24 + 1
        );
        let d = crate::pathutil::join(&[
            "D:\\Media\\Series",
            &format!("Show{k:04}"),
            &format!("Season {:02}", k % 5 + 1),
        ]);
        ix.put_file(FileRecord {
            path: crate::pathutil::join(&[&d, &base]),
            name: base,
            ext: ".mkv".into(),
            size: i as i64 * 1024 + 1,
            mtime: i as i64 * 1_000_000,
            kind: crate::index::FILE_KIND_SERIES.into(),
            title,
            season: (k % 5 + 1) as i64,
            episode: (i % 24 + 1) as i64,
            meta_key: format!("series|realshapeshow{k:04}|0|{}", k % 5 + 1),
            state: crate::index::STATE_PENDING.into(),
            ..FileRecord::default()
        });
    }
    ix.commit().unwrap();
    close_index(&index).unwrap();
    let reopened = shared_index(&index).unwrap();
    let state = reopened.state();
    assert_eq!(state.files, i64::from(FILES));
    assert_eq!(state.keys, i64::from(KEYS));
    let _ = Options::default();
}

/// The store record's `files` is Go `int` (64-bit): a value above `i32::MAX`
/// must encode and decode unchanged, not negative, and never clamp to zero.
#[test]
fn folder_record_files_above_i32_max_round_trips() {
    let big = 1_i64 << 31; // 2147483648
    let rec = FolderRecord {
        fingerprint: "fp".to_string(),
        names_hash: "nh".to_string(),
        files: big,
        schema: 7,
        keys: vec!["k".to_string()],
        scanned_at_nanos: 42,
        per_file: true,
    };
    let mut buf = Vec::new();
    encode_folder_record(&mut buf, &rec);
    let got = decode_folder_record(&buf).expect("decode");
    assert_eq!(got.files, big, "files decoded negative or clamped");
    assert_eq!(got, rec);
}

/// End-to-end store round-trip: write `files = 1<<31`, reopen, read it back.
#[test]
fn store_round_trip_files_above_i32_max() {
    let dir = temp_dir("bigfiles");
    let big = 1_i64 << 31;
    let dir_key = crate::pathutil::join(&[&dir.path, "lib"]);
    let path = crate::pathutil::join(&[&dir.path, "scanindex.json"]);
    let ix = load_scan_index(&path);
    ix.record_full(&dir_key, "nh", "fp", 7, &["k".to_string()], big);
    ix.save(&path).unwrap();
    close_index(&path).unwrap();
    let reopened = load_scan_index(&path);
    let rec = reopened.lookup(&dir_key).expect("record after reopen");
    assert_eq!(rec.files, big, "store round-trip clamped files");
}

/// The store record's `season`/`episode`/`year` are Go `int` (64-bit,
/// `go\internal\library\index.go:127-129`): a value above `i32::MAX` must
/// encode and decode unchanged, not negative and not clamped to zero.
#[test]
fn file_record_season_above_i32_max_round_trips() {
    let big = 1_i64 << 31; // 2147483648
    let rec = FileRecord {
        path: "D:\\lib\\Show.S01E01.mkv".into(),
        name: "Show.S01E01.mkv".into(),
        ext: ".mkv".into(),
        kind: crate::index::FILE_KIND_SERIES.into(),
        title: "Show".into(),
        season: big,
        episode: big + 1,
        year: big + 2,
        meta_key: "k".into(),
        state: STATE_RESOLVED.into(),
        ..FileRecord::default()
    };
    let mut buf = Vec::new();
    encode_file_record(&mut buf, &rec);
    let got = decode_file_record(&buf).expect("decode");
    assert_eq!(got.season, big, "season decoded negative or clamped");
    assert_eq!(got.episode, big + 1, "episode decoded negative or clamped");
    assert_eq!(got.year, big + 2, "year decoded negative or clamped");
    assert_eq!(got, rec);
}

/// End-to-end store round-trip: write `season = 1<<31`, reopen, read it back.
#[test]
fn store_round_trip_season_above_i32_max() {
    let dir = temp_dir("bigseason");
    let big = 1_i64 << 31;
    let index = index_path_in(&dir);
    let ix = shared_index(&index).unwrap();
    let path = crate::pathutil::join(&["D:\\lib", "Show.S01E01.mkv"]);
    ix.put_file(FileRecord {
        path: path.clone(),
        name: "Show.S01E01.mkv".into(),
        kind: crate::index::FILE_KIND_SERIES.into(),
        title: "Show".into(),
        season: big,
        meta_key: "series|show|0|2147483648".into(),
        state: STATE_RESOLVED.into(),
        ..FileRecord::default()
    });
    ix.commit().unwrap();
    close_index(&index).unwrap();
    let reopened = shared_index(&index).unwrap();
    let rec = reopened.file(&path).expect("record after reopen");
    assert_eq!(rec.season, big, "store round-trip clamped season");
}

/// Extra, non-committed signal: the REAL Go-written multi-MB store at
/// `%LOCALAPPDATA%\Potflix\library.idx`, the same file `meta_idx_probe`
/// (`crates\store\src\tests.rs`) probes for `meta.idx`. Read-only -- the path is
/// derived from `migration_parity_real_index`'s resolution, `store::inspect`
/// never writes or truncates, and every record payload is decoded into the
/// library's own record types. `cargo test -p library --release -- --ignored
/// --nocapture library_idx_probe`.
#[test]
#[ignore = "reads the owner's real cache read-only; run explicitly with --ignored"]
fn library_idx_probe() {
    let src = real_index_source();
    if src.is_empty() {
        eprintln!("library_idx_probe: LOCALAPPDATA unset; skipping");
        return;
    }
    let path = index_store_path(&src);
    if std::fs::metadata(&path).is_err() {
        eprintln!("library_idx_probe: {path} absent; skipping");
        return;
    }
    let insp = store::inspect(&path).expect("the real library.idx header validates");
    let mut files = 0usize;
    let mut folders = 0usize;
    let mut titled = 0usize;
    for (k, v) in &insp.records {
        if k.starts_with(b"f|") {
            let rec = decode_file_record(v).expect("a real library file record decodes");
            files += 1;
            if !rec.title.is_empty() {
                titled += 1;
            }
        } else if k.starts_with(b"d|") {
            decode_folder_record(v).expect("a real library folder record decodes");
            folders += 1;
        }
    }
    assert!(files + folders > 0, "the real store must have records");
    assert_eq!(
        insp.valid, insp.size,
        "torn tail: last valid boundary != file size"
    );
    eprintln!(
        "library.idx: {} bytes, {} records ({} files, {} folders), last valid boundary {}, {} files with a non-empty title",
        insp.size,
        insp.records.len(),
        files,
        folders,
        insp.valid,
        titled
    );
}
