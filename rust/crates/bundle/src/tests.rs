
#![allow(non_snake_case)]

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::value::RawValue;

use super::*;

/// The schema the tests seed and round-trip: the live cache schema, so a
/// `CACHE_SCHEMA` bump keeps seeding live entries (a pinned old value would
/// go stale by design and fail liveness asserts).
const TEST_SCHEMA: i32 = metadata::cache::CACHE_SCHEMA;


/// `t.TempDir()`: a fresh directory under the approved temp root, removed on
/// drop -- including the store file that lives BESIDE it (`<dir>.idx`).
struct TempDir {
    path: PathBuf,
}

static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let n = TEMP_SEQ.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "potflix-bundle-test-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    /// The directory as the path string the cache functions take.
    fn s(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    fn child(&self, name: &str) -> String {
        self.path.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        let index = PathBuf::from(format!("{}.idx", self.path.to_string_lossy()));
        let _ = std::fs::remove_file(index);
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `cacheNameFor` restates the 16-hex cache filename rule independently of the
/// crate under test.
fn cache_name_for(key: &str) -> String {
    let digest = sha256(key.as_bytes());
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(8) {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{hex}.json")
}

/// A raw JSON value carrying `media` verbatim (Go's `json.RawMessage`).
fn raw_value(media: &str) -> Option<Box<RawValue>> {
    Some(RawValue::from_string(media.to_string()).expect("valid raw media"))
}

/// Stores one live cache entry the way the resolver does: stamped fresh, so it
/// holds a full positive-TTL window.
fn seed_live_entry(dir: &str, key: &str, schema: i32, media: &str) {
    let rec = CacheRecord {
        key: key.to_string(),
        schema,
        media: raw_value(media),
        checked_at: metadata::timefmt::now_rfc3339(),
    };
    let payload = serde_json::to_vec(&rec).expect("marshal live seed");
    cache::cache_put_raw(dir, key, &payload).expect("seed live entry");
}

/// Writes one LEGACY loose per-key cache file and returns its name.
fn seed_cache_file(dir: &str, key: &str, schema: i32, media: &str) -> String {
    let name = cache_name_for(key);
    let rec = CacheRecord {
        key: key.to_string(),
        schema,
        media: raw_value(media),
        checked_at: metadata::timefmt::now_rfc3339(),
    };
    let data = serde_json::to_vec(&rec).expect("marshal seed");
    std::fs::write(Path::new(dir).join(&name), data).expect("write seed");
    name
}

/// A copy of the stored bytes for `key` in `dir`'s live cache.
fn live_value(dir: &str, key: &str) -> Option<Vec<u8>> {
    let mut out = None;
    cache::cache_for_each(
        dir,
        Some(|k: &str, v: &[u8]| -> bool {
            if k == key {
                out = Some(v.to_vec());
                return false;
            }
            true
        }),
    )
    .expect("CacheForEach");
    out
}

/// A minimal well-formed bundle payload for import tests (Go's `json.Marshal`).
fn test_bundle_bytes(entries: &[Entry]) -> Vec<u8> {
    let b = Bundle {
        header: Header {
            format: FORMAT_NAME.to_string(),
            version: FORMAT_VERSION,
            schema: TEST_SCHEMA,
            app_version: "test".to_string(),
            exported_at: String::new(),
            count: entries.len() as i32,
            note: NOTE_TEXT.to_string(),
        },
        entries: entries.to_vec(),
    };
    serde_json::to_vec(&b).expect("marshal test bundle")
}

fn dir_child_names(dir: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("readdir")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[cfg(windows)]
fn make_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}
#[cfg(not(windows))]
fn make_dir_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
#[cfg(windows)]
fn make_file_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}
#[cfg(not(windows))]
fn make_file_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}


#[test]
fn TestBundleRoundTrip() {
    let a = TempDir::new("round-trip-a");
    let b = TempDir::new("round-trip-b");
    let keys = ["movie|alpha|2001", "series|beta|2020|2", "movie|gamma|1999"];
    let media = [
        r#"{"source":"tvmaze","title":"Alpha","artwork":"https://image.example.com/alpha.jpg"}"#,
        r#"{"source":"tvmaze","title":"Beta","genres":"Drama"}"#,
        r#"{"source":"itunes","title":"Gamma","length":"1h 30m"}"#,
    ];
    for (key, m) in keys.iter().zip(media.iter()) {
        seed_live_entry(&a.s(), key, TEST_SCHEMA, m);
    }
    assert_eq!(count(&a.s()).unwrap(), 3);

    let mut buf = Vec::new();
    let est = export(&a.s(), &mut buf, TEST_SCHEMA, "test-0.1").unwrap();
    assert_eq!((est.entries, est.skipped), (3, 0), "Export stats = {est:?}");
    assert_eq!(est.bytes, buf.len() as i64);

    let got: Bundle = serde_json::from_slice(&buf).unwrap();
    assert_eq!(got.header.format, FORMAT_NAME);
    assert_eq!(got.header.version, FORMAT_VERSION);
    assert_eq!(got.header.schema, TEST_SCHEMA);
    assert_eq!(got.header.count, 3);
    for want in [
        "metadata only",
        "titles",
        "synopsis",
        "genres",
        "length",
        "button URLs",
        "poster URLs",
        "no media files",
        "no library paths",
        "no credentials",
    ] {
        assert!(
            got.header.note.contains(want),
            "note missing {want}: {}",
            got.header.note
        );
    }

    let mut reader = Cursor::new(buf.clone());
    let ist = import(&b.s(), &mut reader, TEST_SCHEMA).unwrap();
    assert_eq!(
        (
            ist.added,
            ist.total,
            ist.present,
            ist.skipped_schema,
            ist.invalid
        ),
        (3, 3, 0, 0, 0),
        "Import stats = {ist:?}"
    );
    assert_eq!(count(&b.s()).unwrap(), 3);

    for key in keys.iter() {
        assert!(
            cache::cache_live(&b.s(), key),
            "imported entry {key} not live"
        );
        let da = live_value(&a.s(), key).expect("source vanished");
        let db = live_value(&b.s(), key).expect("imported missing");
        assert_eq!(da, db, "entry {key} differs after round-trip");
    }
    // Visible to the resolver's own probe path immediately afterwards.
    let res = metadata::Resolver::new(metadata::Options {
        dir: b.s(),
        offline: true,
        ..Default::default()
    });
    for key in keys.iter() {
        assert!(
            res.probe_cache(key),
            "ProbeCache({key}) missed right after import"
        );
    }
    // Neither directory gained a loose cache file.
    for dir in [&a, &b] {
        for n in dir_child_names(&dir.s()) {
            assert!(
                !valid_cache_name(&n),
                "bundle wrote a loose cache file {n} into {}",
                dir.s()
            );
        }
    }
}

#[test]
fn TestBundleDeterminism() {
    let dir = TempDir::new("determinism");
    seed_cache_file(
        &dir.s(),
        "movie|alpha|2001",
        TEST_SCHEMA,
        r#"{"title":"Alpha"}"#,
    );
    seed_cache_file(
        &dir.s(),
        "movie|beta|2002",
        TEST_SCHEMA,
        r#"{"title":"Beta"}"#,
    );

    let mut b1 = Vec::new();
    let mut b2 = Vec::new();
    export(&dir.s(), &mut b1, TEST_SCHEMA, "test").unwrap();
    export(&dir.s(), &mut b2, TEST_SCHEMA, "test").unwrap();

    let mut g1: Bundle = serde_json::from_slice(&b1).unwrap();
    let mut g2: Bundle = serde_json::from_slice(&b2).unwrap();
    assert!(
        !g1.header.exported_at.is_empty(),
        "ExportedAt is empty, want a real export timestamp"
    );
    assert!(!g2.header.exported_at.is_empty());
    g1.header.exported_at = String::new();
    g2.header.exported_at = String::new();
    assert_eq!(
        serde_json::to_vec(&g1).unwrap(),
        serde_json::to_vec(&g2).unwrap(),
        "entry lists differ"
    );
}

#[test]
fn TestBundleEntryFileNameFollowsMetadataRule() {
    assert_eq!(
        cache_name_for("movie|tracked|2001"),
        cache::cache_file_name_for("movie|tracked|2001")
    );
    let dir = TempDir::new("file-name");
    let key = "movie|sixteen|2001";
    seed_live_entry(
        &dir.s(),
        key,
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"Sixteen"}"#,
    );

    let mut buf = Vec::new();
    let est = export(&dir.s(), &mut buf, TEST_SCHEMA, "test").unwrap();
    assert_eq!(est.entries, 1);
    let got: Bundle = serde_json::from_slice(&buf).unwrap();
    assert_eq!(got.entries.len(), 1);
    let want = cache::cache_file_name_for(key);
    assert_eq!(got.entries[0].file, want);
    assert!(valid_cache_name(&want) && want.len() == 16 + ".json".len());
}

