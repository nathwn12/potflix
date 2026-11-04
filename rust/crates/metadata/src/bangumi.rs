//! `metadata::bangumi` -- the Bangumi subject source (keyless search).
//!
//! Bangumi (`https://api.bgm.tv`) needs no key for subject search: `GET
//! /search/subject/<title>?type=<n>&responseGroup=large` carries cover,
//! titles, episode count, air date, rating and synopsis in a single call,
//! where `type=2` is anime and `type=6` is live TV.
//!
//! Three behaviours that are contract, not detail:
//!
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best returns a definitive MISS rather than a wrong show.
//!   The pick's `air_date` year NEVER rejects: at equal title strength it
//!   prefers the candidate carrying the query's year, but a missing year, or
//!   a year that disagrees with the filename, is a nice-to-have display
//!   field, never a gate;
//! * the synopsis is never required: some entries carry an empty `summary`,
//!   and an empty one still answers (the resolver keeps a bare match as the
//!   fallback while the walk continues);
//! * Bangumi carries an episode COUNT but no episode table, so the answer
//!   synthesizes numbered stubs ([`episode_stubs`]) -- numbers only, never
//!   fabricated titles. Anime tables (`type=2`) number absolutely (season 0);
//!   live-TV tables (`type=6`) number from season 1, so both absolute and
//!   season-qualified queries can number-match without ever naming a wrong
//!   episode. Movies never reach the wire.

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
use crate::urlutil::path_escape;

/// The provider's default host.
pub const BANGUMI_DEFAULT_BASE: &str = "https://api.bgm.tv";

/// Bangumi subject type: anime.
const BANGUMI_TYPE_ANIME: u32 = 2;
/// Bangumi subject type: live TV.
const BANGUMI_TYPE_TV: u32 = 6;

/// How many search candidates one lookup ranks per type.
const BANGUMI_SEARCH_LIMIT: u32 = 10;

/// `bangumiSource`.
pub struct BangumiSource {
    base: String,
    client: HttpClient,
}

impl BangumiSource {
    /// A source pointed at `base` (`""` means [`BANGUMI_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> BangumiSource {
        BangumiSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            BANGUMI_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for BangumiSource {
    fn default() -> BangumiSource {
        BangumiSource::new("", HttpClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BangumiImages {
    #[serde(default, deserialize_with = "null_default")]
    large: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BangumiEntry {
    #[serde(default, deserialize_with = "null_default")]
    id: i32,
    #[serde(default, deserialize_with = "null_default")]
    url: String,
    #[serde(default, deserialize_with = "null_default")]
    name: String,
    #[serde(rename = "name_cn", default, deserialize_with = "null_default")]
    name_cn: String,
    #[serde(default, deserialize_with = "null_default")]
    summary: String,
    #[serde(default)]
    eps: Option<i32>,
    #[serde(rename = "eps_count", default)]
    eps_count: Option<i32>,
    #[serde(rename = "air_date", default, deserialize_with = "null_default")]
    air_date: String,
    #[serde(default)]
    images: Option<BangumiImages>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BangumiSearchResp {
    #[serde(default, deserialize_with = "null_default")]
    list: Vec<BangumiEntry>,
}

/// The display title: the native `name`, falling back to the Chinese title.
fn bangumi_title(entry: &BangumiEntry) -> String {
    if !entry.name.trim().is_empty() {
        return entry.name.trim().to_string();
    }
    entry.name_cn.trim().to_string()
}

/// The episode count: `eps_count`, falling back to `eps` (either may be null
/// or absent on the wire).
fn bangumi_count(entry: &BangumiEntry) -> i32 {
    entry.eps_count.unwrap_or(0).max(entry.eps.unwrap_or(0))
}

impl BangumiSource {
    /// One validated search pick of `subject_type`: the title-ranked best,
    /// with a known query year preferring any challenger that both carries
    /// it and fully covers the query title. `Ok(None)` is a definitive miss.
    /// Year never gates.
    fn pick(
        &self,
        cancel: &Cancel,
        q: &Query,
        subject_type: u32,
    ) -> Result<Option<BangumiEntry>, Error> {
        let search_url = format!(
            "{}/search/subject/{}?type={subject_type}&responseGroup=large&max_results={BANGUMI_SEARCH_LIMIT}",
            self.base_url(),
            path_escape(&q.title),
        );
        let response = self.client.get(cancel, &search_url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(None);
        }
        let parsed: BangumiSearchResp = decode_capped(response)?;
        let mut cands: Vec<ShowCand> = Vec::with_capacity(parsed.list.len());
        let mut entries: Vec<BangumiEntry> = Vec::with_capacity(parsed.list.len());
        let mut years: Vec<i32> = Vec::with_capacity(parsed.list.len());
        for entry in parsed.list {
            if entry.id == 0 {
                continue;
            }
            let name = bangumi_title(&entry);
            if name.is_empty() {
                continue;
            }
            cands.push(ShowCand {
                name,
                weight: 0,
                episode_count: bangumi_count(&entry),
                aliases: Vec::new(),
            });
            years.push(date_year(&entry.air_date));
            entries.push(entry);
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

impl crate::resolver::Source for BangumiSource {
    fn name(&self) -> &str {
        "bangumi"
    }

    /// `Find`: series only (movies never reach the anime/TV legs through the
    /// resolver, and a series-only source self-gates a ruled-out kind before
    /// touching the wire). Absolute and unknown queries search anime first,
    /// season-qualified ones live TV first; the first validated pick answers.
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::MOVIE || q.title.is_empty() {
            return Ok(None);
        }
        let types: [u32; 2] = if q.season > 0 && !q.absolute {
            [BANGUMI_TYPE_TV, BANGUMI_TYPE_ANIME]
        } else {
            [BANGUMI_TYPE_ANIME, BANGUMI_TYPE_TV]
        };
        let mut picked: Option<BangumiEntry> = None;
        let mut picked_type = types[0];
        for subject_type in types {
            if let Some(entry) = self.pick(cancel, q, subject_type)? {
                picked = Some(entry);
                picked_type = subject_type;
                break;
            }
        }
        let Some(entry) = picked else {
            return Ok(None);
        };
        let title = bangumi_title(&entry);
        if title.is_empty() {
            return Ok(None);
        }
        // Numbered stubs only: Bangumi carries a count but no table, so the
        // stubs can satisfy the number-exact match without ever naming a
        // wrong episode.
        let season = if picked_type == BANGUMI_TYPE_ANIME { 0 } else { 1 };
        let episodes = episode_stubs(bangumi_count(&entry), season);
        let artwork = entry
            .images
            .as_ref()
            .map(|images| normalize_url(&images.large))
            .unwrap_or_default();
        Ok(Some(Media {
            source: "bangumi".to_string(),
            source_id: entry.id.to_string(),
            kind: Kind::SERIES,
            title,
            year: date_year(&entry.air_date),
            artwork,
            synopsis: cap_synopsis(&strip_html(&entry.summary)),
            genres: String::new(),
            length: String::new(),
            show_url: normalize_url(&entry.url),
            episode_url: String::new(),
            movie_url: String::new(),
            trailer_url: String::new(),
            episodes,
            episode_count: 0,
            fetched_at: crate::timefmt::now_rfc3339(),
        }))
    }
}
