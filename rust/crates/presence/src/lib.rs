//! `presence` -- mirrors the Go package `go\internal\presence`.
//!
//! The presence state machine and the activity payload builder.
//!
//! This file is the module body for `go\internal\presence\presence.go`: the
//! transition matrix ([`Tracker`]), the trailing-edge RUN debounce, the card line
//! grammar ([`build_activity`]) and the metadata-resolved card
//! ([`build_activity_resolved`]). [`gate`] is `gate.go` (the send gate) and
//! [`cardspec`] is `cardspec.go` (the Card view's spec model).
//!
//! Ported from me.potflix.rpc.Main (PresenceTracker.poll :265-309,
//! withStateTime :313-317, pushActivity :139-191, fmt :501-508), with one
//! deliberate deviation from the fleet decisions (see [`Tracker`] and the
//! README): repeated RUN pushes within ~1 s are debounced unless the title
//! changed. The debounce is trailing-edge: the suppressed anchor is HELD and
//! emitted when the window expires, so a seek is never dropped forever.
//!
//! Not carried from `presence.go`: the unexported `filterButtons` (:646-652) is
//! never called in the Go original (the builders never populate buttons), so in
//! Rust it would be dead code under `-D warnings`. The pristine-card decision it
//! documents is stated where the call site used to be.

use player::control::PlayStatus;
use player::window::{APP_NAME, SUFFIX};
use titleclean::{clean, trunc};

use crate::clock::IntervalClock;

pub mod cardspec;
pub mod designer;
pub mod gate;

pub use designer::{DesignerSpec, live_designer, set_live_designer};

pub(crate) mod clock;

#[cfg(test)]
mod cardspec_test;
#[cfg(test)]
mod gate_test;
#[cfg(test)]
mod presence_test;

/// SeekJumpMS: a per-poll ct jump above this is a real seek (`presence.go:26`).
pub const SEEK_JUMP_MS: i64 = 1500;

/// SettleMoveMS: ct advance across the window that proves real movement
/// (`presence.go:28`).
pub const SETTLE_MOVE_MS: i64 = 50;

/// MoveWindow is the number of ct samples kept to prove real movement
/// (`presence.go:30`).
pub const MOVE_WINDOW: usize = 3;

/// RunDebounceMS is the fleet-decision-10 deviation: a repeated RUN push within
/// this window, with an unchanged title, is suppressed (`presence.go:35`).
pub const RUN_DEBOUNCE_MS: i64 = 1000;

/// One emission decision (Java PresenceAction). `None` means "emit nothing".
/// `ct` is the sample carried into the payload/log (`presence.go:39-46`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Action {
    /// The player is playing.
    pub running: bool,
    /// The player is paused.
    pub paused: bool,
    /// Whether `start` is meaningful.
    pub has_start: bool,
    /// Epoch ms, meaningful when `has_start`.
    pub start: i64,
    /// Legacy position suffix; no longer populated (the paused card uses a
    /// "Paused" marker).
    pub suffix: String,
    /// The ct sample carried into the payload/log.
    pub ct: i64,
}

impl Action {
    /// Java `describe()`'s leading token: RUN / PAUSE / STOP (`presence.go:49`).
    pub fn tag(&self) -> &'static str {
        if self.running {
            "RUN"
        } else if self.paused {
            "PAUSE"
        } else {
            "STOP"
        }
    }
}

/// Go's nil-receiver `(*Action).Tag()`: a missing action tags as `"null"`.
pub fn action_tag(a: Option<&Action>) -> &'static str {
    a.map_or("null", Action::tag)
}

/// One PresenceTracker (`presence.go:64-93`). Its zero value is ready to use;
/// the Java ctor state is reproduced by `have_prev = false` /
/// `has_last_status = false`.
#[derive(Debug, Clone)]
pub struct Tracker {
    last_title: String,
    last_status: PlayStatus,
    /// Java's `lastStatus != null`.
    has_last_status: bool,
    /// Last VALID ct, used only as the seek baseline; -1 = none.
    prev_ct: i64,
    /// Position held in the paused text; -1 = none.
    frozen_ct: i64,
    have_prev: bool,

    ct_window: [i64; MOVE_WINDOW],
    ct_window_len: usize,
    ct_window_idx: usize,

    // Fleet decision 10 (deviation, revised): RUN debounce anchor. A repeated
    // RUN push inside the window is HELD (pending_run), not dropped, and emitted
    // on the first poll after the window expires (trailing edge).
    has_run_push: bool,
    last_run_push_at: i64,
    last_run_push_title: String,
    pending_run: bool,
    pending_title: String,

    // The most recent emitted action. `current` exposes it so the send gate can
    // rebuild the card on every poll, including polls where the tracker emits
    // nothing.
    last_action: Option<Action>,

    // The monotonic interval clock for the trailing-edge debounce, so a
    // wall-clock step cannot strand a held seek.
    clock: IntervalClock,
}

impl Default for Tracker {
    fn default() -> Tracker {
        Tracker {
            last_title: String::new(),
            last_status: PlayStatus::STOPPED,
            has_last_status: false,
            prev_ct: 0,
            frozen_ct: 0,
            have_prev: false,
            ct_window: [0; MOVE_WINDOW],
            ct_window_len: 0,
            ct_window_idx: 0,
            has_run_push: false,
            last_run_push_at: 0,
            last_run_push_title: String::new(),
            pending_run: false,
            pending_title: String::new(),
            last_action: None,
            clock: IntervalClock::default(),
        }
    }
}

impl Tracker {
    /// The zero value, spelled out: boot `have_prev = false` /
    /// `has_last_status = false` (the Java ctor state).
    pub fn new() -> Tracker {
        Tracker::default()
    }

    fn sample_ct(&mut self, ct: i64) {
        self.ct_window[self.ct_window_idx] = ct;
        self.ct_window_idx = (self.ct_window_idx + 1) % MOVE_WINDOW;
        if self.ct_window_len < MOVE_WINDOW {
            self.ct_window_len += 1;
        }
    }

    /// The minimum of the sampled window (`presence.go:105-113`); Java returns
    /// `Long.MAX_VALUE` when empty, but `moving` already requires len >= 2.
    fn oldest_ct(&self) -> i64 {
        self.ct_window[..self.ct_window_len]
            .iter()
            .copied()
            .min()
            .unwrap_or(0)
    }

