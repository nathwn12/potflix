
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sanitize::{basename_of, one_line, sanitize_error_with, truncate_bytes};
use crate::time::Time;
use crate::{Error, Result};

/// Canonical Reason values. A caller may pass any string; the report only uses
/// these to build a grouped title.
pub const REASON_NO_COVER: &str = "no-cover";
/// See [`REASON_NO_COVER`].
pub const REASON_NO_TITLE: &str = "no-title";
/// See [`REASON_NO_COVER`].
pub const REASON_NO_SYNOPSIS: &str = "no-synopsis";
/// See [`REASON_NO_COVER`].
pub const REASON_NO_METADATA: &str = "no-metadata";

/// Local report file names, both written into the directory handed to
/// [`write_report`].
pub const REPORT_JSON_NAME: &str = "needs-repair.json";
/// See [`REPORT_JSON_NAME`].
pub const REPORT_TXT_NAME: &str = "needs-repair.txt";

/// Caps how many entries [`issue_body`] publishes; the remainder is summarized
/// as one "and N more" line.
const MAX_ISSUE_ENTRIES: usize = 50;

/// Caps a single error string published in the issue body. A BYTE cap, not a
/// rune cap.
const MAX_ERROR_BYTES: usize = 200;

/// One media file that could not be resolved.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The full local path. It NEVER leaves the machine: it is written to the
    /// local JSON/TXT report and never to the issue body.
    #[serde(rename = "Path")]
    pub path: String,
    /// The basename -- the only identifying field allowed to be published. The
    /// issue body collapses anything path-shaped here to its basename as a
    /// second line of defence.
    #[serde(rename = "Name")]
    pub name: String,
    /// The media kind ("movie", "episode", ...). Local only: the published body
    /// carries Reason, Name, Attempts and the error, nothing else.
    #[serde(rename = "Kind")]
    pub kind: String,
    /// Why the file needs repair, normally one of the `REASON_*` values.
    #[serde(rename = "Reason")]
    pub reason: String,
    /// Counts completed repair attempts for this file.
    #[serde(rename = "Attempts")]
    pub attempts: i64,
    /// The raw provider error from the last attempt. It is sanitized (and
    /// truncated to 200 bytes) before publication.
    #[serde(rename = "LastError")]
    pub last_error: String,
    /// When the last attempt ran.
    #[serde(rename = "LastTry")]
    pub last_try: Time,
}

/// The whole needs-repair picture for one scan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// When the report was generated; stamped with now when left zero.
    #[serde(rename = "GeneratedAt")]
    pub generated_at: Time,
    /// The app version that produced it.
    #[serde(rename = "AppVersion")]
    pub app_version: String,
    /// The scan counts.
    #[serde(rename = "Totals")]
    pub totals: Totals,
    /// The files that need repair.
    #[serde(rename = "Entries", default)]
    pub entries: Vec<Entry>,
}

/// The scan count. `needs_repair` equals `entries.len()` after the extras are
/// excluded; the file partition is exact:
/// `files = indexed = resolved + needs_repair + extras + pinned + pending`,
/// where `pending` is every file not yet in one of the other buckets. `pinned`
/// is counted from the override store when the report is written; a file that
/// is latched is counted here ONCE, whatever else its record says. The report
/// does not enforce the arithmetic, it prints what the caller counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    /// Files seen by the scan.
    #[serde(rename = "Files")]
    pub files: i64,
    /// Files indexed.
    #[serde(rename = "Indexed")]
    pub indexed: i64,
    /// Files resolved to usable metadata.
    #[serde(rename = "Resolved")]
    pub resolved: i64,
    /// Files that need repair.
    #[serde(rename = "NeedsRepair")]
    pub needs_repair: i64,
    /// Files the parser classifies as extras (extras-folder leaves, themes,
    /// filename-level extras, unknown names): never fetched, never needs-repair.
    #[serde(rename = "Extras")]
    pub extras: i64,
    /// Files with a manual latch ("Fix match"): resolved from the override
    /// store, never fetched, never needs-repair.
    #[serde(rename = "Pinned")]
    pub pinned: i64,
    /// Files still awaiting a verdict: the remainder of the partition.
    #[serde(rename = "Pending")]
    pub pending: i64,
}

