//! `metadata::tenrai` -- the Tenrai anime source (the Jikan successor).
//!
//! Tenrai (`https://api.tenrai.org/v1`) is Jikan-v4 compatible: `GET
//! /v1/anime?q=<title>&limit=<n>&order_by=members&sort=desc` resolves the
//! title to a MAL id, then `GET /v1/anime/<id>/episodes?page=<n>` carries the
//! episode table (`mal_id` IS the absolute episode number, with per-episode
//! title, synopsis and duration).
//!
//! Two behaviours that are contract, not detail:
//!
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best returns a definitive MISS rather than a wrong show
//!   (the discipline the deleted iTunes leg lacked). The pick's `year` NEVER
//!   rejects: at equal title strength it prefers the candidate carrying the
//!   query's year, but a missing year, or a year that disagrees with the
//!   filename, is a nice-to-have display field, never a gate -- the essentials
//!   are cover, title, episode and synopsis.
//!
//! The wire numbers are already absolute, so -- unlike TVmaze/Cinemeta -- an
//! `Absolute` query is NEVER renumbered positionally: renumbering a partial
//! page walk would corrupt the table. A short table simply lacks the Number,
//! so the resolver falls through to the next source. Episode pages are walked
//! only until the requested Number is found (or the pages run out), capped at
//! [`TENRAI_MAX_EPISODE_PAGES`] so one lookup stays a handful of requests.

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::types::{Episode, Kind, Media, Query, join_genres, normalize_url, runtime_length};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped, drain_and_close};
use ureq::http::StatusCode;

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::serdeutil::null_default;
use crate::urlutil::query_escape;

/// The provider's default host (the `/v1` root is part of the paths below so
/// a test server takes the bare host).
pub const TENRAI_DEFAULT_BASE: &str = "https://api.tenrai.org";

/// How many search candidates one lookup ranks.
const TENRAI_SEARCH_LIMIT: u32 = 10;

/// Upper bound on the episode pages one lookup walks. Page 1 holds the first
/// ~100 episodes, so the common anime query costs search + 1; a 500-episode
/// table still resolves inside the resolver's total budget.
const TENRAI_MAX_EPISODE_PAGES: u32 = 5;

/// `tenraiSource`.
pub struct TenraiSource {
    base: String,
    client: HttpClient,
}

