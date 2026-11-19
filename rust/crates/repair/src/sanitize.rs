
use std::fmt::Write as _;

/// Flattens any whitespace run to a single space and trims the ends, so a
/// multi-line provider error can never break "one line per entry" or a markdown
/// table row. Go keeps this in `report.go`.
pub(crate) fn one_line(s: &str) -> String {
    let mut b = String::with_capacity(s.len());
    let mut pending = false;
    for r in s.chars() {
        if r.is_whitespace() {
            pending = true;
            continue;
        }
        if pending && !b.is_empty() {
            b.push(' ');
        }
        pending = false;
        b.push(r);
    }
    b
}

/// [`sanitize_error`], with the identity terms supplied instead of derived from
/// the machine.
///
/// Go pinned `identityTermSource` as a package variable for this. An explicit
/// argument is the Rust form of the same seam: the tests stay deterministic (a
/// username that happens to collide with a fixture word cannot change the
/// expected output) with no global mutable state in the library.
pub(crate) fn sanitize_error_with(s: &str, terms: &[String]) -> String {
    if s.is_empty() {
        return String::new();
    }
    let s = one_line(s);
    let s = collapse_paths(&s);
    let s = redact_terms(&s, terms);
    redact_id_shaped_digits(&s)
}

/// The published-text filter -- the crate's public entry point.
pub fn sanitize_error(s: &str) -> String {
    sanitize_error_with(s, &identity_terms())
}

/// The machine identity: the Windows account name, the Unix user, and the home
/// directory.
///
/// Go called `user.Current()` for the account and the home directory. `std` has
/// no user database, so the same two values are rebuilt from the environment:
/// `USERPROFILE`/`HOME` are the home directory, and a domain-qualified account
/// (`DOMAIN\user`) is assembled from `USERDOMAIN` + `USERNAME` beside the bare
/// account, exactly like the Go branch that adds `u.Username` and the account
/// after the domain separator.
pub(crate) fn identity_terms() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |v: &str| {
        let t = v.trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    };
    add(&env("USERNAME"));
    add(&env("USER"));
    add(&env("USERPROFILE"));
    add(&env("HOME"));
    let user = env("USERNAME");
    let domain = env("USERDOMAIN");
    if !user.is_empty() && !domain.is_empty() {
        add(&format!("{domain}\\{user}"));
    }
    if let Some(i) = user.rfind(['\\', '/']) {
        add(&user[i + 1..]);
    }
    sort_terms_longest_first(&mut out);
    out
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// Longest first: the home directory before the bare account name, so
/// `C:\Users\tester` is replaced as a whole and never leaves `C:\Users\` behind.
fn sort_terms_longest_first(terms: &mut [String]) {
    terms.sort_by_key(|t| std::cmp::Reverse(t.len()));
}

/// The shortest identity term worth redacting; below it a term is too likely to
/// appear inside unrelated words.
pub(crate) const MIN_REDACT_TERM: usize = 3;

/// Replaces each term, case-insensitively, with `<redacted>`.
///
/// Go compiled `(?i)QuoteMeta(term)`. There is no regex crate in the dependency
/// graph, and none is needed: the term is matched literally, char by char, with
/// a case-folded comparison.
pub(crate) fn redact_terms(s: &str, terms: &[String]) -> String {
    let mut out = s.to_string();
    for term in terms {
        if term.chars().count() < MIN_REDACT_TERM {
            continue;
        }
        out = replace_folded(&out, term, "<redacted>");
    }
    out
}

fn replace_folded(hay: &str, needle: &str, repl: &str) -> String {
    let ndl: Vec<char> = needle.chars().collect();
    let h: Vec<char> = hay.chars().collect();
    let mut out = String::with_capacity(hay.len());
    let mut i = 0;
    while i < h.len() {
        if i + ndl.len() <= h.len()
            && ndl
                .iter()
                .enumerate()
                .all(|(k, n)| chars_eq_folded(h[i + k], *n))
        {
            out.push_str(repl);
            i += ndl.len();
        } else {
            out.push(h[i]);
            i += 1;
        }
    }
    out
}

