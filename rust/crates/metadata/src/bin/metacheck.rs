
use std::process::ExitCode;
use std::time::Duration;

use metadata::resolver::{Options, Resolver};
use metadata::{Resolution, cache::default_cache_dir};
use metadata_core::context::resolve_query;
use metadata_core::parse::parse_raw;
use metadata_http::cancel::Cancel;

/// `lookupTimeout` bounds the online fetch across all providers.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(90);

/// `effectiveDir`: mirrors the resolver's default cache dir so the printed dir
/// is the dir actually used.
fn effective_dir(dir: &str) -> String {
    if dir.trim().is_empty() {
        default_cache_dir()
    } else {
        dir.to_string()
    }
}

/// `cacheSource`: scans the cache dir's legacy per-key `.json` files (kept in
/// place as the migration's recovery copy) for the entry holding `series` and
/// reports its provider name. `""` when the entry is absent.
fn cache_source(dir: &str, series: &str) -> String {
    if series.is_empty() {
        return String::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    #[derive(serde::Deserialize, Default)]
    struct MediaHead {
        #[serde(default)]
        source: String,
        #[serde(default)]
        title: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct RecordHead {
        #[serde(default)]
        media: MediaHead,
    }
    let mut fallback = String::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(".json") {
            continue;
        }
        let Ok(data) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<RecordHead>(&data) else {
            continue;
        };
        if record.media.title == series {
            return record.media.source;
        }
        if fallback.is_empty() && record.media.title.eq_ignore_ascii_case(series) {
            fallback = record.media.source;
        }
    }
    fallback
}

fn usage() {
    eprintln!("usage: metacheck [-offline] [-dir <cacheDir>] [-path <filePath>] <rawTitle>");
    eprintln!("  -offline  cache-only: never touch the network");
    eprintln!("  -dir      cache directory (default %LOCALAPPDATA%\\Potflix\\meta)");
    eprintln!("  -path     full path of the playing file (folder-aware ResolveQuery)");
}

fn main() -> ExitCode {
    let mut offline = false;
    let mut dir_flag = String::new();
    let mut path_flag = String::new();
    let mut positional: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-offline" => offline = true,
            "-dir" => match args.next() {
                Some(value) => dir_flag = value,
                None => {
                    usage();
                    return ExitCode::from(2);
                }
            },
            "-path" => match args.next() {
                Some(value) => path_flag = value,
                None => {
                    usage();
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                usage();
                return ExitCode::from(2);
            }
            other => positional.push(other.to_string()),
        }
    }
    if positional.len() != 1 {
        usage();
        return ExitCode::from(2);
    }
    let raw = positional.remove(0);
    let dir = effective_dir(&dir_flag);
    let mut q = parse_raw(&raw);
    if !path_flag.trim().is_empty() {
        q = resolve_query(&raw, &path_flag);
    }

    let res;
    let mut cache = "miss";
    if !offline {
        let probe = Resolver::new(Options {
            dir: dir.clone(),
            offline: true,
            ..Options::default()
        });
        match probe.lookup_now(&Cancel::background(), &q) {
            Ok(found) if found.found => {
                res = found;
                cache = "hit";
            }
            _ => {
                let cancel = Cancel::with_deadline(LOOKUP_TIMEOUT);
                let resolver = Resolver::new(Options {
                    dir: dir.clone(),
                    ..Options::default()
                });
                match resolver.lookup_now(&cancel, &q) {
                    Ok(found) => res = found,
                    Err(err) => {
                        eprintln!("metacheck: lookup: {err}");
                        print_line(&raw, &Resolution::default(), "", "miss", &dir);
                        return ExitCode::from(1);
                    }
                }
            }
        }
    } else {
        let probe = Resolver::new(Options {
            dir: dir.clone(),
            offline: true,
            ..Options::default()
        });
        match probe.lookup_now(&Cancel::background(), &q) {
            Ok(found) => {
                res = found;
                if res.found {
                    cache = "hit";
                }
            }
            Err(err) => {
                eprintln!("metacheck: offline lookup: {err}");
                print_line(&raw, &Resolution::default(), "", "miss", &dir);
                return ExitCode::from(1);
            }
        }
    }

    let source = if res.found {
        cache_source(&dir, &res.series)
    } else {
        String::new()
    };
    print_line(&raw, &res, &source, cache, &dir);
    if res.found {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn print_line(raw: &str, res: &Resolution, source: &str, cache: &str, dir: &str) {
    let buttons = res
        .buttons
        .iter()
        .map(|button| format!("{}|{}", button.label, button.url))
        .collect::<Vec<_>>()
        .join(",");
    println!(
        "input={raw} series={} season={} episode={} title={} source={source} cache={cache} dir={dir} genres={} length={} year={} buttons={buttons} art={} synopsis={}",
        res.series,
        res.season,
        res.number,
        res.episode,
        res.genres,
        res.length,
        res.year,
        res.artwork,
        res.synopsis
    );
}
