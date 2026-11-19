
use std::sync::LazyLock;

use titleclean::clean;
use titleclean::javacompat::port_pattern;
use titleclean::Regex;

use crate::parse::{
    atoi64, extra_re_matches, group, has_ascii_letter, is_theme, is_year_num, word_bounded_match,
    POT_PLAYER_SUFFIX,
};
use crate::types::{Kind, Query};

fn re(src: &str, folded: bool) -> Regex {
    Regex::new(&port_pattern(src, folded))
}

/// `seasonDirRe` / `seasonShortRe` match a whole folder that IS a season
/// ("Season 1", "S01"). Applied per path component, root first.
static SEASON_DIR_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)^Season[\t\n\f\r ]*(\d+)$", true));
static SEASON_SHORT_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^S(\d{1,2})$", true));
/// `seasonEmbeddedRe` finds a season token inside a release-style folder
/// ("Mr.Harbor.S01.1080p...", "Show.Season.2.BluRay"). Fallback only: the exact
/// pass above wins, and the deepest component wins.
static SEASON_EMBEDDED_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(?:^|[.\t\n\f\r _\-])(?:Season[\t\n\f\r ]*(\d{1,2})|S(\d{1,2}))(?:$|[.\t\n\f\r _\-])",
        true,
    )
});
/// `genericFolderRe` matches whole folders that are never a show: season tokens,
/// disc/part/cour leaves ("01. Series"), quality/source tags and
/// creditless-theme leaves (NC/NCOP/NCED/OP/ED).
static GENERIC_FOLDER_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)^(featurettes?|extras?|specials?|subs?|subtitles?|bonus|season[\t\n\f\r ]*\d+|series[\t\n\f\r ]*\d+|s\d{1,2}|bd|bdrip|brrip|1080p|720p|480p|2160p|4k|nc|ncop|nced|op|ed|disc[\t\n\f\r ]*\d*|disk[\t\n\f\r ]*\d*|part[\t\n\f\r ]*\d+|cour[\t\n\f\r ]*\d+|vol[\t\n\f\r ]*\d*|\d{1,2}[\t\n\f\r ]*[.\-][\t\n\f\r ]*(series|season|disc|disk|part|cour))$",
        true,
    )
});
/// `extrasWordRe` spots an extras folder anywhere in the component ("Deleted
/// Scenes", "Featurettes 5.1", "Trailer", "NCED"). Substring on word boundaries,
/// mirroring the filename-level `extraRe`; bare OP/ED are deliberately absent
/// here ("Speed" must never match -- full-match only in `genericFolderRe`).
/// Go wraps the alternation in `\b...\b` (and repeats it around `nc`), so the
/// boundaries are applied by [`word_bounded_match`].
static EXTRAS_WORD_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(deleted[\t\n\f\r ]+scenes?|extended[\t\n\f\r ]+scenes?|trailers?|gallery|alternate[\t\n\f\r ]+endings?|interviews?|behind[\t\n\f\r ]+the[\t\n\f\r ]+scenes|behind[\t\n\f\r ]+the[\t\n\f\r ]+stor(?:y|ies)|featurettes?|music[\t\n\f\r ]+videos?|samples?|extras?|specials?|gag[\t\n\f\r ]+reels?|blooper[\t\n\f\r ]+reels?|outtakes?|tv[\t\n\f\r ]+spots?|making[\t\n\f\r ]+of|meet[\t\n\f\r ]+the[\t\n\f\r ]+cast|visual[\t\n\f\r ]+effects|in[\t\n\f\r ]+search[\t\n\f\r ]+of|how[\t\n\f\r ]+to[\t\n\f\r ]+survive|previz|production[\t\n\f\r ]+design|time[\t\n\f\r \-]*lapse[\t\n\f\r ]+videos?|secret[\t\n\f\r ]+auditions?|ncop|nced|nc)",
        true,
    )
});
/// `qualityTitleRe` rejects cleaned folder titles that are bare numbers/quality
/// tokens, mirroring the Parse floor.
static QUALITY_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)^(\d+|1080p|720p|480p|360p|2160p|4k|uhd|bd|bdrip|brrip|10bit|8bit)$",
        true,
    )
});
/// `driveRe` spots a Windows volume component ("C:").
static DRIVE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z]:$", false));
/// `rootLookRe` spots a library-root-looking top folder: no lowercase letters
/// (1ARCHIVE, 2SERIES, 2NEW ARCHIVE AND FILMS). Show folders carry lowercase
/// (Zentaro, Mr.Harbor.S01...).
static ROOT_LOOK_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[^a-z]*$", false));
/// `seasonSuffixRe` trims a trailing season token off a cleaned folder show
/// ("Mr Harbor S01" -> "Mr Harbor").
static SEASON_SUFFIX_RE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)[\t\n\f\r ]+S\d{1,2}$", true));
/// `videoExtRe` strips one trailing media extension before number hunting.
static VIDEO_EXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)\.(mkv|mp4|avi|m4v|mov|wmv|ts|flv|webm|mpg|mpeg|m4v|ass|srt)$",
        true,
    )
});
/// `epNumRe` finds separator-delimited numbers. Dots are NOT delimiters, so
/// version fragments (5.1, 3.0), quality tokens (1080p, 10bit, x265) and joined
/// tags (S01E01, E2015) never match; years are rejected by the caller via
/// `is_year_num`.
///
/// `regexlite` has no offset-aware search, so a second copy of the pattern
/// without the leading `^` alternative serves every match after the first: `^`
/// (like `\A`) can only match at the start of the text, so searching a suffix
/// slice with the tail pattern is exactly Go's "continue from the previous
/// match's end".
static EP_NUM_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?:^|[\t\n\f\r _\-\[\]();]+)(\d{1,4})(?:$|[\t\n\f\r _\-\[\]();]+)",
        false,
    )
});
static EP_NUM_TAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?:[\t\n\f\r _\-\[\]();]+)(\d{1,4})(?:$|[\t\n\f\r _\-\[\]();]+)",
        false,
    )
});

