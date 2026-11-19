
use std::sync::LazyLock;

use crate::javacompat::{
    ascii_lower, contains_line_term, java_boundary, java_dollar_replace_all, java_length,
    java_trim, java_trunc, java_upper_first, port_pattern,
};
use crate::regexlite::Regex;

/// Java's `\s` is `[ \t\n\x0B\f\r]`, wider than the usual shorthand (which
/// omits `\x0B`).
pub const JWS: &str = r"[ \t\n\x0B\f\r]";

/// `TAGS` alternation with the `\b` assertions removed; those are applied in
/// [`replace_tags`] using Java's Unicode-aware boundary rule.
pub const TAGS_ALT: &str = r"2160p|1080p|720p|480p|360p|4k|uhd|hdr\d*\+?|dolby[ .\-]?vision|blu[ .\-]?ray|bdrip|brrip|web[ .\-]?dl|webrip|web[ .\-]?hd|hdtv|dvdrip|dvdscr|camrip|hdts|hdcam|hdrip|h[ .\-]?26[45]|x26[45]|xvid|hevc|av1|vp9|10bit|8bit|aac\d?(?:[ .\-]?\d)?|ac3|eac3|dts(?:[ .\-]?hd)?|ddp?a?[ .\-]?\d(?:[ .\-]?\d)?|truehd|atmos|flac|mp3|opus|dual(?:[ .\-]?audio)?|multi[ .\-]?subs?|multi\d*|msubs|subbed|dubbed|extended[ .\-]?cut|unrated|theatrical[ .\-]?cut|directors?[ .\-]?cut|repack|proper|nf|amzn|dsnp|atvp|hmax|imax|sdr|hlg|vmaf\d*|thai|tamil|hindi|kor|jpn|eng|rarbg|rartv|ettv|yts|yify|mkvcage|psa|evo|lol|ion10|vostfr|vost|5[ .\-]?1|7[ .\-]?1";

// EXT: trailing media extension, case-insensitive.
static EXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"\.(mp4|mkv|avi|webm|ts|m2ts|mov|wmv|flv|mpg|mpeg|m4v|ass|srt)$",
        true,
    ))
});

// clean() housekeeping.
static GROUP_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    let pat = String::from("^") + JWS + r"*\[[^\]]+\]" + JWS + r"*";
    Regex::new(&port_pattern(&pat, false))
});
static CRC_RE: LazyLock<Regex> = LazyLock::new(|| {
    let pat = String::from(JWS) + r"*\[[A-Fa-f0-9]{8}\]" + JWS + r"*$";
    Regex::new(&port_pattern(&pat, false))
});
static PAREN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&port_pattern(r"\([^)]*\)", false)));
static WS_RUN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(&(String::from(JWS) + "+"), false)));
static TRAIL_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"[. _\-]+$", false)));
static SCENE_GROUP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"[-_.][A-Za-z0-9]{2,12}$", false)));

// Episode matcher chain (order matters, exactly as Java).
static MULTI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.*?)[. _\-]*[Ss](\d{1,4})[ . _\-]*[Ee](\d{1,3})[ ._\-]*[\-\x{2013}\x{2014}]?[Ee](\d{1,3})(?:[. _\-]+(.*))?$",
        true,
    ))
});
static SERIES_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.*?)[. _\-]*[Ss](\d{1,4})[ . _\-]*[Ee](\d{1,3})(?:[. _\-]+(.*))?$",
        true,
    ))
});
static SEASON_EP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.*?)[. _\-]*Season[ . _\-]*(\d{1,2})[. _\-]+(?:Episode|Ep)\.?[ . _\-]*(\d{1,4})(?:[. _\-]+(.*))?$",
        true,
    ))
});
static CHAPTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.+?)[. _\-]+Chapter[ . _\-]*(\d{1,4})(?:[. _\-]+(.*))?$",
        true,
    ))
});
static SEASON_EXTRA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.*?)[. _\-]*Season[ . _\-]*(\d{1,2})[. _\-]+(.+)$",
        true,
    ))
});
static TN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"^(\d{1,3})(?:[. _\-]+(.*))?$", false)));
static EPWORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.*?)[. _\-]*(?:Episode|Ep)\.?[ . _\-]*(\d{1,4})(?:[. _\-]+(.*))?$",
        true,
    ))
});
static XEP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.*?)[. _\-]+(\d{1,2})x(\d{2,3})(?:[. _\-]+(.*))?$",
        true,
    ))
});
static TRAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.{3,}?)[. _\-]+(0\d{1,2}|\d{3})$",
        true,
    ))
});
static ALT_EP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^(.+?)[. _\-]+(?:EP?|#)?[ . _\-]*(\d{1,3})(?:[. _\-]+(.*))?$",
        true,
    ))
});
static FANSUB_RE: LazyLock<Regex> = LazyLock::new(|| {
    let pat = String::from(r"(?i)^(.+?)")
        + JWS
        + r"*[-;]"
        + JWS
        + r"*(?:Ep?\.?"
        + JWS
        + r"*)?(\d{1,4})(?:"
        + JWS
        + r"*[-;]"
        + JWS
        + r"*(.*))?$";
    Regex::new(&port_pattern(&pat, true))
});
static NCBOOK_RE: LazyLock<Regex> = LazyLock::new(|| {
    let pat = String::from(r"(?i)^(?:(.*?)[. _\-]+)?(?:NC)?(OP|ED)[ . _\-]*(\d{1,3})(?:")
        + JWS
        + r"*[-;]"
        + JWS
        + r"*(.*))?$";
    Regex::new(&port_pattern(&pat, true))
});

