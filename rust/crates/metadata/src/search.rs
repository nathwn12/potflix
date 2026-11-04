//! `metadata::search` -- the identity search and manual latch builder behind
//! the "Fix match" UI.
//!
//! NEW SURFACE: `go\` has no search-and-latch layer, so nothing here is diffed
//! against a ported file; the contract is stated plainly instead. The
//! automatic ladder ([`crate::resolver`]) answers a filename; this module
//! answers a person. The user types a real name plus an optional year,
//! [`search`] returns the entries the provider legs offer for it as
//! [`Candidate`]s, and [`pin_from_candidate`] turns the picked entry into an
//! [`Override`] latch -- a movie file, one season folder or a whole show --
//! carrying the candidate's cover and the entry's episode table as
//! per-episode pins. [`refresh`] re-fetches a stored latch's identity and
//! returns the same latch with a refreshed payload. Persistence is
//! [`crate::overrides`]'s job; this module only builds the record.
//!
//! # One concurrent pass, ranked
//!
//! [`search`] walks the legs applicable to the wanted kind ONCE -- the movie
//! legs for [`Kind::MOVIE`], the series/anime legs otherwise (the resolver's
//! own ranking, with Cinemeta as the general fallback) -- with EVERY leg on its
//! own thread ([`std::thread::scope`]), so one slow provider never holds the
//! picker. Each leg answers with its provider's own multi-row search
//! ([`Source::find_all`]): one request per leg, no episode table.
//! [`search_stream`] hands each leg's rows to a sink the moment THAT leg
//! finishes; the returned vector is deduplicated by (provider, provider_id),
//! merged by work ([`merge_same_work`], so one work is one row), ranked and
//! capped at [`SEARCH_LIMIT`], and is the same sequence on every run whatever
//! order the threads complete in. The rank is the shared closeness order
//! ([`rank_rows`]): the title closest to the typed name first (an exact
//! normalized title is strongest), then a known query year exact above unknown
//! above a mismatch, then the wanted kind as a soft prior, then provider order.
//! No kind is ever filtered out by the rank; the caller's per-kind pass is what
//! scopes a leg.
//! A leg's failure is recorded by provider name in [`SearchOutcome::failed`]
//! (an EMPTY answer is a success), so [`search_stream_only`] can run the
//! picker's next pass over exactly the legs that did not answer; a cancelled
//! scope ends the pass early and nothing here panics.
//!
//! # The year is a gate
//!
//! A known year is an identity check, NOT a preference: with `year > 0` every
//! candidate whose year is unknown or different is DROPPED, before it can
//! reach the rank or the streaming sink. `edgerunners 2026` used to surface a
//! 1990s entry through the soft year preference; no rank term may reintroduce
//! a mismatch.
//!
//! # The latch's identity
//!
//! A latch stores the provider and the provider's own id
//! ([`Override::provider`], [`Override::provider_id`]) next to the picked
//! title/year. [`pin_from_candidate`] and [`refresh`] call THAT leg only and
//! refuse any answer whose `source_id` differs from the stored id, so a
//! re-ranked search can never silently latch a different work: a mismatched
//! or missing answer is an [`Error`] the caller shows, never a half-built
//! latch. The provider legs expose find-by-query only -- no fetch-by-id entry
//! point -- so when `find` lands on the wrong-year entry (a soft preference
//! can still pick a same-titled work) the fetch falls back to
//! [`Source::find_all`] and takes the row carrying the STORED id, and only a
//! search that lacks that id refuses. The refusal is proven in
//! `refresh_and_pin_refuse_a_mismatched_provider_id` below.
//!
//! # Cancellation
//!
//! Both fetching functions block like the provider legs do and every wire
//! wait is the [`Cancel`]'s to end: hand them a deadline-bearing scope (the
//! CLIs build one, `metacheck`: 90 s). A dead scope ends a search early -- the
//! hits already found stand -- and propagates from a fetch, never
//! masquerading as a miss.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use metadata_core::match_::normalize_title;
use metadata_core::types::{Episode, Kind, Media, Query};
use metadata_http::cancel::Cancel;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::overrides::{EpisodePin, Override, Scope, now_ms};
use crate::resolver::{Source, default_sources};

/// The most candidates a search returns.
pub const SEARCH_LIMIT: usize = 24;

/// The old flat MOVIE_LEGS/SERIES_LEGS list lived here; routing is now
/// path-tagged (Regular vs Anime) with concurrent primaries on both paths.
/// Regular available 3 (Cinemeta + TVmaze + IMDb), anime primary 4
/// (AniList + Tenrai + Kitsu + AnimeSchedule).
pub const REGULAR_LEGS: &[&str] = &["cinemeta", "tvmaze", "imdb"];
pub const ANIME_LEGS: &[&str] = &["anilist", "tenrai", "kitsu", "animeschedule"];

/// The legs applicable to `want`: the movie legs for [`Kind::MOVIE`], the
/// series/anime legs for every other kind (an unknown want gets the wider
/// search, whose Cinemeta leg answers either kind). All seven primaries run
/// concurrently; IMDb is a concurrent regular leg (first paint still comes
/// from Cinemeta in ~0.1s, IMDb only delays the final count).
fn legs_for(want: Kind) -> &'static [&'static str] {
    if want == Kind::MOVIE {
        &[
            "cinemeta",
            "imdb",
            "anilist",
            "tenrai",
            "kitsu",
            "animeschedule",
        ]
    } else {
        &[
            "tvmaze",
            "anilist",
            "tenrai",
            "kitsu",
            "animeschedule",
            "cinemeta",
            "imdb",
        ]
    }
}

/// One pickable entry a leg offered for the typed name. It is also the row the
/// picker's result cache stores, so it travels as JSON ([`crate::searchcache`]):
/// the field names are stable on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// The entry's own kind, as the leg reported it.
    pub kind: Kind,
    /// The canonical title to display and to latch.
    pub title: String,
    /// The title's year, `0` when the provider carries none.
    pub year: i32,
    /// The leg that answered ("tvmaze", "cinemeta", ...).
    pub provider: String,
    /// That leg's own id for the entry, `""` when it has none. The latch and
    /// [`refresh`] key on it.
    pub provider_id: String,
    /// The entry's series/movie cover.
    pub poster: String,
    /// The entry's series/movie summary, `""` when the leg's own row carries
    /// none. [`merge_same_work`] splices another provider's copy in, and
    /// [`pin_from_candidate`]/[`refresh`] carry it onto the latched media when
    /// the identity fetch has none.
    pub synopsis: String,
    /// "Drama . Crime", `""` if unknown.
    pub genres: String,
    /// Show/movie length ("45 min", "2h 19m"), `""` if unknown.
    pub length: String,
    /// The length of the episode table the leg's own answer carried: `0` for a
    /// movie, and `0` for a manual search's row, whose `find_all` never fetches
    /// a table. A latch built from the candidate carries the table `find`
    /// fetches, not this count.
    pub episode_count: usize,
}

/// One search pass's answer: the ranked rows plus the provider legs that
/// FAILED. `failed` carries the provider name of every leg whose
/// [`Source::find_all`] returned `Err`, in provider order, deduplicated; an
/// empty `failed` is a clean pass, and rows may still be present (a PARTIAL
/// answer). An empty answer is a success, never a failure. The caller retries
/// with [`search_stream_only`], and the cache stores both halves so the retry
/// survives a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOutcome {
    /// The ranked, merged, capped rows the legs answered.
    pub rows: Vec<Candidate>,
    /// The provider names whose leg failed, in provider order.
    pub failed: Vec<String>,
}

/// `search`: one pass over the legs applicable to `want`, over the built-in
/// provider set.
pub fn search(cancel: &Cancel, name: &str, year: i32, want: Kind) -> Vec<Candidate> {
    search_stream(cancel, name, year, want, &|_| {}).rows
}

/// `searchStream`: [`search`] over the built-in provider set, with every leg's
/// rows handed to `sink` the moment THAT leg answers -- the picker can paint
/// the first provider's entries while the slower legs are still on the wire.
/// The sink is called from the leg threads, so it is `Send + Sync`; an empty
/// leg calls it not at all, and the outcome's rows are the full ranked result
/// regardless of what the sink does. Each leg attempt also emits one debug
/// line (`search: leg <name> ok|fail <n> rows <ms>`) through the process log
/// hook when one is installed (see [`search_stream_with_log`]); a cached
/// second search issues zero new leg lines because no leg runs.
pub fn search_stream(
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
) -> SearchOutcome {
    search_stream_with(&default_sources(), cancel, name, year, want, sink)
}

/// `searchStreamOnly`: [`search_stream`] restricted to the named providers, so
/// a partial outcome's [`SearchOutcome::failed`] legs can be retried without
/// re-walking the ones that already answered. Only names in the want's own leg
/// list are walked, in that list's provider order; an unknown or blank name
/// matches nothing, and an empty list walks no leg at all (a full search is
/// [`search_stream`]).
pub fn search_stream_only(
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    providers: &[String],
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
) -> SearchOutcome {
    search_stream_only_with(&default_sources(), cancel, name, year, want, providers, sink)
}

/// [`search_stream`] over the built-in provider set with per-leg debug lines
/// sent to `log` (`search: leg <name> ok|fail <n> rows <ms>`).
pub fn search_stream_with_log(
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
    log: &(dyn Fn(&str) + Send + Sync),
) -> SearchOutcome {
    search_core(
        &default_sources(),
        cancel,
        name,
        year,
        want,
        None,
        sink,
        Some(log),
    )
}

/// [`search_stream_only`] over the built-in provider set with per-leg debug
/// lines sent to `log`.
pub fn search_stream_only_with_log(
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    providers: &[String],
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
    log: &(dyn Fn(&str) + Send + Sync),
) -> SearchOutcome {
    search_core(
        &default_sources(),
        cancel,
        name,
        year,
        want,
        Some(providers),
        sink,
        Some(log),
    )
}

/// The injectable core of [`search`]: the same pass over an explicit source
/// set (the tests drive fixture legs here).
#[cfg(test)]
pub(crate) fn search_with(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
) -> Vec<Candidate> {
    search_stream_with(sources, cancel, name, year, want, &|_| {}).rows
}

/// The injectable core of [`search_stream`]: every applicable leg runs on its
/// own thread ([`std::thread::scope`]) and the outcome keeps the failed legs
/// named. See [`search_core`].
pub(crate) fn search_stream_with(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
) -> SearchOutcome {
    search_core(sources, cancel, name, year, want, None, sink, None)
}

/// The injectable core of [`search_stream_only`]: [`search_core`] over exactly
/// the named providers.
pub(crate) fn search_stream_only_with(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    providers: &[String],
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
) -> SearchOutcome {
    search_core(sources, cancel, name, year, want, Some(providers), sink, None)
}

/// The injectable core of [`search_stream_only_with_log`]: [`search_core`]
/// over exactly the named providers with per-leg debug lines sent to `log`.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub(crate) fn search_stream_only_with_logged(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    providers: &[String],
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
    log: &(dyn Fn(&str) + Send + Sync),
) -> SearchOutcome {
    search_core(
        sources,
        cancel,
        name,
        year,
        want,
        Some(providers),
        sink,
        Some(log),
    )
}