#[test]
fn TestBundleLegacyBundleFileStillImports() {
    let key = "series|legacy|2016|1";
    let file = cache_name_for(key);
    let media = r#"{"source":"tvmaze","title":"Legacy","genres":"Drama"}"#;
    let legacy = Bundle {
        header: Header {
            format: FORMAT_NAME.to_string(),
            version: FORMAT_VERSION,
            schema: TEST_SCHEMA,
            app_version: "potflix-legacy".to_string(),
            exported_at: "2025-03-04T05:06:07Z".to_string(),
            count: 1,
            note: NOTE_TEXT.to_string(),
        },
        entries: vec![Entry {
            file: file.clone(),
            key: key.to_string(),
            schema: TEST_SCHEMA,
            media: raw_value(media),
            // Deliberately undated: proves a pre-TTL bundle still imports.
            checked_at: String::new(),
        }],
    };
    let mut raw = serde_json::to_vec_pretty(&legacy).unwrap();
    raw.push(b'\n');
    let holder = TempDir::new("legacy-file");
    let path = holder.path.join("potflix-metadata-legacy.json");
    std::fs::write(&path, raw).unwrap();
    let mut f = std::fs::File::open(&path).unwrap();

    let dst = TempDir::new("legacy-file-dst");
    let st = import(&dst.s(), &mut f, TEST_SCHEMA).unwrap();
    assert_eq!(
        (
            st.added,
            st.total,
            st.present,
            st.skipped_schema,
            st.invalid
        ),
        (1, 1, 0, 0, 0),
        "Import stats = {st:?}"
    );
    // A pre-TTL bundle carries no freshness stamp, so the imported record is
    // stored byte-identical but STALE: the next lookup re-fetches it rather
    // than serving undated data indefinitely.
    assert!(!cache::cache_live(&dst.s(), key));
    let got = live_value(&dst.s(), key).expect("legacy-bundle entry missing");
    let rec: CacheRecord = serde_json::from_slice(&got).unwrap();
    assert_eq!(rec.key, key);
    assert_eq!(rec.schema, TEST_SCHEMA);
    assert_eq!(rec.media.as_deref().map(|r| r.get()), Some(media));
    assert!(
        live_value(&dst.s(), "other").is_none(),
        "import invented an extra entry"
    );
}

