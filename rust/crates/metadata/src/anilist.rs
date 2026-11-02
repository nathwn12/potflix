//! `metadata::anilist` -- mirrors `go\internal\metadata\anilist.go`.
//!
//! AniList provider (anime branch, first half of the anime pair). One keyless
//! GraphQL POST; episodes come from `streamingEpisodes`.
//!
//! Two behaviours that are contract:
//!
//! * the episode NUMBER is parsed from the entry's TITLE (`"Episode N - ..."`,
//!   `"Episode N"`, `"Episode N.5"`, or a leading `"N - ..."`), and the
//!   positional index is only the fallback when no number is present. Live
//!   AniList media 21355 serves `streamingEpisodes` numbered 51..66 for an
//!   episode-1 query, so a positional mapping would hand back a wrong title;
//! * `Season` is always 0 (absolute numbering), so the resolver's number-exact
//!   match must not require a season.

use std::time::Duration;

use metadata_core::types::{
    Episode, Kind, Media, Query, join_genres, normalize_url, runtime_length,
};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, decode_capped};
use serde_json::json;

use crate::Error;
use crate::artwork::{cap_synopsis, first_non_empty, strip_html};
use crate::serdeutil::null_default;

/// The provider's default host.
pub const ANILIST_DEFAULT_BASE: &str = "https://graphql.anilist.co";

/// `anilistQuery`, verbatim: `type: ANIME` is inline, so no `media` filter is
/// needed. A `Page` of candidates is ranked locally (`pick_best_show` plus a
/// soft year preference): a singular `Media(search:)` hands back only the
/// server's top pick, which for `dragon ball super` is the 2015 series while
/// the 2026 `Dragon Ball Super: Beerus` release never surfaces.
pub const ANILIST_QUERY: &str = "query ($search:String){Page(perPage:10){media(search:$search,type:ANIME){id idMal title{romaji english} synonyms format season seasonYear episodes genres duration siteUrl description coverImage{extraLarge large} bannerImage streamingEpisodes{title url site thumbnail}}}}";

/// `anilistSource`.
pub struct AnilistSource {
    base: String,
    client: HttpClient,
}

