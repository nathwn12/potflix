//! `metadata::cinemeta` -- Stremio's default catalog addon as a keyless movie and
//! series source (`com.linvo.cinemeta`, base [`CINEMETA_DEFAULT_BASE`]).
//!
//! Wire shape: `/catalog/<movie|series>/top/search=<title>.json` resolves the
//! title to an IMDb id, then `/meta/<movie|series>/<id>.json` carries the
//! details (year, `runtime` like `"148 min"`, `genre[]`, `description`,
//! `poster`) plus the series episode list (`videos[]`) in the same call.
//!
//! Two behaviours that are contract, not detail:
//!
//! * the catalog pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best returns a definitive MISS rather than a wrong movie
//!   (the iTunes leg it replaces returned the first `feature-movie` row with
//!   no title check at all);
//! * year NEVER rejects: a known query year only PREFERS, at equal title
//!   strength, the candidate carrying it (any challenger fully covering the
//!   query title, not just a duplicate exact one) and otherwise rides along
//!   as a display field. A missing year, or a year that disagrees with the
//!   filename, never skips a provider holding the essentials.
//!
//! An `Absolute` series query renumbers the episode table positionally in
//! `(season, number)` air order, exactly as TVmaze does, so the resolver's
//! number-exact match holds.

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::types::{
    Episode, Kind, Media, Query, join_genres, normalize_url, runtime_length,
};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped, drain_and_close};
use ureq::http::StatusCode;

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::serdeutil::null_default;
use crate::urlutil::path_escape;

/// The provider's default host (Stremio Cinemeta v3).
pub const CINEMETA_DEFAULT_BASE: &str = "https://v3-cinemeta.strem.io";

/// `cinemetaSource`.
pub struct CinemetaSource {
    base: String,
    client: HttpClient,
}

