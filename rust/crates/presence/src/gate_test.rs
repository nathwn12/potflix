//! `gate_test` -- mirrors `go\internal\presence\gate_test.go`.
//!
//! The send gate: unchanged steady state is suppressed with a reason; a changed
//! card, a divergence re-anchor, a seek event, and the heartbeat all send; pause
//! carries no timestamps; resume re-anchors.

use crate::gate::{
    DIVERGENCE_MS, DIVERGENCE_THROTTLE_MS, Gate, HEARTBEAT_MS, RATE_MAX_SENDS, RATE_WINDOW_MS,
    REASON_DEBOUNCE, REASON_HEARTBEAT_NOT_DUE, REASON_PAUSED, REASON_RATE_LIMITED,
    REASON_UNCHANGED,
};
use crate::{Activity, Button};

fn running_card(details: &str, state: &str) -> Activity {
    Activity {
        type_: "PLAYING".to_string(),
        details: details.to_string(),
        state: state.to_string(),
        small_image: "play".to_string(),
        has_start: true,
        ..Activity::default()
    }
}

#[test]
fn gate_first_send_then_steady_suppressed() {
    let mut g = Gate::default();
    let (out, v) = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    assert!(v.send, "first card should send, got {v}");
    assert!(
        out.start == 99000 && out.end == 159000,
        "anchors start={} end={}, want 99000/159000",
        out.start,
        out.end
    );
    // Steady playback: pos and now advance together, so the anchor holds.
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        2000,
        60000,
        101000,
        false,
        false,
    );
    assert!(
        !v.send,
        "steady unchanged card should be suppressed, got {v}"
    );
    assert_eq!(
        v.reason, REASON_HEARTBEAT_NOT_DUE,
        "steady suppression reason = {:?}, want {:?}",
        v.reason, REASON_HEARTBEAT_NOT_DUE
    );
}

#[test]
fn gate_changed_details_sent() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    let (_, v) = g.decide(
        running_card("Other", "S1 E1"),
        2000,
        60000,
        101000,
        false,
        false,
    );
    assert!(v.send, "changed details should send, got {v}");
}

#[test]
fn gate_changed_state_sent() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    let (_, v) = g.decide(
        running_card("Show", "S1 E1 - 00:01"),
        2000,
        60000,
        101000,
        false,
        false,
    );
    assert!(v.send, "changed state should send, got {v}");
}

#[test]
fn gate_changed_large_image_sent() {
    let mut g = Gate::default();
    let mut base = running_card("Show", "S1 E1");
    base.large_image = "logo".to_string();
    let _ = g.decide(base, 1000, 60000, 100000, false, false);
    // Late metadata resolves only the cover: name/details/state identical.
    let mut next = running_card("Show", "S1 E1");
    next.large_image = "https://example.example/cover.jpg".to_string();
    let (_, v) = g.decide(next, 2000, 60000, 101000, false, false);
    assert!(v.send, "changed large_image should send, got {v}");
}

#[test]
fn gate_changed_buttons_sent() {
    let mut g = Gate::default();
    let mut base = running_card("Show", "S1 E1");
    base.large_image = "logo".to_string();
    let _ = g.decide(base, 1000, 60000, 100000, false, false);
    // Late metadata resolves only the buttons: name/details/state identical.
    let mut next = running_card("Show", "S1 E1");
    next.large_image = "logo".to_string();
    next.buttons = vec![Button {
        label: "Info".to_string(),
        url: "https://example.example/info".to_string(),
    }];
    let (_, v) = g.decide(next, 2000, 60000, 101000, false, false);
    assert!(v.send, "changed buttons should send, got {v}");
}