// ALT_EP guard patterns.
static ALT_TAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)^.*[. _\-](EP?|#)?[ . _\-]*\d{1,3}([. _\-].*)?$",
        true,
    ))
});

// NCBOOK head season-suffix strip.
static NC_HEAD_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"(?i)[. _\-]+S\d{1,2}$", true)));

// epName cleanup.
static EP_NAME_TRAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    let pat = String::from(JWS) + r"*-" + JWS + r"*[A-Z0-9]{2,}$";
    Regex::new(&port_pattern(&pat, false))
});
static EP_NAME_FILTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"(?i)^(?:WEB|DS4K|OVA|v\d+|\d+|\+)$", true)));

// stripTags().
static DOT_UNDERSCORE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"[._]+", false)));
static BRACKET_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&port_pattern(r"\[.*?\]", false)));
static PAREN_STRIP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"\(.*?\)", false)));
static LEADING_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"^[\-_. ]+", false)));
static TRAILING_SEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"[\-_. ]+$", false)));
static YEAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&port_pattern(r"^(.*)[. _\-]+(19\d{2}|20\d{2})$", false)));

// TAGS core (without \b).
static TAGS_CORE_RE: LazyLock<Regex> = LazyLock::new(|| {
    let pat = String::from("(?:") + TAGS_ALT + ")";
    Regex::new(&port_pattern(&pat, true))
});
// TAGS used for full-match tests during boundary backtracking.
static TAGS_FULL_RE: LazyLock<Regex> = LazyLock::new(|| {
    let pat = String::from("^(?:") + TAGS_ALT + ")$";
    Regex::new(&port_pattern(&pat, true))
});

// SCENE_HINT: presence of codec/source marker.
static SCENE_HINT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&port_pattern(
        r"(?i)(1080p|2160p|720p|4k|uhd|x26[45]|web[ .\-]?dl|bluray|bdrip|hdtv|dvdrip|hevc|av1|opus)",
        true,
    ))
});

/// Mirrors `me.potflix.rpc.Main.Cleaned`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cleaned {
    pub title: String,
    pub display: String,
    pub episode_tag: String,
    pub episode_name: String,
}

