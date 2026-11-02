//! `metadata::animeschedule` -- AnimeSchedule as a keyless anime source.
//!
//! GET `https://animeschedule.net/api/v3/anime?q=<title>`: one request per
//! search, no episode table. The wire carries `title`/`premier`/`episodes`/
//! `description`/`season.title`/`imageVersionRoute` plus genre and poster
//! fields whose exact names drifted across API revisions, so the parser is
//! deliberately tolerant: it accepts an array or an object holding the array
//! under `anime`/`data`/`results`, reads the title/year/poster from any of
//! the known spellings, and prefixes a bare image route with the site root.
//!
//! Behaviours that are contract:
//! * the search pick is title-validated with [`pick_best_show`]: a
//!   low-confidence best is a definitive miss, never a wrong show; year
//!   never gates, it only prefers at equal title strength.
//! * `find_all` is the same one-GET answer, each entry clearing the floor
//!   as its own pickable row with an empty episode table; the entry's own
//!   form decides the kind (`movie` is a movie, everything else a series).

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::parse::date_year;
use metadata_core::types::{Kind, Media, Query, join_genres, normalize_url};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped, drain_and_close};
use ureq::http::StatusCode;

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::resolver::SearchPath;
use crate::urlutil::query_escape;

/// The provider's default host (the `/api/v3` root is part of the paths
/// below so a test server takes the bare host).
pub const ANIMESCHEDULE_DEFAULT_BASE: &str = "https://animeschedule.net";

/// `animescheduleSource`.
pub struct AnimescheduleSource {
    base: String,
    client: HttpClient,
}

impl AnimescheduleSource {
    /// A source pointed at `base` (`""` means [`ANIMESCHEDULE_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> AnimescheduleSource {
        AnimescheduleSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            ANIMESCHEDULE_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for AnimescheduleSource {
    fn default() -> AnimescheduleSource {
        AnimescheduleSource::new("", HttpClient::new(Duration::ZERO))
    }
}

/// One schedule entry, read tolerantly from `serde_json::Value`: every field
/// tries each known spelling, and a missing field is simply empty.
#[derive(Debug, Clone, Default)]
struct ScheduleEntry {
    id: String,
    title: String,
    aliases: Vec<String>,
    year: i32,
    synopsis: String,
    poster: String,
    genres: String,
    kind: Kind,
    page_url: String,
}

fn string_field(value: &serde_json::Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(found) = value.get(*key)
            && let Some(s) = found.as_str()
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
    }
    String::new()
}

fn int_field(value: &serde_json::Value, keys: &[&str]) -> i32 {
    for key in keys {
        if let Some(found) = value.get(*key) {
            if let Some(n) = found.as_i64()
                && n > 0
            {
                return n as i32;
            }
            if let Some(s) = found.as_str() {
                let digits: String = s
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if digits.len() >= 4
                    && let Ok(year) = digits[..4].parse::<i32>()
                {
                    return year;
                }
            }
        }
    }
    0
}

fn genres_field(value: &serde_json::Value) -> String {
    for key in ["genres", "genre", "tags", "themes"] {
        let Some(found) = value.get(key) else {
            continue;
        };
        if let Some(arr) = found.as_array() {
            let mut names: Vec<String> = Vec::new();
            for item in arr {
                if let Some(s) = item.as_str() {
                    if !s.trim().is_empty() {
                        names.push(s.trim().to_string());
                    }
                } else if let Some(obj) = item.as_object() {
                    for gkey in ["name", "text", "title", "slug"] {
                        if let Some(s) = obj.get(gkey).and_then(|v| v.as_str())
                            && !s.trim().is_empty()
                        {
                            names.push(s.trim().to_string());
                            break;
                        }
                    }
                }
            }
            if !names.is_empty() {
                return join_genres(&names);
            }
        } else if let Some(s) = found.as_str()
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
    }
    String::new()
}