    /// Consumes one sample and returns the action to push, or `None`
    /// (`presence.go:117`). `ps`/`ct` are the raw reads; `now` is epoch ms.
    pub fn poll(&mut self, raw_title: &str, ps: PlayStatus, ct: i64, now: i64) -> Option<Action> {
        // Interval bookkeeping (the trailing-edge debounce) runs on a monotonic
        // clock; anchors below keep the epoch now.
        let clk = self.clock.tick(now);

        // New media: forget everything (Java :267-270).
        if self.have_prev && self.last_title != raw_title {
            self.have_prev = false;
            self.has_last_status = false;
            self.last_status = PlayStatus::STOPPED;
            self.frozen_ct = -1;
            self.prev_ct = -1;
            self.ct_window_len = 0;
            self.ct_window_idx = 0;
            self.pending_run = false;
            self.last_action = None;
        }

        // A not-ready read (ct<=0) must NOT overwrite the seek baseline
        // (:273-275).
        let ct_ok = ct > 0;
        let prev_ct_before = self.prev_ct;
        if ct_ok {
            self.sample_ct(ct);
            self.prev_ct = ct;
        }

        // ct ticks are coarse, so one delta is not proof of movement (:278).
        let moving = ct_ok && self.ct_window_len >= 2 && (ct - self.oldest_ct()) >= SETTLE_MOVE_MS;

        // Undefined carries no information: emit nothing, change no state (:281).
        if ps == PlayStatus::UNDEFINED {
            self.have_prev = true;
            return None;
        }

        // A non-Running report while the position is still moving, having
        // settled into Running, is a transient glitch: the player is really
        // playing (:285-286).
        let mut eff = ps;
        if ps != PlayStatus::RUNNING
            && moving
            && self.has_last_status
            && self.last_status == PlayStatus::RUNNING
        {
            eff = PlayStatus::RUNNING;
        }

        let changed = !self.have_prev || !self.has_last_status || eff != self.last_status;
        let seeked = !changed
            && eff == PlayStatus::RUNNING
            && self.has_last_status
            && self.last_status == PlayStatus::RUNNING
            && self.have_prev
            && ct_ok
            && abs64(ct - prev_ct_before) > SEEK_JUMP_MS;
        let paused_seek = !changed
            && eff == PlayStatus::PAUSED
            && self.has_last_status
            && self.last_status == PlayStatus::PAUSED
            && self.frozen_ct >= 0
            && ct_ok
            && abs64(ct - self.frozen_ct) > SEEK_JUMP_MS;

        let mut action = if eff == PlayStatus::RUNNING {
            if (changed || seeked) && ct_ok {
                Some(Action {
                    running: true,
                    has_start: true,
                    start: now - ct,
                    ct,
                    ..Action::default()
                })
            } else {
                None
            }
        } else if eff == PlayStatus::PAUSED {
            if (changed || paused_seek) && ct_ok {
                // No position suffix any more: the paused card carries a
                // "Paused" marker and counts the pause duration UP from
                // timestamps.start (set by the send gate), so there is no
                // frozen " - 00:00" artifact.
                self.frozen_ct = ct;
                Some(Action {
                    paused: true,
                    ct,
                    ..Action::default()
                })
            } else {
                None
            }
        } else if changed {
            // Stopped.
            Some(Action {
                ct,
                ..Action::default()
            })
        } else {
            None
        };

        // Fleet decision 10 (revised): debounce repeated RUN pushes (seek
        // bursts) within ~1 s unless the title changed, but HOLD the suppressed
        // anchor instead of dropping it. The old drop-then-forget behaviour lost
        // the seek re-anchor forever: the per-poll delta never re-fires, so the
        // card kept the pre-seek anchor. A `changed` RUN push is a real
        // state/title transition and always emits.
        if action.as_ref().is_some_and(|a| a.running) {
            if !changed
                && self.has_run_push
                && (clk - self.last_run_push_at) < RUN_DEBOUNCE_MS
                && raw_title == self.last_run_push_title
            {
                self.pending_run = true;
                self.pending_title = raw_title.to_string();
                action = None;
            } else {
                self.has_run_push = true;
                self.last_run_push_at = clk;
                self.last_run_push_title = raw_title.to_string();
                self.pending_run = false;
            }
        } else if action.is_some() {
            // Any non-RUN emit supersedes a held seek.
            self.pending_run = false;
        }

        // Trailing edge: once the window has expired the held seek lands with a
        // fresh anchor (start = now - ct), so the last seek of a scrub is never
        // lost.
        if action.is_none()
            && self.pending_run
            && ct_ok
            && eff == PlayStatus::RUNNING
            && raw_title == self.pending_title
            && (clk - self.last_run_push_at) >= RUN_DEBOUNCE_MS
        {
            action = Some(Action {
                running: true,
                has_start: true,
                start: now - ct,
                ct,
                ..Action::default()
            });
            self.has_run_push = true;
            self.last_run_push_at = clk;
            self.last_run_push_title = raw_title.to_string();
            self.pending_run = false;
        }

        // On emit only, remember title/status; have_prev is always set
        // (:306-308).
        if let Some(a) = action.as_ref() {
            self.last_title = raw_title.to_string();
            self.last_status = eff;
            self.has_last_status = true;
            self.last_action = Some(a.clone());
        }
        self.have_prev = true;
        action
    }

    /// The most recent emitted action, or `None` before any emit
    /// (`presence.go:234`). The send gate uses it to rebuild the card on every
    /// poll, including polls where the tracker emits nothing.
    pub fn current(&self) -> Option<Action> {
        self.last_action.clone()
    }

    /// Whether a RUN push was suppressed by the trailing-edge debounce and is
    /// waiting for the window to expire (`presence.go:244`).
    pub fn pending_debounce(&self) -> bool {
        self.pending_run
    }
}

/// `abs64` (`presence.go:246`): shared with the send gate's divergence check.
pub(crate) fn abs64(v: i64) -> i64 {
    if v < 0 { -v } else { v }
}

/// Mirrors Main.fmt (`presence.go:254`): MM:SS under an hour, else H:MM:SS.
pub fn fmt_ms(ms: i64) -> String {
    let seconds = ms / 1000;
    let s = seconds % 60;
    let m = (seconds / 60) % 60;
    let h = seconds / 3600;
    if h == 0 {
        format!("{m:02}:{s:02}")
    } else {
        format!("{h}:{m:02}:{s:02}")
    }
}

/// The paused card's state suffix: episode context plus this reads e.g.
/// `S1 E2 · Paused` (or just `Paused` with no context) (`presence.go:267`).
/// One separator glyph everywhere: the middle dot, never the dash the old
/// ` - Paused` marker mixed in.
pub(crate) const PAUSED_MARKER: &str = " · Paused";

/// The target budget (UTF-16 units) for synopsis-bearing State lines
/// (`presence.go:275`).
pub(crate) const STATE_SYNOPSIS_BUDGET: usize = 72;

/// Slot budgets (UTF-16 units), named in one place: the header (`name`), the
/// first body line (`details`) and the hover tooltip (`large_text`) each cap
/// at 128; the second body line (`state`) caps at 128 plain and at
/// [`STATE_SYNOPSIS_BUDGET`] when it carries a synopsis.
pub(crate) const NAME_BUDGET: usize = 128;
pub(crate) const DETAILS_BUDGET: usize = 128;
pub(crate) const STATE_BUDGET: usize = 128;
pub(crate) const LARGE_TEXT_BUDGET: usize = 128;

/// Discord caps `large_image` at 300 chars (`code=4000` otherwise). A
/// Kitsu-signed Backblaze URL can exceed it, so an over-budget URL is omitted
/// whole rather than truncated (a cut signed URL is a broken link). No
/// substitute is sent: the branded mark is never a missing-art fallback; no
/// new network calls.
pub(crate) const LARGE_IMAGE_BUDGET: usize = 300;

/// The body-line joiner: U+00B7 MIDDLE DOT with single spaces.
pub(crate) const JOINER: &str = " \u{b7} ";

/// The `1<<28` "no cap" budget the sanitizers use before a real cap is applied.
pub(crate) const HUGE: usize = 1 << 28;

/// Java's `\s` class (`[ \t\n\x0B\f\r]`), the class `^\s*-\s*` matches on.
const JAVA_WS: [char; 6] = [' ', '\t', '\n', '\u{0b}', '\u{0c}', '\r'];

/// Java's `^\s*-\s*` (`presence.go:284`) applied by hand, extended to the
/// middle-dot separator the paused marker now uses: a leading `-` or `·`
/// (with surrounding whitespace) is stripped when there is no context. The
/// workspace forbids a third-party regex crate here, and the pattern is a
/// single anchored match, so the hand-rolled form is exact. A string without
/// the separator does not match and is returned unchanged, exactly as
/// `replaceAll` would leave it.
fn strip_leading_dash(s: &str) -> &str {
    let after_ws = s.trim_start_matches(|c| JAVA_WS.contains(&c));
    let after_sep = after_ws
        .strip_prefix('-')
        .or(after_ws.strip_prefix('·'))
        .unwrap_or(after_ws);
    if after_sep.len() == after_ws.len() {
        return s;
    }
    after_sep.trim_start_matches(|c| JAVA_WS.contains(&c))
}