#[test]
fn gate_identical_art_and_buttons_suppressed() {
    let mut g = Gate::default();
    let card = || {
        let mut c = running_card("Show", "S1 E1");
        c.large_image = "https://example.example/cover.jpg".to_string();
        c.buttons = vec![Button {
            label: "Info".to_string(),
            url: "https://example.example/info".to_string(),
        }];
        c
    };
    let _ = g.decide(card(), 1000, 60000, 100000, false, false);
    // Truly identical repeat (same art + same ordered buttons) stays quiet.
    let (_, v) = g.decide(card(), 2000, 60000, 101000, false, false);
    assert!(
        !v.send,
        "identical art+buttons repeat should be suppressed, got {v}"
    );
    assert_eq!(
        v.reason, REASON_HEARTBEAT_NOT_DUE,
        "identical suppression reason = {:?}, want {:?}",
        v.reason, REASON_HEARTBEAT_NOT_DUE
    );
}

#[test]
fn gate_heartbeat_due_sent() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    // pos and now advance 30 s together: the anchor holds, the heartbeat fires.
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        31000,
        60000,
        130000,
        false,
        false,
    );
    assert!(v.send, "heartbeat due should send, got {v}");
}

#[test]
fn gate_divergence_sent() {
    // Forward: the position jumps +5 s while wall time advances 1 s.
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        6000,
        60000,
        101000,
        false,
        false,
    );
    assert!(v.send, "forward divergence should send, got {v}");

    // Backward: the position jumps -5 s.
    let mut g2 = Gate::default();
    let _ = g2.decide(
        running_card("Show", "S1 E1"),
        6000,
        60000,
        100000,
        false,
        false,
    );
    let (_, v) = g2.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        101000,
        false,
        false,
    );
    assert!(v.send, "backward divergence should send, got {v}");
}

#[test]
fn gate_divergence_throttled() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        6000,
        60000,
        101000,
        false,
        false,
    );
    assert!(v.send, "first divergence should send, got {v}");
    // A second divergence 1 s later is inside the throttle window.
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        12000,
        60000,
        102000,
        false,
        false,
    );
    assert!(
        !v.send,
        "divergence inside the throttle should be suppressed, got {v}"
    );
}

#[test]
fn gate_event_seek_reanchors() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    let (out, v) = g.decide(
        running_card("Show", "S1 E1"),
        30000,
        60000,
        101000,
        true,
        false,
    );
    assert!(v.send, "seek event should send, got {v}");
    assert_eq!(
        out.start, 71000,
        "seek anchor start={}, want 71000",
        out.start
    );
}

#[test]
fn gate_unchanged_event_suppressed() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    // An event with the same content and anchor is redundant.
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        2000,
        60000,
        101000,
        true,
        false,
    );
    assert!(
        !v.send && v.reason == REASON_UNCHANGED,
        "redundant event = {v}, want suppressed:{REASON_UNCHANGED}"
    );
}

#[test]
fn gate_debounce_reason() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        2000,
        60000,
        101000,
        false,
        true,
    );
    assert!(
        !v.send && v.reason == REASON_DEBOUNCE,
        "held debounce = {v}, want suppressed:{REASON_DEBOUNCE}"
    );
}

#[test]
fn gate_pause_sends_parked_card() {
    // Pause sends a parked paused card and does NOT clear the activity. The card
    // keeps the cleaned details/state, small_image "pause", and NO timestamps
    // at all: a start-only anchor would render as a count-up that reads as
    // playing, so the composer strips it and the gate must not re-add it.
    let mut g = Gate::default();
    let (out, v) = g.decide(
        Activity {
            type_: "PLAYING".to_string(),
            details: "Show".to_string(),
            state: "S1 E1 - Paused".to_string(),
            small_image: "pause".to_string(),
            ..Activity::default()
        },
        1000,
        60000,
        100000,
        true,
        false,
    );
    assert!(v.send, "pause must send a parked card, got {v}");
    assert!(!v.clear, "pause must NOT clear the activity");
    assert!(
        out.small_image == "pause" && !out.has_start && out.start == 0 && out.end == 0,
        "paused card = {out:?}, want pause with no timestamps"
    );
}

