//! `repair::issue` -- mirrors `go\internal\repair\issue.go`.
//!
//! The GitHub issue path: `gh issue create` when there is a usable `gh`, else a
//! prefilled `https://github.com/<repo>/issues/new?title=...&body=...` URL the
//! owner can open in one click.
//!
//! Two rules the shape of this module exists to keep:
//!
//!   * A fallback is NOT a failure. With no `gh` (or no runner) the call returns
//!     the prefilled URL and no error -- nothing failed, there is simply no `gh`
//!     to run.
//!   * A runner FAILURE returns BOTH the prefilled URL and the error, so a caller
//!     can offer the manual path without hiding the failure.
//!
//! The `gh` executable name and the "is gh installed" answer are injectable
//! ([`Gh`]) so the tests exercise every branch -- including the real-`gh` branch
//! -- on a machine with no `gh` installed, and never execute one.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::report::{issue_body, issue_title, Report};
use crate::sanitize::one_line;
use crate::{Error, Result};

/// Runs `gh` and returns its stdout.
///
/// Go's `type Runner func(ctx context.Context, args ...string) (string, error)`,
/// minus the context (the crate has no async runtime): a caller supplies `gh`, a
/// stub, or nothing at all.
pub type Runner = Box<dyn Fn(&[String]) -> Result<String> + Send + Sync>;

/// The executable looked up and run when no runner is injected.
pub const DEFAULT_GH_BIN: &str = "gh";

/// The `gh` seams: the executable name and the "is gh on PATH" answer.
///
/// Go pinned the package variables `ghBin` and `hasGH` for this. Fields are the
/// Rust form of the same seam, per call instead of global: a test can pin a name
/// that cannot exist and answer "installed" without touching the machine.
#[derive(Debug, Default, Clone)]
pub struct Gh {
    /// The `gh` executable name. `None` means [`DEFAULT_GH_BIN`].
    pub bin: Option<String>,
    /// Whether a `gh` is on PATH. `None` probes PATH for the executable name.
    pub installed: Option<bool>,
}

impl Gh {
    fn bin_name(&self) -> String {
        self.bin
            .clone()
            .unwrap_or_else(|| DEFAULT_GH_BIN.to_string())
    }

    fn is_installed(&self) -> bool {
        match self.installed {
            Some(v) => v,
            None => has_gh_named(&self.bin_name()),
        }
    }
}

/// Configures issue creation.
#[derive(Default)]
pub struct Options {
    /// `owner/name`. Empty means "the repository gh is already in", which works
    /// for the gh path but cannot build a fallback URL.
    pub repo: String,
    /// The gh invocation. `None` uses a real gh when [`Gh::installed`] reports
    /// one is on PATH.
    pub runner: Option<Runner>,
    /// Attached in order as repeated `--label` flags.
    pub labels: Vec<String>,
    /// The `gh` seams.
    pub gh: Gh,
}

/// Reports whether a `gh` executable is on PATH. It looks the file up, it does
/// not run it and it never touches the network.
pub fn has_gh() -> bool {
    has_gh_named(DEFAULT_GH_BIN)
}

/// [`has_gh`] for an explicit executable name (Go's `ghBin` seam).
pub fn has_gh_named(bin: &str) -> bool {
    which(bin).is_some()
}