/// Mirrors Main.withStateTime (`presence.go:293`): context plus a suffix; a
/// leading dash is stripped when there is no context. Used for the paused card's
/// "Paused" marker ([`PAUSED_MARKER`]). Deviation from the Java reference: the
/// Java code truncates context+suffix together, which let the 128-unit cap eat
/// the tail of the marker (a 120-unit context + the 9-unit " - Paused" rendered
/// "... - Paus..."). Here the context is budgeted to `limit - len(suffix)`
/// first, so the suffix always survives whole and the result stays within the
/// limit.
pub fn with_state_time(context: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return context.to_string();
    }
    if context.is_empty() {
        return strip_leading_dash(suffix).to_string();
    }
    // `len` is BYTES in Go too (`128-len(suffix)`), so the budget is measured the
    // same way here.
    trunc(context, 128usize.saturating_sub(suffix.len())) + suffix
}

/// The Discord payload (contract section 5) (`presence.go:318-344`). `type_` is
/// always "PLAYING"; the fields not listed are deliberately unset.
///
/// Netflix-style mapping: the header (`name`) is the media title -- the movie
/// title, or the series title for an episode -- so the client renders
/// `Watching <title>`, never `Watching Potflix`. The first body line
/// (`details`) is the movie `<year> · <genres>` (and blanks for a series,
/// whose title the header now carries); the second body line (`state`) is the
/// movie runtime or the episode `SxxEyy · Episode`. `large_text` is the
/// work-level synopsis snippet (never episode-pinned prose); `large_image` is
/// the resolved artwork and nothing else. `paused` marks the parked pause
/// card for the send gate.
///
/// `small_image`/`small_text`/`buttons` are retained only so existing callers
/// keep compiling; the builders never populate them and the wire never
/// serializes them, so the card carries no corner icon and no click-through
/// links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    /// The activity type; always "PLAYING" from the builders.
    pub type_: String,
    /// The card header: the media title (movie title, or series title for an
    /// episode). Empty only on the idle card.
    pub name: String,
    /// The first body line.
    pub details: String,
    /// The second body line.
    pub state: String,
    /// The resolved https artwork URL, or empty when none resolved. Never the
    /// branded app mark.
    pub large_image: String,
    /// The synopsis snippet (<=128, explicitly ellipsized), or empty.
    pub large_text: String,
    /// True for the parked pause card. The gate keys its once-and-park behaviour
    /// off this, not off `small_image`.
    pub paused: bool,
    /// Legacy compat: never populated by the builders (no corner icon).
    pub small_image: String,
    /// Legacy compat: never populated by the builders (no corner icon).
    pub small_text: String,
    /// Whether `start` is meaningful.
    pub has_start: bool,
    /// Epoch ms anchor.
    pub start: i64,
    /// The projected end (epoch ms), set by the send gate while running and the
    /// duration is known. Serialized as `timestamps.end` while playing so
    /// Discord renders the remaining-time bar; zero (and omitted) while paused.
    pub end: i64,
    /// Legacy: the builders never populate it.
    pub buttons: Vec<Button>,
}

/// One presence-local card link (`presence.go:416-419`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Button {
    /// The button label.
    pub label: String,
    /// The button URL.
    pub url: String,
}

/// Composes the payload for one action from the raw window title
/// (`presence.go:349`, Java pushActivity :139-191). The idle card is used when
/// the title carries no media name.
///
/// LEGACY filename-only mapping: the shipped send path composes through
/// [`compose_card`] (per-kind specs, metadata-resolved lines), and the
/// headless `--selftest` positive control goes through
/// [`build_activity_resolved`] for the same reason. This stays as the
/// no-metadata fallback the unit tests pin, not as a second shipped mapping.
pub fn build_activity(raw_title: &str, a: Option<&Action>) -> Activity {
    let empty = Action::default();
    let a = a.unwrap_or(&empty);
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    if raw.trim().is_empty() || raw == APP_NAME {
        return Activity {
            type_: "PLAYING".to_string(),
            details: "Browsing".to_string(),
            state: "Idle".to_string(),
            // No assets on the idle card: no branded mark as artwork, no icon.
            ..Activity::default()
        };
    }

    let c = clean(raw);
    let title = trunc(&c.title, 128);
    // First body line: the episode name when known, else the episode tag, else
    // the title.
    let mut details = title.clone();
    if !c.episode_name.is_empty() {
        details = trunc(&c.episode_name, 128);
    } else if !c.episode_tag.is_empty() {
        details = trunc(&c.episode_tag, 128);
    }
    // Second body line: the episode tag, but only when it adds information
    // beyond the first line. The comparison uses the FINAL emitted values: both
    // sides are truncated to the 128 cap first, so a tag longer than the cap
    // cannot repeat the details line it already fills.
    let mut state = String::new();
    let tag = trunc(&c.episode_tag, 128);
    if !tag.is_empty() && tag != details {
        state = tag;
    }

    // Paused: the state is the episode tag plus a "Paused" marker, not a frozen
    // position. The pause duration is the count-UP timer anchored by the gate
    // (timestamps.start = the pause moment).
    if a.paused {
        state = with_state_time(&state, PAUSED_MARKER);
    }
    let mut act = Activity {
        type_: "PLAYING".to_string(),
        details,
        state,
        // No artwork is resolved on the filename-only path, and no branded mark
        // is ever substituted for it.
        paused: a.paused,
        ..Activity::default()
    };
    // The header is the media title on every path. The shipped per-kind
    // composer sets the same identity; this fallback is pinned by its own
    // unit tests, not sent.
    act.name = title;
    // No small_image/small_text on any branch: the card carries no
    // play/pause/stop corner icon. The pause is signalled by `paused` (for the
    // gate) and the " · Paused" state marker (for the viewer).
    if a.has_start {
        act.has_start = true;
        act.start = a.start;
    }
    act
}

/// Canonical titles from a metadata lookup; a zero field is ignored
/// (`presence.go:422-432`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// Canonical series or movie title.
    pub series: String,
    /// Canonical episode title.
    pub episode_name: String,
    /// Cover/poster URL (https); zero value = absent.
    pub artwork: String,
    /// One-line plain text; zero value = absent. Whether it may ride the
    /// Discord artwork tooltip is decided by [`Resolved::synopsis_is_work`]:
    /// episode-pinned prose never rides (it spoils the episode).
    pub synopsis: String,
    /// Provenance of [`Resolved::synopsis`]: true when the note describes the
    /// WORK (a movie synopsis, a series/show synopsis), false when it is
    /// pinned to a single EPISODE (the series latch's episode pin). Default
    /// is episode: episode prose must NEVER ride the tooltip, so `large_text`
    /// is sent only when this is true and there is no episode-only prose to
    /// protect. The 128-unit cap still applies.
    pub synopsis_is_work: bool,
    /// Movie genres, e.g. "Action, Drama"; zero value = absent.
    pub genres: String,
    /// Movie runtime, e.g. "2h 19m"; zero value = absent.
    pub length: String,
    /// Movie release year; <=0 = absent.
    pub year: i32,
    /// True: movie card lines; false: series card lines.
    pub is_movie: bool,
    /// Season number of the playing episode; <=0 = unknown. Filled from the
    /// latch pin or query ([`crate::episode_code`]'s `Sxx` half) and from the
    /// resolver answer; never fabricated from an absolute number.
    pub season: i64,
    /// Episode number within the season; <=0 = unknown. The `Exx` half.
    pub number: i32,
    /// Absolute/anime episode number; <=0 = absent. Renders `E02` only when
    /// `season`/`number` carry no code, so an absolute episode never grows a
    /// season it was never given.
    pub absolute: i32,
    /// Ignored: the pristine card carries no links.
    pub buttons: Vec<Button>,
    /// Kind::UNKNOWN fallback: the title parsed but nothing resolved. The card
    /// is the cleaned title plus the parsed code if any: no art, no prose, no
    /// timestamps. A parsed-but-unmatched series/movie (provisional card) is
    /// NOT unknown -- it keeps its progress bar.
    pub unknown: bool,
}