impl TenraiSource {
    /// A source pointed at `base` (`""` means [`TENRAI_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> TenraiSource {
        TenraiSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            TENRAI_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for TenraiSource {
    fn default() -> TenraiSource {
        TenraiSource::new("", HttpClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiImageSet {
    #[serde(default, deserialize_with = "null_default")]
    image_url: String,
    #[serde(default, deserialize_with = "null_default")]
    large_image_url: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiImages {
    #[serde(default)]
    jpg: Option<TenraiImageSet>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiGenre {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiEntry {
    #[serde(default, deserialize_with = "null_default")]
    mal_id: i32,
    #[serde(default, deserialize_with = "null_default")]
    url: String,
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default, deserialize_with = "null_default")]
    title_english: String,
    /// `TV`, `Movie`, `OVA`, `ONA`, `Special`, `Music`: the entry's own form.
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    media_type: String,
    #[serde(default)]
    year: Option<i32>,
    #[serde(default, deserialize_with = "null_default")]
    synopsis: String,
    #[serde(default, deserialize_with = "null_default")]
    duration: String,
    #[serde(default)]
    images: Option<TenraiImages>,
    #[serde(default, deserialize_with = "null_default")]
    genres: Vec<TenraiGenre>,
    #[serde(default)]
    members: Option<i32>,
    #[serde(default)]
    episodes: Option<i32>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiSearchResp {
    #[serde(default, deserialize_with = "null_default")]
    data: Vec<TenraiEntry>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiEpisode {
    /// The absolute episode number.
    #[serde(default, deserialize_with = "null_default")]
    mal_id: i32,
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default, deserialize_with = "null_default")]
    synopsis: String,
    #[serde(default, deserialize_with = "null_default")]
    url: String,
    /// Per-episode seconds (`1427` ~= `23 min`).
    #[serde(default)]
    duration: Option<i32>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiPagination {
    #[serde(default)]
    has_next_page: bool,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct TenraiEpisodesResp {
    #[serde(default)]
    pagination: Option<TenraiPagination>,
    #[serde(default, deserialize_with = "null_default")]
    data: Vec<TenraiEpisode>,
}

/// `tenraiArt` prefers the large still, falling back to the default.
fn tenrai_art(images: Option<&TenraiImages>) -> String {
    let Some(jpg) = images.and_then(|images| images.jpg.as_ref()) else {
        return String::new();
    };
    if !jpg.large_image_url.trim().is_empty() {
        return jpg.large_image_url.trim().to_string();
    }
    jpg.image_url.trim().to_string()
}

/// `tenraiMinutes` parses the show-level `"23 min per ep"` (leading digits);
/// 0 when unparseable.
fn tenrai_minutes(s: &str) -> i32 {
    let digits: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}

fn build_episode(e: &TenraiEpisode) -> Episode {
    let length = e.duration.unwrap_or(0) / 60;
    Episode {
        season: 0,
        number: e.mal_id,
        title: e.title.trim().to_string(),
        artwork: String::new(),
        synopsis: cap_synopsis(&strip_html(&e.synopsis)),
        url: normalize_url(&e.url),
        length: runtime_length(length),
    }
}

impl TenraiSource {
    /// The `/v1/anime?q=` rows for `title`, ONE request. Rows without an id or
    /// a title are dropped; the provider's own member order is kept.
    fn search_rows(&self, cancel: &Cancel, title: &str) -> Result<Vec<TenraiEntry>, Error> {
        if title.is_empty() {
            return Ok(Vec::new());
        }
        let search_url = format!(
            "{}/v1/anime?q={}&limit={TENRAI_SEARCH_LIMIT}&order_by=members&sort=desc",
            self.base_url(),
            query_escape(title),
        );
        let response = self.client.get(cancel, &search_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(Vec::new());
        }
        let parsed: TenraiSearchResp = decode_capped(response)?;
        Ok(parsed
            .data
            .into_iter()
            .filter(|entry| entry.mal_id != 0 && entry_name(entry).is_some())
            .collect())
    }

    /// One validated search pick: the title-ranked best, with a known query
    /// year preferring any challenger that both carries it and fully covers
    /// the query title. `Ok(None)` is a definitive miss. Year never gates:
    /// it may only ride along as a display field.
    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<TenraiEntry>, Error> {
        let rows = self.search_rows(cancel, &q.title)?;
        let mut cands: Vec<ShowCand> = Vec::with_capacity(rows.len());
        let mut years: Vec<i32> = Vec::with_capacity(rows.len());
        for entry in &rows {
            let Some(name) = entry_name(entry) else {
                continue;
            };
            // Both spellings score: a romaji query must find the
            // English-titled main series, not a romaji-named special.
            let mut aliases = Vec::new();
            for title in [&entry.title_english, &entry.title] {
                let title = title.trim();
                if !title.is_empty() {
                    aliases.push(title.to_string());
                }
            }
            cands.push(ShowCand {
                name,
                weight: entry.members.unwrap_or(0),
                episode_count: entry.episodes.unwrap_or(0),
                aliases,
            });
            years.push(entry.year.unwrap_or(0));
        }
        if cands.is_empty() {
            return Ok(None);
        }
        let Some(winner) = pick_best_show(&q.title, &cands) else {
            // Low-confidence best: a wrong title is worse than no title.
            return Ok(None);
        };
        let mut winner_idx = None;
        for (i, cand) in cands.iter().enumerate() {
            if *cand == winner {
                winner_idx = Some(i);
                break;
            }
        }
        let Some(winner_idx) = winner_idx else {
            return Ok(None);
        };
        // Soft year preference only: no year match keeps the title winner.
        let take =
            crate::yearpref::prefer_year_match(&q.title, q.year, &cands, &years, winner_idx);
        Ok(rows.get(take).cloned())
    }

    /// The episode table for a validated pick, walked only until the requested
    /// Number is found (or the pages run out), capped at
    /// [`TENRAI_MAX_EPISODE_PAGES`].
    fn episodes(&self, cancel: &Cancel, picked: &TenraiEntry, want: i32) -> Result<Vec<Episode>, Error> {
        let mut out: Vec<Episode> = Vec::new();
        // A query without an episode number can never number-exact match, so
        // one page is enough table to return.
        let mut page: u32 = 1;
        loop {
            let url = format!(
                "{}/v1/anime/{}/episodes?page={page}",
                self.base_url(),
                picked.mal_id,
            );
            let response = self.client.get(cancel, &url)?;
            if response.status() == StatusCode::NOT_FOUND {
                drain_and_close(response);
                return Ok(out);
            }
            let parsed: TenraiEpisodesResp = decode_capped(response)?;
            for e in &parsed.data {
                if e.mal_id <= 0 {
                    continue;
                }
                out.push(build_episode(e));
            }
            // The page is already fetched, so the whole page is kept (a later
            // query for another episode of the same show is then served from
            // the cache); paging stops once the requested Number is in hand.
            if want > 0 && out.iter().any(|episode| episode.number == want) {
                return Ok(out);
            }
            let more = parsed.pagination.is_some_and(|p| p.has_next_page);
            if !more || page >= TENRAI_MAX_EPISODE_PAGES {
                return Ok(out);
            }
            page += 1;
        }
    }
}

impl crate::resolver::Source for TenraiSource {
    fn name(&self) -> &str {
        "tenrai"
    }

    fn path(&self) -> crate::resolver::SearchPath {
        crate::resolver::SearchPath::Anime
    }

    /// `Find`: series only (movies never reach the anime legs through the
    /// resolver, and a series-only source self-gates a ruled-out kind before
    /// touching the wire).
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::MOVIE || q.title.is_empty() {
            return Ok(None);
        }
        let Some(picked) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        let Some(title) = entry_name(&picked) else {
            return Ok(None);
        };
        let episodes = self.episodes(cancel, &picked, q.episode)?;
        // The wire numbers are already absolute: no positional renumbering, so
        // a short table simply lacks the Number and the chain falls through.
        let mut media = tenrai_media(&picked, title, Kind::SERIES);
        media.episodes = episodes;
        Ok(Some(media))
    }

    /// The manual search's rows: the same ONE `/v1/anime?q=` request, each
    /// entry whose title clears the matcher's floor as its own pickable row,
    /// with an EMPTY episode table (no episode page is fetched). The entry's
    /// own `type` decides the kind: `Movie` is a movie, everything else a
    /// series.
    fn find_all(&self, cancel: &Cancel, q: &Query) -> Result<Vec<Media>, Error> {
        if q.title.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Media> = Vec::new();
        for entry in self.search_rows(cancel, &q.title)? {
            let Some(name) = entry_name(&entry) else {
                continue;
            };
            let mut aliases = Vec::new();
            for title in [&entry.title_english, &entry.title] {
                let title = title.trim();
                if !title.is_empty() {
                    aliases.push(title.to_string());
                }
            }
            let cand = ShowCand {
                name: name.clone(),
                weight: entry.members.unwrap_or(0),
                episode_count: entry.episodes.unwrap_or(0),
                aliases,
            };
            // The same floor `Find` applies: a fuzzy row never becomes a pick.
            if pick_best_show(&q.title, std::slice::from_ref(&cand)).is_none() {
                continue;
            }
            let kind = if entry.media_type.trim().eq_ignore_ascii_case("movie") {
                Kind::MOVIE
            } else {
                Kind::SERIES
            };
            out.push(tenrai_media(&entry, name, kind));
            if out.len() == crate::resolver::FIND_ALL_LIMIT {
                break;
            }
        }
        Ok(out)
    }
}

/// The entry's display name: its English title, else its romaji one; `None`
/// when it carries neither.
fn entry_name(entry: &TenraiEntry) -> Option<String> {
    let english = entry.title_english.trim();
    if !english.is_empty() {
        return Some(english.to_string());
    }
    let title = entry.title.trim();
    (!title.is_empty()).then(|| title.to_string())
}

/// The candidate-ready [`Media`] for one search row: identity, year, cover,
/// genres and page URL, with the kind the caller derives from `type`. The
/// episode table is the caller's to fill.
fn tenrai_media(entry: &TenraiEntry, title: String, kind: Kind) -> Media {
    let genres: Vec<String> = entry.genres.iter().map(|g| g.name.clone()).collect();
    Media {
        source: "tenrai".to_string(),
        source_id: entry.mal_id.to_string(),
        kind,
        title,
        year: entry.year.unwrap_or(0),
        artwork: tenrai_art(entry.images.as_ref()),
        synopsis: cap_synopsis(&strip_html(&entry.synopsis)),
        genres: join_genres(&genres),
        length: runtime_length(tenrai_minutes(&entry.duration)),
        show_url: normalize_url(&entry.url),
        episode_url: String::new(),
        movie_url: String::new(),
        trailer_url: String::new(),
        episodes: Vec::new(),
        fetched_at: crate::timefmt::now_rfc3339(),
    }
}