#[test]
fn TestBundleFillMissingOnly() {
    let a = TempDir::new("fill-a");
    let b = TempDir::new("fill-b");
    let keys = ["movie|alpha|2001", "movie|beta|2002", "movie|gamma|2003"];
    for k in keys.iter() {
        seed_live_entry(
            &a.s(),
            k,
            TEST_SCHEMA,
            &format!(r#"{{"source":"tvmaze","title":"{k}"}}"#),
        );
    }
    let mut buf = Vec::new();
    export(&a.s(), &mut buf, TEST_SCHEMA, "test").unwrap();

    seed_live_entry(
        &b.s(),
        keys[0],
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"stale ogniwa"}"#,
    );
    let before = live_value(&b.s(), keys[0]).expect("pre-seeded entry missing");

    let mut reader = Cursor::new(buf.clone());
    let st = import(&b.s(), &mut reader, TEST_SCHEMA).unwrap();
    assert_eq!(
        (st.present, st.added, st.total),
        (1, 2, 3),
        "Import stats = {st:?}"
    );
    let after = live_value(&b.s(), keys[0]).expect("pre-existing entry vanished");
    assert_eq!(before, after, "pre-existing stored bytes replaced");
    assert_eq!(count(&b.s()).unwrap(), 3);
}

#[test]
fn TestBundleSchemaMismatch() {
    let dir = TempDir::new("schema-mismatch");
    let name = cache_name_for("movie|stale|2001");
    let data = test_bundle_bytes(&[Entry {
        file: name,
        key: "movie|stale|2001".to_string(),
        schema: TEST_SCHEMA + 1,
        media: raw_value(r#"{"title":"Stale"}"#),
        checked_at: String::new(),
    }]);
    let mut reader = Cursor::new(data);
    let st = import(&dir.s(), &mut reader, TEST_SCHEMA).unwrap();
    assert_eq!(
        (
            st.skipped_schema,
            st.total,
            st.added,
            st.present,
            st.invalid
        ),
        (1, 1, 0, 0, 0),
        "Import stats = {st:?}"
    );
    assert!(!cache::cache_has(&dir.s(), "movie|stale|2001"));
    assert_eq!(count(&dir.s()).unwrap(), 0);
}

#[test]
fn TestBundleInvalidEntries() {
    let parent = TempDir::new("invalid");
    let dir = parent.path.join("cache");
    std::fs::create_dir_all(&dir).unwrap();
    let raw = r#"{"title":"X"}"#;
    let data = test_bundle_bytes(&[
        Entry {
            file: "../escape.json".to_string(),
            key: "evil".to_string(),
            schema: TEST_SCHEMA,
            media: raw_value(raw),
            checked_at: String::new(),
        },
        Entry {
            file: "not-a-cache-name.json".to_string(),
            key: "k2".to_string(),
            schema: TEST_SCHEMA,
            media: raw_value(raw),
            checked_at: String::new(),
        },
        Entry {
            file: cache_name_for("k3"),
            key: String::new(),
            schema: TEST_SCHEMA,
            media: raw_value(raw),
            checked_at: String::new(),
        },
    ]);
    let mut reader = Cursor::new(data);
    let st = import(&s(&dir), &mut reader, TEST_SCHEMA).unwrap();
    assert_eq!(
        (st.invalid, st.total, st.added),
        (3, 3, 0),
        "Import stats = {st:?}"
    );
    let kids = dir_child_names(&parent.s());
    assert_eq!(kids, vec!["cache".to_string()], "parent dir gained files");
    assert!(
        !parent.path.join("escape.json").exists(),
        "escape file exists outside dir"
    );
    assert!(dir_child_names(&s(&dir)).is_empty(), "cache dir not empty");
}

#[test]
fn TestBundleCorruptSourceSkipped() {
    let dir = TempDir::new("corrupt");
    seed_live_entry(
        &dir.s(),
        "movie|good-a|2001",
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"A"}"#,
    );
    seed_live_entry(
        &dir.s(),
        "movie|good-b|2002",
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"B"}"#,
    );
    cache::cache_put_raw(&dir.s(), "movie|broken|2003", br#"{"key": oops, not json"#).unwrap();
    std::fs::write(
        dir.path.join(cache_name_for("movie|legacy-corrupt|2004")),
        br#"{"key": oops, not json"#,
    )
    .unwrap();

    let mut buf = Vec::new();
    let st = export(&dir.s(), &mut buf, TEST_SCHEMA, "test").unwrap();
    assert_eq!((st.entries, st.skipped), (2, 1), "Export stats = {st:?}");
    let got: Bundle = serde_json::from_slice(&buf).unwrap();
    assert_eq!(got.entries.len(), 2);
    for e in &got.entries {
        assert!(
            !e.key.contains("broken") && !e.key.contains("corrupt"),
            "bundle carries a record that does not decode: {e:?}"
        );
    }
}