/// Sanitizes free text for the RPC card (`presence.go:438`): trim, collapse runs
/// of whitespace/newlines/tabs to single spaces, strip C0 control chars, then
/// truncate to `max` via [`titleclean::trunc`]. Internal underscores and other
/// real title content are preserved.
pub(crate) fn rp_text(s: &str, max: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let stripped: String = collapsed
        .chars()
        .filter(|c| !(*c < '\u{20}' || *c == '\u{7f}'))
        .collect();
    trunc(&stripped, max)
}

/// Sanitizes `s` via [`rp_text`], then caps it at `max` UTF-16 units at a word
/// boundary (`presence.go:454`): when the sanitized text fits it is returned
/// as-is, byte for byte; otherwise the prefix fitting in `max-1` units is cut
/// at its last space and a single U+2026 is appended, so a cut ends on a whole
/// word wherever the text has word boundaries. With no space in the prefix it
/// still signals the cut: the prefix fitting in `max-1` units (cut on a char
/// boundary, never a split surrogate) carries a single trailing U+2026 within
/// the `max` budget — a spaceless run (one long token, CJK text) cut on a
/// character boundary. A cut landing on a sentence end — the last kept
/// character is `.`/`!`/`?` and the character after it is whitespace or
/// end-of-text — keeps that sentence with its own terminator and carries no
/// ellipsis.
///
/// DEPARTURE from the frozen reference (`presence.go:454`): the cut base is
/// stripped of trailing punctuation before the ellipsis is appended. Go kept
/// the comma/semicolon the clause cut stopped at, so the wire carried
/// `Majin Buu,…` / `meet Elliot;…` (and a mid-word `…and Ernest` rode the
/// tooltip's hard cut at the call site); here every truncated line ends on a
/// word character plus U+2026, or on nothing extra when nothing was cut. A
/// truncated line is always signalled — either a complete sentence that keeps
/// its own terminator with no ellipsis, or a single U+2026 that ends the line.
/// A line is never silently truncated.
pub(crate) fn trim_word(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let full = rp_text(s, HUGE);
    if utf16_units(&full) <= max {
        return full;
    }
    if max == 1 {
        return "\u{2026}".to_string();
    }
    let runes: Vec<char> = full.chars().collect();
    let mut used = 0usize;
    let mut end = 0usize;
    for (i, r) in runes.iter().enumerate() {
        let sz = r.len_utf16();
        if used + sz > max - 1 {
            break;
        }
        used += sz;
        end = i + 1;
    }
    let prefix: String = runes[..end].iter().collect();
    if !prefix.is_empty()
        && let Some(idx) = prefix.rfind(' ')
        && idx > 0
    {
        let base = strip_cut_trail(&prefix[..idx]);
        if base.is_empty() {
            return "\u{2026}".to_string();
        }
        // Sentence-end cut (matches `smart_trim_synopsis`): the last kept
        // character is `.`/`!`/`?` and the next character is whitespace or
        // end-of-text, so keep the complete sentence with its own terminator
        // and no ellipsis.
        if let Some(last) = base.chars().next_back()
            && matches!(last, '.' | '!' | '?')
        {
            let kept = base.chars().count();
            if (kept == runes.len() || runes[kept].is_whitespace())
                && utf16_units(base) <= max
            {
                return base.to_string();
            }
        }
        return format!("{base}\u{2026}");
    }
    // No word boundary in the prefix: still signal the cut. The already
    // sanitized `prefix` (char boundary, `max-1` units) carries a single
    // trailing U+2026 within budget; stripping only shrinks, so the budget
    // still holds and no surrogate pair is split.
    let base = strip_cut_trail(&prefix);
    if base.is_empty() {
        return "\u{2026}".to_string();
    }
    // Same sentence-end rule on the spaceless fallback: a character-boundary
    // cut landing on `.`/`!`/`?` followed by whitespace or end-of-text keeps
    // the sentence with no ellipsis.
    if let Some(last) = base.chars().next_back()
        && matches!(last, '.' | '!' | '?')
    {
        let kept = base.chars().count();
        if (kept == runes.len() || runes[kept].is_whitespace()) && utf16_units(base) <= max {
            return base.to_string();
        }
    }
    format!("{base}\u{2026}")
}

/// Trailing marks stripped from a cut BEFORE the U+2026 terminator is appended:
/// the clause separators (`,`, `;`, `:`), the dash family (`-`, `–`, `—`), the
/// middle dot joiner (`·`), and surrounding whitespace. A cut that stopped at
/// `Majin Buu,` or `meet Elliot;` therefore ends `Buu…` / `Elliot…`, never
/// `,…` / `;…`. Sentence-terminal `.`/`!`/`?` are deliberately NOT in this set:
/// the sentence-end path returns a complete sentence with its own terminator
/// and no ellipsis, and must keep it.
fn strip_cut_trail(s: &str) -> &str {
    let mut out = s;
    loop {
        let trimmed = out.trim_end_matches(|c: char| {
            c.is_whitespace() || matches!(c, ',' | ';' | ':' | '-' | '–' | '—' | '·')
        });
        if trimmed.len() == out.len() {
            return out;
        }
        out = trimmed;
    }
}

/// Words that must never dangle before the ellipsis (`presence.go:490-497`):
/// "... and" / "... the" reads as a cut-off thought, so they are stripped first.
const SYNOPSIS_CONNECTORS: [&str; 34] = [
    "and", "or", "but", "the", "a", "an", "of", "to", "in", "on", "with", "for", "from", "by",
    "as", "at", "into", "than", "that", "which", "while", "when", "his", "her", "their", "its",
    "this", "these", "those", "is", "are", "was", "were", "who",
];

/// The runes Go's `strings.Trim` cutset names (`presence.go:514`).
const CONNECTOR_TRIM: &str = ",;:.!?\"'()[]-.\u{b7}";

/// Drops trailing connector words (case-insensitive) from `base` so the ellipsis
/// never follows "and"/"the"/etc. (`presence.go:501`).
fn strip_dangling_connector(base: &str) -> String {
    let mut base = base.to_string();
    loop {
        let t = base.trim().to_string();
        if t.is_empty() {
            return String::new();
        }
        let idx = t.rfind(' ');
        let (last, rest) = match idx {
            Some(i) => (&t[i + 1..], &t[..i]),
            None => (t.as_str(), ""),
        };
        let clean = last
            .trim_matches(|c| CONNECTOR_TRIM.contains(c))
            .to_lowercase();
        if clean.is_empty() {
            return t;
        }
        if !SYNOPSIS_CONNECTORS.contains(&clean.as_str()) {
            return t;
        }
        if idx.is_none() {
            return String::new();
        }
        let next = rest.to_string();
        base = next;
    }
}

/// How many leading runes of `rs` fit in `max_units` UTF-16 units
/// (`presence.go:529`).
fn fit_runes(rs: &[char], max_units: usize) -> usize {
    if max_units == 0 {
        return 0;
    }
    let mut used = 0usize;
    for (i, r) in rs.iter().enumerate() {
        let sz = r.len_utf16();
        if used + sz > max_units {
            return i;
        }
        used += sz;
    }
    rs.len()
}

/// The smallest leading rune count of `rs[..end]` whose UTF-16 length reaches
/// `target_units` (the start of the back half) (`presence.go:549`).
fn rune_count_for_units(rs: &[char], end: usize, target_units: usize) -> usize {
    if target_units == 0 {
        return 0;
    }
    let mut used = 0usize;
    for (i, r) in rs.iter().enumerate().take(end) {
        used += r.len_utf16();
        if used >= target_units {
            return i + 1;
        }
    }
    0
}

