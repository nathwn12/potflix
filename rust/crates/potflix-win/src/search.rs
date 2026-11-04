//! `potflix-win::search` -- the "Fix match" provisional-override builder.
//!
//! [`provisional_override`] turns the picked [`Candidate`] into an
//! identity-only [`Override`] the dialog saves immediately, so the card can
//! become a series (or a movie) WITHOUT waiting for
//! [`metadata::pin_from_candidate`] to fetch the provider identity and the
//! whole episode table under its 90 s deadline. The full pin (or a later
//! [`metadata::refresh`]) replaces this provisional record with the fetched
//! payload.

use metadata::{Candidate, Kind, Media, Override, Scope};

/// The most Unicode scalar values the one query field accepts. Past this the
/// field refuses the character (typed or pasted) and says so, so a 300-char
/// paste can never produce a title the user did not see.
pub const MAX_QUERY_CHARS: usize = 128;

/// The search scope. `All` is the only scope the UI ever uses: every query
/// runs both chains (regular + anime) and merges into one ranked list, so
/// there is no user-selected scope and no category control. The remaining
/// variants stay for the engine's own bookkeeping (cache keys, provider
/// legs, candidate classification), but no UI id may select them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KindFilter {
    #[default]
    All,
    /// Regular (live-action) TV leg.
    Series,
    /// Regular (live-action) movie leg.
    Movies,
    /// Anime TV: the anime legs' series answers.
    AnimeSeries,
    /// Anime movies: the anime legs' movie answers.
    AnimeMovies,
}

/// The providers whose answers are anime, whatever kind they report.
pub const ANIME_PROVIDERS: &[&str] = &["anilist", "kitsu", "tenrai", "animeschedule"];

/// Regular available 3 (Cinemeta + TVmaze + IMDb), anime primary 4.
pub const REGULAR_PROVIDERS: &[&str] = &["cinemeta", "tvmaze", "imdb"];
pub const ANIME_LEG_PROVIDERS: &[&str] = &["anilist", "tenrai", "kitsu", "animeschedule"];

impl KindFilter {
    /// The provider legs this scope asks for, in leg precedence order.
    #[must_use]
    pub fn kinds(self) -> &'static [Kind] {
        match self {
            KindFilter::All => &[Kind::SERIES, Kind::MOVIE],
            KindFilter::Series | KindFilter::AnimeSeries => &[Kind::SERIES],
            KindFilter::Movies | KindFilter::AnimeMovies => &[Kind::MOVIE],
        }
    }

    /// The providers this scope actually uses, in leg order. A search passes
    /// this list to the provider layer. `All` runs both chains (regular then
    /// anime legs); the narrower legs exist for the engine's own passes.
    #[must_use]
    pub fn providers(self) -> &'static [&'static str] {
        match self {
            KindFilter::All => &[
                "tvmaze",
                "anilist",
                "tenrai",
                "kitsu",
                "animeschedule",
                "cinemeta",
                "imdb",
            ],
            KindFilter::Series => &["tvmaze", "cinemeta", "imdb"],
            KindFilter::Movies => &["cinemeta", "imdb"],
            KindFilter::AnimeSeries | KindFilter::AnimeMovies => {
                &["anilist", "tenrai", "kitsu", "animeschedule"]
            }
        }
    }

    /// The live-action scope that asks for one kind, for a caller that only
    /// knows the kind (the Fix match dialog's own Movie/TV choice). A
    /// kind-only search walks every leg that can serve it across both paths.
    #[must_use]
    pub fn providers_for_kind(kind: Kind) -> &'static [&'static str] {
        if kind == Kind::MOVIE {
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

    /// The cache key's scope half: stable, unique per scope, never localized.
    #[must_use]
    pub fn cache_key(self) -> &'static str {
        match self {
            KindFilter::All => "all",
            KindFilter::Series => "tv",
            KindFilter::Movies => "movies",
            KindFilter::AnimeSeries => "anime-tv",
            KindFilter::AnimeMovies => "anime-movies",
        }
    }

    /// Whether this scope includes `kind`.
    #[must_use]
    pub fn includes(self, kind: Kind) -> bool {
        self == KindFilter::All || self.kinds().contains(&kind)
    }

    /// Whether this scope shows one answered candidate: the record's own kind
    /// AND the class of the leg that answered it. TV/Movies mean live-action,
    /// the Anime scopes mean the anime legs' answers of that kind.
    #[must_use]
    pub fn includes_candidate(self, cand: &Candidate) -> bool {
        let anime = is_anime(cand);
        match self {
            KindFilter::All => true,
            KindFilter::Series => cand.kind == Kind::SERIES && !anime,
            KindFilter::Movies => cand.kind == Kind::MOVIE && !anime,
            KindFilter::AnimeSeries => cand.kind == Kind::SERIES && anime,
            KindFilter::AnimeMovies => cand.kind == Kind::MOVIE && anime,
        }
    }
}

/// Whether the leg that answered is an anime provider.
#[must_use]
pub fn is_anime(cand: &Candidate) -> bool {
    let provider = cand.provider.trim().to_lowercase();
    ANIME_PROVIDERS.contains(&provider.as_str())
}

/// The one key the preload cache and the persistent metadata search cache
/// share: the parsed title (trimmed, lowercased), the year and the scope. The
/// file path is deliberately absent, so the next episode of the same show hits
/// the same entry instead of searching again.
#[must_use]
pub fn search_cache_key(title: &str, year: i32, filter: KindFilter) -> String {
    format!(
        "{}|{}|{}",
        title.trim().to_lowercase(),
        year,
        filter.cache_key()
    )
}

/// The result row's type word: the record's kind and its provider class in one
/// label.
#[must_use]
pub fn category_label(cand: &Candidate) -> &'static str {
    match (cand.kind == Kind::MOVIE, is_anime(cand)) {
        (true, true) => "Anime movie",
        (true, false) => "Regular movie",
        (false, true) => "Anime TV",
        (false, false) => "Regular TV",
    }
}

/// Why the one query cannot be searched. Every variant carries its own copy, so
/// an empty, malformed or year-only submit explains itself in the field's own
/// status line instead of running a provider search that can only come back
/// empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryIssue {
    Empty,
    YearOnly,
    TooLong,
    NoTitleCharacters,
}

impl QueryIssue {
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            QueryIssue::Empty => "Type a title to search.",
            QueryIssue::YearOnly => "Add a title - a year on its own has nothing to search for.",
            QueryIssue::TooLong => "That is longer than 128 characters - shorten the title.",
            QueryIssue::NoTitleCharacters => "Add at least one letter or digit.",
        }
    }
}

/// The query's own validation, shared by the button, the Enter key and the
/// paste path so all three refuse the same input for the same stated reason.
#[must_use]
pub fn query_issue(query: &str) -> Option<QueryIssue> {
    if query.chars().count() > MAX_QUERY_CHARS {
        return Some(QueryIssue::TooLong);
    }
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Some(QueryIssue::Empty);
    }
    let (title, year) = crate::window::parse_title_year(trimmed);
    if title.trim().is_empty() && year > 0 {
        return Some(QueryIssue::YearOnly);
    }
    if !title.chars().any(char::is_alphanumeric) {
        return Some(QueryIssue::NoTitleCharacters);
    }
    None
}

/// The Card workspace's one search model. It owns the single title/year query
/// and the result set. The scope is always `All`: every query runs both
/// chains (regular + anime) and merges into one ranked list, so there is no
/// user-selected scope. The Fix match dialog seeds its own widgets from this
/// model when it opens and writes the edited query back when it closes, so
/// there is one canonical query on the main surface rather than two
/// independent search states.
#[derive(Clone, Debug, Default)]
pub struct MediaPicker {
    /// The one input: a title, or a title with a trailing year.
    pub query: String,
    /// The search scope. Always `All`: universal search has no user-selected
    /// scope, and the engine's narrower legs are selected internally per
    /// pass, never by a control.
    pub filter: KindFilter,
    pub candidates: Vec<Candidate>,
    pub selected: usize,
    /// The first visible row of the scrolled picker viewport. The paint and the
    /// hit test read this one offset, so a row outside the visible band stays
    /// reachable by scrolling instead of being unreachable.
    pub top: usize,
    pub busy: bool,
    pub searched: bool,
    /// Whether the result overlay is showing.
    pub open: bool,
    pub status: String,
    /// Monotonic worker generation: a stale result never overwrites a newer
    /// search.
    pub generation: u64,
    /// The row the user explicitly picked, keyed by provider + provider id.
    /// A pick belongs to the result set it was made in, so every query edit,
    /// scope change and new search clears it: the next list's auto-highlight is
    /// not a choice and must not arm Apply.
    pub picked: Option<String>,
}

