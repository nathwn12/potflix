//! `presence::cardspec` -- mirrors `go\internal\presence\cardspec.go`.
//!
//! The card specification: which lines the presence card carries and how the
//! clean title, canonical title and episode tag compose into them.
//!
//! A [`CardSpec`] chooses what the card's two body lines say. Each line picks
//! from a small fixed preset set ([`card_presets`]) - there is no template
//! language. The zero value falls back to the default on every line, so no
//! migration is needed for stored configs.
//!
//! [`default_card_spec`] is the shipped per-kind mapping: movie details
//! `<year> · <genres>` over a runtime state; series details EMPTY over
//! `SxxEyy · Episode`. The work-level synopsis stays in the artwork tooltip
//! as a <=128-unit snippet (episode-pinned prose never rides); both body
//! lines are identity/playback only, never the title. Any state
//! preset whose final text would repeat the series title blanks instead.
//! The paused " · Paused" marker rides the last populated body line.

use player::window::{APP_NAME, SUFFIX};
use serde::{Deserialize, Serialize};

use crate::{
    Action, PAUSED_MARKER, Resolved, compose_details_for, compose_name_inner, compose_state_for,
    unknown_code_state, with_state_time,
};

/// Details-line preset id: the shipped default.
pub const CARD_DETAILS_DEFAULT: &str = "default";

/// Details-line preset id: the clean title. RETIRED (T5-B): it repeats the
/// activity header, so it is no longer offered ([`card_presets`]) and stored
/// specs carrying it migrate to [`CARD_DETAILS_DEFAULT`] (see
/// [`CardSpec::migrated`]). The composer still renders it for old in-flight
/// specs, and [`CardSpec::normalized`] still accepts it.
pub const CARD_DETAILS_TITLE: &str = "title";

/// Details-line preset id: the episode label. RETIRED: it renders
/// byte-identical to [`CARD_DETAILS_DEFAULT`] for both kinds (the default
/// already is the episode title for series and the synopsis-led line for
/// movies), so it is no longer offered ([`card_presets`]) and stored specs
/// carrying it migrate to [`CARD_DETAILS_DEFAULT`] (see
/// [`CardSpec::migrated`]). The composer still renders it for old in-flight
/// specs, and [`CardSpec::normalized`] still accepts it.
pub const CARD_DETAILS_EPISODE: &str = "episode";

/// Details-line preset id: the series name. RETIRED (T5-B): same as
/// [`CARD_DETAILS_TITLE`] -- repeats the header, no longer offered, stored
/// specs migrate to [`CARD_DETAILS_DEFAULT`].
pub const CARD_DETAILS_SERIES: &str = "series";

/// State-line preset id: the shipped default.
pub const CARD_STATE_DEFAULT: &str = "default";

/// State-line preset id: the play-state tag.
pub const CARD_STATE_TAG: &str = "tag";

/// State-line preset id: the synopsis.
pub const CARD_STATE_SYNOPSIS: &str = "synopsis";

/// State-line preset id: nothing.
pub const CARD_STATE_NONE: &str = "none";

/// The stored choice of one preset per body line (`cardspec.go:41-44`).
///
/// The serde shape is the stored config contract the app crate embeds as
/// `Card presence.CardSpec` (`go\config.go:22-30`): Go's
/// `json:"details,omitempty"` / `json:"state,omitempty"`, i.e. an empty line is
/// OMITTED from the JSON and an absent line loads as empty (then normalizes to
/// the default).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardSpec {
    /// Details-line preset id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub details: String,
    /// State-line preset id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
}

/// One named choice in the [`card_presets`] set (`cardspec.go:47-50`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardPreset {
    /// The stored preset id.
    pub id: String,
    /// The short owner-drawn button label.
    pub label: String,
}

/// Today's shipped output: both lines on default (`cardspec.go:53`).
pub fn default_card_spec() -> CardSpec {
    CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: CARD_STATE_DEFAULT.to_string(),
    }
}

impl CardSpec {
    /// Maps empty/unknown ids back to the default line, so a zero value (or a
    /// stored typo) can never reach the renderer (`cardspec.go:59`).
    pub fn normalized(&self) -> CardSpec {
        let mut out = self.clone();
        match out.details.as_str() {
            CARD_DETAILS_DEFAULT | CARD_DETAILS_TITLE | CARD_DETAILS_EPISODE
            | CARD_DETAILS_SERIES => {}
            _ => out.details = CARD_DETAILS_DEFAULT.to_string(),
        }
        match out.state.as_str() {
            CARD_STATE_DEFAULT | CARD_STATE_TAG | CARD_STATE_SYNOPSIS | CARD_STATE_NONE => {}
            _ => out.state = CARD_STATE_DEFAULT.to_string(),
        }
        out
    }

    /// T5-B migration for stored specs, extended to the retired Episode
    /// preset: the retired Details presets ([`CARD_DETAILS_TITLE`]/
    /// [`CARD_DETAILS_SERIES`], which repeat the activity header, and
    /// [`CARD_DETAILS_EPISODE`], which renders byte-identical to the default
    /// for both kinds) reset to the default line. Kept ids, unknown ids and
    /// every state line survive byte-for-byte, so the migration can never lose
    /// an unrelated spec. The store applies this on load and on save; the
    /// renderer keeps composing retired ids for old in-flight specs, so only
    /// the persisted value changes.
    pub fn migrated(&self) -> CardSpec {
        let mut out = self.clone();
        match out.details.as_str() {
            CARD_DETAILS_TITLE | CARD_DETAILS_SERIES | CARD_DETAILS_EPISODE => {
                out.details = CARD_DETAILS_DEFAULT.to_string();
            }
            _ => {}
        }
        out
    }