/// `String.length()`: UTF-16 code units, the unit the Java-exact
/// [`titleclean::trunc`] limit is measured in.
fn utf16_units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// [`trim_word`] with a seamless cut (Netflix: break after punctuation, single
/// U+2026; Discord: snippet, one line), all measured in UTF-16 units
/// (`presence.go:573`): fits unchanged, byte for byte; else last sentence end
/// (.!?, end-or-space) in the back half with NO ellipsis; else last clause
/// boundary (, ; space-dash variants) in the back half plus U+2026; else last
/// word boundary plus U+2026 (dangling connectors stripped first); else a
/// char-boundary hard cut to `max-1` units plus U+2026 (never a split
/// surrogate); never mid-word, never "...".
///
/// DEPARTURE from the frozen reference (`presence.go:573-644`): every cut that
/// takes the ellipsis also strips trailing punctuation first (see
/// [`trim_word`]), so the wire carries `Buu…`, never `Buu,…`. A truncated line
/// is always signalled — either a complete sentence that keeps its own
/// terminator with no ellipsis, or a single U+2026 that ends the line. A line
/// is never silently truncated.
pub(crate) fn smart_trim_synopsis(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let full = rp_text(s, HUGE);
    if utf16_units(&full) <= max {
        return full;
    }
    if max == 1 {
        return "\u{2026}".to_string();
    }
    let runes: Vec<char> = full.chars().collect();
    let end_fit = fit_runes(&runes, max);
    let half_fit = rune_count_for_units(&runes, end_fit, max / 2);
    for i in (half_fit..end_fit).rev() {
        let c = runes[i];
        if (c == '.' || c == '!' || c == '?')
            && (i + 1 == runes.len() || runes[i + 1].is_whitespace())
        {
            let cand = runes[..=i].iter().collect::<String>().trim().to_string();
            if utf16_units(&cand) <= max {
                return cand;
            }
        }
    }
    let end_ell = fit_runes(&runes, max - 1);
    let half_ell = rune_count_for_units(&runes, end_ell, (max - 1) / 2);
    let mut upper = end_ell;
    if upper >= runes.len() {
        upper = runes.len() - 1;
    }
    for i in (half_ell..=upper).rev() {
        if i < end_ell
            && (runes[i] == ',' || runes[i] == ';')
            && i + 1 < runes.len()
            && runes[i + 1].is_whitespace()
        {
            let cleaned = strip_dangling_connector(
                runes[..=i].iter().collect::<String>().trim(),
            );
            let base = strip_cut_trail(&cleaned);
            if base.is_empty() {
                return "\u{2026}".to_string();
            }
            return format!("{base}\u{2026}");
        }
        if (runes[i] == '-' || runes[i] == '\u{2013}' || runes[i] == '\u{2014}')
            && i >= 1
            && i + 1 < runes.len()
            && runes[i - 1] == ' '
            && runes[i + 1].is_whitespace()
        {
            let start = i - 1;
            if start >= half_ell && start < end_ell {
                let cleaned = strip_dangling_connector(
                    runes[..start].iter().collect::<String>().trim(),
                );
                let base = strip_cut_trail(&cleaned);
                if base.is_empty() {
                    return "\u{2026}".to_string();
                }
                return format!("{base}\u{2026}");
            }
        }
    }
    for i in (0..end_ell).rev() {
        if runes[i] == ' ' {
            let cleaned =
                strip_dangling_connector(runes[..i].iter().collect::<String>().trim());
            let base = strip_cut_trail(&cleaned);
            if base.is_empty() {
                return "\u{2026}".to_string();
            }
            return format!("{base}\u{2026}");
        }
    }
    // No boundary anywhere: still signal the cut. Hard-cut the sanitized text
    // on a char boundary to `max-1` units with a single trailing U+2026,
    // staying within budget without splitting a surrogate pair.
    let prefix: String = runes[..end_ell].iter().collect();
    let base = strip_cut_trail(&prefix);
    if base.is_empty() {
        return "\u{2026}".to_string();
    }
    format!("{base}\u{2026}")
}

/// The Discord artwork-tooltip snippet: the stored synopsis at
/// [`LARGE_TEXT_BUDGET`] UTF-16 units. A note that fits rides whole; a longer
/// note is cut on a character boundary (word-aware when the text has spaces)
/// and ALWAYS ends with a single U+2026, so a truncated Discord tooltip is
/// never mistaken for the complete note. The complete stored note stays in
/// Potflix's own hover/focus tooltip; this function is only the snippet that
/// fits Discord's 128-unit `large_text` cap.
pub(crate) fn synopsis_snippet(s: &str) -> String {
    let full = rp_text(s, HUGE);
    if utf16_units(&full) <= LARGE_TEXT_BUDGET {
        return full;
    }
    if LARGE_TEXT_BUDGET <= 1 {
        return "\u{2026}".to_string();
    }
    let runes: Vec<char> = full.chars().collect();
    let mut used = 0usize;
    let mut end = 0usize;
    for (i, r) in runes.iter().enumerate() {
        let sz = r.len_utf16();
        if used + sz > LARGE_TEXT_BUDGET - 1 {
            break;
        }
        used += sz;
        end = i + 1;
    }
    let mut prefix: String = runes[..end].iter().collect();
    // Prefer a whole word: cut at the last space inside the budget. A spaceless
    // run keeps the character-boundary prefix.
    if let Some(pos) = prefix.rfind(' ') {
        prefix.truncate(pos);
    }
    let base = strip_cut_trail(prefix.trim())
        .trim_end_matches(['.', '!', '?'])
        .trim_end();
    if base.is_empty() {
        return "\u{2026}".to_string();
    }
    format!("{base}\u{2026}")
}

/// The header identity: the canonical series/movie title, else the clean
/// filename title. Empty only for the idle card (the caller returns early).
fn compose_name_inner(raw_title: &str, r: &Resolved) -> String {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    if raw.trim().is_empty() || raw == APP_NAME {
        return String::new();
    }
    if !r.series.is_empty() {
        return trunc(&r.series, NAME_BUDGET);
    }
    let title = trunc(&clean(raw).title, NAME_BUDGET);
    if title.is_empty() {
        rp_text(raw.trim(), NAME_BUDGET)
    } else {
        title
    }
}

/// The Discord card header exactly as the client renders it: `Watching <name>`.
/// `name` is the media title (the movie title, or the series title for an
/// episode), never the application name; an empty `name` (the idle card) has no
/// header line. One definition, so the Card-view preview and the wire can never
/// disagree about the header.
pub fn header_line(a: &Activity) -> String {
    if a.name.trim().is_empty() {
        return String::new();
    }
    format!("Watching {}", a.name)
}

/// The hover slot: the synopsis snippet, but ONLY when the note describes
/// the work ([`Resolved::synopsis_is_work`]). Episode-pinned prose (the
/// series latch's playing-episode summary) must never ride the tooltip -- it
/// spoils the episode -- so where only episode prose exists this sends
/// nothing. Discord has no dedicated synopsis field, so the complete stored
/// note lives in Potflix's own hover/focus tooltip and this slot carries the
/// <=128-unit, explicitly ellipsized snippet. A movie's note is work-level
/// and rides; a series' episode pin never does. No work synopsis means no
/// tooltip (empty, omitted on the wire), never a duplicate of the title.
fn compose_large_text_inner(r: &Resolved, _is_movie: bool) -> String {
    if r.synopsis_is_work {
        synopsis_snippet(&r.synopsis)
    } else {
        String::new()
    }
}

/// The artwork slot: the https URL when present and within
/// [`LARGE_IMAGE_BUDGET`], else EMPTY. The card never substitutes the branded
/// app mark for missing artwork (owner decision 2026-10-28): an unresolved
/// poster means no image, never a Potflix icon standing in for the media. Never
/// truncated and never over budget (Discord rejects `large_image` longer than
/// 300 chars); an over-budget signed URL is dropped for the same reason.
fn compose_large_image_inner(r: &Resolved) -> String {
    if (r.artwork.starts_with("http://") || r.artwork.starts_with("https://"))
        && r.artwork.len() <= LARGE_IMAGE_BUDGET
    {
        r.artwork.clone()
    } else {
        String::new()
    }
}