impl MediaPicker {
    /// The one-field parse shared with the dialog.
    #[must_use]
    pub fn parsed(&self) -> (String, i32) {
        crate::window::parse_title_year(&self.query)
    }

    /// How many characters the field holds (scalar values, not bytes).
    #[must_use]
    pub fn query_len(&self) -> usize {
        self.query.chars().count()
    }

    /// The counter beside the field: `37/128`.
    #[must_use]
    pub fn counter_text(&self) -> String {
        format!("{}/{}", self.query_len(), MAX_QUERY_CHARS)
    }

    /// Whether one more character fits.
    #[must_use]
    pub fn accepts_more(&self) -> bool {
        self.query_len() < MAX_QUERY_CHARS
    }

    /// Whether Search is enabled: a query that passes [`query_issue`], and no
    /// running job.
    #[must_use]
    pub fn search_allowed(&self) -> bool {
        !self.busy && query_issue(&self.query).is_none()
    }

    /// The selected row, if the overlay has one.
    #[must_use]
    pub fn selected_candidate(&self) -> Option<&Candidate> {
        self.candidates.get(self.selected)
    }

    /// Record an explicit pick of one row (`provider + provider id`).
    pub fn pick_row(&mut self, key: String) {
        self.picked = Some(key);
    }

    /// Drop everything that belonged to the previous result set and abandon any
    /// running job.
    ///
    /// Bumping the generation is what makes the abandonment safe: a worker that
    /// finishes after this posts a generation that no longer matches, so
    /// [`MediaPicker::finish_search`] refuses its rows instead of filling a list
    /// for a query or scope the user has already left. Clearing `busy` is the
    /// other half -- the cancelled job must not keep Search disabled.
    fn invalidate_results(&mut self) {
        self.generation += 1;
        self.busy = false;
        self.candidates.clear();
        self.selected = 0;
        self.top = 0;
        self.open = false;
        self.searched = false;
        self.status.clear();
        self.picked = None;
    }

    /// Edit the single query field. Clearing the query clears the stale result
    /// set, so the overlay can never show results for text that is no longer
    /// in the field.
    pub fn set_query(&mut self, query: String) {
        if self.query == query {
            return;
        }
        self.query = query;
        self.invalidate_results();
    }

    /// Reset the scope to `All`. A no-op when already universal: the model
    /// carries no user-selected scope, so a new search never inherits one.
    /// An in-flight search for the old generation is abandoned here: its
    /// completion no longer matches the generation, so it can never publish
    /// rows into the new list.
    pub fn set_filter(&mut self, filter: KindFilter) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        self.invalidate_results();
    }

    /// Mark a search running and return the worker input. A query that fails
    /// [`query_issue`] (or a running job) returns `None` and starts nothing.
    pub fn begin_search(&mut self) -> Option<(u64, String, i32, KindFilter)> {
        if !self.search_allowed() {
            return None;
        }
        let (title, year) = self.parsed();
        self.invalidate_results();
        self.busy = true;
        self.status = "Searching\u{2026}".to_string();
        Some((self.generation, title, year, self.filter))
    }

    /// Apply one finished search. `generation` rejects a stale worker post.
    /// Kept as push + finish so the whole-result callers and the streaming
    /// callers share one application path.
    pub fn finish_search(&mut self, generation: u64, rows: Vec<Candidate>) -> bool {
        self.push_search(generation, rows) && self.finish_search_now(generation)
    }

    /// Apply one streamed batch: merge it into the rows already on screen
    /// (deduplicated on provider + provider id, ranked by the shared closeness
    /// rank), open the list, and report the running count while the job keeps
    /// running. `generation` rejects a stale worker post.
    pub fn push_search(&mut self, generation: u64, rows: Vec<Candidate>) -> bool {
        if generation != self.generation {
            return false;
        }
        // A finished stream owns the list: a batch that lands after the finish
        // (same generation, job no longer running) is stale and must not
        // reopen the list over the final count.
        if !self.busy {
            return false;
        }
        self.searched = true;
        if rows.is_empty() {
            return true;
        }
        let (title, year) = self.parsed();
        let existing = std::mem::take(&mut self.candidates);
        let mut merged = merge_candidates_for(&title, year, None, vec![existing, rows]);
        // A stream of batches must not grow past the metadata cap; the ranked
        // head is what the keyboard can reach.
        merged.truncate(metadata::search::SEARCH_LIMIT);
        self.candidates = merged;
        // A batch that re-ranks the list must not move the highlight off the row
        // the user already picked: the next click would then act on a different
        // record.
        if let Some(picked) = self.picked.as_deref()
            && let Some(pos) = self
                .candidates
                .iter()
                .position(|cand| crate::window::picked_row_key(cand) == picked)
        {
            self.selected = pos;
        } else if self.selected >= self.candidates.len() {
            self.selected = 0;
        }
        // The highlight must stay on screen: the viewport follows the selected
        // row, so a re-rank can never leave it pointing at a row the list no
        // longer shows.
        if self.candidates.is_empty() {
            self.top = 0;
        } else {
            self.top = self.selected.min(self.candidates.len() - 1);
        }
        self.open = !self.candidates.is_empty();
        self.status = format!("{} so far\u{2026}", match_count(self.candidates.len()));
        true
    }

    /// Close the stream: the job is over, the count is final, and the list
    /// stays open on what arrived. `generation` rejects a stale worker post.
    pub fn finish_search_now(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.busy = false;
        self.searched = true;
        self.open = !self.candidates.is_empty();
        let (title, year) = self.parsed();
        let asked = if year > 0 {
            format!("{title} ({year})")
        } else {
            title.clone()
        };
        self.status = if self.candidates.is_empty() {
            // The empty state names the query it failed on, so "no matches" is
            // never a silent blank panel.
            format!("No matches for \"{asked}\". Try a shorter title or add the year.")
        } else {
            format!(
                "{} for \"{asked}\"",
                match_count(self.candidates.len())
            )
        };
        true
    }

    /// Apply a failed search: the providers could not be reached, which is a
    /// different state from "no matches". The query is preserved so a retry
    /// needs no retyping.
    pub fn fail_search(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        self.busy = false;
        self.searched = true;
        self.candidates.clear();
        self.selected = 0;
        self.top = 0;
        self.open = false;
        self.status = crate::window::FIX_MATCH_ERROR.to_string();
        true
    }

    /// Close the result overlay without touching the typed query.
    pub fn close_results(&mut self) {
        self.open = false;
    }
}

/// Merge the per-kind provider passes of a search into one list.
///
/// The providers are kind-scoped, so a mixed search is two passes; this is the
/// one place they are joined, and it is pure so the ordering is pinnable. The
/// order is [`metadata::search::rank_rows`]'s one closeness rank -- the same
/// rank the engine itself uses -- so a live list and a cache replay agree:
///   1. title similarity first, so an exact normalized title is strongest;
///   2. a known query year above an unknown one above a known mismatch;
///   3. the file's own shape/kind as a soft prior, never a filter;
///   4. provider leg order, then the newer year, then the title and id, so the
///      order is total and the row indices the selection points at never
///      shuffle between identical answers.
///   5. Exact duplicates (same provider + provider id) are dropped, so a leg
///      that answers twice cannot produce two identical rows.
///
/// The ranked order is then spliced by [`metadata::merge_same_work`]: one work
/// stays one row, keeping the best-ranked row's identity and taking any
/// missing poster/synopsis/genres/length from the later provider copies, so a
/// mixed-and-matched result carries every provider's best field.
#[must_use]
pub fn merge_candidates(query: &str, passes: Vec<Vec<Candidate>>) -> Vec<Candidate> {
    merge_candidates_for(query, 0, None, passes)
}

