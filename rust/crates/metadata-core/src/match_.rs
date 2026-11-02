//! `metadata::match_` -- mirrors `go\internal\metadata\match.go`.
//!
//! Candidate-title matching against provider results (the scoring that decides
//! which hit is the right series/episode). TVmaze returns its search hits in its
//! own rank order, which puts a spinoff above the main show, so the resolver
//! re-ranks them with [`pick_best_show`].
//!
//! # Accent folding is a hand-rolled table
//!
//! [`folded_accent`] is a LINEAR switch transcribed rune for rune from the Go
//! source, deliberately not a Unicode crate: `æ`→`ae`, `œ`→`oe`, `ß`→`ss`
//! (inside the `s` group), `þ`→`th`, `ð`→`d`, `ı`→`i`. Mn marks are stripped
//! BEFORE the switch, so a combining accent vanishes rather than folding. The
//! Mn test uses `titleclean`'s JDK-11 table (`jdk_is_mn`), the only Unicode
//! general-category data the workspace allows into the graph.

use std::collections::HashSet;

use titleclean::javacompat::jdk_is_mn;

/// `ShowCand` is one candidate show for title matching. Weight carries the
/// TVmaze show weight (higher ranks more popular/authoritative); EpisodeCount
/// breaks remaining ties toward the fuller catalog entry. `aliases` carries
/// the entry's other titles (an anime entry's romaji name beside its English
/// one): every alias scores exactly like `name`, so a query in one script
/// still finds the entry whose display title is in another. A provider with a
/// single title leaves it empty, which is exactly the old behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShowCand {
    pub name: String,
    pub weight: i32,
    pub episode_count: i32,
    pub aliases: Vec<String>,
}

/// Tuning for [`pick_best_show`]: candidates need at least this similarity to
/// answer at all, and candidates within [`MATCH_TIE_MARGIN`] of the top
/// similarity form a tie group broken by Weight then EpisodeCount.
pub(crate) const MATCH_MIN_SIMILARITY: f64 = 0.5;
pub(crate) const MATCH_TIE_MARGIN: f64 = 0.1;

/// `foldedAccent` maps a lowercased accented/special Latin rune to ASCII.
/// `None` means the rune is not a special case.
fn folded_accent(r: char) -> Option<&'static str> {
    match r {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'ǎ' | 'ȁ' | 'ȃ' => {
            Some("a")
        }
        'æ' => Some("ae"),
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => Some("c"),
        'ď' => Some("d"),
        'ð' => Some("d"),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' | 'ȅ' | 'ȇ' => Some("e"),
        'ĝ' | 'ğ' | 'ġ' | 'ģ' => Some("g"),
        'ĥ' => Some("h"),
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ǐ' | 'ȉ' | 'ȋ' | 'ı' => {
            Some("i")
        }
        'ĵ' => Some("j"),
        'ķ' => Some("k"),
        'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => Some("l"),
        'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' => Some("n"),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'ǒ' | 'ȍ' | 'ȏ' => {
            Some("o")
        }
        'œ' => Some("oe"),
        'ŕ' | 'ŗ' | 'ř' => Some("r"),
        // 'ß' → "ss", the rest of the group → "s".
        'ß' => Some("ss"),
        'ś' | 'ŝ' | 'ş' | 'š' | 'ș' => Some("s"),
        'ţ' | 'ť' | 'ŧ' | 'ț' => Some("t"),
        'þ' => Some("th"),
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'ǔ' | 'ȕ' | 'ȗ' => {
            Some("u")
        }
        'ŵ' => Some("w"),
        'ý' | 'ÿ' | 'ŷ' => Some("y"),
        'ź' | 'ż' | 'ž' => Some("z"),
        _ => None,
    }
}

/// `normalizeTitle` lowercases, folds punctuation to spaces and diacritics to
/// ASCII, folds a trailing sequel numeral (`ii`→`2`) so `Rocky II` and
/// `Rocky 2` compare equal, and collapses whitespace, so "Re:ZERO", "Mr. Robot" and "Café"
/// compare as "re zero", "mr robot" and "cafe".
///
/// Two deliberate readings of the Go text: `strings.ToLower` is a per-rune
/// simple mapping while Rust's `char::to_lowercase` is the full mapping, whose
/// only multi-char expansion (U+0130 → "i" + U+0307) loses its extra mark to
/// the Mn strip immediately below; and Go's `IsLetter || IsNumber` is read as
/// Rust's `is_alphabetic() || is_numeric()`, which differs only for the
/// Other_Alphabetic spacing marks (Mc) no real title carries.
pub fn normalize_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for r in s.to_lowercase().chars() {
        if jdk_is_mn(r) {
            continue;
        }
        if let Some(rep) = folded_accent(r) {
            out.push_str(rep);
            continue;
        }
        if r.is_alphabetic() || r.is_numeric() {
            out.push(r);
            continue;
        }
        out.push(' ');
    }
    let mut tokens: Vec<String> = out.split_whitespace().map(str::to_string).collect();
    if let Some(last) = tokens.last_mut()
        && let Some(digit) = sequel_numeral(last)
    {
        *last = digit.to_string();
    }
    tokens.join(" ")
}