/// `splitDirs` splits a full path into components on either separator, drops the
/// trailing filename, and reports the index of the top folder under the volume
/// (the library root: e.g. X:\Library1, Y:\Library2, ...). Both
/// separators are accepted so unit paths and live paths behave alike.
pub(crate) fn split_dirs(path: &str) -> (Vec<&str>, usize) {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return (Vec::new(), 0);
    }
    let dirs = parts[..parts.len() - 1].to_vec();
    let mut top = 0;
    while top < dirs.len() && (DRIVE_RE.is_match(dirs[top]) || dirs[top] == ".") {
        top += 1;
    }
    (dirs, top)
}

/// `extrasWordRe` against a raw path component or a cleaned folder title.
fn extras_word_matches(s: &str) -> bool {
    word_bounded_match(&EXTRAS_WORD_RE, s)
}

/// `isGenericFolder` reports whether a path component can never be a show name.
pub(crate) fn is_generic_folder(name: &str) -> bool {
    if SEASON_DIR_RE.is_match(name) || SEASON_SHORT_RE.is_match(name) {
        return true;
    }
    if GENERIC_FOLDER_RE.is_match(name) || extras_word_matches(name) {
        return true;
    }
    let t = clean(name).title;
    !t.is_empty() && (GENERIC_FOLDER_RE.is_match(&t) || extras_word_matches(&t))
}

/// `underExtrasFolder` reports whether any folder on the path is an extras leaf
/// (Deleted Scenes, Featurettes, ...). Such files stay KindUnknown even when a
/// bare number is present: the inventory buckets them F.
///
/// Public for the repair inventory (`library::catchup`): a needs-repair row
/// whose path sits under an extras leaf is not a metadata gap, so it must be
/// excluded from the list rather than re-fetched forever.
pub fn under_extras_folder(path: &str) -> bool {
    let (dirs, _) = split_dirs(path);
    for d in dirs {
        if extras_word_matches(d) {
            return true;
        }
        let t = clean(d).title;
        if !t.is_empty() && extras_word_matches(&t) {
            return true;
        }
    }
    false
}

/// `ContextFromPath` derives (show, season) from a full playing-file path. The
/// filename is dropped. Season is the root-first exact "Season N"/"SNN" folder,
/// falling back to the deepest embedded release-style token (".S01."). Show is
/// the deepest surviving folder: drives, the library-root top folder when it
/// looks like a root (no lowercase: MOVIES, SHOWS, ...), season and generic
/// folders are skipped, and the survivor is titleclean-normalised (dots become
/// spaces). `("", 0)` when nothing usable remains.
pub fn context_from_path(path: &str) -> (String, i64) {
    if path.trim().is_empty() {
        return (String::new(), 0);
    }
    let (dirs, top) = split_dirs(path);
    if dirs.is_empty() {
        return (String::new(), 0);
    }
    let mut season = 0;
    for d in &dirs {
        if let Some(caps) = SEASON_DIR_RE.captures(d) {
            season = atoi64(group(&caps, 1));
            break;
        }
        if let Some(caps) = SEASON_SHORT_RE.captures(d) {
            season = atoi64(group(&caps, 1));
            break;
        }
    }
    if season == 0 {
        for d in dirs.iter().rev() {
            if let Some(caps) = SEASON_EMBEDDED_RE.captures(d) {
                let s = group(&caps, 1);
                let s = if s.is_empty() { group(&caps, 2) } else { s };
                let n = atoi64(s);
                if n > 0 {
                    season = n;
                    break;
                }
            }
        }
    }
    let mut show = String::new();
    for (i, d) in dirs.iter().enumerate().rev() {
        if d.is_empty() || *d == "." || *d == ".." || DRIVE_RE.is_match(d) {
            continue;
        }
        // The volume-top folder is the library root (the synthetic roots are all
        // volume tops; root-direct files carry no show). It is skipped only when
        // it looks like a root rather than a show: library roots carry no
        // lowercase letters (1ARCHIVE, 2SERIES, 2NEW ARCHIVE AND FILMS) while
        // show folders do (Zentaro, Mr.Harbor.S01...).
        if i == top && ROOT_LOOK_RE.is_match(d) {
            continue;
        }
        if is_generic_folder(d) {
            continue;
        }
        let t = clean(d).title;
        let t = SEASON_SUFFIX_RE
            .replace_all(t.trim(), "")
            .trim()
            .to_string();
        if t.is_empty() || QUALITY_TITLE_RE.is_match(&t) || !has_ascii_letter(&t) {
            continue;
        }
        show = t;
        break;
    }
    if show.is_empty() {
        return (String::new(), 0);
    }
    (show, season)
}