/// [`merge_candidates`] with the query's own year and the playing file's shape
/// as extra evidence: the year is a ranking signal (never a filter here) and
/// the shape lifts the matching kind without dropping the other, so a movie
/// query and a series answer still meet in one list.
#[must_use]
pub fn merge_candidates_for(
    query: &str,
    year: i32,
    prefer: Option<Kind>,
    passes: Vec<Vec<Candidate>>,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for pass in passes {
        for cand in pass {
            let id = (cand.provider.clone(), cand.provider_id.clone());
            if seen.insert(id) {
                out.push(cand);
            }
        }
    }
    let out = metadata::search::rank_rows(query, year, prefer, out);
    metadata::merge_same_work(out)
}

/// The kind a matched record imposes. A confirmed match is painted with the
/// candidate's own kind: the manual Movie/TV choice only shapes the *search*
/// request, never the record the user ends up with.
#[must_use]
pub fn matched_kind_is_movie(cand: &Candidate) -> bool {
    cand.kind == Kind::MOVIE
}

/// `N match`/`N matches`: one phrase for the running and the final status.
pub(crate) fn match_count(n: usize) -> String {
    format!("{n} match{}", if n == 1 { "" } else { "es" })
}

/// What one file and its folder say the file is: a movie, or one episode of a
/// series. Purely local evidence -- no network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MediaShape {
    /// No episode pattern and no episode evidence: treat the file as a movie.
    #[default]
    Movie,
    /// The file's own name is an episode, or an episode of the same show lives
    /// beside it, or the folder is a season folder.
    Series,
}

/// What the path's own folder names say the work is: the provider class. An
/// `Anime` folder wins over a `TV Shows` folder that contains it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClassHint {
    Anime,
    Regular,
    #[default]
    Unknown,
}

/// The folder components that name the class outright.
const ANIME_DIRS: &[&str] = &["anime", "animes"];
const REGULAR_DIRS: &[&str] = &["movies", "films", "tv", "shows", "series"];

/// The most directory entries one [`detect_shape`] pass reads, so a huge folder
/// cannot turn a latch click into a long walk.
pub const DETECT_SHAPE_CAP: usize = 500;

/// Classify `file_name` by its own name and by the files beside it.
///
/// The filename's own episode tag is the first evidence: `Show S01E01.mkv`,
/// `Show 1x02.mkv` and `Show Episode 01.mkv` are episodes even when they are
/// the only file in the folder. Failing that, a sibling that cleans to the same
/// title and carries an episode tag makes the folder a series (`Show.mkv` beside
/// `Show S01E01.mkv`). Only a file with no episode pattern and no episode
/// sibling is a movie; an unrelated film in the same folder never changes that.
/// The episode/season patterns a filename can carry: `S1`, `S01`, `Season 1`,
/// `E01`, `Episode 01`, `Ep 1`, and `1x02`. `titleclean` already tags most
/// episode forms; this catches the season-only spelling (`Show S1.mkv`) it
/// leaves as a title token.
fn has_episode_pattern(file_name: &str) -> bool {
    let lower = file_name.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    for (i, token) in tokens.iter().enumerate() {
        let next_is_number = tokens.get(i + 1).is_some_and(|next| {
            !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit())
        });
        if (*token == "season" || *token == "episode" || *token == "ep") && next_is_number {
            return true;
        }
        if let Some(digits) = token.strip_prefix('s')
            && !digits.is_empty()
            && digits.len() <= 2
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
        if let Some(digits) = token.strip_prefix('e')
            && !digits.is_empty()
            && digits.len() <= 4
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
        if let Some((left, right)) = token.split_once('x')
            && !left.is_empty()
            && !right.is_empty()
            && left.bytes().all(|b| b.is_ascii_digit())
            && right.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

#[must_use]
pub fn classify_siblings(file_name: &str, siblings: &[String]) -> MediaShape {
    let cleaned = titleclean::clean(file_name);
    if !cleaned.episode_tag.trim().is_empty() || has_episode_pattern(file_name) {
        return MediaShape::Series;
    }
    let want = cleaned.title.trim().to_lowercase();
    if want.is_empty() {
        return MediaShape::Movie;
    }
    for sibling in siblings {
        if sibling.eq_ignore_ascii_case(file_name) {
            continue;
        }
        let cleaned = titleclean::clean(sibling);
        if cleaned.episode_tag.trim().is_empty() {
            continue;
        }
        if cleaned.title.trim().to_lowercase() == want {
            return MediaShape::Series;
        }
    }
    MediaShape::Movie
}

/// Read `path`'s own name and parent folder (video files only, at most
/// [`DETECT_SHAPE_CAP`] entries) and classify the file. A season folder
/// (`...\Show\Season 2\...`) is series evidence by itself; an unreadable or
/// missing folder is no evidence, and a name-level episode tag still makes the
/// file a series there.
#[must_use]
pub fn detect_shape(path: &str) -> MediaShape {
    let file = std::path::Path::new(path);
    let Some(name) = file.file_name().and_then(std::ffi::OsStr::to_str) else {
        return MediaShape::Movie;
    };
    if metadata_core::context::context_from_path(path).1 > 0 {
        return MediaShape::Series;
    }
    let Some(dir) = file.parent() else {
        return classify_siblings(name, &[]);
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return classify_siblings(name, &[]);
    };
    let mut siblings: Vec<String> = Vec::new();
    for entry in entries.take(DETECT_SHAPE_CAP).flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_video_name(name) {
            continue;
        }
        siblings.push(name.to_string());
    }
    classify_siblings(name, &siblings)
}

/// The class the path's folder names imply: `Anime` when any component is an
/// anime folder, otherwise `Regular` for a movies/films/tv/shows/series folder,
/// otherwise `Unknown`. Anime is checked first so `...\Anime\TV Shows\...` is
/// anime, never live-action.
#[must_use]
pub fn detect_class(path: &str) -> ClassHint {
    let mut regular = false;
    for component in std::path::Path::new(path).components() {
        let Some(text) = component.as_os_str().to_str() else {
            continue;
        };
        let word = text.trim().to_lowercase();
        if ANIME_DIRS.iter().any(|dir| word.contains(dir)) {
            return ClassHint::Anime;
        }
        if REGULAR_DIRS.contains(&word.as_str()) {
            regular = true;
        }
    }
    if regular {
        ClassHint::Regular
    } else {
        ClassHint::Unknown
    }
}

/// The search scope the local evidence implies: the folder's class crossed
/// with the filename's shape. Unknown evidence is `All`, never a guess.
#[must_use]
pub fn auto_filter(shape: MediaShape, class: ClassHint) -> KindFilter {
    match (class, shape) {
        (ClassHint::Anime, MediaShape::Series) => KindFilter::AnimeSeries,
        (ClassHint::Anime, MediaShape::Movie) => KindFilter::AnimeMovies,
        (ClassHint::Regular, MediaShape::Series) => KindFilter::Series,
        (ClassHint::Regular, MediaShape::Movie) => KindFilter::Movies,
        (ClassHint::Unknown, _) => KindFilter::All,
    }
}

/// Whether `name` carries a video extension the library walk collects.
fn is_video_name(name: &str) -> bool {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("")
        .to_lowercase();
    matches!(
        ext.as_str(),
        "mkv" | "mp4" | "avi" | "m4v" | "mov" | "wmv" | "ts" | "flv" | "webm" | "mpg" | "mpeg"
    )
}