/// One state-ladder rung: the filename episode tag, or "" when absent. A
/// present rung already carries the paused marker when paused.
fn rung_tag(raw_title: &str, a: Option<&Action>) -> String {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    let mut tag_raw = String::new();
    if !(raw.trim().is_empty() || raw == APP_NAME) {
        tag_raw = clean(raw).episode_tag;
    }
    let tag = rp_text(&tag_raw, STATE_BUDGET);
    if tag.is_empty() {
        return String::new();
    }
    if a.is_some_and(|x| x.paused) {
        with_state_time(&tag, PAUSED_MARKER)
    } else {
        tag
    }
}

/// One state-ladder rung: genres and length joined with [`JOINER`], or "" when
/// both are absent.
fn rung_genres_length(r: &Resolved, a: Option<&Action>) -> String {
    let g = rp_text(&r.genres, HUGE);
    let l = rp_text(&r.length, HUGE);
    let mut parts: Vec<&str> = Vec::new();
    if !g.is_empty() {
        parts.push(&g);
    }
    if !l.is_empty() {
        parts.push(&l);
    }
    let prefix = parts.join(JOINER);
    if prefix.is_empty() {
        return String::new();
    }
    if a.is_some_and(|x| x.paused) {
        with_state_time(
            &trim_word(&prefix, STATE_BUDGET - PAUSED_MARKER.len()),
            PAUSED_MARKER,
        )
    } else {
        trim_word(&prefix, STATE_BUDGET)
    }
}

/// One state-ladder rung: the synopsis alone at [`STATE_SYNOPSIS_BUDGET`], or
/// "" when absent.
fn rung_synopsis_alone(r: &Resolved, a: Option<&Action>) -> String {
    if rp_text(&r.synopsis, HUGE).is_empty() {
        return String::new();
    }
    if a.is_some_and(|x| x.paused) {
        with_state_time(
            &smart_trim_synopsis(&r.synopsis, STATE_SYNOPSIS_BUDGET - PAUSED_MARKER.len()),
            PAUSED_MARKER,
        )
    } else {
        smart_trim_synopsis(&r.synopsis, STATE_SYNOPSIS_BUDGET)
    }
}

/// The tail rung of every ladder: the clean filename title, so a slot is never
/// blank. Falls back to the sanitized raw title when the parser yields nothing.
fn rung_clean_title(raw_title: &str, a: Option<&Action>) -> String {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    if raw.trim().is_empty() || raw == APP_NAME {
        return String::new();
    }
    let title = trunc(&clean(raw).title, STATE_BUDGET);
    let base = if title.is_empty() {
        rp_text(raw.trim(), STATE_BUDGET)
    } else {
        title
    };
    if base.is_empty() {
        return String::new();
    }
    if a.is_some_and(|x| x.paused) {
        with_state_time(&base, PAUSED_MARKER)
    } else {
        base
    }
}

/// The shipped MOVIE details line: `<year> · <genre1>, <genre2>`. The title
/// never appears here (the header already carries it). Over budget the line
/// drops whole trailing genres first -- never an ellipsis: a cut genre list
/// reads as the complete list, while `,…` would read as a truncation. The
/// year alone always fits, so the line is empty only when both facts are
/// missing.
fn movie_details_line(r: &Resolved) -> String {
    let year = if r.year > 0 {
        r.year.to_string()
    } else {
        String::new()
    };
    let genres: Vec<&str> = r
        .genres
        .split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .collect();
    let mut keep = genres.len();
    loop {
        let line = match (year.is_empty(), keep) {
            (true, 0) => String::new(),
            (true, _) => genres[..keep].join(", "),
            (_, 0) => year.clone(),
            _ => format!("{year}{JOINER}{}", genres[..keep].join(", ")),
        };
        if utf16_units(&line) <= DETAILS_BUDGET || keep == 0 {
            return line;
        }
        keep -= 1;
    }
}

/// The one `SxxExx` formatter (T1): season+number renders `S01E02`
/// (zero-padded to at least two digits, so `S12E34`). A season-0/absent
/// season never fabricates `S00`: AniList absolute episodes arrive shaped
/// `(season 0, number = absolute, absolute = ...)` and render `E02` like any
/// other season-less episode. When season carries no code but both numbers
/// are set and disagree (`season 0, number != absolute`), the absolute number
/// wins (`E{absolute}`): the absolute count is the provider's own episode
/// index, while the bare number in a season-0 shape is the specials-slot
/// position. There is no distinct signal for a real season-0 special in this
/// shape, so it renders `E0N` the same way; an absolute-only episode renders
/// `E02` and never fabricates a season; a tag already matching `SxxExx`
/// (compact, case-insensitive, no space) passes through normalized; anything
/// with no usable number yields `""` and the caller falls back, so a missing
/// number omits the code instead of inventing one.
pub(crate) fn episode_code(season: i64, number: i32, absolute: i32, tag: &str) -> String {
    if season > 0 && number > 0 {
        return format!("S{season:02}E{number:02}");
    }
    if absolute > 0 {
        return format!("E{absolute:02}");
    }
    if number > 0 {
        return format!("E{number:02}");
    }
    passthrough_sxxexx(tag).unwrap_or_default()
}

/// A tag already shaped `SxxExx` passes through normalized (`s1e2` becomes
/// `S01E02`); anything else -- the spaced `S1 E2` filename tag, an episode
/// name, an empty tag -- is `None` and keeps the current tag behavior at the
/// call site. Hand-rolled: the workspace forbids a regex crate here.
fn passthrough_sxxexx(tag: &str) -> Option<String> {
    let rest = tag
        .trim()
        .strip_prefix('S')
        .or(tag.trim().strip_prefix('s'))?;
    let (season_digits, rest) = split_digits(rest)?;
    let rest = rest.strip_prefix('E').or(rest.strip_prefix('e'))?;
    let (number_digits, rest) = split_digits(rest)?;
    if !rest.is_empty() {
        return None;
    }
    let season: u32 = season_digits.parse().ok()?;
    let number: u32 = number_digits.parse().ok()?;
    Some(format!("S{season:02}E{number:02}"))
}

/// Splits leading ASCII digits off `s`; `None` when there are none.
fn split_digits(s: &str) -> Option<(&str, &str)> {
    let n = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    Some(s.split_at(n))
}

/// The UNKNOWN state line: the parsed episode code if any, else the filename
/// tag as-is, else empty. Never the episode name (no prose on an unresolved
/// card), never normalized into a season the lookup did not give.
fn unknown_code_state(raw_title: &str, r: &Resolved) -> String {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    if raw.trim().is_empty() || raw == APP_NAME {
        return String::new();
    }
    let tag = clean(raw).episode_tag;
    let code = episode_code(r.season, r.number, r.absolute, &tag);
    if !code.is_empty() {
        return code;
    }
    rp_text(&tag, STATE_BUDGET)
}

