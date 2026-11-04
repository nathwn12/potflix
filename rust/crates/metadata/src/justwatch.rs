//! `metadata::justwatch` -- the JustWatch movie leg (unofficial GraphQL).
//!
//! JustWatch (`https://apis.justwatch.com/graphql`) is keyless for search:
//! one `GetSearchTitles` POST (`popularTitles` filtered by `searchQuery`)
//! carries cover, synopsis and length for movies AND series in a single call.
//!
//! Two behaviours that are contract, not detail:
//!
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best returns a definitive MISS rather than a wrong title.
//!   The pick's `originalReleaseYear` NEVER rejects: at equal title strength
//!   it prefers the candidate carrying the query's year, but a missing year,
//!   or a year that disagrees with the filename, is a nice-to-have display
//!   field, never a gate;
//! * the endpoint is unofficial and undocumented, so it is coverage only: it
//!   ranks behind Cinemeta and Wikipedia in the movie chain and never serves
//!   series queries (a series key returns `Ok(None)` before any fetch).
//!
//! Genre short codes (`"act"`, `"drm"`) are machine tags, not display genres,
//! so they are deliberately dropped (the Kitsu precedent: no genres rather
//! than wrong ones). The poster template (`/poster/<id>/{profile}/<slug>.
//! {format}`) resolves against [`JW_IMAGE_BASE`] at `s718`/`jpg`, verified
//! live (a `HEAD` returns `image/jpeg`).

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::types::{Episode, Kind, Media, Query, normalize_url, runtime_length};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped, drain_and_close};
use serde_json::json;
use ureq::http::StatusCode;

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::serdeutil::null_default;

/// The provider's default host.
pub const JUSTWATCH_DEFAULT_BASE: &str = "https://apis.justwatch.com";

/// The image host backing `posterUrl` templates.
pub const JW_IMAGE_BASE: &str = "https://images.justwatch.com";

/// How many search candidates one lookup ranks.
const JUSTWATCH_SEARCH_FIRST: u32 = 8;

/// `justwatchSource`.
pub struct JustWatchSource {
    base: String,
    client: HttpClient,
}