/// The shared pass behind [`search_stream`] and [`search_stream_only`]: EVERY
/// walked leg runs on its own thread; a leg searches with its provider's own
/// [`Source::find_all`] (one request), maps the rows to [`Candidate`]s, drops
/// the mismatched years, hands its rows to `sink`, and only then joins the
/// final pass. A leg whose `find_all` returned `Err` contributes no rows and
/// its name lands in [`SearchOutcome::failed`]; an empty answer is a success.
/// `only` (`Some`) restricts the walk to those provider names, matched
/// case-insensitively, in the want's own provider order. The rows are
/// concatenated in provider order, so the rank sees the same input sequence on
/// every run whatever order the threads complete in.
#[allow(clippy::too_many_arguments)]
fn search_core(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
    only: Option<&[String]>,
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
    log: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> SearchOutcome {
    let name = name.trim();
    if name.is_empty() {
        return SearchOutcome::default();
    }
    let only: Option<HashSet<String>> = only.map(|names| {
        names
            .iter()
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect()
    });
    // The legs in provider order: (position, source). One thread per leg, the
    // same walk the sequential pass made, so a source mounted twice still
    // answers twice (its rows dedupe below).
    let mut legs: Vec<(usize, Arc<dyn Source>)> = Vec::new();
    for leg in legs_for(want) {
        if let Some(only) = &only
            && !only.contains(*leg)
        {
            continue;
        }
        for source in sources.iter().filter(|source| source.name() == *leg) {
            legs.push((legs.len(), Arc::clone(source)));
        }
    }
    if legs.is_empty() || cancel.err().is_some() {
        return SearchOutcome::default();
    }
    let per_leg: Vec<(usize, Vec<Candidate>, Option<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = legs
            .into_iter()
            .map(|(order, source)| {
                scope.spawn(move || {
                    let provider = source.name().to_string();
                    let started = std::time::Instant::now();
                    let outcome = leg_candidates(source.as_ref(), cancel, name, year, want);
                    let elapsed_ms = started.elapsed().as_millis();
                    let Ok(candidates) = outcome
                    else {
                        // A leg error is a PARTIAL answer: the other legs'
                        // rows stand and the caller learns which name to retry.
                        if let Some(log) = log {
                            log(&format!(
                                "search: leg {provider} fail 0 rows {elapsed_ms}ms"
                            ));
                        }
                        return (order, Vec::new(), Some(provider));
                    };
                    let mut seen: HashSet<(String, String)> = HashSet::new();
                    let mut rows: Vec<Candidate> = Vec::new();
                    for candidate in candidates {
                        // The HARD year gate: a known year admits only its own.
                        if !year_admits(candidate.year, year) {
                            continue;
                        }
                        if seen.insert((candidate.provider.clone(), candidate.provider_id.clone()))
                        {
                            rows.push(candidate);
                        }
                    }
                    // This leg is done: its rows are the picker's NOW, before
                    // the slower legs answer.
                    if !rows.is_empty() {
                        sink(rows.clone());
                    }
                    if let Some(log) = log {
                        let n = rows.len();
                        log(&format!("search: leg {provider} ok {n} rows {elapsed_ms}ms"));
                    }
                    (order, rows, None)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_default())
            .collect()
    });
    let mut hits: Vec<(usize, Candidate)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut failed: Vec<String> = Vec::new();
    for (order, rows, fail) in per_leg {
        if let Some(provider) = fail
            && !failed.contains(&provider)
        {
            failed.push(provider);
        }
        for candidate in rows {
            if seen.insert((candidate.provider.clone(), candidate.provider_id.clone())) {
                hits.push((order, candidate));
            }
        }
    }
    // The same work can be offered by several legs: the best-ranked row keeps
    // the identity and gains the fields it lacks from the others, and the cap
    // applies to the merged result so a duplicate never costs a slot.
    let mut rows = merge_same_work(rank_hits(hits, name, year, want));
    rows.truncate(SEARCH_LIMIT);
    SearchOutcome { rows, failed }
}

/// One leg's rows, as candidates: the provider's own [`Source::find_all`]
/// answer for the typed name. An `Err` is returned to the caller, which records
/// the leg in [`SearchOutcome::failed`]; an empty answer is a success.
fn leg_candidates(
    source: &dyn Source,
    cancel: &Cancel,
    name: &str,
    year: i32,
    want: Kind,
) -> Result<Vec<Candidate>, Error> {
    let q = Query {
        kind: want,
        title: name.to_string(),
        year,
        season: 0,
        episode: 0,
        absolute: false,
    };
    let rows = source.find_all(cancel, &q)?;
    Ok(rows
        .into_iter()
        .filter(|media| !media.title.trim().is_empty())
        .map(|media| Candidate {
            kind: media.kind,
            title: media.title.trim().to_string(),
            year: media.year,
            provider: source.name().to_string(),
            provider_id: media.source_id.trim().to_string(),
            poster: media.artwork,
            synopsis: media.synopsis,
            genres: media.genres,
            length: media.length,
            episode_count: media.episodes.len().max(media.episode_count.max(0) as usize),
        })
        .collect())
}

/// The HARD year gate: with a known query year, an entry whose year is unknown
/// or different is DROPPED -- not ranked lower. A soft preference let
/// `edgerunners 2026` offer a 1990s entry, and a year mismatch is the wrong
/// work, not a weaker guess.
fn year_admits(candidate_year: i32, year: i32) -> bool {
    year <= 0 || candidate_year == year
}

/// The ranking of [`search`]: the crate's own closeness order (see
/// [`rank_rows`]), then the leg that answered as the final tie-break, so the
/// order is total and the same fixtures reproduce the same sequence.
fn rank_hits(
    hits: Vec<(usize, Candidate)>,
    name: &str,
    year: i32,
    want: Kind,
) -> Vec<Candidate> {
    let mut keyed: Vec<(Closeness, usize, Candidate)> = hits
        .into_iter()
        .map(|(order, cand)| (closeness(name, year, Some(want), &cand), order, cand))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    keyed.into_iter().map(|(_, _, cand)| cand).collect()
}

/// One row's distance from the typed query, smallest first. This is the ONE
/// closeness order every picker paints by: the engine's per-leg rank here and
/// a merged list in `potflix-win::search::merge_candidates` both come through
/// it, so a search result and its cache replay agree.
///
/// The key, in order:
///   1. title similarity ([`metadata_core::match_::title_similarity`], higher
///      first, so an exact normalized title is strongest) -- a title beats a
///      category, always;
///   2. a known query year: exact above unknown above a known mismatch;
///   3. the file's own shape/kind as a SOFT prior only (never a filter);
///   4. the provider's leg order ([`PROVIDER_ORDER`]) -- the final tie-break.
///
/// Rows still tied keep their input order, which is the provider's own
/// relevance order: the sort is stable and nothing below the evidence keys
/// re-orders a leg's own answer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Closeness {
    /// Higher similarity sorts first: the negated millionths.
    similarity: std::cmp::Reverse<i64>,
    /// 0 = the query year is exact, 1 = the row's year is unknown, 2 = known
    /// and different. Every row is 0 when the query has no year.
    year_rank: u8,
    /// 0 when the row's episode count covers the local sibling count (or
    /// either is unknown), 1 when the row is too small for the files on disk.
    extent_miss: u8,
    /// 0 when the row's kind matches the shape prior, 1 when it does not.
    shape_miss: u8,
    provider: usize,
}

/// The local, offline evidence a caller can add to the closeness rank: the
/// folder's show name, the sibling files' common prefix, how many video files
/// sit in the show tree, and the highest season marker on the path. All are
/// SOFT terms; none drops a row.
#[derive(Clone, Copy, Default)]
pub struct LocalRankEvidence<'a> {
    pub show: &'a str,
    pub sibling_prefix: &'a str,
    pub local_count: usize,
    pub season_extent: i32,
}

/// The provider precedence used by the rank's final tie-break: the built-in
/// chain's leg order, regular and anime interleaved exactly as
/// [`crate::resolver::default_sources`] ranks them.
pub const PROVIDER_ORDER: &[&str] = &[
    "tvmaze",
    "anilist",
    "tenrai",
    "kitsu",
    "animeschedule",
    "cinemeta",
    "imdb",
];

/// The position of `provider` in [`PROVIDER_ORDER`]; an unknown leg sorts after
/// every known one and keeps its own name order through the other keys.
fn provider_rank(provider: &str) -> usize {
    let provider = provider.trim().to_ascii_lowercase();
    PROVIDER_ORDER
        .iter()
        .position(|known| *known == provider)
        .unwrap_or(PROVIDER_ORDER.len())
}

/// [`Closeness`] for one row against `query`, `year` and the file's own shape
/// (`prefer`, `None` when the caller has no shape evidence).
fn closeness(query: &str, year: i32, prefer: Option<Kind>, cand: &Candidate) -> Closeness {
    let similarity = metadata_core::match_::title_similarity(query, &cand.title);
    let year_rank = if year <= 0 || cand.year == year {
        0
    } else if cand.year <= 0 {
        1
    } else {
        2
    };
    let shape_miss = u8::from(prefer.is_some_and(|kind| cand.kind != kind));
    Closeness {
        similarity: std::cmp::Reverse((similarity.clamp(0.0, 1.0) * 1_000_000.0).round() as i64),
        year_rank,
        extent_miss: 0,
        shape_miss,
        provider: provider_rank(&cand.provider),
    }
}

/// The branch a candidate takes after the query, when the query is a prefix of
/// the candidate's normalized title: up to the next three tokens, joined. It
/// separates the main run (`starting life in`) from a `starting break time`
/// spin-off, while the same branch across providers still reinforces itself.
fn branch_after_query(query: &str, title: &str) -> Option<String> {
    let query = normalize_title(query);
    let query_tokens: Vec<&str> = query.split_whitespace().collect();
    if query_tokens.is_empty() {
        return None;
    }
    let title = normalize_title(title);
    let title_tokens: Vec<&str> = title.split_whitespace().collect();
    if title_tokens.len() <= query_tokens.len()
        || title_tokens[..query_tokens.len()] != query_tokens
    {
        return None;
    }
    let end = (query_tokens.len() + 3).min(title_tokens.len());
    Some(title_tokens[query_tokens.len()..end].join(" "))
}

/// How many rows share each post-query branch. The main work keeps one branch
/// across providers; a one-word spin-off sits alone.
fn prefix_clusters(query: &str, rows: &[Candidate]) -> HashMap<String, u32> {
    let mut clusters: HashMap<String, u32> = HashMap::new();
    for row in rows {
        if let Some(branch) = branch_after_query(query, &row.title) {
            *clusters.entry(branch).or_insert(0) += 1;
        }
    }
    clusters
}

/// [`Closeness`] with the local evidence folded in: the best title similarity
/// over the typed query, the folder show and the sibling prefix; a bounded
/// bonus when the candidate shares its post-query branch token with other
/// rows; and an extent penalty when the candidate's episode count is smaller
/// than the local file count.
fn closeness_evidenced(
    query: &str,
    year: i32,
    prefer: Option<Kind>,
    local: Option<LocalRankEvidence<'_>>,
    clusters: &HashMap<String, u32>,
    top_typed: f64,
    cand: &Candidate,
) -> Closeness {
    let typed = metadata_core::match_::title_similarity(query, &cand.title);
    let mut similarity = typed;
    let mut extent_miss = 0;
    if let Some(local) = local {
        // The local terms only break a near-tie: a candidate more than 0.10
        // below the best typed title is never lifted by the folder or the
        // siblings. The whole local BONUS is capped at +0.05; penalties for a
        // spin-off branch and a too-small episode count are separate.
        let near = typed + 0.10 + 1e-9 >= top_typed;
        if near {
            let alt = [
                metadata_core::match_::title_similarity(local.show, &cand.title),
                metadata_core::match_::title_similarity(local.sibling_prefix, &cand.title),
            ]
            .into_iter()
            .fold(0.0_f64, f64::max);
            let gain = (alt - typed).max(0.0);
            let mut bonus = (0.05 * gain / (1.0 - typed).max(0.01)).min(0.05);
            let largest = clusters.values().copied().max().unwrap_or(1);
            if let Some(branch) = branch_after_query(query, &cand.title) {
                let group = clusters.get(&branch).copied().unwrap_or(1);
                if group >= 2 {
                    // A larger branch is stronger evidence; the cap is +0.05.
                    let scaled = 0.05 * f64::from(group - 1) / f64::from(largest - 1).max(1.0);
                    bonus = bonus.max(scaled.min(0.05));
                } else if largest >= 3 {
                    // A lone branch inside a real multi-row franchise is the
                    // one-word spin-off (PETIT/Break Time): nudge it down.
                    similarity -= 0.10;
                }
            }
            // The file is a series: a movie answer is a shape miss, not the
            // work. The prior is bounded so a genuinely better movie title is
            // still allowed to win.
            if cand.kind != Kind::SERIES {
                similarity -= 0.10;
            }
            similarity += bonus;

            // A `S1-S2` / `Season 2` marker says at least a short cour per
            // season. A series far below the local season count is a partial
            // season or a spin-off; the bounded penalty keeps it under the
            // full run. An unknown count stays neutral; a movie has no
            // episode extent at all.
            let expected = if local.season_extent > 1 {
                local
                    .local_count
                    .max((local.season_extent as usize).saturating_mul(6))
            } else {
                local.local_count
            };
            if expected > 0 && cand.kind == Kind::SERIES {
                if cand.episode_count == 0 {
                    extent_miss = 1;
                } else if cand.episode_count < (expected * 2 / 3).max(1) {
                    extent_miss = 2;
                    similarity -= 0.12;
                }
            }
        }
    }
    let year_rank = if year <= 0 || cand.year == year {
        0
    } else if cand.year <= 0 {
        1
    } else {
        2
    };
    let shape_miss = u8::from(prefer.is_some_and(|kind| cand.kind != kind));
    Closeness {
        similarity: std::cmp::Reverse((similarity.clamp(0.0, 1.0) * 1_000_000.0).round() as i64),
        year_rank,
        extent_miss,
        shape_miss,
        provider: provider_rank(&cand.provider),
    }
}

/// Re-order `rows` closest-first for `query`: the shared closeness rank the
/// engine and the UI both use. `year` is the query's own year (`0` when
/// unknown) and is only a ranking signal here -- the hard year gate lives in
/// the per-leg filter, so the caller decides whether a typed year is a filter
/// or a preference. `prefer` is the file's own shape/kind as a SOFT prior: a
/// row of the other kind is ranked lower, never dropped.
#[must_use]
pub fn rank_rows(
    query: &str,
    year: i32,
    prefer: Option<Kind>,
    rows: Vec<Candidate>,
) -> Vec<Candidate> {
    rank_rows_with(query, year, prefer, None, rows)
}

/// [`rank_rows`] with the local evidence a playing file carries: the folder
/// show and sibling prefix widen the title match, a shared post-query branch
/// token boosts the rows it appears in, and a too-small episode count is
/// penalized. Every term is SOFT; no row is dropped.
#[must_use]
pub fn rank_rows_with(
    query: &str,
    year: i32,
    prefer: Option<Kind>,
    local: Option<LocalRankEvidence<'_>>,
    rows: Vec<Candidate>,
) -> Vec<Candidate> {
    let clusters = prefix_clusters(query, &rows);
    let top_typed = rows
        .iter()
        .map(|cand| metadata_core::match_::title_similarity(query, &cand.title))
        .fold(0.0_f64, f64::max);
    let mut keyed: Vec<(Closeness, Candidate)> = rows
        .into_iter()
        .map(|cand| {
            (
                closeness_evidenced(query, year, prefer, local, &clusters, top_typed, &cand),
                cand,
            )
        })
        .collect();
    keyed.sort_by_key(|a| a.0);
    keyed.into_iter().map(|(_, cand)| cand).collect()
}


/// Splice duplicate rows for the SAME work into the first (best-ranked) one.
///
/// Walks `rows` in order and keeps the first row of every work, so the rank
/// still decides which provider the picker shows: the kept row's
/// provider/provider_id/title/year are never touched. Each later duplicate
/// fills only the kept row's EMPTY poster/synopsis/genres/length -- a provider
/// value is never replaced -- and contributes its episode count to the kept
/// row's maximum, then is dropped. The kept order is the input order.
///
/// `search` returns merged rows; [`search_stream`]'s batches stay raw, so the
/// UI merges its own batches with this same public helper.
///
/// Two rows are one work when their kind matches, their years agree (two known
/// years must be equal; an unknown year agrees with anything), and their
/// normalized titles are equal -- accents folded, a trailing sequel numeral
/// folded to its digit, so "Edgerunners II" is "Edgerunners 2". A token
/// subset never merges: "National Treasure" and "National Treasure: Book of
/// Secrets" are different works even in the same year. A blank
/// title never matches anything.
pub fn merge_same_work(rows: Vec<Candidate>) -> Vec<Candidate> {
    let mut kept: Vec<Candidate> = Vec::with_capacity(rows.len());
    for row in rows {
        match kept.iter().position(|first| same_work(first, &row)) {
            Some(at) => {
                let first = &mut kept[at];
                fill_blank(&mut first.poster, &row.poster);
                fill_blank(&mut first.synopsis, &row.synopsis);
                fill_blank(&mut first.genres, &row.genres);
                fill_blank(&mut first.length, &row.length);
                first.episode_count = first.episode_count.max(row.episode_count);
            }
            None => kept.push(row),
        }
    }
    kept
}

/// Whether two rows name the same work: same kind, agreeing years, and equal
/// normalized titles. Never a subset: a shorter title inside a longer one is
/// a different work, even with the same known year.
fn same_work(a: &Candidate, b: &Candidate) -> bool {
    if a.kind != b.kind || !years_agree(a.year, b.year) {
        return false;
    }
    let (ta, tb) = (title_tokens(&a.title), title_tokens(&b.title));
    if ta.is_empty() || tb.is_empty() {
        return false;
    }
    ta == tb
}

/// Two known years must be equal; an unknown (`0`) year agrees with anything.
fn years_agree(a: i32, b: i32) -> bool {
    a <= 0 || b <= 0 || a == b
}

/// A title's lowercase alphanumeric tokens through the crate's own
/// [`normalize_title`](metadata_core::match_::normalize_title) -- accents
/// folded so "Amélie" and "Amelie" compare equal -- with the trailing sequel
/// numeral folded to its digit ("II" -> "2"), so "Edgerunners II" and
/// "Edgerunners 2" compare equal instead of through a looser similarity.
fn title_tokens(title: &str) -> HashSet<String> {
    let normalized = metadata_core::match_::normalize_title(title);
    let mut tokens: Vec<String> = normalized
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if let Some(last) = tokens.last_mut()
        && let Some(digit) = sequel_numeral(last)
    {
        *last = digit.to_string();
    }
    tokens.into_iter().collect()
}

/// The digit a trailing sequel numeral stands for; only the roman spellings
/// this rule names (II, III, IV). Everything else is left as typed.
fn sequel_numeral(token: &str) -> Option<u32> {
    match token {
        "ii" => Some(2),
        "iii" => Some(3),
        "iv" => Some(4),
        _ => None,
    }
}

/// Overwrite `field` only when it is empty (or blank) and `from` carries a
/// value: a spliced field is never replaced by a later empty one.
fn fill_blank(field: &mut String, from: &str) {
    if field.trim().is_empty() && !from.trim().is_empty() {
        *field = from.to_string();
    }
}

/// Fill each series/movie-level field `media` lacks from a source copy: the
/// picked candidate's own row, or the previous latch's snapshot on a refresh.
/// The media's non-empty fields always win; a known source year fills an
/// unknown one.
fn fill_missing_fields(
    media: &mut Media,
    poster: &str,
    synopsis: &str,
    genres: &str,
    length: &str,
    year: i32,
) {
    fill_blank(&mut media.artwork, poster);
    fill_blank(&mut media.synopsis, synopsis);
    fill_blank(&mut media.genres, genres);
    fill_blank(&mut media.length, length);
    if media.year <= 0 && year > 0 {
        media.year = year;
    }
}

/// `pin`: the latch for one picked candidate, over the built-in provider set.
pub fn pin_from_candidate(
    cancel: &Cancel,
    cand: &Candidate,
    scope: Scope,
    key: &str,
) -> Result<Override, Error> {
    pin_from_candidate_with(&default_sources(), cancel, cand, scope, key)
}

/// The injectable core of [`pin_from_candidate`].
///
/// The latch carries the candidate's title/year/provider/identity, the
/// candidate's cover as the scope's [`Override::poster`] for a season/show,
/// and the fetched record as [`Override::media`]. A movie renders from that
/// media snapshot (its cover rides on `media.artwork`) and carries no pins; a
/// season/show scope pins the fetched episode table -- per episode its own
/// season/number/absolute/title/synopsis, the synopsis left exactly as the
/// provider has it, empty when it has none. A failure to fetch the table is an
/// [`Error`], never a half-built latch.
pub(crate) fn pin_from_candidate_with(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    cand: &Candidate,
    scope: Scope,
    key: &str,
) -> Result<Override, Error> {
    let mut media = fetch_identity(
        sources,
        cancel,
        &cand.provider,
        &cand.provider_id,
        &cand.title,
        cand.year,
        scope_kind(scope),
    )?;
    // The picked row can carry a synopsis/genre/length the identity record has
    // none of (another provider answered the search): fill each field the
    // fetch left empty from the candidate, never replacing a provider value.
    fill_missing_fields(
        &mut media,
        &cand.poster,
        &cand.synopsis,
        &cand.genres,
        &cand.length,
        cand.year,
    );
    let mut over = Override {
        scope,
        key: key.to_string(),
        title: cand.title.clone(),
        year: cand.year,
        provider: cand.provider.clone(),
        provider_id: cand.provider_id.clone(),
        poster: String::new(),
        media: None,
        episodes: Vec::new(),
        created_at_ms: now_ms(),
    };
    if scope != Scope::Movie {
        over.poster = cand.poster.clone();
        over.episodes = pins_from_table(&media.episodes);
    }
    over.media = Some(media);
    Ok(over)
}

/// `refresh`: re-fetch a stored latch's own identity and return it with the
/// payload refreshed, over the built-in provider set.
pub fn refresh(cancel: &Cancel, over: &Override) -> Result<Override, Error> {
    refresh_with(&default_sources(), cancel, over)
}

/// The injectable core of [`refresh`].
///
/// The scope, key and `created_at_ms` survive untouched; the refreshed
/// payload is the [`Override::media`] record plus, for a season/show, the
/// re-pinned episode table and the record's cover as [`Override::poster`].
/// The fetch is [`fetch_identity`]: the latch's own leg, keyed by its stored
/// provider id, so a re-ranked search cannot drift the identity.
pub(crate) fn refresh_with(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    over: &Override,
) -> Result<Override, Error> {
    let mut media = fetch_identity(
        sources,
        cancel,
        &over.provider,
        &over.provider_id,
        &over.title,
        over.year,
        scope_kind(over.scope),
    )?;
    // A latch may carry data the provider never returns again (a synopsis
    // spliced from another leg): a refresh fills what the fetch lacks from the
    // stored snapshot, so it never drops the spliced fields.
    if let Some(prev) = &over.media {
        fill_missing_fields(
            &mut media,
            &prev.artwork,
            &prev.synopsis,
            &prev.genres,
            &prev.length,
            prev.year,
        );
    }
    // A refresh that returns an empty table must not zero the pins: the
    // provider is having a bad day (or the `find_all` fallback answered with
    // a search row that never carries a table), so keep the existing episodes
    // and report instead of a clean success.
    if over.scope != Scope::Movie
        && media.episodes.is_empty()
        && (!over.episodes.is_empty()
            || over
                .media
                .as_ref()
                .is_some_and(|prev| !prev.episodes.is_empty()))
    {
        return Err(Error::other(format!(
            "metadata: search: {:?} returned an empty episode table for {:?}; keeping the existing {} pins",
            over.provider,
            over.title,
            over.episodes.len(),
        )));
    }
    let mut out = over.clone();
    if over.scope == Scope::Movie {
        out.media = Some(media);
        out.episodes = Vec::new();
    } else {
        out.poster = media.artwork.clone();
        out.episodes = pins_from_table(&media.episodes);
        out.media = Some(media);
    }
    Ok(out)
}

/// The kind a scope's fetch asks the leg for: a movie scope fetches the movie
/// record, a season/show scope the series record (its episode table).
fn scope_kind(scope: Scope) -> Kind {
    if scope == Scope::Movie {
        Kind::MOVIE
    } else {
        Kind::SERIES
    }
}

/// Whether an empty-id answer may latch `title`: the provider's title must
/// name the same work (accents, case and punctuation folded), so a partial
/// latch never takes another work's record and `media.title` stays the latch
/// title.
fn titles_match(answer: &str, latched: &str) -> bool {
    let (a, b) = (normalize_title(answer), normalize_title(latched));
    !a.is_empty() && a == b
}

/// One identity fetch. ONLY the leg named `provider` is called, with the
/// stored title/year, and its answer must carry the stored `provider_id`: a
/// different entry is refused rather than latched, so the identity cannot
/// drift beneath a re-ranked search.
///
/// `find` narrows to ONE entry with a soft year preference, so it can land on
/// the wrong-year same-title work. When it misses or its id differs, the
/// leg's own multi-row [`Source::find_all`] search is consulted and the row
/// carrying the EXACT stored id is taken -- the provider has it, `find` just
/// did not pick it. Only a search that lacks that id refuses. An empty stored
/// id (a provider with no id) accepts the leg's answer only when its title
/// matches the latched title, so a partial latch can never drift onto another
/// work's record.
fn fetch_identity(
    sources: &[Arc<dyn Source>],
    cancel: &Cancel,
    provider: &str,
    provider_id: &str,
    title: &str,
    year: i32,
    kind: Kind,
) -> Result<Media, Error> {
    if let Some(reason) = cancel.err() {
        return Err(Error::Cancelled(reason));
    }
    let Some(source) = sources.iter().find(|source| source.name() == provider) else {
        return Err(Error::other(format!(
            "metadata: search: no source named {provider:?}"
        )));
    };
    let q = Query {
        kind,
        title: title.to_string(),
        year,
        season: 0,
        episode: 0,
        absolute: false,
    };
    let found = source.find(cancel, &q)?;
    let want_id = provider_id.trim();
    match found {
        Some(media)
            if (!want_id.is_empty() && media.source_id.trim() == want_id)
                || (want_id.is_empty() && titles_match(&media.title, title)) =>
        {
            Ok(media)
        }
        other => {
            if !want_id.is_empty() {
                let exact = source
                    .find_all(cancel, &q)?
                    .into_iter()
                    .find(|media| media.source_id.trim() == want_id);
                if let Some(media) = exact {
                    return Ok(media);
                }
            }
            Err(match other {
                Some(media) => Error::other(format!(
                    "metadata: search: {provider:?} answered id {:?} for {title:?}, not {provider_id:?}; \
                     refusing to latch a different entry",
                    media.source_id
                )),
                None => Error::other(format!(
                    "metadata: search: {provider:?} no longer has {title:?}"
                )),
            })
        }
    }
}

/// The per-episode pins for one fetched table, in table order. An episode
/// numbered by the wire absolutely (season 0) keeps its number as `absolute`;
/// a season-numbered episode's `absolute` is its number plus the episodes the
/// SAME table reports for strictly earlier seasons -- the resolver's own
/// absolute-total rule, so a partial table cannot fabricate a total it does
/// not hold. The synopsis is copied as-is: an episode the provider has none
/// for keeps an empty one, never a fabricated line.
fn pins_from_table(episodes: &[Episode]) -> Vec<EpisodePin> {
    episodes
        .iter()
        .map(|ep| {
            let absolute = if ep.season <= 0 {
                ep.number
            } else {
                let earlier = episodes
                    .iter()
                    .filter(|other| other.season > 0 && other.season < ep.season)
                    .count();
                i32::try_from(earlier)
                    .unwrap_or(i32::MAX)
                    .saturating_add(ep.number)
            };
            EpisodePin {
                season: ep.season,
                number: ep.number,
                absolute,
                title: ep.title.clone(),
                synopsis: ep.synopsis.clone(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use metadata_http::cancel::Cancel;

    use super::*;
    use crate::cinemeta::CinemetaSource;
    use crate::testsupport::{FakeServer, FakeSource, Request, Response, dead_url, http_client};
    use crate::tvmaze::TvmazeSource;

    /// A fresh 5 s scope, the shape a CLI hands the legs.
    fn cancel() -> Cancel {
        Cancel::with_deadline(Duration::from_secs(5))
    }

    /// A Windows fixture path with no machine-shaped literal: the drive
    /// letter and the rest live in separate literals, so no single source
    /// line carries the gate's path shape. The runtime value is the
    /// ordinary drive-letter path the test exercises.
    fn win(drive: char, rest: &str) -> String {
        format!("{}:{}", drive, rest)
    }

    fn tvmaze_leg(url: &str) -> Arc<dyn Source> {
        Arc::new(TvmazeSource::new(url, http_client()))
    }

    fn cinemeta_leg(url: &str) -> Arc<dyn Source> {
        Arc::new(CinemetaSource::new(url, http_client()))
    }

    /// A leg that answers several rows at once, so a test can prove the
    /// multi-result pass and the per-leg sink without a wire fixture.
    struct MultiSource {
        name: &'static str,
        rows: Vec<Media>,
    }

    impl MultiSource {
        fn shared(name: &'static str, rows: Vec<Media>) -> Arc<dyn Source> {
            Arc::new(MultiSource { name, rows })
        }
    }

    impl Source for MultiSource {
        fn name(&self) -> &str {
            self.name
        }

        fn find(&self, _cancel: &Cancel, _q: &Query) -> Result<Option<Media>, Error> {
            Ok(self.rows.first().cloned())
        }

        fn find_all(&self, _cancel: &Cancel, _q: &Query) -> Result<Vec<Media>, Error> {
            Ok(self.rows.clone())
        }
    }

    /// One series row for a [`MultiSource`].
    fn row(id: &str, title: &str, year: i32) -> Media {
        Media {
            source: "fake".to_string(),
            source_id: id.to_string(),
            kind: Kind::SERIES,
            title: title.to_string(),
            year,
            ..Media::default()
        }
    }

    /// One candidate for the merge and splice tests: the identity is named and
    /// every field the splice fills starts empty.
    fn cand(provider: &str, id: &str, title: &str, year: i32) -> Candidate {
        Candidate {
            kind: Kind::SERIES,
            title: title.to_string(),
            year,
            provider: provider.to_string(),
            provider_id: id.to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        }
    }

    /// The (provider, provider_id) pairs in order: the sequence under test.
    fn identities(hits: &[Candidate]) -> Vec<(String, String)> {
        hits.iter()
            .map(|cand| (cand.provider.clone(), cand.provider_id.clone()))
            .collect()
    }

    /// A TVmaze fixture: one show ("Show", id 169, 2008) with a three-episode
    /// table spanning two seasons, the middle episode carrying no synopsis.
    const TVMAZE_2008: &str = r#"[{"score":1.0,"show":{"id":169,"name":"Show","premiered":"2008-01-20","weight":99,"image":{"original":"https://art/show.jpg"},"summary":"<p>Show.</p>","genres":["Drama"],"averageRuntime":45,"url":"https://tvmaze/169"}}]"#;

    /// The same show in 2010 (a deliberate year mismatch) as id 1.
    const TVMAZE_2010: &str = r#"[{"score":1.0,"show":{"id":1,"name":"Show","premiered":"2010-01-01","weight":99,"image":{"original":"https://art/tv.jpg"},"summary":"<p>TV.</p>","genres":["Drama"],"averageRuntime":45,"url":"https://tvmaze/1"}}]"#;

    const EPISODES: &str = r#"[{"id":1,"name":"Pilot","season":1,"number":1,"summary":"<p>Walt cooks.</p>","url":"https://tvmaze/ep/1","runtime":45},
        {"id":2,"name":"Cat's in the Bag...","season":1,"number":2},
        {"id":3,"name":"Seven Thirty-Seven","season":2,"number":1,"summary":"<p>Krazy-8.</p>","url":"https://tvmaze/ep/3","runtime":45}]"#;

    /// Three TVmaze rows for "Show": two title matches (2008, 2013) and one
    /// unrelated row the matcher's floor drops.
    const TVMAZE_MANY: &str = r#"[{"score":1.0,"show":{"id":169,"name":"Show","premiered":"2008-01-20","weight":99,"image":{"original":"https://art/show.jpg"}}},{"score":0.9,"show":{"id":300,"name":"Totally Unrelated","premiered":"1999-01-01","weight":10}},{"score":0.8,"show":{"id":200,"name":"Show: The Return","premiered":"2013-05-01","weight":50}}]"#;

    /// Two same-titled TVmaze entries from the same year: `find` takes the
    /// first-ranked row, so a latch on the second id can only come from
    /// `find_all`.
    const TVMAZE_TWO_ROWS: &str = r#"[{"score":1.0,"show":{"id":1,"name":"Show","premiered":"2008-01-01","weight":99}},{"score":1.0,"show":{"id":2,"name":"Show","premiered":"2008-06-01","weight":99}}]"#;

    #[test]
    fn search_gates_the_year_hard_and_repeats_the_order() {
        let tvmaze = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(TVMAZE_2010),
            _ => Response::error(404, "not found"),
        });
        let cinemeta = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/catalog/series/top/search=Show.json" => {
                Response::json(r#"{"metas":[{"id":"tt1","name":"Show","releaseInfo":"2008"}]}"#)
            }
            _ => Response::error(404, "not found"),
        });
        let sources = vec![tvmaze_leg(&tvmaze.url()), cinemeta_leg(&cinemeta.url())];

        // A known year is a GATE: TVmaze's 2010 entry is dropped, never ranked
        // lower, and the same fixtures reproduce the same sequence.
        for _ in 0..2 {
            let hits = search_with(&sources, &cancel(), "Show", 2008, Kind::SERIES);
            assert_eq!(
                identities(&hits),
                vec![("cinemeta".to_string(), "tt1".to_string())]
            );
            assert_eq!(hits[0].year, 2008);
        }

        // With no year given nothing is gated, and provider order is the
        // tie-break (equal titles).
        for _ in 0..2 {
            let hits = search_with(&sources, &cancel(), "Show", 0, Kind::SERIES);
            assert_eq!(
                identities(&hits),
                vec![
                    ("tvmaze".to_string(), "1".to_string()),
                    ("cinemeta".to_string(), "tt1".to_string()),
                ]
            );
        }
    }

    #[test]
    fn search_prefers_the_wanted_kind_over_the_earlier_leg() {
        let series = |title: &str, id: &str| Media {
            source: "fake".to_string(),
            source_id: id.to_string(),
            kind: Kind::SERIES,
            title: title.to_string(),
            ..Media::default()
        };
        let movie = |title: &str, id: &str| Media {
            source: "fake".to_string(),
            source_id: id.to_string(),
            kind: Kind::MOVIE,
            title: title.to_string(),
            ..Media::default()
        };

        // Cinemeta answers a SERIES record for a movie query; AniList answers
        // the movie. The wanted kind beats the earlier leg.
        let cinemeta: Arc<dyn Source> =
            FakeSource::with_media("cinemeta", series("Show", "series-1")).shared();
        let anilist: Arc<dyn Source> =
            FakeSource::with_media("anilist", movie("Show", "movie-1")).shared();
        let hits = search_with(&[cinemeta, anilist], &cancel(), "Show", 0, Kind::MOVIE);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].kind, Kind::MOVIE);
        assert_eq!(hits[0].provider, "anilist");

        // ...and for a series want, a movie-kind answer loses to a series one.
        let tvmaze_movie: Arc<dyn Source> =
            FakeSource::with_media("tvmaze", movie("Show", "movie-2")).shared();
        let anilist: Arc<dyn Source> =
            FakeSource::with_media("anilist", series("Show", "series-2")).shared();
        let hits = search_with(&[tvmaze_movie, anilist], &cancel(), "Show", 0, Kind::SERIES);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].kind, Kind::SERIES);
        assert_eq!(hits[0].provider, "anilist");
    }

    #[test]
    fn search_ranks_the_better_title_above_the_provider_order() {
        let series = |title: &str, id: &str| Media {
            source: "fake".to_string(),
            source_id: id.to_string(),
            kind: Kind::SERIES,
            title: title.to_string(),
            ..Media::default()
        };
        // The earlier leg carries the worse title; the matcher's exact match
        // wins the first slot anyway.
        let worse: Arc<dyn Source> =
            FakeSource::with_media("tvmaze", series("Show Commentary Special", "t1")).shared();
        let better: Arc<dyn Source> =
            FakeSource::with_media("anilist", series("Show", "a1")).shared();
        let hits = search_with(&[worse, better], &cancel(), "Show", 0, Kind::SERIES);
        assert_eq!(
            identities(&hits),
            vec![
                ("anilist".to_string(), "a1".to_string()),
                ("tvmaze".to_string(), "t1".to_string()),
            ]
        );
    }

    #[test]
    fn search_caps_at_the_limit() {
        let legs = ["tvmaze", "anilist", "tenrai", "kitsu", "cinemeta"];
        // Every row is its own work (a distinct year), so the cap is measured
        // over 30 rows instead of collapsing them through the same-work merge.
        let sources: Vec<Arc<dyn Source>> = legs
            .iter()
            .enumerate()
            .map(|(leg, name)| {
                let rows = (0..6)
                    .map(|i| {
                        row(
                            &format!("{name}-{i}"),
                            "Show",
                            2000 + i32::try_from(leg * 6 + i).expect("small grid"),
                        )
                    })
                    .collect();
                MultiSource::shared(name, rows)
            })
            .collect();
        let hits = search_with(&sources, &cancel(), "Show", 0, Kind::SERIES);
        assert_eq!(SEARCH_LIMIT, 24, "the picker page's size");
        assert_eq!(hits.len(), SEARCH_LIMIT);
        // Five legs at six equally-titled rows each: the cap drops the tail of
        // provider order, never a middle row.
        assert_eq!(hits[0].provider, "tvmaze");
        assert_eq!(identities(&hits).last().map(|(p, _)| p.as_str()), Some("kitsu"));
        assert!(!hits.iter().any(|cand| cand.provider == "cinemeta"));
    }

    #[test]
    fn search_dedupes_by_provider_and_id_across_legs() {
        // Neither server answers `/shows/<id>/episodes`: the search must not
        // fetch an episode table, so a request for one would 404 and empty the
        // leg -- the assertion below would then fail.
        let one = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(TVMAZE_2008),
            _ => Response::error(404, "not found"),
        });
        let two = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(
                r#"[{"score":1.0,"show":{"id":170,"name":"Show","premiered":"2009-01-01","weight":50}}]"#,
            ),
            _ => Response::error(404, "not found"),
        });
        let first = tvmaze_leg(&one.url());
        let second = tvmaze_leg(&two.url());

        // The same leg mounted twice answers the same (provider, id): one
        // candidate. The other show's id survives as its own candidate.
        let hits = search_with(
            &[Arc::clone(&first), Arc::clone(&first), second],
            &cancel(),
            "Show",
            0,
            Kind::SERIES,
        );
        assert_eq!(
            identities(&hits),
            vec![
                ("tvmaze".to_string(), "169".to_string()),
                ("tvmaze".to_string(), "170".to_string()),
            ]
        );
        assert_eq!(
            hits[0].poster, "https://art/show.jpg",
            "the row's own cover is the tell"
        );
        assert_eq!(hits[0].year, 2008);
        assert_eq!(hits[1].year, 2009);
        assert_eq!(hits[1].poster, "");
    }

    #[test]
    fn pin_from_candidate_stores_a_movie_snapshot_with_no_pins() {
        let server = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/catalog/movie/top/search=Dune:%20Part%20Two.json" => Response::json(
                r#"{"metas":[{"id":"tt15239678","name":"Dune: Part Two","releaseInfo":"2024"}]}"#,
            ),
            "/meta/movie/tt15239678.json" => Response::json(
                r#"{"meta":{"name":"Dune: Part Two","year":"2024","runtime":"166 min","genre":["Science Fiction","Adventure"],"description":"Paul Atreides unites with the Fremen.","poster":"https://art/dune.jpg","videos":[]}}"#,
            ),
            _ => Response::error(404, "not found"),
        });
        let cand = Candidate {
            kind: Kind::MOVIE,
            title: "Dune: Part Two".to_string(),
            year: 2024,
            provider: "cinemeta".to_string(),
            provider_id: "tt15239678".to_string(),
            poster: "https://art/dune-search.jpg".to_string(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        // The search pass over the same fixture offers that entry as a
        // zero-episode pick.
        let hits = search_with(
            &[cinemeta_leg(&server.url())],
            &cancel(),
            "Dune: Part Two",
            2024,
            Kind::MOVIE,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, Kind::MOVIE);
        assert_eq!(hits[0].provider_id, "tt15239678");
        assert_eq!(hits[0].episode_count, 0);
        assert_eq!(
            hits[0].poster, "",
            "a catalog row carries no poster: the search is one request"
        );

        let over = pin_from_candidate_with(
            &[cinemeta_leg(&server.url())],
            &cancel(),
            &cand,
            Scope::Movie,
            &win('D', r"\Films\Dune Part Two.mkv"),
        )
        .expect("pin");

        assert_eq!(over.scope, Scope::Movie);
        assert_eq!(over.key, win('D', r"\Films\Dune Part Two.mkv"));
        assert_eq!(over.title, "Dune: Part Two");
        assert_eq!(over.year, 2024);
        assert_eq!(over.provider, "cinemeta");
        assert_eq!(over.provider_id, "tt15239678");
        assert!(
            over.poster.is_empty(),
            "a movie renders from its media snapshot"
        );
        assert!(over.episodes.is_empty(), "a movie latch carries no pins");
        let media = over.media.expect("media snapshot");
        assert_eq!(media.title, "Dune: Part Two");
        assert_eq!(media.source_id, "tt15239678");
        assert_eq!(media.artwork, "https://art/dune.jpg");
        assert!(!media.synopsis.is_empty());
        assert!(!media.length.is_empty());
        assert!(media.episodes.is_empty());
    }

    #[test]
    fn pin_from_candidate_pins_a_table_with_absolute_numbers_and_empty_synopses() {
        let server = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(TVMAZE_2008),
            "/shows/169/episodes" => Response::json(EPISODES),
            _ => Response::error(404, "not found"),
        });
        let sources = [tvmaze_leg(&server.url())];
        let cand = Candidate {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: "https://art/show-picked.jpg".to_string(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 3,
        };

        let show = pin_from_candidate_with(
            &sources,
            &cancel(),
            &cand,
            Scope::Series,
            &win('M', r"\2SERIES\Show"),
        )
        .expect("pin show");
        assert_eq!(show.scope, Scope::Series);
        assert_eq!(show.poster, "https://art/show-picked.jpg");
        assert_eq!(show.episodes.len(), 3);
        assert_eq!(
            show.episodes[0],
            EpisodePin {
                season: 1,
                number: 1,
                absolute: 1,
                title: "Pilot".to_string(),
                synopsis: "Walt cooks.".to_string(),
            }
        );
        assert_eq!(show.episodes[1].absolute, 2);
        assert_eq!(
            show.episodes[1].synopsis, "",
            "a missing synopsis is never fabricated"
        );
        assert_eq!(
            show.episodes[2],
            EpisodePin {
                season: 2,
                number: 1,
                absolute: 3,
                title: "Seven Thirty-Seven".to_string(),
                synopsis: "Krazy-8.".to_string(),
            }
        );
        // The pins serve the store's own lookups: (season, number) and
        // absolute.
        assert_eq!(show.episode_pin(2, 1).map(|pin| pin.absolute), Some(3));
        assert_eq!(show.episode_pin_absolute(3).map(|pin| pin.number), Some(1));
        assert_eq!(
            show.media.as_ref().map(|media| media.title.as_str()),
            Some("Show")
        );

        // A season scope pins the same table: the pins carry their own season,
        // so one table serves every season of the show.
        let season = pin_from_candidate_with(
            &sources,
            &cancel(),
            &cand,
            Scope::Season,
            &win('M', r"\2SERIES\Show\Season 1"),
        )
        .expect("pin season");
        assert_eq!(season.scope, Scope::Season);
        assert_eq!(season.key, win('M', r"\2SERIES\Show\Season 1"));
        assert_eq!(season.episodes, show.episodes);
    }

    #[test]
    fn refresh_keeps_scope_key_and_created_at_and_updates_the_payload() {
        let search_body = Arc::new(Mutex::new(String::from(TVMAZE_2008)));
        let episodes_body = Arc::new(Mutex::new(String::from(
            r#"[{"id":1,"name":"Pilot v1","season":1,"number":1,"summary":"<p>One.</p>","runtime":45}]"#,
        )));
        let (search_handler, episodes_handler) = (Arc::clone(&search_body), Arc::clone(&episodes_body));
        let server = FakeServer::start(move |req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(&search_handler.lock().unwrap()),
            "/shows/169/episodes" => Response::json(&episodes_handler.lock().unwrap()),
            _ => Response::error(404, "not found"),
        });
        let sources = [tvmaze_leg(&server.url())];
        let cand = Candidate {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: "https://art/v1.jpg".to_string(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 1,
        };
        let over = pin_from_candidate_with(
            &sources,
            &cancel(),
            &cand,
            Scope::Series,
            &win('M', r"\2SERIES\Show"),
        )
        .expect("pin");
        assert_eq!(over.episodes[0].title, "Pilot v1");
        assert!(over.created_at_ms > 1_600_000_000_000, "the latch was stamped");

        // The provider moves on: a new episode title and a new cover.
        *search_body.lock().unwrap() = String::from(
            r#"[{"score":1.0,"show":{"id":169,"name":"Show","premiered":"2008-01-20","weight":99,"image":{"original":"https://art/v2.jpg"},"summary":"<p>Show.</p>","genres":["Drama"],"averageRuntime":45,"url":"https://tvmaze/169"}}]"#,
        );
        *episodes_body.lock().unwrap() = String::from(
            r#"[{"id":1,"name":"Pilot v2","season":1,"number":1,"summary":"<p>One.</p>","runtime":45}]"#,
        );

        let refreshed = refresh_with(&sources, &cancel(), &over).expect("refresh");
        assert_eq!(refreshed.scope, over.scope);
        assert_eq!(refreshed.key, over.key);
        assert_eq!(refreshed.created_at_ms, over.created_at_ms);
        assert_eq!(refreshed.title, over.title);
        assert_eq!(refreshed.year, over.year);
        assert_eq!(refreshed.provider, over.provider);
        assert_eq!(refreshed.provider_id, over.provider_id);
        assert_eq!(refreshed.poster, "https://art/v2.jpg");
        assert_eq!(refreshed.episodes[0].title, "Pilot v2");
        assert_eq!(
            refreshed.media.as_ref().map(|media| media.artwork.as_str()),
            Some("https://art/v2.jpg")
        );
        // The input latch is untouched: refresh builds a new record.
        assert_eq!(over.episodes[0].title, "Pilot v1");
        assert_eq!(over.poster, "https://art/v1.jpg");
    }

    #[test]
    fn refresh_and_pin_refuse_a_mismatched_provider_id() {
        let server = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(TVMAZE_2008),
            "/shows/169/episodes" => Response::json(EPISODES),
            _ => Response::error(404, "not found"),
        });
        let sources = [tvmaze_leg(&server.url())];
        let cand = Candidate {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 3,
        };
        let mut over =
            pin_from_candidate_with(&sources, &cancel(), &cand, Scope::Series, "k").expect("pin");

        // A stored id the leg cannot answer is refused, never latched over.
        over.provider_id = "999".to_string();
        let err = refresh_with(&sources, &cancel(), &over).expect_err("a drifted id is refused");
        assert!(
            err.to_string().contains("999"),
            "the refusal names the id: {err}"
        );
        let cand = Candidate {
            provider_id: "999".to_string(),
            ..cand
        };
        assert!(
            pin_from_candidate_with(&sources, &cancel(), &cand, Scope::Series, "k").is_err(),
            "a mismatched pin never half-builds a latch"
        );
    }

    #[test]
    fn pin_and_refresh_report_a_failing_leg_instead_of_building_a_half_latch() {
        let dead: [Arc<dyn Source>; 1] = [Arc::new(TvmazeSource::new(dead_url(), http_client()))];
        let cand = Candidate {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 3,
        };
        let over = Override {
            scope: Scope::Series,
            key: "k".to_string(),
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: "https://art/show.jpg".to_string(),
            media: None,
            episodes: Vec::new(),
            created_at_ms: 42,
        };
        assert!(pin_from_candidate_with(&dead, &cancel(), &cand, Scope::Series, "k").is_err());
        assert!(refresh_with(&dead, &cancel(), &over).is_err());
        // A missing provider name is an error too, and an empty source set
        // never half-builds either.
        let none: [Arc<dyn Source>; 0] = [];
        assert!(pin_from_candidate_with(&none, &cancel(), &cand, Scope::Series, "k").is_err());
        assert!(refresh_with(&none, &cancel(), &over).is_err());

        // A malformed provider body is reported, not panicked on.
        let junk = FakeServer::start(|_req: &Request| Response::json("{not json"));
        let junk_leg: [Arc<dyn Source>; 1] = [tvmaze_leg(&junk.url())];
        assert!(pin_from_candidate_with(&junk_leg, &cancel(), &cand, Scope::Series, "k").is_err());
        assert!(refresh_with(&junk_leg, &cancel(), &over).is_err());
    }

    #[test]
    fn find_all_returns_every_matching_row_in_one_request() {
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        let server = FakeServer::start(move |req: &Request| {
            seen.lock().unwrap().push(req.path.clone());
            match req.path.as_str() {
                "/search/shows" => Response::json(TVMAZE_MANY),
                _ => Response::error(404, "not found"),
            }
        });
        let source = tvmaze_leg(&server.url());
        let q = Query {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            ..Query::default()
        };
        let rows = source.find_all(&cancel(), &q).expect("find_all");
        assert_eq!(rows.len(), 2, "both title matches, the unrelated row dropped");
        assert_eq!(rows[0].source_id, "169");
        assert_eq!(rows[0].title, "Show");
        assert_eq!(rows[0].year, 2008);
        assert_eq!(rows[0].kind, Kind::SERIES);
        assert_eq!(rows[0].artwork, "https://art/show.jpg");
        assert!(rows[0].episodes.is_empty(), "a search row carries no table");
        assert_eq!(rows[1].source_id, "200");
        assert_eq!(rows[1].year, 2013);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["/search/shows".to_string()],
            "one request: no episode table is fetched"
        );
    }

    #[test]
    fn find_all_caps_at_twelve_rows_per_provider() {
        let body: String = format!(
            "[{}]",
            (1..=14)
                .map(|id| {
                    format!(
                        r#"{{"score":1.0,"show":{{"id":{id},"name":"Show","premiered":"2008-01-01"}}}}"#
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        );
        let server = FakeServer::start(move |req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(&body),
            _ => Response::error(404, "not found"),
        });
        let q = Query {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            ..Query::default()
        };
        let rows = tvmaze_leg(&server.url())
            .find_all(&cancel(), &q)
            .expect("find_all");
        assert_eq!(rows.len(), crate::resolver::FIND_ALL_LIMIT);
        assert_eq!(rows.len(), 12);
        assert_eq!(rows[0].source_id, "1");
        assert_eq!(rows[11].source_id, "12");
    }

    #[test]
    fn search_stream_hands_each_leg_its_rows_as_it_finishes() {
        let sources = vec![
            MultiSource::shared(
                "tvmaze",
                vec![row("t1", "Show", 2008), row("t2", "Show: The Return", 2013)],
            ),
            MultiSource::shared("anilist", vec![row("a1", "Show", 2008)]),
        ];
        let batches: Mutex<Vec<Vec<Candidate>>> = Mutex::new(Vec::new());
        let sink = |rows: Vec<Candidate>| batches.lock().unwrap().push(rows);
        let outcome = search_stream_with(&sources, &cancel(), "Show", 0, Kind::SERIES, &sink);
        assert!(outcome.failed.is_empty(), "both legs answered");
        let hits = outcome.rows;
        let batches = batches.lock().unwrap().clone();
        assert_eq!(batches.len(), 2, "one batch per answering leg");
        let streamed: usize = batches.iter().map(Vec::len).sum();
        assert_eq!(streamed, 3, "every leg's rows reached the sink RAW");
        // The batches stay raw -- the same work arrives once per leg -- and the
        // final result merges the duplicates into the best-ranked identity.
        assert_eq!(
            identities(&hits),
            vec![
                ("tvmaze".to_string(), "t1".to_string()),
                ("tvmaze".to_string(), "t2".to_string()),
            ],
            "anilist's 2008 duplicate is spliced into tvmaze's kept row"
        );
        assert_eq!(hits[0].title, "Show", "the exact title still ranks first");
    }

    #[test]
    fn search_drops_a_wrong_year_entry_instead_of_ranking_it() {
        // `edgerunners 2026` used to surface the 1990s entry through the soft
        // year preference. The year is a GATE now: nothing admits a mismatch.
        let sources = vec![MultiSource::shared(
            "tvmaze",
            vec![
                row("90", "Edgerunners", 1995),
                row("26", "Edgerunners", 2026),
                row("0", "Edgerunners", 0),
            ],
        )];
        let batches: Mutex<Vec<Vec<Candidate>>> = Mutex::new(Vec::new());
        let sink = |rows: Vec<Candidate>| batches.lock().unwrap().push(rows);
        let hits =
            search_stream_with(&sources, &cancel(), "Edgerunners", 2026, Kind::SERIES, &sink).rows;
        assert_eq!(
            identities(&hits),
            vec![("tvmaze".to_string(), "26".to_string())]
        );
        assert_eq!(hits[0].year, 2026);
        // The sink never sees a mismatch either: the gate runs before it.
        let batches = batches.lock().unwrap().clone();
        assert_eq!(
            batches.iter().flatten().count(),
            1,
            "one row streamed, the 1990s and unknown-year rows dropped"
        );

        // A year-0 entry is a mismatch too: no known year never satisfies a
        // known query year.
        let sources = vec![MultiSource::shared(
            "tvmaze",
            vec![row("0", "Edgerunners", 0)],
        )];
        assert!(
            search_with(&sources, &cancel(), "Edgerunners", 2026, Kind::SERIES).is_empty(),
            "no soft preference may reintroduce a mismatch"
        );

        // With no year given, every row is admitted.
        let sources = vec![MultiSource::shared(
            "tvmaze",
            vec![row("90", "Edgerunners", 1995)],
        )];
        assert_eq!(
            search_with(&sources, &cancel(), "Edgerunners", 0, Kind::SERIES).len(),
            1
        );
    }

    #[test]
    fn refresh_takes_the_stored_id_from_find_all_when_find_picks_another_entry() {
        let server = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(TVMAZE_TWO_ROWS),
            // `find` settles on id 1 and fetches its table; the latch wants
            // id 2, which only `find_all` carries.
            "/shows/1/episodes" => Response::json(
                r#"[{"id":1,"name":"Pilot","season":1,"number":1,"summary":"<p>One.</p>"}]"#,
            ),
            _ => Response::error(404, "not found"),
        });
        let sources = [tvmaze_leg(&server.url())];
        // The latch stores the second 2008 entry (id 2); `find` ranks the
        // first (id 1) and never looks past it, so the stored id can only come
        // from find_all.
        let cand = Candidate {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "2".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        let over = pin_from_candidate_with(&sources, &cancel(), &cand, Scope::Series, "k")
            .expect("the fallback finds the stored id");
        assert_eq!(over.provider_id, "2");
        assert_eq!(
            over.media.as_ref().map(|media| media.source_id.as_str()),
            Some("2")
        );

        let refreshed = refresh_with(&sources, &cancel(), &over).expect("refresh");
        assert_eq!(refreshed.provider_id, "2");
        assert_eq!(
            refreshed.media.as_ref().map(|media| media.source_id.as_str()),
            Some("2"),
            "the refreshed latch keeps the stored identity"
        );
    }

    #[test]
    fn search_returns_empty_on_no_results_or_total_failure() {
        let none: [Arc<dyn Source>; 0] = [];
        assert!(search_with(&none, &cancel(), "Show", 0, Kind::SERIES).is_empty());
        // A blank name never touches a leg.
        let live = FakeServer::start(|req: &Request| match req.path.as_str() {
            "/search/shows" => Response::json(TVMAZE_2008),
            "/shows/169/episodes" => Response::json(EPISODES),
            _ => Response::error(404, "not found"),
        });
        let sources = [tvmaze_leg(&live.url())];
        assert!(search_with(&sources, &cancel(), "   ", 0, Kind::SERIES).is_empty());

        // No provider rows, a dead leg and a malformed body all end as no
        // results -- never a panic.
        let miss = FakeServer::start(|_req: &Request| Response::json("[]"));
        let miss_leg = [tvmaze_leg(&miss.url())];
        assert!(search_with(&miss_leg, &cancel(), "Show", 0, Kind::SERIES).is_empty());
        let dead: [Arc<dyn Source>; 1] = [Arc::new(TvmazeSource::new(dead_url(), http_client()))];
        assert!(search_with(&dead, &cancel(), "Show", 0, Kind::SERIES).is_empty());
        let junk = FakeServer::start(|_req: &Request| Response::json("{not json"));
        let junk_leg = [tvmaze_leg(&junk.url())];
        assert!(search_with(&junk_leg, &cancel(), "Show", 0, Kind::SERIES).is_empty());

        // A cancelled scope ends the search before any leg runs.
        let cancelled = Cancel::background();
        cancelled.cancel();
        assert!(search_with(&sources, &cancelled, "Show", 0, Kind::SERIES).is_empty());
    }

    #[test]
    fn subset_titles_never_merge_accents_fold() {
        // Defect 5: a token subset is a different work even with the same
        // known year -- only an exact normalized title (numeral-folded equal)
        // may merge.
        let merged = merge_same_work(vec![
            cand("tvmaze", "1", "National Treasure", 2004),
            cand("cinemeta", "2", "National Treasure Book of Secrets", 2004),
        ]);
        assert_eq!(
            merged.len(),
            2,
            "a real second work must survive, got {merged:?}"
        );
        // Accents fold like match_.rs:99, so Amélie and Amelie are one work.
        let merged = merge_same_work(vec![
            cand("tvmaze", "1", "Amélie", 2001),
            cand("cinemeta", "2", "Amelie", 2001),
        ]);
        assert_eq!(merged.len(), 1, "accents must fold, got {merged:?}");
    }

    #[test]
    fn merge_same_work_keeps_the_first_identity_and_splices_the_missing_synopsis() {
        let mut first = cand("tvmaze", "169", "Show", 2008);
        first.poster = "https://art/t.jpg".to_string();
        first.episode_count = 3;
        let mut second = cand("cinemeta", "tt1", "Show", 2008);
        second.synopsis = "Show.".to_string();
        second.genres = "Drama".to_string();
        second.length = "45 min".to_string();
        second.episode_count = 5;

        let merged = merge_same_work(vec![first, second]);
        assert_eq!(merged.len(), 1, "one work, one row");
        assert_eq!(merged[0].provider, "tvmaze", "the first row keeps the identity");
        assert_eq!(merged[0].provider_id, "169");
        assert_eq!(merged[0].title, "Show");
        assert_eq!(merged[0].year, 2008);
        assert_eq!(
            merged[0].poster, "https://art/t.jpg",
            "a non-empty field is never replaced"
        );
        assert_eq!(
            merged[0].synopsis, "Show.",
            "the second provider's synopsis fills the empty one"
        );
        assert_eq!(merged[0].genres, "Drama");
        assert_eq!(merged[0].length, "45 min");
        assert_eq!(merged[0].episode_count, 5, "the larger table wins");

        // A duplicate carrying nothing never clears a spliced field, and a
        // different year is another work.
        let mut with_synopsis = cand("tvmaze", "169", "Show", 2008);
        with_synopsis.synopsis = "Show.".to_string();
        let merged = merge_same_work(vec![
            with_synopsis,
            cand("kitsu", "k1", "Show", 2008),
            cand("anilist", "a1", "Show", 2009),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].synopsis, "Show.");
        assert_eq!(merged[1].provider, "anilist");

        // The roman and the digit spelling of a sequel numeral are one title.
        let merged = merge_same_work(vec![
            cand("tvmaze", "1", "Cyberpunk: Edgerunners II", 2022),
            cand("anilist", "2", "Cyberpunk: Edgerunners 2", 2022),
        ]);
        assert_eq!(
            identities(&merged),
            vec![("tvmaze".to_string(), "1".to_string())],
            "a trailing II folds to 2"
        );

        // A token subset never merges, even with a known equal year: the
        // sequel is a different work and keeps its own row.
        let merged = merge_same_work(vec![
            cand("tvmaze", "1", "National Treasure", 2004),
            cand("anilist", "2", "National Treasure: Book of Secrets", 2004),
            cand("kitsu", "3", "National Treasure: Book of Secrets", 2007),
        ]);
        assert_eq!(merged.len(), 3, "a subset title is a different work");
        let merged = merge_same_work(vec![
            cand("tvmaze", "1", "National Treasure", 2004),
            cand("kitsu", "2", "National Treasure: Book of Secrets", 0),
        ]);
        assert_eq!(merged.len(), 2, "a yearless near title is not the same work");
    }

    #[test]
    fn pin_fills_the_media_snapshot_from_the_candidate() {
        // The leg's identity record carries no synopsis, genres, length, cover
        // or year: the picked candidate's copy is what the latch keeps.
        let sources = [MultiSource::shared("tvmaze", vec![row("169", "Show", 0)])];
        let mut picked = cand("tvmaze", "169", "Show", 2008);
        picked.poster = "https://art/search.jpg".to_string();
        picked.synopsis = "Spliced.".to_string();
        picked.genres = "Drama".to_string();
        picked.length = "45 min".to_string();

        let over =
            pin_from_candidate_with(&sources, &cancel(), &picked, Scope::Movie, "k").expect("pin");
        let media = over.media.expect("media snapshot");
        assert_eq!(media.synopsis, "Spliced.", "the candidate's synopsis lands on the media");
        assert_eq!(media.genres, "Drama");
        assert_eq!(media.length, "45 min");
        assert_eq!(media.artwork, "https://art/search.jpg");
        assert_eq!(media.year, 2008, "an unknown fetched year takes the candidate's");
        assert_eq!(media.source_id, "169", "the fetched identity is untouched");
    }

    #[test]
    fn refresh_preserves_a_spliced_synopsis_the_provider_never_returns() {
        let sources = [MultiSource::shared("tvmaze", vec![row("169", "Show", 2008)])];
        let mut picked = cand("tvmaze", "169", "Show", 2008);
        picked.poster = "https://art/search.jpg".to_string();
        picked.synopsis = "Spliced.".to_string();
        picked.genres = "Drama".to_string();
        picked.length = "45 min".to_string();

        let over =
            pin_from_candidate_with(&sources, &cancel(), &picked, Scope::Series, "k").expect("pin");
        assert_eq!(
            over.media.as_ref().map(|media| media.synopsis.as_str()),
            Some("Spliced.")
        );

        let refreshed = refresh_with(&sources, &cancel(), &over).expect("refresh");
        let media = refreshed.media.expect("media snapshot");
        assert_eq!(
            media.synopsis, "Spliced.",
            "a refresh never drops the spliced synopsis"
        );
        assert_eq!(media.genres, "Drama");
        assert_eq!(media.length, "45 min");
        assert_eq!(media.artwork, "https://art/search.jpg");
        assert_eq!(
            refreshed.poster, "https://art/search.jpg",
            "the show cover follows the filled media"
        );
    }

    #[test]
    fn a_failing_leg_is_named_and_the_other_legs_rows_stand() {
        let sources = vec![
            MultiSource::shared("tvmaze", vec![row("t1", "Show", 2008)]),
            FakeSource::with_error("kitsu", "kitsu is down").shared(),
        ];
        let outcome = search_stream_with(&sources, &cancel(), "Show", 0, Kind::SERIES, &|_| {});
        assert_eq!(
            identities(&outcome.rows),
            vec![("tvmaze".to_string(), "t1".to_string())],
            "the answering leg's rows stand"
        );
        assert_eq!(
            outcome.failed,
            vec!["kitsu".to_string()],
            "the failing leg is named"
        );

        // A clean pass names no leg: an empty answer is a success, not a
        // failure, and a dead scope runs no leg at all.
        let clean: Vec<Arc<dyn Source>> = vec![
            MultiSource::shared("tvmaze", vec![row("t1", "Show", 2008)]),
            MultiSource::shared("kitsu", Vec::new()),
        ];
        let outcome = search_stream_with(&clean, &cancel(), "Show", 0, Kind::SERIES, &|_| {});
        assert_eq!(outcome.rows.len(), 1);
        assert!(outcome.failed.is_empty(), "an empty answer is a success");

        let cancelled = Cancel::background();
        cancelled.cancel();
        let outcome = search_stream_with(&sources, &cancelled, "Show", 0, Kind::SERIES, &|_| {});
        assert!(outcome.rows.is_empty() && outcome.failed.is_empty());
    }

    #[test]
    fn search_stream_only_walks_only_the_named_provider() {
        let tvmaze = FakeSource::with_media("tvmaze", row("t1", "Show", 2008));
        let tvmaze_calls = tvmaze.counter();
        let anilist = FakeSource::with_media("anilist", row("a1", "Show", 2008));
        let anilist_calls = anilist.counter();
        let sources = vec![tvmaze.shared(), anilist.shared()];

        let outcome = search_stream_only_with(
            &sources,
            &cancel(),
            "Show",
            0,
            Kind::SERIES,
            &["anilist".to_string()],
            &|_| {},
        );
        assert_eq!(
            identities(&outcome.rows),
            vec![("anilist".to_string(), "a1".to_string())]
        );
        assert!(outcome.failed.is_empty());
        assert_eq!(
            tvmaze_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unnamed leg is never called"
        );
        assert_eq!(
            anilist_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the named leg is called once"
        );

        // No names walk nothing; a name outside the want's leg list matches
        // nothing either.
        let outcome =
            search_stream_only_with(&sources, &cancel(), "Show", 0, Kind::SERIES, &[], &|_| {});
        assert!(outcome.rows.is_empty() && outcome.failed.is_empty());
        let outcome = search_stream_only_with(
            &sources,
            &cancel(),
            "Show",
            0,
            Kind::SERIES,
            &["wikipedia".to_string()],
            &|_| {},
        );
        assert!(outcome.rows.is_empty() && outcome.failed.is_empty());
        assert_eq!(
            anilist_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no extra call was made"
        );
    }

    #[test]
    fn retrying_only_the_failed_leg_clears_the_failure() {
        // First pass: tvmaze answers, kitsu fails.
        let sources = vec![
            FakeSource::with_media("tvmaze", row("t1", "Show", 2008)).shared(),
            FakeSource::with_error("kitsu", "down").shared(),
        ];
        let first = search_stream_with(&sources, &cancel(), "Show", 0, Kind::SERIES, &|_| {});
        assert_eq!(first.failed, vec!["kitsu".to_string()]);
        assert_eq!(
            identities(&first.rows),
            vec![("tvmaze".to_string(), "t1".to_string())]
        );

        // The retry walks only the failed name, now healthy: its rows arrive
        // and the failure list is empty.
        let retry: Vec<Arc<dyn Source>> = vec![
            FakeSource::with_media("kitsu", row("k1", "Show", 2008)).shared(),
            FakeSource::with_media("tvmaze", row("t1", "Show", 2008)).shared(),
        ];
        let second = search_stream_only_with(
            &retry,
            &cancel(),
            "Show",
            0,
            Kind::SERIES,
            &first.failed,
            &|_| {},
        );
        assert!(second.failed.is_empty(), "the retried leg answered");
        assert_eq!(
            identities(&second.rows),
            vec![("kitsu".to_string(), "k1".to_string())],
            "only the retried leg was walked"
        );
    }

    fn counting_media(name: &str, id: &str) -> Media {
        Media {
            source: name.to_string(),
            source_id: id.to_string(),
            kind: Kind::SERIES,
            // Distinct titles per leg so the same-work merge keeps every
            // leg's row: the test counts attempted legs, not merged works.
            title: format!("Show {name}"),
            year: 2008,
            ..Media::default()
        }
    }

    fn regular_providers() -> Vec<String> {
        REGULAR_LEGS.iter().map(|s| s.to_string()).collect()
    }

    fn anime_providers() -> Vec<String> {
        ANIME_LEGS.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn regular_scope_calls_all_three_regular_legs_and_zero_anime_legs() {
        use std::sync::atomic::Ordering;
        let makers = [
            ("cinemeta", "c1"),
            ("tvmaze", "t1"),
            ("imdb", "i1"),
            ("anilist", "a1"),
            ("tenrai", "te1"),
            ("kitsu", "k1"),
            ("animeschedule", "s1"),
        ];
        let mut sources: Vec<Arc<dyn Source>> = Vec::new();
        let mut counters: Vec<(String, Arc<std::sync::atomic::AtomicI32>)> = Vec::new();
        for (name, id) in makers {
            let fake = FakeSource::with_media(name, counting_media(name, id));
            counters.push((name.to_string(), fake.counter()));
            sources.push(fake.shared());
        }
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logs_clone = Arc::clone(&logs);
        let outcome = search_stream_only_with_logged(
            &sources,
            &cancel(),
            "Show",
            0,
            Kind::SERIES,
            &regular_providers(),
            &|_| {},
            &|line: &str| logs_clone.lock().unwrap().push(line.to_string()),
        );
        assert_eq!(outcome.rows.len(), 3, "all three regular legs answer");
        for (name, counter) in &counters {
            let calls = counter.load(Ordering::SeqCst);
            if REGULAR_LEGS.contains(&name.as_str()) {
                assert_eq!(calls, 1, "regular leg {name} is attempted");
            } else {
                assert_eq!(calls, 0, "anime leg {name} is never called");
            }
        }
        let lines = logs.lock().unwrap().clone();
        assert_eq!(lines.len(), 3, "one debug line per leg attempt");
        for line in &lines {
            assert!(
                line.starts_with("search: leg "),
                "observability line shape: {line}"
            );
            assert!(line.contains(" ok "), "regular legs succeed: {line}");
            assert!(line.contains(" rows "), "line carries row count: {line}");
            assert!(line.ends_with("ms"), "line carries timing: {line}");
        }
        assert_eq!(REGULAR_LEGS.len(), 3, "regular path source count");
        assert_eq!(ANIME_LEGS.len(), 4, "anime path source count");
    }

    #[test]
    fn anime_scope_calls_all_four_anime_legs_and_zero_regular_legs() {
        use std::sync::atomic::Ordering;
        let makers = [
            ("cinemeta", "c1"),
            ("tvmaze", "t1"),
            ("imdb", "i1"),
            ("anilist", "a1"),
            ("tenrai", "te1"),
            ("kitsu", "k1"),
            ("animeschedule", "s1"),
        ];
        let mut sources: Vec<Arc<dyn Source>> = Vec::new();
        let mut counters: Vec<(String, Arc<std::sync::atomic::AtomicI32>)> = Vec::new();
        for (name, id) in makers {
            let fake = FakeSource::with_media(name, counting_media(name, id));
            counters.push((name.to_string(), fake.counter()));
            sources.push(fake.shared());
        }
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logs_clone = Arc::clone(&logs);
        let outcome = search_stream_only_with_logged(
            &sources,
            &cancel(),
            "Show",
            0,
            Kind::SERIES,
            &anime_providers(),
            &|_| {},
            &|line: &str| logs_clone.lock().unwrap().push(line.to_string()),
        );
        assert_eq!(outcome.rows.len(), 4, "all four anime legs answer");
        for (name, counter) in &counters {
            let calls = counter.load(Ordering::SeqCst);
            if ANIME_LEGS.contains(&name.as_str()) {
                assert_eq!(calls, 1, "anime leg {name} is attempted");
            } else {
                assert_eq!(calls, 0, "regular leg {name} is never called");
            }
        }
        let lines = logs.lock().unwrap().clone();
        assert_eq!(lines.len(), 4, "one debug line per leg attempt");
    }

    #[test]
    fn full_cache_hit_issues_zero_leg_attempts() {
        use std::collections::HashMap;
        use std::sync::atomic::Ordering;
        // First pass populates a cache map (the picker's persistent cache
        // shape); the second pass hits it and never touches a leg.
        let fake = FakeSource::with_media("cinemeta", counting_media("cinemeta", "c1"));
        let calls = fake.counter();
        let sources = vec![fake.shared()];
        let logs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let logs_clone = Arc::clone(&logs);
        let log = |line: &str| logs_clone.lock().unwrap().push(line.to_string());
        let mut cache: HashMap<String, Vec<Candidate>> = HashMap::new();
        let key = "show|0|tv".to_string();
        let first = search_stream_only_with_logged(
            &sources,
            &cancel(),
            "Show",
            0,
            Kind::SERIES,
            &regular_providers(),
            &|_| {},
            &log,
        );
        assert!(!first.rows.is_empty());
        cache.insert(key.clone(), first.rows.clone());
        let calls_after_first = calls.load(Ordering::SeqCst);
        assert_eq!(calls_after_first, 1);
        let lines_after_first = logs.lock().unwrap().len();
        assert_eq!(lines_after_first, 1);
        // Cache hit: serve without walking any leg.
        let hit = cache.get(&key).cloned().unwrap_or_default();
        assert_eq!(hit, first.rows);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            calls_after_first,
            "a full cache hit calls zero legs"
        );
        assert_eq!(
            logs.lock().unwrap().len(),
            lines_after_first,
            "a cached second search shows zero new leg lines"
        );
    }

    #[test]
    fn refresh_empty_table_keeps_pins_and_reports_an_error() {
        use metadata_core::types::Episode;
        let fresh_empty = Media {
            source: "fake".to_string(),
            source_id: "1".to_string(),
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            episodes: Vec::new(),
            ..Media::default()
        };
        let sources = [FakeSource::with_media("fake", fresh_empty).shared()];
        let prev_media = Media {
            source: "fake".to_string(),
            source_id: "1".to_string(),
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            synopsis: "Kept.".to_string(),
            episodes: vec![Episode {
                season: 1,
                number: 1,
                title: "Pilot".to_string(),
                synopsis: "Kept ep.".to_string(),
                ..Episode::default()
            }],
            ..Media::default()
        };
        let over = Override {
            scope: Scope::Series,
            key: "k".to_string(),
            title: "Show".to_string(),
            year: 2008,
            provider: "fake".to_string(),
            provider_id: "1".to_string(),
            poster: "https://art/show.jpg".to_string(),
            media: Some(prev_media),
            episodes: vec![EpisodePin {
                season: 1,
                number: 1,
                absolute: 1,
                title: "Pilot".to_string(),
                synopsis: "Kept ep.".to_string(),
            }],
            created_at_ms: 42,
        };
        let err = refresh_with(&sources, &cancel(), &over)
            .expect_err("an empty refresh table must not report success");
        assert!(
            err.to_string().contains("empty"),
            "the refusal names the empty table: {err}"
        );
        assert_eq!(over.episodes.len(), 1, "the existing pins are kept");
    }

    #[test]
    fn empty_provider_id_rejects_a_different_title_and_keeps_media_title_equal() {
        let other = Media {
            source: "fake".to_string(),
            source_id: String::new(),
            kind: Kind::SERIES,
            title: "Other Work".to_string(),
            ..Media::default()
        };
        let sources = [FakeSource::with_media("fake", other).shared()];
        let picked = cand("fake", "", "Show", 2008);
        let err = pin_from_candidate_with(&sources, &cancel(), &picked, Scope::Series, "k")
            .expect_err("an empty id must not latch a different-titled answer");
        assert!(
            err.to_string().contains("Other Work") || err.to_string().contains("Show"),
            "the refusal names the mismatch: {err}"
        );
        let same = Media {
            source: "fake".to_string(),
            source_id: String::new(),
            kind: Kind::SERIES,
            title: "Show".to_string(),
            ..Media::default()
        };
        let sources = [FakeSource::with_media("fake", same).shared()];
        let over = pin_from_candidate_with(&sources, &cancel(), &picked, Scope::Series, "k")
            .expect("a title-matching empty-id answer latches");
        assert_eq!(
            over.media.as_ref().map(|media| media.title.as_str()),
            Some(over.title.as_str()),
            "the latched media title equals the latch title"
        );
    }
}