/// One char compared to another, case-insensitively (`(?i)` without full
/// multi-char folding).
fn chars_eq_folded(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Matches a Discord snowflake: 17-20 digits standing alone.
///
/// The client id must never be published; this is the belt to the structural
/// braces (the report simply has no field that carries it). A bare
/// unix-nanosecond timestamp is the same shape and is redacted with it --
/// harmless inside error text, and cheaper than a leak.
pub(crate) fn redact_id_shaped_digits(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let left_ok = start == 0 || !is_word_byte(b[start - 1]);
        let right_ok = i == b.len() || !is_word_byte(b[i]);
        if (17..=20).contains(&(i - start)) && left_ok && right_ok {
            out.push_str(&s[copied..start]);
            out.push_str("<id>");
            copied = i;
        }
    }
    out.push_str(&s[copied..]);
    out
}

/// Go's ASCII `\b` word byte (`[0-9A-Za-z_]`); non-ASCII is never a word char.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Rewrites every absolute local path inside `s` to its basename.
///
/// Recognised starts: a drive-letter path (`C:\` or `C:/`), a UNC or
/// extended-length path (`\\server\share`, `\\?\C:\`), and a POSIX absolute
/// path. A start is only honoured at a word boundary, which keeps URLs such as
/// `https://host/a/b` untouched.
fn collapse_paths(s: &str) -> String {
    if s.is_empty() || (!s.contains(['\\', '/']) && !s.contains(':')) {
        return s.to_string();
    }
    let mut b = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(span) = path_span(s, i) {
            b.push_str(&basename_of(span));
            i += span.len();
            continue;
        }
        let c = s[i..].chars().next().expect("index is a char boundary");
        b.push(c);
        i += c.len_utf8();
    }
    b
}

/// The path starting at byte `i`, if one starts there.
fn path_span(s: &str, i: usize) -> Option<&str> {
    if !path_start(s, i) {
        return None;
    }
    let end = consume_path(s, i);
    if end <= i {
        return None;
    }
    Some(&s[i..end])
}

/// Whether a path begins at byte `i`.
fn path_start(s: &str, i: usize) -> bool {
    let b = s.as_bytes();
    if i >= b.len() {
        return false;
    }
    if i > 0 && is_path_byte(b[i - 1]) {
        return false; // mid-word: not the start of a path
    }
    match () {
        _ if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'\\' => true, // \\server\share or \\?\C:\
        _ if b[i] == b'/' && i + 1 < b.len() && !is_sep(b[i + 1]) && is_path_byte(b[i + 1]) => {
            true // /home/user/x, but never the // of a URL
        }
        _ if b[i] == b'\\' && i + 1 < b.len() && !is_sep(b[i + 1]) && is_path_byte(b[i + 1]) => {
            true // \Users\tester\x.mkv - rooted without a drive
        }
        _ if i + 2 < b.len() && is_drive_letter(b[i]) && b[i + 1] == b':' && is_sep(b[i + 2]) => {
            true // C:\ or C:/
        }
        _ => false,
    }
}

/// The end byte of the path starting at `i`.
///
/// The first word is always taken; a following space only continues the path
/// when the next word still looks like a path part (it carries a separator or a
/// dot). That captures `D:\My Movies\Some File.mkv` and still stops at
/// `while reading` in `open D:\a\b.mkv while reading`.
fn consume_path(s: &str, i: usize) -> usize {
    let b = s.as_bytes();
    let mut j = i;
    if b[j] == b'\\' && j + 1 < b.len() && b[j + 1] == b'\\' {
        j += 2;
        if j + 1 < b.len() && b[j] == b'?' && is_sep(b[j + 1]) {
            j += 2; // \\?\
        }
        if j + 2 < b.len() && is_drive_letter(b[j]) && b[j + 1] == b':' && is_sep(b[j + 2]) {
            j += 3; // \\?\C:\ - the drive would otherwise be left behind
        }
    } else if is_sep(b[j]) {
        j += 1; // a POSIX root, or the single leading \ of \Users\...
    } else if is_drive_letter(b[j]) && j + 2 < b.len() && b[j + 1] == b':' && is_sep(b[j + 2]) {
        j += 3;
    }

    let mut first = true;
    loop {
        let word_start = j;
        j = consume_word(s, j);
        if j == word_start {
            if first && j > i {
                return j;
            }
            break;
        }
        first = false;
        let mut k = j;
        while k < b.len() && b[k] == b' ' {
            k += 1;
        }
        if k == j || k >= b.len() || !looks_like_path_word(s, k) {
            break;
        }
        j = k;
    }
    j
}

