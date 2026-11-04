//! `metadata::wikipedia` -- mirrors `go\internal\metadata\wikipedia.go`.
//!
//! Wikipedia provider: the movie fallback immediately after iTunes, and the
//! artwork source. Movie-only, so a series key never reaches the wire.
//!
//! Two calls on the base path: `list=search` picks the top film hit, then
//! `prop=extracts|pageimages` yields the synopsis and, when one exists, the page
//! image (`thumbnail`, else `original`). MEASURED 2026-10-10 against the live
//! API: PageImages never returns film posters (they are fair-use), so a resolved
//! page with NO page image gets at most two best-effort same-host calls through
//! the SAME Action API -- `prop=images&imlimit=max` lists the article's files,
//! and the poster-like file is resolved to a direct URL via
//! `prop=imageinfo&iiprop=url`. `imlimit=max` is mandatory: the poster
//! "File:Spider-Man Brand New Day poster.jpg" is the 18th of 24 files, so the
//! default 10-file page hid it.
//!
//! # Error vs miss
//!
//! The Action API serves its error envelope with HTTP 200
//! (`{"error":{"code":"maxlag","info":"Waiting..."}}`). Without the explicit
//! check the empty `query` reads as a definitive miss and the resolver persists
//! a 30-day negative for a lookup that never happened. Transport failures and
//! decode failures are transient too. Every transient cause is sanitized: the
//! request URL carries the escaped owner title, so the message must never
//! contain it.
//!
//! # The one wire divergence
//!
//! Wikimedia's User-Agent policy wants a descriptive app identifier, and
//! `wikipedia_test.go` asserts `Potflix` case-sensitively, while
//! `metadata-http::HttpClient` sends the shared `potflix-metadata/1.0`. This
//! module therefore owns [`WikiClient`], which mirrors `http.go`'s
//! `fetchWithRetry` exactly -- same limiter, same bounded 429 retries with
//! backoff and jitter, same `Retry-After` bound, same `decode_capped` -- and
//! differs only in the header.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, SystemTime};

use metadata_core::types::{Kind, Media, Query, normalize_url};
use metadata_http::cancel::Cancel;
use metadata_http::{
    ACCEPT_JSON, DEFAULT_HTTP_TIMEOUT, MAX_429_RETRIES, decode_capped, drain_and_close,
    parse_retry_after, retry_wait_for_attempt, wait_for_host,
};
use serde::Deserialize;
use ureq::Body;
use ureq::http::{Response, StatusCode, Uri};
use ureq::tls::{TlsConfig, TlsProvider};

use crate::Error;
use crate::artwork::{cap_synopsis, first_non_empty, strip_html};
use crate::serdeutil::null_default;
use crate::urlutil::{path_escape, query_escape};

/// The provider's default host.
pub const WIKIPEDIA_DEFAULT_BASE: &str = "https://en.wikipedia.org";

/// `wikipediaUserAgent` identifies the app per the Wikimedia User-Agent policy
/// (descriptive app identifier, never a credential). Derived from this crate's
/// own version (the workspace version), so it can never go stale.
pub const WIKIPEDIA_USER_AGENT: &str = concat!(
    "Potflix/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/nathwn12/potflix; contact: potflix-app)"
);

/// The GET transport for Wikipedia: `http.go`'s wire shape with the Wikimedia
/// `User-Agent` instead of the shared one.
pub struct WikiClient {
    agent: ureq::Agent,
}

