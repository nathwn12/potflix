//! `metadata::types` -- mirrors `go\internal\metadata\types.go`.
//!
//! The shared record types (resolved title, provider result, cache entry,
//! options): the [`Query`] the poll loop derives from a filename, the [`Media`]
//! record a provider returns and the cache stores, and the user-facing
//! [`Resolution`].
//!
//! The JSON tags on [`Episode`], [`Button`] and [`Media`] ARE the on-disk cache
//! wire format (`cache.go`), so they are copied verbatim from the Go structs:
//! camelCase `sourceId` / `fetchedAt` / `showUrl` / `episodeUrl` / `movieUrl` /
//! `trailerUrl`, lowercase `season` / `number` / `title` / `artwork` /
//! `synopsis` / `url` / `length`, `label` / `url` on [`Button`], and Go's
//! `omitempty` set -- present on year/artwork/synopsis/genres/length/showUrl/
//! episodeUrl/movieUrl/trailerUrl/episodes, absent on source/sourceId/kind/
//! title/fetchedAt. One wrong tag silently invalidates every cached record on
//! the owner's machine.
//!
//! [`Kind`] is a plain JSON integer (Go's type has no custom marshaler, so the
//! fixtures carry `"kind":1`), which `#[serde(transparent)]` preserves.
//!
//! [`Media::fetched_at`] is RFC3339 text -- exactly the bytes Go's `time.Time`
//! writes -- kept as a `String` so a cached record round-trips byte for byte
//! and this crate needs no date crate (the workspace pins none; `repair::time`
//! owns the workspace's calendar arithmetic).
//!
//! Deliberately NOT ported here: Go's `Source` interface and `Options`. Both
//! carry a `context.Context` and the provider set, which belong to the resolver
//! (`metadata::resolver`) and the transport (`metadata-http::cancel`); this
//! crate stays network-free.

use serde::{Deserialize, Serialize};

/// `Kind` classifies a query or media entry. Mirrors Go's `type Kind int`, and
/// encodes as a plain JSON integer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Kind(i32);

impl Kind {
    /// Go's `KindUnknown`.
    pub const UNKNOWN: Kind = Kind(0);
    /// Go's `KindSeries`.
    pub const SERIES: Kind = Kind(1);
    /// Go's `KindMovie`.
    pub const MOVIE: Kind = Kind(2);

    /// Wraps a raw wire value (a cache record's `"kind"`).
    pub const fn new(raw: i32) -> Kind {
        Kind(raw)
    }

    /// The raw wire value.
    pub const fn value(self) -> i32 {
        self.0
    }
}

/// `Query` is a parsed identification request. Go gives it no JSON tags and
/// nothing persists it, so it is deliberately not serializable here either.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Query {
    pub kind: Kind,
    /// Cleaned series/movie name.
    pub title: String,
    /// 0 if unknown.
    pub year: i32,
    /// 0 if unknown. Go's `Query.Season` is an `int` (`types.go:27`), i.e. 64-bit,
    /// and the season-folder capture is unbounded (`context.go:21`), so a
    /// `Season <N>` folder number above `i32::MAX` must survive whole.
    pub season: i64,
    pub episode: i32,
    /// True when [`Query::episode`] is an absolute anime number.
    pub absolute: bool,
}

/// `Episode` is a single resolved episode title.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Episode {
    #[serde(default)]
    pub season: i32,
    #[serde(default)]
    pub number: i32,
    #[serde(default)]
    pub title: String,
    /// Episode still/poster URL, `""` if unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artwork: String,
    /// Plain-text episode summary, `""` if unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub synopsis: String,
    /// Episode page URL (TVmaze `episode.url`, AniList streaming url), `""` if
    /// unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Per-episode length ("45 min"), `""` if unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub length: String,
}

/// `Button` is one card page button (label + https page URL).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Button {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub url: String,
}

