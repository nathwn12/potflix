
#![allow(clippy::collapsible_if, clippy::collapsible_match)]

use std::io::{Write, stderr, stdout};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use metadata::Query;
use metadata_http::cancel::Cancel;
use std::sync::Arc;

const RATE_INTERVAL: Duration = Duration::from_millis(400);

const STATUS_SKIPPED: &str = "skipped";
const STATUS_DRY: &str = "dry";
const STATUS_RESOLVED: &str = "resolved";
const STATUS_NOT_FOUND: &str = "not-found";
const STATUS_ERROR: &str = "error";
const STATUS_CANCELLED: &str = "cancelled";

#[derive(Default)]
struct Config {
    roots: Vec<String>,
    cache_dir: String,
    dry: bool,
    offline: bool,
    limit: i32,
    concurrency: usize,
    gate: Duration,
    index_path: String,
}

fn usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "usage: metawarm -roots \"D:\\Movies;D:\\Shows\" [-cache <dir>] [-dry] [-offline] [-limit N] [-concurrency N] [-gate <dur>] [-index <file>]"
    );
}

fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s == "0" {
        return Some(Duration::ZERO);
    }
    let (num, unit) = if let Some(n) = s.strip_suffix("ms") {
        (n, "ms")
    } else if let Some(n) = s.strip_suffix('s') {
        (n, "s")
    } else if let Some(n) = s.strip_suffix('m') {
        (n, "m")
    } else if let Some(n) = s.strip_suffix('h') {
        (n, "h")
    } else {
        (s, "s")
    };
    let value: f64 = num.parse().ok()?;
    let base = match unit {
        "ms" => Duration::from_millis(1),
        "s" => Duration::from_secs(1),
        "m" => Duration::from_secs(60),
        _ => Duration::from_secs(3600),
    };
    Some(base.mul_f64(value))
}

fn parse_args(args: &[String], err: &mut dyn Write) -> Result<Config, String> {
    let mut c = Config {
        concurrency: 3,
        ..Config::default()
    };
    let mut roots_arg: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let stripped = arg.trim_start_matches('-');
        let (name, inline) = match stripped.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (stripped, None),
        };
        let take = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("missing value for -{name}"))
        };
        match name {
            "roots" => roots_arg = Some(take(&mut i)?),
            "cache" => c.cache_dir = take(&mut i)?,
            "dry" => c.dry = true,
            "offline" => c.offline = true,
            "limit" => {
                c.limit = take(&mut i)?
                    .parse()
                    .map_err(|_| "bad -limit".to_string())?
            }
            "concurrency" => {
                c.concurrency = take(&mut i)?
                    .parse()
                    .map_err(|_| "bad -concurrency".to_string())?
            }
            "gate" => {
                c.gate = parse_duration(&take(&mut i)?).ok_or_else(|| "bad -gate".to_string())?
            }
            "index" => c.index_path = take(&mut i)?,
            other => return Err(format!("unknown flag -{other}")),
        }
        i += 1;
    }
    if let Some(raw) = roots_arg {
        for r in raw.split(';') {
            let r = r.trim();
            if !r.is_empty() {
                c.roots.push(r.to_string());
            }
        }
    }
    if c.roots.is_empty() {
        usage(err);
        return Err("no roots: -roots is required".to_string());
    }
    if c.limit < 0 {
        return Err(format!("bad -limit {}: must be >= 0", c.limit));
    }
    if c.concurrency < 1 {
        return Err(format!("bad -concurrency {}: must be >= 1", c.concurrency));
    }
    Ok(c)
}