impl AnilistSource {
    /// A source pointed at `base` (`""` means [`ANILIST_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> AnilistSource {
        AnilistSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            ANILIST_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for AnilistSource {
    fn default() -> AnilistSource {
        AnilistSource::new("", HttpClient::new(Duration::ZERO))
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AnilistTitle {
    #[serde(default, deserialize_with = "null_default")]
    romaji: String,
    #[serde(default, deserialize_with = "null_default")]
    english: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AnilistCover {
    #[serde(rename = "extraLarge", default, deserialize_with = "null_default")]
    extra_large: String,
    #[serde(default, deserialize_with = "null_default")]
    large: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AnilistStreaming {
    #[serde(default, deserialize_with = "null_default")]
    title: String,
    #[serde(default, deserialize_with = "null_default")]
    url: String,
    #[serde(default, deserialize_with = "null_default")]
    thumbnail: String,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AnilistMedia {
    #[serde(default, deserialize_with = "null_default")]
    id: i32,
    #[serde(rename = "idMal", default, deserialize_with = "null_default")]
    id_mal: i32,
    #[serde(default, deserialize_with = "null_default")]
    title: AnilistTitle,
    #[serde(default, deserialize_with = "null_default")]
    synonyms: Vec<String>,
    /// `TV`, `MOVIE`, `OVA`, `ONA`, ...: the entry's own form, so a manual
    /// search can offer a movie as a movie.
    #[serde(default, deserialize_with = "null_default")]
    format: String,
    #[serde(rename = "seasonYear", default)]
    season_year: Option<i32>,
    #[serde(default, deserialize_with = "null_default")]
    genres: Vec<String>,
    #[serde(default)]
    duration: Option<i32>,
    #[serde(rename = "siteUrl", default, deserialize_with = "null_default")]
    site_url: String,
    #[serde(default, deserialize_with = "null_default")]
    description: String,
    #[serde(rename = "coverImage", default)]
    cover_image: Option<AnilistCover>,
    #[serde(rename = "bannerImage", default, deserialize_with = "null_default")]
    banner_image: String,
    #[serde(
        rename = "streamingEpisodes",
        default,
        deserialize_with = "null_default"
    )]
    streaming: Vec<AnilistStreaming>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AnilistPage {
    #[serde(default, deserialize_with = "null_default")]
    media: Vec<AnilistMedia>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AnilistData {
    #[serde(rename = "Page", default)]
    page: Option<AnilistPage>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct AnilistResp {
    #[serde(default)]
    data: Option<AnilistData>,
    #[serde(default)]
    errors: Option<serde_json::Value>,
}

/// A 200 body carrying a provider error or a null payload is transient, never
/// a definitive miss: returning `Ok(None)` here would persist a 30-day
/// negative and never mark the leg failed.
fn anilist_items(parsed: AnilistResp) -> Result<Vec<AnilistMedia>, Error> {
    if let Some(errors) = parsed.errors
        && !errors.is_null()
        && !errors.as_array().is_some_and(|arr| arr.is_empty())
        && !errors.as_object().is_some_and(|obj| obj.is_empty())
    {
        return Err(Error::other(
            "metadata: anilist error document (transient)",
        ));
    }
    let Some(data) = parsed.data else {
        return Err(Error::other(
            "metadata: anilist null payload (transient)",
        ));
    };
    let Some(page) = data.page else {
        return Err(Error::other(
            "metadata: anilist null payload (transient)",
        ));
    };
    Ok(page.media)
}

/// Go's `\s`, which `regexp` defines as `[\t\n\f\r ]`.
fn is_go_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{000c}' | '\r' | ' ')
}

/// Java/Go's default `.`: any code point except a line terminator.
fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{0085}' | '\u{2028}' | '\u{2029}')
}

fn strip_go_spaces(s: &str) -> &str {
    s.trim_start_matches(is_go_space)
}

fn strip_go_spaces_plus(s: &str) -> Option<&str> {
    let stripped = strip_go_spaces(s);
    if stripped.len() == s.len() {
        None
    } else {
        Some(stripped)
    }
}

fn take_digits(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s.split_at(end)
}

fn positive(digits: &str) -> Option<i32> {
    digits.parse::<i32>().ok().filter(|n| *n > 0)
}

/// `anilistEpNumRe = (?i)^episode\s+(\d+)(?:\.\d+)?\s*(?:-\s*(.*))?$`, hand-rolled
/// because the workspace pins no regex crate and `titleclean`'s matcher (built
/// for Java's pattern set) carries no `(?i)` flag, `\s` or `\b`.
///
/// `None` means the pattern did not match (which includes Go's `Atoi` guard
/// failing on an over-long digit run). `Some((n, None))` is Go's
/// NON-PARTICIPATING trailing group: the caller then keeps the whole title.
fn match_episode_prefix(s: &str) -> Option<(i32, Option<&str>)> {
    let head = s.get(.."episode".len())?;
    if !head.eq_ignore_ascii_case("episode") {
        return None;
    }
    let rest = strip_go_spaces_plus(&s["episode".len()..])?;
    let (digits, rest) = take_digits(rest);
    let number = positive(digits)?;
    let rest = match rest.strip_prefix('.') {
        Some(after_dot) => {
            let (fraction, after_fraction) = take_digits(after_dot);
            if fraction.is_empty() {
                return None;
            }
            after_fraction
        }
        None => rest,
    };
    let rest = strip_go_spaces(rest);
    if rest.is_empty() {
        return Some((number, None));
    }
    let tail = strip_go_spaces(rest.strip_prefix('-')?);
    if tail.chars().any(is_line_terminator) {
        return None;
    }
    Some((number, Some(tail)))
}

/// `anilistLeadingNumRe = ^(\d{1,4})(?:\.\d+)?\s*-\s*(.+)$`.
fn match_leading_number(s: &str) -> Option<(i32, &str)> {
    let (digits, rest) = take_digits(s);
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let number = positive(digits)?;
    let rest = match rest.strip_prefix('.') {
        Some(after_dot) => {
            let (fraction, after_fraction) = take_digits(after_dot);
            if fraction.is_empty() {
                return None;
            }
            after_fraction
        }
        None => rest,
    };
    let rest = strip_go_spaces(rest).strip_prefix('-')?;
    let tail = strip_go_spaces(rest);
    if tail.is_empty() || tail.chars().any(is_line_terminator) {
        return None;
    }
    Some((number, tail))
}

/// `parseAnilistEpisode`: the episode number from the entry's title when it
/// carries one, plus that prefix stripped for the display title. `fallback` (the
/// running index) is returned only when no number is present.
pub(crate) fn parse_anilist_episode(s: &str, fallback: i32) -> (i32, String) {
    let trimmed = s.trim();
    if let Some((number, tail)) = match_episode_prefix(trimmed) {
        let rest = tail.map(|t| t.trim().to_string()).unwrap_or_default();
        if rest.is_empty() {
            return (number, trimmed.to_string());
        }
        return (number, rest);
    }
    if let Some((number, tail)) = match_leading_number(trimmed) {
        return (number, tail.trim().to_string());
    }
    if fallback > 0 {
        return (fallback, trimmed.to_string());
    }
    (0, trimmed.to_string())
}

/// `canonicalTitle`: verified English wins; otherwise romaji. Synonyms are
/// NEVER display text -- they stay in `match_aliases` for matching only, but
/// an unverified synonym must not stand in as the English card title (live
/// `Media(id:20760)` carries `english` null with a FIRST synonym of romaji
/// arc names while the English title sits third, so the synonym fallback
/// displayed arc names and dropped `Gintama` from the card).
fn canonical_title(english: &str, romaji: &str) -> String {
    let english = english.trim();
    if !english.is_empty() {
        return english.to_string();
    }
    romaji.trim().to_string()
}

/// The entry's match titles: every non-empty one of english, romaji and the
/// synonyms, so a romaji query still finds the English-titled main series
/// instead of a romaji-named special.
fn match_aliases(english: &str, synonyms: &[String], romaji: &str) -> Vec<String> {
    let mut out = Vec::new();
    for title in std::iter::once(english)
        .chain(std::iter::once(romaji))
        .chain(synonyms.iter().map(String::as_str))
    {
        let title = title.trim();
        if !title.is_empty() {
            out.push(title.to_string());
        }
    }
    out
}

/// Generic tokens carry no identity: function words (articles, prepositions,
/// conjunctions), romanised particles, and release-form vocabulary recur
/// across unrelated entries, so sharing one proves nothing about which work
/// the query names. This is a closed vocabulary list, not a length rule: a
/// short content token (`kai`) corroborates while a long generic one
/// (`season`) never does.
fn is_generic_token(token: &str) -> bool {
    matches!(
        token,
        // English function words.
        "the" | "a" | "an" | "of" | "and" | "or" | "nor" | "in" | "on" | "at" | "to"
        | "for" | "from" | "with" | "by" | "as" | "vs" | "per" | "via"
        // Articles and prepositions common in French, German, Spanish,
        // Portuguese and Italian titles.
        | "de" | "la" | "le" | "les" | "des" | "du" | "el" | "il" | "der" | "die"
        | "das" | "und" | "et" | "y"
        // Romanised Japanese particles.
        | "no" | "na" | "ni" | "wo" | "o" | "wa" | "ga" | "he" | "e" | "mo" | "ka"
        | "yo" | "ne" | "ya" | "sa" | "zo" | "ze"
        // Release-form vocabulary: the kind of release, never which work.
        | "ova" | "ona" | "oad" | "tv" | "movie" | "film" | "special" | "season"
        | "series" | "part" | "cour" | "episode" | "edition" | "version"
        | "collection" | "volume" | "vol"
    )
}

/// Numbered labels (`2nd`, `3rd`, ...) are numeral forms, not names: like a
/// bare numeral they mark an installment and can never establish identity.
fn is_ordinal_numeral(token: &str) -> bool {
    for suffix in ["st", "nd", "rd", "th"] {
        if let Some(stem) = token.strip_suffix(suffix)
            && !stem.is_empty()
            && stem.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// A corroborating token is a content token: it carries a letter (so bare
/// numerals never corroborate) and is not generic (see
/// [`is_generic_token`]). What matters is whether the token can establish
/// identity, never how long it is -- `kai` corroborates while `ova` does
/// not.
fn is_content_token(token: &str) -> bool {
    token.chars().any(|c| c.is_alphabetic())
        && !is_generic_token(token)
        && !is_ordinal_numeral(token)
}

/// Canonical corroboration for the year override. Canonical titles decide
/// identity; an alias may extend identity ONLY with canonical
/// corroboration: the query shares a content token (see
/// [`is_content_token`]) with the candidate's canonical (`english` +
/// `romaji`) titles. An alias-only cover with no such token must not
/// authorize a year override -- `Space Battleship Yamato`/1974 reaches `1650`
/// (`yamato` is in `Uchuu Senkan Yamato`) and `DBZ Kai`/2014 reaches `20635`
/// (`kai` is in `Dragon Ball Kai`), while `Monster`/2019 keeps `19` (nothing
/// in `Golden Kamuy 2nd Season OVA` overlaps `monster`).
fn query_corroborated_by_canonical(query: &str, english: &str, romaji: &str) -> bool {
    let canonical = [english, romaji]
        .into_iter()
        .flat_map(crate::yearpref::tokens)
        .collect::<Vec<_>>();
    crate::yearpref::tokens(query)
        .into_iter()
        .any(|token| is_content_token(&token) && canonical.contains(&token))
}

/// The entry's canonical titles for year-override coverage: verified
/// `english` + `romaji` only. Synonyms stay in [`match_aliases`] for
/// ordinary matching/scoring, but an unverified synonym must never authorize
/// a year override (live `124756` carries a `"Monster"` synonym with year
/// 2019 that would otherwise steal a `Monster`/2019 query from the 2004
/// series `19`, whose canonical titles do cover it).
fn canonical_aliases(english: &str, romaji: &str) -> Vec<String> {
    let mut out = Vec::new();
    for title in [english, romaji] {
        let title = title.trim();
        if !title.is_empty() {
            out.push(title.to_string());
        }
    }
    out
}

impl crate::resolver::Source for AnilistSource {
    fn name(&self) -> &str {
        "anilist"
    }

    fn path(&self) -> crate::resolver::SearchPath {
        crate::resolver::SearchPath::Anime
    }

    /// `Find`: the keyless GraphQL endpoint, candidates ranked locally by
    /// title with a soft year preference, absolute episodes built from
    /// `streamingEpisodes`, sorted by Number for deterministic ordering. A
    /// low-confidence best is a definitive MISS, never a wrong show; year
    /// never gates, it only swaps one complete answer for another.
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::MOVIE || q.title.is_empty() {
            return Ok(None);
        }
        let payload = json!({
            "query": ANILIST_QUERY,
            "variables": {"search": q.title},
        });
        // Read-only GraphQL search: side-effect-free, so the idempotent POST
        // retry (backoff + jitter, honoring Retry-After) applies. A mutating
        // POST would use `post_json` and never retry.
        let response = self.client.post_json_idempotent(cancel, self.base_url(), &payload)?;
        let parsed: AnilistResp = decode_capped(response)?;
        let items = anilist_items(parsed)?;
        // Title identity check: no candidates is a definitive miss (a film
        // query such as `inception` must return `Ok(None)`, not a wrong anime).
        let mut cands: Vec<metadata_core::match_::ShowCand> = Vec::with_capacity(items.len());
        let mut entries: Vec<AnilistMedia> = Vec::with_capacity(items.len());
        let mut years: Vec<i32> = Vec::with_capacity(items.len());
        for media in items {
            if media.id == 0 {
                continue;
            }
            let name = canonical_title(&media.title.english, &media.title.romaji);
            if name.is_empty() {
                continue;
            }
            cands.push(metadata_core::match_::ShowCand {
                name,
                weight: 0,
                episode_count: 0,
                aliases: match_aliases(&media.title.english, &media.synonyms, &media.title.romaji),
            });
            years.push(media.season_year.unwrap_or(0));
            entries.push(media);
        }
        if cands.is_empty() {
            return Ok(None);
        }
        let Some(winner) = metadata_core::match_::pick_best_show(&q.title, &cands) else {
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
        // The override judges canonical titles (english + romaji); synonyms
        // stay in `cands` for matching/scoring and may extend identity only
        // with canonical corroboration ([`query_corroborated_by_canonical`]):
        // an uncorroborated synonym must never authorize the swap (live
        // `124756` carries a `"Monster"` synonym with year 2019 that would
        // otherwise steal a `Monster`/2019 query from the 2004 series `19`),
        // while a corroborated alias still may (live `1650` carries the
        // `Space Battleship Yamato` synonym sharing `yamato` with `Uchuu
        // Senkan Yamato`).
        let mut year_cands = cands.clone();
        for (cand, media) in year_cands.iter_mut().zip(entries.iter()) {
            let mut titles = canonical_aliases(&media.title.english, &media.title.romaji);
            if query_corroborated_by_canonical(&q.title, &media.title.english, &media.title.romaji) {
                titles.extend(
                    media
                        .synonyms
                        .iter()
                        .map(|synonym| synonym.trim().to_string())
                        .filter(|synonym| !synonym.is_empty()),
                );
            }
            cand.aliases = titles;
        }
        let take =
            crate::yearpref::prefer_year_match(&q.title, q.year, &year_cands, &years, winner_idx);
        let media = &entries[take];
        let title = canonical_title(&media.title.english, &media.title.romaji);
        if title.is_empty() {
            return Ok(None);
        }
        let mut out = anilist_media(media, title, Kind::SERIES);
        let mut episodes: Vec<Episode> = Vec::with_capacity(media.streaming.len());
        for (i, entry) in media.streaming.iter().enumerate() {
            let (number, title) = parse_anilist_episode(&entry.title, i as i32 + 1);
            episodes.push(Episode {
                season: 0,
                number,
                title,
                artwork: entry.thumbnail.trim().to_string(),
                url: normalize_url(&entry.url),
                ..Episode::default()
            });
        }
        episodes.sort_by_key(|episode| episode.number);
        out.episodes = episodes;
        Ok(Some(out))
    }

    /// The manual search's rows: the same one-POST `Page` answer, each entry
    /// whose title clears the matcher's floor as its own pickable row. The
    /// entry's own `format` decides the kind (`MOVIE` is a movie, everything
    /// else a series), and no episode table is fetched: `streamingEpisodes`
    /// rides along in the answer, but a search row stays candidate-ready.
    fn find_all(&self, cancel: &Cancel, q: &Query) -> Result<Vec<Media>, Error> {
        if q.title.is_empty() {
            return Ok(Vec::new());
        }
        let payload = json!({
            "query": ANILIST_QUERY,
            "variables": {"search": q.title},
        });
        let response = self.client.post_json_idempotent(cancel, self.base_url(), &payload)?;
        let parsed: AnilistResp = decode_capped(response)?;
        let items = anilist_items(parsed)?;
        let mut out: Vec<Media> = Vec::new();
        for media in items {
            if media.id == 0 {
                continue;
            }
            let name = canonical_title(&media.title.english, &media.title.romaji);
            if name.is_empty() {
                continue;
            }
            let cand = metadata_core::match_::ShowCand {
                name: name.clone(),
                weight: 0,
                episode_count: 0,
                aliases: match_aliases(&media.title.english, &media.synonyms, &media.title.romaji),
            };
            // The same floor `Find` applies: a fuzzy row never becomes a pick.
            if metadata_core::match_::pick_best_show(&q.title, std::slice::from_ref(&cand))
                .is_none()
            {
                continue;
            }
            let kind = if media.format.trim().eq_ignore_ascii_case("movie") {
                Kind::MOVIE
            } else {
                Kind::SERIES
            };
            out.push(anilist_media(&media, name, kind));
            if out.len() == crate::resolver::FIND_ALL_LIMIT {
                break;
            }
        }
        Ok(out)
    }
}

/// The candidate-ready [`Media`] for one AniList entry: identity, year, cover,
/// rating metadata and page URL, with the kind the caller derives from the
/// entry's `format`. The episode table is the caller's to fill.
fn anilist_media(media: &AnilistMedia, title: String, kind: Kind) -> Media {
    let show_url = if media.id_mal > 0 {
        format!("https://myanimelist.net/anime/{}", media.id_mal)
    } else {
        normalize_url(&media.site_url)
    };
    let artwork = match media.cover_image.as_ref() {
        Some(cover) => first_non_empty(&[&cover.extra_large, &cover.large, &media.banner_image]),
        None => media.banner_image.trim().to_string(),
    };
    Media {
        source: "anilist".to_string(),
        source_id: media.id.to_string(),
        kind,
        title,
        year: media.season_year.unwrap_or(0),
        artwork,
        synopsis: cap_synopsis(&strip_html(&media.description)),
        genres: join_genres(&media.genres),
        length: media.duration.map(runtime_length).unwrap_or_default(),
        show_url,
        episode_url: String::new(),
        movie_url: String::new(),
        trailer_url: String::new(),
        episodes: Vec::new(),
        fetched_at: crate::timefmt::now_rfc3339(),
    }
}

/// The token test is vocabulary, not length: the three corroborating tokens
/// the year override depends on (`kai`, `yamato`, `monster`) count, while
/// function words, particles, release-form words, ordinals and bare numbers
/// never do -- however long or short either side runs.
#[cfg(test)]
mod content_token_tests {
    use super::{is_content_token, query_corroborated_by_canonical};

    #[test]
    fn content_token_is_vocabulary_not_length() {
        for token in ["kai", "yamato", "monster", "ball", "dragon"] {
            assert!(is_content_token(token), "{token:?} can establish identity");
        }
        for token in [
            "the", "of", "a", "and", "ova", "season", "movie", "special", "2nd", "3rd", "2014",
            "19", "no", "ni",
        ] {
            assert!(
                !is_content_token(token),
                "{token:?} is generic and corroborates nothing"
            );
        }
    }

    #[test]
    fn kai_corroborates_but_the_alone_does_not() {
        assert!(query_corroborated_by_canonical(
            "DBZ Kai",
            "Dragon Ball Z Kai: The Final Chapters",
            "Dragon Ball Kai (2014)"
        ));
        assert!(!query_corroborated_by_canonical(
            "The DBZ",
            "Dragon Ball Z Kai: The Final Chapters",
            "Dragon Ball Kai (2014)"
        ));
    }
}
