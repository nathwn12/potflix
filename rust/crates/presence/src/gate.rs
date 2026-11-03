//! `presence::gate` -- mirrors `go\internal\presence\gate.go`.
//!
//! The send gate: what has to be true before a presence push is allowed out.
//!
//! The app is already event-driven: the tracker emits only when the card changes
//! or a real seek moves the position. The gate sits between the tracker and the
//! wire and decides whether the current card actually goes out. It exists because
//! the tracker's per-poll seek delta can be swallowed (held by the debounce, then
//! never re-fired), which left the card frozen on a stale anchor. The gate adds
//! the two self-healing paths: a divergence re-anchor (live position vs the last
//! anchor Discord was told) and a coarse heartbeat. It also records EVERY
//! decision, so a suppressed poll is visible instead of silent.
//!
//! This is NOT about Discord rate limiting alone: the tracker emits 0/min while
//! steady (change/seek events only), the 30 s heartbeat adds 2/min, and a scrub
//! is ~9/min - all far below Discord's update limit in the steady case. The
//! heartbeat is self-healing for residual divergence, nothing more.
//!
//! The Discord SDK limit still needs a guard for bursts: at most
//! [`RATE_MAX_SENDS`] payload sends per [`RATE_WINDOW_MS`] (5 per 20 s). The
//! gate enforces it as a sliding window over the monotonic interval clock. A
//! changed card sends immediately when the window permits; when the window is
//! full the newest card is coalesced (held) and flushed on the first poll
//! after the window opens. An identical payload is never sent from the hold:
//! a revert to the last-sent card drops the hold. The poll interval itself is
//! never shortened to flush faster.

use core::fmt;
use std::collections::VecDeque;

use crate::clock::IntervalClock;
use crate::{Activity, Button, abs64};

/// HeartbeatMS re-pushes an otherwise-unchanged running card so any residual
/// divergence self-corrects. It is not a rate-limit workaround (`gate.go:23`).
pub const HEARTBEAT_MS: i64 = 30_000;

/// DivergenceMS is how far the live position may differ from what the last-sent
/// anchor predicts before the card is re-anchored at once (`gate.go:26`).
pub const DIVERGENCE_MS: i64 = 2_000;

/// DivergenceThrottleMS bounds divergence re-anchors to at most one per this
/// interval, so a noisy position read cannot turn into a push burst
/// (`gate.go:29`).
pub const DIVERGENCE_THROTTLE_MS: i64 = 5_000;

/// RateWindowMS is the sliding-window span for the Discord SDK payload guard:
/// at most [`RATE_MAX_SENDS`] sends per window.
pub const RATE_WINDOW_MS: i64 = 20_000;

/// RateMaxSends is the documented Discord SDK payload limit per
/// [`RATE_WINDOW_MS`].
pub const RATE_MAX_SENDS: usize = 5;

/// ReasonUnchanged: an event arrived but the payload is identical to the last
/// sent card (a redundant emit) (`gate.go:36`).
pub const REASON_UNCHANGED: &str = "unchanged";

/// ReasonDebounce: a seek was held by the trailing-edge debounce and is waiting
/// for the window to expire (`gate.go:39`).
pub const REASON_DEBOUNCE: &str = "debounce";

/// ReasonHeartbeatNotDue: the card is steady, divergence is within tolerance, and
/// the heartbeat interval has not elapsed (`gate.go:42`).
pub const REASON_HEARTBEAT_NOT_DUE: &str = "heartbeat-not-due";

/// ReasonPaused: the paused card with this content was already sent and is
/// parked; no further card is (re)sent while paused and the content is
/// unchanged, so even the heartbeat stays quiet (`gate.go:46`).
pub const REASON_PAUSED: &str = "paused";

/// ReasonRateLimited: a would-send card was held by the sliding-window payload
/// guard ([`RATE_MAX_SENDS`] per [`RATE_WINDOW_MS`]). The newest held card
/// flushes on the first poll after the window opens.
pub const REASON_RATE_LIMITED: &str = "rate-limited";

/// One gate decision (`gate.go:50-57`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the card goes out.
    pub send: bool,
    /// Retained for the consumer's SET_ACTIVITY-null path but no longer set: the
    /// pause transition now sends a paused card and keeps it (outcome: pause
    /// persists), so no decision clears the activity.
    pub clear: bool,
    /// Empty when `send`; else one of the `REASON_*` constants.
    pub reason: String,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.send {
            if self.clear {
                f.write_str("sent+clear")
            } else {
                f.write_str("sent")
            }
        } else {
            write!(f, "suppressed:{}", self.reason)
        }
    }
}

/// Whether the card is the parked pause card (`gate.go:98-104`): the builder's
/// `paused` flag, or the legacy `small_image` "pause" marker on cards built
/// before the pristine-card change (kept so previously built cards keep their
/// meaning).
fn is_paused_card(card: &Activity) -> bool {
    card.paused || card.small_image == "pause"
}

/// Whether two card button lists carry the same ordered label+URL pairs
/// (`gate.go:108`). Order matters: the wire serializes buttons in order.
fn buttons_equal(a: &[Button], b: &[Button]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.label == y.label && x.url == y.url)
}