/// The details fill ladder. Movie `default` is `<year> · <genres>` (whole
/// genres dropped over budget, never ellipsized, never the title -- the
/// header already carries it); series `default` is the canonical series
/// title, else the clean filename title (blanked downstream when it would
/// repeat the header). The episode title/code never reach line 1 under the
/// default. The retired `title`/`series`/`episode` ids keep their legacy
/// meanings for old in-flight specs. Movie branches never consult the episode
/// code.
pub(crate) fn compose_details_for(
    raw_title: &str,
    r: &Resolved,
    is_movie: bool,
    details_preset: &str,
) -> String {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    let canonical_ep = if r.episode_name.is_empty() {
        String::new()
    } else {
        trunc(&r.episode_name, DETAILS_BUDGET)
    };
    let canonical_series = if r.series.is_empty() {
        String::new()
    } else {
        trunc(&r.series, DETAILS_BUDGET)
    };
    let canonical_series_year = if !r.series.is_empty() && is_movie && r.year > 0 {
        trunc(&format!("{} ({})", r.series, r.year), DETAILS_BUDGET)
    } else {
        canonical_series.clone()
    };
    // The episode code never reaches the details line; movies never consult
    // it either.
    // Owner mapping: MOVIE line 1 is `<year> · <genres>` (never the title --
    // the header carries it); SERIES line 1 is the canonical series title,
    // never the episode title or the episode code. Both fall back to the
    // clean filename title when the lookup is a miss. Over-budget genres drop
    // whole (see `movie_details_line`); a word-aware cut would read as a
    // truncation, a dropped genre reads as the complete list.
    let title_base = if !r.series.is_empty() {
        r.series.clone()
    } else {
        let t = clean(raw).title;
        if t.is_empty() {
            rp_text(raw.trim(), HUGE)
        } else {
            t
        }
    };
    let default_line = if is_movie {
        movie_details_line(r)
    } else {
        trim_word(&title_base, DETAILS_BUDGET)
    };
    match details_preset {
        cardspec::CARD_DETAILS_TITLE => {
            let header = compose_name_inner(raw_title, r);
            if header.is_empty() {
                default_line
            } else {
                header
            }
        }
        cardspec::CARD_DETAILS_EPISODE => {
            if is_movie {
                default_line
            } else if !canonical_ep.is_empty() {
                canonical_ep
            } else {
                default_line
            }
        }
        cardspec::CARD_DETAILS_SERIES => {
            if !canonical_series.is_empty() {
                canonical_series_year
            } else {
                default_line
            }
        }
        _ => default_line,
    }
}

/// The state fill ladder (first non-empty rung wins; only `none` may empty the
/// line, plus any rung whose final text would equal the series title). Series
/// `default` is `SxxEyy · Episode` (the filename tag when no code is available,
/// the episode title else the filename's own episode name): over budget the
/// episode title drops whole before the code is ever touched. The explicit
/// `tag`/`synopsis` presets keep their legacy shapes. Movie `default` is the
/// runtime alone, plus the paused marker; the year and genres live on the
/// details line.
/// The paused marker rides the last populated body line, placed by the caller
/// (`CardSpec::render_kind`); an empty base stays empty here, never a bare
/// marker.
///
/// The shipped MOVIE state line is the runtime alone -- e.g. `2h 46m` -- plus
/// the paused marker when paused. Genres live on the details line, so they
/// are never repeated here; the title never appears on either body line.
fn default_movie_state(r: &Resolved, a: Option<&Action>) -> String {
    let paused = a.is_some_and(|x| x.paused);
    let base = rp_text(&r.length, STATE_BUDGET);
    if base.is_empty() {
        // No runtime: the line stays empty and the paused marker rides the
        // last populated body line, placed by the caller -- never a bare
        // marker here.
        return String::new();
    }
    let cap = if paused {
        STATE_BUDGET - PAUSED_MARKER.len()
    } else {
        STATE_BUDGET
    };
    let line = trim_word(&base, cap);
    if paused {
        with_state_time(&line, PAUSED_MARKER)
    } else {
        line
    }
}

/// The shipped SERIES episode state line: `SxxEyy • Episode`. The code comes
/// from the resolved season/number (absolute when that is all the provider
/// gives); the episode title is the resolved title, else the filename's own
/// episode name. With no code and no title the filename tag is the fallback;
/// with no episode fact at all the line stays empty rather than repeat the
/// series title that now lives on the details line. Over budget the episode
/// title drops whole before the code is touched. The paused marker rides the
/// line when the line is populated; placement on the last populated body line
/// is the caller's job.
fn default_episode_state(raw_title: &str, a: Option<&Action>, r: &Resolved) -> String {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    let c = clean(raw);
    let code = episode_code(r.season, r.number, r.absolute, &c.episode_tag);
    // No resolved numbers: the filename's own tag is the only honest code, and
    // it rides as-is (never normalized into a season the lookup did not give).
    let code = if code.is_empty() {
        rp_text(&c.episode_tag, STATE_BUDGET)
    } else {
        code
    };
    let ep = if r.episode_name.is_empty() {
        rp_text(&c.episode_name, HUGE)
    } else {
        rp_text(&r.episode_name, HUGE)
    };
    let joined = if !code.is_empty() && !ep.is_empty() {
        format!("{code}{JOINER}{ep}")
    } else if !code.is_empty() {
        code.clone()
    } else if !ep.is_empty() {
        ep
    } else {
        rp_text(&c.episode_tag, STATE_BUDGET)
    };
    let paused = a.is_some_and(|x| x.paused);
    if joined.is_empty() {
        // No episode fact: the line stays empty; the paused marker rides the
        // last populated body line, placed by the caller -- never a bare
        // marker here.
        return String::new();
    }
    let cap = if paused {
        STATE_BUDGET - PAUSED_MARKER.len()
    } else {
        STATE_BUDGET
    };
    // Over budget the episode title drops whole before the code is touched:
    // the code is the identity, the title the decoration.
    let line = if utf16_units(&joined) <= cap {
        joined
    } else if !code.is_empty() && utf16_units(&code) <= cap {
        code
    } else {
        trim_word(&joined, cap)
    };
    if paused {
        with_state_time(&line, PAUSED_MARKER)
    } else {
        line
    }
}

pub(crate) fn compose_state_for(
    raw_title: &str,
    a: Option<&Action>,
    r: &Resolved,
    is_movie: bool,
    state_preset: &str,
) -> String {
    let line = compose_state_inner(raw_title, a, r, is_movie, state_preset);
    // No body line ever repeats the header: a clean-title fallback (or, in
    // principle, any other rung) equal to the header blanks, keeping the
    // deliberate one-line shape. Paused keeps the marker (`"Paused"`).
    if line.is_empty() {
        return line;
    }
    let header = compose_name_inner(raw_title, r);
    if header.is_empty() {
        return line;
    }
    if line == header {
        return String::new();
    }
    if line == with_state_time(&header, PAUSED_MARKER) {
        return with_state_time("", PAUSED_MARKER);
    }
    line
}

fn compose_state_inner(
    raw_title: &str,
    a: Option<&Action>,
    r: &Resolved,
    is_movie: bool,
    state_preset: &str,
) -> String {
    if state_preset == cardspec::CARD_STATE_NONE {
        return String::new();
    }
    let paused = a.is_some_and(|x| x.paused);
    // The combined rung follows the explicit kind, not the stored flag. It is
    // only live when a synopsis exists: `synopsis_state` parks an empty
    // synopsis as "Paused", which must fall through the ladder instead.
    let mut shadow = r.clone();
    shadow.is_movie = is_movie;
    let combined = if rp_text(&r.synopsis, HUGE).is_empty() {
        String::new()
    } else {
        synopsis_state(raw_title, a, &shadow)
    };
    let tag = rung_tag(raw_title, a);
    let synopsis = rung_synopsis_alone(r, a);
    let clean = rung_clean_title(raw_title, a);
    if is_movie {
        match state_preset {
            cardspec::CARD_STATE_TAG => {
                if !tag.is_empty() {
                    return tag;
                }
                let gl = rung_genres_length(r, a);
                if !gl.is_empty() {
                    return gl;
                }
                if !synopsis.is_empty() {
                    return synopsis;
                }
                if !clean.is_empty() {
                    return clean;
                }
            }
            cardspec::CARD_STATE_SYNOPSIS => {
                if !combined.is_empty() {
                    return combined;
                }
                if !synopsis.is_empty() {
                    return synopsis;
                }
                if !clean.is_empty() {
                    return clean;
                }
            }
            _ => {
                return default_movie_state(r, a);
            }
        }
    } else {
        match state_preset {
            cardspec::CARD_STATE_TAG => {
                if !tag.is_empty() {
                    return tag;
                }
                if !synopsis.is_empty() {
                    return synopsis;
                }
                if !clean.is_empty() {
                    return clean;
                }
            }
            cardspec::CARD_STATE_SYNOPSIS => {
                if !combined.is_empty() {
                    return combined;
                }
                if !tag.is_empty() {
                    return tag;
                }
                if !clean.is_empty() {
                    return clean;
                }
            }
            _ => {
                return default_episode_state(raw_title, a, r);
            }
        }
    }
    if paused {
        return with_state_time("", PAUSED_MARKER);
    }
    String::new()
}