#[test]
fn TestBundleClear() {
    let dir = TempDir::new("clear");
    seed_live_entry(
        &dir.s(),
        "movie|keep-a|2001",
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"A"}"#,
    );
    seed_live_entry(
        &dir.s(),
        "movie|keep-b|2002",
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"B"}"#,
    );
    let legacy = seed_cache_file(
        &dir.s(),
        "movie|legacy-c|2003",
        TEST_SCHEMA,
        r#"{"title":"C"}"#,
    );
    std::fs::write(dir.path.join("notes.json"), br#"{"keep":true}"#).unwrap();
    std::fs::write(dir.path.join(".tmp-orphan-123.json"), br#"partial"#).unwrap();
    let sub = dir.path.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("inner.json"), br#"{"in":"sub"}"#).unwrap();

    let removed = clear(&dir.s()).unwrap();
    assert_eq!(removed, 2, "Clear cleared {removed} titles, want 2");
    assert!(!cache::cache_has(&dir.s(), "movie|keep-a|2001"));
    assert!(!cache::cache_has(&dir.s(), "movie|keep-b|2002"));
    assert!(!cache::cache_has(&dir.s(), "movie|legacy-c|2003"));
    assert_eq!(count(&dir.s()).unwrap(), 0);
    for gone in [&legacy, &".tmp-orphan-123.json".to_string()] {
        assert!(!dir.path.join(gone).exists(), "{gone} was not removed");
    }
    assert_eq!(
        std::fs::read(dir.path.join("notes.json")).unwrap(),
        br#"{"keep":true}"#
    );
    assert_eq!(
        std::fs::read(sub.join("inner.json")).unwrap(),
        br#"{"in":"sub"}"#
    );
    assert!(
        dir.path.exists(),
        "Clear removed the cache directory itself"
    );
}

#[test]
fn TestBundlePrivacyGuard() {
    let dir = TempDir::new("privacy");
    seed_live_entry(
        &dir.s(),
        "movie|url|2001",
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"Linked","artwork":"https://image.example.com/poster.jpg","movieUrl":"https://example.com/watch"}"#,
    );

    let mut buf = Vec::new();
    export(&dir.s(), &mut buf, TEST_SCHEMA, "test").unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(
        !text
            .as_bytes()
            .windows(3)
            .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && w[2] == b'\\'),
        "bundle contains a Windows drive path"
    );
    assert!(
        !text.contains("LOCALAPPDATA"),
        "bundle mentions LOCALAPPDATA"
    );
    assert!(
        text.contains("https://image.example.com/poster.jpg"),
        "poster URL lost from bundle"
    );
}