impl CinemetaSource {
    /// A source pointed at `base` (`""` means [`CINEMETA_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> CinemetaSource {
        CinemetaSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            CINEMETA_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for CinemetaSource {
    fn default() -> CinemetaSource {
        CinemetaSource::new("", HttpClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CinemetaMeta {
    #[serde(default, deserialize_with = "null_default")]
    id: String,
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(rename = "releaseInfo", default, deserialize_with = "null_default")]
    release_info: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CinemetaCatalogResp {
    #[serde(default, deserialize_with = "null_default")]
    metas: Vec<CinemetaMeta>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CinemetaVideo {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(default)]
    season: Option<i32>,
    #[serde(default)]
    number: Option<i32>,
    #[serde(default, deserialize_with = "null_default")]
    overview: String,
    #[serde(default, deserialize_with = "null_default")]
    thumbnail: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CinemetaDetails {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    year: String,
    #[serde(default, deserialize_with = "null_default")]
    runtime: String,
    #[serde(default, deserialize_with = "null_default")]
    genre: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    description: String,
    #[serde(default, deserialize_with = "null_default")]
    poster: String,
    #[serde(default, deserialize_with = "null_default")]
    videos: Vec<CinemetaVideo>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct CinemetaMetaResp {
    #[serde(default)]
    meta: Option<CinemetaDetails>,
}

/// `releaseYear`: the leading year of a catalog `releaseInfo` (`"2010"` or
/// `"2008-2013"`) or a meta `year` (`"2010"`); 0 when unknown.
fn release_year(s: &str) -> i32 {
    let digits: String = s.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 4 {
        digits[..4].parse().unwrap_or(0)
    } else {
        0
    }
}

/// `releaseRangeContains`: whether a catalog `releaseInfo` range covers
/// `year`: `"2013-2023"` covers 2013..=2023, a lone `"2013"` covers only
/// 2013, an open end (`"2013-"`, `"2013-?"`) covers 2013 and later. False for
/// `year <= 0` and for a range with no parseable start.
fn release_range_contains(release_info: &str, year: i32) -> bool {
    if year <= 0 {
        return false;
    }
    let info = release_info.trim();
    let (start_text, end_text) = match info.split_once('-') {
        Some((start, end)) => (start.trim(), Some(end.trim())),
        None => (info, None),
    };
    let start_digits: String = start_text
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if start_digits.len() < 4 {
        return false;
    }
    let start: i32 = start_digits[..4].parse().unwrap_or(0);
    if start == 0 || year < start {
        return false;
    }
    match end_text {
        Some(end) => {
            let end_digits: String =
                end.chars().take_while(|c| c.is_ascii_digit()).collect();
            // Open end: no upper bound.
            if end_digits.len() < 4 {
                return true;
            }
            let end_year: i32 = end_digits[..4].parse().unwrap_or(0);
            end_year == 0 || year <= end_year
        }
        // A lone year is a range of one.
        None => year == start,
    }
}

/// `runtimeMinutes` parses Cinemeta's `"148 min"` (with an `"Xh Ym"` fallback);
/// 0 when unparseable.
fn runtime_minutes(s: &str) -> i32 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    if let Some(mins) = s.strip_suffix("min").and_then(|n| n.trim().parse().ok()) {
        return mins;
    }
    let mut total = 0;
    let mut current = String::new();
    let mut seen = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            current.push(c);
        } else if (c == 'h' || c == 'm') && !current.is_empty() {
            let n: i32 = current.parse().unwrap_or(0);
            current.clear();
            seen = true;
            if c == 'h' {
                total += n * 60;
            } else {
                total += n;
            }
        } else if !c.is_whitespace() {
            current.clear();
        }
    }
    if seen { total } else { 0 }
}

/// The IMDb page for `id`, for the card's View button.
fn imdb_url(id: &str) -> String {
    if id.trim().is_empty() {
        return String::new();
    }
    normalize_url(&format!("https://www.imdb.com/title/{}/", id.trim()))
}

fn build_episode(video: &CinemetaVideo, season: i32, number: i32) -> Episode {
    Episode {
        season,
        number,
        title: video.name.trim().to_string(),
        artwork: normalize_url(&video.thumbnail),
        synopsis: cap_synopsis(&strip_html(&video.overview)),
        url: String::new(),
        length: String::new(),
    }
}

impl CinemetaSource {
    /// The catalog search rows for `catalog` (`"movie"` or `"series"`), in the
    /// provider's own rank order. Rows without an id or a name are dropped.
    fn catalog(
        &self,
        cancel: &Cancel,
        q: &Query,
        catalog: &str,
    ) -> Result<Vec<CinemetaMeta>, Error> {
        let search_url = format!(
            "{}/catalog/{catalog}/top/search={}.json",
            self.base_url(),
            path_escape(&q.title)
        );
        let response = self.client.get(cancel, &search_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(Vec::new());
        }
        let catalog_resp: CinemetaCatalogResp = decode_capped(response)?;
        let mut entries: Vec<CinemetaMeta> = Vec::with_capacity(catalog_resp.metas.len());
        for meta in catalog_resp.metas {
            if meta.id.trim().is_empty() || meta.name.trim().is_empty() {
                continue;
            }
            entries.push(meta);
        }
        Ok(entries)
    }

    /// `pickIndex`: the title-ranked best row, with a known query year
    /// preferring any challenger that both carries it and fully covers the
    /// query title. `None` when the best similarity is below the matcher's
    /// floor. Year never gates beyond that soft preference.
    fn pick_index(q: &Query, entries: &[CinemetaMeta]) -> Option<usize> {
        let cands: Vec<ShowCand> = entries
            .iter()
            .map(|meta| ShowCand {
                name: meta.name.clone(),
                weight: 0,
                episode_count: 0,
                aliases: Vec::new(),
            })
            .collect();
        let winner = pick_best_show(&q.title, &cands)?;
        let winner_idx = cands.iter().position(|cand| *cand == winner)?;
        let years: Vec<i32> = entries
            .iter()
            .map(|entry| release_year(&entry.release_info))
            .collect();
        Some(crate::yearpref::prefer_year_match(
            &q.title, q.year, &cands, &years, winner_idx,
        ))
    }

    /// The `/meta` details for a validated catalog pick.
    fn details(
        &self,
        cancel: &Cancel,
        kind: &str,
        picked: &CinemetaMeta,
    ) -> Result<Option<CinemetaDetails>, Error> {
        let meta_url = format!(
            "{}/meta/{kind}/{}.json",
            self.base_url(),
            path_escape(picked.id.trim())
        );
        let response = self.client.get(cancel, &meta_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(None);
        }
        let parsed: CinemetaMetaResp = decode_capped(response)?;
        Ok(parsed.meta)
    }

    fn find_movie(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        let entries = self.catalog(cancel, q, "movie")?;
        // A title below the floor is still the right work when the search
        // engine's own top row carries the query year (parse_raw salvages that
        // year out of junk-laden scene names): two independent keys -- the
        // engine's rank and the year -- so an unrelated first row still loses.
        let Some(idx) = Self::pick_index(q, &entries).or_else(|| year_seed_index(q, &entries))
        else {
            return Ok(None);
        };
        let picked = &entries[idx];
        let Some(details) = self.details(cancel, "movie", picked)? else {
            return Ok(None);
        };
        let title = details.name.trim();
        if title.is_empty() {
            return Ok(None);
        }
        let mut year = release_year(&details.year);
        if year == 0 {
            year = release_year(&picked.release_info);
        }
        Ok(Some(Media {
            source: "cinemeta".to_string(),
            source_id: picked.id.trim().to_string(),
            kind: Kind::MOVIE,
            title: title.to_string(),
            year,
            artwork: normalize_url(&details.poster),
            synopsis: cap_synopsis(&strip_html(&details.description)),
            genres: join_genres(&details.genre),
            length: runtime_length(runtime_minutes(&details.runtime)),
            show_url: String::new(),
            episode_url: String::new(),
            movie_url: imdb_url(picked.id.trim()),
            trailer_url: String::new(),
            episodes: Vec::new(),
            fetched_at: crate::timefmt::now_rfc3339(),
        }))
    }

    /// Candidate order: the title-ranked row first (when one clears the
    /// floor), then the catalog's own top row -- but only when the query year
    /// falls inside the top row's release range (open-ended handled), or when
    /// the query carries no year at all. The top row is where a romaji or
    /// junk-laden query lands on the work's other-named entry (token scoring
    /// cannot see `Shingeki no Kyojin` in `Attack on Titan`); premiere-year
    /// equality is NOT required, so a later-season query (2018 on a 2013
    /// show) still takes the fallback. A year-less query takes it too, but
    /// the row must then prove itself by its own episode table (the
    /// `match_episode` check below), so a genuinely different show still
    /// loses. A query with no episode number never takes the top-row path.
    fn find_series(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        let entries = self.catalog(cancel, q, "series")?;
        let mut order: Vec<usize> = Vec::new();
        if let Some(i) = Self::pick_index(q, &entries) {
            order.push(i);
        }
        if q.episode > 0
            && order.first() != Some(&0)
            && let Some(top) = entries.first()
        {
            let ranged = q.year > 0 && release_range_contains(&top.release_info, q.year);
            if ranged || q.year == 0 {
                order.push(0);
            }
        }
        for i in order {
            let pinned = &entries[i];
            let Some(details) = self.details(cancel, "series", pinned)? else {
                continue;
            };
            let Some(media) = series_media(q, pinned, details) else {
                continue;
            };
            if q.episode > 0 && crate::resolver::match_episode(&media.episodes, q).is_none() {
                continue;
            }
            return Ok(Some(media));
        }
        Ok(None)
    }
}

/// A low-confidence seed for the movie leg: when the title floor rejects every
/// catalog row but the query carries a known year and the catalog's OWN top
/// row carries that same year, the top row is the work the search engine found
/// under a title token scoring cannot see (a junk-laden scene name whose year
/// `parse_raw` salvaged). Two independent keys -- the engine's rank and the
/// year -- so an unrelated first row still loses, and a query without a year
/// never seeds.
fn year_seed_index(q: &Query, entries: &[CinemetaMeta]) -> Option<usize> {
    if q.year <= 0 {
        return None;
    }
    let first = entries.first()?;
    (release_year(&first.release_info) == q.year).then_some(0)
}

/// The series `Media` for one catalog row plus its details. `None` when the
/// details carry no usable name (the caller then tries the next candidate).
fn series_media(q: &Query, picked: &CinemetaMeta, details: CinemetaDetails) -> Option<Media> {
    let title = details.name.trim();
    if title.is_empty() {
        return None;
    }
    let mut year = release_year(&details.year);
    if year == 0 {
        year = release_year(&picked.release_info);
    }
    let mut episodes: Vec<Episode> = Vec::with_capacity(details.videos.len());
    for video in &details.videos {
        let Some(number) = video.number else {
            continue;
        };
        episodes.push(build_episode(video, video.season.unwrap_or(0), number));
    }
    episodes.sort_by_key(|episode| (episode.season, episode.number));
    if q.absolute {
        // Absolute N maps to the Nth entry in (season, number) air order,
        // exactly as the TVmaze source renumbers, so number-exact matching
        // holds for absolute anime queries.
        for (i, episode) in episodes.iter_mut().enumerate() {
            episode.number = i as i32 + 1;
        }
    }
    Some(Media {
        source: "cinemeta".to_string(),
        source_id: picked.id.trim().to_string(),
        kind: Kind::SERIES,
        title: title.to_string(),
        year,
        artwork: normalize_url(&details.poster),
        synopsis: cap_synopsis(&strip_html(&details.description)),
        genres: join_genres(&details.genre),
        length: String::new(),
        show_url: imdb_url(picked.id.trim()),
        episode_url: String::new(),
        movie_url: String::new(),
        trailer_url: String::new(),
        episodes,
        fetched_at: crate::timefmt::now_rfc3339(),
    })
}

impl crate::resolver::Source for CinemetaSource {
    fn name(&self) -> &str {
        "cinemeta"
    }

    /// `Find`: movies via `/catalog/movie`, series via `/catalog/series`. An
    /// `UNKNOWN` query tries series first, then movie (a series answer can
    /// only match when the query carries an episode number).
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.title.is_empty() {
            return Ok(None);
        }
        if q.kind == Kind::MOVIE {
            return self.find_movie(cancel, q);
        }
        if q.kind == Kind::SERIES {
            return self.find_series(cancel, q);
        }
        if let Some(media) = self.find_series(cancel, q)? {
            return Ok(Some(media));
        }
        self.find_movie(cancel, q)
    }
}
