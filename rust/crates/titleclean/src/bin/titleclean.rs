
use std::fs;
use std::io::{BufWriter, Write};
use std::process::ExitCode;

use titleclean::{clean, java_dollar_replace_all, java_trim, Regex};

fn main() -> ExitCode {
    let mut corpus: Option<String> = None;
    let mut out: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let flag = arg.trim_start_matches('-');
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (flag.to_string(), None),
        };
        let value = match inline {
            Some(value) => value,
            None => match args.next() {
                Some(value) => value,
                None => return usage(),
            },
        };
        match name.as_str() {
            "corpus" => corpus = Some(value),
            "out" => out = Some(value),
            _ => return usage(),
        }
    }
    let (Some(corpus), Some(out)) = (corpus, out) else {
        return usage();
    };
    match run(&corpus, &out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: titleclean -corpus <in.txt> -out <out.tsv>");
    ExitCode::from(2)
}

fn run(corpus: &str, out: &str) -> Result<(), String> {
    let data = fs::read(corpus).map_err(|e| format!("read corpus: {e}"))?;
    let text =
        std::str::from_utf8(&data).map_err(|_| format!("corpus is not valid UTF-8: {corpus}"))?;
    let file = fs::File::create(out).map_err(|e| format!("create out: {e}"))?;
    let mut w = BufWriter::new(file);

    let pot_suffix = Regex::new(r" - PotPlayer\z");
    for line in split_lines(text) {
        let input = java_trim(line);
        if input.is_empty() {
            continue;
        }
        let c = clean(&java_dollar_replace_all(&pot_suffix, &input, ""));
        writeln!(
            w,
            "{input}\t{}\t{}\t{}\t{}",
            c.title, c.display, c.episode_tag, c.episode_name
        )
        .map_err(|e| format!("write out: {e}"))?;
    }
    w.flush().map_err(|e| format!("flush out: {e}"))?;
    Ok(())
}

/// Mirrors `java.nio.file.Files.readAllLines`: split on `\n`, `\r\n` or `\r`,
/// with no trailing empty element.
fn split_lines(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' || c == b'\r' {
            out.push(&s[start..i]);
            if c == b'\r' && i + 1 < b.len() && b[i + 1] == b'\n' {
                i += 1;
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < b.len() {
        out.push(&s[start..]);
    }
    out
}