#[test]
fn TestBundleOversizeRejected() {
    let dir = TempDir::new("oversize");
    let big = vec![b'{'; MAX_BUNDLE_BYTES + 1];
    let mut reader = Cursor::new(big);
    let err = import(&dir.s(), &mut reader, TEST_SCHEMA).unwrap_err();
    assert!(
        err.to_string().contains("64 MB"),
        "error does not name the limit: {err}"
    );
    assert!(
        dir_child_names(&dir.s()).is_empty(),
        "oversize import wrote files"
    );
    assert_eq!(count(&dir.s()).unwrap(), 0);
}

#[test]
fn TestBundleImportNoReplace() {
    let dir = TempDir::new("no-replace");
    let key = "movie|noreplace|2001";
    let name = cache_name_for(key);
    seed_live_entry(
        &dir.s(),
        key,
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"ORIGINAL"}"#,
    );
    let before = live_value(&dir.s(), key).expect("pre-seeded entry missing");

    let data = test_bundle_bytes(&[Entry {
        file: name,
        key: key.to_string(),
        schema: TEST_SCHEMA,
        media: raw_value(r#"{"source":"tvmaze","title":"BUNDLE"}"#),
        checked_at: String::new(),
    }]);
    let mut reader = Cursor::new(data);
    let st = import(&dir.s(), &mut reader, TEST_SCHEMA).unwrap();
    assert_eq!(
        (st.present, st.added, st.total),
        (1, 0, 1),
        "Import stats = {st:?}"
    );
    let after = live_value(&dir.s(), key).expect("pre-existing entry vanished");
    assert_eq!(before, after, "pre-existing stored bytes replaced");
    for n in dir_child_names(&dir.s()) {
        assert!(!n.starts_with(".tmp-"), "temp file left behind: {n}");
    }
}

#[test]
fn TestBundleForeignFilesIgnored() {
    let dir = TempDir::new("foreign");
    std::fs::write(dir.path.join("notes.json"), br#"{"keep":true}"#).unwrap();
    seed_live_entry(
        &dir.s(),
        "movie|real|2001",
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"Real"}"#,
    );

    let mut buf = Vec::new();
    let est = export(&dir.s(), &mut buf, TEST_SCHEMA, "test").unwrap();
    assert_eq!((est.entries, est.skipped), (1, 0), "Export stats = {est:?}");
    clear(&dir.s()).unwrap();
    assert_eq!(
        std::fs::read(dir.path.join("notes.json")).unwrap(),
        br#"{"keep":true}"#
    );
}

#[test]
fn TestBundleOversizeLegacyEntryNeverBecomesAnEntry() {
    let dir = TempDir::new("oversize-legacy");
    seed_live_entry(
        &dir.s(),
        "movie|good|2001",
        TEST_SCHEMA,
        r#"{"source":"tvmaze","title":"Good"}"#,
    );
    let key = "movie|big|2002";
    std::fs::write(
        dir.path.join(cache_name_for(key)),
        vec![b'x'; (4 << 20) + 1],
    )
    .unwrap();

    let mut buf = Vec::new();
    let est = export(&dir.s(), &mut buf, TEST_SCHEMA, "test").unwrap();
    assert_eq!((est.entries, est.skipped), (1, 0), "Export stats = {est:?}");
    assert!(
        !cache::cache_has(&dir.s(), key),
        "oversize legacy file was stored as a cache entry"
    );
}

