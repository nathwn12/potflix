//! `metadata::episodate` -- the Episodate TV source (unofficial community DB).
//!
//! Episodate (`https://www.episodate.com`) is keyless JSON: `GET
//! /api/search?q=<title>` resolves the title to a permalink, then `GET
//! /api/show-details?q=<permalink>` carries the show record (name, seasons,
//! `description`, `image_path`) plus the full episode table (`season`,
//! `episode`, `name`) in the same call.
//!
//! Two behaviours that are contract, not detail:
//!
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best returns a definitive MISS rather than a wrong show.
//!   The pick's `start_date` year NEVER rejects: at equal title strength it
//!   prefers the candidate carrying the query's year, but a missing year, or
//!   a year that disagrees with the filename, is a nice-to-have display
//!   field, never a gate -- the essentials are cover, title, episode and
//!   synopsis;
//! * movies never reach the wire: Episodate holds TV only, so a movie key
//!   returns `Ok(None)` before any fetch.

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::parse::date_year;
use metadata_core::types::{Episode, Kind, Media, Query, normalize_url};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped, drain_and_close};
use ureq::http::StatusCode;

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::serdeutil::null_default;
use crate::urlutil::query_escape;

/// The provider's default host.
pub const EPISODATE_DEFAULT_BASE: &str = "https://www.episodate.com";

/// `episodateSource`.
pub struct EpisodateSource {
    base: String,
    client: HttpClient,
}

impl EpisodateSource {
    /// A source pointed at `base` (`""` means [`EPISODATE_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> EpisodateSource {
        EpisodateSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            EPISODATE_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for EpisodateSource {
    fn default() -> EpisodateSource {
        EpisodateSource::new("", HttpClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct EpisodateHit {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    permalink: String,
    #[serde(rename = "start_date", default, deserialize_with = "null_default")]
    start_date: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct EpisodateSearchResp {
    #[serde(rename = "tv_shows", default, deserialize_with = "null_default")]
    tv_shows: Vec<EpisodateHit>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct EpisodateEpisode {
    #[serde(default, deserialize_with = "null_default")]
    season: i32,
    #[serde(default, deserialize_with = "null_default")]
    episode: i32,
    #[serde(default, deserialize_with = "null_default")]
    name: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct EpisodateShow {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(default, deserialize_with = "null_default")]
    url: String,
    #[serde(default, deserialize_with = "null_default")]
    description: String,
    #[serde(rename = "start_date", default, deserialize_with = "null_default")]
    start_date: String,
    #[serde(rename = "image_path", default, deserialize_with = "null_default")]
    image_path: String,
    #[serde(
        rename = "image_thumbnail_path",
        default,
        deserialize_with = "null_default"
    )]
    image_thumbnail_path: String,
    #[serde(default, deserialize_with = "null_default")]
    episodes: Vec<EpisodateEpisode>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct EpisodateDetailsResp {
    #[serde(rename = "tvShow", default)]
    tv_show: Option<EpisodateShow>,
}

impl EpisodateSource {
    /// One validated search pick: the title-ranked best, with a known query
    /// year preferring any challenger that both carries it and fully covers
    /// the query title. `Ok(None)` is a definitive miss. Year never gates.
    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<EpisodateHit>, Error> {
        let search_url = format!("{}/api/search?q={}", self.base_url(), query_escape(&q.title));
        let response = self.client.get(cancel, &search_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(None);
        }
        let parsed: EpisodateSearchResp = decode_capped(response)?;
        let mut cands: Vec<ShowCand> = Vec::with_capacity(parsed.tv_shows.len());
        let mut entries: Vec<EpisodateHit> = Vec::with_capacity(parsed.tv_shows.len());
        let mut years: Vec<i32> = Vec::with_capacity(parsed.tv_shows.len());
        for hit in parsed.tv_shows {
            if hit.permalink.trim().is_empty() || hit.name.trim().is_empty() {
                continue;
            }
            cands.push(ShowCand {
                name: hit.name.clone(),
                weight: 0,
                episode_count: 0,
                aliases: Vec::new(),
            });
            years.push(date_year(&hit.start_date));
            entries.push(hit);
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
        Ok(Some(entries[take].clone()))
    }

    /// The show record (with the episode table) for a validated pick.
    fn details(
        &self,
        cancel: &Cancel,
        picked: &EpisodateHit,
    ) -> Result<Option<EpisodateShow>, Error> {
        let details_url = format!(
            "{}/api/show-details?q={}",
            self.base_url(),
            query_escape(picked.permalink.trim()),
        );
        let response = self.client.get(cancel, &details_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(None);
        }
        let parsed: EpisodateDetailsResp = decode_capped(response)?;
        Ok(parsed.tv_show)
    }
}

impl crate::resolver::Source for EpisodateSource {
    fn name(&self) -> &str {
        "episodate"
    }

    /// `Find`: series only (movies never reach the TV-only Episodate legs
    /// through the resolver, and a series-only source self-gates a ruled-out
    /// kind before touching the wire).
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::MOVIE || q.title.is_empty() {
            return Ok(None);
        }
        let Some(picked) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        let Some(show) = self.details(cancel, &picked)? else {
            return Ok(None);
        };
        let mut title = show.name.trim().to_string();
        if title.is_empty() {
            title = picked.name.trim().to_string();
        }
        if title.is_empty() {
            return Ok(None);
        }
        let mut episodes: Vec<Episode> = Vec::with_capacity(show.episodes.len());
        for entry in &show.episodes {
            if entry.episode <= 0 {
                continue;
            }
            episodes.push(Episode {
                season: entry.season,
                number: entry.episode,
                title: entry.name.trim().to_string(),
                artwork: String::new(),
                synopsis: String::new(),
                url: String::new(),
                length: String::new(),
            });
        }
        episodes.sort_by_key(|episode| (episode.season, episode.number));
        let artwork = if !show.image_path.trim().is_empty() {
            normalize_url(&show.image_path)
        } else {
            normalize_url(&show.image_thumbnail_path)
        };
        Ok(Some(Media {
            source: "episodate".to_string(),
            source_id: picked.permalink.trim().to_string(),
            kind: Kind::SERIES,
            title,
            year: date_year(&show.start_date),
            artwork,
            synopsis: cap_synopsis(&strip_html(&show.description)),
            genres: String::new(),
            length: String::new(),
            show_url: normalize_url(&show.url),
            episode_url: String::new(),
            movie_url: String::new(),
            trailer_url: String::new(),
            episodes,
            episode_count: 0,
            fetched_at: crate::timefmt::now_rfc3339(),
        }))
    }
}