/// Remembers what was last sent so it can decide whether the current card needs
/// to go out (`gate.go:122-130`). The zero value is ready to use.
///
/// The rate guard rides alongside: `send_times` holds the monotonic-clock
/// stamps of the sends inside the current [`RATE_WINDOW_MS`] window, and
/// `pending` holds the newest held card while the window is full. A held card
/// never sends an identical payload: the flush sends the CURRENT card (fresh
/// anchor) and a revert to the last-sent card drops the hold.
#[derive(Debug, Clone, Default)]
pub struct Gate {
    has_sent: bool,
    last: Activity,
    last_start: i64,
    last_sent_at: i64,
    last_divergence_at: i64,
    force: bool,
    clock: IntervalClock,
    send_times: VecDeque<i64>,
    pending: Option<Activity>,
}

impl Gate {
    /// The zero value, spelled out.
    pub fn new() -> Gate {
        Gate::default()
    }

    /// Makes the next [`Gate::decide`] send regardless of the card
    /// (`gate.go:134`). Positive control and test hook: it lets a known event be
    /// recorded through the real log.
    pub fn force_next(&mut self) {
        self.force = true;
    }

    /// Consulted on every poll (`gate.go:143`). `card` is the card built from the
    /// current tracker state; `pos`/`total` are the live PotPlayer times; `now`
    /// is epoch ms; `event` reports whether the tracker emitted a change/seek
    /// this poll; `debounced` reports whether the tracker is holding a seek for
    /// the trailing edge. It returns the card with its timestamps anchored
    /// (start plus the projected end) while running and with NO timestamps at
    /// all while paused, and the verdict. A pause sends a paused card once and
    /// then stays quiet.
    ///
    /// Rate guard: at most [`RATE_MAX_SENDS`] sends per [`RATE_WINDOW_MS`]
    /// (sliding window over the monotonic clock). A would-send card goes out
    /// immediately when the window permits; when the window is full the newest
    /// card is held ([`REASON_RATE_LIMITED`]) and the CURRENT card flushes on
    /// the first poll after the window opens -- so a burst coalesces to the
    /// latest and a single correction never waits for the heartbeat. The flush
    /// never sends an identical payload: a revert to the last-sent card drops
    /// the hold. The poll interval is never shortened to flush faster.
    pub fn decide(
        &mut self,
        card: Activity,
        pos: i64,
        total: i64,
        now: i64,
        event: bool,
        debounced: bool,
    ) -> (Activity, Verdict) {
        // Interval bookkeeping runs on a monotonic clock; the anchors below keep
        // the epoch now.
        let clk = self.clock.tick(now);
        self.prune_rate(clk);
        let mut out = card;
        // Re-anchor only on a ready position read. A not-ready read (pos<=0, see
        // presence.go) must keep the card's existing anchor: recomputing here
        // would reset the elapsed clock to 0:00 and emit a full-duration
        // countdown.
        if out.has_start && pos > 0 {
            out.start = now - pos;
            out.end = 0;
            // end = now + (total - pos); serialized as timestamps.end while
            // playing (README deviation: both start and end go on the wire), so
            // Discord renders the time bar against the real media position.
            if total > pos {
                out.end = now + (total - pos);
            }
        }

        // Pause: send a paused card and KEEP it. The activity type is Watching
        // and the card carries NO timestamps at all: a start-only anchor
        // renders as a count-up that reads as playing, so the composer strips
        // the anchor and the gate must not re-add it. There is NO
        // timestamps.end while paused either. Decided here, before the suppression
        // paths, so the pause card is never debounced or throttled. The pause is
        // detected via is_paused_card (the builder's Paused flag, with the legacy
        // small_image marker honoured); no pause icon rides the wire.
        if is_paused_card(&out) {
            if self.has_sent
                && is_paused_card(&self.last)
                && self.last.paused == out.paused
                && self.last.name == out.name
                && self.last.details == out.details
                && self.last.state == out.state
                && self.last.large_image == out.large_image
                && buttons_equal(&self.last.buttons, &out.buttons)
            {
                // Already showing the parked paused card for this content: stay
                // quiet, so the pause timer is not restarted and Discord is not
                // spammed. Not even the heartbeat re-pushes while paused. A
                // content change (a different title/state, e.g. the next
                // episode) falls through and re-sends, so the parked card cannot
                // go stale - while the unchanged card keeps the no-spam
                // property.
                return (
                    out,
                    Verdict {
                        reason: REASON_PAUSED.to_string(),
                        ..Verdict::default()
                    },
                );
            }
            out.has_start = false;
            out.start = 0;
            out.end = 0;
            if self.send_times.len() >= RATE_MAX_SENDS {
                // The pause transition is owed but the window is full: hold the
                // newest paused card; it flushes on the first poll after the
                // window opens.
                self.pending = Some(out.clone());
                return (
                    out,
                    Verdict {
                        reason: REASON_RATE_LIMITED.to_string(),
                        ..Verdict::default()
                    },
                );
            }
            self.pending = None;
            return (self.commit(out, clk), Verdict::sent());
        }

        if self.force {
            self.force = false;
            self.pending = None;
            return (self.commit(out, clk), Verdict::sent());
        }

        // Flush: a held card exists and the window just opened. Send the CURRENT
        // card (fresh anchor, latest lines) when it still differs from what was
        // sent; a revert to the last-sent card drops the stale hold instead of
        // sending an identical payload. The anchor counts without requiring the
        // tracker's one-shot event: the held seek's event is gone, but its
        // anchor update is still owed.
        if self.pending.is_some() && self.send_times.len() < RATE_MAX_SENDS {
            let content_changed = self.content_changed(&out);
            let anchor_moved = out.has_start && out.start != self.last_start;
            let divergence_due = out.has_start
                && pos > 0
                && abs64(pos - (now - self.last_start)) > DIVERGENCE_MS
                && (clk - self.last_divergence_at) >= DIVERGENCE_THROTTLE_MS;
            let heartbeat_due = self.has_sent && (clk - self.last_sent_at) >= HEARTBEAT_MS;
            if self.has_sent && (content_changed || anchor_moved || divergence_due || heartbeat_due)
            {
                if divergence_due {
                    self.last_divergence_at = clk;
                }
                self.pending = None;
                return (self.commit(out, clk), Verdict::sent());
            }
            if self.has_sent {
                // Reverted to the last-sent card: the hold is stale.
                self.pending = None;
            }
            // First-send hold cannot happen (an empty window always permits the
            // first send), so a pending without `has_sent` just falls through.
        }

        if !self.has_sent {
            return (self.commit(out, clk), Verdict::sent());
        }

        let content_changed = self.content_changed(&out);
        let anchor_moved = out.has_start && out.start != self.last_start;

        if content_changed {
            return self.send_or_hold(out, clk);
        }
        if event && anchor_moved {
            return self.send_or_hold(out, clk);
        }
        // Divergence: the live position no longer matches what the last-sent
        // anchor predicts (a seek the tracker swallowed, a resume, or clock
        // drift). Re-anchor at once, throttled.
        if out.has_start
            && pos > 0
            && abs64(pos - (now - self.last_start)) > DIVERGENCE_MS
            && (clk - self.last_divergence_at) >= DIVERGENCE_THROTTLE_MS
        {
            if self.send_times.len() >= RATE_MAX_SENDS {
                self.pending = Some(out.clone());
                return (
                    out,
                    Verdict {
                        reason: REASON_RATE_LIMITED.to_string(),
                        ..Verdict::default()
                    },
                );
            }
            self.last_divergence_at = clk;
            self.pending = None;
            return (self.commit(out, clk), Verdict::sent());
        }
        if clk - self.last_sent_at >= HEARTBEAT_MS {
            return self.send_or_hold(out, clk);
        }

        let reason = if debounced {
            REASON_DEBOUNCE
        } else if event {
            REASON_UNCHANGED
        } else {
            REASON_HEARTBEAT_NOT_DUE
        };
        (
            out,
            Verdict {
                reason: reason.to_string(),
                ..Verdict::default()
            },
        )
    }

