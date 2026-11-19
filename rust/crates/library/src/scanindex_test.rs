
use std::sync::Arc;
use std::time::Duration;

use crate::library_test::{ResolverScope, temp_dir};
use crate::pathutil::join as path_join;
use crate::scanindex::{folder_fingerprint, folder_names_hash, load_scan_index};

fn write_scan_file(path: &str, content: &str) {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn fingerprint_of(dir: &str) -> String {
    folder_fingerprint(dir).unwrap()
}

fn names_hash_of(dir: &str) -> String {
    folder_names_hash(dir).unwrap()
}

fn all_present(_: &str) -> bool {
    true
}

fn try_set_mtime(path: &str, t: std::time::SystemTime) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(t);
    }
}

#[test]
fn fingerprint_stable() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("fp").path, "stable"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    assert_eq!(fingerprint_of(&dir), fingerprint_of(&dir));
}

#[test]
fn fingerprint_add_file() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("fp").path, "add"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    let before = fingerprint_of(&dir);
    write_scan_file(&path_join(&[&dir, "b.mkv"]), "b");
    assert_ne!(fingerprint_of(&dir), before);
}

#[test]
fn fingerprint_rename() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("fp").path, "rename"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    let before = fingerprint_of(&dir);
    std::fs::rename(path_join(&[&dir, "a.mkv"]), path_join(&[&dir, "b.mkv"])).unwrap();
    assert_ne!(fingerprint_of(&dir), before);
}

#[test]
fn fingerprint_touch_mtime() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("fp").path, "touch"]);
    let p = path_join(&[&dir, "a.mkv"]);
    write_scan_file(&p, "a");
    let before = fingerprint_of(&dir);
    try_set_mtime(&p, std::time::SystemTime::now() + Duration::from_secs(7200));
    assert_ne!(fingerprint_of(&dir), before);
}

#[test]
fn fingerprint_subfolder() {
    let _rx = ResolverScope::new();
    let parent = path_join(&[&temp_dir("fp").path, "parent"]);
    let sub = path_join(&[&parent, "sub"]);
    std::fs::create_dir_all(&sub).unwrap();
    let before = fingerprint_of(&parent);
    let inner = path_join(&[&sub, "ep01.mkv"]);
    write_scan_file(&inner, "x");
    assert!(std::fs::metadata(&inner).is_ok());
    assert_ne!(fingerprint_of(&parent), before);
}

#[test]
fn fingerprint_missing() {
    let _rx = ResolverScope::new();
    let missing = path_join(&[&temp_dir("fp").path, "no-such-dir"]);
    assert!(folder_fingerprint(&missing).is_err());
}

#[test]
fn should_skip_hit() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("skip").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let fp = fingerprint_of(&dir);
    let ixdir = temp_dir("ix");
    let ix = load_scan_index(&path_join(&[&ixdir.path, "scanindex.json"]));
    ix.record(&dir, &fp, 7, &["k1".into(), "k2".into()]);
    assert!(ix.should_skip(&dir, &fp, 7, Some(&all_present)));
}

#[test]
fn should_skip_key_absent() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("skip").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let fp = fingerprint_of(&dir);
    let ixdir = temp_dir("ix");
    let ix = load_scan_index(&path_join(&[&ixdir.path, "scanindex.json"]));
    ix.record(&dir, &fp, 7, &["k1".into(), "k2".into()]);
    let present = |k: &str| k != "k2";
    assert!(!ix.should_skip(&dir, &fp, 7, Some(&present)));
}

#[test]
fn should_skip_schema_mismatch() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("skip").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let fp = fingerprint_of(&dir);
    let ixdir = temp_dir("ix");
    let ix = load_scan_index(&path_join(&[&ixdir.path, "scanindex.json"]));
    ix.record(&dir, &fp, 7, &["k1".into()]);
    assert!(!ix.should_skip(&dir, &fp, 8, Some(&all_present)));
}

#[test]
fn should_skip_fp_mismatch() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("skip").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let fp = fingerprint_of(&dir);
    let ixdir = temp_dir("ix");
    let ix = load_scan_index(&path_join(&[&ixdir.path, "scanindex.json"]));
    ix.record(&dir, &fp, 7, &["k1".into()]);
    assert!(!ix.should_skip(&dir, &format!("{fp}tampered"), 7, Some(&all_present)));
}

#[test]
fn should_skip_no_record() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("skip").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let fp = fingerprint_of(&dir);
    let ixdir = temp_dir("ix");
    let fresh = load_scan_index(&path_join(&[&ixdir.path, "empty.json"]));
    assert!(!fresh.should_skip(&dir, &fp, 7, Some(&all_present)));
}

#[test]
fn should_skip_nil_key_present() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("skip").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let fp = fingerprint_of(&dir);
    let ixdir = temp_dir("ix");
    let ix = load_scan_index(&path_join(&[&ixdir.path, "scanindex.json"]));
    ix.record(&dir, &fp, 7, &["k1".into()]);
    assert!(!ix.should_skip(&dir, &fp, 7, None));
}