fn poster_field(base: &str, value: &serde_json::Value) -> String {
    // `imageVersionRoute` is a versioned route (`/images/...`); `poster`,
    // `cover`, `image` and their nested spellings carry the same picture
    // under other revisions. A bare route is prefixed with the site root.
    for key in [
        "poster",
        "posterImage",
        "cover",
        "coverImage",
        "image",
        "imageUrl",
        "image_url",
        "imageVersionRoute",
        "thumbnail",
    ] {
        let Some(found) = value.get(key) else {
            continue;
        };
        if let Some(s) = found.as_str()
            && !s.trim().is_empty()
        {
            let s = s.trim();
            if s.starts_with("http://") || s.starts_with("https://") {
                return normalize_url(s);
            }
            if s.starts_with('/') {
                return normalize_url(&format!("{base}{s}"));
            }
            return normalize_url(&format!("{base}/{s}"));
        }
        if let Some(obj) = found.as_object() {
            for inner in ["original", "large", "url", "src"] {
                if let Some(s) = obj.get(inner).and_then(|v| v.as_str())
                    && !s.trim().is_empty()
                {
                    let s = s.trim();
                    if s.starts_with("http://") || s.starts_with("https://") {
                        return normalize_url(s);
                    }
                    if s.starts_with('/') {
                        return normalize_url(&format!("{base}{s}"));
                    }
                    return normalize_url(&format!("{base}/{s}"));
                }
            }
        }
    }
    // Nested `images: { jpg: { image_url } }` (Jikan-shaped mirrors).
    if let Some(images) = value.get("images")
        && let Some(obj) = images.as_object()
    {
        for section in ["jpg", "webp"] {
            if let Some(inner) = obj.get(section)
                && let Some(map) = inner.as_object()
            {
                for k in ["large_image_url", "image_url", "small_image_url"] {
                    if let Some(s) = map.get(k).and_then(|v| v.as_str())
                        && !s.trim().is_empty()
                    {
                        return normalize_url(s.trim());
                    }
                }
            }
        }
    }
    String::new()
}

fn year_field(value: &serde_json::Value) -> i32 {
    let direct = int_field(value, &["year", "seasonYear", "releaseYear"]);
    if direct > 0 {
        return direct;
    }
    for key in ["premier", "premiere", "aired", "startDate", "releaseDate", "date"] {
        if let Some(found) = value.get(key) {
            if let Some(n) = found.as_i64()
                && n > 1900
                && n < 2200
            {
                return n as i32;
            }
            if let Some(s) = found.as_str() {
                let year = date_year(s);
                if year > 0 {
                    return year;
                }
                let digits: String = s
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if digits.len() >= 4
                    && let Ok(year) = digits[..4].parse::<i32>()
                    && year > 1900
                {
                    return year;
                }
            }
        }
    }
    // `season: { title: "Fall 2022" }` carries the year as a last resort.
    if let Some(season) = value.get("season") {
        if let Some(s) = season.as_str() {
            let year = date_year(s);
            if year > 0 {
                return year;
            }
        } else if let Some(obj) = season.as_object() {
            for k in ["title", "name", "season", "year"] {
                if let Some(s) = obj.get(k).and_then(|v| v.as_str()) {
                    let year = date_year(s);
                    if year > 0 {
                        return year;
                    }
                }
                if let Some(n) = obj.get(k).and_then(|v| v.as_i64())
                    && n > 1900
                    && n < 2200
                {
                    return n as i32;
                }
            }
        }
    }
    0
}

fn kind_field(value: &serde_json::Value) -> Kind {
    for key in ["type", "format", "subtype", "kind", "mediaType"] {
        if let Some(found) = value.get(key)
            && let Some(s) = found.as_str()
            && s.trim().eq_ignore_ascii_case("movie")
        {
            return Kind::MOVIE;
        }
    }
    Kind::SERIES
}

fn entry_from_value(base: &str, value: &serde_json::Value) -> Option<ScheduleEntry> {
    let title = string_field(
        value,
        &["title", "name", "romaji", "english", "title_english"],
    );
    if title.is_empty() {
        return None;
    }
    let id = string_field(value, &["id", "malId", "mal_id", "slug"]);
    let id = if id.is_empty() { title.clone() } else { id };
    let mut aliases = Vec::new();
    for key in [
        "title",
        "name",
        "romaji",
        "english",
        "title_english",
        "preferredTitle",
        "synonyms",
    ] {
        if let Some(found) = value.get(key) {
            if let Some(s) = found.as_str() {
                if !s.trim().is_empty() && s.trim() != title {
                    aliases.push(s.trim().to_string());
                }
            } else if let Some(arr) = found.as_array() {
                for item in arr {
                    if let Some(s) = item.as_str()
                        && !s.trim().is_empty()
                    {
                        aliases.push(s.trim().to_string());
                    }
                }
            }
        }
    }
    // Real shape nests the alternate spellings under `names:{romaji,english,
    // native}`; without them a romaji/english query never corroborates.
    if let Some(names) = value.get("names").and_then(|v| v.as_object()) {
        for key in ["romaji", "english", "native", "preferred"] {
            if let Some(s) = names.get(key).and_then(|v| v.as_str())
                && !s.trim().is_empty()
                && s.trim() != title
                && !aliases.iter().any(|a| a == s.trim())
            {
                aliases.push(s.trim().to_string());
            }
        }
    }
    let synopsis = {
        let raw = string_field(value, &["description", "synopsis", "plot", "overview"]);
        cap_synopsis(&strip_html(&raw))
    };
    let page = string_field(value, &["url", "link", "siteUrl"]);
    let page_url = if page.is_empty() {
        normalize_url(&format!("{base}/anime/{}/", id.trim()))
    } else {
        normalize_url(&page)
    };
    Some(ScheduleEntry {
        id,
        title,
        aliases,
        year: year_field(value),
        synopsis,
        poster: poster_field(base, value),
        genres: genres_field(value),
        kind: kind_field(value),
        page_url,
    })
}