/// `Media` is a cached provider record for one show/season or movie.
///
/// Field order matches the Go declaration so a re-written record keeps Go's key
/// order (`bundle` determinism, `cache` byte-identical refresh).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Media {
    /// "tvmaze"|"anilist"|"jikan"|"itunes"|"wikipedia".
    #[serde(default)]
    pub source: String,
    #[serde(rename = "sourceId", default)]
    pub source_id: String,
    #[serde(default)]
    pub kind: Kind,
    /// Canonical series/movie title.
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub year: i32,
    /// Series/movie-level poster. Per-episode art lives on [`Episode`]: the
    /// resolver ALWAYS uses the series/movie poster (episode stills look
    /// unofficial).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artwork: String,
    /// Series/movie-level summary.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub synopsis: String,
    /// "Drama . Crime", `""` if unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub genres: String,
    /// Show/movie length ("45 min", "2h 19m"), `""` if unknown.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub length: String,
    /// Series/show page (TVmaze show.url, MAL/AniList), `""` if unknown.
    #[serde(rename = "showUrl", default, skip_serializing_if = "String::is_empty")]
    pub show_url: String,
    /// TVmaze matched-episode page, `""` if unknown.
    #[serde(
        rename = "episodeUrl",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub episode_url: String,
    /// iTunes trackViewUrl, `""` if unknown.
    #[serde(rename = "movieUrl", default, skip_serializing_if = "String::is_empty")]
    pub movie_url: String,
    /// iTunes previewUrl, `""` if unknown.
    #[serde(
        rename = "trailerUrl",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub trailer_url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<Episode>,
    /// The provider's TOTAL episode count when its search row carries one but
    /// no table (AniList's `episodes` integer). `0` when unknown; additive and
    /// cache-compatible, so an old record loads with no count.
    #[serde(
        rename = "episodeCount",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub episode_count: i32,
    /// RFC3339 UTC, as Go's `time.Time` writes it.
    #[serde(rename = "fetchedAt", default)]
    pub fetched_at: String,
}

/// `Resolution` is the user-facing answer for a [`Query`]. No JSON tags in Go.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Resolution {
    pub series: String,
    pub episode: String,
    /// Go's `Resolution.Season` is an `int` (`types.go:76`), so a season number
    /// above `i32::MAX` reaches the caller unchanged.
    pub season: i64,
    pub number: i32,
    pub found: bool,
    /// Series/movie poster only, else `""`.
    pub artwork: String,
    /// Plain-text one-line summary, capped, `""` if unknown.
    pub synopsis: String,
    /// "Drama . Crime", `""` if unknown.
    pub genres: String,
    /// "45 min" / "2h 19m", `""` if unknown.
    pub length: String,
    pub year: i32,
    pub is_movie: bool,
    /// Max 2, https-only page buttons.
    pub buttons: Vec<Button>,
}

/// `omitempty` for a zero integer: Go omits `year` when it is 0.
fn is_zero(v: &i32) -> bool {
    *v == 0
}

/// `joinGenres` joins non-blank genre names with " . " (U+00B7).
pub fn join_genres(gs: &[String]) -> String {
    let out: Vec<&str> = gs
        .iter()
        .map(|g| g.trim())
        .filter(|g| !g.is_empty())
        .collect();
    out.join(" \u{00b7} ")
}

/// `normalizeURL` forces provider page URLs to https and drops non-http(s).
/// `""` when empty or not http(s).
pub fn normalize_url(raw: &str) -> String {
    let u = raw.trim();
    if u.is_empty() {
        return String::new();
    }
    let u = match u.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => u.to_string(),
    };
    match u.strip_prefix("https://") {
        // Go requires more than the scheme itself.
        Some(rest) if !rest.is_empty() => u,
        _ => String::new(),
    }
}

/// `runtimeLength` formats whole minutes as "45 min", `""` when <= 0.
pub fn runtime_length(mins: i32) -> String {
    if mins <= 0 {
        return String::new();
    }
    format!("{mins} min")
}

/// `millisLength` formats iTunes `trackTimeMillis` as "2h 19m" / "2h" /
/// "45 min"; the millis round half up to the nearest minute.
pub fn millis_length(millis: i64) -> String {
    if millis <= 0 {
        return String::new();
    }
    let total_min = (millis + 30000) / 60000;
    if total_min <= 0 {
        return String::new();
    }
    let h = total_min / 60;
    let m = total_min % 60;
    if h > 0 && m > 0 {
        format!("{h}h {m}m")
    } else if h > 0 {
        format!("{h}h")
    } else {
        format!("{total_min} min")
    }
}