#[test]
fn should_skip_absent_file() {
    let _rx = ResolverScope::new();
    let ixdir = temp_dir("ix");
    let ix = load_scan_index(&path_join(&[&ixdir.path, "does-not-exist.json"]));
    assert_eq!(ix.len(), 0);
    let other = temp_dir("skip").path.clone();
    assert!(!ix.should_skip(&other, "fp", 7, Some(&all_present)));
}

#[test]
fn should_skip_corrupt_file() {
    let _rx = ResolverScope::new();
    let ixdir = temp_dir("ix");
    let path = path_join(&[&ixdir.path, "scanindex.json"]);
    std::fs::write(&path, "{{not json").unwrap();
    let ix = load_scan_index(&path);
    assert_eq!(ix.len(), 0);
    assert!(!ix.should_skip(&temp_dir("skip").path.clone(), "fp", 7, Some(&all_present)));
}

#[test]
fn scan_index_round_trip() {
    let _rx = ResolverScope::new();
    let tmp = temp_dir("rt");
    let d1 = path_join(&[&tmp.path, "a"]);
    let d2 = path_join(&[&tmp.path, "b"]);
    write_scan_file(&path_join(&[&d1, "x.mkv"]), "x");
    write_scan_file(&path_join(&[&d2, "y.mkv"]), "y");
    let fp1 = fingerprint_of(&d1);
    let fp2 = fingerprint_of(&d2);
    let ix = load_scan_index(&path_join(&[&tmp.path, "idx.json"]));
    ix.record(&d1, &fp1, 3, &["ka".into()]);
    ix.record(&d2, &fp2, 3, &["kb".into(), "kc".into()]);
    let path = path_join(&[&tmp.path, "sub", "dir", "scanindex.json"]);
    ix.save(&path).unwrap();
    assert!(std::fs::metadata(crate::index::index_store_path(&path)).is_ok());
    let loaded = load_scan_index(&path);
    assert_eq!(loaded.len(), 2);
    let ka = |k: &str| k == "ka";
    let kb_kc = |k: &str| k == "kb" || k == "kc";
    let kb = |k: &str| k == "kb";
    assert!(loaded.should_skip(&d1, &fp1, 3, Some(&ka)));
    assert!(loaded.should_skip(&d2, &fp2, 3, Some(&kb_kc)));
    assert!(!loaded.should_skip(&d2, &fp2, 3, Some(&kb)));

    crate::index::close_index(&path).unwrap();
    std::fs::write(crate::index::index_store_path(&path), "not a store file").unwrap();
    assert_eq!(load_scan_index(&path).len(), 0);
}

#[test]
fn scan_index_concurrent() {
    let _rx = ResolverScope::new();
    let tmp = temp_dir("conc");
    let path = path_join(&[&tmp.path, "scanindex.json"]);
    let ix = Arc::new(load_scan_index(&path));
    let mut handles = Vec::new();
    for g in 0..8 {
        let ix = Arc::clone(&ix);
        let tmp_path = tmp.path.clone();
        let path = path.clone();
        handles.push(std::thread::spawn(move || {
            let dir = path_join(&[&tmp_path, &format!("d{g}")]);
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..25 {
                ix.record(&dir, &format!("fp-{g}-{i}"), 1, &[format!("k-{g}")]);
                if i % 5 == 0 {
                    ix.save(&path).unwrap();
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    ix.save(&path).unwrap();
    assert_eq!(load_scan_index(&path).len(), 8);
}

#[test]
fn scan_index_prune() {
    let _rx = ResolverScope::new();
    let tmp = temp_dir("prune");
    let keep = path_join(&[&tmp.path, "keep"]);
    let gone = path_join(&[&tmp.path, "gone"]);
    write_scan_file(&path_join(&[&keep, "a.mkv"]), "a");
    write_scan_file(&path_join(&[&gone, "b.mkv"]), "b");
    let fp_keep = fingerprint_of(&keep);
    let ix = load_scan_index(&path_join(&[&tmp.path, "idx.json"]));
    ix.record(&keep, &fp_keep, 1, &["k1".into()]);
    ix.record(&gone, &fingerprint_of(&gone), 1, &["k2".into()]);
    assert_eq!(ix.len(), 2);
    std::fs::remove_dir_all(&gone).unwrap();
    ix.prune_existing();
    assert_eq!(ix.len(), 1);
    assert!(ix.should_skip(&keep, &fp_keep, 1, Some(&all_present)));
}

#[test]
fn scan_index_normalize() {
    let _rx = ResolverScope::new();
    let tmp = temp_dir("norm");
    let dir = path_join(&[&tmp.path, "Videos"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    let fp = fingerprint_of(&dir);
    let ix = load_scan_index(&path_join(&[&tmp.path, "idx.json"]));
    ix.record(&dir, &fp, 1, &["k".into()]);
    let unclean = format!("{dir}\\.");
    ix.record(&unclean, &fp, 1, &["k".into()]);
    assert_eq!(ix.len(), 1);
    assert!(ix.should_skip(&unclean, &fp, 1, Some(&all_present)));
    if cfg!(windows) {
        assert!(ix.should_skip(&dir.to_uppercase(), &fp, 1, Some(&all_present)));
    }
}

#[test]
fn names_hash_stable() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("nh").path, "stable"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    assert_eq!(names_hash_of(&dir), names_hash_of(&dir));
}

#[test]
fn names_hash_add_file() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("nh").path, "add"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    let before = names_hash_of(&dir);
    write_scan_file(&path_join(&[&dir, "b.mkv"]), "b");
    assert_ne!(names_hash_of(&dir), before);
}

#[test]
fn names_hash_rename_same_count() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("nh").path, "rename"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    let before = names_hash_of(&dir);
    std::fs::rename(path_join(&[&dir, "a.mkv"]), path_join(&[&dir, "b.mkv"])).unwrap();
    assert_ne!(names_hash_of(&dir), before);
}

#[test]
fn names_hash_rename_despite_mtime_restore() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("nh").path, "lag"]);
    write_scan_file(&path_join(&[&dir, "a.mkv"]), "a");
    let before = names_hash_of(&dir);
    let old = std::fs::metadata(&dir).unwrap().modified().unwrap();
    std::fs::rename(path_join(&[&dir, "a.mkv"]), path_join(&[&dir, "b.mkv"])).unwrap();
    try_set_mtime(&dir, old);
    assert_ne!(names_hash_of(&dir), before);
}

#[test]
fn names_hash_touch_file() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("nh").path, "touch"]);
    let p = path_join(&[&dir, "a.mkv"]);
    write_scan_file(&p, "a");
    let before = names_hash_of(&dir);
    try_set_mtime(&p, std::time::SystemTime::now() + Duration::from_secs(7200));
    let _ = before;
}