/// Applies Java's `TAGS.replaceAll(" ")`, where the `\b` assertions use Java's
/// Unicode-aware word rule instead of an ASCII one. Java's backtracking is
/// emulated: when the greedy end fails the right boundary, the longest shorter
/// end that still fully matches the alternation on a boundary is used (e.g.
/// `HDR10+` matches `HDR10`, leaving `+`).
fn replace_tags(r: &str) -> String {
    let runes: Vec<char> = r.chars().collect();
    let mut out: Vec<u8> = Vec::with_capacity(r.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i <= r.len() {
        let Some((l0, l1)) = TAGS_CORE_RE.find(&r[i..]) else {
            break;
        };
        let s0 = i + l0;
        let e_max = i + l1;
        if !java_boundary(&runes, r[..s0].chars().count()) {
            i = next_rune_index(r, s0);
            continue;
        }
        let mut matched = false;
        let mut e = e_max;
        while e > s0 {
            if TAGS_FULL_RE.is_match(&r[s0..e]) && java_boundary(&runes, r[..e].chars().count()) {
                out.extend_from_slice(&r.as_bytes()[last..s0]);
                out.push(b' ');
                last = e;
                i = e;
                matched = true;
                break;
            }
            match prev_rune_index(r, e) {
                Some(pe) => e = pe,
                None => break,
            }
        }
        if !matched {
            i = next_rune_index(r, s0);
        }
    }
    out.extend_from_slice(&r.as_bytes()[last..]);
    String::from_utf8(out).expect("replace_tags preserves UTF-8")
}

/// Mirrors Java's `s.matches("(?i).*\b(19|20)\d{2}\b.*")`.
pub(crate) fn has_year(s: &str) -> bool {
    if contains_line_term(s) {
        return false;
    }
    let r: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i + 4 <= r.len() {
        let century = (r[i] == '1' && r[i + 1] == '9') || (r[i] == '2' && r[i + 1] == '0');
        if century
            && r[i + 2].is_ascii_digit()
            && r[i + 3].is_ascii_digit()
            && java_boundary(&r, i)
            && java_boundary(&r, i + 4)
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Mirrors `s.matches("(?i).*\b(Season|Chapter|Part|Episode|Ep|Vol(ume)?)$")`.
pub(crate) fn alt_head_bad(s: &str) -> bool {
    if contains_line_term(s) {
        return false;
    }
    let runes: Vec<char> = s.chars().collect();
    let lower = ascii_lower(s);
    for w in [
        "season", "chapter", "part", "episode", "ep", "volume", "vol",
    ] {
        if lower.len() >= w.len()
            && &lower.as_bytes()[lower.len() - w.len()..] == w.as_bytes()
            && let Some(pos) = runes.len().checked_sub(w.len())
            && java_boundary(&runes, pos)
        {
            return true;
        }
    }
    false
}

/// Mirrors `s.matches("^(19|20)\d{2}\b.*")`.
pub(crate) fn year_lead(s: &str) -> bool {
    if contains_line_term(s) {
        return false;
    }
    let r: Vec<char> = s.chars().collect();
    if r.len() < 4 {
        return false;
    }
    if !((r[0] == '1' && r[1] == '9') || (r[0] == '2' && r[1] == '0')) {
        return false;
    }
    if !r[2].is_ascii_digit() || !r[3].is_ascii_digit() {
        return false;
    }
    java_boundary(&r, 4)
}

/// A faithful port of `me.potflix.rpc.Main.clean(String)`.
pub fn clean(raw: &str) -> Cleaned {
    let mut out = Cleaned::default();
    let mut s = java_dollar_replace_all(&EXT_RE, &java_trim(raw), "");
    s = GROUP_PREFIX_RE.replace_all(&s, ""); // [Group] prefix
    s = java_dollar_replace_all(&CRC_RE, &s, ""); // [CRC32] suffix
    let yr = has_year(&s);

    let mut ds = PAREN_RE.replace_all(&s, " ");
    ds = WS_RUN_RE.replace_all(&ds, " ");
    ds = java_trim(&ds);
    ds = java_dollar_replace_all(&TRAIL_SEP_RE, &ds, "");

    if let Some(gs) = NCBOOK_RE.captures(&ds) {
        let mut nc = Cleaned::default();
        let nhead = java_dollar_replace_all(&NC_HEAD_SEP_RE, group(&gs, 1), "");
        let op = group(&gs, 2);
        let ob = op.as_bytes();
        let is_op = ob.len() >= 2 && matches!(ob[0], b'O' | b'o') && matches!(ob[1], b'P' | b'p');
        nc.episode_tag = if is_op {
            format!("OP {}", atoi(group(&gs, 3)))
        } else {
            format!("ED {}", atoi(group(&gs, 3)))
        };
        nc.episode_name = strip_tags(group(&gs, 4));
        let mut ntitle = strip_tags(&nhead);
        if ntitle.is_empty() {
            if !nc.episode_name.is_empty() {
                ntitle = std::mem::take(&mut nc.episode_name);
            } else {
                nc.title = java_trunc(&nc.episode_tag, 100);
                nc.display = nc.title.clone();
                return nc;
            }
        }
        nc.title = java_trunc(&ntitle, 100);
        nc.display = build_display(&nc.title, &nc.episode_tag, &nc.episode_name);
        return nc;
    }

    if SCENE_HINT_RE.is_match(&s) {
        s = java_dollar_replace_all(&SCENE_GROUP_RE, &s, "");
    }

    let mut head = String::new();
    let mut ep_tag = String::new();
    let mut rest = String::new();
    let mut got = false;

    if let Some(gs) = MULTI_RE.captures(&s) {
        head = group(&gs, 1).to_string();
        ep_tag = format!(
            "S{} E{}-{}",
            atoi(group(&gs, 2)),
            atoi(group(&gs, 3)),
            atoi(group(&gs, 4))
        );
        rest = group(&gs, 5).to_string();
        got = true;
    }
    if !got && let Some(gs) = SEASON_EP_RE.captures(&s) {
        head = group(&gs, 1).to_string();
        ep_tag = format!("S{} E{}", atoi(group(&gs, 2)), atoi(group(&gs, 3)));
        rest = group(&gs, 4).to_string();
        got = true;
    }
    if !got && !yr && let Some(gs) = CHAPTER_RE.captures(&s) {
        head = group(&gs, 1).to_string();
        ep_tag = format!("E{}", atoi(group(&gs, 2)));
        rest = group(&gs, 3).to_string();
        got = true;
    }
    if !got && let Some(gs) = SEASON_EXTRA_RE.captures(&s) {
        if let Some(tn) = TN_RE.captures(group(&gs, 3)) {
            head = group(&gs, 1).to_string();
            ep_tag = format!("S{} E{}", atoi(group(&gs, 2)), atoi(group(&tn, 1)));
            rest = group(&tn, 2).to_string();
            got = true;
        } else {
            let ttl = java_trim(&format!(
                "{} {}",
                strip_tags(group(&gs, 1)),
                strip_tags(group(&gs, 3))
            ));
            out.title = if ttl.is_empty() {
                java_trunc(&strip_tags(&s), 100)
            } else {
                java_trunc(&ttl, 100)
            };
            out.display = out.title.clone();
            return out;
        }
    }
    if !got && let Some(gs) = SERIES_RE.captures(&s) {
        head = group(&gs, 1).to_string();
        ep_tag = format!("S{} E{}", atoi(group(&gs, 2)), atoi(group(&gs, 3)));
        rest = group(&gs, 4).to_string();
        got = true;
    }
    if !got && let Some(gs) = FANSUB_RE.captures(&s) {
        let num = group(&gs, 2);
        let fnum = atoi(num);
        let year_like = java_length(num) == 4 && (1900..=2100).contains(&fnum);
        if !year_like {
            head = group(&gs, 1).to_string();
            ep_tag = format!("E{fnum}");
            rest = group(&gs, 3).to_string();
            got = true;
        }
    }
    if !got && let Some(gs) = EPWORD_RE.captures(&s) {
        head = group(&gs, 1).to_string();
        ep_tag = format!("E{}", atoi(group(&gs, 2)));
        rest = group(&gs, 3).to_string();
        got = true;
    }
    if !got && let Some(gs) = XEP_RE.captures(&s) {
        head = group(&gs, 1).to_string();
        ep_tag = format!("S{} E{}", atoi(group(&gs, 2)), atoi(group(&gs, 3)));
        rest = group(&gs, 4).to_string();
        got = true;
    }
    if !got && !yr && let Some(gs) = TRAIL_RE.captures(&s) {
        head = group(&gs, 1).to_string();
        ep_tag = format!("E{}", atoi(group(&gs, 2)));
        rest = String::new();
        got = true;
    }
    if !got
        && let Some(gs) = ALT_EP_RE.captures(&s)
        && java_length(group(&gs, 1)) >= 3
        && java_length(group(&gs, 2)) <= 3
        && gs.get(3).copied().flatten().is_some()
        && java_length(group(&gs, 3)) < 40
        && ALT_TAIL_RE.is_match(&s)
        && java_length(&strip_tags(&s)) > java_length(group(&gs, 1))
        && !alt_head_bad(group(&gs, 1))
        && !year_lead(group(&gs, 3))
    {
        head = group(&gs, 1).to_string();
        ep_tag = format!("E{}", atoi(group(&gs, 2)));
        rest = group(&gs, 3).to_string();
        got = true;
    }

    let mut core = if got {
        strip_tags(&head)
    } else {
        strip_tags(&s)
    };
    let ep_name_src = if got {
        strip_tags(&rest)
    } else {
        String::new()
    };
    let mut ep_name = java_dollar_replace_all(&EP_NAME_TRAIL_RE, &ep_name_src, "");
    ep_name = java_trim(&ep_name);
    if EP_NAME_FILTER_RE.is_match(&ep_name) {
        ep_name = String::new();
    }
    if got && core.is_empty() {
        if !ep_name.is_empty() {
            out.title = java_trunc(&ep_name, 100);
            out.display = format!("{} \u{2014} {}", out.title, ep_tag);
            out.episode_tag = ep_tag;
            out.episode_name = String::new();
            return out;
        }
        out.title = ep_tag.clone();
        out.display = ep_tag.clone();
        out.episode_tag = ep_tag;
        out.episode_name = String::new();
        return out;
    }
    if core.is_empty() {
        core = strip_tags(&s);
    }
    out.title = java_trunc(&core, 100);
    out.episode_tag = ep_tag;
    out.episode_name = ep_name;
    out.display = build_display(&out.title, &out.episode_tag, &out.episode_name);
    out
}

/// Reads capture group `i` with Java's null-group rule folded in: a
/// non-participating group reads as the empty string.
fn group<'a>(gs: &[Option<&'a str>], i: usize) -> &'a str {
    gs.get(i).copied().flatten().unwrap_or("")
}

