
/// `maxSynopsisUnits` caps a synopsis at 128 UTF-16 code units (Discord's
/// asset-text cap, so the LargeText hover tooltip carries the short synopsis
/// whole); `cap_synopsis` backs off to a word/sentence boundary so the card
/// never shows a cut-off word. The unit is the UTF-16 code unit -- Discord's
/// asset-text cap and every presence slot budget (`NAME`/`DETAILS`/`STATE`/
/// `LARGE_TEXT_BUDGET` in the presence crate) use the same unit.
pub const MAX_SYNOPSIS_UNITS: usize = 128;

/// `firstNonEmpty`: the first string that is non-empty after trimming
/// whitespace, or `""` when all are blank.
pub fn first_non_empty(candidates: &[&str]) -> String {
    for candidate in candidates {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

/// `htmlEntities` decodes the entities providers emit in summaries, in the
/// Replacer's order (`&amp;` is applied before `&lt;`, so an escaped entity
/// survives as text).
const HTML_ENTITIES: [(&str, &str); 6] = [
    ("&nbsp;", " "),
    ("&amp;", "&"),
    ("&quot;", "\""),
    ("&#39;", "'"),
    ("&lt;", "<"),
    ("&gt;", ">"),
];

/// `stripHTML` drops `<tags>` (each replaced with one space so words on either
/// side never join), decodes the common entities, collapses all whitespace runs
/// (including newlines) to single spaces, and trims.
///
/// Entity decoding runs AFTER tag stripping, so literal `&lt;p&gt;` text
/// survives as `<p>`; a bare `<` with no following `>` is literal text, not a
/// tag (a bare "a < b" comparison).
pub fn strip_html(s: &str) -> String {
    if s.trim().is_empty() {
        return String::new();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        match bytes[i..].iter().position(|b| *b == b'>') {
            Some(offset) => {
                out.push(' ');
                i += offset + 1;
            }
            None => {
                out.push('<');
                i += 1;
            }
        }
    }
    let mut decoded = out;
    for (from, to) in HTML_ENTITIES {
        if decoded.contains(from) {
            decoded = decoded.replace(from, to);
        }
    }
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `capSynopsis` trims `s` to ~[`MAX_SYNOPSIS_UNITS`] UTF-16 code units,
/// preferring to end at the last sentence boundary (`.` `!` `?` followed by a
/// space or the end) in the back half, else at the last word boundary past the
/// halfway mark, else hard at the cap. Input at or under the cap is returned
/// trimmed as-is.
///
/// Named reading: Go's `unicode.IsSpace` is read as Rust's `char::is_whitespace`
/// (the `White_Space` property); the two differ only for the U+200B-family
/// zero-width characters no synopsis carries.
pub fn cap_synopsis(s: &str) -> String {
    let s = s.trim();
    if s.encode_utf16().count() <= MAX_SYNOPSIS_UNITS {
        return s.to_string();
    }
    let runes: Vec<char> = s.chars().collect();
    let mut used = 0usize;
    let mut end = 0usize;
    for (i, r) in runes.iter().enumerate() {
        let sz = r.len_utf16();
        if used + sz > MAX_SYNOPSIS_UNITS {
            break;
        }
        used += sz;
        end = i + 1;
    }
    let mut acc = 0usize;
    let mut half = 0usize;
    for (i, r) in runes.iter().enumerate().take(end) {
        acc += r.len_utf16();
        if acc >= MAX_SYNOPSIS_UNITS / 2 {
            half = i + 1;
            break;
        }
    }
    let cut = &runes[..end];
    for i in (half..cut.len()).rev() {
        let c = cut[i];
        if (c == '.' || c == '!' || c == '?') && (i + 1 == cut.len() || cut[i + 1].is_whitespace())
        {
            return cut[..=i].iter().collect::<String>().trim().to_string();
        }
    }
    for i in (half..cut.len()).rev() {
        if cut[i].is_whitespace() {
            return cut[..i].iter().collect::<String>().trim().to_string();
        }
    }
    cut.iter().collect::<String>().trim().to_string()
}
