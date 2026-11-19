
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::report::{Entry, Report, Totals, REASON_NO_COVER, REASON_NO_TITLE};
use crate::time::Time;

/// A temporary directory removed on drop (`t.TempDir`).
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "potflix-repair-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        TempDir(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Go's `sampleReport`, with the two timestamps the Go test used
/// (2026-10-12T21:30:00Z and 2026-10-12T20:00:00Z).
pub(crate) fn sample_report() -> Report {
    Report {
        generated_at: Time::from_unix(1_791_840_600, 0),
        app_version: "1.2.3".to_string(),
        totals: Totals {
            files: 40,
            indexed: 40,
            resolved: 19,
            needs_repair: 2,
            extras: 5,
            pinned: 2,
            pending: 12,
        },
        entries: vec![
            Entry {
                path: "D:\\1movies\\Some Folder\\Some File.mkv".to_string(),
                name: "Some File.mkv".to_string(),
                kind: "movie".to_string(),
                reason: REASON_NO_COVER.to_string(),
                attempts: 3,
                last_error: "open D:\\1movies\\Some Folder\\Some File.mkv: Access is denied"
                    .to_string(),
                last_try: Time::from_unix(1_791_838_800, 0),
            },
            Entry {
                path: "C:\\Users\\tester\\Desktop\\x.mkv".to_string(),
                name: "x.mkv".to_string(),
                kind: "episode".to_string(),
                reason: REASON_NO_TITLE.to_string(),
                attempts: 1,
                last_error: "metadata: no match".to_string(),
                last_try: Time::from_unix(1_791_835_200, 0),
            },
        ],
    }
}

/// Go's `pinIdentity(...)`: the identity terms supplied verbatim, in the order
/// given (the pinned lists are NOT sorted -- the real source sorts itself).
pub(crate) fn terms(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

/// `report_test.go`'s `pad2`.
pub(crate) fn pad2(i: usize) -> String {
    format!("{i:02}")
}

/// Splits a URL into (base, query) and percent-decodes the query, standing in
/// for Go's `url.Parse` + `Values.Get` (there is no URL crate in the graph).
pub(crate) fn parse_query(url: &str) -> (String, HashMap<String, String>) {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base.to_string(), query.to_string()),
        None => (url.to_string(), String::new()),
    };
    let mut out = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(k), percent_decode(v));
    }
    (base, out)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