#[test]
fn gate_pause_not_debounced() {
    // Regression: a pause arriving while the tracker is holding a seek (the
    // debounced flag set) must still send the parked card. The pause rides on the
    // transition, not on the debounce-suppressed path.
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    let (_, v) = g.decide(
        Activity {
            type_: "PLAYING".to_string(),
            details: "Show".to_string(),
            state: "S1 E1 - Paused".to_string(),
            small_image: "pause".to_string(),
            ..Activity::default()
        },
        1000,
        60000,
        100250,
        true,
        true,
    );
    assert!(
        v.send && !v.clear,
        "pause was suppressed/debounced or cleared: {v} (clear={})",
        v.clear
    );
}

#[test]
fn gate_paused_steady_stays_quiet() {
    // Once the parked card is sent, no further card goes out while paused - not
    // even the heartbeat, which would restart the pause timer. The activity is
    // never cleared.
    let mut g = Gate::default();
    let (_, v) = g.decide(
        Activity {
            type_: "PLAYING".to_string(),
            details: "Show".to_string(),
            state: "S1 E1 - Paused".to_string(),
            small_image: "pause".to_string(),
            ..Activity::default()
        },
        1000,
        60000,
        100000,
        true,
        false,
    );
    assert!(
        v.send && !v.clear,
        "first pause should send a parked card (no clear), got {v} (clear={})",
        v.clear
    );
    for now in [100250, 120000, 140000, 200000] {
        let (_, v) = g.decide(
            Activity {
                type_: "PLAYING".to_string(),
                details: "Show".to_string(),
                state: "S1 E1 - Paused".to_string(),
                small_image: "pause".to_string(),
                ..Activity::default()
            },
            1000,
            60000,
            now,
            false,
            false,
        );
        assert!(
            !v.send,
            "paused poll at now={now} sent (clear={}, reason={:?}); want quiet",
            v.clear, v.reason
        );
        assert_eq!(
            v.reason, REASON_PAUSED,
            "paused suppression reason = {:?}, want {:?}",
            v.reason, REASON_PAUSED
        );
    }
}

#[test]
fn gate_paused_content_change_resends() {
    // The parked paused card is suppressed only while its content is unchanged.
    // A content change while paused (e.g. a different title/state) must re-send,
    // or the parked card stays stale forever. This pins BOTH halves: the
    // unchanged card sends exactly once and stays quiet, the changed card sends.
    let mut g = Gate::default();
    let paused = |details: &str, state: &str| Activity {
        type_: "PLAYING".to_string(),
        details: details.to_string(),
        state: state.to_string(),
        small_image: "pause".to_string(),
        ..Activity::default()
    };
    let (_, v) = g.decide(
        paused("ShowA", "S1 E1 - Paused"),
        1000,
        60000,
        100000,
        true,
        false,
    );
    assert!(v.send, "first paused card should send, got {v}");
    // Unchanged: still quiet (the no-spam property).
    let (_, v) = g.decide(
        paused("ShowA", "S1 E1 - Paused"),
        1000,
        60000,
        100250,
        false,
        false,
    );
    assert!(
        !v.send && v.reason == REASON_PAUSED,
        "unchanged paused card = {v}, want suppressed:{REASON_PAUSED}"
    );
    // Changed content: must re-send, with no timestamps (paused cards never
    // carry an anchor).
    let (out, v) = g.decide(
        paused("ShowB", "S1 E2 - Paused"),
        1000,
        60000,
        100500,
        false,
        false,
    );
    assert!(v.send, "changed paused card should re-send, got {v}");
    assert!(
        out.details == "ShowB"
            && out.state == "S1 E2 - Paused"
            && !out.has_start
            && out.start == 0
            && out.end == 0,
        "changed paused card = {out:?}, want ShowB/S1 E2 - Paused with no timestamps"
    );
    // The re-sent card becomes the new parked reference: unchanged again stays
    // quiet.
    let (_, v) = g.decide(
        paused("ShowB", "S1 E2 - Paused"),
        1000,
        60000,
        100750,
        false,
        false,
    );
    assert!(
        !v.send && v.reason == REASON_PAUSED,
        "re-parked unchanged card = {v}, want suppressed:{REASON_PAUSED}"
    );
}