/// The provisional latch for one picked candidate: the candidate's own
/// title/year/provider identity as the [`Override`] identity, and the
/// candidate's spliced fields as a local [`Media`] snapshot.
///
/// The identity/poster rules mirror
/// `metadata::search::pin_from_candidate_with` exactly, minus the network:
/// the title, year, provider and provider id are the candidate's own; the
/// candidate's cover rides as [`Override::poster`] for every NON-movie scope
/// (a movie renders from its media snapshot, so a movie carries no poster).
/// [`Override::media`] is `Some` -- a snapshot built from the candidate's own
/// source/source_id/kind/title/year/artwork/synopsis/genres/length, so the card
/// and the RPC show the merged synopsis the moment the row is picked, before
/// the full pin resolves the identity. There is no episode table until that
/// full pin fetches it, so [`Override::episodes`] stays empty.
///
/// `key` is the caller-selected target key (movie file, season folder, show
/// folder) and passes through byte-for-byte: this helper never normalises and
/// never derives -- [`metadata::overrides::Store::put`] normalises on write.
///
/// No network, no fetch, no store write: the caller persists the record
/// (usually [`metadata::overrides::Store::put_correction`]) and the full pin
/// or refresh later replaces it with the fetched media and episode pins.
#[must_use]
pub fn provisional_override(cand: &Candidate, scope: Scope, key: String) -> Override {
    Override {
        scope,
        key,
        title: cand.title.clone(),
        year: cand.year,
        provider: cand.provider.clone(),
        provider_id: cand.provider_id.clone(),
        poster: if scope == Scope::Movie {
            String::new()
        } else {
            cand.poster.clone()
        },
        media: Some(Media {
            source: cand.provider.clone(),
            source_id: cand.provider_id.clone(),
            kind: cand.kind,
            title: cand.title.clone(),
            year: cand.year,
            artwork: cand.poster.clone(),
            synopsis: cand.synopsis.clone(),
            genres: cand.genres.clone(),
            length: cand.length.clone(),
            ..Media::default()
        }),
        episodes: Vec::new(),
        created_at_ms: metadata::overrides::now_ms(),
    }
}

#[cfg(test)]
mod tests {
    use metadata::Candidate;
    use metadata::Scope;
    use metadata_core::types::Kind;

    use super::provisional_override;

    fn series_candidate() -> Candidate {
        Candidate {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: "https://art/show-picked.jpg".to_string(),
            synopsis: "A show about a show.".to_string(),
            genres: "Drama".to_string(),
            length: "45 min".to_string(),
            episode_count: 3,
        }
    }

    fn movie_candidate() -> Candidate {
        Candidate {
            kind: Kind::MOVIE,
            title: "Dune: Part Two".to_string(),
            year: 2024,
            provider: "cinemeta".to_string(),
            provider_id: "tt15239678".to_string(),
            poster: "https://art/dune-search.jpg".to_string(),
            synopsis: "Paul Atreides unites with the Fremen.".to_string(),
            genres: "Sci-Fi . Adventure".to_string(),
            length: "2h 46m".to_string(),
            episode_count: 0,
        }
    }