#[test]
fn names_hash_missing() {
    let _rx = ResolverScope::new();
    let missing = path_join(&[&temp_dir("nh").path, "no-such-dir"]);
    assert!(folder_names_hash(&missing).is_err());
}

#[test]
fn should_skip_names_contract() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("ssn").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let nh = names_hash_of(&dir);
    let ixdir = temp_dir("ix");
    let ix = load_scan_index(&path_join(&[&ixdir.path, "scanindex.json"]));
    ix.record_full(&dir, &nh, &fingerprint_of(&dir), 7, &["k1".into()], 1);
    assert!(ix.should_skip_names(&dir, &nh, 7, Some(&all_present)));
    assert!(!ix.should_skip_names(&dir, &format!("{nh}x"), 7, Some(&all_present)));
    assert!(!ix.should_skip_names(&dir, &nh, 8, Some(&all_present)));
    let none = |_: &str| false;
    assert!(!ix.should_skip_names(&dir, &nh, 7, Some(&none)));
    assert!(!ix.should_skip_names(&dir, &nh, 7, None));

    let legacydir = temp_dir("leg");
    let legacy = load_scan_index(&path_join(&[&legacydir.path, "legacy.json"]));
    legacy.record(&dir, &fingerprint_of(&dir), 7, &["k1".into()]);
    assert!(!legacy.should_skip_names(&dir, &nh, 7, Some(&all_present)));
}

#[test]
fn inconsistent_records_never_skip() {
    let _rx = ResolverScope::new();
    let dir = path_join(&[&temp_dir("inc").path, "lib"]);
    write_scan_file(&path_join(&[&dir, "show.mkv"]), "v");
    let fp = fingerprint_of(&dir);
    let nh = names_hash_of(&dir);

    let tdir = temp_dir("trunc");
    let trunc = load_scan_index(&path_join(&[&tdir.path, "trunc.json"]));
    trunc.record_full(&dir, &nh, &fp, 7, &[], 5);
    assert!(!trunc.should_skip(&dir, &fp, 7, Some(&all_present)));
    assert!(!trunc.should_skip_names(&dir, &nh, 7, Some(&all_present)));

    let vdir = temp_dir("vac");
    let vacuous = load_scan_index(&path_join(&[&vdir.path, "vac.json"]));
    vacuous.record_full(
        &dir,
        &nh,
        &fp,
        7,
        &["k1".into(), "k2".into(), "k3".into()],
        2,
    );
    assert!(!vacuous.should_skip(&dir, &fp, 7, Some(&all_present)));
    assert!(!vacuous.should_skip_names(&dir, &nh, 7, Some(&all_present)));

    let odir = temp_dir("ok");
    let ok = load_scan_index(&path_join(&[&odir.path, "ok.json"]));
    ok.record_full(&dir, &nh, &fp, 7, &["k1".into(), "k2".into()], 2);
    assert!(ok.should_skip(&dir, &fp, 7, Some(&all_present)));
    assert!(ok.should_skip_names(&dir, &nh, 7, Some(&all_present)));
}

#[test]
fn oversize_treated_absent() {
    let _rx = ResolverScope::new();
    let dir = temp_dir("oversize");
    let path = path_join(&[&dir.path, "scanindex.json"]);
    std::fs::write(&path, "x".repeat((8 << 20) + 1)).unwrap();
    let ix = load_scan_index(&path);
    assert_eq!(ix.len(), 0);
    let skip_dir = temp_dir("skip").path.clone();
    assert!(!ix.should_skip(&skip_dir, "fp", 7, Some(&all_present)));
}