#[test]
fn gate_resume_reanchors() {
    let mut g = Gate::default();
    let _ = g.decide(
        Activity {
            type_: "PLAYING".to_string(),
            details: "Show".to_string(),
            state: "S1 E1 - Paused".to_string(),
            small_image: "pause".to_string(),
            ..Activity::default()
        },
        1000,
        60000,
        100000,
        true,
        false,
    );
    let (out, v) = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        101000,
        true,
        false,
    );
    assert!(
        v.send && !v.clear,
        "resume should send a card (not a clear), got {v} (clear={})",
        v.clear
    );
    assert!(
        out.has_start && out.start == 100000 && out.end == 160000,
        "resume anchors start={} end={}, want 100000/160000",
        out.start,
        out.end
    );
}

#[test]
fn gate_end_omitted_when_total_unknown() {
    let mut g = Gate::default();
    let (out, v) = g.decide(running_card("Show", "S1 E1"), 1000, 0, 100000, false, false);
    assert!(v.send, "first card should send, got {v}");
    assert_eq!(
        out.end, 0,
        "end={}, want omitted (0) when total is unknown",
        out.end
    );
}

#[test]
fn gate_force_next() {
    let mut g = Gate::default();
    let _ = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    g.force_next();
    let (_, v) = g.decide(
        running_card("Show", "S1 E1"),
        2000,
        60000,
        101000,
        false,
        false,
    );
    assert!(v.send, "forced decide should send, got {v}");
}

/// F1: a running card can be handed back by the tracker while the player read is
/// not ready (ct<=0, presence.go). The heartbeat is due, but the not-ready read
/// must keep the card's existing anchor instead of resetting the elapsed clock
/// to 0:00 and emitting a full-duration countdown.
#[test]
fn gate_not_ready_heartbeat_keeps_anchor() {
    let mut g = Gate::default();
    let (out, v) = g.decide(
        running_card("Show", "S1 E1"),
        1000,
        60000,
        100000,
        false,
        false,
    );
    assert!(
        v.send && out.start == 99000,
        "first send start={} verdict={v}, want 99000/sent",
        out.start
    );
    // The card carries the last anchor; pos=0 marks the not-ready read.
    let mut card = running_card("Show", "S1 E1");
    card.start = 99000;
    let (out2, v2) = g.decide(card, 0, 60000, 130000, false, false);
    assert!(v2.send, "heartbeat due should send, got {v2}");
    assert!(
        out2.start == 99000 && out2.end == 0,
        "not-ready heartbeat reset the anchor: start={} end={}, want start=99000 end=0",
        out2.start,
        out2.end
    );
}

/// F2: intervals run on a monotonic clock. A backward wall-clock step must not
/// make the heartbeat delta negative and stall self-healing for the step.
#[test]
fn gate_backward_clock_step_does_not_stall_heartbeat() {
    // An idle card carries no start, so the divergence path cannot fire: the
    // heartbeat is the only self-healing route - the case the reviewer saw stall
    // with a -55 s step. (A paused card cannot be used here: the parked pause
    // stays quiet after its one send, by design.)
    let idle = Activity {
        type_: "PLAYING".to_string(),
        details: "Browsing".to_string(),
        state: "Idle".to_string(),
        small_image: "stop".to_string(),
        ..Activity::default()
    };
    let mut g = Gate::default();
    let (_, v) = g.decide(idle.clone(), 0, 0, 100000, false, false);
    assert!(v.send, "first idle card should send, got {v}");
    let (_, v) = g.decide(idle.clone(), 0, 0, 120000, false, false);
    assert!(!v.send, "20 s in, heartbeat should not be due, got {v}");
    // The wall clock steps back 55 s; only 20 s of interval has elapsed.
    let (_, v) = g.decide(idle.clone(), 0, 0, 65000, false, false);
    assert!(!v.send, "55 s backward step fired the heartbeat early: {v}");
    // 10 s of real interval later the monotonic clock reaches 30 s: the heartbeat
    // self-heals instead of stalling for the 55 s step.
    let (_, v) = g.decide(idle, 0, 0, 75000, false, false);
    assert!(v.send, "heartbeat stalled after a backward step, got {v}");
}

