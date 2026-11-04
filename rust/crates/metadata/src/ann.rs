//! `metadata::ann` -- the Anime News Network encyclopedia source (keyless XML).
//!
//! ANN (`https://cdn.animenewsnetwork.com/encyclopedia/api.xml`) needs no key
//! for title search: `GET ?anime=~<title>` returns one `<anime>` report per
//! candidate, carrying the multilingual titles, the cover (`Picture`), the
//! synopsis (`Plot Summary`), the episode count (`Number of episodes`), the
//! running time and the vintage year.
//!
//! Four behaviours that are contract, not detail:
//!
//! * the pick is title-validated with [`pick_best_show`] over the reports'
//!   Main titles: a low-confidence best returns a definitive MISS rather than
//!   a wrong show. The vintage year NEVER rejects: at equal title strength
//!   it prefers the candidate carrying the query's year, but a missing year,
//!   or a year that disagrees with the filename, is a nice-to-have display
//!   field, never a gate;
//! * the `~` operator is a case-insensitive SUBSTRING match, so punctuation
//!   in the stored title defeats a plain query (`re zero` misses `Re:ZERO`).
//!   A miss therefore retries once with spaces rewritten as colons (`re:zero`,
//!   verified live). Both attempts are title-validated, so the retry can only
//!   add a validated answer, never a wrong one;
//! * ANN carries an episode COUNT but no episode table, so the answer
//!   synthesizes numbered absolute stubs ([`episode_stubs`]) -- numbers only,
//!   never fabricated titles;
//! * movies never reach the wire: this leg serves the series chain, so a
//!   movie key returns `Ok(None)` before any fetch.
//!
//! The XML is scanned by hand: the workspace pins no XML crate, and the
//! reports need only the `<anime>` blocks and their `<info>` rows. ANN emits
//! `&#039;` for apostrophes, which the shared entity table does not cover,
//! so it is decoded locally before the shared strip helpers run.

use std::time::Duration;

use metadata_core::match_::{ShowCand, pick_best_show};
use metadata_core::parse::date_year;
use metadata_core::types::{Kind, Media, Query, normalize_url, runtime_length};
use metadata_http::cancel::Cancel;
use metadata_http::http::{HttpClient, MAX_BODY_BYTES, drain_and_close};
use ureq::Body;
use ureq::http::{Response, StatusCode};

use crate::Error;
use crate::artwork::{cap_synopsis, strip_html};
use crate::justwatch::episode_stubs;
use crate::urlutil::query_escape;

/// The provider's default host.
pub const ANN_DEFAULT_BASE: &str = "https://cdn.animenewsnetwork.com";

/// `annSource`.
pub struct AnnSource {
    base: String,
    client: HttpClient,
}

impl AnnSource {
    /// A source pointed at `base` (`""` means [`ANN_DEFAULT_BASE`]).
    pub fn new(base: impl Into<String>, client: HttpClient) -> AnnSource {
        AnnSource {
            base: base.into(),
            client,
        }
    }

    fn base_url(&self) -> &str {
        if self.base.is_empty() {
            ANN_DEFAULT_BASE
        } else {
            &self.base
        }
    }
}

impl Default for AnnSource {
    fn default() -> AnnSource {
        AnnSource::new("", HttpClient::new(Duration::ZERO))
    }
}

/// One parsed `<anime>` report.
#[derive(Debug, Clone, Default)]
struct AnnEntry {
    id: String,
    main: String,
    picture: String,
    plot: String,
    episodes: Option<i32>,
    runtime: Option<i32>,
    vintage: String,
}

/// `url.QueryEscape` with colons kept literal: ANN treats `:` as title
/// content (`~re:zero` matches `Re:ZERO`), and the server reads a literal
/// colon while `query_escape` would send `%3A`.
fn ann_escape(s: &str) -> String {
    query_escape(s).replace("%3A", ":")
}