    /// Whether both lines are the shipped default (`cardspec.go:75`).
    pub fn is_default(&self) -> bool {
        let n = self.normalized();
        n.details == CARD_DETAILS_DEFAULT && n.state == CARD_STATE_DEFAULT
    }

    /// Returns the (details, state) body lines for the given inputs under this
    /// spec (`cardspec.go:147`). The default spec is the shipped per-kind
    /// mapping: movie `<year> · <genres>` over the runtime, series EMPTY over
    /// `SxxEyy · Episode`. No body line ever repeats the activity header: the
    /// header carries the media title, so a line identical to it blanks. The
    /// synopsis is never a body line under the default;
    /// [`CARD_STATE_SYNOPSIS`] is an explicit legacy preset. The idle card
    /// keeps its shipped lines under any spec.
    pub fn render(&self, raw_title: &str, a: Option<&Action>, r: &Resolved) -> (String, String) {
        self.render_kind(raw_title, a, r, r.is_movie)
    }

    /// [`CardSpec::render`] with an explicit kind: `is_movie` drives the
    /// per-kind branches instead of the stored `r.is_movie` flag, so anime
    /// (`is_movie=false`, even with a year set) resolves through the series
    /// branch. The push path passes the stored flag; the Card-view preview
    /// passes the live kind.
    pub fn render_kind(
        &self,
        raw_title: &str,
        a: Option<&Action>,
        r: &Resolved,
        is_movie: bool,
    ) -> (String, String) {
        if is_idle_title(raw_title) {
            return ("Browsing".to_string(), "Idle".to_string());
        }
        // UNKNOWN: the parsed title (in the header) plus the parsed code if
        // any on line 2; no art, no prose, no timestamps (the composer
        // strips those). Presets do not reach this card: there is nothing
        // resolved to restyle.
        if r.unknown {
            let mut state = unknown_code_state(raw_title, r);
            if a.is_some_and(|x| x.paused) {
                state = with_state_time(&state, PAUSED_MARKER);
            }
            return (String::new(), state);
        }
        let spec = self.normalized();
        let mut details = compose_details_for(raw_title, r, is_movie, &spec.details);
        let mut state = compose_state_for(raw_title, a, r, is_movie, &spec.state);
        // PAUSED under the default state line: the default branches mark a
        // populated state themselves; when the state is empty the marker
        // rides the last populated body line instead -- the details when one
        // exists, else a bare marker on the state.
        if a.is_some_and(|x| x.paused)
            && spec.state == CARD_STATE_DEFAULT
            && state.is_empty()
        {
            if details.is_empty() {
                state = with_state_time("", PAUSED_MARKER);
            } else {
                details = with_state_time(&details, PAUSED_MARKER);
            }
        }
        // One rule for both surfaces (wire and Card-view preview): no body line
        // repeats the header. The header carries the media title, so a series'
        // own title on line 1 blanks; the episode state still carries
        // `SxxEyy · Episode`. A movie's line 1 keeps `<year> · <genres>`.
        let header = compose_name_inner(raw_title, r);
        if !details.is_empty() && details == header {
            details.clear();
        }
        (details, state)
    }
}

/// The small named set per line (`cardspec.go:83`): 1 stored Details value
/// and 4 State choices. Details keeps only Default (the per-kind default
/// line: empty for TV, `<year> · <genres>` for movies); the stored value stays
/// for compatibility but the Card view offers no Details row. Episode
/// rendered byte-identical to Default for both kinds, so it is retired
/// alongside Title and Series (which repeated the activity header, T5-B).
/// Labels stay short so the owner-drawn preset buttons ellipsize cleanly at
/// any DPI.
pub fn card_presets() -> (Vec<CardPreset>, Vec<CardPreset>) {
    let details = vec![
        CardPreset {
            id: CARD_DETAILS_DEFAULT.to_string(),
            label: "Default".to_string(),
        },
    ];
    let states = vec![
        CardPreset {
            id: CARD_STATE_DEFAULT.to_string(),
            label: "Default".to_string(),
        },
        CardPreset {
            id: CARD_STATE_TAG.to_string(),
            label: "Tag".to_string(),
        },
        CardPreset {
            id: CARD_STATE_SYNOPSIS.to_string(),
            label: "Synopsis".to_string(),
        },
        CardPreset {
            id: CARD_STATE_NONE.to_string(),
            label: "Minimal".to_string(),
        },
    ];
    (details, states)
}

/// Mirrors [`build_activity`](crate::build_activity)'s idle gate
/// (`cardspec.go:99-107`): the title carries no media name. The idle card
/// ("Browsing"/"Idle") is never restyled by presets.
pub(crate) fn is_idle_title(raw_title: &str) -> bool {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    raw.trim().is_empty() || raw == APP_NAME
}