/// `bareEpisodeNumber` reads one episode number from a raw filename: the first
/// separator-delimited run that is not a year (1900-2100). Dots never delimit,
/// so versions (5.1), quality (1080p/10bit/x265) and joined tags (S01E01) cannot
/// match. 0 when none.
pub(crate) fn bare_episode_number(raw_title: &str) -> i32 {
    let mut s = raw_title
        .strip_suffix(POT_PLAYER_SUFFIX)
        .unwrap_or(raw_title);
    if let Some(i) = s.rfind(['/', '\\']) {
        s = &s[i + 1..];
    }
    let s = s.trim();
    let s = match VIDEO_EXT_RE.find(s) {
        Some((a, _)) => &s[..a],
        None => s,
    };
    for n in ep_numbers(s) {
        if n > 0 && !is_year_num(n) {
            return n;
        }
    }
    0
}

/// Every `epNumRe` match's number, in Go's `FindAllStringSubmatch` order: the
/// leftmost match, then the next one from its end. The first search uses the
/// `^`-bearing pattern; every later one searches a suffix slice with
/// [`EP_NUM_TAIL_RE`] (see its doc).
fn ep_numbers(s: &str) -> Vec<i32> {
    let mut out = Vec::new();
    let mut from = 0usize;
    loop {
        let (hay, re) = if from == 0 {
            (s, &*EP_NUM_RE)
        } else {
            (&s[from..], &*EP_NUM_TAIL_RE)
        };
        let Some((_, end)) = re.find(hay) else {
            break;
        };
        let caps = re.captures(hay).expect("find agreed");
        out.push(atoi(group(&caps, 1)));
        let end = from + end;
        if end <= from {
            break;
        }
        from = end;
    }
    out
}

/// `ResolveQuery` merges folder context into the filename-only
/// [`parse_raw`](crate::parse::parse_raw) result. An empty path falls back
/// cleanly to filename-only.
///
///   * KindUnknown: a bare-number filename ("01.mkv") plus a usable folder show
///     becomes a series query (absolute when the folders carry no season).
///     Themes, filename-level extras, and files under extras folders stay
///     Unknown, so they are never fetched or cached.
///   * KindSeries: an empty title takes the folder show; season fills from the
///     folders only for non-absolute queries; a still-empty title demotes to
///     Unknown.
///   * KindMovie: an empty title takes the folder show.
pub fn resolve_query(raw_title: &str, path: &str) -> Query {
    let q = crate::parse::parse_raw(raw_title);
    if path.trim().is_empty() {
        return q;
    }
    let (show, season) = context_from_path(path);
    match q.kind {
        Kind::UNKNOWN => {
            if show.is_empty() || under_extras_folder(path) {
                return q;
            }
            let c = clean(
                raw_title
                    .strip_suffix(POT_PLAYER_SUFFIX)
                    .unwrap_or(raw_title),
            );
            if is_theme(&c.title, &c.episode_tag) || extra_re_matches(&c.title) {
                return q;
            }
            let n = bare_episode_number(raw_title);
            if n > 0 {
                return Query {
                    kind: Kind::SERIES,
                    title: show,
                    season,
                    episode: n,
                    absolute: season == 0,
                    ..Query::default()
                };
            }
            q
        }
        Kind::SERIES => {
            let mut q = q;
            if q.title.is_empty() {
                q.title = show;
            }
            if q.season == 0 && season > 0 && !q.absolute {
                q.season = season;
            }
            if q.title.is_empty() {
                q.kind = Kind::UNKNOWN;
            }
            q
        }
        // KindMovie (and any out-of-range kind).
        _ => {
            let mut q = q;
            if q.title.is_empty() && !show.is_empty() {
                q.title = show;
            }
            q
        }
    }
}

/// Go's `atoi` for the bounded `\d{1,4}` episode captures: a non-numeric
/// capture is 0.
fn atoi(s: &str) -> i32 {
    s.parse().unwrap_or(0)
}