/// Writes the JSON (machine) and TXT (human) report into `dir` and returns their
/// paths. A zero `generated_at` is stamped with now.
///
/// Both files are staged as temps in `dir` and renamed into place, so a reader
/// never sees a half-written report. If either file cannot be staged or renamed,
/// no part of this call's output is left behind: the temps are removed and a JSON
/// renamed by this call is removed again, because a report that says "17 files
/// need repair" is worse than no report at all. A stale report from an earlier
/// run is therefore sacrificed on a failed write -- reports are regenerated on
/// demand.
pub fn write_report(dir: impl AsRef<Path>, r: &Report) -> Result<(PathBuf, PathBuf)> {
    let dir = dir.as_ref();
    if dir.as_os_str().is_empty() {
        return Err(Error::new("repair: WriteReport: empty dir"));
    }
    let mut r = r.clone();
    if r.generated_at.is_zero() {
        r.generated_at = Time::now();
    }
    fs::create_dir_all(dir).map_err(|e| Error::caused_by("repair: WriteReport", e))?;

    let json_path = dir.join(REPORT_JSON_NAME);
    let txt_path = dir.join(REPORT_TXT_NAME);

    let mut data = serde_json::to_vec_pretty(&r)
        .map_err(|e| Error::caused_by("repair: WriteReport: encode json", e))?;
    data.push(b'\n');

    let jtmp = stage_file(&json_path, &data)?;
    let ttmp = match stage_file(&txt_path, render_text(&r).as_bytes()) {
        Ok(t) => t,
        Err(e) => {
            let _ = fs::remove_file(&jtmp);
            return Err(e);
        }
    };
    if let Err(e) = fs::rename(&jtmp, &json_path) {
        let _ = fs::remove_file(&jtmp);
        let _ = fs::remove_file(&ttmp);
        return Err(Error::caused_by("repair: WriteReport: publish json", e));
    }
    if let Err(e) = fs::rename(&ttmp, &txt_path) {
        let _ = fs::remove_file(&ttmp);
        let _ = fs::remove_file(&json_path);
        return Err(Error::caused_by("repair: WriteReport: publish txt", e));
    }
    Ok((json_path, txt_path))
}

/// Reads back a report written by [`write_report`].
pub fn read_report(json_path: impl AsRef<Path>) -> Result<Report> {
    let json_path = json_path.as_ref();
    let b = fs::read(json_path).map_err(|e| Error::caused_by("repair: ReadReport", e))?;
    serde_json::from_slice(&b).map_err(|e| {
        Error::caused_by(
            format!("repair: ReadReport: parse {}", json_path.display()),
            e,
        )
    })
}

/// Writes `data` to a temp file beside `final_path` and returns the temp path.
/// The caller owns the temp file and must rename or remove it.
fn stage_file(final_path: &Path, data: &[u8]) -> Result<PathBuf> {
    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let base = final_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut last: Option<std::io::Error> = None;
    for _ in 0..16 {
        let tmp = dir.join(format!("{base}.{}.tmp", unique_suffix()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(mut f) => {
                let landed = f.write_all(data).and_then(|()| f.sync_all());
                drop(f);
                if let Err(e) = landed {
                    let _ = fs::remove_file(&tmp);
                    return Err(Error::caused_by(
                        format!("repair: WriteReport: write {base}"),
                        e,
                    ));
                }
                return Ok(tmp);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last = Some(e);
                continue;
            }
            Err(e) => {
                return Err(Error::caused_by(
                    format!("repair: WriteReport: stage {base}"),
                    e,
                ));
            }
        }
    }
    Err(Error::caused_by(
        format!("repair: WriteReport: stage {base}"),
        last.expect("a collision was recorded"),
    ))
}

/// A process-unique temp-file suffix (Go used `os.CreateTemp`'s random number).
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{n}-{t}", std::process::id())
}

