//! `metadata::imdb` -- IMDb's first-party GraphQL as a keyless regular source.
//!
//! POST `https://api.graphql.imdb.com/` with a read-only `mainSearch`
//! query; one request per search, no episode table. The wire carries
//! `mainSearch edges[].node.entity Title id/titleText.text/releaseYear.year/`
//! `plot.plotText.plainText/primaryImage.url/runtime.seconds/`
//! `genres.genres[].text` plus the title-type field, which maps `MOVIE` vs
//! series. The parser is tolerant: missing or differently-cased type shapes
//! still map (any `movie` token means movie, everything else series), so a
//! live shape drift keeps answering instead of erroring.
//!
//! Behaviours that are contract:
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best is a definitive miss, never a wrong title; year
//!   never gates, it only prefers at equal title strength.
//! * `find_all` is the same one-POST answer, each Title clearing the floor
//!   as its own pickable row with an empty episode table.

use std::time::{Duration, SystemTime};

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::types::{Kind, Media, Query, join_genres, normalize_url, runtime_length};
use metadata_http::cancel::Cancel;
use metadata_http::{
    ACCEPT_JSON, CONTENT_TYPE_JSON, DEFAULT_HTTP_TIMEOUT, MAX_429_RETRIES, decode_capped,
    drain_and_close, parse_retry_after, retry_wait_for_attempt, wait_for_host,
};
use serde_json::json;
use ureq::Body;
use ureq::http::{Response, StatusCode, Uri};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::resolver::SearchPath;
use crate::serdeutil::null_default;

/// The provider's default host (GraphQL endpoint is the host root).
pub const IMDB_DEFAULT_BASE: &str = "https://api.graphql.imdb.com";

/// IMDb's browser headers: CloudFront 403s the shared metadata UA without
/// them (measured 2026-11-02: default UA -> 403, browser UA + Origin +
/// Referer -> 200). Read-only search, no credentials.
pub const IMDB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";
const IMDB_ORIGIN: &str = "https://www.imdb.com";
const IMDB_REFERER: &str = "https://www.imdb.com/";

/// The read-only search query. `titleType` is requested in both common
/// shapes (`text` and `id`) so the kind mapping survives a server-side
/// rename; the parser accepts either. `options:{searchTerm:}` is required:
/// a bare `searchTerm:` arg fails GraphQL validation.
pub const IMDB_QUERY: &str = "query ($search: String!) { mainSearch(first: 10, options: {searchTerm: $search}) { edges { node { entity { __typename ... on Title { id titleText { text } titleType { id text } releaseYear { year } plot { plotText { plainText } } primaryImage { url } runtime { seconds } genres { genres { text } } } } } } } }";

/// The POST transport for IMDb: `http.go`'s idempotent wire shape with the
/// browser headers CloudFront requires. Same limiter, same bounded 429
/// retries, same caps -- only the headers differ.
pub struct ImdbClient {
    agent: ureq::Agent,
}

impl ImdbClient {
    /// A client whose whole-request budget is `timeout`; zero takes
    /// [`DEFAULT_HTTP_TIMEOUT`].
    pub fn new(timeout: Duration) -> ImdbClient {
        let timeout = if timeout.is_zero() {
            DEFAULT_HTTP_TIMEOUT
        } else {
            timeout
        };
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .http_status_as_error(false)
            .build();
        ImdbClient {
            agent: config.into(),
        }
    }

    /// Read-only GraphQL search POST with IMDb's browser headers and the
    /// shared idempotent 429 retries.
    pub fn post_search<T: serde::Serialize>(
        &self,
        cancel: &Cancel,
        url: &str,
        payload: &T,
    ) -> Result<Response<Body>, crate::Error> {
        let body = serde_json::to_vec(payload).map_err(crate::Error::from)?;
        let uri: Uri = url
            .parse()
            .map_err(|err: ureq::http::uri::InvalidUri| {
                crate::Error::other(format!("metadata: invalid request URL: {err}"))
            })?;
        let mut attempt: u32 = 0;
        loop {
            let response = self.attempt(cancel, &uri, &body)?;
            if response.status().as_u16() != StatusCode::TOO_MANY_REQUESTS.as_u16() {
                return Ok(response);
            }
            if attempt >= MAX_429_RETRIES {
                return Ok(response);
            }
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let server_wait = parse_retry_after(retry_after, SystemTime::now());
            let wait = retry_wait_for_attempt(server_wait, attempt);
            drain_and_close(response);
            cancel.wait(wait).map_err(crate::Error::from)?;
            attempt += 1;
        }
    }