/// The single card composer the app's push path and the Card-view preview both
/// call (`presence.go:662`): one card with a per-kind fill
/// ladder (a bare movie default with no year/genres/length stays
/// empty rather than repeating the title). `is_movie` drives the
/// kind branches; the idle card keeps its shipped lines under any spec.
///
/// The header (`name`) is always the media title, so the client renders
/// `Watching <title>`; the artwork slot is the resolved poster URL or empty
/// (never the branded mark), and the small status slot is never populated.
pub fn compose_card(
    raw_title: &str,
    a: Option<&Action>,
    r: &Resolved,
    is_movie: bool,
    spec: &CardSpec,
) -> Activity {
    let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
    if raw.trim().is_empty() || raw == APP_NAME {
        return Activity {
            type_: "PLAYING".to_string(),
            details: "Browsing".to_string(),
            state: "Idle".to_string(),
            // No assets on the idle card: no branded mark as artwork, no icon.
            ..Activity::default()
        };
    }
    let (details, state) = spec.render_kind(raw_title, a, r, is_movie);
    let name = compose_name_inner(raw_title, r);
    let paused = a.is_some_and(|x| x.paused);
    let mut act = Activity {
        type_: "PLAYING".to_string(),
        name,
        details,
        state,
        large_image: compose_large_image_inner(r),
        large_text: compose_large_text_inner(r, is_movie),
        paused,
        // No small slot: the card carries no status icon over the artwork.
        ..Activity::default()
    };
    if let Some(x) = a
        && x.has_start
    {
        act.has_start = true;
        act.start = x.start;
    }
    // PAUSED carries no timestamps at all: a start-only anchor renders as a
    // count-up that reads as playing. UNKNOWN carries none either: there is
    // no resolved progress to anchor, and no art or prose to show.
    if paused || r.unknown {
        act.has_start = false;
        act.start = 0;
        act.end = 0;
    }
    if r.unknown {
        act.large_image.clear();
        act.large_text.clear();
    }
    act
}

/// [`compose_card`] with the stored kind and the shipped default spec
/// (`presence.go:662`).
///
/// Movie (`r.is_movie`): header is the movie title; details is
/// `<year> · <genres>` (whole genres dropped over budget, never the title);
/// state is the runtime alone, plus the paused marker when paused. Series:
/// header is the series title (clean-title fallback); details blanks because
/// the header already carries the title; state is `SxxEyy · Episode`, else
/// the filename tag, else the episode title, plus the paused marker. The
/// tooltip (`large_text`) carries the work-level synopsis only, as a
/// <=128-unit, explicitly ellipsized snippet -- episode-pinned prose never
/// rides; the complete note stays in Potflix's own tooltip. `large_image` is
/// the resolved artwork URL or empty. Paused and unknown cards carry no
/// timestamps at all.
pub fn build_activity_resolved(raw_title: &str, a: Option<&Action>, r: &Resolved) -> Activity {
    compose_card(raw_title, a, r, r.is_movie, &cardspec::default_card_spec())
}

/// The synopsis-bearing state line: a movie joins genres, length and the
/// synopsis with " \u{b7} "; a series joins the `SxxExx` episode code (the
/// raw episode tag when no code is available) and the synopsis with a literal
/// `" - "`, both capped word-aware at [`STATE_SYNOPSIS_BUDGET`]. It backs
/// the explicit [`cardspec::CARD_STATE_SYNOPSIS`] preset only; the shipped
/// default keeps the synopsis in `large_text`.
/// With no synopsis the slot hides (`""`, `"Paused"` when paused), so the
/// preset honors the empty-hides contract (`cardspec_test::card_spec_empty_hide`).
pub(crate) fn synopsis_state(raw_title: &str, a: Option<&Action>, r: &Resolved) -> String {
    let paused = a.is_some_and(|x| x.paused);
    let s_full = rp_text(&r.synopsis, HUGE);
    if s_full.is_empty() {
        if paused {
            return with_state_time("", PAUSED_MARKER);
        }
        return String::new();
    }
    if r.is_movie {
        let g = rp_text(&r.genres, HUGE);
        let l = rp_text(&r.length, HUGE);
        let mut prefix_parts: Vec<&str> = Vec::new();
        if !g.is_empty() {
            prefix_parts.push(&g);
        }
        if !l.is_empty() {
            prefix_parts.push(&l);
        }
        let prefix = prefix_parts.join(" \u{b7} ");
        let join_budget = if paused {
            STATE_SYNOPSIS_BUDGET - PAUSED_MARKER.len()
        } else {
            STATE_SYNOPSIS_BUDGET
        };
        let sep = if prefix.is_empty() { "" } else { " \u{b7} " };
        let prefix_units = utf16_units(&prefix);
        let sep_units = utf16_units(sep);
        if prefix_units + sep_units >= join_budget {
            let full_joined = format!("{prefix}{sep}{s_full}");
            return if paused {
                with_state_time(&trim_word(&full_joined, join_budget), PAUSED_MARKER)
            } else {
                trim_word(&full_joined, join_budget)
            };
        }
        let trimmed = smart_trim_synopsis(&s_full, join_budget - prefix_units - sep_units);
        let joined = format!("{prefix}{sep}{trimmed}");
        if paused {
            with_state_time(&joined, PAUSED_MARKER)
        } else {
            joined
        }
    } else {
        let raw = raw_title.strip_suffix(SUFFIX).unwrap_or(raw_title);
        let mut tag_raw = String::new();
        if !(raw.trim().is_empty() || raw == APP_NAME) {
            tag_raw = clean(raw).episode_tag;
        }
        // The `SxxExx` code prefixes the synopsis where available; otherwise
        // the current raw-tag behavior stands.
        let code = episode_code(r.season, r.number, r.absolute, &tag_raw);
        let tag_t = if code.is_empty() {
            rp_text(&tag_raw, STATE_BUDGET)
        } else {
            code
        };
        if tag_t.is_empty() {
            if paused {
                return with_state_time(
                    &smart_trim_synopsis(&s_full, STATE_SYNOPSIS_BUDGET - PAUSED_MARKER.len()),
                    PAUSED_MARKER,
                );
            }
            return smart_trim_synopsis(&s_full, STATE_SYNOPSIS_BUDGET);
        }
        if paused {
            let join_budget = STATE_SYNOPSIS_BUDGET - PAUSED_MARKER.len();
            let sep = " - ";
            let prefix_units = utf16_units(&tag_t);
            let sep_units = utf16_units(sep);
            if prefix_units + sep_units >= join_budget {
                return with_state_time(
                    &trim_word(&format!("{tag_t}{sep}{s_full}"), join_budget),
                    PAUSED_MARKER,
                );
            }
            let trimmed = smart_trim_synopsis(&s_full, join_budget - prefix_units - sep_units);
            return with_state_time(&format!("{tag_t}{sep}{trimmed}"), PAUSED_MARKER);
        }
        let join_budget = STATE_SYNOPSIS_BUDGET;
        let prefix_units = utf16_units(&tag_t);
        let sep_units = utf16_units(" - ");
        if prefix_units + sep_units >= join_budget {
            trim_word(&format!("{tag_t} - {s_full}"), join_budget)
        } else {
            let trimmed = smart_trim_synopsis(&s_full, join_budget - prefix_units - sep_units);
            format!("{tag_t} - {trimmed}")
        }
    }
}

// Go callers write `presence.CardSpec`; the module path `cardspec::CardSpec` is
// the canonical one.
pub use cardspec::{CardPreset, CardSpec};