/// Proves the pristine-card pause path: a builder-produced paused card
/// (paused=true, no small_image icon) sends once with NO timestamps and then
/// stays parked and quiet.
#[test]
fn gate_pause_flag_parks_icon_free_card() {
    let mut g = Gate::default();
    let paused = Activity {
        type_: "PLAYING".to_string(),
        name: "Show".to_string(),
        details: "Pilot".to_string(),
        state: "S1 E2 - Paused".to_string(),
        large_image: "logo".to_string(),
        large_text: "PotFlix".to_string(),
        paused: true,
        ..Activity::default()
    };
    let (out, v) = g.decide(paused.clone(), 1000, 60000, 100000, true, false);
    assert!(
        v.send && !v.clear,
        "pause-flag card should send a parked card, got {v} (clear={})",
        v.clear
    );
    assert!(
        !out.has_start && out.start == 0 && out.end == 0,
        "parked pause card = {out:?}, want no timestamps"
    );
    let (_, v) = g.decide(paused, 1000, 60000, 200000, false, false);
    assert!(
        !v.send && v.reason == REASON_PAUSED,
        "parked pause repeat = {v}, want suppressed:{REASON_PAUSED}"
    );
}

/// The constants the tests reason about are the gate's own (`gate.go:20-30`).
#[test]
fn gate_timing_constants() {
    assert_eq!(HEARTBEAT_MS, 30_000);
    assert_eq!(DIVERGENCE_MS, 2_000);
    assert_eq!(DIVERGENCE_THROTTLE_MS, 5_000);
    assert_eq!(RATE_WINDOW_MS, 20_000);
    assert_eq!(RATE_MAX_SENDS, 5);
    assert_eq!(REASON_RATE_LIMITED, "rate-limited");
}

/// T2-B: a single correction sends immediately when the window permits -- it
/// never waits for the 30 s heartbeat.
#[test]
fn gate_rate_single_correction_sends_immediately() {
    let mut g = Gate::default();
    let (_, v) = g.decide(running_card("Show", "S1 E1"), 1000, 60000, 100000, false, false);
    assert!(v.send, "first card should send, got {v}");
    // A changed card 250 ms later (one poll) sends at once under the limit.
    let (_, v) = g.decide(
        running_card("Other", "S1 E1"),
        1250,
        60000,
        100250,
        false,
        false,
    );
    assert!(v.send, "single correction should send immediately, got {v}");
}

/// T2-B: 6 content changes inside 20 s send 5 and hold the latest; the hold
/// flushes on the first poll after the window with the newest lines, and the
/// intermediate card never goes out.
#[test]
fn gate_rate_burst_coalesces_to_latest() {
    let mut g = Gate::default();
    const NOW0: i64 = 1_000_000;
    // 6 distinct cards, one poll (250 ms) apart: all inside one 20 s window.
    let mut sends = 0;
    let mut last_verdict = String::new();
    for i in 0..6 {
        let card = running_card(&format!("Show{i}"), "S1 E1");
        // ct/now advance together so the anchor holds; only the content moves.
        let (_, v) = g.decide(card, 1000 + 250 * i, 60000, NOW0 + 250 * i, false, false);
        last_verdict = format!("{v}");
        if v.send {
            sends += 1;
        }
    }
    assert_eq!(sends, 5, "6 rapid changes must send 5, got {sends}");
    assert!(
        last_verdict.contains(REASON_RATE_LIMITED),
        "6th change must hold with {REASON_RATE_LIMITED}, got {last_verdict}"
    );
    // The held card is the newest (Show5), not an intermediate one: advancing
    // past the window and polling steady with Show5 flushes it.
    let (_, v) = g.decide(
        running_card("Show5", "S1 E1"),
        2250,
        60000,
        NOW0 + RATE_WINDOW_MS + 250,
        false,
        false,
    );
    assert!(v.send, "newest held card must flush after the window, got {v}");
    // And it parked: a repeat stays quiet (never an identical duplicate).
    let (_, v) = g.decide(
        running_card("Show5", "S1 E1"),
        2500,
        60000,
        NOW0 + RATE_WINDOW_MS + 500,
        false,
        false,
    );
    assert!(
        !v.send,
        "flushed card repeat must not duplicate, got {v}"
    );
}