fn schedule_media(entry: &ScheduleEntry) -> Media {
    Media {
        source: "animeschedule".to_string(),
        source_id: entry.id.trim().to_string(),
        kind: entry.kind,
        title: entry.title.clone(),
        year: entry.year,
        artwork: entry.poster.clone(),
        synopsis: entry.synopsis.clone(),
        genres: entry.genres.clone(),
        length: String::new(),
        show_url: entry.page_url.clone(),
        episode_url: String::new(),
        movie_url: String::new(),
        trailer_url: String::new(),
        episodes: Vec::new(),
        fetched_at: crate::timefmt::now_rfc3339(),
    }
}

impl AnimescheduleSource {
    /// A 200 body carrying a provider error or a null payload is transient,
    /// never a definitive miss: `Ok(empty)` here would persist a 30-day
    /// negative and never mark the leg failed.
    fn decode_items(value: serde_json::Value) -> Result<Vec<serde_json::Value>, Error> {
        if let Some(obj) = value.as_object() {
            if let Some(errors) = obj.get("errors")
                && !errors.is_null()
                && !errors.as_array().is_some_and(|arr| arr.is_empty())
                && !errors.as_object().is_some_and(|obj| obj.is_empty())
            {
                return Err(Error::other(
                    "metadata: animeschedule error document (transient)",
                ));
            }
            for key in ["anime", "data", "results", "items"] {
                if let Some(found) = obj.get(key)
                    && found.is_null()
                {
                    return Err(Error::other(
                        "metadata: animeschedule null payload (transient)",
                    ));
                }
            }
        }
        if value.is_null() {
            return Err(Error::other(
                "metadata: animeschedule null payload (transient)",
            ));
        }
        let items: Vec<serde_json::Value> = if let Some(arr) = value.as_array() {
            arr.clone()
        } else if let Some(obj) = value.as_object() {
            ["anime", "data", "results", "items"]
                .iter()
                .find_map(|k| obj.get(*k).and_then(|v| v.as_array()).cloned())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(items)
    }

    fn search_rows(&self, cancel: &Cancel, title: &str) -> Result<Vec<ScheduleEntry>, Error> {
        if title.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/api/v3/anime?q={}", self.base_url(), query_escape(title));
        let response = self.client.get(cancel, &url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(Vec::new());
        }
        let value: serde_json::Value = decode_capped(response)?;
        let items = Self::decode_items(value)?;
        let mut out = Vec::with_capacity(items.len());
        for item in &items {
            if let Some(entry) = entry_from_value(self.base_url(), item) {
                out.push(entry);
            }
        }
        Ok(out)
    }

    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<ScheduleEntry>, Error> {
        let rows = self.search_rows(cancel, &q.title)?;
        let mut cands: Vec<ShowCand> = Vec::with_capacity(rows.len());
        let mut years: Vec<i32> = Vec::with_capacity(rows.len());
        for entry in &rows {
            cands.push(ShowCand {
                name: entry.title.clone(),
                weight: 0,
                episode_count: 0,
                aliases: entry.aliases.clone(),
            });
            years.push(entry.year);
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
        Ok(rows.get(take).cloned())
    }
}

impl crate::resolver::Source for AnimescheduleSource {
    fn name(&self) -> &str {
        "animeschedule"
    }

    fn path(&self) -> SearchPath {
        SearchPath::Anime
    }

    /// `Find`: series only through the resolver (movies never reach the anime
    /// legs there, and a series-only source self-gates a ruled-out kind
    /// before touching the wire). The list endpoint carries no table, so a
    /// series answer rides empty and the chain falls through on an episode
    /// query; the search rows still offer the pick.
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::MOVIE || q.title.is_empty() {
            return Ok(None);
        }
        let Some(picked) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        Ok(Some(schedule_media(&picked)))
    }

    /// The manual search's rows: the same one-GET answer, each entry clearing
    /// the floor as its own pickable row with an empty episode table. The
    /// entry's own form decides the kind.
    fn find_all(&self, cancel: &Cancel, q: &Query) -> Result<Vec<Media>, Error> {
        if q.title.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Media> = Vec::new();
        for entry in self.search_rows(cancel, &q.title)? {
            let cand = ShowCand {
                name: entry.title.clone(),
                weight: 0,
                episode_count: 0,
                aliases: entry.aliases.clone(),
            };
            if pick_best_show(&q.title, std::slice::from_ref(&cand)).is_none() {
                continue;
            }
            out.push(schedule_media(&entry));
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
    use crate::testsupport::{FakeServer, Request, Response, http_client};

    /// Recorded `/api/v3/anime` fixture: array shape with the documented
    /// fields plus genre/poster spellings the tolerant parser must accept.
    const SCHEDULE_FIXTURE: &str = r#"[{"id":"21","title":"One Piece","premier":"1999-10-20","episodes":1000,"description":"Pirates chase the ultimate treasure.","season":{"title":"Fall 1999"},"imageVersionRoute":"/images/one-piece.jpg","genres":[{"name":"Action"},{"name":"Adventure"}]},{"id":"5114","title":"Fullmetal Alchemist: Brotherhood","premier":"2009-04-05","episodes":64,"description":"Two brothers seek the stone.","season":{"title":"Spring 2009"},"imageVersionRoute":"https://cdn.animeschedule.net/images/fma.jpg","genres":["Action","Drama"]}]"#;

    fn server() -> FakeServer {
        FakeServer::start(|req: &Request| match req.path.as_str() {
            "/api/v3/anime" => Response::json(SCHEDULE_FIXTURE),
            _ => Response::error(404, "not found"),
        })
    }

    #[test]
    fn animeschedule_parses_recorded_fixture_with_tolerant_fields() {
        let server = server();
        let source = AnimescheduleSource::new(server.url(), http_client());
        let cancel = Cancel::background();
        let q = Query {
            kind: Kind::SERIES,
            title: "One Piece".to_string(),
            ..Query::default()
        };
        let media = source
            .find(&cancel, &q)
            .expect("find")
            .expect("schedule media");
        assert_eq!(media.source, "animeschedule");
        assert_eq!(media.source_id, "21");
        assert_eq!(media.title, "One Piece");
        assert_eq!(media.year, 1999);
        assert_eq!(media.kind, Kind::SERIES);
        assert!(!media.synopsis.is_empty());
        assert!(
            media.artwork.starts_with("https://"),
            "poster = {:?}",
            media.artwork
        );
        assert!(media.artwork.contains("one-piece.jpg"));
        assert!(media.genres.contains("Action"));
        assert_eq!(source.path(), SearchPath::Anime);
        // The second entry's absolute poster URL and string-genre shape.
        let rows = source
            .find_all(&cancel, &q)
            .expect("find_all");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_id, "21");
        let other = source
            .find_all(
                &cancel,
                &Query {
                    kind: Kind::SERIES,
                    title: "Fullmetal Alchemist: Brotherhood".to_string(),
                    ..Query::default()
                },
            )
            .expect("find_all brotherhood");
        assert_eq!(other.len(), 1);
        assert_eq!(
            other[0].artwork, "https://cdn.animeschedule.net/images/fma.jpg"
        );
    }

    /// LIVE: the real AnimeSchedule endpoint answers `Frieren` with year,
    /// poster and description. Real network, real input.
    #[test]
    #[ignore = "live network; run with --ignored"]
    fn animeschedule_live_frieren_round_trip() {
        use std::time::Duration;
        let source = AnimescheduleSource::new("", http_client());
        let cancel = Cancel::with_deadline(Duration::from_secs(25));
        let q = Query {
            kind: Kind::SERIES,
            title: "Frieren".to_string(),
            year: 2023,
            ..Query::default()
        };
        let media = source
            .find(&cancel, &q)
            .expect("live Find")
            .expect("the live endpoint must answer Frieren");
        assert!(!media.title.is_empty(), "media = {media:?}");
        assert_eq!(media.year, 2023, "media = {media:?}");
        assert!(
            media.artwork.starts_with("https://"),
            "artwork = {:?}",
            media.artwork
        );
        assert!(
            !media.synopsis.is_empty(),
            "the live description must be non-empty"
        );
    }
}