/// The end of the run of path bytes starting at `i`.
///
/// Go stopped here on an invalid UTF-8 byte so it never swallowed one; Rust's
/// `&str` cannot carry one, so the run simply follows the runes.
fn consume_word(s: &str, mut i: usize) -> usize {
    while i < s.len() {
        let r = s[i..].chars().next().expect("index is a char boundary");
        if !is_path_rune(r) {
            return i;
        }
        i += r.len_utf8();
    }
    i
}

/// Whether the word at `k` reads as a further path part rather than the prose
/// that follows one.
fn looks_like_path_word(s: &str, k: usize) -> bool {
    let w = &s[k..consume_word(s, k)];
    if w.is_empty() {
        return false;
    }
    if w.contains(['\\', '/']) {
        return true;
    }
    w.contains('.') && !w.starts_with('.')
}

/// Reduces `p` to its last element, dropping separators, any drive or UNC head
/// and trailing punctuation, so `D:\a\b.mkv` is `b.mkv` and `(see D:\a\b.mkv)`
/// does not publish `b.mkv)`.
pub(crate) fn basename_of(p: &str) -> String {
    let mut p = p.trim_end_matches(['\\', '/']);
    if let Some(rest) = p.strip_prefix("\\\\?\\") {
        p = rest;
    }
    if let Some(rest) = p.strip_prefix("\\\\") {
        p = rest;
    }
    let b = p.as_bytes();
    if b.len() >= 2 && is_drive_letter(b[0]) && b[1] == b':' {
        p = p[2..].trim_start_matches(['\\', '/']);
    }
    if let Some(i) = p.rfind(['\\', '/']) {
        p = &p[i + 1..];
    }
    let p = p.trim_end_matches(|c| ".,;:)]}'\"!?".contains(c));
    if p.is_empty() {
        "<path>".to_string() // a bare root, e.g. "C:\" or "\\server"
    } else {
        p.to_string()
    }
}

/// Whether `b` is a path separator.
fn is_sep(b: u8) -> bool {
    b == b'\\' || b == b'/'
}

fn is_drive_letter(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// Whether `b` may sit inside a path word. Separators are included, so
/// `1movies\Some` is one word.
fn is_path_byte(b: u8) -> bool {
    if b >= 0x80 {
        return true;
    }
    if b.is_ascii_alphanumeric() {
        return true;
    }
    matches!(
        b,
        b'.' | b'_' | b'-' | b'~' | b'+' | b'(' | b')' | b'[' | b']' | b'\\' | b'/'
    )
}

/// [`is_path_byte`] for a decoded rune; non-ASCII letters and digits are path
/// bytes so non-Latin file names survive.
///
/// Named reading: Go used `unicode.IsLetter || unicode.IsDigit`, and `IsDigit` is
/// the Nd category only, while `char::is_numeric` is Nd+Nl+No. The difference is
/// a filename whose rune is a superscript or a Roman numeral -- not a path
/// separator either way, so the span is identical.
fn is_path_rune(r: char) -> bool {
    if (r as u32) < 0x80 {
        return is_path_byte(r as u8);
    }
    r.is_alphabetic() || r.is_numeric()
}

/// Shortens `s` to at most `n` bytes, never splitting a rune, and marks the cut
/// with `...`.
pub(crate) fn truncate_bytes(s: &str, n: usize) -> String {
    if n == 0 || s.len() <= n {
        return s.to_string();
    }
    let (suffix, mut cut) = if n > 3 { ("...", n - 3) } else { ("", n) };
    let b = s.as_bytes();
    while cut > 0 && !is_rune_start(b[cut]) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + suffix.len());
    let _ = write!(out, "{}{suffix}", &s[..cut]);
    out
}