impl WikiClient {
    /// A client whose whole-request budget is `timeout`; zero takes
    /// [`DEFAULT_HTTP_TIMEOUT`], mirroring `newHTTPClient`.
    pub fn new(timeout: Duration) -> WikiClient {
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
                    .build(),
            )
            // Go's `client.Do` hands back every status; non-2xx is
            // `decode_capped`'s error and a 429 is the retry's trigger.
            .http_status_as_error(false)
            .build();
        WikiClient {
            agent: config.into(),
        }
    }

    /// `wikiGet`: a GET with the Wikimedia `User-Agent` and the shared
    /// bounded 429 retries (backoff + jitter, honoring `Retry-After`). GET is
    /// idempotent, so it retries; the URL is never logged and errors carry
    /// the status only.
    pub fn get(&self, cancel: &Cancel, url: &str) -> Result<Response<Body>, Error> {
        let uri: Uri = url.parse().map_err(|err: ureq::http::uri::InvalidUri| {
            Error::other(format!("metadata: invalid request URL: {err}"))
        })?;
        let mut attempt: u32 = 0;
        loop {
            let response = self.attempt(cancel, &uri)?;
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
            cancel.wait(wait).map_err(Error::Cancelled)?;
            attempt += 1;
        }
    }

    fn attempt(&self, cancel: &Cancel, uri: &Uri) -> Result<Response<Body>, Error> {
        wait_for_host(cancel, uri.host().unwrap_or_default()).map_err(Error::Cancelled)?;
        if let Some(reason) = cancel.err() {
            return Err(Error::Cancelled(reason));
        }
        self.agent
            .get(uri.clone())
            .header("User-Agent", WIKIPEDIA_USER_AGENT)
            .header("Accept", ACCEPT_JSON)
            .call()
            .map_err(|err| Error::other(format!("metadata: request failed: {err}")))
    }
}

impl Default for WikiClient {
    fn default() -> WikiClient {
        WikiClient::new(Duration::ZERO)
    }
}

/// `wikipediaSource`.
pub struct WikipediaSource {
    base: String,
    client: WikiClient,
}

impl WikipediaSource {
    /// A source pointed at `base` (`""` means [`WIKIPEDIA_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: WikiClient) -> WikipediaSource {
        WikipediaSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            WIKIPEDIA_DEFAULT_BASE
        } else {
            &self.base
        }
    }

    /// `wikiGet` on this source's client.
    fn wiki_get(&self, cancel: &Cancel, url: &str) -> Result<Response<Body>, Error> {
        self.client.get(cancel, url)
    }
}

impl Default for WikipediaSource {
    fn default() -> WikipediaSource {
        WikipediaSource::new("", WikiClient::new(Duration::ZERO))
    }
}

/// `wikiTransient` wraps a transient failure with the sentinel so the resolver's
/// honest counters stay honest. The cause is sanitized: the request URL (which
/// carries the escaped title) and any URL-like payload never reach the message.
pub(crate) fn wiki_transient(cause: impl fmt::Display) -> Error {
    Error::unavailable(format!(
        "metadata: wikipedia lookup unavailable ({}): {}",
        wiki_sanitize_cause(&cause.to_string()),
        crate::LOOKUP_UNAVAILABLE_MESSAGE
    ))
}

/// `wikiSanitizeCause`: a title-free diagnostic for `cause`. Transport messages
/// embed the request URL -- which carries the escaped search title -- so any
/// message containing an http(s) URL or `api.php` is redacted. URL-free
/// diagnostics (status, decode, context) pass through verbatim.
pub(crate) fn wiki_sanitize_cause(cause: &str) -> String {
    let message = cause.trim();
    if message.is_empty() {
        return "request failed (URL redacted)".to_string();
    }
    if message.contains("http://") || message.contains("https://") || message.contains("api.php") {
        return "request failed (URL redacted)".to_string();
    }
    message.to_string()
}

/// `wikiAPIError` is the Action API error envelope served with HTTP 200.
#[derive(Debug, Clone, Default, Deserialize)]
struct WikiApiError {
    #[serde(default, deserialize_with = "null_default")]
    code: String,
}

/// `wikiMissingBool` accepts the API's missing marker in either shape: the live
/// API serves `{"missing":""}` (string), while some fixtures use
/// `{"missing":true}` (bool). Any string value marks missing; bools follow their
/// value; null means present; absent means present.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WikiMissingBool(pub bool);

