//! `metadata::kitsu` -- the Kitsu anime source (an independent database).
//!
//! Kitsu is keyless JSON:API (`https://kitsu.io/api/edge`): `GET
//! /anime?filter[text]=<title>&page[limit]=<n>` resolves the title to an
//! anime id, then `GET
//! /episodes?filter[mediaId]=<id>&sort=number&page[limit]=20[&page[offset]=<n>]`
//! carries one episode page (`seasonNumber`, `number`, `length`,
//! `canonicalTitle`, a Kitsu-hosted thumbnail).
//!
//! Three behaviours that are contract, not detail:
//!
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best returns a definitive MISS rather than a wrong show
//!   (the discipline the deleted iTunes leg lacked). The pick's `startDate`
//!   year NEVER rejects: at equal title strength it prefers the candidate
//!   carrying the query's year, but a missing year, or a year that disagrees
//!   with the filename, is a nice-to-have display field, never a gate -- the
//!   essentials are cover, title, episode and synopsis;
//! * the episode fetch is exactly ONE page (the 20-entry page holding the
//!   requested Number, or the first page when the query carries none), so one
//!   lookup stays two requests. The native (`seasonNumber`, `number`) numbering
//!   is kept verbatim: a positional renumber over a single page would be wrong,
//!   and the resolver's number-exact match falls through to the next source
//!   when the page lacks the Number.

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::parse::date_year;
use metadata_core::types::{Episode, Kind, Media, Query, normalize_url, runtime_length};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped, drain_and_close};
use ureq::http::StatusCode;

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::serdeutil::null_default;
use crate::urlutil::query_escape;

/// The provider's default host.
pub const KITSU_DEFAULT_BASE: &str = "https://kitsu.io/api/edge";

/// Kitsu's JSON:API answers `406 Not Acceptable` unless the request carries
/// this `Accept` (sent via [`HttpClient::get_with_accept`]).
const KITSU_ACCEPT: &str = "application/vnd.api+json";

/// How many search candidates one lookup ranks.
const KITSU_SEARCH_LIMIT: u32 = 10;

/// The one episode page's size (Kitsu's page maximum is 20).
const KITSU_EPISODE_LIMIT: u32 = 20;

/// `kitsuSource`.
pub struct KitsuSource {
    base: String,
    client: HttpClient,
}