/// T2-B: an identical payload is never sent from the hold -- a revert to the
/// last-sent card drops the hold instead of flushing it.
#[test]
fn gate_rate_revert_drops_hold_without_duplicate() {
    let mut g = Gate::default();
    const NOW0: i64 = 2_000_000;
    let (_, v) = g.decide(running_card("Show", "S1 E1"), 1000, 60000, NOW0, false, false);
    assert!(v.send);
    // Fill the window with 4 more distinct cards (5 sends total).
    for i in 1..5 {
        let (_, v) = g.decide(
            running_card(&format!("Show{i}"), "S1 E1"),
            1000 + 250 * i,
            60000,
            NOW0 + 250 * i,
            false,
            false,
        );
        assert!(v.send, "fill {i} should send, got {v}");
    }
    // 6th change held.
    let (_, v) = g.decide(
        running_card("Held", "S1 E1"),
        2250,
        60000,
        NOW0 + 1250,
        false,
        false,
    );
    assert_eq!(v.reason, REASON_RATE_LIMITED, "6th must hold, got {v}");
    // Revert to the last-sent card (Show4) after the window: nothing goes out.
    // ct keeps the anchor (now - ct == last start), so the payload is truly
    // identical -- steady playback advances now and ct together.
    let (_, v) = g.decide(
        running_card("Show4", "S1 E1"),
        22500,
        60000,
        NOW0 + RATE_WINDOW_MS + 1500,
        false,
        false,
    );
    assert!(
        !v.send && v.reason != REASON_RATE_LIMITED,
        "revert must drop the hold without sending, got {v}"
    );
}

/// T2-B: a paused card held by the window still parks -- it flushes once after
/// the window and then stays quiet.
#[test]
fn gate_rate_paused_flush_parks() {
    let mut g = Gate::default();
    const NOW0: i64 = 3_000_000;
    let (_, v) = g.decide(running_card("Show", "S1 E1"), 1000, 60000, NOW0, false, false);
    assert!(v.send);
    for i in 1..5 {
        let (_, v) = g.decide(
            running_card(&format!("Show{i}"), "S1 E1"),
            1000 + 250 * i,
            60000,
            NOW0 + 250 * i,
            false,
            false,
        );
        assert!(v.send);
    }
    // Pause transition while the window is full: held, not parked yet.
    let paused = Activity {
        type_: "PLAYING".to_string(),
        details: "Show4".to_string(),
        state: "S1 E1 - Paused".to_string(),
        small_image: "pause".to_string(),
        ..Activity::default()
    };
    let (_, v) = g.decide(paused.clone(), 2250, 60000, NOW0 + 1250, true, false);
    assert_eq!(v.reason, REASON_RATE_LIMITED, "paused hold, got {v}");
    // First poll after the window flushes the parked pause card.
    let (out, v) = g.decide(paused.clone(), 2250, 60000, NOW0 + RATE_WINDOW_MS + 1500, false, false);
    assert!(v.send, "paused flush must send, got {v}");
    assert!(!out.has_start && out.start == 0 && out.end == 0, "parked pause carries no timestamps");
    // Then it stays parked.
    let (_, v) = g.decide(paused, 2250, 60000, NOW0 + RATE_WINDOW_MS + 1750, false, false);
    assert_eq!(v.reason, REASON_PAUSED, "parked repeat, got {v}");
}