/// Whether `b` starts a UTF-8 sequence (`utf8.RuneStart`): anything that is not
/// a continuation byte (`0x80..0xC0`).
fn is_rune_start(b: u8) -> bool {
    !(0x80..0xC0).contains(&b)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::test_support::terms;

    /// Go's `pinIdentity`: the terms are supplied verbatim, in the order given
    /// (the two-argument pins are NOT sorted -- the real source sorts itself).
    #[test]
    fn TestSanitizeErrorStripsAbsolutePaths() {
        let pinned = terms(&["tester", "C:\\Users\\tester"]);
        let cases: &[(&str, &str, &str)] = &[
            (
                "drive path with spaces",
                "open D:\\1movies\\Some Folder\\Some File.mkv: Access is denied",
                "open Some File.mkv: Access is denied",
            ),
            (
                "drive path under a user profile",
                "stat C:\\Users\\tester\\Desktop\\x.mkv: file does not exist",
                "stat x.mkv: file does not exist",
            ),
            (
                "forward slashes",
                "C:/Users/tester/Desktop/x.mkv is unreadable",
                "x.mkv is unreadable",
            ),
            (
                "unc share",
                "\\\\media-server\\movies\\No Cover.mkv missing",
                "No Cover.mkv missing",
            ),
            (
                "extended length path",
                "\\\\?\\C:\\very\\long\\path\\x.mkv",
                "x.mkv",
            ),
            (
                "drive-less rooted path",
                "read \\Users\\tester\\x.mkv failed",
                "read x.mkv failed",
            ),
            (
                "quoted path",
                "open \"D:\\My Movies\\A File.mkv\" failed",
                "open \"A File.mkv\" failed",
            ),
            (
                "posix path",
                "/home/tester/media/x.mkv: not found",
                "x.mkv: not found",
            ),
            (
                "prose after a path survives",
                "cannot open D:\\a\\b.mkv while reading",
                "cannot open b.mkv while reading",
            ),
            (
                "url is not a path",
                "GET https://api.example.com/v2/movie/123 failed",
                "GET https://api.example.com/v2/movie/123 failed",
            ),
            (
                "prose with a colon is untouched",
                "metadata: not found (404) at 22:30:45",
                "metadata: not found (404) at 22:30:45",
            ),
            (
                "path holds the basename only",
                "D:\\1movies\\Some Folder\\Some File.mkv",
                "Some File.mkv",
            ),
        ];
        for (name, input, want) in cases {
            let got = sanitize_error_with(input, &pinned);
            assert_eq!(got, *want, "case {name}: SanitizeError({input:?})");
        }
    }

    #[test]
    fn TestSanitizeErrorNeverLeavesAPublishablePath() {
        let pinned = terms(&["tester", "C:\\Users\\tester"]);
        let inputs = [
            "open D:\\1movies\\Some Folder\\Some File.mkv: Access is denied",
            "C:\\Users\\tester\\Desktop\\x.mkv",
            "\\\\?\\C:\\very\\long\\path\\x.mkv",
            "\\\\media-server\\movies\\No Cover.mkv",
            "error for /home/tester/x.mkv and \\Users\\tester\\y.mkv",
        ];
        for input in inputs {
            let got = sanitize_error_with(input, &pinned);
            assert!(
                !got.contains('\\') && !got.contains('/'),
                "SanitizeError({input:?}) kept a separator: {got:?}"
            );
            assert!(
                !got.to_lowercase().contains("users"),
                "SanitizeError({input:?}) kept a directory name: {got:?}"
            );
            assert!(
                !got.to_lowercase().contains("tester"),
                "SanitizeError({input:?}) kept the account name: {got:?}"
            );
        }
    }

    #[test]
    fn TestSanitizeErrorRedactsIdentityTermsAndIDs() {
        let pinned = terms(&["tester", "C:\\Users\\tester"]);
        let got = sanitize_error_with(
            "repair tester\\AppData for tester (client 1234567890123456789) failed",
            &pinned,
        );
        assert!(!got.contains("tester"), "account name survived: {got:?}");
        assert!(
            !got.contains("1234567890123456789"),
            "id-shaped digits survived: {got:?}"
        );
        assert!(
            got.contains("<redacted>") && got.contains("<id>"),
            "expected redaction markers, got {got:?}"
        );
    }

    /// The pipeline order: the whitespace flatten runs BEFORE the path collapse,
    /// so a path split by a newline is still one path.
    #[test]
    fn TestSanitizeErrorCollapsesPathsAfterFlatteningWhitespace() {
        let pinned = terms(&["tester"]);
        let got = sanitize_error_with("open D:\\My\nMovies\\x.mkv failed", &pinned);
        assert_eq!(got, "open x.mkv failed");
    }

    /// The identity terms are sorted longest-first, so the home directory is
    /// replaced as a whole instead of leaving `C:\Users\` behind.
    #[test]
    fn TestIdentityTermsSortLongestFirst() {
        let mut t = vec![
            "tester".to_string(),
            "C:\\Users\\tester".to_string(),
            "nt".to_string(),
        ];
        sort_terms_longest_first(&mut t);
        assert_eq!(
            t,
            vec![
                "C:\\Users\\tester".to_string(),
                "tester".to_string(),
                "nt".to_string()
            ]
        );
        assert_eq!(
            redact_terms("C:\\Users\\tester", &t),
            "<redacted>",
            "the longest term must win the whole home directory"
        );
    }

    #[test]
    fn TestSanitizeErrorFlattensWhitespace() {
        let got = sanitize_error("first line\n\tsecond   line\r\n");
        assert_eq!(got, "first line second line");
    }

    #[test]
    fn TestSanitizeErrorEmpty() {
        assert_eq!(sanitize_error(""), "");
    }

    /// The live source is the machine identity; the test only proves it produces
    /// terms this process can actually redact (never what they are).
    #[test]
    fn TestIdentityTermsAreUsable() {
        let list = identity_terms();
        for term in &list {
            if term.chars().count() < MIN_REDACT_TERM {
                continue; // deliberately not redacted
            }
            let rt = redact_terms(&format!("prefix {term} suffix"), &list);
            assert!(
                !rt.contains(term.as_str()),
                "redactTerms left {term:?} in {rt:?}"
            );
        }
    }

    /// The pinned cases above are synthetic. This case covers the live identity
    /// instead: it derives the machine's own home directory at runtime -- never
    /// writing the value down -- and proves no part of it reaches the published
    /// text. The bare account name is asserted on its own, in prose, where no
    /// path span exists to collapse it: only the term redaction can remove it.
    #[test]
    fn TestSanitizeErrorRedactsTheLiveHomeDirectory() {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        if home.trim().is_empty() {
            return; // no home directory to derive
        }
        let got = sanitize_error(&format!("open {home}\\x.mkv: Access is denied"));
        assert!(
            !got.contains(&home),
            "live home directory survived: {got:?}"
        );
        assert!(
            !got.to_lowercase().contains("users"),
            "profile directory survived: {got:?}"
        );
        assert!(
            !got.contains('\\') && !got.contains('/'),
            "a separator survived: {got:?}"
        );

        let account = basename_of(&home);
        if account.is_empty() || account == "<path>" || account.chars().count() < MIN_REDACT_TERM {
            return; // deliberately not redacted, or nothing to take from the path
        }
        let got = sanitize_error(&format!("repair failed for {account} (see log)"));
        assert!(
            !got.to_lowercase().contains(&account.to_lowercase()),
            "live account name {account:?} survived: {got:?}"
        );
    }

    #[test]
    fn TestTruncateBytes() {
        let long = "a".repeat(300);
        let got = truncate_bytes(&long, 200);
        assert_eq!(got.len(), 200);
        assert!(got.ends_with("..."), "got {got}");
        assert_eq!(truncate_bytes("short", 200), "short");
        let uni = "é".repeat(200);
        let got = truncate_bytes(&uni, 200);
        assert!(got.len() <= 200, "got len {}", got.len());
        assert!(got.ends_with("..."));
        assert!(!got.contains('\u{FFFD}'), "split a rune: {got:?}");
    }

    #[test]
    fn TestBasenameOf() {
        let cases: &[(&str, &str)] = &[
            ("D:\\a\\b.mkv", "b.mkv"),
            ("D:\\a\\b.mkv\\", "b.mkv"),
            ("C:\\", "<path>"),
            ("\\\\server\\share", "share"),
            ("/home/u/x.mkv", "x.mkv"),
            ("Some File.mkv", "Some File.mkv"),
            ("Movie (2020).mkv)", "Movie (2020).mkv"),
            ("D:\\a\\.", "<path>"),
            ("D:\\a\\b.mkv.", "b.mkv"),
            ("\\\\?\\C:\\very\\deep\\x.m", "x.m"),
            ("D:\\My Movies\\A (x).mkv", "A (x).mkv"),
        ];
        for (input, want) in cases {
            assert_eq!(basename_of(input), *want, "basenameOf({input:?})");
        }
    }
}