#[test]
fn TestBundleReparsePointDirRefused() {
    let parent = TempDir::new("reparse-dir");
    let real = parent.path.join("real");
    std::fs::create_dir(&real).unwrap();
    seed_cache_file(
        &s(&real),
        "movie|behind-link|2001",
        TEST_SCHEMA,
        r#"{"title":"Behind"}"#,
    );
    let link = parent.path.join("link");
    if make_dir_link(&real, &link).is_err() {
        return; // os.Symlink refused (no privilege/Developer Mode?)
    }

    let mut buf = Vec::new();
    let err = export(&s(&link), &mut buf, TEST_SCHEMA, "test").unwrap_err();
    assert!(
        err.to_string().contains("reparse-point"),
        "Export error = {err}"
    );
    let err = clear(&s(&link)).unwrap_err();
    assert!(
        err.to_string().contains("reparse-point"),
        "Clear error = {err}"
    );
    let data = test_bundle_bytes(&[Entry {
        file: cache_name_for("movie|new|2002"),
        key: "movie|new|2002".to_string(),
        schema: TEST_SCHEMA,
        media: raw_value(r#"{"title":"New"}"#),
        checked_at: String::new(),
    }]);
    let mut reader = Cursor::new(data);
    let err = import(&s(&link), &mut reader, TEST_SCHEMA).unwrap_err();
    assert!(
        err.to_string().contains("reparse-point"),
        "Import error = {err}"
    );
    // The real directory behind the link must be untouched.
    assert_eq!(count(&s(&real)).unwrap(), 1);
    assert!(cache::cache_has(&s(&real), "movie|behind-link|2001"));
}

#[test]
fn TestBundleClearKeepsReparsePointEntries() {
    let dir = TempDir::new("clear-reparse");
    let outside = TempDir::new("clear-reparse-outside");
    let secret = outside.path.join("secret.json");
    std::fs::write(
        &secret,
        br#"{"key":"movie|secret|2001","schema":3,"media":{"title":"Secret"}}"#,
    )
    .unwrap();
    let link_name = cache_name_for("movie|secret|2001");
    let link_path = dir.path.join(&link_name);
    if make_file_link(&secret, &link_path).is_err() {
        return; // os.Symlink refused (no privilege/Developer Mode?)
    }
    let good = seed_cache_file(
        &dir.s(),
        "movie|good|2002",
        TEST_SCHEMA,
        r#"{"title":"Good"}"#,
    );

    let removed = clear(&dir.s()).unwrap();
    assert_eq!(removed, 2, "Clear cleared {removed} titles, want 2");
    assert!(
        std::fs::symlink_metadata(&link_path).is_ok(),
        "reparse-point entry was deleted through the link"
    );
    assert!(
        std::fs::metadata(&secret).is_ok(),
        "outside link target touched"
    );
    assert!(
        !dir.path.join(&good).exists(),
        "neighbouring legacy cache file was not removed"
    );
}

#[test]
fn TestBundleEmptyDirRefused() {
    let mut buf = Vec::new();
    assert!(export("", &mut buf, TEST_SCHEMA, "test").is_err());
    assert!(count("").is_err());
    assert!(clear("").is_err());
    let data = test_bundle_bytes(&[Entry {
        file: cache_name_for("k"),
        key: "k".to_string(),
        schema: TEST_SCHEMA,
        media: raw_value("{}"),
        checked_at: String::new(),
    }]);
    let mut reader = Cursor::new(data);
    assert!(import("", &mut reader, TEST_SCHEMA).is_err());
}

#[test]
fn TestBundleMissingDirIsEmpty() {
    let holder = TempDir::new("missing");
    let dir = holder.child("meta");
    assert_eq!(count(&dir).unwrap(), 0);
    assert_eq!(clear(&dir).unwrap(), 0);
    let mut buf = Vec::new();
    let est = export(&dir, &mut buf, TEST_SCHEMA, "test").unwrap();
    assert_eq!(est.entries, 0);
}

// --- sha256 (restates metadata::cache's private rule independently) ---------

#[allow(clippy::needless_range_loop)]
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}

#[test]
fn TestSha256Sanity() {
    // "abc" digest, checkable by hand against the published vector.
    let digest = sha256(b"abc");
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