fn session_id() -> String {
    let id = std::env::var("POTFLIX_SESSION_ID").unwrap_or_default();
    let id = id.trim();
    if id.is_empty() {
        return "manual".to_string();
    }
    id.chars()
        .map(|r| match r {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect()
}

fn report_path() -> Result<String, String> {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let base = if base.is_empty() {
        match std::env::temp_dir().to_str() {
            Some(d) if !d.is_empty() => d.to_string(),
            _ => return Err("cannot resolve report dir: no LOCALAPPDATA".to_string()),
        }
    } else {
        base
    };
    Ok(format!(
        "{base}/Potflix/inventory/warm-report--hold--{}.txt",
        session_id()
    ))
}

fn write_report(
    path: &str,
    plans: &[library::FilePlan],
    status_of: &dyn Fn(&library::FilePlan) -> String,
) -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for p in plans {
        out.push_str(&status_of(p));
        out.push('\t');
        out.push_str(&p.path);
        out.push('\n');
    }
    std::fs::write(path, out)
}

#[derive(Default)]
struct Counts {
    files: i32,
    skipped: i32,
    unique: i32,
    dry: i32,
    resolved: i32,
    not_found: i32,
    errors: i32,
    denied: i64,
    folders_scanned: i64,
    folders_skipped: i64,
    keys: i32,
    keys_cached: i32,
    keys_fetched: i32,
    keys_not_found: i32,
    keys_failed: i32,
    cancelled: bool,
}

fn real_main(cancel: &Cancel, args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let cfg = match parse_args(args, err) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "metawarm: {e}");
            return 2;
        }
    };
    let use_index = !cfg.index_path.is_empty();
    let mut files: Vec<String>;
    let denied: i64;
    let mut f_scanned = 0;
    let mut f_skipped = 0;
    let mut visited: Vec<String> = Vec::new();
    #[allow(unused_mut)]
    let mut skipped: std::collections::HashMap<String, library::scanindex::FolderRecord> =
        std::collections::HashMap::new();
    let mut cancelled = false;
    if use_index && !cfg.dry {
        let (cr, walk_cancelled, e) = library::collect_indexed_with_context(
            cancel,
            &cfg.roots,
            &cfg.cache_dir,
            &cfg.index_path,
            true,
            false,
        );
        if let Some(e) = e {
            let _ = writeln!(err, "metawarm: {e}");
            return 1;
        }
        if walk_cancelled || cancel.err().is_some() {
            cancelled = true;
        }
        files = cr.files;
        denied = cr.denied;
        f_scanned = cr.folders_scanned;
        f_skipped = cr.folders_skipped;
        visited = cr.visited;
        skipped = cr.skipped;
        if cfg.limit > 0 && files.len() > cfg.limit as usize {
            files.truncate(cfg.limit as usize);
            visited.clear();
            skipped.clear();
        }
    } else {
        let (f, d, walk_cancelled, e) =
            library::collect_files_with_context(cancel, &cfg.roots, cfg.limit);
        if let Some(e) = e {
            let _ = writeln!(err, "metawarm: {e}");
            return 1;
        }
        files = f;
        denied = d;
        if walk_cancelled || cancel.err().is_some() {
            cancelled = true;
        }
    }
    let (plans, groups) = library::build_plan(&files);

    let mut c = Counts {
        files: plans.len() as i32,
        unique: groups.len() as i32,
        denied,
        folders_scanned: f_scanned,
        folders_skipped: f_skipped,
        keys: groups.len() as i32,
        ..Counts::default()
    };
    let mut pre_cached: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    if !cfg.dry {
        for g in &groups {
            if metadata::cache::cache_live(
                &cfg.cache_dir,
                &metadata::cache::cache_key_for(&g.query),
            ) {
                c.keys_cached += 1;
                pre_cached.insert(g.key.clone(), true);
            }
        }
    }
    if use_index && !cfg.dry && !visited.is_empty() && !cancelled && cancel.err().is_none() {
        library::record_scan(&cfg.index_path, &visited, &skipped, &plans);
    }
    let mut status_by_key: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if cfg.dry {
        for g in &groups {
            status_by_key.insert(g.key.clone(), STATUS_DRY.to_string());
        }
    } else {
        let resolver = metadata::Resolver::new(metadata::Options {
            dir: cfg.cache_dir.clone(),
            offline: cfg.offline,
            ..Default::default()
        });
        let gate_interval = if cfg.offline {
            Duration::ZERO
        } else {
            cfg.gate
        };
        let _ = RATE_INTERVAL;
        let last_ckpt = Mutex::new(Instant::now());
        let visited_ck = visited.clone();
        let skipped_ck = skipped.clone();
        let plans_ck = plans.clone();
        let index_path = cfg.index_path.clone();
        let lookup = |lctx: &Cancel, q: &Query| resolver.lookup_now(lctx, q);
        let lookup_ref: &library::LookupFn<'_> = &lookup;
        let on_done = |_g: &library::Group, _fr: library::FetchResult| {
            if !use_index || visited_ck.is_empty() {
                return;
            }
            let mut last = last_ckpt.lock().unwrap();
            if last.elapsed() < Duration::from_secs(2) {
                return;
            }
            *last = Instant::now();
            drop(last);
            library::record_scan(&index_path, &visited_ck, &skipped_ck, &plans_ck);
        };
        let on_done_ref: &library::OnDone<'_> = &on_done;
        let results = library::fetch_all(
            cancel,
            &groups,
            lookup_ref,
            cfg.concurrency,
            gate_interval,
            Some(on_done_ref),
        );
        for g in &groups {
            match results.get(&g.key) {
                None => {
                    if cancel.err().is_some() {
                        status_by_key.insert(g.key.clone(), STATUS_CANCELLED.to_string());
                    } else {
                        status_by_key.insert(g.key.clone(), STATUS_ERROR.to_string());
                    }
                }
                Some(r) => {
                    if let Some(_e) = &r.err {
                        if cancel.err().is_some() {
                            status_by_key.insert(g.key.clone(), STATUS_CANCELLED.to_string());
                        } else {
                            status_by_key.insert(g.key.clone(), STATUS_ERROR.to_string());
                        }
                    } else if r.res.found {
                        status_by_key.insert(g.key.clone(), STATUS_RESOLVED.to_string());
                    } else {
                        status_by_key.insert(g.key.clone(), STATUS_NOT_FOUND.to_string());
                    }
                }
            }
        }
        let prefetch: std::collections::HashMap<String, String> = status_by_key.clone();
        let counts = Arc::new(Mutex::new((0i32, 0i32, 0i32))); // fetched, notfound, failed
        for g in &groups {
            let st = prefetch.get(&g.key).map(|s| s.as_str()).unwrap_or("");
            match st {
                STATUS_RESOLVED => {
                    if !pre_cached.get(&g.key).copied().unwrap_or(false) {
                        let mut m = counts.lock().unwrap();
                        m.0 += 1;
                    }
                }
                STATUS_NOT_FOUND => {
                    if !pre_cached.get(&g.key).copied().unwrap_or(false) {
                        let mut m = counts.lock().unwrap();
                        m.1 += 1;
                    }
                }
                STATUS_ERROR => {
                    if !pre_cached.get(&g.key).copied().unwrap_or(false) {
                        let is_unavailable = results
                            .get(&g.key)
                            .and_then(|r| r.err.as_ref())
                            .is_some_and(|e| e.is_lookup_unavailable());
                        let mut m = counts.lock().unwrap();
                        if is_unavailable {
                            m.2 += 1;
                        } else {
                            m.1 += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        let m = counts.lock().unwrap();
        c.keys_fetched = m.0;
        c.keys_not_found = m.1;
        c.keys_failed = m.2;
        if cancel.err().is_some() {
            cancelled = true;
        }
        c.cancelled = cancelled;
    }
    if cancel.err().is_some() {
        cancelled = true;
        c.cancelled = true;
    }
    let status_of = |p: &library::FilePlan| -> String {
        if p.skipped {
            return STATUS_SKIPPED.to_string();
        }
        status_by_key
            .get(&p.key)
            .cloned()
            .unwrap_or_else(|| STATUS_CANCELLED.to_string())
    };
    for p in &plans {
        match status_of(p).as_str() {
            STATUS_SKIPPED => c.skipped += 1,
            STATUS_DRY => c.dry += 1,
            STATUS_RESOLVED => c.resolved += 1,
            STATUS_NOT_FOUND => c.not_found += 1,
            STATUS_ERROR => c.errors += 1,
            _ => {}
        }
    }
    let report = match report_path() {
        Ok(p) => p,
        Err(e) => {
            let _ = writeln!(err, "metawarm: {e}");
            return 1;
        }
    };
    if let Err(e) = write_report(&report, &plans, &status_of) {
        let _ = writeln!(err, "metawarm: {e}");
        return 1;
    }
    if use_index && !cfg.dry && !visited.is_empty() && !cancelled && cancel.err().is_none() {
        library::record_scan(&cfg.index_path, &visited, &skipped, &plans);
    }

    let mut b = String::new();
    use std::fmt::Write as _;
    let _ = write!(
        b,
        "metawarm summary: roots={} files={} skipped(unknown)={} unique={} denied={}",
        cfg.roots.len(),
        c.files,
        c.skipped,
        c.unique,
        c.denied
    );
    let _ = write!(
        b,
        " folders: {} scanned, {} skipped",
        c.folders_scanned, c.folders_skipped
    );
    let _ = write!(
        b,
        " keys: {} ({} cached, {} fetched, {} not found, {} failed)",
        c.keys, c.keys_cached, c.keys_fetched, c.keys_not_found, c.keys_failed
    );
    if cfg.dry {
        let _ = write!(b, " dry={}", c.dry);
    } else if cfg.offline {
        let _ = write!(b, " hits={} missing={}", c.resolved, c.not_found);
    } else {
        let _ = write!(b, " resolved={} not-found={}", c.resolved, c.not_found);
    }
    let _ = write!(
        b,
        " errors={} dry={} offline={}",
        c.errors, cfg.dry, cfg.offline
    );
    if cancelled {
        let _ = write!(b, " cancelled");
    }
    let _ = write!(b, "\nreport: {report}\n");
    let _ = out.write_all(b.as_bytes());
    0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cancel = Cancel::background();
    let code = real_main(&cancel, &args, &mut stdout(), &mut stderr());
    std::process::exit(code);
}