impl KitsuSource {
    /// A source pointed at `base` (`""` means [`KITSU_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> KitsuSource {
        KitsuSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            KITSU_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for KitsuSource {
    fn default() -> KitsuSource {
        KitsuSource::new("", HttpClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuTitles {
    #[serde(default, deserialize_with = "null_default")]
    en: String,
    #[serde(default, deserialize_with = "null_default")]
    en_jp: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuPoster {
    #[serde(default, deserialize_with = "null_default")]
    large: String,
    #[serde(default, deserialize_with = "null_default")]
    original: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuThumb {
    #[serde(default, deserialize_with = "null_default")]
    original: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuAnimeAttrs {
    #[serde(default, deserialize_with = "null_default")]
    slug: String,
    #[serde(rename = "canonicalTitle", default, deserialize_with = "null_default")]
    canonical_title: String,
    /// `TV`, `movie`, `OVA`, `ONA`, `special`, `music`: the entry's own form.
    #[serde(default, deserialize_with = "null_default")]
    subtype: String,
    #[serde(default)]
    titles: Option<KitsuTitles>,
    #[serde(rename = "startDate", default, deserialize_with = "null_default")]
    start_date: String,
    #[serde(default, deserialize_with = "null_default")]
    synopsis: String,
    #[serde(rename = "posterImage", default)]
    poster_image: Option<KitsuPoster>,
    #[serde(rename = "episodeLength", default)]
    episode_length: Option<i32>,
    #[serde(rename = "episodeCount", default)]
    episode_count: Option<i32>,
    #[serde(rename = "userCount", default)]
    user_count: Option<i32>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuAnime {
    #[serde(default, deserialize_with = "null_default")]
    id: String,
    #[serde(default)]
    attributes: Option<KitsuAnimeAttrs>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuAnimeResp {
    #[serde(default, deserialize_with = "null_default")]
    data: Vec<KitsuAnime>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuEpisodeAttrs {
    #[serde(rename = "canonicalTitle", default, deserialize_with = "null_default")]
    canonical_title: String,
    #[serde(rename = "seasonNumber", default)]
    season_number: Option<i32>,
    #[serde(default)]
    number: Option<i32>,
    #[serde(default, deserialize_with = "null_default")]
    synopsis: String,
    #[serde(default)]
    length: Option<i32>,
    #[serde(default)]
    thumbnail: Option<KitsuThumb>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuEpisodeEntry {
    #[serde(default)]
    attributes: Option<KitsuEpisodeAttrs>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct KitsuEpisodesResp {
    #[serde(default, deserialize_with = "null_default")]
    data: Vec<KitsuEpisodeEntry>,
}

/// The entry's display title: English wins (`en`), the canonical title rides
/// second, romaji (`en_jp`) is the last resort. Returns the display title
/// plus every non-empty spelling for alias-aware matching, so a romaji query
/// still finds the English-titled main series instead of a romaji-named
/// special.
fn display_and_aliases(attrs: &KitsuAnimeAttrs) -> (String, Vec<String>) {
    let canonical = attrs.canonical_title.trim();
    let en = attrs
        .titles
        .as_ref()
        .map(|titles| titles.en.trim())
        .unwrap_or_default();
    let en_jp = attrs
        .titles
        .as_ref()
        .map(|titles| titles.en_jp.trim())
        .unwrap_or_default();
    let display = if !en.is_empty() {
        en.to_string()
    } else if !canonical.is_empty() {
        canonical.to_string()
    } else {
        en_jp.to_string()
    };
    let mut aliases = Vec::new();
    for title in [canonical, en, en_jp] {
        if !title.is_empty() {
            aliases.push(title.to_string());
        }
    }
    (display, aliases)
}

/// `kitsuPoster` prefers the large poster, falling back to the original.
fn kitsu_poster(poster: Option<&KitsuPoster>) -> String {
    let Some(poster) = poster else {
        return String::new();
    };
    if !poster.large.trim().is_empty() {
        return poster.large.trim().to_string();
    }
    poster.original.trim().to_string()
}

fn build_episode(attrs: &KitsuEpisodeAttrs) -> Option<Episode> {
    let number = attrs.number?;
    if number <= 0 {
        return None;
    }
    let artwork = attrs
        .thumbnail
        .as_ref()
        .map(|thumb| thumb.original.trim().to_string())
        .unwrap_or_default();
    Some(Episode {
        season: attrs.season_number.unwrap_or(0),
        number,
        title: attrs.canonical_title.trim().to_string(),
        artwork,
        synopsis: cap_synopsis(&strip_html(&attrs.synopsis)),
        url: String::new(),
        length: runtime_length(attrs.length.unwrap_or(0)),
    })
}

impl KitsuSource {
    /// The `/anime?filter[text]` rows for `title`, ONE request, in the
    /// provider's own relevance order. Rows without an id, attributes or a
    /// title are dropped.
    fn search_rows(
        &self,
        cancel: &Cancel,
        title: &str,
    ) -> Result<Vec<(KitsuAnime, KitsuAnimeAttrs)>, Error> {
        if title.is_empty() {
            return Ok(Vec::new());
        }
        let search_url = format!(
            "{}/anime?filter[text]={}&page[limit]={KITSU_SEARCH_LIMIT}",
            self.base_url(),
            query_escape(title),
        );
        let response = self
            .client
            .get_with_accept(cancel, &search_url, KITSU_ACCEPT)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(Vec::new());
        }
        let parsed: KitsuAnimeResp = decode_capped(response)?;
        let mut rows: Vec<(KitsuAnime, KitsuAnimeAttrs)> = Vec::with_capacity(parsed.data.len());
        for anime in parsed.data {
            let Some(attrs) = anime.attributes.clone() else {
                continue;
            };
            if anime.id.trim().is_empty() {
                continue;
            }
            let (name, _) = display_and_aliases(&attrs);
            if name.is_empty() {
                continue;
            }
            rows.push((anime, attrs));
        }
        Ok(rows)
    }

    /// One validated search pick: the title-ranked best, with a known query
    /// year preferring any challenger that both carries it and fully covers
    /// the query title. `Ok(None)` is a definitive miss. Year never gates:
    /// it may only ride along as a display field.
    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<(KitsuAnime, KitsuAnimeAttrs)>, Error> {
        let rows = self.search_rows(cancel, &q.title)?;
        let mut cands: Vec<ShowCand> = Vec::with_capacity(rows.len());
        let mut years: Vec<i32> = Vec::with_capacity(rows.len());
        for (_, attrs) in &rows {
            let (name, aliases) = display_and_aliases(attrs);
            cands.push(ShowCand {
                name,
                weight: attrs.user_count.unwrap_or(0),
                episode_count: attrs.episode_count.unwrap_or(0),
                aliases,
            });
            years.push(date_year(&attrs.start_date));
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

    /// The one episode page holding the requested Number (the first page when
    /// the query carries none). Native numbering is kept verbatim.
    fn episodes(
        &self,
        cancel: &Cancel,
        id: &str,
        want: i32,
    ) -> Result<Vec<Episode>, Error> {
        let offset = if want > 0 {
            (want - 1) as u32 / KITSU_EPISODE_LIMIT * KITSU_EPISODE_LIMIT
        } else {
            0
        };
        let url = format!(
            "{}/episodes?filter[mediaId]={}&sort=number&page[limit]={KITSU_EPISODE_LIMIT}&page[offset]={offset}",
            self.base_url(),
            query_escape(id),
        );
        let response = self.client.get_with_accept(cancel, &url, KITSU_ACCEPT)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(Vec::new());
        }
        let parsed: KitsuEpisodesResp = decode_capped(response)?;
        let mut out: Vec<Episode> = Vec::with_capacity(parsed.data.len());
        for entry in &parsed.data {
            if let Some(attrs) = &entry.attributes
                && let Some(episode) = build_episode(attrs)
            {
                out.push(episode);
            }
        }
        out.sort_by_key(|episode| (episode.season, episode.number));
        Ok(out)
    }
}

impl crate::resolver::Source for KitsuSource {
    fn name(&self) -> &str {
        "kitsu"
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
        let Some((anime, attrs)) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        let (title, _) = display_and_aliases(&attrs);
        if title.is_empty() {
            return Ok(None);
        }
        let episodes = self.episodes(cancel, anime.id.trim(), q.episode)?;
        let mut media = kitsu_media(&anime, &attrs, title, Kind::SERIES);
        media.episodes = episodes;
        Ok(Some(media))
    }

    /// The manual search's rows: the same ONE `/anime?filter[text]` request,
    /// each entry whose title clears the matcher's floor as its own pickable
    /// row, with an EMPTY episode table (no episode page is fetched). The
    /// entry's own `subtype` decides the kind: `movie` is a movie, everything
    /// else a series.
    fn find_all(&self, cancel: &Cancel, q: &Query) -> Result<Vec<Media>, Error> {
        if q.title.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Media> = Vec::new();
        for (anime, attrs) in self.search_rows(cancel, &q.title)? {
            let (name, aliases) = display_and_aliases(&attrs);
            if name.is_empty() {
                continue;
            }
            let cand = ShowCand {
                name: name.clone(),
                weight: attrs.user_count.unwrap_or(0),
                episode_count: attrs.episode_count.unwrap_or(0),
                aliases,
            };
            // The same floor `Find` applies: a fuzzy row never becomes a pick.
            if pick_best_show(&q.title, std::slice::from_ref(&cand)).is_none() {
                continue;
            }
            let kind = if attrs.subtype.trim().eq_ignore_ascii_case("movie") {
                Kind::MOVIE
            } else {
                Kind::SERIES
            };
            out.push(kitsu_media(&anime, &attrs, name, kind));
            if out.len() == crate::resolver::FIND_ALL_LIMIT {
                break;
            }
        }
        Ok(out)
    }
}

/// The candidate-ready [`Media`] for one Kitsu entry: identity, year, cover,
/// length and page URL, with the kind the caller derives from `subtype`. The
/// episode table is the caller's to fill.
fn kitsu_media(anime: &KitsuAnime, attrs: &KitsuAnimeAttrs, title: String, kind: Kind) -> Media {
    Media {
        source: "kitsu".to_string(),
        source_id: anime.id.trim().to_string(),
        kind,
        title,
        year: date_year(&attrs.start_date),
        artwork: kitsu_poster(attrs.poster_image.as_ref()),
        synopsis: cap_synopsis(&strip_html(&attrs.synopsis)),
        // Genres live behind a second request per show; the episode page is
        // the lookup's whole episode budget, so this stays empty.
        genres: String::new(),
        length: runtime_length(attrs.episode_length.unwrap_or(0)),
        show_url: normalize_url(&format!(
            "https://kitsu.io/anime/{}",
            attrs.slug.trim()
        )),
        episode_url: String::new(),
        movie_url: String::new(),
        trailer_url: String::new(),
        episodes: Vec::new(),
        fetched_at: crate::timefmt::now_rfc3339(),
    }
}
