//! The ported Go suite (`go\internal\store\store_test.go`, all ten tests) plus
//! the cross-implementation gate: the crate must read files WRITTEN BY THE GO
//! IMPLEMENTATION (`tests/fixtures/go-*.pflx`, synthetic keys only) and
//! reproduce those same bytes from the same operations.
//!
//! Assertions are kept as the Go originals have them; none is relaxed.

use super::*;
use std::collections::HashMap as Map;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

/// Temp directories are named for the session that created them and remove
/// themselves (the live store handle shares with delete, so that always works).
const TEMP_PREFIX: &str = "pflxstore-ses_eceb4be7dffeRcOAPnHvDSq5Ed";

/// A temp directory that removes itself, whatever the test did to it.
struct TempDir {
    dir: PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{TEMP_PREFIX}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        TempDir { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }

    fn join(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn test_key(i: usize) -> Vec<u8> {
    format!("key-{i:06}").into_bytes()
}

fn test_value(i: usize) -> Vec<u8> {
    format!("value-{i:04}-payload!!").into_bytes()
}

/// Opens a store in a fresh temp dir; the dir and the store clean up themselves.
fn open_test(name: &str, opts: Options) -> (Db, TempDir, PathBuf) {
    let dir = TempDir::new();
    let path = dir.join(name);
    let db = open(&path, opts).unwrap_or_else(|e| panic!("Open({}): {e}", path.display()));
    (db, dir, path)
}

/// Parses the file's frames from the locked format and returns the file offset
/// after each complete frame. Deliberately a second, independent reading of the
/// format, so a bug in replay does not also become the test's expectation.
fn frame_ends(path: &Path) -> Vec<u64> {
    let raw = fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(
        raw.len() >= HEADER_SIZE,
        "{}: {} bytes is shorter than the header",
        path.display(),
        raw.len()
    );
    let mut ends = Vec::new();
    let mut off = HEADER_SIZE;
    while off < raw.len() {
        assert!(
            raw[off] == TAG_PUT || raw[off] == TAG_DELETE,
            "{}: unknown tag {} at {off}",
            path.display(),
            raw[off]
        );
        off += 1;
        if off + 8 > raw.len() {
            break;
        }
        let payload_len =
            u32::from_le_bytes(raw[off..off + 4].try_into().expect("4 bytes")) as usize;
        let want = u32::from_le_bytes(raw[off + 4..off + 8].try_into().expect("4 bytes"));
        off += 8;
        if off + payload_len > raw.len() {
            break;
        }
        if crate::crc32::crc32(&raw[off..off + payload_len]) != want {
            break;
        }
        off += payload_len;
        ends.push(off as u64);
    }
    ends
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .len()
}

fn dump(db: &Db) -> Map<Vec<u8>, Vec<u8>> {
    let mut out = Map::new();
    db.for_each(|k, v| {
        out.insert(k.to_vec(), v.to_vec()); // non-empty Vec, so an empty value compares equal
        true
    })
    .expect("for_each");
    out
}

fn temp_leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .expect("read_dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".compact-"))
        .collect()
}

// 1. round trip, including an empty value and binary keys/values.
#[test]
fn round_trip() {
    let (db, _dir, _path) = open_test("index.pflx", Options::default());

    let mut recs: Map<Vec<u8>, Vec<u8>> = Map::new();
    recs.insert(b"plain-key".to_vec(), b"{\"a\":1}".to_vec());
    recs.insert(b"empty-value".to_vec(), Vec::new());
    recs.insert(Vec::new(), b"the empty key is byte-exact too".to_vec());
    recs.insert(
        b"binary-\x00\xff\xfe".to_vec(),
        vec![0x00, 0x01, 0xff, 0xfe, 0x7f, 0x00],
    );
    recs.insert(
        "unicode-🎬/season-2".as_bytes().to_vec(),
        b"value\nwith\ttabs".to_vec(),
    );
    let mut long_key = b"longish-".to_vec();
    long_key.extend_from_slice(&[b'k'; 300]);
    recs.insert(long_key, vec![b'v'; 4096]);

    for (k, v) in &recs {
        db.put(k, v).unwrap_or_else(|e| panic!("Put({k:?}): {e}"));
    }
    assert_eq!(db.len(), recs.len(), "Len");
    for (k, want) in &recs {
        let got = db.get(k).unwrap_or_else(|| panic!("Get({k:?}): missing"));
        assert_eq!(&got[..], &want[..], "Get({k:?})");
    }
    assert!(
        db.get(b"never-written").is_none(),
        "Get(never-written) reported a value"
    );
    assert_eq!(dump(&db), recs, "ForEach did not reproduce the written map");

    // overwrite and delete
    db.put(b"plain-key", b"second").expect("overwrite");
    assert_eq!(
        db.get(b"plain-key").as_deref(),
        Some(&b"second"[..]),
        "overwrite"
    );
    db.delete(b"plain-key").expect("Delete");
    assert!(db.get(b"plain-key").is_none(), "deleted key still readable");
    db.delete(b"plain-key").expect("Delete of an absent key");
    assert_eq!(db.len(), recs.len() - 1, "Len after delete");

    // ForEach stops on false
    let mut seen = 0;
    db.for_each(|_, _| {
        seen += 1;
        false
    })
    .expect("ForEach");
    assert_eq!(
        seen, 1,
        "ForEach visited {seen} records after returning false, want 1"
    );

    db.commit().expect("Commit");
    db.close().expect("Close");
    db.close().expect("second Close");
    assert!(
        db.put(b"x", b"y").unwrap_err().is_closed(),
        "Put after Close"
    );
    assert!(
        db.delete(b"x").unwrap_err().is_closed(),
        "Delete after Close"
    );
    assert!(db.commit().unwrap_err().is_closed(), "Commit after Close");
    assert!(db.compact().unwrap_err().is_closed(), "Compact after Close");
    assert!(
        db.for_each(|_, _| true).unwrap_err().is_closed(),
        "ForEach after Close"
    );
    assert!(
        db.get(b"plain-key").is_none(),
        "Get after Close reported a value"
    );
    assert_eq!(db.len(), recs.len() - 1, "Len after Close");
}

// 2. reopen after Close: the snapshot plus the delta replay to the same state.
#[test]
fn reopen_snapshot_and_delta() {
    let (db, _dir, path) = open_test("index.pflx", Options::default());

    let mut want: Map<Vec<u8>, Vec<u8>> = Map::new();
    for i in 0..300 {
        db.put(test_key(i), &test_value(i)).expect("Put");
        want.insert(test_key(i), test_value(i));
    }
    db.commit().expect("Commit");
    let before = db.stats(); // snapshot boundary, then a delta on top of it
    db.compact().expect("Compact");
    assert_eq!(
        db.stats().compactions,
        before.compactions + 1,
        "Compactions"
    );
    for i in 300..400 {
        db.put(test_key(i), &test_value(i)).expect("Put");
        want.insert(test_key(i), test_value(i));
    }
    for i in 100..120 {
        db.delete(test_key(i)).expect("Delete");
        want.remove(&test_key(i));
    }
    db.close().expect("Close");

    let reopened = open(&path, Options::default()).expect("reopen");
    assert_eq!(dump(&reopened), want, "reopened state differs");
    let st = reopened.stats();
    assert_eq!(st.records, want.len() as i64, "Records");
    assert_eq!(st.delta_bytes, 0, "DeltaBytes after open");
    assert_eq!(
        st.snapshot_bytes,
        file_size(&path) as i64,
        "SnapshotBytes must be the file size"
    );
    assert_eq!(st.compactions, 0, "Compactions after open");

    // A write on the reopened store survives its own reopen.
    reopened.put(b"late", b"entry").expect("Put");
    reopened.commit().expect("Commit");
    let third = open(&path, Options::default()).expect("second reopen");
    assert_eq!(
        third.get(b"late").as_deref(),
        Some(&b"entry"[..]),
        "Get(late)"
    );
    assert_eq!(third.len(), want.len() + 1, "Len");
}

// 3. torn tail: a truncated file opens without error, keeps every complete
// frame, drops the torn one, and is repaired in place.
#[test]
fn torn_tail_repair() {
    const RECORDS: usize = 200;
    for cut in [1u64, 5, 25] {
        let dir = TempDir::new();
        let path = dir.join("index.pflx");
        let db = open(&path, Options::default()).expect("Open");
        for i in 0..RECORDS {
            db.put(test_key(i), &test_value(i)).expect("Put");
        }
        db.commit().expect("Commit");
        db.close().expect("Close");

        let ends = frame_ends(&path);
        assert_eq!(ends.len(), RECORDS, "cut {cut}: parsed frames");
        let size = file_size(&path);
        {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("open to truncate");
            f.set_len(size - cut).expect("truncate");
        }

        let repaired = open(&path, Options::default()).expect("Open after truncation");
        let full = ends.iter().filter(|&&end| end <= size - cut).count();
        assert_eq!(full, RECORDS - 1, "cut {cut}: frames were complete");
        assert_eq!(repaired.len(), full, "cut {cut}: Len");
        for i in 0..full {
            assert_eq!(
                repaired.get(test_key(i)).as_deref(),
                Some(&test_value(i)[..]),
                "cut {cut}: Get(key-{i:06})"
            );
        }
        assert!(
            repaired.get(test_key(RECORDS - 1)).is_none(),
            "cut {cut}: the torn frame's record survived"
        );
        assert_eq!(
            file_size(&path),
            ends[full - 1],
            "cut {cut}: file size after repair"
        );

        // The repaired store keeps working.
        repaired
            .put(b"after-repair", b"ok")
            .expect("Put after repair");
        repaired.commit().expect("Commit after repair");
        repaired.close().expect("Close");
        let again = open(&path, Options::default()).expect("reopen after repair");
        assert_eq!(
            again.get(b"after-repair").as_deref(),
            Some(&b"ok"[..]),
            "Get(after-repair)"
        );
        assert_eq!(again.len(), full + 1, "Len");
    }
}

// 4. corruption: one flipped payload byte drops that frame and everything after
// it, keeps everything before, and is not an error.
#[test]
fn corrupt_frame_drops_rest() {
    const RECORDS: usize = 20;
    const DAMAGED: usize = 4; // zero-based frame index

    let dir = TempDir::new();
    let path = dir.join("index.pflx");
    let db = open(&path, Options::default()).expect("Open");
    for i in 0..RECORDS {
        db.put(test_key(i), &test_value(i)).expect("Put");
    }
    db.commit().expect("Commit");
    db.close().expect("Close");

    let ends = frame_ends(&path);
    assert_eq!(ends.len(), RECORDS, "parsed frames");
    let mut raw = fs::read(&path).expect("read");
    let mut payload_start = HEADER_SIZE;
    if DAMAGED > 0 {
        payload_start = ends[DAMAGED - 1] as usize;
    }
    payload_start += 9; // frame header: tag + payloadLen + crc
    raw[payload_start] ^= 0x40;
    fs::write(&path, &raw).expect("write");

    let damaged = open(&path, Options::default()).expect("Open after corruption");
    assert_eq!(
        damaged.len(),
        DAMAGED,
        "Len: the damaged frame and everything after it"
    );
    for i in 0..DAMAGED {
        assert_eq!(
            damaged.get(test_key(i)).as_deref(),
            Some(&test_value(i)[..]),
            "Get(key-{i:06})"
        );
    }
    assert!(
        damaged.get(test_key(DAMAGED)).is_none(),
        "the damaged frame survived"
    );
    assert_eq!(
        file_size(&path),
        ends[DAMAGED - 1],
        "file size after repair"
    );
}

// 5. compaction: automatic and explicit.
#[test]
fn compaction() {
    // automatic
    {
        const THRESHOLD: i64 = 16 << 10;
        const RECORDS: usize = 5000;
        let dir = TempDir::new();
        let path = dir.join("index.pflx");
        let db = open(
            &path,
            Options {
                compact_threshold: THRESHOLD,
                ..Options::default()
            },
        )
        .expect("Open");
        for i in 0..RECORDS {
            db.put(test_key(i), &test_value(i)).expect("Put");
        }
        let st = db.stats();
        assert!(
            st.compactions > 0,
            "Compactions = 0 with a {THRESHOLD}-byte delta budget, want > 0"
        );
        assert!(
            st.delta_bytes < THRESHOLD,
            "DeltaBytes = {}, want below the {THRESHOLD}-byte budget",
            st.delta_bytes
        );
        assert_eq!(db.len(), RECORDS, "Len");
        for i in 0..RECORDS {
            assert_eq!(
                db.get(test_key(i)).as_deref(),
                Some(&test_value(i)[..]),
                "Get(key-{i:06}) missing or wrong after {} compactions",
                st.compactions
            );
        }
        let leftovers = temp_leftovers(dir.path());
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
        db.commit().expect("Commit");
        db.close().expect("Close");
        let reopened = open(&path, Options::default()).expect("reopen");
        assert_eq!(reopened.len(), RECORDS, "Len after reopen");
        assert_eq!(
            reopened.get(test_key(RECORDS - 1)).as_deref(),
            Some(&test_value(RECORDS - 1)[..]),
            "Get(key-{:06}) after reopen",
            RECORDS - 1
        );
    }

    // snapshot-smaller-than-delta
    {
        let dir = TempDir::new();
        let path = dir.join("index.pflx");
        // A budget no test run reaches, so the delta is the whole file.
        let db = open(
            &path,
            Options {
                compact_threshold: 1 << 62,
                ..Options::default()
            },
        )
        .expect("Open");

        const CHURN: usize = 5000;
        const LIVE: usize = 400; // every key is rewritten ~12 times
        let mut want: Map<Vec<u8>, Vec<u8>> = Map::new();
        for i in 0..CHURN {
            let k = test_key(i % LIVE);
            let v = format!("value-{i:06}-rewritten-payload").into_bytes();
            db.put(&k, &v).expect("Put");
            want.insert(k, v);
        }
        db.commit().expect("Commit");
        let st = db.stats();
        let before = file_size(&path);
        assert_eq!(
            before as i64,
            st.delta_bytes + HEADER_SIZE as i64,
            "file size != header+delta on an uncompacted store"
        );

        db.compact().expect("Compact");
        let after = file_size(&path);
        assert!(
            after < st.delta_bytes as u64,
            "compacted file is {after} bytes, not smaller than the pre-compaction delta of {} bytes",
            st.delta_bytes
        );
        let st2 = db.stats();
        assert_eq!(st2.delta_bytes, 0, "DeltaBytes after Compact");
        assert_eq!(st2.compactions, 1, "Compactions");
        assert_eq!(
            st2.snapshot_bytes, after as i64,
            "SnapshotBytes must be the new file size"
        );
        assert_eq!(dump(&db), want, "live keys differ after compaction");
        let leftovers = temp_leftovers(dir.path());
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }
}

// 6. large values, and MaxValueSize rejecting without breaking the DB.
#[test]
fn large_value_and_max_value_size() {
    let big = b"0123456789abcdef".repeat((256 << 10) / 16);
    let dir = TempDir::new();
    let path = dir.join("blobs.pflx");
    let db = open(&path, Options::default()).expect("Open");
    db.put(b"big", &big).expect("Put(256 KiB)");
    db.commit().expect("Commit");
    assert_eq!(db.get(b"big").as_deref(), Some(&big[..]), "Get(big)");
    db.close().expect("Close");
    let reopened = open(&path, Options::default()).expect("reopen");
    assert_eq!(
        reopened.get(b"big").as_deref(),
        Some(&big[..]),
        "Get(big) after reopen"
    );
    reopened.close().expect("Close");

    let limited_dir = TempDir::new();
    let limited = open(
        limited_dir.join("meta.pflx"),
        Options {
            max_value_size: 1024,
            ..Options::default()
        },
    )
    .expect("Open");
    assert!(
        limited
            .put(b"too-big", &vec![0u8; 1025])
            .unwrap_err()
            .is_value_too_large(),
        "Put(1025 bytes), want ErrValueTooLarge"
    );
    assert!(
        limited.get(b"too-big").is_none(),
        "a rejected value was stored"
    );
    limited
        .put(b"exact", &vec![0u8; 1024])
        .expect("Put(1024 bytes)");
    limited
        .put(b"small", b"still usable")
        .expect("Put after ErrValueTooLarge");
    limited.commit().expect("Commit");
    assert_eq!(
        limited.get(b"small").as_deref(),
        Some(&b"still usable"[..]),
        "Get(small)"
    );
    assert_eq!(limited.len(), 2, "Len");
}

// 7. concurrent readers and writers.
#[test]
fn concurrent_access() {
    const WORKERS: usize = 8;
    const OPS: usize = 200;

    let dir = TempDir::new();
    let path = dir.join("index.pflx");
    let db = open(&path, Options::default()).expect("Open");

    let mut failures: Vec<String> = Vec::new();
    thread::scope(|scope| {
        let mut handles = Vec::new();
        for w in 0..WORKERS {
            let db = &db;
            handles.push(scope.spawn(move || -> Result<(), String> {
                for i in 0..OPS {
                    let key = format!("w{w}-key-{:02}", i % 37);
                    let val = format!("w{w}-value-{i:04}").into_bytes();
                    db.put(&key, &val)
                        .map_err(|e| format!("worker {w}: Put: {e}"))?;
                    // No other worker touches this worker's key space, so the
                    // value right after Put is the value Get must return.
                    match db.get(&key) {
                        Some(got) if got[..] == val[..] => {}
                        other => return Err(format!("worker {w}: Get({key}) = {other:?}")),
                    }
                    let _ = db.len();
                    let _ = db.stats();
                    db.for_each(|_, _| true)
                        .map_err(|e| format!("worker {w}: ForEach: {e}"))?;
                    if i % 50 == 0 {
                        db.commit()
                            .map_err(|e| format!("worker {w}: Commit: {e}"))?;
                    }
                    if i % 97 == 0 {
                        let trash = format!("scratch-{w}-{i}");
                        db.put(&trash, &val)
                            .map_err(|e| format!("worker {w}: Put(scratch): {e}"))?;
                        db.delete(&trash)
                            .map_err(|e| format!("worker {w}: Delete: {e}"))?;
                        if db.get(&trash).is_some() {
                            return Err(format!(
                                "worker {w}: deleted key {trash} is still readable"
                            ));
                        }
                    }
                }
                Ok(())
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => failures.push(e),
                Err(_) => failures.push("worker panicked".to_string()),
            }
        }
    });
    assert!(failures.is_empty(), "{failures:?}");

    db.commit().expect("Commit");
    assert!(!db.is_empty(), "Len = 0 after concurrent work");
    db.close().expect("Close");
    let reopened = open(&path, Options::default()).expect("reopen");
    assert_eq!(reopened.len(), db.len(), "Len after reopen");
    // Worker 0 rewrites w0-key-00 on every i % 37 == 0, so its last write wins.
    let last = (0..OPS).rfind(|i| i % 37 == 0).expect("at least one");
    let want_val = format!("w0-value-{last:04}").into_bytes();
    assert_eq!(
        reopened.get(b"w0-key-00").as_deref(),
        Some(&want_val[..]),
        "Get(w0-key-00)"
    );
}

// 8. delete-while-open. This is the regression: a plain open shares the file as
// FILE_SHARE_READ|FILE_SHARE_WRITE only, so while a handle is live the store
// file cannot be removed - the exact failure being fixed.
#[test]
fn remove_while_open_is_not_blocked() {
    let dir = TempDir::new();
    let path = dir.join("index.pflx");
    let db = open(&path, Options::default()).expect("Open");
    db.put(b"kept", b"value").expect("Put");
    db.commit().expect("Commit");

    // The handle is live and the file holds data. Removing the whole directory
    // must not report "being used by another process".
    fs::remove_dir_all(dir.path()).expect(
        "RemoveAll with a live store handle, want no error: the store file must be shared with FILE_SHARE_DELETE",
    );
    assert!(!path.exists(), "stat after RemoveAll: want the file gone");
    // The document is released only by Close; the removal above is what a temp
    // cleanup, "Clear index", or "Clear metadata" does.
    let _ = db.close();
}

// 9. the post-delete contract: reads keep serving memory, the durability claims
// report ErrRemoved, and the report is sticky until Compact repairs.
#[test]
fn removal_while_open_reports_err_removed() {
    let dir = TempDir::new();
    let path = dir.join("index.pflx");
    let db = open(&path, Options::default()).expect("Open");

    db.put(b"kept", b"value").expect("Put");
    db.commit().expect("Commit");

    // Removal is allowed, and never observed by the writer as a write error:
    // the bytes go into a file nothing can reach.
    fs::remove_dir_all(dir.path()).expect("RemoveAll with a live store handle");

    // Reads claim no durability, so they keep serving the in-memory records.
    assert_eq!(
        db.get(b"kept").as_deref(),
        Some(&b"value"[..]),
        "Get(kept) after removal"
    );
    assert_eq!(db.len(), 1, "Len after removal");
    db.for_each(|_, _| true)
        .expect("ForEach after removal, want Ok (reads stay in memory)");

    // The durability claim reports it.
    assert!(
        db.commit().unwrap_err().is_removed(),
        "Commit after removal, want ErrRemoved"
    );
    assert!(
        !path.exists(),
        "stat after the failed Commit: want the file still gone"
    );
    // Sticky, like every other append failure: the DB never silently writes on.
    assert!(
        db.put(b"late", b"x").unwrap_err().is_removed(),
        "Put after removal, want ErrRemoved"
    );
    assert!(
        db.delete(b"kept").unwrap_err().is_removed(),
        "Delete after removal, want ErrRemoved"
    );
    assert!(
        db.commit().unwrap_err().is_removed(),
        "second Commit after removal, want ErrRemoved"
    );

    // Compact is the repair: it writes a fresh snapshot at the path, which
    // recreates the file, and clears the failure. The in-memory records are the
    // ones it writes - they stayed authoritative while the file was gone.
    fs::create_dir_all(dir.path()).expect("recreate dir");
    db.compact().expect("Compact after removal");
    assert!(path.exists(), "stat after Compact: want the file rewritten");
    db.commit().expect("Commit after Compact");
    db.close().expect("Close");
    let reopened = open(&path, Options::default()).expect("reopen");
    assert_eq!(
        reopened.get(b"kept").as_deref(),
        Some(&b"value"[..]),
        "reopened Get(kept)"
    );
}

// 10. compaction still works with the new open mode: the swap closes the handle
// before the rename, and both open sites - Open and the reopen the swap does -
// carry FILE_SHARE_DELETE, so a compacted store is removable too.
#[test]
fn compaction_and_removal_with_shared_open() {
    let dir = TempDir::new();
    let path = dir.join("index.pflx");
    let db = open(
        &path,
        Options {
            compact_threshold: 16 << 10,
            ..Options::default()
        },
    )
    .expect("Open");

    const RECORDS: usize = 2000;
    for i in 0..RECORDS {
        db.put(test_key(i), &test_value(i)).expect("Put");
    }
    db.commit().expect("Commit");
    assert!(
        db.stats().compactions > 0,
        "Compactions = 0, want > 0 with a 16 KiB delta budget"
    );
    assert_eq!(db.len(), RECORDS, "Len");
    for i in 0..RECORDS {
        assert_eq!(
            db.get(test_key(i)).as_deref(),
            Some(&test_value(i)[..]),
            "Get(key-{i:06}) after compaction missing or wrong"
        );
    }
    let leftovers = temp_leftovers(dir.path());
    assert!(
        leftovers.is_empty(),
        "leftover temp files after the swap: {leftovers:?}"
    );

    // Reopen: the file this handle points at is the renamed snapshot.
    db.close().expect("Close");
    let reopened = open(&path, Options::default()).expect("reopen after compaction");
    assert_eq!(reopened.len(), RECORDS, "Len after reopen");
    // The reopened handle shares with delete as well: compaction's second open
    // site must not regress the fix.
    fs::remove_dir_all(dir.path()).expect("RemoveAll after the compaction reopen");
    assert!(
        reopened.commit().unwrap_err().is_removed(),
        "Commit on the reopened handle after removal, want ErrRemoved"
    );
    let _ = reopened.close();
}


/// The records the Go generator wrote: `k4` was put then tombstoned, `k2` was
/// overwritten, `k5` holds a zero-length value.
const GO_FIXTURE_RECORDS: [(&str, &str); 4] =
    [("k1", "v1"), ("k2", "v2"), ("k3", "v3"), ("k5", "")];

const GO_FIXTURES: [&str; 2] = ["go-append-delta-v1.pflx", "go-compacted-v1.pflx"];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Opens a Go-written fixture and returns its records as `(key, value)` pairs in
/// sorted key order -- the same shape the Go generator printed.
fn fixture_records(path: &Path) -> Vec<(String, String)> {
    let db = open(path, Options::default()).expect("open the Go-written fixture");
    let mut pairs = Vec::new();
    db.for_each(|k, v| {
        pairs.push((
            String::from_utf8_lossy(k).into_owned(),
            String::from_utf8_lossy(v).into_owned(),
        ));
        true
    })
    .expect("for_each");
    pairs.sort();
    pairs
}

#[test]
fn go_written_fixtures_open_identically() {
    let want: Vec<(String, String)> = GO_FIXTURE_RECORDS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for name in GO_FIXTURES {
        let path = fixture(name);
        let size_before = file_size(&path);
        let got = fixture_records(&path);
        assert_eq!(got.len(), GO_FIXTURE_RECORDS.len(), "{name}: record count");
        assert_eq!(got, want, "{name}: records");
        assert_eq!(
            file_size(&path),
            size_before,
            "{name}: a complete fixture must not be truncated"
        );
        println!(
            "rust  {name}: size={size_before} count={} records={got:?}",
            got.len()
        );
    }
}

#[test]
fn go_written_fixtures_carry_the_locked_header() {
    for name in GO_FIXTURES {
        let raw = fs::read(fixture(name)).expect("read fixture");
        assert!(raw.len() >= HEADER_SIZE, "{name}: shorter than the header");
        assert_eq!(&raw[0..8], b"PFLXST01", "{name}: magic");
        assert_eq!(
            u32::from_le_bytes(raw[8..12].try_into().expect("4 bytes")),
            1,
            "{name}: version"
        );
        assert_eq!(
            u32::from_le_bytes(raw[12..16].try_into().expect("4 bytes")),
            0,
            "{name}: flags"
        );
        assert_eq!(
            u32::from_le_bytes(raw[20..24].try_into().expect("4 bytes")),
            0,
            "{name}: reserved"
        );
        assert_eq!(
            u32::from_le_bytes(raw[16..20].try_into().expect("4 bytes")),
            crate::crc32::crc32(&raw[0..16]),
            "{name}: header CRC"
        );
    }
}

/// The strongest form of the gate: the same operations, from this crate, must
/// produce the SAME BYTES as the Go implementation -- both the append-log shape
/// (tombstone and overwrite frames included) and the compacted shape (sorted
/// put frames only).
#[test]
fn rust_reproduces_the_go_written_bytes() {
    fn ops(db: &Db) {
        for (k, v) in [
            ("k1", "v1"),
            ("k2", "v2-old"),
            ("k3", "v3"),
            ("k4", "v4-tombstoned"),
            ("k5", ""),
        ] {
            db.put(k, v.as_bytes()).expect("Put");
        }
        db.delete("k4").expect("Delete");
        db.put("k2", b"v2").expect("overwrite");
    }

    let dir = TempDir::new();

    let delta = dir.join("delta.pflx");
    {
        let db = open(&delta, Options::default()).expect("Open");
        ops(&db);
        db.commit().expect("Commit");
        db.close().expect("Close");
    }
    assert_eq!(
        fs::read(&delta).expect("read rust delta"),
        fs::read(fixture("go-append-delta-v1.pflx")).expect("read go delta"),
        "append-delta bytes differ from the Go implementation's"
    );

    let compacted = dir.join("compacted.pflx");
    {
        let db = open(&compacted, Options::default()).expect("Open");
        ops(&db);
        db.compact().expect("Compact");
        db.commit().expect("Commit");
        db.close().expect("Close");
    }
    assert_eq!(
        fs::read(&compacted).expect("read rust snapshot"),
        fs::read(fixture("go-compacted-v1.pflx")).expect("read go snapshot"),
        "compacted bytes differ from the Go implementation's"
    );
}

/// Extra, non-committed signal: the REAL Go-written multi-MB store at
/// `%LOCALAPPDATA%\Potflix\meta.idx`. Read-only -- it never opens the store for
/// append and never truncates. `cargo test -p store --release -- --ignored
/// --nocapture meta_idx_probe`.
#[test]
#[ignore = "reads the owner's real cache read-only; run explicitly with --ignored"]
fn meta_idx_probe() {
    let local = std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA");
    let path = PathBuf::from(local).join("Potflix").join("meta.idx");
    let mut f = File::open(&path).expect("read-only open of the real Go-written store");
    let size = f.metadata().expect("stat").len();
    let mut hdr = [0u8; HEADER_SIZE];
    read_header(&path, &mut f, &mut hdr).expect("the real store's header validates");
    f.seek(SeekFrom::Start(HEADER_SIZE as u64)).expect("seek");
    let mut records = Records::new();
    let mut payload_buf = Vec::new();
    let valid = replay_frames(&mut f, size, &mut records, &mut payload_buf);
    let tail = if valid == size { "" } else { " (torn tail)" };
    println!(
        "meta.idx: {size} bytes, {} records, last valid boundary {valid}{tail}",
        records.len()
    );
    assert!(!records.is_empty(), "the real store must have records");
}