/// The digit a trailing sequel numeral stands for: `i`→`1`, `ii`→`2`,
/// `iii`→`3`, `iv`→`4`, `v`→`5` (plus `vi`–`x` for the longer series), so a
/// digit spelling and the roman spelling compare equal.
fn sequel_numeral(token: &str) -> Option<u32> {
    match token {
        "i" => Some(1),
        "ii" => Some(2),
        "iii" => Some(3),
        "iv" => Some(4),
        "v" => Some(5),
        "vi" => Some(6),
        "vii" => Some(7),
        "viii" => Some(8),
        "ix" => Some(9),
        "x" => Some(10),
        _ => None,
    }
}

/// `titleSimilarity` scores query against candidate in [0,1]. Exact normalized
/// equality is 1; otherwise 0.7*coverage (fraction of query tokens found in the
/// candidate) + 0.3*Jaccard, where the Jaccard term supplies the length penalty
/// for much-longer candidates. No query token in the candidate is 0, except
/// that unspaced scripts (CJK, where a comma becomes a space on one side only)
/// fall back to a character-level score so `你好，李焕英` and `你好李焕英`
/// still clear the floor. Both
/// denominators are DEDUPED token sets.
pub(crate) fn title_similarity(query: &str, candidate: &str) -> f64 {
    let nq = normalize_title(query);
    let nc = normalize_title(candidate);
    if nq == nc {
        return 1.0;
    }
    if nq.is_empty() || nc.is_empty() {
        return 0.0;
    }
    let qset: HashSet<&str> = nq.split_whitespace().collect();
    let cset: HashSet<&str> = nc.split_whitespace().collect();
    let inter = qset.iter().filter(|t| cset.contains(*t)).count();
    if inter == 0 {
        return unspaced_fallback(&nq, &nc);
    }
    let union = qset.len() + cset.len() - inter;
    let inter = inter as f64;
    let sim = 0.7 * inter / qset.len() as f64 + 0.3 * inter / union as f64;
    sim.clamp(0.0, 1.0)
}

/// Character-level fallback for unspaced scripts: when token overlap is empty
/// but either side carries CJK (Han, Hiragana, Katakana, Hangul), score the
/// spaceless character sets with the same 0.7/0.3 shape so a spacing-only
/// difference still clears the floor while unrelated pairs stay at 0.
fn unspaced_fallback(nq: &str, nc: &str) -> f64 {
    if !nq.chars().any(is_cjk) && !nc.chars().any(is_cjk) {
        return 0.0;
    }
    let qchars: HashSet<char> = nq.chars().filter(|c| *c != ' ').collect();
    let cchars: HashSet<char> = nc.chars().filter(|c| *c != ' ').collect();
    if qchars.is_empty() || cchars.is_empty() {
        return 0.0;
    }
    let inter = qchars.intersection(&cchars).count();
    if inter == 0 {
        return 0.0;
    }
    let union = qchars.len() + cchars.len() - inter;
    let inter = inter as f64;
    let sim = 0.7 * inter / qchars.len() as f64 + 0.3 * inter / union as f64;
    sim.clamp(0.0, 1.0)
}

/// Whether `c` sits in a CJK script block (Han, Hiragana, Katakana, Hangul,
/// Bopomofo): scripts without word spaces, where token overlap is the wrong
/// measure and characters carry the identity.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}'
        | '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}' | '\u{31F0}'..='\u{31FF}'
        | '\u{AC00}'..='\u{D7AF}' | '\u{1100}'..='\u{11FF}' | '\u{3100}'..='\u{312F}')
}

/// `pickBestShow` picks the candidate best matching query. Each candidate
/// scores its BEST similarity over `name` plus `aliases`, and the rest is
/// unchanged: the top-similarity candidate wins outright unless others fall
/// within [`MATCH_TIE_MARGIN`] of it (an INCLUSIVE `>= top - margin` window),
/// in which case the tie group breaks by higher Weight then higher
/// EpisodeCount then higher similarity, with a full tie keeping the earliest
/// index. Exact normalized matches always outrank near matches: ONE `sim == 1`
/// title (name or alias) anywhere excludes every candidate whose best is not
/// exact. `None` when the list is empty or the best similarity is below
/// [`MATCH_MIN_SIMILARITY`], so the resolver falls through.
pub fn pick_best_show(query: &str, cands: &[ShowCand]) -> Option<ShowCand> {
    if cands.is_empty() {
        return None;
    }
    let sims: Vec<f64> = cands
        .iter()
        .map(|c| {
            let mut best = title_similarity(query, &c.name);
            for alias in &c.aliases {
                best = best.max(title_similarity(query, alias));
            }
            best
        })
        .collect();
    let top = sims.iter().copied().fold(-1.0f64, f64::max);
    if top < MATCH_MIN_SIMILARITY {
        return None;
    }
    let has_exact = sims.contains(&1.0);
    let mut idx: Option<usize> = None;
    for (i, c) in cands.iter().enumerate() {
        if has_exact {
            if sims[i] != 1.0 {
                continue;
            }
        } else if sims[i] < top - MATCH_TIE_MARGIN {
            continue;
        }
        let Some(j) = idx else {
            idx = Some(i);
            continue;
        };
        let b = &cands[j];
        if c.weight != b.weight {
            if c.weight > b.weight {
                idx = Some(i);
            }
        } else if c.episode_count != b.episode_count {
            if c.episode_count > b.episode_count {
                idx = Some(i);
            }
        } else if sims[i] > sims[j] {
            idx = Some(i);
        }
    }
    idx.map(|i| cands[i].clone())
}