    fn attempt(
        &self,
        cancel: &Cancel,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Response<Body>, crate::Error> {
        wait_for_host(cancel, uri.host().unwrap_or_default()).map_err(crate::Error::from)?;
        if let Some(reason) = cancel.err() {
            return Err(crate::Error::from(reason));
        }
        self.agent
            .post(uri.clone())
            .header("User-Agent", IMDB_USER_AGENT)
            .header("Accept", ACCEPT_JSON)
            .header("Content-Type", CONTENT_TYPE_JSON)
            .header("Origin", IMDB_ORIGIN)
            .header("Referer", IMDB_REFERER)
            .send(body)
            .map_err(|err| crate::Error::other(format!("metadata: request failed: {err}")))
    }
}

impl Default for ImdbClient {
    fn default() -> ImdbClient {
        ImdbClient::new(Duration::ZERO)
    }
}

/// `imdbSource`.
pub struct ImdbSource {
    base: String,
    client: ImdbClient,
}

impl ImdbSource {
    /// A source pointed at `base` (`""` means [`IMDB_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: ImdbClient) -> ImdbSource {
        ImdbSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            IMDB_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for ImdbSource {
    fn default() -> ImdbSource {
        ImdbSource::new("", ImdbClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbText {
    #[serde(default, deserialize_with = "null_default")]
    text: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbTitleType {
    #[serde(default, deserialize_with = "null_default")]
    id: String,
    #[serde(default, deserialize_with = "null_default")]
    text: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbYear {
    #[serde(default)]
    year: Option<i32>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbPlotText {
    #[serde(default, rename = "plainText", deserialize_with = "null_default")]
    plain_text: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbPlot {
    #[serde(default, rename = "plotText")]
    plot_text: Option<ImdbPlotText>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbImage {
    #[serde(default, deserialize_with = "null_default")]
    url: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbRuntime {
    #[serde(default)]
    seconds: Option<i32>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbGenreItem {
    #[serde(default, deserialize_with = "null_default")]
    text: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbGenres {
    #[serde(default, deserialize_with = "null_default")]
    genres: Vec<ImdbGenreItem>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbEntity {
    #[serde(default, deserialize_with = "null_default")]
    id: String,
    #[serde(default, rename = "titleText")]
    title_text: Option<ImdbText>,
    #[serde(default, rename = "titleType")]
    title_type: Option<ImdbTitleType>,
    #[serde(default, rename = "releaseYear")]
    release_year: Option<ImdbYear>,
    #[serde(default)]
    plot: Option<ImdbPlot>,
    #[serde(default, rename = "primaryImage")]
    primary_image: Option<ImdbImage>,
    #[serde(default)]
    runtime: Option<ImdbRuntime>,
    #[serde(default)]
    genres: Option<ImdbGenres>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbNode {
    #[serde(default)]
    entity: Option<ImdbEntity>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbEdge {
    #[serde(default)]
    node: Option<ImdbNode>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbMainSearch {
    #[serde(default, deserialize_with = "null_default")]
    edges: Vec<ImdbEdge>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbData {
    #[serde(default, rename = "mainSearch")]
    main_search: Option<ImdbMainSearch>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct ImdbResp {
    #[serde(default)]
    data: Option<ImdbData>,
}

/// The title-type text from either `text` or `id`, whichever carries one.
fn title_type_text(kind: Option<&ImdbTitleType>) -> String {
    let Some(kind) = kind else {
        return String::new();
    };
    if !kind.text.trim().is_empty() {
        return kind.text.trim().to_string();
    }
    kind.id.trim().to_string()
}

/// `movie` anywhere in the type means movie (covers `movie`, `tvMovie`,
/// `videoMovie` spellings); everything else (`tvSeries`, `tvMiniSeries`,
/// `tvSpecial`, missing) is a series. The search layer re-ranks by kind
/// anyway, so an unknown type never blocks a row.
fn kind_for_type(type_text: &str) -> Kind {
    if type_text.to_lowercase().contains("movie") {
        Kind::MOVIE
    } else {
        Kind::SERIES
    }
}

fn title_of(entity: &ImdbEntity) -> String {
    entity
        .title_text
        .as_ref()
        .map(|t| t.text.trim().to_string())
        .unwrap_or_default()
}

fn year_of(entity: &ImdbEntity) -> i32 {
    entity
        .release_year
        .as_ref()
        .and_then(|y| y.year)
        .unwrap_or(0)
}

fn synopsis_of(entity: &ImdbEntity) -> String {
    let raw = entity
        .plot
        .as_ref()
        .and_then(|p| p.plot_text.as_ref())
        .map(|t| t.plain_text.as_str())
        .unwrap_or_default();
    cap_synopsis(&strip_html(raw))
}

fn artwork_of(entity: &ImdbEntity) -> String {
    entity
        .primary_image
        .as_ref()
        .map(|img| normalize_url(&img.url))
        .unwrap_or_default()
}

fn length_of(entity: &ImdbEntity) -> String {
    let seconds = entity.runtime.as_ref().and_then(|r| r.seconds).unwrap_or(0);
    if seconds <= 0 {
        return String::new();
    }
    runtime_length(seconds / 60)
}

fn genres_of(entity: &ImdbEntity) -> String {
    let genres: Vec<String> = entity
        .genres
        .as_ref()
        .map(|g| g.genres.iter().map(|item| item.text.clone()).collect())
        .unwrap_or_default();
    join_genres(&genres)
}

fn imdb_media(entity: &ImdbEntity, title: String, kind: Kind) -> Media {
    let id = entity.id.trim().to_string();
    Media {
        source: "imdb".to_string(),
        source_id: id.clone(),
        kind,
        title,
        year: year_of(entity),
        artwork: artwork_of(entity),
        synopsis: synopsis_of(entity),
        genres: genres_of(entity),
        length: length_of(entity),
        show_url: if kind == Kind::MOVIE {
            String::new()
        } else {
            normalize_url(&format!("https://www.imdb.com/title/{}/", id.trim()))
        },
        episode_url: String::new(),
        movie_url: if kind == Kind::MOVIE {
            normalize_url(&format!("https://www.imdb.com/title/{}/", id.trim()))
        } else {
            String::new()
        },
        trailer_url: String::new(),
        episodes: Vec::new(),
        fetched_at: crate::timefmt::now_rfc3339(),
    }
}

impl ImdbSource {
    fn search_entities(&self, cancel: &Cancel, title: &str) -> Result<Vec<ImdbEntity>, Error> {
        if title.is_empty() {
            return Ok(Vec::new());
        }
        let payload = json!({
            "query": IMDB_QUERY,
            "variables": {"search": title},
        });
        let response = self.client.post_search(cancel, self.base_url(), &payload)?;
        let parsed: ImdbResp = decode_capped(response)?;
        let edges = parsed
            .data
            .and_then(|d| d.main_search)
            .map(|s| s.edges)
            .unwrap_or_default();
        let mut out = Vec::with_capacity(edges.len());
        for edge in edges {
            let Some(entity) = edge.node.and_then(|n| n.entity) else {
                continue;
            };
            if entity.id.trim().is_empty() || title_of(&entity).is_empty() {
                continue;
            }
            out.push(entity);
        }
        Ok(out)
    }

    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<(ImdbEntity, Kind)>, Error> {
        let rows = self.search_entities(cancel, &q.title)?;
        let mut cands: Vec<ShowCand> = Vec::with_capacity(rows.len());
        let mut years: Vec<i32> = Vec::with_capacity(rows.len());
        let mut kinds: Vec<Kind> = Vec::with_capacity(rows.len());
        for entity in &rows {
            let name = title_of(entity);
            let kind = kind_for_type(&title_type_text(entity.title_type.as_ref()));
            // The resolver asks per kind: a movie lookup skips series rows
            // here so a same-titled series never steals a film query (the
            // search rows keep every kind; the rank prefers the want).
            if q.kind == Kind::MOVIE && kind != Kind::MOVIE {
                continue;
            }
            if q.kind == Kind::SERIES && kind != Kind::SERIES {
                continue;
            }
            cands.push(ShowCand {
                name,
                weight: 0,
                episode_count: 0,
                aliases: Vec::new(),
            });
            years.push(year_of(entity));
            kinds.push(kind);
        }
        if cands.is_empty() {
            return Ok(None);
        }
        let Some(winner) = pick_best_show(&q.title, &cands) else {
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
        let take =
            crate::yearpref::prefer_year_match(&q.title, q.year, &cands, &years, winner_idx);
        // Map back to the filtered rows: kinds/cands/rows share the filter,
        // so re-walk the entities in the same filtered order.
        let mut filtered: Vec<&ImdbEntity> = Vec::new();
        for entity in &rows {
            let kind = kind_for_type(&title_type_text(entity.title_type.as_ref()));
            if q.kind == Kind::MOVIE && kind != Kind::MOVIE {
                continue;
            }
            if q.kind == Kind::SERIES && kind != Kind::SERIES {
                continue;
            }
            filtered.push(entity);
        }
        let entity = filtered.get(take).cloned().cloned();
        Ok(entity.map(|e| {
            let kind = kinds.get(take).copied().unwrap_or(Kind::SERIES);
            (e, kind)
        }))
    }
}

impl crate::resolver::Source for ImdbSource {
    fn name(&self) -> &str {
        "imdb"
    }

    fn path(&self) -> SearchPath {
        SearchPath::Regular
    }

    /// `Find`: the one-POST `mainSearch` answer, title-validated. Movies
    /// answer at once (no episode table); series rows carry no table here,
    /// so a series query with an episode number falls through to the next
    /// leg while the search rows still offer the pick.
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.title.is_empty() {
            return Ok(None);
        }
        // UNKNOWN queries try series first (a series answer can only match
        // when the query carries an episode number), then movie.
        if q.kind == Kind::UNKNOWN {
            let series_q = Query {
                kind: Kind::SERIES,
                ..q.clone()
            };
            if let Some((entity, kind)) = self.pick(cancel, &series_q)? {
                let title = title_of(&entity);
                if !title.is_empty() {
                    return Ok(Some(imdb_media(&entity, title, kind)));
                }
            }
            let movie_q = Query {
                kind: Kind::MOVIE,
                ..q.clone()
            };
            if let Some((entity, kind)) = self.pick(cancel, &movie_q)? {
                let title = title_of(&entity);
                if !title.is_empty() {
                    return Ok(Some(imdb_media(&entity, title, kind)));
                }
            }
            return Ok(None);
        }
        let Some((entity, kind)) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        let title = title_of(&entity);
        if title.is_empty() {
            return Ok(None);
        }
        Ok(Some(imdb_media(&entity, title, kind)))
    }

    /// The manual search's rows: the same one-POST answer, each Title
    /// clearing the floor as its own pickable entry with an empty episode
    /// table. The entry's own title-type decides the kind.
    fn find_all(&self, cancel: &Cancel, q: &Query) -> Result<Vec<Media>, Error> {
        if q.title.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Media> = Vec::new();
        for entity in self.search_entities(cancel, &q.title)? {
            let name = title_of(&entity);
            if name.is_empty() {
                continue;
            }
            let cand = ShowCand {
                name: name.clone(),
                weight: 0,
                episode_count: 0,
                aliases: Vec::new(),
            };
            if pick_best_show(&q.title, std::slice::from_ref(&cand)).is_none() {
                continue;
            }
            let kind = kind_for_type(&title_type_text(entity.title_type.as_ref()));
            out.push(imdb_media(&entity, name, kind));
            if out.len() == crate::resolver::FIND_ALL_LIMIT {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolver::Source;
    use crate::testsupport::{FakeServer, Request, Response};

    fn test_client() -> ImdbClient {
        ImdbClient::new(std::time::Duration::from_secs(5))
    }

    /// Recorded `mainSearch` fixture: a movie Title and a series Title.
    const GRAPHQL_FIXTURE: &str = r#"{"data":{"mainSearch":{"edges":[{"node":{"entity":{"__typename":"Title","id":"tt1375666","titleText":{"text":"Inception"},"titleType":{"id":"movie","text":"Movie"},"releaseYear":{"year":2010},"plot":{"plotText":{"plainText":"A skilled thief steals secrets."}},"primaryImage":{"url":"https://art/inception.jpg"},"runtime":{"seconds":8880},"genres":{"genres":[{"text":"Sci-Fi"},{"text":"Thriller"}]}}}},{"node":{"entity":{"__typename":"Title","id":"tt0903747","titleText":{"text":"Breaking Bad"},"titleType":{"id":"tvSeries","text":"TV Series"},"releaseYear":{"year":2008},"plot":{"plotText":{"plainText":"A chemistry teacher turns bad."}},"primaryImage":{"url":"https://art/breaking.jpg"},"runtime":{"seconds":2700},"genres":{"genres":[{"text":"Drama"}]}}}}]}}}"#;

    fn server() -> FakeServer {
        FakeServer::start(|req: &Request| match req.path.as_str() {
            "/" => Response::json(GRAPHQL_FIXTURE),
            _ => Response::error(404, "not found"),
        })
    }

    #[test]
    fn imdb_parses_recorded_fixture_and_maps_title_type() {
        let server = server();
        let source = ImdbSource::new(server.url(), test_client());
        let cancel = Cancel::background();
        // Movie want: the movie Title answers as a movie.
        let movie_q = Query {
            kind: Kind::MOVIE,
            title: "Inception".to_string(),
            ..Query::default()
        };
        let movie = source
            .find(&cancel, &movie_q)
            .expect("find")
            .expect("movie media");
        assert_eq!(movie.source, "imdb");
        assert_eq!(movie.source_id, "tt1375666");
        assert_eq!(movie.title, "Inception");
        assert_eq!(movie.year, 2010);
        assert_eq!(movie.kind, Kind::MOVIE);
        assert_eq!(movie.length, "148 min");
        assert!(!movie.synopsis.is_empty());
        assert_eq!(movie.artwork, "https://art/inception.jpg");
        assert!(movie.genres.contains("Sci-Fi"));
        assert_eq!(
            movie.movie_url,
            "https://www.imdb.com/title/tt1375666/"
        );
        // Series want: the series Title answers as a series.
        let series_q = Query {
            kind: Kind::SERIES,
            title: "Breaking Bad".to_string(),
            ..Query::default()
        };
        let series = source
            .find(&cancel, &series_q)
            .expect("find")
            .expect("series media");
        assert_eq!(series.source_id, "tt0903747");
        assert_eq!(series.kind, Kind::SERIES);
        assert_eq!(series.year, 2008);
        // Search rows keep both kinds.
        let all = source
            .find_all(
                &cancel,
                &Query {
                    kind: Kind::SERIES,
                    title: "Breaking Bad".to_string(),
                    ..Query::default()
                },
            )
            .expect("find_all");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, Kind::SERIES);
        let movies = source
            .find_all(&cancel, &movie_q)
            .expect("find_all movies");
        assert_eq!(movies.len(), 1);
        assert_eq!(movies[0].kind, Kind::MOVIE);
        assert_eq!(source.path(), SearchPath::Regular);
    }

    /// LIVE: the real IMDb GraphQL endpoint answers `Dune Part Two` with
    /// year, poster and synopsis. Real network, real input.
    #[test]
    #[ignore = "live network; run with --ignored"]
    fn imdb_live_dune_round_trip() {
        use std::time::Duration;
        let source = ImdbSource::new("", test_client());
        let cancel = Cancel::with_deadline(Duration::from_secs(25));
        let q = Query {
            kind: Kind::MOVIE,
            title: "Dune Part Two".to_string(),
            year: 2024,
            ..Query::default()
        };
        let media = source
            .find(&cancel, &q)
            .expect("live Find")
            .expect("the live endpoint must answer Dune Part Two");
        assert!(!media.title.is_empty(), "media = {media:?}");
        assert_eq!(media.year, 2024, "media = {media:?}");
        assert!(
            media.artwork.starts_with("https://"),
            "artwork = {:?}",
            media.artwork
        );
        assert!(
            !media.synopsis.is_empty(),
            "the live synopsis must be non-empty"
        );
    }
}