/// The human summary: a header, then one line per entry.
fn render_text(r: &Report) -> String {
    let mut b = String::new();
    b.push_str("Potflix needs-repair report\n");
    let _ = writeln!(b, "generated:   {}", stamp(r.generated_at));
    let _ = writeln!(b, "app version: {}", one_line(&r.app_version));
    let _ = writeln!(
        b,
        "totals:      {} files, {} indexed, {} resolved, {} needs repair, {} extras, {} pinned, {} pending",
        r.totals.files,
        r.totals.indexed,
        r.totals.resolved,
        r.totals.needs_repair,
        r.totals.extras,
        r.totals.pinned,
        r.totals.pending
    );
    if r.entries.is_empty() {
        b.push_str("\nnothing needs repair.\n");
        return b;
    }
    b.push('\n');
    for e in &r.entries {
        let _ = write!(
            b,
            "{}  {}  attempts={}  local={}",
            reason_code(&e.reason),
            one_line(&e.name),
            e.attempts,
            one_line(&e.path)
        );
        let err_line = one_line(&e.last_error);
        if !err_line.is_empty() {
            let _ = write!(b, "  last={err_line}");
        }
        b.push('\n');
    }
    b
}

/// Summarizes the report for a GitHub issue title, e.g.
/// `needs-repair: 17 files with no metadata (12 no cover, 5 no title)`.
/// It is never empty.
pub fn issue_title(r: &Report) -> String {
    let n = r.entries.len();
    if n == 0 {
        return "needs-repair: no files need repair".to_string();
    }
    let mut title = format!("needs-repair: {n} {} with no metadata", plural(n, "file"));
    let parts = reason_breakdown(r);
    if !parts.is_empty() {
        title.push_str(" (");
        title.push_str(&parts.join(", "));
        title.push(')');
    }
    title
}

/// Renders the report as markdown. It contains only the app version, the
/// generation timestamp, the totals and, per entry, the reason, the basename, the
/// attempt count and a shortened error string -- every free-text field sanitized.
/// Callers may pass unsanitized entries; that is the point.
pub fn issue_body(r: &Report) -> String {
    issue_body_with(r, &crate::sanitize::identity_terms())
}

/// [`issue_body`] with the identity terms supplied instead of derived (the test
/// seam; see [`sanitize_error_with`]).
pub(crate) fn issue_body_with(r: &Report, terms: &[String]) -> String {
    let mut b = String::new();
    b.push_str("### Potflix needs-repair report\n\n");
    let _ = writeln!(
        b,
        "- app version: {}",
        code(&sanitize_error_with(&r.app_version, terms))
    );
    let _ = writeln!(b, "- generated: {}", stamp(r.generated_at));
    let _ = writeln!(
        b,
        "- totals: {} files, {} indexed, {} resolved, {} needs repair, {} extras, {} pinned, {} pending",
        r.totals.files,
        r.totals.indexed,
        r.totals.resolved,
        r.totals.needs_repair,
        r.totals.extras,
        r.totals.pinned,
        r.totals.pending
    );

    let mut shown = &r.entries[..];
    let mut hidden = 0;
    if shown.len() > MAX_ISSUE_ENTRIES {
        hidden = shown.len() - MAX_ISSUE_ENTRIES;
        shown = &shown[..MAX_ISSUE_ENTRIES];
    }
    if shown.is_empty() {
        return b;
    }
    b.push_str("\n| # | reason | file | attempts | last error |\n");
    b.push_str("|---|--------|------|----------|------------|\n");
    for (i, e) in shown.iter().enumerate() {
        let _ = writeln!(
            b,
            "| {} | {} | {} | {} | {} |",
            i + 1,
            cell(&reason_code(&e.reason)),
            cell(&basename_of(&one_line(&e.name))),
            e.attempts,
            truncate_bytes(
                &cell(&sanitize_error_with(&e.last_error, terms)),
                MAX_ERROR_BYTES
            ),
        );
    }
    if hidden > 0 {
        let _ = write!(b, "\nand {hidden} more\n");
    }
    b
}