impl JustWatchSource {
    /// A source pointed at `base` (`""` means [`JUSTWATCH_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> JustWatchSource {
        JustWatchSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            JUSTWATCH_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for JustWatchSource {
    fn default() -> JustWatchSource {
        JustWatchSource::new("", HttpClient::new(Duration::ZERO))
    }
}

const JUSTWATCH_QUERY: &str = "query GetSearchTitles($searchTitlesFilter: TitleFilter!, $country: Country!, $language: Language!, $first: Int!) { popularTitles(country: $country, filter: $searchTitlesFilter, first: $first, sortBy: POPULAR, sortRandomSeed: 0) { edges { node { id objectId objectType content(country: $country, language: $language) { title posterUrl shortDescription originalReleaseYear runtime genres { shortName } } __typename } __typename } } }";

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JwContent {
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(rename = "posterUrl", default, deserialize_with = "null_default")]
    poster_url: String,
    #[serde(rename = "shortDescription", default, deserialize_with = "null_default")]
    short_description: String,
    #[serde(rename = "originalReleaseYear", default)]
    original_release_year: Option<i32>,
    #[serde(default)]
    runtime: Option<i32>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JwNode {
    #[serde(default, deserialize_with = "null_default")]
    id: String,
    #[serde(rename = "objectType", default, deserialize_with = "null_default")]
    object_type: String,
    #[serde(default)]
    content: Option<JwContent>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JwEdge {
    #[serde(default)]
    node: Option<JwNode>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JwPopular {
    #[serde(default, deserialize_with = "null_default")]
    edges: Vec<JwEdge>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JwData {
    #[serde(rename = "popularTitles", default)]
    popular_titles: Option<JwPopular>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct JwResp {
    #[serde(default)]
    data: Option<JwData>,
}

/// The poster template with the verified profile/format filled in.
fn jw_artwork(template: &str) -> String {
    let url = template
        .trim()
        .replace("{profile}", "s718")
        .replace("{format}", "jpg");
    if url.trim().is_empty() || url.contains('{') {
        return String::new();
    }
    if url.starts_with("http") {
        return normalize_url(&url);
    }
    normalize_url(&format!("{JW_IMAGE_BASE}{url}"))
}

impl JustWatchSource {
    /// One validated search pick: the title-ranked best, with a known query
    /// year preferring any challenger that both carries it and fully covers
    /// the query title. `Ok(None)` is a definitive miss. Year never gates.
    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<(JwNode, JwContent)>, Error> {
        let mut filter = json!({
            "searchQuery": q.title,
            "includeTitlesWithoutUrl": true,
        });
        if q.kind == Kind::MOVIE
            && let Some(obj) = filter.as_object_mut()
        {
            obj.insert("objectTypes".to_string(), json!(["MOVIE"]));
        }
        let payload = json!({
            "operationName": "GetSearchTitles",
            "variables": {
                "searchTitlesFilter": filter,
                "country": "US",
                "language": "en",
                "first": JUSTWATCH_SEARCH_FIRST,
            },
            "query": JUSTWATCH_QUERY,
        });
        let url = format!("{}/graphql", self.base_url());
        // Read-only GraphQL search: side-effect-free, so the idempotent POST
        // retry (backoff + jitter, honoring Retry-After) applies. A mutating
        // POST would use `post_json` and never retry.
        let response = self.client.post_json_idempotent(cancel, &url, &payload)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(None);
        }
        let parsed: JwResp = decode_capped(response)?;
        let mut cands: Vec<ShowCand> = Vec::new();
        let mut entries: Vec<(JwNode, JwContent)> = Vec::new();
        let mut years: Vec<i32> = Vec::new();
        let edges = parsed
            .data
            .and_then(|data| data.popular_titles)
            .map(|popular| popular.edges)
            .unwrap_or_default();
        for edge in edges {
            let Some(node) = edge.node else { continue };
            let Some(content) = node.content.clone() else {
                continue;
            };
            if node.id.trim().is_empty() {
                continue;
            }
            let name = content.title.trim().to_string();
            if name.is_empty() {
                continue;
            }
            cands.push(ShowCand {
                name,
                weight: 0,
                episode_count: 0,
                aliases: Vec::new(),
            });
            years.push(content.original_release_year.unwrap_or(0));
            entries.push((node, content));
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

impl crate::resolver::Source for JustWatchSource {
    fn name(&self) -> &str {
        "justwatch"
    }

    /// `Find`: the movie leg only (a series key never reaches the wire). An
    /// `UNKNOWN` query takes the node's own kind: a `MOVIE` node answers as a
    /// movie, a `SHOW` node as a series record with no episode table (it can
    /// only match when the query carries no episode number -- which never
    /// number-matches, so it is effectively a miss downstream and never a
    /// wrong answer).
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::SERIES || q.title.is_empty() {
            return Ok(None);
        }
        let Some((node, content)) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        let title = content.title.trim().to_string();
        if title.is_empty() {
            return Ok(None);
        }
        let year = content.original_release_year.unwrap_or(0);
        let artwork = jw_artwork(&content.poster_url);
        let synopsis = cap_synopsis(&strip_html(&content.short_description));
        let length = runtime_length(content.runtime.unwrap_or(0));
        let object_type = node.object_type.trim().to_ascii_uppercase();
        if q.kind == Kind::MOVIE || object_type == "MOVIE" {
            return Ok(Some(Media {
                source: "justwatch".to_string(),
                source_id: node.id.trim().to_string(),
                kind: Kind::MOVIE,
                title,
                year,
                artwork,
                synopsis,
                genres: String::new(),
                length,
                show_url: String::new(),
                episode_url: String::new(),
                movie_url: String::new(),
                trailer_url: String::new(),
                episodes: Vec::new(),
                episode_count: 0,
                fetched_at: crate::timefmt::now_rfc3339(),
            }));
        }
        if object_type == "SHOW" {
            return Ok(Some(Media {
                source: "justwatch".to_string(),
                source_id: node.id.trim().to_string(),
                kind: Kind::SERIES,
                title,
                year,
                artwork,
                synopsis,
                genres: String::new(),
                length,
                show_url: String::new(),
                episode_url: String::new(),
                movie_url: String::new(),
                trailer_url: String::new(),
                episodes: Vec::new(),
                episode_count: 0,
                fetched_at: crate::timefmt::now_rfc3339(),
            }));
        }
        Ok(None)
    }
}

/// Numbered episode stubs for sources that carry an episode COUNT but no
/// episode table (Bangumi, LiveChart.me, ANN, and JustWatch SHOW nodes).
/// The stubs carry numbers only -- never fabricated titles or synopses -- so
/// they can satisfy the resolver's number-exact match without ever producing
/// a wrong title. `season` is the caller's convention (absolute anime tables
/// use 0).
pub(crate) fn episode_stubs(count: i32, season: i32) -> Vec<Episode> {
    if count <= 0 {
        return Vec::new();
    }
    (1..=count)
        .map(|number| Episode {
            season,
            number,
            ..Episode::default()
        })
        .collect()
}