/// A faithful port of `me.potflix.rpc.Main.stripTags(String)`.
pub fn strip_tags(s: &str) -> String {
    let mut r = DOT_UNDERSCORE_RE.replace_all(s, " ");
    r = BRACKET_RE.replace_all(&r, " ");
    r = PAREN_STRIP_RE.replace_all(&r, " ");
    r = replace_tags(&r);
    r = WS_RUN_RE.replace_all(&r, " ");
    r = java_trim(&r);
    r = LEADING_SEP_RE.replace_all(&r, "");
    r = java_dollar_replace_all(&TRAILING_SEP_RE, &r, "");

    let mut year = String::new();
    if let Some(gs) = YEAR_RE.captures(&r) {
        let head = java_trim(group(&gs, 1));
        year = format!(" ({})", group(&gs, 2));
        r = head;
    }
    r = fold_title_marks(&r);
    r = java_upper_first(&r);
    java_trim(&format!("{r}{year}"))
}

/// The `?` family a scene release writes where a filename cannot hold `:`:
/// ASCII `?` (U+003F), fullwidth `？` (U+FF1F) and small `﹖` (U+FE56).
pub fn is_title_mark_rune(c: char) -> bool {
    c == '?' || c == '\u{FF1F}' || c == '\u{FE56}'
}

