
use metadata_core::match_::{ShowCand, pick_best_show};

/// Lowercase alphanumeric tokens (punctuation folds to spaces, like the
/// matcher's own normalization, minus accent folding -- so this check is
/// conservative: it may decline a swap, never force a wrong one). Shared with
/// [`crate::anilist`] so corroboration tokens and coverage tokens agree.
pub(crate) fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Full query-token coverage: every query token appears in the candidate.
/// The candidate covers the query when its display name OR any canonical
/// alias covers every query token, so an English display name can never hide
/// a romaji alias the query names (Sailor Moon Crystal judged by `Pretty
/// Guardian Sailor Moon Crystal` alone fails a romaji query its `Bishoujo
/// Senshi Sailor Moon Crystal` alias covers). Only canonical titles count:
/// providers whose match aliases carry unverified synonyms (AniList) pass a
/// canonical-only view, so an arbitrary synonym can never authorize a year
/// override for an unrelated entry (`Monster`/2019 must keep the 2004 series
/// `19`, not the `Golden Kamuy` OVA `124756` whose synonym is `Monster`).
fn covers_candidate(query: &str, cand: &ShowCand) -> bool {
    std::iter::once(cand.name.as_str())
        .chain(cand.aliases.iter().map(String::as_str))
        .any(|title| covers_title(query, title))
}

/// Full query-token coverage: every query token appears in the title.
fn covers_title(query: &str, candidate: &str) -> bool {
    let q = tokens(query);
    if q.is_empty() {
        return false;
    }
    let c = tokens(candidate);
    q.iter().all(|t| c.contains(t))
}

/// The index to take: `winner`, unless `query_year > 0`, the winner's own
/// year disagrees, and some other candidate both carries the query year and
/// fully covers the query title -- then the best such challenger (ranked by
/// [`pick_best_show`] over the year-matching subset, so weight/popularity
/// still breaks ties among them).
pub(crate) fn prefer_year_match(
    query: &str,
    query_year: i32,
    cands: &[ShowCand],
    years: &[i32],
    winner: usize,
) -> usize {
    if query_year <= 0 || winner >= cands.len() || cands.len() != years.len() {
        return winner;
    }
    if years[winner] == query_year {
        return winner;
    }
    let mut sub: Vec<ShowCand> = Vec::new();
    let mut back: Vec<usize> = Vec::new();
    for (i, cand) in cands.iter().enumerate() {
        if years[i] == query_year && covers_candidate(query, cand) {
            sub.push(cand.clone());
            back.push(i);
        }
    }
    if sub.is_empty() {
        return winner;
    }
    let Some(alt) = pick_best_show(query, &sub) else {
        return winner;
    };
    sub.iter().position(|c| *c == alt).map(|at| back[at]).unwrap_or(winner)
}
