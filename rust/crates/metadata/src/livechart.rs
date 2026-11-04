//! `metadata::livechart` -- the LiveChart.me anime source (keyless JSON).
//!
//! LiveChart.me (`https://www.livechart.me`) needs no key for anime search:
//! `GET /api/v1/anime?q=<title>` carries the synopsis, alternative titles,
//! episode count, premiere date and poster in a single call.
//!
//! Three behaviours that are contract, not detail:
//!
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best returns a definitive MISS rather than a wrong show.
//!   The pick's `premiere_date` year NEVER rejects: at equal title strength
//!   it prefers the candidate carrying the query's year, but a missing year,
//!   or a year that disagrees with the filename, is a nice-to-have display
//!   field, never a gate;
//! * LiveChart.me carries an episode COUNT but no episode table, so the
//!   answer synthesizes numbered absolute stubs ([`episode_stubs`]) --
//!   numbers only, never fabricated titles. The alternative titles ride along
//!   only as extra pick candidates when the display title is empty;
//! * movies never reach the wire: this is an anime-only table, so a movie key
//!   returns `Ok(None)` before any fetch.

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::parse::date_year;
use metadata_core::types::{Kind, Media, Query, normalize_url};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped, drain_and_close};
use ureq::http::StatusCode;

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::justwatch::episode_stubs;
use crate::serdeutil::null_default;
use crate::urlutil::query_escape;

/// The provider's default host.
pub const LIVECHART_DEFAULT_BASE: &str = "https://www.livechart.me";

/// `livechartSource`.
pub struct LivechartSource {
    base: String,
    client: HttpClient,
}

impl LivechartSource {
    /// A source pointed at `base` (`""` means [`LIVECHART_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> LivechartSource {
        LivechartSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            LIVECHART_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for LivechartSource {
    fn default() -> LivechartSource {
        LivechartSource::new("", HttpClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct LivechartPoster {
    #[serde(default, deserialize_with = "null_default")]
    url: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct LivechartItem {
    #[serde(default, deserialize_with = "null_default")]
    id: i64,
    #[serde(rename = "romaji_title", default, deserialize_with = "null_default")]
    romaji_title: String,
    #[serde(rename = "english_title", default, deserialize_with = "null_default")]
    english_title: String,
    #[serde(rename = "native_title", default, deserialize_with = "null_default")]
    native_title: String,
    #[serde(rename = "alternate_titles", default, deserialize_with = "null_default")]
    alternate_titles: Vec<String>,
    #[serde(default, deserialize_with = "null_default")]
    synopsis: String,
    #[serde(rename = "premiere_date", default, deserialize_with = "null_default")]
    premiere_date: String,
    #[serde(rename = "episode_count", default)]
    episode_count: Option<i32>,
    #[serde(default)]
    poster: Option<LivechartPoster>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct LivechartResp {
    #[serde(default, deserialize_with = "null_default")]
    items: Vec<LivechartItem>,
}

/// The display title: English, else romaji, else native.
fn livechart_title(item: &LivechartItem) -> String {
    if !item.english_title.trim().is_empty() {
        return item.english_title.trim().to_string();
    }
    if !item.romaji_title.trim().is_empty() {
        return item.romaji_title.trim().to_string();
    }
    item.native_title.trim().to_string()
}

impl LivechartSource {
    /// One validated search pick: the title-ranked best, with a known query
    /// year preferring any challenger that both carries it and fully covers
    /// the query title. `Ok(None)` is a definitive miss. Year never gates.
    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<LivechartItem>, Error> {
        let search_url = format!("{}/api/v1/anime?q={}", self.base_url(), query_escape(&q.title));
        let response = self.client.get(cancel, &search_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(None);
        }
        let parsed: LivechartResp = decode_capped(response)?;
        let mut cands: Vec<ShowCand> = Vec::with_capacity(parsed.items.len());
        let mut entries: Vec<LivechartItem> = Vec::with_capacity(parsed.items.len());
        let mut years: Vec<i32> = Vec::with_capacity(parsed.items.len());
        for item in parsed.items {
            if item.id == 0 {
                continue;
            }
            let mut name = livechart_title(&item);
            if name.is_empty() {
                name = item
                    .alternate_titles
                    .iter()
                    .find(|alt| !alt.trim().is_empty())
                    .cloned()
                    .unwrap_or_default();
            }
            if name.is_empty() {
                continue;
            }
            cands.push(ShowCand {
                name,
                weight: 0,
                episode_count: item.episode_count.unwrap_or(0),
                aliases: Vec::new(),
            });
            years.push(date_year(&item.premiere_date));
            entries.push(item);
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
}

impl crate::resolver::Source for LivechartSource {
    fn name(&self) -> &str {
        "livechart"
    }

    /// `Find`: series only (movies never reach the anime-only LiveChart.me leg
    /// through the resolver, and a series-only source self-gates a ruled-out
    /// kind before touching the wire).
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::MOVIE || q.title.is_empty() {
            return Ok(None);
        }
        let Some(item) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        let title = livechart_title(&item);
        if title.is_empty() {
            return Ok(None);
        }
        // Numbered absolute stubs only: the API carries a count but no table.
        let episodes = episode_stubs(item.episode_count.unwrap_or(0), 0);
        let artwork = item
            .poster
            .as_ref()
            .map(|poster| normalize_url(&poster.url))
            .unwrap_or_default();
        Ok(Some(Media {
            source: "livechart".to_string(),
            source_id: item.id.to_string(),
            kind: Kind::SERIES,
            title,
            year: date_year(&item.premiere_date),
            artwork,
            synopsis: cap_synopsis(&strip_html(&item.synopsis)),
            genres: String::new(),
            length: String::new(),
            show_url: normalize_url(&format!("https://www.livechart.me/anime/{}", item.id)),
            episode_url: String::new(),
            movie_url: String::new(),
            trailer_url: String::new(),
            episodes,
            episode_count: 0,
            fetched_at: crate::timefmt::now_rfc3339(),
        }))
    }
}