impl<'de> Deserialize<'de> for WikiMissingBool {
    fn deserialize<D>(deserializer: D) -> Result<WikiMissingBool, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct MissingVisitor;

        impl<'de> serde::de::Visitor<'de> for MissingVisitor {
            type Value = WikiMissingBool;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a missing marker: \"\" (string), true/false, or null")
            }

            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<WikiMissingBool, E> {
                Ok(WikiMissingBool(true))
            }

            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<WikiMissingBool, E> {
                Ok(WikiMissingBool(value))
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<WikiMissingBool, E> {
                Ok(WikiMissingBool(false))
            }

            fn visit_none<E: serde::de::Error>(self) -> Result<WikiMissingBool, E> {
                Ok(WikiMissingBool(false))
            }

            fn visit_some<D: serde::Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<WikiMissingBool, D::Error> {
                deserializer.deserialize_any(self)
            }
        }

        deserializer.deserialize_any(MissingVisitor)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiSearchHit {
    #[serde(default, deserialize_with = "null_default")]
    title: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiSearchQuery {
    #[serde(default, deserialize_with = "null_default")]
    search: Vec<WikiSearchHit>,
}

/// `wikiSearchResp` is `GET w/api.php?action=query&list=search`.
#[derive(Debug, Clone, Default, Deserialize)]
struct WikiSearchResp {
    #[serde(default)]
    error: Option<WikiApiError>,
    #[serde(default, deserialize_with = "null_default")]
    query: WikiSearchQuery,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImageSource {
    #[serde(default, deserialize_with = "null_default")]
    source: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiDetailPage {
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default)]
    missing: WikiMissingBool,
    #[serde(default, deserialize_with = "null_default")]
    extract: String,
    #[serde(default, deserialize_with = "null_default")]
    thumbnail: WikiImageSource,
    #[serde(default, deserialize_with = "null_default")]
    original: WikiImageSource,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiDetailQuery {
    #[serde(default, deserialize_with = "null_default")]
    pages: HashMap<String, WikiDetailPage>,
}

/// `wikiDetailResp` is `GET w/api.php?action=query&prop=extracts|pageimages`.
#[derive(Debug, Clone, Default, Deserialize)]
struct WikiDetailResp {
    #[serde(default)]
    error: Option<WikiApiError>,
    #[serde(default, deserialize_with = "null_default")]
    query: WikiDetailQuery,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImageEntry {
    #[serde(default, deserialize_with = "null_default")]
    title: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImagesPage {
    #[serde(default, deserialize_with = "null_default")]
    images: Vec<WikiImageEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImagesQuery {
    #[serde(default, deserialize_with = "null_default")]
    pages: HashMap<String, WikiImagesPage>,
}

/// `wikiImagesResp` is `GET w/api.php?action=query&prop=images`.
#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImagesResp {
    #[serde(default, deserialize_with = "null_default")]
    query: WikiImagesQuery,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImageInfoEntry {
    #[serde(default, deserialize_with = "null_default")]
    url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImageInfoPage {
    #[serde(default)]
    missing: WikiMissingBool,
    #[serde(default, deserialize_with = "null_default")]
    imageinfo: Vec<WikiImageInfoEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImageInfoQuery {
    #[serde(default, deserialize_with = "null_default")]
    pages: HashMap<String, WikiImageInfoPage>,
}

/// `wikiImageInfoResp` is `GET w/api.php?action=query&prop=imageinfo&iiprop=url`.
#[derive(Debug, Clone, Default, Deserialize)]
struct WikiImageInfoResp {
    #[serde(default, deserialize_with = "null_default")]
    query: WikiImageInfoQuery,
}

/// `wikiDisambiguationKinds`: the trailing parentheticals Wikipedia appends to a
/// page title to tell same-named works apart (`wikipedia.go` has no equivalent:
/// the Go era stored the raw page title, so the card showed `Dune (2021 film)`
/// and the movie line doubled the year as `Dune (2021 film) (2021)`).
const WIKI_DISAMBIGUATION_KINDS: [&str; 17] = [
    "film",
    "movie",
    "tv series",
    "television series",
    "tv program",
    "tv show",
    "series",
    "miniseries",
    "limited series",
    "anime",
    "novel",
    "book",
    "video game",
    "song",
    "album",
    "play",
    "musical",
];

/// `wikiStripDisambiguation`: drops a trailing Wikipedia-style disambiguation
/// parenthetical — `(film)`, `(2021 film)`, `(TV series)` — so the card carries
/// the bare title. ONLY a trailing parenthetical whose content is an optional
/// 4-digit year plus one of [`WIKI_DISAMBIGUATION_KINDS`] (case-insensitive) is
/// stripped; anything else (`Nymphomaniac (Director's Cut)`, `Heat (1995)`)
/// survives untouched, because a real title must never be eaten. The year is
/// derived separately by [`wiki_year`], which keeps reading the RAW page title,
/// so this changes only the title string, never the year.
pub(crate) fn strip_disambiguation(title: &str) -> String {
    let trimmed = title.trim();
    if !trimmed.ends_with(')') {
        return trimmed.to_string();
    }
    let Some(open) = trimmed.rfind('(') else {
        return trimmed.to_string();
    };
    let inner = trimmed[open + 1..trimmed.len() - 1].trim();
    let mut rest = inner;
    // An optional 4-digit year prefix (`2021 film`); a bare year with no kind
    // word (`Heat (1995)`) is NOT disambiguation by this convention.
    if rest.len() > 4
        && rest.as_bytes()[..4].iter().all(|b| b.is_ascii_digit())
        && rest.as_bytes()[4].is_ascii_whitespace()
    {
        rest = rest[4..].trim_start();
    }
    if !WIKI_DISAMBIGUATION_KINDS.contains(&rest.to_lowercase().as_str()) {
        return trimmed.to_string();
    }
    let stem = trimmed[..open].trim();
    if stem.is_empty() {
        return trimmed.to_string();
    }
    stem.to_string()
}

/// `wikiPosterExtRank` ranks a plausible poster photo extension: `.jpg`/`.jpeg`
/// first, then `.png`, then `.webp`. Anything else is implausible for a film
/// poster and rejected.
fn wiki_poster_ext_rank(lower: &str) -> Option<i32> {
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        return Some(0);
    }
    if lower.ends_with(".png") {
        return Some(1);
    }
    if lower.ends_with(".webp") {
        return Some(2);
    }
    None
}

/// `wikiPosterBlocked`: a lowercased file name that looks like documentation
/// about posters rather than a poster itself.
fn wiki_poster_blocked(lower: &str) -> bool {
    ["guideline", "logo", "list", "sheet", "style"]
        .iter()
        .any(|bad| lower.contains(bad))
}

/// `wikiPosterPlausible`: whether "poster" appears as a standalone word (plural
/// "posters" allowed), not as part of a longer word such as "posterize". The
/// check scans every "poster" occurrence for non-letter boundaries; one
/// plausible occurrence suffices.
fn wiki_poster_plausible(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        let Some(offset) = find_bytes(&bytes[i..], b"poster") else {
            return false;
        };
        let start = i + offset;
        let before_ok = start == 0 || !is_ascii_letter(bytes[start - 1]);
        let after = start + 6;
        // Go's switch: at or past the end, or a non-letter, both bound the word;
        // a plural "posters" counts only when its own trailing `s` is bounded.
        let after_ok = after >= bytes.len()
            || !is_ascii_letter(bytes[after])
            || ((bytes[after] == b's' || bytes[after] == b'S')
                && (after + 1 >= bytes.len() || !is_ascii_letter(bytes[after + 1])));
        if before_ok && after_ok {
            return true;
        }
        i = start + 1;
    }
    false
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_ascii_letter(byte: u8) -> bool {
    byte.is_ascii_alphabetic()
}

/// `wikiPickPoster`: the poster-like `File:` title from an article image list,
/// or `""`. Deliberately strict -- a wrong poster is worse than none. First a
/// file with a standalone "poster" word wins (ranked by photo extension); when
/// no such file exists, a file named exactly like the article page (e.g.
/// `File:Ready Player One (film).png` on the `Ready Player One (film)` page)
/// is the infobox lead image, which for a film article IS the poster --
/// MEASURED 2026-10-15: PageImages serves no page image for that article
/// (fair-use posters are excluded), and the literal-word rule dropped the only
/// poster file, so the card fell back to `logo` while `Backrooms (film)` rode
/// its `*poster.jpg` file. The exact-title fallback carries the same photo
/// extension and blocked-word guards, so a logo or cast photo never qualifies.
pub(crate) fn wiki_pick_poster(titles: &[String], page: &str) -> String {
    let mut best = String::new();
    let mut best_rank = 0;
    let mut found = false;
    for title in titles {
        let name = title.trim();
        if name.is_empty() {
            continue;
        }
        let lower = name.to_lowercase();
        if !lower.contains("poster") {
            continue;
        }
        if wiki_poster_blocked(&lower) {
            continue;
        }
        if !wiki_poster_plausible(&lower) {
            continue;
        }
        let Some(rank) = wiki_poster_ext_rank(&lower) else {
            continue;
        };
        if !found || rank < best_rank {
            best = name.to_string();
            best_rank = rank;
            found = true;
        }
    }
    if found {
        return best;
    }
    // No "poster"-word file: the infobox lead image named exactly like the page.
    let want = page.trim().replace('_', " ").to_lowercase();
    if want.is_empty() {
        return String::new();
    }
    for title in titles {
        let name = title.trim();
        if name.is_empty() {
            continue;
        }
        // The `File:` namespace prefix is not part of the comparison.
        let stem = match name.split_once(':') {
            Some((_, rest)) => rest,
            None => name,
        };
        let stem = stem.trim();
        // A photo extension is still required: the stem match replaces only
        // the "poster"-word signal, never the format or blocked-word guards.
        let dot = stem.rfind('.');
        let core = match dot {
            Some(i) => stem[..i].trim(),
            None => continue,
        };
        if core.replace('_', " ").to_lowercase() != want {
            continue;
        }
        let lower = name.to_lowercase();
        if wiki_poster_blocked(&lower) {
            continue;
        }
        let Some(rank) = wiki_poster_ext_rank(&lower) else {
            continue;
        };
        if !found || rank < best_rank {
            best = name.to_string();
            best_rank = rank;
            found = true;
        }
    }
    best
}

/// `wikiParenYear = \((19\d{2}|20\d{2})\b`, hand-rolled (the workspace pins no
/// regex crate). `\b` is ASCII-only, as in Go.
fn wiki_paren_year(title: &str) -> Option<i32> {
    let bytes = title.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'(' {
            continue;
        }
        if let Some(year) = year_at_with_boundaries(bytes, i + 1, false) {
            return Some(year);
        }
    }
    None
}

/// `wikiAnyYear = \b(19\d{2}|20\d{2})\b`.
fn wiki_any_year(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if i != 0 && is_ascii_word(bytes[i - 1]) {
            continue;
        }
        if let Some(year) = year_at_with_boundaries(bytes, i, true) {
            return Some(year);
        }
    }
    None
}

/// Four digits starting at `i` matching `19\d\d` or `20\d\d`, with the `\b`
/// boundaries the two patterns require.
fn year_at_with_boundaries(bytes: &[u8], i: usize, leading_checked: bool) -> Option<i32> {
    if i + 4 > bytes.len() {
        return None;
    }
    let digits = &bytes[i..i + 4];
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if &digits[..2] != b"19" && &digits[..2] != b"20" {
        return None;
    }
    if !leading_checked && i != 0 && is_ascii_word(bytes[i - 1]) {
        return None;
    }
    let after = i + 4;
    if after < bytes.len() && is_ascii_word(bytes[after]) {
        return None;
    }
    let year = i32::from(digits[0] - b'0') * 1000
        + i32::from(digits[1] - b'0') * 100
        + i32::from(digits[2] - b'0') * 10
        + i32::from(digits[3] - b'0');
    Some(year)
}

fn is_ascii_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `wikiYear`: the release year from the page title first, then from the extract
/// text, and 0 when neither yields one. It never invents a year.
pub(crate) fn wiki_year(page_title: &str, extract: &str) -> i32 {
    if let Some(year) = wiki_paren_year(page_title)
        && (1900..=2100).contains(&year)
    {
        return year;
    }
    if let Some(year) = wiki_any_year(extract)
        && (1900..=2100).contains(&year)
    {
        return year;
    }
    0
}

impl WikipediaSource {
    /// `wikiPosterURL` is best-effort (never transient, never a miss): at most
    /// two SAME-host calls -- `prop=images` lists the article's files, then
    /// `prop=imageinfo&iiprop=url` resolves the picked poster file to a direct
    /// URL. Any failure, any non-poster list, or any empty URL yields `""`.
    fn wiki_poster_url(&self, cancel: &Cancel, page: &str) -> String {
        if page.trim().is_empty() {
            return String::new();
        }
        if cancel.err().is_some() {
            return String::new();
        }
        let list_url = format!(
            "{}/w/api.php?action=query&prop=images&imlimit=max&titles={}&format=json",
            self.base_url(),
            query_escape(page)
        );
        let Ok(response) = self.wiki_get(cancel, &list_url) else {
            return String::new();
        };
        let Ok(listing) = decode_capped::<WikiImagesResp>(response) else {
            return String::new();
        };
        let mut names: Vec<String> = Vec::new();
        for page_entry in listing.query.pages.values() {
            for image in &page_entry.images {
                names.push(image.title.clone());
            }
        }
        let picked = wiki_pick_poster(&names, page);
        if picked.is_empty() {
            return String::new();
        }
        if cancel.err().is_some() {
            return String::new();
        }
        let info_url = format!(
            "{}/w/api.php?action=query&prop=imageinfo&iiprop=url&titles={}&format=json",
            self.base_url(),
            query_escape(&picked)
        );
        let Ok(response) = self.wiki_get(cancel, &info_url) else {
            return String::new();
        };
        let Ok(info) = decode_capped::<WikiImageInfoResp>(response) else {
            return String::new();
        };
        for page_entry in info.query.pages.values() {
            if page_entry.missing.0 {
                continue;
            }
            for entry in &page_entry.imageinfo {
                let url = entry.url.trim();
                if !url.is_empty() {
                    return url.to_string();
                }
            }
        }
        String::new()
    }

    /// `wikiPageURL`: the canonical article URL for a page title.
    fn wiki_page_url(&self, page_title: &str) -> String {
        normalize_url(&format!(
            "{}/wiki/{}",
            self.base_url(),
            path_escape(&page_title.replace(' ', "_"))
        ))
    }
}

impl crate::resolver::Source for WikipediaSource {
    fn name(&self) -> &str {
        "wikipedia"
    }

    /// `Find` is movie-only (a series key never reaches the wire): `(nil, nil)`
    /// for series or blank titles BEFORE any fetch, an
    /// [`Error::Unavailable`] on transient failure, and `(nil, nil)` on a
    /// definitive "no such page".
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::SERIES || q.title.is_empty() {
            return Ok(None);
        }
        if let Some(reason) = cancel.err() {
            return Err(Error::Cancelled(reason));
        }

        let search_url = format!(
            "{}/w/api.php?action=query&list=search&srsearch={}&srlimit=5&format=json",
            self.base_url(),
            query_escape(&format!("{} film", q.title))
        );
        let response = match self.wiki_get(cancel, &search_url) {
            Ok(response) => response,
            Err(err) => return Err(wiki_transient(err)),
        };
        let search = match decode_capped::<WikiSearchResp>(response) {
            Ok(search) => search,
            Err(err) => return Err(wiki_transient(err)),
        };
        if let Some(error) = &search.error {
            let code = error.code.trim();
            let code = if code.is_empty() { "unknown" } else { code };
            return Err(wiki_transient(format!("wikipedia api error {code}")));
        }
        let mut page = String::new();
        for hit in &search.query.search {
            let title = hit.title.trim();
            if !title.is_empty() {
                page = title.to_string();
                break;
            }
        }
        if page.is_empty() {
            return Ok(None);
        }

        let detail_url = format!(
            "{}/w/api.php?action=query&prop=extracts|pageimages&exintro&explaintext&piprop=thumbnail|original&pithumbsize=600&titles={}&format=json",
            self.base_url(),
            query_escape(&page)
        );
        let response = match self.wiki_get(cancel, &detail_url) {
            Ok(response) => response,
            Err(err) => return Err(wiki_transient(err)),
        };
        let detail = match decode_capped::<WikiDetailResp>(response) {
            Ok(detail) => detail,
            Err(err) => return Err(wiki_transient(err)),
        };
        if let Some(error) = &detail.error {
            let code = error.code.trim();
            let code = if code.is_empty() { "unknown" } else { code };
            return Err(wiki_transient(format!("wikipedia api error {code}")));
        }
        if detail.query.pages.is_empty() {
            return Ok(None);
        }
        let mut title = String::new();
        let mut extract = String::new();
        let mut thumb = String::new();
        let mut matched = false;
        for entry in detail.query.pages.values() {
            if entry.missing.0 {
                continue;
            }
            if entry.title.trim().is_empty()
                && entry.extract.trim().is_empty()
                && entry.thumbnail.source.trim().is_empty()
                && entry.original.source.trim().is_empty()
            {
                continue;
            }
            title = entry.title.clone();
            extract = entry.extract.clone();
            // The page image: the pithumbsize rendering when one exists, else
            // the original file URL (piprop=thumbnail|original).
            thumb = first_non_empty(&[&entry.thumbnail.source, &entry.original.source]);
            matched = true;
            break;
        }
        if !matched {
            return Ok(None);
        }
        let title = title.trim();
        // The year is derived from the RAW page title first: `wiki_year` reads
        // the parenthesized year (`Dune (2021 film)` -> 2021), which the
        // disambiguation strip below removes.
        let year = wiki_year(title, &extract);
        let title = if title.is_empty() {
            q.title.trim().to_string()
        } else {
            strip_disambiguation(title)
        };
        // Poster fallback, same host, at most two extra calls, ONLY for an
        // already-resolved page with no page image at all: a present page image
        // returns untouched above.
        let mut artwork = thumb.trim().to_string();
        if artwork.is_empty() {
            artwork = self.wiki_poster_url(cancel, &page);
        }
        Ok(Some(Media {
            source: "wikipedia".to_string(),
            source_id: title.clone(),
            kind: Kind::MOVIE,
            year,
            movie_url: self.wiki_page_url(&title),
            title,
            artwork: normalize_url(&artwork),
            synopsis: cap_synopsis(&strip_html(&extract)),
            genres: String::new(),
            length: String::new(),
            show_url: String::new(),
            episode_url: String::new(),
            trailer_url: String::new(),
            episodes: Vec::new(),
            episode_count: 0,
            fetched_at: crate::timefmt::now_rfc3339(),
        }))
    }
}