/// Groups entries by reason, busiest first then alphabetical, so the title is
/// stable for the same set of entries.
fn reason_breakdown(r: &Report) -> Vec<String> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for e in &r.entries {
        *counts.entry(reason_code(&e.reason)).or_insert(0) += 1;
    }
    let mut keys: Vec<(String, usize)> = counts.into_iter().collect();
    keys.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    keys.into_iter()
        .map(|(k, c)| format!("{c} {}", k.replace('-', " ")))
        .collect()
}

/// The raw reason, defaulted when the caller left it empty.
fn reason_code(reason: &str) -> String {
    let r = one_line(reason);
    if r.is_empty() {
        "unspecified".to_string()
    } else {
        r
    }
}

/// Makes a string safe inside a markdown table cell.
fn cell(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Renders `s` as inline markdown code.
fn code(s: &str) -> String {
    format!("`{}`", s.replace('`', "'"))
}

fn stamp(t: Time) -> String {
    t.rfc3339()
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::test_support::{pad2, sample_report, terms, TempDir};

    fn pinned() -> Vec<String> {
        let user = format!("{}:{}", 'C', r"\Users\tester");
        terms(&["tester", user.as_str()])
    }

    #[test]
    fn TestWriteReportReadReportRoundTrip() {
        let dir = TempDir::new("round-trip");
        let want = sample_report();

        let (json_path, txt_path) = write_report(dir.path(), &want).expect("WriteReport");
        assert_eq!(json_path, dir.path().join(REPORT_JSON_NAME));
        assert_eq!(txt_path, dir.path().join(REPORT_TXT_NAME));

        let got = read_report(&json_path).expect("ReadReport");
        assert_eq!(got.generated_at, want.generated_at);
        assert_eq!(got.app_version, want.app_version);
        assert_eq!(got.totals, want.totals);
        assert_eq!(got.entries.len(), want.entries.len());
        for (i, (g, w)) in got.entries.iter().zip(want.entries.iter()).enumerate() {
            assert_eq!(g, w, "entry {i}");
        }

        let body = fs::read_to_string(&txt_path).expect("read txt");
        let entry_lines = body
            .trim_end_matches('\n')
            .lines()
            .filter(|ln| ln.contains("attempts="))
            .count();
        assert_eq!(entry_lines, want.entries.len(), "txt:\n{body}");
        for e in &want.entries {
            assert!(
                body.contains(&e.name),
                "txt is missing {:?}:\n{body}",
                e.name
            );
            assert!(
                body.contains(&e.path),
                "txt (local) is missing path {:?}:\n{body}",
                e.path
            );
        }
    }

    /// The rendered report must name the app version that produced it: the
    /// owner's report carried an empty `app version:` header, so the txt line
    /// and the JSON field are both pinned non-empty.
    #[test]
    fn TestWriteReportRenderCarriesNonEmptyAppVersion() {
        let dir = TempDir::new("app-version");
        let want = sample_report();
        let (json_path, txt_path) = write_report(dir.path(), &want).expect("WriteReport");

        let body = fs::read_to_string(&txt_path).expect("read txt");
        assert!(
            body.contains(&format!("app version: {}\n", want.app_version)),
            "txt lost the app version:\n{body}"
        );
        assert!(
            !body.contains("app version: \n") && !body.contains("app version:\n"),
            "txt carries an empty app version:\n{body}"
        );

        let got = read_report(&json_path).expect("ReadReport");
        assert!(
            !got.app_version.trim().is_empty(),
            "json carries an empty app version"
        );
        assert_eq!(got.app_version, want.app_version);
    }

    /// The totals line names every bucket of the exact partition the inventory
    /// writes: `files = indexed = resolved + needs_repair + extras + pinned +
    /// pending`. Extras and pinned are their own buckets and are never folded
    /// into `resolved`; the JSON carries the pinned count too.
    #[test]
    fn TestWriteReportTotalsLineCountsExtrasSeparately() {
        let dir = TempDir::new("totals");
        let want = sample_report();
        let (json_path, txt_path) = write_report(dir.path(), &want).expect("WriteReport");
        let body = fs::read_to_string(&txt_path).expect("read txt");
        assert!(
            body.contains(
                "totals:      40 files, 40 indexed, 19 resolved, 2 needs repair, 5 extras, 2 pinned, 12 pending\n"
            ),
            "txt totals line lost a bucket:\n{body}"
        );
        let got = read_report(&json_path).expect("ReadReport");
        assert_eq!(got.totals.pinned, 2, "json lost the pinned count");
        let wire: serde_json::Value =
            serde_json::from_slice(&fs::read(&json_path).expect("read json")).expect("wire json");
        assert_eq!(wire["Totals"]["Pinned"], serde_json::json!(2));
        let t = want.totals;
        assert_eq!(t.files, t.indexed, "files != indexed");
        assert_eq!(
            t.indexed,
            t.resolved + t.needs_repair + t.extras + t.pinned + t.pending,
            "the fixture is not a partition"
        );
    }

    /// The local txt/json are the owner's own list: every entry is written. The
    /// 50-entry cap belongs to the published issue body
    /// (`TestIssueBodyCapsEntriesAtFifty`) and must never reach these files.
    #[test]
    fn TestWriteReportLocalFormsCarryEveryEntry() {
        let dir = TempDir::new("uncapped");
        let mut r = sample_report();
        r.entries.clear();
        for i in 1..=80 {
            r.entries.push(Entry {
                path: format!("{}:{}{}{}.mkv", 'D', r"\1movies", r"\file-", pad2(i)),
                name: format!("file-{}.mkv", pad2(i)),
                reason: REASON_NO_COVER.to_string(),
                attempts: 1,
                ..Default::default()
            });
        }
        let (json_path, txt_path) = write_report(dir.path(), &r).expect("WriteReport");

        let got = read_report(&json_path).expect("ReadReport");
        assert_eq!(got.entries.len(), 80, "json capped the local report");

        let body = fs::read_to_string(&txt_path).expect("read txt");
        let lines = body.lines().filter(|ln| ln.contains("attempts=")).count();
        assert_eq!(lines, 80, "txt capped the local report:\n{body}");
        assert!(
            !body.contains("and 30 more"),
            "txt rendered the issue-body remainder line:\n{body}"
        );
        assert!(body.contains("file-80.mkv"), "txt lost the last entry");
    }

    /// A failed write must leave nothing behind: no half-written report, no temp
    /// file. The failure is forced by occupying the TXT target with a directory,
    /// which makes the second rename fail after the JSON rename has already
    /// happened.
    #[test]
    fn TestWriteReportFailedWriteLeavesNoPartialFile() {
        let dir = TempDir::new("failed-write");
        fs::create_dir(dir.path().join(REPORT_TXT_NAME)).expect("seed blocker");

        assert!(
            write_report(dir.path(), &sample_report()).is_err(),
            "WriteReport succeeded with the txt target blocked, want error"
        );
        assert!(
            !dir.path().join(REPORT_JSON_NAME).exists(),
            "json report left behind"
        );
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for n in &names {
            assert!(!n.contains(".tmp"), "temp file left behind: {n}");
        }
        assert_eq!(names, vec![REPORT_TXT_NAME.to_string()], "dir contents");
    }

    /// A directory that cannot be created (it is a file) fails before anything is
    /// staged, and the file it collided with is untouched.
    #[test]
    fn TestWriteReportUnwritableDir() {
        let parent = TempDir::new("unwritable-dir");
        let blocker = parent.path().join("not-a-dir");
        fs::write(&blocker, "keep me").expect("seed blocker");
        assert!(
            write_report(blocker.join("sub"), &sample_report()).is_err(),
            "WriteReport succeeded under a file path, want error"
        );
        assert_eq!(
            fs::read_to_string(&blocker).expect("read blocker"),
            "keep me"
        );
    }

    #[test]
    fn TestReadReportMissingOrBroken() {
        let dir = TempDir::new("missing-or-broken");
        assert!(
            read_report(dir.path().join("nope.json")).is_err(),
            "ReadReport on a missing file returned no error"
        );
        let broken = dir.path().join("broken.json");
        fs::write(&broken, "{not json").expect("write broken");
        assert!(
            read_report(&broken).is_err(),
            "ReadReport on broken json returned no error"
        );
    }

    /// The published body is the only thing that leaves the machine. This is the
    /// sanitization proof: paths shaped like a real user profile (a drive path
    /// with spaces, a path under a user profile) must not survive in any form, in
    /// any field, and the basename must.
    #[test]
    fn TestIssueBodyNeverCarriesALocalPath() {
        let mut r = sample_report();
        r.entries[1].name = format!("{}:{}", 'C', r"\Users\tester\Desktop\x.mkv");
        r.entries[0].last_error =
            format!("open {}:{}{}", 'D', r"\1movies\Some Folder\Some File.mkv", ": Access is denied");
        r.entries[1].last_error =
            format!("metadata: lookup failed for {}:{}", 'C', r"\Users\tester\Desktop\x.mkv");

        // Pinned identity (Go's pinIdentity) AND the live one: the body must be
        // safe whichever terms the machine supplies.
        for body in [issue_body_with(&r, &pinned()), issue_body(&r)] {
            assert!(
                !has_drive_letter_path(&body),
                "body carries a drive-letter path:\n{body}"
            );
            assert!(
                !has_profile_directory(&body),
                "body carries a profile directory:\n{body}"
            );
            for bad in [
                "tester",
                "\\",
                "/",
                "1movies",
                "Desktop",
                "Some Folder",
                "D:",
                "C:",
            ] {
                assert!(!body.contains(bad), "body carries {bad:?}:\n{body}");
            }
            for want in [
                "Some File.mkv",
                "x.mkv",
                "1.2.3",
                REASON_NO_COVER,
                REASON_NO_TITLE,
            ] {
                assert!(body.contains(want), "body is missing {want:?}:\n{body}");
            }
            for e in &r.entries {
                assert!(!body.contains(&e.path), "body carries a full path:\n{body}");
            }
        }
    }

    /// The Go regex `(?i)[a-z]:\\`, without a regex crate.
    fn has_drive_letter_path(s: &str) -> bool {
        let b = s.as_bytes();
        b.windows(3)
            .any(|w| w[0].is_ascii_alphabetic() && w[1] == b':' && w[2] == b'\\')
    }

    /// The Go regex `(?i)Users[\\/]`, without a regex crate.
    fn has_profile_directory(s: &str) -> bool {
        s.as_bytes()
            .windows(6)
            .any(|w| w[..5].eq_ignore_ascii_case(b"Users") && (w[5] == b'\\' || w[5] == b'/'))
    }

    #[test]
    fn TestIssueBodyCapsEntriesAtFifty() {
        let mut r = sample_report();
        r.entries.clear();
        for i in 1..=80 {
            r.entries.push(Entry {
                path: format!("{}:{}", 'D', r"\1movies\file.mkv"),
                name: format!("file-{}.mkv", pad2(i)),
                reason: REASON_NO_COVER.to_string(),
                attempts: 1,
                ..Default::default()
            });
        }
        let body = issue_body(&r);

        let rows = body
            .lines()
            .filter(|ln| {
                ln.strip_prefix("| ")
                    .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            })
            .count();
        assert_eq!(rows, 50, "entry rows");
        assert!(body.contains("and 30 more"), "missing the and-30-more line");
        assert!(
            body.contains("file-50.mkv") && !body.contains("file-51.mkv"),
            "cap boundary is wrong"
        );
    }

    #[test]
    fn TestIssueBodyTruncatesASingleErrorTo200Bytes() {
        let long = "z".repeat(300);
        let body = issue_body(&Report {
            entries: vec![Entry {
                name: "a.mkv".to_string(),
                reason: REASON_NO_COVER.to_string(),
                last_error: long,
                ..Default::default()
            }],
            ..Default::default()
        });
        assert!(
            body.contains(&format!("{}...", "z".repeat(197))),
            "expected a 200-byte truncated cell:\n{body}"
        );
        assert!(
            !body.contains(&"z".repeat(201)),
            "error was not truncated:\n{body}"
        );
    }

    #[test]
    fn TestIssueBodyZeroEntries() {
        let mut r = sample_report();
        r.entries.clear();
        let body = issue_body(&r);
        assert!(
            !body.contains("| # |") && !body.contains("more"),
            "empty report rendered a table or a remainder line:\n{body}"
        );
        assert!(
            body.contains(
                "totals: 40 files, 40 indexed, 19 resolved, 2 needs repair, 5 extras, 2 pinned, 12 pending"
            ),
            "empty report lost the totals:\n{body}"
        );
        assert!(
            body.contains("1.2.3"),
            "empty report lost the app version:\n{body}"
        );
    }

    #[test]
    fn TestIssueTitle() {
        let zero = issue_title(&Report::default());
        assert!(!zero.is_empty(), "IssueTitle returned an empty string");
        assert!(
            zero.contains("no files need repair"),
            "zero title = {zero:?}"
        );

        let one = issue_title(&Report {
            entries: vec![Entry {
                reason: REASON_NO_TITLE.to_string(),
                ..Default::default()
            }],
            ..Default::default()
        });
        assert_eq!(one, "needs-repair: 1 file with no metadata (1 no title)");

        let mut r = Report::default();
        for _ in 0..12 {
            r.entries.push(Entry {
                path: format!("{}:{}", 'D', r"\1movies\x.mkv"),
                name: "x.mkv".to_string(),
                reason: REASON_NO_COVER.to_string(),
                ..Default::default()
            });
        }
        for _ in 0..5 {
            r.entries.push(Entry {
                path: format!("{}:{}", 'C', r"\Users\tester\y.mkv"),
                name: "y.mkv".to_string(),
                reason: REASON_NO_TITLE.to_string(),
                ..Default::default()
            });
        }
        let got = issue_title(&r);
        assert_eq!(
            got,
            "needs-repair: 17 files with no metadata (12 no cover, 5 no title)"
        );
        assert!(
            !got.contains(['\\', '/']) && !got.to_lowercase().contains("tester"),
            "title carries a local path: {got:?}"
        );
    }

    #[test]
    fn TestIssueTitleUnsortedCounts() {
        let r = Report {
            entries: vec![
                Entry {
                    reason: REASON_NO_TITLE.to_string(),
                    ..Default::default()
                },
                Entry {
                    reason: REASON_NO_COVER.to_string(),
                    ..Default::default()
                },
                Entry {
                    reason: REASON_NO_COVER.to_string(),
                    ..Default::default()
                },
                Entry::default(),
            ],
            ..Default::default()
        };
        assert_eq!(
            issue_title(&r),
            "needs-repair: 4 files with no metadata (2 no cover, 1 no title, 1 unspecified)"
        );
    }
}