/// Deviation P13 from the frozen Java cleaner. A scene release stands in for
/// the `:` Windows forbids in a filename ("Spider-Man? Brand New Day (2026)
/// (1080p ...).mkv"), and Java's UTF-8 encoder writes the same `?` for an
/// unpaired surrogate, so an extracted title can carry a mark that means `:`.
///
/// The rule: a mark folds ONLY when it is preceded by a non-space character AND
/// is not the last character of the extracted title. A mark that ends the title
/// is a real question mark ("Who Framed Roger Rabbit?") and is left alone: once
/// the parenthesised tail is stripped, no scene convention tells a genuine
/// question mark from a stand-in whose tail is gone.
pub fn fold_title_marks(s: &str) -> String {
    let mut r: Vec<char> = s.chars().collect();
    let mut folded = false;
    let mut i = 1;
    while i + 1 < r.len() {
        if is_title_mark_rune(r[i]) && r[i - 1] > ' ' {
            r[i] = ':';
            folded = true;
        }
        i += 1;
    }
    if !folded {
        return s.to_string();
    }
    r.into_iter().collect()
}

/// Mirrors `me.potflix.rpc.Main.buildDisplay`. The separator is U+2014.
pub fn build_display(title: &str, tag: &str, name: &str) -> String {
    if tag.is_empty() {
        return title.to_string();
    }
    if title.is_empty() {
        return tag.to_string();
    }
    if name.is_empty() {
        return format!("{title} \u{2014} {tag}");
    }
    format!("{title} \u{2014} {tag} \"{}\"", java_trunc(name, 60))
}

fn atoi(s: &str) -> i32 {
    s.parse::<i32>().unwrap_or(0)
}

fn next_rune_index(s: &str, i: usize) -> usize {
    match s[i..].chars().next() {
        Some(c) => i + c.len_utf8(),
        None => i + 1,
    }
}

fn prev_rune_index(s: &str, i: usize) -> Option<usize> {
    if i == 0 {
        return None;
    }
    s[..i].chars().next_back().map(|c| i - c.len_utf8())
}