/// The value of `key="..."` in a tag, `""` when absent.
fn tag_attr(tag: &str, key: &str) -> String {
    let needle = format!("{key}=\"");
    let Some(start) = tag.find(&needle) else {
        return String::new();
    };
    let rest = &tag[start + needle.len()..];
    rest.find('"').map_or(String::new(), |end| rest[..end].to_string())
}

/// The row text of an `<info>` element: nested tags (the `<img>` rows inside
/// `Picture`) fold to spaces, mirroring the shared strip helper.
fn info_text(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut rest = inner;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        out.push(' ');
        match rest[start..].find('>') {
            Some(end) => rest = &rest[start + end + 1..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// ANN's apostrophe, which the shared entity table does not cover.
fn ann_entities(s: &str) -> String {
    if s.contains("&#039;") {
        s.replace("&#039;", "'")
    } else {
        s.to_string()
    }
}

/// The first integer in `s` (`"26"` -> 26, `"25 min"` -> 25); `None` when the
/// row carries no digits.
fn leading_int(s: &str) -> Option<i32> {
    let digits: String = s
        .trim()
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Splits one `<anime>...</anime>` block into its report. `None` when the
/// block carries no usable Main title.
fn parse_entry(open_tag: &str, inner: &str) -> Option<AnnEntry> {
    let id = tag_attr(open_tag, "id");
    if id.trim().is_empty() {
        return None;
    }
    let mut entry = AnnEntry {
        id: id.trim().to_string(),
        ..AnnEntry::default()
    };
    let mut alts: Vec<String> = Vec::new();
    for chunk in inner.split("<info ").skip(1) {
        let Some(tag_end) = chunk.find('>') else {
            continue;
        };
        let tag = &chunk[..tag_end];
        if tag.trim_end().ends_with('/') {
            continue;
        }
        let rest = &chunk[tag_end + 1..];
        let Some(close) = rest.find("</info>") else {
            continue;
        };
        let text = ann_entities(info_text(&rest[..close]).trim());
        match tag_attr(tag, "type").as_str() {
            "Main title" => {
                if entry.main.is_empty() && !text.trim().is_empty() {
                    entry.main = text.trim().to_string();
                }
            }
            "Alternative title" => {
                if !text.trim().is_empty() {
                    alts.push(text.trim().to_string());
                }
            }
            "Picture" => {
                if entry.picture.is_empty() {
                    let src = tag_attr(tag, "src");
                    if !src.trim().is_empty() {
                        entry.picture = src.trim().to_string();
                    }
                }
            }
            "Plot Summary" => {
                if entry.plot.is_empty() && !text.trim().is_empty() {
                    entry.plot = text.trim().to_string();
                }
            }
            "Number of episodes" => {
                if entry.episodes.is_none() {
                    entry.episodes = leading_int(&text);
                }
            }
            "Running time" => {
                if entry.runtime.is_none() {
                    entry.runtime = leading_int(&text);
                }
            }
            "Vintage" if entry.vintage.is_empty() && !text.trim().is_empty() => {
                entry.vintage = text.trim().to_string();
            }
            "Vintage" => {}
            _ => {}
        }
    }
    if entry.main.is_empty() {
        // Reports always carry a Main title; without one there is nothing to
        // validate the pick against.
        entry.main = alts.into_iter().next().unwrap_or_default();
    }
    if entry.main.is_empty() {
        return None;
    }
    Some(entry)
}

/// All `<anime>` reports in a response body. A `<warning>` (no results) or a
/// body without reports yields an empty vec, never an error.
fn parse_reports(body: &str) -> Vec<AnnEntry> {
    let mut out = Vec::new();
    for chunk in body.split("<anime ").skip(1) {
        let Some(tag_end) = chunk.find('>') else {
            continue;
        };
        let open_tag = &chunk[..tag_end];
        let rest = &chunk[tag_end + 1..];
        let Some(close) = rest.find("</anime>") else {
            continue;
        };
        if let Some(entry) = parse_entry(open_tag, &rest[..close]) {
            out.push(entry);
        }
    }
    out
}

/// `decodeCapped` for the XML API: the body as text, capped at
/// [`MAX_BODY_BYTES`], with a non-2xx status as an error. The remainder past
/// the cap is discarded with the connection (ANN reports are small; the JSON
/// legs drain for pool reuse, which this volunteer leg does not need).
fn body_text(response: Response<Body>) -> Result<String, Error> {
    use std::io::Read as _;
    if !response.status().is_success() {
        let status = response.status();
        drain_and_close(response);
        return Err(metadata_http::Error::Status(status).into());
    }
    let mut body = response.into_body();
    let mut buf = Vec::new();
    body.as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut buf)
        .map_err(|err| Error::other(format!("metadata: read response: {err}")))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

impl AnnSource {
    /// The reports for one search variant (`~<query>`).
    fn search(&self, cancel: &Cancel, variant: &str) -> Result<Vec<AnnEntry>, Error> {
        let url = format!("{}/encyclopedia/api.xml?anime=~{}", self.base_url(), variant);
        let response = self.client.get(cancel, &url)?;
        if response.status() == StatusCode::NOT_FOUND {
            drain_and_close(response);
            return Ok(Vec::new());
        }
        let body: String = body_text(response)?;
        Ok(parse_reports(&body))
    }

    /// One validated pick: the title-ranked best Main title, with a known
    /// query year preferring any challenger that both carries it and fully
    /// covers the query title. `Ok(None)` is a definitive miss. Year never
    /// gates.
    fn pick(&self, cancel: &Cancel, q: &Query) -> Result<Option<AnnEntry>, Error> {
        let mut entries = self.search(cancel, &ann_escape(&q.title))?;
        if entries.is_empty() && q.title.contains(' ') {
            // The `~` operator is a literal substring match, so a space in
            // the query misses a colon in the stored title (`re zero` vs
            // `Re:ZERO`). One validated retry with colons, never a guess.
            entries = self.search(cancel, &ann_escape(&q.title.replace(' ', ":")))?;
        }
        let mut cands: Vec<ShowCand> = Vec::with_capacity(entries.len());
        let mut years: Vec<i32> = Vec::with_capacity(entries.len());
        for entry in &entries {
            cands.push(ShowCand {
                name: entry.main.clone(),
                weight: 0,
                episode_count: entry.episodes.unwrap_or(0),
                aliases: Vec::new(),
            });
            years.push(date_year(&entry.vintage));
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

impl crate::resolver::Source for AnnSource {
    fn name(&self) -> &str {
        "ann"
    }

    /// `Find`: series only (movies never reach the anime legs through the
    /// resolver, and a series-only source self-gates a ruled-out kind before
    /// touching the wire).
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error> {
        if q.kind == Kind::MOVIE || q.title.is_empty() {
            return Ok(None);
        }
        let Some(entry) = self.pick(cancel, q)? else {
            return Ok(None);
        };
        // Numbered absolute stubs only: ANN carries a count but no table.
        let episodes = episode_stubs(entry.episodes.unwrap_or(0), 0);
        Ok(Some(Media {
            source: "ann".to_string(),
            source_id: entry.id.clone(),
            kind: Kind::SERIES,
            title: entry.main.clone(),
            year: date_year(&entry.vintage),
            artwork: normalize_url(&entry.picture),
            synopsis: cap_synopsis(&strip_html(&ann_entities(&entry.plot))),
            genres: String::new(),
            length: runtime_length(entry.runtime.unwrap_or(0)),
            show_url: normalize_url(&format!(
                "https://www.animenewsnetwork.com/encyclopedia/anime.php?id={}",
                entry.id.trim(),
            )),
            episode_url: String::new(),
            movie_url: String::new(),
            trailer_url: String::new(),
            episodes,
            episode_count: 0,
            fetched_at: crate::timefmt::now_rfc3339(),
        }))
    }
}