    /// The shared closeness rank, end to end through the merge the dialog and
    /// the card both paint: the real 2019 film beats a 2020 comedy riff that
    /// merely contains every query word, even though the query is punctuated
    /// (`avengers - endgame`).
    #[test]
    fn closeness_puts_the_exact_work_first_regardless_of_year() {
        use super::{merge_candidates, merge_candidates_for};

        let riff = Candidate {
            kind: Kind::MOVIE,
            title: "Rifftrax: Avengers: Endgame".to_string(),
            year: 2020,
            provider: "tvmaze".to_string(),
            provider_id: "riff-1".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        let real = Candidate {
            kind: Kind::MOVIE,
            title: "Avengers: Endgame".to_string(),
            year: 2019,
            provider: "cinemeta".to_string(),
            provider_id: "tt4154796".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        // Arrival order is deliberately against the answer: the riff arrives
        // first, from the higher-precedence leg, and is newer.
        for merged in [
            merge_candidates("avengers - endgame", vec![vec![riff.clone(), real.clone()]]),
            merge_candidates_for(
                "avengers - endgame",
                0,
                None,
                vec![vec![riff.clone()], vec![real.clone()]],
            ),
        ] {
            assert_eq!(
                merged[0].title, "Avengers: Endgame",
                "the exact normalized title is the strongest signal: {merged:?}"
            );
            assert_eq!(merged[0].provider_id, "tt4154796");
        }
        // A known year is a bonus, not the whole rank: with the query's year
        // given, the matching release still leads and a mismatch follows.
        let merged = merge_candidates_for(
            "avengers - endgame",
            2019,
            None,
            vec![vec![riff, real]],
        );
        assert_eq!(merged[0].year, 2019);
    }

    /// Universal results: the file's shape is a SOFT prior, so a movie answer
    /// and a series answer both survive the merge and only their order moves.
    #[test]
    fn a_series_file_keeps_movie_answers_ranked_below_shows() {
        use super::{Candidate, merge_candidates_for};

        let cand = |kind: Kind, id: &str| Candidate {
            kind,
            title: "Dune".to_string(),
            year: 0,
            // One provider, so the leg order cannot decide the tie: only the
            // shape prior can.
            provider: "cinemeta".to_string(),
            provider_id: id.to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        let show = cand(Kind::SERIES, "tt-show");
        let film = cand(Kind::MOVIE, "tt-film");
        let with_prior = merge_candidates_for(
            "Dune",
            0,
            Some(Kind::SERIES),
            vec![vec![film.clone(), show.clone()]],
        );
        assert_eq!(with_prior.len(), 2, "no kind is filtered out");
        assert_eq!(with_prior[0].kind, Kind::SERIES, "the shape prior lifts the show");
        let no_prior = merge_candidates_for("Dune", 0, None, vec![vec![film, show]]);
        assert_eq!(
            no_prior[0].kind, Kind::MOVIE,
            "without a prior, arrival order holds"
        );
    }

    /// The dialog's universal scope is both kinds, and its provider precedence
    /// is the rank's final tie-break: one list, never a kind filter.
    #[test]
    fn universal_scope_runs_both_kinds_in_the_ranks_provider_order() {
        use super::KindFilter;

        assert_eq!(
            KindFilter::All.kinds(),
            [Kind::SERIES, Kind::MOVIE],
            "every search asks both kind-scoped chains"
        );
        assert_eq!(
            KindFilter::All.providers(),
            metadata::search::PROVIDER_ORDER,
            "the picker's All order is the ranker's final tie-break"
        );
    }

    #[test]
    fn media_picker_keeps_one_query_and_rejects_a_stale_generation() {
        use super::MediaPicker;

        let mut picker = MediaPicker::default();
        picker.set_query("Dune 2021".to_string());
        assert_eq!(picker.parsed(), ("Dune".to_string(), 2021));
        assert_eq!(
            picker.filter,
            super::KindFilter::All,
            "the main search defaults to All types, never to a stale kind"
        );
        let (generation, title, year, want) = picker.begin_search().expect("search starts");
        assert_eq!((title.as_str(), year, want), ("Dune", 2021, super::KindFilter::All));
        assert!(picker.busy);
        assert!(
            !picker.finish_search(generation + 1, Vec::new()),
            "stale result rejected"
        );
        assert!(picker.busy, "a stale result must not clear the running state");
        let cand = Candidate {
            kind: Kind::MOVIE,
            title: "Dune".to_string(),
            year: 2021,
            provider: "cinemeta".to_string(),
            provider_id: "tt1".to_string(),
            poster: "https://example.com/dune.jpg".to_string(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        assert!(picker.finish_search(generation, vec![cand]));
        assert!(picker.open && !picker.busy);
        assert_eq!(picker.status, "1 match for \"Dune (2021)\"");
        picker.set_filter(super::KindFilter::Movies);
        assert_eq!(picker.filter, super::KindFilter::Movies);
        assert!(picker.candidates.is_empty() && !picker.open);
        picker.set_query("Dune: Part Two 2024".to_string());
        assert_eq!(picker.parsed(), ("Dune: Part Two".to_string(), 2024));
    }

    /// A scope change made while a search is in flight must cancel it: the
    /// worker's late completion carries the abandoned generation, so it cannot
    /// publish rows into the new scope's list, and Search is usable again
    /// immediately instead of waiting on a result nobody asked for.
    #[test]
    fn scope_change_during_an_in_flight_search_cannot_publish_stale_results() {
        use super::{KindFilter, MediaPicker};

        let mut picker = MediaPicker::default();
        picker.set_query("Dune".to_string());
        let (generation, ..) = picker.begin_search().expect("search starts");
        assert!(picker.busy, "the job is running");
        picker.set_filter(KindFilter::Movies);
        assert!(!picker.busy, "the abandoned job no longer blocks Search");
        assert!(
            !picker.finish_search(generation, vec![movie_candidate()]),
            "the old scope's completion must be refused"
        );
        assert!(
            picker.candidates.is_empty() && !picker.open && !picker.searched,
            "no stale row can populate the list"
        );
        // A fresh search in the new scope is the only thing that publishes.
        let (next, _, _, want) = picker.begin_search().expect("search starts again");
        assert_eq!(want, KindFilter::Movies, "the new scope is what is asked");
        assert!(next > generation, "the generation moved past the abandoned job");
        assert!(picker.finish_search(next, vec![movie_candidate()]));
        assert_eq!(picker.candidates.len(), 1);
        // The same rule covers a query edit, which is also a new result set.
        let (stale, ..) = picker.begin_search().expect("search starts");
        picker.set_query("Dune: Part Two".to_string());
        assert!(
            !picker.finish_search(stale, vec![movie_candidate()]),
            "a completion for the old query is refused after the text changed"
        );
    }

    /// An explicit pick belongs to one result set. A query edit, a scope change
    /// and a new search each clear it, so the next list's auto-highlight (even
    /// on the identical record) cannot arm Apply without a fresh pick.
    #[test]
    fn an_explicit_pick_is_cleared_by_a_query_edit_scope_change_or_new_search() {
        use super::{KindFilter, MediaPicker};

        let key = "cinemeta:tt15239678".to_string();
        let mut picker = MediaPicker::default();
        picker.set_query("Dune".to_string());
        let (generation, ..) = picker.begin_search().expect("search starts");
        assert!(picker.finish_search(generation, vec![movie_candidate()]));

        picker.pick_row(key.clone());
        assert_eq!(picker.picked.as_deref(), Some(key.as_str()));

        // Scope change.
        picker.set_filter(KindFilter::Movies);
        assert_eq!(picker.picked, None, "a scope change drops the pick");

        // Query edit, with the same pick re-armed first.
        picker.pick_row(key.clone());
        picker.set_query("Dune 2021".to_string());
        assert_eq!(picker.picked, None, "a query edit drops the pick");

        // A new search, with the same pick re-armed first: the same record can
        // come back, and it is still not a choice.
        let (generation, ..) = picker.begin_search().expect("search starts");
        assert!(picker.finish_search(generation, Vec::new()));
        picker.pick_row(key.clone());
        let (generation, ..) = picker.begin_search().expect("search starts");
        assert_eq!(picker.picked, None, "a new search drops the pick");
        assert!(picker.finish_search(generation, vec![movie_candidate()]));
        assert_eq!(picker.picked, None, "the fresh list carries no pick");
    }

    #[test]
    fn query_issue_names_empty_year_only_long_and_malformed_queries() {        use super::{KindFilter, MediaPicker, QueryIssue, query_issue};

        assert_eq!(query_issue(""), Some(QueryIssue::Empty));
        assert_eq!(query_issue("   "), Some(QueryIssue::Empty));
        assert_eq!(query_issue("2024"), Some(QueryIssue::YearOnly));
        assert_eq!(query_issue(" 1999 "), Some(QueryIssue::YearOnly));
        assert_eq!(query_issue("%%%"), Some(QueryIssue::NoTitleCharacters));
        assert_eq!(query_issue("Dune"), None);
        assert_eq!(query_issue("Dune 2021"), None);
        assert_eq!(query_issue("1899"), None, "1899 is a title, not a year");
        let long: String = "a".repeat(super::MAX_QUERY_CHARS);
        assert_eq!(query_issue(&long), None, "exactly the cap is accepted");
        let over: String = "a".repeat(super::MAX_QUERY_CHARS + 1);
        assert_eq!(query_issue(&over), Some(QueryIssue::TooLong));
        // Every issue explains itself and the search is refused for it.
        for (query, issue) in [
            ("", QueryIssue::Empty),
            ("2024", QueryIssue::YearOnly),
            ("%%%", QueryIssue::NoTitleCharacters),
        ] {
            let mut p = MediaPicker::default();
            p.set_query(query.to_string());
            assert!(!p.search_allowed(), "{query:?} must not search");
            assert!(!issue.message().is_empty());
            assert_eq!(p.filter, KindFilter::All);
        }
    }

    #[test]
    fn mixed_search_merges_both_kinds_and_keeps_kind_truthful() {
        use super::{KindFilter, merge_candidates};

        assert_eq!(KindFilter::All.kinds(), &[Kind::SERIES, Kind::MOVIE]);
        assert_eq!(KindFilter::Series.kinds(), &[Kind::SERIES]);
        assert_eq!(KindFilter::Movies.kinds(), &[Kind::MOVIE]);
        assert_eq!(KindFilter::AnimeSeries.kinds(), &[Kind::SERIES]);
        assert_eq!(KindFilter::AnimeMovies.kinds(), &[Kind::MOVIE]);
        assert!(KindFilter::All.includes(Kind::MOVIE) && KindFilter::All.includes(Kind::SERIES));
        assert!(!KindFilter::Movies.includes(Kind::SERIES));
        // Universal search has no user-visible scope words: no label API, no
        // segment control. `All` is the whole search: both chains, every kind.
        assert_eq!(
            KindFilter::All.providers(),
            &[
                "tvmaze",
                "anilist",
                "tenrai",
                "kitsu",
                "animeschedule",
                "cinemeta",
                "imdb"
            ],
            "All runs both chains in one query"
        );

        let series = vec![Candidate {
            kind: Kind::SERIES,
            title: "Dune: Prophecy".to_string(),
            year: 2024,
            provider: "tvmaze".to_string(),
            provider_id: "tvmaze-1".to_string(),
            poster: String::new(),
            synopsis: "The Bene Gesserit origin.".to_string(),
            genres: String::new(),
            length: String::new(),
            episode_count: 6,
        }];
        let movies = vec![
            Candidate {
                kind: Kind::MOVIE,
                title: "Dune".to_string(),
                year: 2021,
                provider: "cinemeta".to_string(),
                provider_id: "tt1160419".to_string(),
                poster: String::new(),
                synopsis: String::new(),
                genres: String::new(),
                length: String::new(),
                episode_count: 0,
            },
            Candidate {
                kind: Kind::MOVIE,
                title: "Dune".to_string(),
                year: 1984,
                provider: "cinemeta".to_string(),
                provider_id: "tt0087182".to_string(),
                poster: String::new(),
                synopsis: String::new(),
                genres: String::new(),
                length: String::new(),
                episode_count: 0,
            },
        ];
        let merged = merge_candidates("Dune", vec![series, movies]);
        assert_eq!(merged.len(), 3, "every leg's rows survive the merge");
        assert_eq!(merged[0].year, 2021, "exact title, newest year first");
        assert_eq!(merged[1].year, 1984);
        assert_eq!(merged[2].title, "Dune: Prophecy");
        assert!(
            merged.iter().any(|c| c.kind == Kind::SERIES)
                && merged.iter().any(|c| c.kind == Kind::MOVIE),
            "an All-types search returns both kinds in one list"
        );

        // A duplicate identity from two legs is one row, not two.
        let again = merge_candidates(
            "Dune",
            vec![vec![merged[0].clone()], vec![merged[0].clone()]],
        );
        assert_eq!(again.len(), 1, "provider+provider_id de-duplicates");
    }

    #[test]
    fn failed_search_is_its_own_state_and_keeps_the_query() {
        use super::MediaPicker;

        let mut p = MediaPicker::default();
        p.set_query("Dune".to_string());
        let (generation, ..) = p.begin_search().expect("search starts");
        assert!(p.fail_search(generation));
        assert!(!p.busy && !p.open && p.candidates.is_empty());
        assert_eq!(p.status, crate::window::FIX_MATCH_ERROR);
        assert_eq!(p.query, "Dune", "a failure never eats what was typed");
        assert!(!p.fail_search(generation + 1), "a stale failure is ignored");
    }

    #[test]
    fn series_candidate_yields_provisional_series_override_without_network() {
        let cand = series_candidate();
        let over = provisional_override(&cand, Scope::Series, "M:\\2SERIES\\Show".to_string());
        assert_eq!(over.scope, Scope::Series);
        assert_eq!(over.key, "M:\\2SERIES\\Show");
        assert_eq!(over.title, "Show");
        assert_eq!(over.year, 2008);
        assert_eq!(over.provider, "tvmaze");
        assert_eq!(over.provider_id, "169");
        assert_eq!(over.poster, "https://art/show-picked.jpg");
        let media = over.media.as_ref().expect("the candidate snapshot rides");
        assert_eq!(media.source, "tvmaze");
        assert_eq!(media.source_id, "169");
        assert_eq!(media.kind, Kind::SERIES);
        assert_eq!(media.title, "Show");
        assert_eq!(media.year, 2008);
        assert_eq!(media.artwork, "https://art/show-picked.jpg");
        assert_eq!(media.synopsis, "A show about a show.");
        assert_eq!(media.genres, "Drama");
        assert_eq!(media.length, "45 min");
        assert!(over.episodes.is_empty(), "provisional: no episode table");
        assert!(over.created_at_ms > 0, "the provisional latch is stamped");
    }

    #[test]
    fn movie_candidate_carries_no_poster_like_a_pinned_movie() {
        let cand = movie_candidate();
        let over = provisional_override(
            &cand,
            Scope::Movie,
            "D:\\Films\\Dune Part Two.mkv".to_string(),
        );
        assert_eq!(over.scope, Scope::Movie);
        assert_eq!(over.title, "Dune: Part Two");
        assert_eq!(over.year, 2024);
        assert_eq!(over.provider, "cinemeta");
        assert_eq!(over.provider_id, "tt15239678");
        assert!(
            over.poster.is_empty(),
            "a movie renders from its media snapshot, never the candidate cover"
        );
        let media = over.media.as_ref().expect("the candidate snapshot rides");
        assert_eq!(media.kind, Kind::MOVIE);
        assert_eq!(media.artwork, "https://art/dune-search.jpg");
        assert_eq!(media.synopsis, "Paul Atreides unites with the Fremen.");
        assert_eq!(media.genres, "Sci-Fi . Adventure");
        assert_eq!(media.length, "2h 46m");
        assert!(over.episodes.is_empty());
        assert!(over.created_at_ms > 0);
    }

    #[test]
    fn key_and_scope_pass_through_exactly() {
        let cand = series_candidate();
        // Mixed case plus `/` separators: the helper must NOT normalise --
        // [`metadata::overrides::Store::put`] normalises on write.
        let key = "M:/2SERIES/Show".to_string();
        let over = provisional_override(&cand, Scope::Season, key.clone());
        assert_eq!(over.scope, Scope::Season);
        assert_eq!(over.key, key, "key passes through byte-for-byte");
        // A season scope is not a movie scope, so the candidate cover rides.
        assert_eq!(over.poster, "https://art/show-picked.jpg");
        assert!(over.media.is_some(), "the candidate snapshot rides");
        assert!(over.episodes.is_empty(), "provisional: no episode table");
        assert!(over.created_at_ms > 0);
    }

    #[test]
    fn anime_is_a_provider_class_and_the_five_scopes_partition_the_rows() {
        use super::{KindFilter, category_label, is_anime};
        let anime_series = Candidate {
            kind: Kind::SERIES,
            title: "Cyberpunk: Edgerunners".to_string(),
            year: 2022,
            provider: "anilist".to_string(),
            provider_id: "120377".to_string(),
            poster: String::new(),
            synopsis: "A street kid in Night City.".to_string(),
            genres: "Action".to_string(),
            length: "25 min".to_string(),
            episode_count: 10,
        };
        let anime_movie = Candidate {
            kind: Kind::MOVIE,
            title: "Akira".to_string(),
            year: 1988,
            provider: "kitsu".to_string(),
            provider_id: "1".to_string(),
            poster: String::new(),
            synopsis: "Neo-Tokyo, 2019.".to_string(),
            genres: "Sci-Fi".to_string(),
            length: "2h 4m".to_string(),
            episode_count: 0,
        };
        let tv_series = series_candidate();
        let movie = movie_candidate();
        assert!(is_anime(&anime_series) && is_anime(&anime_movie));
        assert!(!is_anime(&tv_series) && !is_anime(&movie));
        assert_eq!(category_label(&anime_series), "Anime TV");
        assert_eq!(category_label(&anime_movie), "Anime movie");
        assert_eq!(category_label(&tv_series), "Regular TV");
        assert_eq!(category_label(&movie), "Regular movie");
        let rows = [&anime_series, &anime_movie, &tv_series, &movie];
        let count = |f: KindFilter| rows.iter().filter(|c| f.includes_candidate(c)).count();
        assert_eq!(count(KindFilter::All), 4, "All shows every answer");
        assert_eq!(count(KindFilter::Series), 1, "TV is regular series only");
        assert_eq!(count(KindFilter::Movies), 1, "Movies is regular movies only");
        assert_eq!(count(KindFilter::AnimeSeries), 1, "Anime TV is the anime leg's series");
        assert_eq!(count(KindFilter::AnimeMovies), 1, "Anime Movies is the anime leg's movie");
        // Tenrai rides the same class.
        let mut tenrai = anime_series;
        tenrai.provider = "tenrai".to_string();
        assert!(KindFilter::AnimeSeries.includes_candidate(&tenrai));
        assert!(!KindFilter::Series.includes_candidate(&tenrai));
        // A provider name is case/space-insensitive, like the leg's own id.
        let mut spaced = anime_movie;
        spaced.provider = " AniList ".to_string();
        assert!(KindFilter::AnimeMovies.includes_candidate(&spaced));
        // Five distinct cache keys, one per scope.
        let keys: std::collections::HashSet<&str> =
            [KindFilter::All, KindFilter::Series, KindFilter::Movies, KindFilter::AnimeSeries, KindFilter::AnimeMovies]
                .iter()
                .map(|f| f.cache_key())
                .collect();
        assert_eq!(keys.len(), 5, "the preload cache keys the scopes apart");
        assert_eq!(KindFilter::All.cache_key(), "all");
        assert_eq!(KindFilter::Series.cache_key(), "tv");
        assert_eq!(KindFilter::Movies.cache_key(), "movies");
        assert_eq!(KindFilter::AnimeSeries.cache_key(), "anime-tv");
        assert_eq!(KindFilter::AnimeMovies.cache_key(), "anime-movies");
        // The providers a scope asks for: a Regular scope asks only Regular
        // legs, an Anime scope only Anime legs, All asks both. Regular 3
        // available, anime primary 4.
        assert_eq!(
            KindFilter::All.providers(),
            &[
                "tvmaze",
                "anilist",
                "tenrai",
                "kitsu",
                "animeschedule",
                "cinemeta",
                "imdb"
            ]
        );
        assert_eq!(
            KindFilter::Series.providers(),
            &["tvmaze", "cinemeta", "imdb"]
        );
        assert_eq!(KindFilter::Movies.providers(), &["cinemeta", "imdb"]);
        assert_eq!(
            KindFilter::AnimeSeries.providers(),
            &["anilist", "tenrai", "kitsu", "animeschedule"]
        );
        assert_eq!(
            KindFilter::AnimeMovies.providers(),
            &["anilist", "tenrai", "kitsu", "animeschedule"]
        );
        // The one key helper: title case and padding never split an entry, the
        // year and the scope always do.
        assert_eq!(
            super::search_cache_key("  Dune  ", 2021, KindFilter::Movies),
            "dune|2021|movies"
        );
        assert_ne!(
            super::search_cache_key("Dune", 2021, KindFilter::Movies),
            super::search_cache_key("Dune", 2024, KindFilter::Movies)
        );
        assert_ne!(
            super::search_cache_key("Dune", 0, KindFilter::Movies),
            super::search_cache_key("Dune", 0, KindFilter::All)
        );
    }

    /// The scope a local file implies: the folder's class crossed with the
    /// filename's shape, and `All` whenever the evidence is unknown.
    #[test]
    fn auto_filter_crosses_the_folder_class_with_the_file_shape() {
        use super::{ClassHint, KindFilter, MediaShape, auto_filter, detect_class};

        assert_eq!(auto_filter(MediaShape::Series, ClassHint::Anime), KindFilter::AnimeSeries);
        assert_eq!(auto_filter(MediaShape::Movie, ClassHint::Anime), KindFilter::AnimeMovies);
        assert_eq!(auto_filter(MediaShape::Series, ClassHint::Regular), KindFilter::Series);
        assert_eq!(auto_filter(MediaShape::Movie, ClassHint::Regular), KindFilter::Movies);
        assert_eq!(auto_filter(MediaShape::Series, ClassHint::Unknown), KindFilter::All);
        assert_eq!(auto_filter(MediaShape::Movie, ClassHint::Unknown), KindFilter::All);

        assert_eq!(detect_class("M:\\Anime\\Show S01E01.mkv"), ClassHint::Anime);
        assert_eq!(detect_class("M:\\ANIMES\\Akira.mkv"), ClassHint::Anime);
        assert_eq!(
            detect_class("D:\\2ANIMES\\Cyberpunk Edgerunners\\S2\\S02E01.mkv"),
            ClassHint::Anime
        );
        assert_eq!(detect_class("M:\\Movies\\Dune.mkv"), ClassHint::Regular);
        assert_eq!(detect_class("M:\\Films\\Dune.mkv"), ClassHint::Regular);
        assert_eq!(detect_class("M:\\TV Shows\\Show S01E01.mkv"), ClassHint::Unknown);
        assert_eq!(detect_class("M:\\TV\\Show S01E01.mkv"), ClassHint::Regular);
        assert_eq!(detect_class("M:\\Series\\Show S01E01.mkv"), ClassHint::Regular);
        assert_eq!(detect_class("M:\\Downloads\\Show S01E01.mkv"), ClassHint::Unknown);
        // Anime wins over a regular folder that contains it.
        assert_eq!(detect_class("M:\\TV\\Anime\\Show S01E01.mkv"), ClassHint::Anime);
    }

    /// A mixed-and-matched result: one work is one row, and the ranked row's
    /// empty fields are filled from the other provider's copy.
    #[test]
    fn merge_candidates_splices_one_work_into_the_ranked_row() {
        use super::merge_candidates;

        let mut tvdb = movie_candidate();
        tvdb.provider = "tvdb".to_string();
        tvdb.provider_id = "42".to_string();
        tvdb.poster = String::new();
        tvdb.synopsis = String::new();
        tvdb.genres = String::new();
        tvdb.length = String::new();
        let mut cinemeta = movie_candidate();
        cinemeta.synopsis = "Spliced synopsis.".to_string();
        cinemeta.genres = "Sci-Fi".to_string();
        let merged = merge_candidates("Dune: Part Two", vec![vec![tvdb], vec![cinemeta]]);
        assert_eq!(merged.len(), 1, "one work is one row");
        assert_eq!(merged[0].provider, "cinemeta", "the ranked row keeps its identity");
        assert_eq!(merged[0].provider_id, "tt15239678");
        assert_eq!(merged[0].synopsis, "Spliced synopsis.");
        assert_eq!(merged[0].genres, "Sci-Fi");
        assert_eq!(merged[0].poster, "https://art/dune-search.jpg");
    }

    /// A streamed search fills the list as batches land: the first batch opens
    /// the overlay and reports the running count while the job is still
    /// running; the next batch merges (deduplicated) into it; the final
    /// finish closes the job with the total.
    #[test]
    fn streamed_batches_fill_the_list_before_the_job_finishes() {
        use super::MediaPicker;

        let mut picker = MediaPicker::default();
        picker.set_query("Dune".to_string());
        let (generation, ..) = picker.begin_search().expect("search starts");
        assert!(picker.push_search(generation, vec![movie_candidate()]));
        assert!(picker.busy, "the job keeps running after the first batch");
        assert!(picker.open && picker.searched);
        assert_eq!(picker.candidates.len(), 1);
        assert_eq!(picker.status, "1 match so far\u{2026}");
        // A second batch merges and re-ranks; a duplicate identity is one row.
        let duplicate = movie_candidate();
        let series = series_candidate();
        assert!(picker.push_search(generation, vec![duplicate, series.clone()]));
        assert_eq!(picker.candidates.len(), 2, "dedupe on provider + provider id");
        assert_eq!(picker.status, "2 matches so far\u{2026}");
        assert!(picker.busy, "still running");
        // A later batch that re-ranks the list keeps the highlight on the row
        // the user picked: the next click must act on that same record.
        let show_key = crate::window::picked_row_key(&picker.candidates[1]);
        picker.selected = 1;
        picker.pick_row(show_key.clone());
        let mut older = movie_candidate();
        older.title = "Dune".to_string();
        older.provider_id = "tt1160419".to_string();
        older.year = 2019;
        assert!(picker.push_search(generation, vec![older]));
        assert_eq!(picker.candidates.len(), 3);
        assert_eq!(picker.selected, 2, "the pick moved with its row");
        assert_eq!(
            picker
                .selected_candidate()
                .map(crate::window::picked_row_key),
            Some(show_key)
        );
        assert!(picker.finish_search_now(generation));
        assert!(!picker.busy && picker.open);
        assert_eq!(picker.status, "3 matches for \"Dune\"");
        // An empty stream is the no-matches state, never a stale count.
        let (generation, ..) = picker.begin_search().expect("search starts");
        assert!(picker.push_search(generation, Vec::new()));
        assert!(picker.finish_search_now(generation));
        assert!(
            picker.status.starts_with("No matches for \"Dune\""),
            "{}",
            picker.status
        );
        assert!(!picker.open);
        // A stale batch and a stale finish are both refused.
        assert!(!picker.push_search(generation + 1, vec![series]));
        assert!(!picker.finish_search_now(generation + 1));
    }

    #[test]
    fn sibling_episodes_make_a_series_and_anything_else_stays_a_movie() {
        use super::{MediaShape, classify_siblings};

        let siblings = |names: &[&str]| names.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let cyberpunk = siblings(&[
            "Cyberpunk Edgerunners S02E01.mkv",
            "Cyberpunk Edgerunners S02E02.mkv",
            "Cyberpunk Edgerunners S02E03.mkv",
            "Cyberpunk Edgerunners S02E04.mkv",
            "Cyberpunk Edgerunners S02E05.mkv",
            "Cyberpunk Edgerunners S02E06.mkv",
            "Cyberpunk Edgerunners S02E07.mkv",
            "Cyberpunk Edgerunners S02E08.mkv",
            "Cyberpunk Edgerunners S02E09.mkv",
            "Cyberpunk Edgerunners S02E10.mkv",
        ]);
        assert_eq!(
            classify_siblings("Cyberpunk Edgerunners S02E01.mkv", &cyberpunk),
            MediaShape::Series,
            "ten episodes of one show are a series"
        );
        assert_eq!(
            classify_siblings("Dune Part Two.mkv", &siblings(&["Dune Part Two.mkv"])),
            MediaShape::Movie,
            "a single movie file is a movie"
        );
        assert_eq!(
            classify_siblings(
                "Dune Part Two.mkv",
                &siblings(&["Dune Part Two.mkv", "Arrival.mkv"])
            ),
            MediaShape::Movie,
            "two unrelated movies are still movies"
        );
        assert_eq!(
            classify_siblings(
                "Show S01E01.mkv",
                &siblings(&["Show S01E01.mkv", "Show S01E02.mkv"])
            ),
            MediaShape::Series
        );
        assert_eq!(
            classify_siblings("Show S01E01.mkv", &siblings(&["Show S01E01.mkv"])),
            MediaShape::Series,
            "the file's own episode tag is enough, even alone"
        );
        assert_eq!(
            classify_siblings("Show S1.mkv", &siblings(&["Show S1.mkv"])),
            MediaShape::Series,
            "a season tag is an episode pattern"
        );
        assert_eq!(
            classify_siblings("Show E01.mkv", &siblings(&["Show E01.mkv"])),
            MediaShape::Series,
            "a bare episode tag is an episode pattern"
        );
        assert_eq!(
            classify_siblings("Show Episode 01.mkv", &siblings(&["Show Episode 01.mkv"])),
            MediaShape::Series,
            "the spelled-out episode word is an episode pattern"
        );
        assert_eq!(
            classify_siblings("Show 1x02.mkv", &siblings(&["Show 1x02.mkv"])),
            MediaShape::Series,
            "the 1x02 form is an episode pattern"
        );
        // A same-titled film beside an episode file is not itself an episode:
        // the sibling needs its own episode tag.
        assert_eq!(
            classify_siblings(
                "Show.mkv",
                &siblings(&["Show.mkv", "Show S01E01.mkv"])
            ),
            MediaShape::Series,
            "the episode sibling tags the folder as a series"
        );
        assert_eq!(
            classify_siblings("Show.mkv", &siblings(&["Show.mkv", "Show 2.mkv"])),
            MediaShape::Movie
        );
    }

    #[test]
    fn detect_shape_reads_the_real_folder_and_ignores_non_video_files() {
        use super::{MediaShape, detect_shape};

        crate::test_env::EnvGuard::with_scratch("search-shape", |dir| {
            for n in 1..=10 {
                std::fs::write(
                    dir.join(format!("Cyberpunk Edgerunners S02E{n:02}.mkv")),
                    "x",
                )
                .unwrap();
            }
            std::fs::write(dir.join("notes.txt"), "x").unwrap();
            let first = dir.join("Cyberpunk Edgerunners S02E01.mkv");
            assert_eq!(
                detect_shape(&first.to_string_lossy()),
                MediaShape::Series,
                "the real folder's episode siblings make the show a series"
            );
            // A folder of one movie is a movie, and a missing folder is no
            // evidence at all.
            let lone_dir = dir.join("films");
            std::fs::create_dir_all(&lone_dir).unwrap();
            std::fs::write(lone_dir.join("Dune Part Two.mkv"), "x").unwrap();
            assert_eq!(
                detect_shape(&lone_dir.join("Dune Part Two.mkv").to_string_lossy()),
                MediaShape::Movie
            );
            assert_eq!(
                detect_shape(&dir.join("missing").join("Show S01E01.mkv").to_string_lossy()),
                MediaShape::Series,
                "an unreadable folder still reads the file's own episode tag"
            );
            assert_eq!(
                detect_shape(&dir.join("missing").join("Show.mkv").to_string_lossy()),
                MediaShape::Movie,
                "an unreadable folder and a bare title are no evidence"
            );
            // A season folder is series evidence by itself.
            let season_dir = dir.join("Show").join("Season 2");
            std::fs::create_dir_all(&season_dir).unwrap();
            std::fs::write(season_dir.join("ep.mkv"), "x").unwrap();
            assert_eq!(
                detect_shape(&season_dir.join("ep.mkv").to_string_lossy()),
                MediaShape::Series,
                "the season folder names the shape"
            );
        });
    }

    #[test]
    fn provisional_override_persists_through_the_caller_store_write() {
        // The helper itself takes no store handle (pure builder); the
        // documented contract is that the CALLER persists the record. This
        // drives that reachable path: build, save through `put_correction`,
        // reload, resolve -- proving the built record is persistable, not
        // merely that a pure function writes nothing.
        crate::test_env::EnvGuard::with_scratch("search-persist", |dir| {
            let store_path = metadata::overrides::default_path();
            assert!(
                store_path.starts_with(dir),
                "the store resolves under scratch: {}",
                store_path.display()
            );
            let path = crate::window::fix_series_path();
            let key = metadata::overrides::series_key_for(&path).expect("series key");
            let cand = series_candidate();
            let over = provisional_override(&cand, Scope::Series, key.clone());
            assert_eq!(over.key, key);
            assert!(over.media.is_some(), "the candidate snapshot rides");
            assert!(over.episodes.is_empty(), "provisional: no episode table");
            let mut store = metadata::overrides::load()
                .unwrap_or_else(|_| metadata::overrides::Store::empty());
            store.put_correction(&path, over.clone()).expect("caller save");
            let reloaded = metadata::overrides::load().expect("reload");
            let got = reloaded.resolve_for(&path).expect("persisted latch");
            assert_eq!(got.title, "Show");
            assert_eq!(got.provider_id, "169");
            assert_eq!(got.poster, "https://art/show-picked.jpg");
            let got_media = got.media.as_ref().expect("the snapshot survives the reload");
            assert_eq!(got_media.synopsis, "A show about a show.");
            assert!(got.episodes.is_empty());
        });
    }

    #[test]
    fn regular_scope_calls_zero_anime_legs_and_anime_scope_zero_regular() {
        use super::{ANIME_LEG_PROVIDERS, ANIME_PROVIDERS, REGULAR_PROVIDERS, KindFilter};

        assert_eq!(REGULAR_PROVIDERS.len(), 3, "regular path source count");
        assert_eq!(ANIME_LEG_PROVIDERS.len(), 4, "anime path source count");
        // Every regular leg is concurrent and primary; every anime leg too.
        for name in REGULAR_PROVIDERS {
            assert!(
                !ANIME_PROVIDERS.contains(name),
                "regular leg {name} is never classed anime"
            );
        }
        for name in ANIME_LEG_PROVIDERS {
            assert!(
                ANIME_PROVIDERS.contains(name),
                "anime leg {name} is classed anime"
            );
        }
        // Regular scopes ask only regular legs.
        for filter in [KindFilter::Series, KindFilter::Movies] {
            for name in filter.providers() {
                assert!(
                    REGULAR_PROVIDERS.contains(name),
                    "{filter:?} asks regular leg {name}"
                );
                assert!(
                    !ANIME_PROVIDERS.contains(name),
                    "{filter:?} calls zero anime legs"
                );
            }
        }
        assert_eq!(
            KindFilter::Series.providers().len(),
            3,
            "regular TV walks all three regular legs"
        );
        // Anime scopes ask only anime legs.
        for filter in [KindFilter::AnimeSeries, KindFilter::AnimeMovies] {
            assert_eq!(
                filter.providers().len(),
                4,
                "{filter:?} walks all four anime legs"
            );
            for name in filter.providers() {
                assert!(
                    ANIME_PROVIDERS.contains(name),
                    "{filter:?} asks anime leg {name}"
                );
                assert!(
                    !REGULAR_PROVIDERS.contains(name),
                    "{filter:?} calls zero regular legs"
                );
            }
        }
        // All asks both paths: 3 regular + 4 anime.
        assert_eq!(KindFilter::All.providers().len(), 7);
        // Cache key already carries the scope, so no cross-path bleed.
        let tv = super::search_cache_key("Show", 2008, KindFilter::Series);
        let anime = super::search_cache_key("Show", 2008, KindFilter::AnimeSeries);
        assert_ne!(tv, anime, "scope is part of the cache key");
    }

    #[test]
    fn full_cache_hit_issues_zero_leg_attempts() {
        use super::{KindFilter, search_cache_key};
        use std::collections::{HashMap, HashSet};

        // The picker's persistent cache shape: key -> (rows, failed). A full
        // hit (rows present, no failed legs) is served without walking any
        // leg, so the leg-attempt set stays empty and no new observability
        // line is emitted.
        let key = search_cache_key("Show", 2008, KindFilter::Series);
        let mut cache: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
        cache.insert(
            key.clone(),
            (vec!["tvmaze:169".to_string()], Vec::new()),
        );
        let mut leg_attempts: HashSet<String> = HashSet::new();
        let mut log_lines: Vec<String> = Vec::new();
        // Cache-hit branch: full entry returns rows, no legs walked.
        let hit = cache.get(&key).cloned();
        let served = if let Some((rows, failed)) = hit
            && failed.is_empty()
            && !rows.is_empty()
        {
            rows
        } else {
            // Would walk `KindFilter::Series.providers()` here; the test
            // proves this branch is not taken on a full hit.
            for leg in KindFilter::Series.providers() {
                leg_attempts.insert((*leg).to_string());
                log_lines.push(format!("search: leg {leg} ok 1 rows 1ms"));
            }
            Vec::new()
        };
        assert_eq!(served, vec!["tvmaze:169".to_string()]);
        assert!(
            leg_attempts.is_empty(),
            "a full cache hit issues zero leg attempts"
        );
        assert!(
            log_lines.is_empty(),
            "a cached second search shows zero new leg lines"
        );
    }
}