/// The PATH lookup `exec.LookPath` did, without running anything.
fn which(bin: &str) -> Option<PathBuf> {
    let path = Path::new(bin);
    if path.components().count() > 1 {
        return if path.is_file() {
            Some(path.to_path_buf())
        } else {
            None
        };
    }
    let path_var = std::env::var_os("PATH")?;
    let dirs = std::env::split_paths(&path_var);
    let mut exts: Vec<String> = vec![String::new()];
    if cfg!(windows) {
        let pathext =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        exts.extend(
            pathext
                .split(';')
                .filter(|s| !s.is_empty())
                .map(str::to_ascii_lowercase),
        );
    }
    for dir in dirs {
        for ext in &exts {
            let candidate = dir.join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Files the report as a GitHub issue and returns the issue URL.
///
/// With a usable runner (`opts.runner`, else a real `gh` on PATH) it runs
/// `gh issue create --repo <repo> --title <t> --body <b> [--label L]...` and
/// returns the URL gh printed. Without one it returns a prefilled
/// `https://github.com/<repo>/issues/new?title=...&body=...` URL and no error:
/// nothing failed, there is simply no `gh` to run.
///
/// When a runner exists and fails, both are returned: the prefilled URL and the
/// error, so the caller can offer the manual path without hiding the failure.
/// A request that carried label flags is retried the same way EXACTLY ONCE
/// without them (a repo that lacks one label rejects the whole create): the
/// retry's URL is returned while the error still carries the FIRST attempt's
/// cause text, so the operator learns why the labels were dropped. With no
/// label flags there is no retry, and when both attempts fail the error carries
/// both causes' text. The body is sanitized in either case ([`issue_body`]).
pub fn create_issue(r: &Report, opts: &Options) -> (String, Option<Error>) {
    let title = issue_title(r);
    let body = issue_body(r);
    let fallback = || issue_url(&opts.repo, &title, &body, &opts.labels);
    let args = issue_args(&opts.repo, &title, &body, &opts.labels);
    let labelled = args.iter().any(|a| a == "--label");
    let failure = |err: Error| -> (String, Option<Error>) {
        match fallback() {
            Ok(url) => (url, Some(Error::caused_by("repair: CreateIssue", err))),
            Err(ferr) => (
                String::new(),
                Some(Error::caused_by(
                    format!("repair: CreateIssue: {err} (no fallback url)"),
                    ferr,
                )),
            ),
        }
    };

    if opts.runner.is_none() && !opts.gh.is_installed() {
        // No gh, nothing failed: the prefilled URL and no error.
        return match fallback() {
            Ok(url) => (url, None),
            Err(e) => (String::new(), Some(e)),
        };
    }

    match run_attempt(opts, &args) {
        Err(first_err) if labelled => {
            let bare = issue_args(&opts.repo, &title, &body, &[]);
            match run_attempt(opts, &bare) {
                Ok(stdout) => {
                    let url = first_url(&stdout);
                    if url.is_empty() {
                        // The retry ran but printed no URL: nothing is known to
                        // have been filed, so keep the manual path and the FIRST
                        // cause.
                        return failure(Error::caused_by(
                            "repair: CreateIssue: retried without labels, gh printed no issue url",
                            first_err,
                        ));
                    }
                    (
                        url,
                        Some(Error::caused_by(
                            "repair: CreateIssue: filed without labels",
                            first_err,
                        )),
                    )
                }
                // No silent loss: the cause text carries BOTH attempts' text.
                Err(retry_err) => failure(Error::caused_by(
                    "repair: CreateIssue: retried without labels",
                    format!("{first_err}; retry: {retry_err}"),
                )),
            }
        }
        Err(err) => failure(err),
        Ok(stdout) => {
            let url = first_url(&stdout);
            if !url.is_empty() {
                return (url, None);
            }
            match fallback() {
                Ok(url) => (
                    url,
                    Some(Error::new("repair: CreateIssue: gh printed no issue url")),
                ),
                Err(_) => (
                    String::new(),
                    Some(Error::new(
                        "repair: CreateIssue: gh printed no issue url and there is no repo for a fallback url",
                    )),
                ),
            }
        }
    }
}

/// One create attempt: the injected runner when there is one, else the real
/// `gh`. The retry path reuses it so the two branches can never drift.
fn run_attempt(opts: &Options, args: &[String]) -> Result<String> {
    match &opts.runner {
        Some(runner) => runner(args),
        None => run_gh(&opts.gh.bin_name(), args),
    }
}

/// The argv handed to gh. It is built once so the injected-runner path and the
/// real-gh path can never drift apart.
fn issue_args(repo: &str, title: &str, body: &str, labels: &[String]) -> Vec<String> {
    let mut args = vec!["issue".to_string(), "create".to_string()];
    let r = repo.trim();
    if !r.is_empty() {
        args.push("--repo".to_string());
        args.push(r.to_string());
    }
    args.push("--title".to_string());
    args.push(title.to_string());
    args.push("--body".to_string());
    args.push(body.to_string());
    for l in labels {
        let s = l.trim();
        if !s.is_empty() {
            args.push("--label".to_string());
            args.push(s.to_string());
        }
    }
    args
}

/// The prefilled new-issue URL used when gh is unavailable.
fn issue_url(repo: &str, title: &str, body: &str, labels: &[String]) -> Result<String> {
    let mut r = repo.trim().trim_matches('/').to_string();
    if let Some(rest) = r.strip_prefix("https://github.com/") {
        r = rest.to_string();
    }
    if let Some(rest) = r.strip_prefix("github.com/") {
        r = rest.to_string();
    }
    if r.is_empty() {
        return Err(Error::new("repair: CreateIssue: empty repo"));
    }
    let mut pairs: Vec<(&str, &str)> = vec![("title", title), ("body", body)];
    let joined;
    if !labels.is_empty() {
        joined = labels.join(",");
        pairs.push(("labels", &joined));
    }
    // `url.Values.Encode`: keys sorted, each value escaped as a query component.
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let query: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", query_escape(v)))
        .collect();
    Ok(format!(
        "https://github.com/{r}/issues/new?{}",
        query.join("&")
    ))
}

/// Go's `url.QueryEscape` for the query component: alphanumerics and `-_.~`
/// survive, a space is `+`, everything else is `%XX` (uppercase), byte-wise.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b' ' => out.push('+'),
            _ if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The real [`Runner`]: it runs gh and returns its stdout.
fn run_gh(bin: &str, args: &[String]) -> Result<String> {
    let Some(first) = args.first() else {
        return Err(Error::new("repair: runGH: no args"));
    };
    match Command::new(bin).args(args).output() {
        Err(e) => Err(Error::caused_by(format!("repair: {bin} {first}"), e)),
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            let msg = one_line(&String::from_utf8_lossy(&out.stderr));
            let base = format!("repair: {bin} {first}: {}", out.status);
            Err(if msg.is_empty() {
                Error::new(base)
            } else {
                Error::caused_by(base, msg)
            })
        }
    }
}

/// Returns the first http(s) URL in `s` -- gh prints the created issue's URL on
/// its own line.
fn first_url(s: &str) -> String {
    for line in s.split('\n') {
        let line = line.trim();
        let mut i: Option<usize> = None;
        if let Some(j) = line.find("http://") {
            i = Some(j);
        }
        if let Some(j) = line.find("https://")
            && i.is_none_or(|k| j < k)
        {
            i = Some(j);
        }
        let Some(i) = i else { continue };
        let u = line[i..].trim_end_matches([' ', '\t', '\r']);
        let u = u.trim_end_matches(|c| ".,;:)]}\"'".contains(c));
        if !u.is_empty() {
            return u.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::test_support::{parse_query, sample_report};

    /// Go's `pinGH`: the "is gh installed" answer and the executable name, with
    /// the real gh never executed.
    fn gh(installed: bool, bin: &str) -> Gh {
        Gh {
            bin: Some(bin.to_string()),
            installed: Some(installed),
        }
    }

    /// A runner that records the argv it was handed and returns `stdout`.
    struct Capture {
        calls: Arc<Mutex<Vec<Vec<String>>>>,
        stdout: String,
    }

    impl Capture {
        fn new(stdout: &str) -> Capture {
            Capture {
                calls: Arc::new(Mutex::new(Vec::new())),
                stdout: stdout.to_string(),
            }
        }

        fn runner(&self) -> Runner {
            let seen = Arc::clone(&self.calls);
            let out = self.stdout.clone();
            Box::new(move |args: &[String]| {
                seen.lock().expect("lock").push(args.to_vec());
                Ok(out.clone())
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("lock").clone()
        }
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn TestCreateIssueRunnerArgvAndURL() {
        let r = sample_report();
        let capture = Capture::new(
            "Creating issue in nathwn12/potflix\nhttps://github.com/nathwn12/potflix/issues/12\n",
        );
        let opts = Options {
            repo: "nathwn12/potflix".to_string(),
            runner: Some(capture.runner()),
            labels: vec!["needs-repair".to_string(), "bug".to_string()],
            gh: gh(true, "gh"),
        };

        let (got, err) = create_issue(&r, &opts);
        assert!(err.is_none(), "CreateIssue: {err:?}");
        assert_eq!(got, "https://github.com/nathwn12/potflix/issues/12");

        let mut want = strings(&[
            "issue",
            "create",
            "--repo",
            "nathwn12/potflix",
            "--title",
        ]);
        want.push(issue_title(&r));
        want.push("--body".to_string());
        want.push(issue_body(&r));
        want.extend(strings(&["--label", "needs-repair", "--label", "bug"]));

        let calls = capture.calls();
        assert_eq!(calls.len(), 1, "runner called {} times, want 1", calls.len());
        assert_eq!(calls[0], want, "argv");
    }

    #[test]
    fn TestCreateIssueWithoutLabelsOmitsLabelFlags() {
        let r = sample_report();
        let capture = Capture::new("https://github.com/o/r/issues/1");
        let opts = Options {
            repo: "o/r".to_string(),
            runner: Some(capture.runner()),
            labels: Vec::new(),
            gh: Gh::default(),
        };
        let (_, err) = create_issue(&r, &opts);
        assert!(err.is_none(), "CreateIssue: {err:?}");

        let calls = capture.calls();
        let got = &calls[0];
        assert!(
            !got.iter().any(|a| a == "--label"),
            "argv has an unexpected --label: {got:?}"
        );
        let mut want = strings(&["issue", "create", "--repo", "o/r", "--title"]);
        want.push(issue_title(&r));
        want.push("--body".to_string());
        want.push(issue_body(&r));
        assert_eq!(*got, want, "argv");
    }

    /// The no-gh path is deterministic: the seam says gh is missing, so
    /// CreateIssue must return a prefilled URL and no error, whether or not gh
    /// really exists.
    #[test]
    fn TestCreateIssueFallsBackToAPrefilledURL() {
        let r = sample_report();
        let opts = Options {
            repo: "nathwn12/potflix".to_string(),
            gh: gh(false, "gh"),
            ..Default::default()
        };
        let (got, err) = create_issue(&r, &opts);
        assert!(err.is_none(), "CreateIssue: {err:?}");

        let (base, q) = parse_query(&got);
        assert_eq!(base, "https://github.com/nathwn12/potflix/issues/new");
        assert_eq!(q.get("title").map(String::as_str), Some(issue_title(&r).as_str()));
        assert_eq!(q.get("body").map(String::as_str), Some(issue_body(&r).as_str()));
        let body = q.get("body").map(String::as_str).unwrap_or_default();
        assert!(
            !body.contains('\\'),
            "fallback body carries a separator:\n{body}"
        );
        assert_eq!(q.get("labels"), None, "labels must be absent");
    }

    #[test]
    fn TestCreateIssueFallbackCarriesLabels() {
        let opts = Options {
            repo: "o/r".to_string(),
            labels: vec!["needs-repair".to_string(), "bug".to_string()],
            gh: gh(false, "gh"),
            ..Default::default()
        };
        let (got, err) = create_issue(&sample_report(), &opts);
        assert!(err.is_none(), "CreateIssue: {err:?}");
        let (_, q) = parse_query(&got);
        assert_eq!(
            q.get("labels").map(String::as_str),
            Some("needs-repair,bug")
        );
    }

    #[test]
    fn TestCreateIssueEmptyRepoWithoutGHIsAnError() {
        let opts = Options {
            gh: gh(false, "gh"),
            ..Default::default()
        };
        let (_, err) = create_issue(&sample_report(), &opts);
        assert!(
            err.is_some(),
            "CreateIssue with no repo and no gh returned no error"
        );
    }

    /// A runner that fails still hands back the prefilled URL, alongside the
    /// error. With no label flags there is no retry: exactly one attempt.
    #[test]
    fn TestCreateIssueRunnerFailureReturnsFallbackAndError() {
        const BOOM: &str = "gh: not logged in";
        let calls = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&calls);
        let runner: Runner = Box::new(move |_args: &[String]| -> Result<String> {
            *seen.lock().expect("lock") += 1;
            Err(Error::new(BOOM))
        });
        let opts = Options {
            repo: "o/r".to_string(),
            runner: Some(runner),
            gh: gh(true, "gh"),
            ..Default::default()
        };

        let (got, err) = create_issue(&sample_report(), &opts);
        let err = err.expect("want the runner's failure");
        assert_eq!(
            err.cause_text(),
            Some(BOOM),
            "errors.Is(err, boom): err = {err}"
        );
        assert!(
            got.starts_with("https://github.com/o/r/issues/new?"),
            "url = {got:?}, want a prefilled fallback"
        );
        assert_eq!(*calls.lock().expect("lock"), 1, "no labels, no retry");
    }

    /// A label gh cannot attach fails the first create; the retry omits the
    /// label flags, its URL is returned, and the error still names the first
    /// failure.
    #[test]
    fn TestCreateIssueRetriesOnceWithoutLabels() {
        let r = sample_report();
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let runner: Runner = Box::new(move |args: &[String]| {
            let n = {
                let mut g = seen.lock().expect("lock");
                g.push(args.to_vec());
                g.len()
            };
            if n == 1 {
                Err(Error::caused_by(
                    "repair: gh issue: exit status 1",
                    "could not add label: 'needs-repair' not found",
                ))
            } else {
                Ok("https://github.com/o/r/issues/7\n".to_string())
            }
        });
        let opts = Options {
            repo: "o/r".to_string(),
            runner: Some(runner),
            labels: vec!["needs-repair".to_string()],
            gh: gh(true, "gh"),
        };

        let (got, err) = create_issue(&r, &opts);
        assert_eq!(got, "https://github.com/o/r/issues/7");
        let err = err.expect("want the first attempt's failure kept");
        let cause = err.cause_text().unwrap_or_default();
        assert!(
            cause.contains("could not add label: 'needs-repair' not found"),
            "cause_text = {cause:?}"
        );

        let calls = calls.lock().expect("lock").clone();
        assert_eq!(calls.len(), 2, "want exactly one retry");
        assert_eq!(
            calls[0],
            issue_args(
                "o/r",
                &issue_title(&r),
                &issue_body(&r),
                &strings(&["needs-repair"])
            ),
            "first attempt argv"
        );
        assert_eq!(
            calls[1],
            issue_args("o/r", &issue_title(&r), &issue_body(&r), &[]),
            "retry argv must be the same create with the labels omitted"
        );
        assert!(
            !calls[1].iter().any(|a| a == "--label"),
            "retry argv has --label: {:?}",
            calls[1]
        );
    }

    /// Both attempts failing loses nothing: the returned error's cause text
    /// carries both causes.
    #[test]
    fn TestCreateIssueBothAttemptsFailCarriesBothCauses() {
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&calls);
        let runner: Runner = Box::new(move |args: &[String]| {
            let n = {
                let mut g = seen.lock().expect("lock");
                g.push(args.to_vec());
                g.len()
            };
            Err(Error::caused_by(
                format!("repair: gh issue attempt {n}"),
                if n == 1 {
                    "could not add label: 'needs-repair' not found"
                } else {
                    "could not create issue: 500 server error"
                },
            ))
        });
        let opts = Options {
            repo: "o/r".to_string(),
            runner: Some(runner),
            labels: vec!["needs-repair".to_string()],
            gh: gh(true, "gh"),
        };

        let (got, err) = create_issue(&sample_report(), &opts);
        assert!(
            got.starts_with("https://github.com/o/r/issues/new?"),
            "url = {got:?}, want a prefilled fallback"
        );
        let err = err.expect("want an error");
        let cause = err.cause_text().unwrap_or_default();
        assert!(
            cause.contains("could not add label: 'needs-repair' not found"),
            "first cause lost: {cause:?}"
        );
        assert!(
            cause.contains("could not create issue: 500 server error"),
            "retry cause lost: {cause:?}"
        );
        assert_eq!(
            calls.lock().expect("lock").len(),
            2,
            "want exactly one retry"
        );
    }

    /// With no injected runner and a gh name that cannot exist, the real-gh
    /// branch runs, the spawn fails, and the call degrades to the fallback. No gh
    /// process is ever started.
    #[test]
    fn TestCreateIssueRealGHBranchDegradesWhenGHIsMissing() {
        let opts = Options {
            repo: "o/r".to_string(),
            gh: gh(true, "potflix-no-such-gh-binary-TestCreateIssueRealGHBranch"),
            ..Default::default()
        };
        let (got, err) = create_issue(&sample_report(), &opts);
        assert!(err.is_some(), "want an error from the failed gh exec");
        assert!(
            got.starts_with("https://github.com/o/r/issues/new?"),
            "url = {got:?}, want a prefilled fallback"
        );
    }

    #[test]
    fn TestFirstURL() {
        let cases: &[(&str, &str)] = &[
            (
                "https://github.com/o/r/issues/3\n",
                "https://github.com/o/r/issues/3",
            ),
            (
                "Creating issue in o/r\nhttps://github.com/o/r/1\n",
                "https://github.com/o/r/1",
            ),
            ("no url here", ""),
            ("", ""),
            (
                "see (https://github.com/o/r/issues/9).",
                "https://github.com/o/r/issues/9",
            ),
            ("http://example.test/x", "http://example.test/x"),
        ];
        for (input, want) in cases {
            assert_eq!(first_url(input), *want, "firstURL({input:?})");
        }
    }

    #[test]
    fn TestHasGHDoesNotPanic() {
        assert!(
            !has_gh_named("potflix-no-such-gh-binary"),
            "HasGH found a binary that cannot exist"
        );
        let _ = has_gh(); // must not panic either
    }
}