    /// Whether the card body differs from the last-sent card (anchor excluded).
    fn content_changed(&self, out: &Activity) -> bool {
        out.name != self.last.name
            || out.details != self.last.details
            || out.state != self.last.state
            || out.paused != self.last.paused
            || out.small_image != self.last.small_image
            || out.large_image != self.last.large_image
            || !buttons_equal(&self.last.buttons, &out.buttons)
            || out.has_start != self.last.has_start
    }

    /// Drops window stamps older than [`RATE_WINDOW_MS`].
    fn prune_rate(&mut self, clk: i64) {
        while self
            .send_times
            .front()
            .is_some_and(|t| clk - *t >= RATE_WINDOW_MS)
        {
            self.send_times.pop_front();
        }
    }

    /// Sends immediately when the sliding window permits, else holds the newest
    /// card and reports [`REASON_RATE_LIMITED`]. The hold coalesces: each held
    /// card overwrites the previous one, so a burst flushes as the latest.
    fn send_or_hold(&mut self, out: Activity, clk: i64) -> (Activity, Verdict) {
        if self.send_times.len() >= RATE_MAX_SENDS {
            self.pending = Some(out.clone());
            return (
                out,
                Verdict {
                    reason: REASON_RATE_LIMITED.to_string(),
                    ..Verdict::default()
                },
            );
        }
        self.pending = None;
        (self.commit(out, clk), Verdict::sent())
    }

    /// Records a sent card as the new reference point (`gate.go:239`). `clk` is
    /// the monotonic interval clock; `out.start` is the epoch anchor. Also
    /// stamps the sliding-window send time.
    fn commit(&mut self, out: Activity, clk: i64) -> Activity {
        self.has_sent = true;
        self.last = out.clone();
        self.last_start = out.start;
        self.last_sent_at = clk;
        self.prune_rate(clk);
        self.send_times.push_back(clk);
        out
    }
}

impl Verdict {
    /// A plain send verdict (no clear, no reason).
    fn sent() -> Verdict {
        Verdict {
            send: true,
            ..Verdict::default()
        }
    }
}
