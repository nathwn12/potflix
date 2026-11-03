//! `presence_test` -- mirrors `go\internal\presence\presence_test.go`.
//!
//! Table-driven tests over the transition matrix, plus the debounce deviation and
//! the payload helpers. Expectations are derived from the port contract
//! (section 4) and the Java PresenceTracker ported in `presence.go`.

use player::control::PlayStatus;
use titleclean::trunc;

use crate::cardspec::{CARD_DETAILS_DEFAULT, CARD_STATE_DEFAULT, CARD_STATE_SYNOPSIS, CardSpec};
use crate::{
    Action, Activity, Button, PAUSED_MARKER, Resolved, STATE_SYNOPSIS_BUDGET, Tracker, action_tag,
    build_activity, build_activity_resolved, compose_card, fmt_ms, rp_text, smart_trim_synopsis,
    trim_word, with_state_time,
};

/// One poll of the transition matrix.
struct Sample {
    title: &'static str,
    status: PlayStatus,
    ct: i64,
    now: i64,
}

/// One expectation: `tag` is `""` for no emit.
#[derive(Clone, Copy)]
struct Want {
    tag: &'static str,
    start: i64,
    has_start: bool,
}

fn none() -> Want {
    Want {
        tag: "",
        start: 0,
        has_start: false,
    }
}

fn run_w(start: i64) -> Want {
    Want {
        tag: "RUN",
        start,
        has_start: true,
    }
}

fn pause_w() -> Want {
    Want {
        tag: "PAUSE",
        start: 0,
        has_start: false,
    }
}

fn stop_w() -> Want {
    Want {
        tag: "STOP",
        start: 0,
        has_start: false,
    }
}

/// Reproduces the contract's `decision ...` log line (events.go's
/// `eventDecision` shape). The tracker itself does not log; this is the test's
/// rendering of each poll, so the matrix produces a readable decision log.
fn decision_line(s: &Sample, a: Option<&Action>) -> String {
    match a {
        None => format!(
            "decision ms={} st={} ct={} title=[{}] action=null",
            s.now, s.status, s.ct, s.title
        ),
        Some(a) => format!(
            "decision ms={} st={} ct={} title=[{}] action={} running={} paused={} hasStart={} start={} suf={} ct={}",
            s.now,
            s.status,
            s.ct,
            s.title,
            a.tag(),
            a.running,
            a.paused,
            a.has_start,
            a.start,
            a.suffix,
            a.ct
        ),
    }
}

/// Runs one sequence and reports every mismatch (Go's `t.Errorf` + `continue`),
/// with the decision log attached.
fn run_sequence(name: &str, samples: &[Sample], wants: &[Want]) {
    assert_eq!(
        samples.len(),
        wants.len(),
        "{name}: {} samples vs {} wants",
        samples.len(),
        wants.len()
    );
    let mut tr = Tracker::default();
    let mut log: Vec<String> = Vec::with_capacity(samples.len());
    let mut failures: Vec<String> = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        let a = tr.poll(s.title, s.status, s.ct, s.now);
        log.push(decision_line(s, a.as_ref()));

        let got_tag = action_tag(a.as_ref());
        let want_tag = if wants[i].tag.is_empty() {
            "null"
        } else {
            wants[i].tag
        };
        if got_tag != want_tag {
            failures.push(format!("{name}[{i}]: tag={got_tag} want={want_tag}"));
            continue;
        }
        let Some(a) = a.as_ref() else {
            continue;
        };
        if a.has_start != wants[i].has_start || a.start != wants[i].start {
            failures.push(format!(
                "{name}[{i}]: hasStart={} start={} want hasStart={} start={}",
                a.has_start, a.start, wants[i].has_start, wants[i].start
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{name} decision log:\n{}\n{}",
        log.join("\n"),
        failures.join("\n")
    );
}

#[test]
fn transition_matrix_running_advance_pause_stop() {
    const TA: &str = "A - PotPlayer";
    run_sequence(
        "A",
        &[
            Sample {
                title: TA,
                status: PlayStatus::RUNNING,
                ct: 1000,
                now: 100000,
            },
            Sample {
                title: TA,
                status: PlayStatus::RUNNING,
                ct: 1250,
                now: 100250,
            },
            Sample {
                title: TA,
                status: PlayStatus::PAUSED,
                ct: 1250,
                now: 100500,
            },
            Sample {
                title: TA,
                status: PlayStatus::PAUSED,
                ct: 1250,
                now: 100750,
            },
            Sample {
                title: TA,
                status: PlayStatus::RUNNING,
                ct: 1260,
                now: 101000,
            },
            Sample {
                title: TA,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 101250,
            },
            Sample {
                title: TA,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 101500,
            },
        ],
        &[
            // Index 2 is a Running->Paused report while ct still moves in the
            // window: flap suppression keeps it Running, so Pause lands on 3.
            run_w(99000),
            none(),
            none(),
            pause_w(),
            run_w(99740),
            stop_w(),
            none(),
        ],
    );
}

#[test]
fn transition_matrix_seek_and_debounce() {
    const TB: &str = "B - PotPlayer";
    run_sequence(
        "B",
        &[
            Sample {
                title: TB,
                status: PlayStatus::RUNNING,
                ct: 1000,
                now: 200000,
            },
            Sample {
                title: TB,
                status: PlayStatus::RUNNING,
                ct: 5000,
                now: 200250,
            },
            Sample {
                title: TB,
                status: PlayStatus::RUNNING,
                ct: 9000,
                now: 201000,
            },
            Sample {
                title: TB,
                status: PlayStatus::RUNNING,
                ct: 9200,
                now: 201250,
            },
        ],
        &[run_w(199000), none(), run_w(192000), none()],
    );
}

#[test]
fn transition_matrix_flap_suppression() {
    const TC: &str = "C - PotPlayer";
    run_sequence(
        "C",
        &[
            Sample {
                title: TC,
                status: PlayStatus::RUNNING,
                ct: 1000,
                now: 300000,
            },
            Sample {
                title: TC,
                status: PlayStatus::RUNNING,
                ct: 1600,
                now: 300250,
            },
            // glitch: still moving -> Running
            Sample {
                title: TC,
                status: PlayStatus::PAUSED,
                ct: 2200,
                now: 300500,
            },
            // still moving -> Running
            Sample {
                title: TC,
                status: PlayStatus::PAUSED,
                ct: 2200,
                now: 300750,
            },
            // window flushed -> real Pause
            Sample {
                title: TC,
                status: PlayStatus::PAUSED,
                ct: 2200,
                now: 301000,
            },
            Sample {
                title: TC,
                status: PlayStatus::PAUSED,
                ct: 2200,
                now: 301250,
            },
            // paused seek
            Sample {
                title: TC,
                status: PlayStatus::PAUSED,
                ct: 4000,
                now: 301500,
            },
            Sample {
                title: TC,
                status: PlayStatus::PAUSED,
                ct: 4000,
                now: 301750,
            },
        ],
        &[
            run_w(299000),
            none(),
            none(),
            none(),
            pause_w(),
            none(),
            pause_w(),
            none(),
        ],
    );
}

#[test]
fn transition_matrix_title_change_reset() {
    const TD: &str = "D - PotPlayer";
    const TE: &str = "E - PotPlayer";
    run_sequence(
        "D",
        &[
            Sample {
                title: TD,
                status: PlayStatus::RUNNING,
                ct: 1000,
                now: 400000,
            },
            Sample {
                title: TE,
                status: PlayStatus::RUNNING,
                ct: 500,
                now: 400250,
            },
            Sample {
                title: TE,
                status: PlayStatus::RUNNING,
                ct: 750,
                now: 400500,
            },
        ],
        &[run_w(399000), run_w(399750), none()],
    );
}

#[test]
fn transition_matrix_undefined_carries_nothing() {
    const TE: &str = "E - PotPlayer";
    run_sequence(
        "E",
        &[
            Sample {
                title: TE,
                status: PlayStatus::RUNNING,
                ct: 1000,
                now: 500000,
            },
            Sample {
                title: TE,
                status: PlayStatus::UNDEFINED,
                ct: 0,
                now: 500250,
            },
            Sample {
                title: TE,
                status: PlayStatus::UNDEFINED,
                ct: 0,
                now: 500500,
            },
            // unchanged: lastStatus still Running
            Sample {
                title: TE,
                status: PlayStatus::RUNNING,
                ct: 1250,
                now: 500750,
            },
            // ct<=0 -> no movement, real transition
            Sample {
                title: TE,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 501000,
            },
            Sample {
                title: TE,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 501250,
            },
        ],
        &[run_w(499000), none(), none(), none(), stop_w(), none()],
    );
}

#[test]
fn transition_matrix_undefined_first_then_running() {
    const TF: &str = "F - PotPlayer";
    run_sequence(
        "F",
        &[
            Sample {
                title: TF,
                status: PlayStatus::UNDEFINED,
                ct: 0,
                now: 600000,
            },
            Sample {
                title: TF,
                status: PlayStatus::RUNNING,
                ct: 1000,
                now: 600250,
            },
            Sample {
                title: TF,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 600500,
            },
            Sample {
                title: TF,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 600750,
            },
        ],
        &[none(), run_w(599250), stop_w(), none()],
    );
}

#[test]
fn transition_matrix_paused_ct_not_ok() {
    const TG: &str = "G - PotPlayer";
    run_sequence(
        "G",
        &[
            Sample {
                title: TG,
                status: PlayStatus::PAUSED,
                ct: 0,
                now: 700000,
            },
            Sample {
                title: TG,
                status: PlayStatus::PAUSED,
                ct: 0,
                now: 700250,
            },
            Sample {
                title: TG,
                status: PlayStatus::PAUSED,
                ct: 1000,
                now: 700500,
            },
        ],
        &[none(), none(), pause_w()],
    );
}

#[test]
fn transition_matrix_initial_stopped() {
    const TH: &str = "H - PotPlayer";
    run_sequence(
        "H",
        &[
            Sample {
                title: TH,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 800000,
            },
            Sample {
                title: TH,
                status: PlayStatus::STOPPED,
                ct: 0,
                now: 800250,
            },
        ],
        &[stop_w(), none()],
    );
}

#[test]
fn run_debounce() {
    // A seek inside the ~1 s window, same title, is HELD (trailing edge) and
    // lands when the window expires - it is no longer dropped forever, which is
    // what froze the card on a stale anchor.
    const X: &str = "X - PotPlayer";
    let mut tr = Tracker::default();
    let a = tr.poll(X, PlayStatus::RUNNING, 1000, 0);
    assert!(
        a.as_ref().is_some_and(|a| a.running),
        "first RUN should emit, got {a:?}"
    );
    let a = tr.poll(X, PlayStatus::RUNNING, 5000, 200);
    assert!(
        a.is_none(),
        "seek within 1s same title should be held, got {a:?}"
    );
    assert!(
        tr.pending_debounce(),
        "held seek should report pending_debounce"
    );
    let a = tr.poll(X, PlayStatus::RUNNING, 5000, 900);
    assert!(
        a.is_none(),
        "seek still inside the window should be held, got {a:?}"
    );
    // The window expires: the held seek lands with a fresh anchor.
    let a = tr.poll(X, PlayStatus::RUNNING, 5200, 1001);
    match a.as_ref() {
        Some(a) if a.running => assert_eq!(
            a.start,
            1001 - 5200,
            "trailing-edge anchor start={}, want {}",
            a.start,
            1001 - 5200
        ),
        other => panic!("held seek should land after the window, got {other:?}"),
    }
    assert!(
        !tr.pending_debounce(),
        "pending flag should clear after the held seek lands"
    );

    // A title change bypasses the debounce even inside the window.
    let a = tr.poll("Y - PotPlayer", PlayStatus::RUNNING, 500, 1050);
    assert!(
        a.as_ref().is_some_and(|a| a.running),
        "title change should bypass debounce, got {a:?}"
    );

    // After the window, a seek with the same title emits again.
    const Z: &str = "Z - PotPlayer";
    let mut tr2 = Tracker::default();
    let _ = tr2.poll(Z, PlayStatus::RUNNING, 1000, 0);
    let a = tr2.poll(Z, PlayStatus::RUNNING, 3000, 999);
    assert!(a.is_none(), "seek at +999ms should be held, got {a:?}");
    let a = tr2.poll(Z, PlayStatus::RUNNING, 5000, 1200);
    assert!(
        a.as_ref().is_some_and(|a| a.running),
        "seek after the window should emit, got {a:?}"
    );
}

/// F2: the trailing-edge debounce runs on the monotonic clock too, so a backward
/// wall-clock step cannot strand a held seek until the wall clock catches up.
#[test]
fn run_debounce_backward_clock_step() {
    const X: &str = "X - PotPlayer";
    let mut tr = Tracker::default();
    let a = tr.poll(X, PlayStatus::RUNNING, 1000, 100000);
    assert!(
        a.as_ref().is_some_and(|a| a.running),
        "first RUN should emit, got {a:?}"
    );
    // A seek 200 ms later is held inside the ~1 s window.
    let a = tr.poll(X, PlayStatus::RUNNING, 5000, 100200);
    assert!(
        a.is_none(),
        "seek inside the window should be held, got {a:?}"
    );
    // The wall clock steps back 55 s: the held seek is still inside the window.
    let a = tr.poll(X, PlayStatus::RUNNING, 5000, 45200);
    assert!(
        a.is_none(),
        "seek should still be held right after the step, got {a:?}"
    );
    // ~1 s of monotonic interval later it lands, rather than waiting 55 s.
    let a = tr.poll(X, PlayStatus::RUNNING, 5000, 46300);
    assert!(
        a.as_ref().is_some_and(|a| a.running),
        "held seek stranded after a backward step, got {a:?}"
    );
}

#[test]
fn paused_not_debounced() {
    const S: &str = "T - PotPlayer";
    let mut tr = Tracker::default();
    let a = tr.poll(S, PlayStatus::PAUSED, 1000, 0);
    assert!(
        a.as_ref().is_some_and(|a| a.paused),
        "initial pause should emit, got {a:?}"
    );
    let a = tr.poll(S, PlayStatus::PAUSED, 4000, 100);
    assert!(
        a.as_ref().is_some_and(|a| a.paused),
        "paused seek should emit (no debounce), got {a:?}"
    );
}

#[test]
fn fmt_ms_cases() {
    let cases: [(i64, &str); 10] = [
        (0, "00:00"),
        (999, "00:00"),
        (1000, "00:01"),
        (59000, "00:59"),
        (60000, "01:00"),
        (61000, "01:01"),
        (3599000, "59:59"),
        (3600000, "1:00:00"),
        (3661000, "1:01:01"),
        (7322000, "2:02:02"),
    ];
    for (ms, want) in cases {
        assert_eq!(fmt_ms(ms), want, "fmt_ms({ms})");
    }
}

#[test]
fn with_state_time_cases() {
    let cases: [(&str, &str, &str); 7] = [
        ("S01 E01", " - 01:23", "S01 E01 - 01:23"),
        ("", " - 01:23", "01:23"),
        ("", "", ""),
        ("ctx", "", "ctx"),
        ("", " - ", ""),
        ("", "  -  01:23", "01:23"),
        ("", "\t-\t01:00", "01:00"),
    ];
    for (context, suffix, want) in cases {
        assert_eq!(
            with_state_time(context, suffix),
            want,
            "with_state_time({context:?},{suffix:?})"
        );
    }

    let long = "x".repeat(200);
    let got = with_state_time(&long, " - 00:01");
    assert_eq!(
        got,
        format!("{}{}", trunc(&long, 128 - " - 00:01".len()), " - 00:01"),
        "long context should be truncated to leave room for the suffix: {got:?}"
    );
    assert!(
        got.ends_with(" - 00:01"),
        "the suffix must survive truncation: {got:?}"
    );
    let n = java_units(&got);
    assert!(n <= 128, "state length {n} exceeds the 128 limit: {got:?}");
}

/// `String.length()`: UTF-16 code units, the unit the Java-exact
/// `titleclean::trunc` limit is measured in.
fn java_units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// F3: the paused marker must never be truncated. A 120-unit context plus the
/// 9-unit " · Paused" marker is 129 units; the old concatenate-then-truncate
/// rendered the state as "... - Paus...". The budgeted context keeps the marker
/// whole and the total within 128.
#[test]
fn paused_marker_survives_context_cap() {
    let ctx = "x".repeat(120);
    let got = with_state_time(&ctx, PAUSED_MARKER);
    assert!(
        got.ends_with(PAUSED_MARKER),
        "paused marker was truncated: {got:?}"
    );
    let n = java_units(&got);
    assert!(n <= 128, "state length {n} exceeds the 128 limit: {got:?}");
}

#[test]
fn build_activity_cases() {
    // Netflix-style mapping: the header (name) is the media title, the first
    // body line (details) is the episode name when known, else the episode tag,
    // else the title, and the second body line (state) is the episode tag when it
    // adds information, else empty.
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    const TV_TAG_ONLY_RAW: &str = "Show.S01E02.1080p.mkv - PotPlayer";
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    struct Case {
        name: &'static str,
        raw: String,
        act: Option<Action>,
        want: Activity,
        start: i64,
    }
    let running = |start: i64| {
        Some(Action {
            running: true,
            has_start: true,
            start,
            ct: 1000,
            ..Action::default()
        })
    };
    let cases = vec![
        Case {
            name: "movie running",
            raw: MOVIE_RAW.to_string(),
            act: running(12345),
            want: Activity {
                type_: "PLAYING".to_string(),
                name: "Movie".to_string(),
                details: "Movie".to_string(),
                state: String::new(),
                has_start: true,
                ..Activity::default()
            },
            start: 12345,
        },
        Case {
            name: "tv with episode name and tag",
            raw: TV_RAW.to_string(),
            act: running(12345),
            want: Activity {
                type_: "PLAYING".to_string(),
                name: "Show".to_string(),
                details: "Pilot".to_string(),
                state: "S1 E2".to_string(),
                has_start: true,
                ..Activity::default()
            },
            start: 12345,
        },
        Case {
            name: "tv with tag only",
            raw: TV_TAG_ONLY_RAW.to_string(),
            act: running(12345),
            want: Activity {
                type_: "PLAYING".to_string(),
                name: "Show".to_string(),
                details: "S1 E2".to_string(),
                state: String::new(),
                has_start: true,
                ..Activity::default()
            },
            start: 12345,
        },
        Case {
            name: "tv paused",
            raw: TV_RAW.to_string(),
            act: Some(Action {
                paused: true,
                ct: 83000,
                ..Action::default()
            }),
            want: Activity {
                type_: "PLAYING".to_string(),
                name: "Show".to_string(),
                details: "Pilot".to_string(),
                state: "S1 E2 \u{b7} Paused".to_string(),
                paused: true,
                ..Activity::default()
            },
            start: 0,
        },
        Case {
            name: "movie paused has marker alone",
            raw: MOVIE_RAW.to_string(),
            act: Some(Action {
                paused: true,
                ct: 83000,
                ..Action::default()
            }),
            want: Activity {
                type_: "PLAYING".to_string(),
                name: "Movie".to_string(),
                details: "Movie".to_string(),
                state: "Paused".to_string(),
                paused: true,
                ..Activity::default()
            },
            start: 0,
        },
        Case {
            name: "tv stopped",
            raw: TV_RAW.to_string(),
            act: Some(Action {
                ct: 0,
                ..Action::default()
            }),
            want: Activity {
                type_: "PLAYING".to_string(),
                name: "Show".to_string(),
                details: "Pilot".to_string(),
                state: "S1 E2".to_string(),
                ..Activity::default()
            },
            start: 0,
        },
        Case {
            name: "idle",
            raw: "PotPlayer".to_string(),
            act: Some(Action::default()),
            want: Activity {
                type_: "PLAYING".to_string(),
                details: "Browsing".to_string(),
                state: "Idle".to_string(),
                ..Activity::default()
            },
            start: 0,
        },
        Case {
            name: "empty media is idle",
            raw: " - PotPlayer".to_string(),
            act: Some(Action::default()),
            want: Activity {
                type_: "PLAYING".to_string(),
                details: "Browsing".to_string(),
                state: "Idle".to_string(),
                ..Activity::default()
            },
            start: 0,
        },
        Case {
            // No episode name, and the over-128-unit title fills the details
            // line on its own: the state line must stay empty rather than
            // repeating the truncated details.
            name: "long title without episode context emits no state",
            raw: format!("{}.mkv - PotPlayer", "A".repeat(200)),
            act: running(12345),
            want: Activity {
                type_: "PLAYING".to_string(),
                name: format!("{}\u{2026}", "A".repeat(99)),
                details: format!("{}\u{2026}", "A".repeat(99)),
                state: String::new(),
                has_start: true,
                ..Activity::default()
            },
            start: 12345,
        },
    ];
    for c in &cases {
        let got = build_activity(&c.raw, c.act.as_ref());
        let mut want = c.want.clone();
        want.start = c.start;
        assert_eq!(got, want, "{}: card = {got:?}, want {want:?}", c.name);
    }
}

#[test]
fn action_tag_forms() {
    assert_eq!(action_tag(None), "null", "nil action should tag as null");
    assert_eq!(
        action_tag(Some(&Action {
            running: true,
            ..Action::default()
        })),
        "RUN",
        "running action should tag RUN"
    );
    assert_eq!(
        action_tag(Some(&Action {
            paused: true,
            ..Action::default()
        })),
        "PAUSE",
        "paused action should tag PAUSE"
    );
    assert_eq!(
        action_tag(Some(&Action::default())),
        "STOP",
        "default action should tag STOP"
    );
}

#[test]
fn build_activity_resolved_overrides() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };

    // Zero overrides: the filename's own title IS the header, the filename
    // code/episode is the state, and there is no synopsis tooltip.
    let got = build_activity_resolved(TV_RAW, Some(&act), &Resolved::default());
    assert_eq!(got.name, "Show", "header = the clean title");
    assert_eq!(
        got.details, "",
        "details blanks: the header carries the title"
    );
    assert_eq!(got.state, "S1 E2 \u{b7} Pilot", "state = tag + episode name");
    assert_eq!(got.large_text, "", "no synopsis = no tooltip");

    // Series override moves the header; line 2 keeps the episode identity.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&act),
        &Resolved {
            series: "Canonical Show".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, trunc("Canonical Show", 128), "series header");
    assert_eq!(got.details, "", "details blanks under the new header");
    assert_eq!(got.state, "S1 E2 \u{b7} Pilot", "state unchanged");

    // Episode override moves the episode title on line 2, never the header.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&act),
        &Resolved {
            episode_name: "Canonical Episode".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "header stays the series title");
    assert_eq!(got.details, "", "details stays blank");
    assert_eq!(got.state, "S1 E2 \u{b7} Canonical Episode", "episode state");

    // Overrides truncate at 128, never silently.
    let long = "y".repeat(200);
    let got = build_activity_resolved(
        TV_RAW,
        Some(&act),
        &Resolved {
            series: long.clone(),
            episode_name: long.clone(),
            ..Resolved::default()
        },
    );
    let n = java_units(&got.name);
    assert!(n <= 128, "header length {n} exceeds 128: {:?}", got.name);
    assert!(got.name.ends_with('\u{2026}'), "header signals the cut");
    let n = java_units(&got.state);
    assert!(n <= 128, "state length {n} exceeds 128: {:?}", got.state);
}

#[test]
fn build_activity_resolved_rich() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    const TV_TAG_ONLY_RAW: &str = "Show.S01E02.1080p.mkv - PotPlayer";
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };

    // artwork https URL becomes large_image
    let art = "https://example.com/poster.jpg";
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            artwork: art.to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.large_image, art, "large_image");

    // artwork empty or invalid sends NO image: the branded mark is never a
    // missing-art fallback.
    for art in [
        "",
        "logo",
        "ftp://example.com/x.jpg",
        "example.com/x.jpg",
        " /https://example.com/x.jpg",
    ] {
        let got = build_activity_resolved(
            TV_RAW,
            Some(&run_act),
            &Resolved {
                artwork: art.to_string(),
                ..Resolved::default()
            },
        );
        assert_eq!(
            got.large_image, "",
            "artwork {art:?}: no image slot, never the branded logo"
        );
    }

    // series episode tag synopsis semantics
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Canonical Series".to_string(),
            episode_name: "Canonical Episode".to_string(),
            synopsis: "A brave synopsis".to_string(),
            // Work-level show synopsis: it may ride the tooltip.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, trunc("Canonical Series", 128), "header");
    assert_eq!(got.details, "", "details blanks: the header carries the title");
    assert_eq!(
        got.state, "S1 E2 \u{b7} Canonical Episode",
        "state = code + episode title"
    );
    assert!(
        !got.state.contains("A brave synopsis"),
        "default state must not carry the synopsis: {:?}",
        got.state
    );
    // Hover slot: the synopsis snippet, never a duplicate of the title.
    assert_eq!(
        got.large_text,
        rp_text("A brave synopsis", 128),
        "large_text"
    );

    // A movie with only a synopsis: the header is the filename title (no year
    // known), line 1 blanks (it would repeat the header), line 2 stays empty,
    // and the synopsis is tooltip-only.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            is_movie: true,
            synopsis: "Just a synopsis".to_string(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Movie", "header");
    assert_eq!(got.details, "", "details would repeat the header");
    assert_eq!(got.state, "", "state");
    assert_eq!(got.large_text, "Just a synopsis", "large_text");

    // tag only becomes state; the header carries the show and line 1 blanks
    let got = build_activity_resolved(TV_TAG_ONLY_RAW, Some(&run_act), &Resolved::default());
    assert_eq!(got.name, "Show", "header");
    assert_eq!(got.details, "", "details blanks");
    assert_eq!(got.state, "S1 E2", "state");

    // A long movie synopsis is tooltip-only, explicitly ellipsized at 128;
    // the header is the title and line 1 blanks.
    let long = "s".repeat(200);
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            is_movie: true,
            synopsis: long.clone(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Movie", "header");
    assert_eq!(got.details, "", "details blanks");
    assert_eq!(got.state, "", "state");
    assert_eq!(
        got.large_text,
        format!("{}\u{2026}", "s".repeat(127)),
        "large_text"
    );
    let n = java_units(&got.large_text);
    assert!(n <= 128, "large_text length {n} exceeds 128: {:?}", got.large_text);
}

/// Discord rejects `large_image` longer than 300 chars (`code=4000`), which a
/// Kitsu-signed Backblaze URL can exceed. The composer must omit the
/// over-budget image entirely (no branded fallback), never truncate it (a cut
/// signed URL is a broken link) and never emit the oversized field.
#[test]
fn large_image_over_300_is_omitted() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let long_art = format!("https://example.com/{}", "x".repeat(400));
    assert!(long_art.len() > 300, "fixture must exceed the Discord cap");
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            artwork: long_art.clone(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.large_image, "",
        "over-budget artwork must be omitted (never the branded logo), got {:?}",
        got.large_image
    );
    assert!(
        got.large_image.len() <= 300,
        "large_image still oversized: {:?}",
        got.large_image
    );
    // Boundary: exactly 300 chars rides verbatim.
    let edge = format!("https://example.com/{}", "y".repeat(300 - "https://example.com/".len()));
    assert_eq!(edge.len(), 300, "edge fixture must be exactly 300 chars");
    let got_edge = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            artwork: edge.clone(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        got_edge.large_image, edge,
        "300-char artwork must pass through, got {:?}",
        got_edge.large_image
    );
}

#[test]
fn rp_text_cases() {
    assert_eq!(rp_text("a\n\nb  c", 128), "a b c", "collapse");
    assert_eq!(
        rp_text("eps1.1_ones-and-zer0es.mpeg", 128),
        "eps1.1_ones-and-zer0es.mpeg",
        "must preserve underscores/dots/dashes"
    );
    assert_eq!(rp_text("a\tb\rc", 128), "a b c", "tabs/newlines");
    assert_eq!(rp_text("a\x00b\x7Fc", 128), "abc", "C0 strip");
    let long = "z".repeat(200);
    assert_eq!(rp_text(&long, 128), trunc(&long, 128), "trunc");
}

#[test]
fn trim_word_cases() {
    assert_eq!(trim_word("hello world", 128), "hello world", "fits");
    // A long synopsis of whole words must end on "..." at a word boundary,
    // never mid-word: the hard cut would leave a dangling "wo...".
    let syn = format!("{}tail", "word ".repeat(40));
    let got = trim_word(&syn, 128);
    assert!(
        got.ends_with('\u{2026}'),
        "trim_word long = {got:?}, want trailing ellipsis"
    );
    assert!(
        !got.trim_end_matches('\u{2026}').ends_with("wo"),
        "trim_word ended mid-word: {got:?}"
    );
    assert!(
        got.trim_end_matches('\u{2026}').ends_with("word"),
        "trim_word should end on a whole word: {got:?}"
    );
    let n = java_units(&got);
    assert!(n <= 128, "trim_word length {n} exceeds 128: {got:?}");
    // No spaces: the cut is still signalled with a trailing ellipsis within
    // budget (never a silent hard cut, never a split surrogate).
    let nospace = "s".repeat(200);
    let got = trim_word(&nospace, 128);
    assert!(
        got.ends_with('\u{2026}'),
        "trim_word nospace = {got:?}, want trailing ellipsis"
    );
    assert_eq!(
        got,
        format!("{}\u{2026}", "s".repeat(127)),
        "trim_word nospace"
    );
    let n = java_units(&got);
    assert!(
        n <= 128,
        "trim_word nospace length {n} exceeds 128: {got:?}"
    );
    // CJK run (no spaces): the same signalled cut.
    let cjk = "\u{6f22}".repeat(300);
    let got = trim_word(&cjk, 128);
    assert!(
        got.ends_with('\u{2026}'),
        "trim_word cjk = {got:?}, want trailing ellipsis"
    );
    let n = java_units(&got);
    assert!(n <= 128, "trim_word cjk length {n} exceeds 128: {got:?}");
    let body = got.trim_end_matches('\u{2026}');
    let last = body.chars().next_back().unwrap();
    assert!(
        last.is_alphanumeric(),
        "trim_word cjk cut ends on {last:?}, want a word character: {got:?}"
    );
    // Surrogate safety: an emoji straddling the cut is dropped whole, never
    // split (the old UTF-16 hard cut left a `?` from the lone surrogate).
    let split = format!("{}\u{1f600}{}", "a".repeat(126), "b".repeat(100));
    let got = trim_word(&split, 128);
    assert!(
        got.ends_with('\u{2026}'),
        "trim_word surrogate = {got:?}, want trailing ellipsis"
    );
    assert!(
        !got.contains('?'),
        "trim_word split a surrogate (`?`): {got:?}"
    );
    assert_eq!(
        got,
        format!("{}\u{2026}", "a".repeat(126)),
        "trim_word surrogate"
    );
    // Sentence-end cut: the word-boundary cut lands on `.` followed by a
    // space, so the complete sentence is kept with its own terminator and no
    // ellipsis (the `x×120 + ".…"` shape was the live defect).
    let sentence = format!("{}. {}", "x".repeat(120), "y".repeat(20));
    let got = trim_word(&sentence, 128);
    assert_eq!(
        got,
        "x".repeat(120) + ".",
        "trim_word sentence cut"
    );
    assert!(
        !got.contains('\u{2026}'),
        "complete sentence needs no ellipsis: {got:?}"
    );
    let n = java_units(&got);
    assert!(
        n <= 128,
        "trim_word sentence length {n} exceeds 128: {got:?}"
    );
    assert_eq!(trim_word("", 128), "", "trim_word empty");
}

/// The punctuation rule every truncated card line owes: an ellipsis-terminated
/// cut ends on a word character (never `,` `;` `:` `-` `–` `—` `·` or blank),
/// and the cut lands on a word boundary (the source resumes at whitespace, at
/// a stripped cut mark, or not at all).
fn assert_clean_cut(full: &str, got: &str) {
    assert!(
        got.ends_with('\u{2026}'),
        "cut line must end with U+2026: {got:?}"
    );
    assert!(!got.contains("..."), "never three dots: {got:?}");
    let body = got.trim_end_matches('\u{2026}');
    assert!(!body.is_empty(), "empty cut body: {got:?}");
    let last = body.chars().next_back().unwrap();
    assert!(
        !last.is_whitespace()
            && !matches!(last, ',' | ';' | ':' | '-' | '–' | '—' | '·'),
        "cut leaves trailing punctuation: {got:?}"
    );
    let tail = full
        .strip_prefix(body)
        .unwrap_or_else(|| panic!("cut body is not a source prefix: {got:?}"));
    let next = tail
        .chars()
        .next()
        .unwrap_or_else(|| panic!("cut text was not cut: {got:?}"));
    assert!(
        next.is_whitespace() || matches!(next, ',' | ';' | ':' | '-' | '–' | '—' | '·'),
        "cut ends mid-word (source resumes with {next:?}): {got:?}"
    );
}

/// A truncated `trim_word` line strips the cut punctuation too: `word, next`
/// cut at the space ends `word…`, never `word,…`.
#[test]
fn trim_word_strips_cut_punctuation() {
    let s = format!("{}stop, {}", "filler ".repeat(20), "rest ".repeat(20));
    let got = trim_word(&s, 128);
    assert_clean_cut(&s, &got);
    // The 128-unit budget fits 18 whole fillers: the cut lands on the word
    // boundary, never inside `filler` or the `stop,` tail.
    let want = format!("{}\u{2026}", "filler ".repeat(18).trim());
    assert_eq!(got, want, "trim_word cut");
}

/// Owner mapping: the movie body is title/playback; the synopsis is tooltip
/// only.
#[test]
fn movie_default_state_skips_synopsis() {
    const MOVIE_RAW: &str = "Dragon.Ball.Movie.1990.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let syn = "Years after the battle with Earth's greatest enemy, Majin Buu, the world faces a new threat from beyond the stars and more";
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dragon Ball".to_string(),
            year: 1990,
            is_movie: true,
            synopsis: syn.to_string(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "1990", "details");
    assert_eq!(got.state, "", "state");
    assert!(
        !got.state.contains("Majin Buu"),
        "default state must not carry the synopsis: {:?}",
        got.state
    );
    assert_eq!(got.large_text, syn, "large_text carries the synopsis");
}

/// T4: the default series state is the synopsis line (tag plus synopsis when
/// no `SxxExx` code is known): the synopsis no longer stays off the card.
/// The tooltip keeps the whole note.
#[test]
fn series_default_state_is_code_plus_synopsis() {
    const TV_RAW: &str = "Show.S02E04.Episode.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let syn = "Allsafe is controlled; the Dark Army is ready to meet Elliot; the fsociety plan falls apart at midnight somehow";
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            synopsis: syn.to_string(),
            // No episode pin: this is the work synopsis, so it rides.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert!(
        got.state.starts_with("S2 E4 \u{b7} "),
        "state = {:?}, want the code plus the episode title",
        got.state
    );
    assert!(
        !got.state.contains("Elliot"),
        "default state must not carry the synopsis: {:?}",
        got.state
    );
    let n = java_units(&got.state);
    assert!(
        n <= crate::STATE_BUDGET,
        "state length {n} exceeds {}: {:?}",
        crate::STATE_BUDGET,
        got.state
    );
    assert_eq!(got.large_text, crate::synopsis_snippet(syn), "large_text");
    assert_eq!(
        got.details, "",
        "details blanks: the header carries the series title"
    );
    assert_eq!(got.name, "Show", "header = the series title");
}

/// The owner's RPC contract for the two representative cases: MOVIE header is
/// the movie title over `<year> · <genres>` + runtime; EPISODE header is the
/// series title over `SxxEyy · Episode`; Watching on the wire; artwork
/// preserved; no icon assets anywhere; the work synopsis only in the artwork
/// tooltip, capped and explicitly ellipsized, episode prose nowhere.
#[test]
fn media_rpc_contract_movie_and_episode() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    const EP_RAW: &str = "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer";
    const ART: &str = "https://example.com/poster.jpg";
    let run = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let long_syn = "A retired operative living quietly is pulled back when a ruthless syndicate burns his home. He crosses continents, gathers old allies, and faces a final choice that will decide everything he loves and more.";
    let movie = build_activity_resolved(
        MOVIE_RAW,
        Some(&run),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi, Adventure".to_string(),
            length: "2h 46m".to_string(),
            synopsis: long_syn.to_string(),
            artwork: ART.to_string(),
            // A movie synopsis describes the work, so it rides the tooltip.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(movie.type_, "PLAYING", "activity kind marker");
    assert_eq!(movie.name, "Dune: Part Two", "header is the movie title");
    assert_eq!(movie.details, "2024 \u{b7} Sci-Fi, Adventure", "movie details");
    assert_eq!(movie.state, "2h 46m", "movie state");
    assert_eq!(movie.large_image, ART, "movie poster");
    assert_eq!(movie.small_image, "", "no status asset");
    assert_eq!(movie.small_text, "", "no status text");
    assert!(
        !movie.details.contains("retired operative")
            && !movie.state.contains("retired operative"),
        "synopsis must not ride a body line"
    );
    assert!(
        movie.large_text.ends_with('\u{2026}') && java_units(&movie.large_text) <= 128,
        "Discord tooltip must be a capped, explicit snippet: {:?}",
        movie.large_text
    );

    let episode = build_activity_resolved(
        EP_RAW,
        Some(&run),
        &Resolved {
            series: "Breaking Bad".to_string(),
            episode_name: "Problem Dog".to_string(),
            season: 4,
            number: 7,
            synopsis: long_syn.to_string(),
            artwork: ART.to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(episode.type_, "PLAYING", "activity kind marker");
    assert_eq!(episode.name, "Breaking Bad", "header is the series title");
    assert_eq!(
        episode.details, "",
        "episode details blanks: the header carries the title"
    );
    assert_eq!(episode.state, "S04E07 \u{b7} Problem Dog", "episode state");
    assert_eq!(episode.large_image, ART, "series poster");
    assert_eq!(episode.small_image, "", "no status asset");
    assert_eq!(episode.small_text, "", "no status text");
    assert!(
        !episode.details.contains("Problem Dog")
            && !episode.state.contains("retired operative"),
        "episode identity is split correctly"
    );
    // Episode-pinned prose never rides the tooltip: it would spoil the
    // episode. The large_text stays empty even though a synopsis is stored.
    assert_eq!(
        episode.large_text, "",
        "episode prose must not ride: {:?}",
        episode.large_text
    );
}


/// line ends on a whole word with U+2026, never mid-word with no terminator.
#[test]
fn hover_tooltip_cut_ends_clean() {
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let syn = "Ready Player One is a 2018 American science fiction action film directed by Steven Spielberg and cowritten with Zak Penn alongside Ernest Cline. Based on the novel, Wade hunts the prize.";
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Ready Player One".to_string(),
            year: 2018,
            is_movie: true,
            synopsis: syn.to_string(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2018", "details");
    assert_eq!(got.state, "", "no runtime facts, so no state");
    assert_eq!(got.name, "Ready Player One", "header is the movie title");
    // The synopsis rides the artwork tooltip, cut at 128 and always
    // ellipsized; line 1 stays the clean title.
    assert!(
        got.large_text.ends_with('\u{2026}'),
        "large_text must signal the cut: {:?}",
        got.large_text
    );
    assert_clean_cut(syn, &got.large_text);
    let n = got.large_text.encode_utf16().count();
    assert!(
        n <= 128,
        "large_text length {n} exceeds 128: {:?}",
        got.large_text
    );
}

#[test]
fn build_activity_resolved_movie() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };

    // Movie with a year and no synopsis: the year on line 1 and an
    // empty line 2 (no runtime fact).
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024", "details");
    assert_eq!(got.state, "", "state");
    assert_eq!(got.large_text, "", "large_text");
    assert_eq!(got.name, "Dune: Part Two", "header is the movie title");

    // Bare movie: the header is the title, no fabricated year; line 1 blanks
    // because it would repeat the header.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune: Part Two", "header");
    assert_eq!(got.details, "", "details would repeat the header");
    assert_eq!(got.state, "", "state");
    assert_eq!(got.large_text, "", "large_text");

    // Year and genres on line 1; runtime on line 2; the work synopsis is
    // only in the artwork tooltip.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "details");
    assert_eq!(got.state, "2h 46m", "state");
    assert_eq!(got.large_text, "Paul unites the Fremen", "large_text");

    // Missing year/genres: no line 1; line 2 is the runtime; the work
    // synopsis is tooltip-only.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune".to_string(),
            is_movie: true,
            length: "2h 19m".to_string(),
            synopsis: "A quest".to_string(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune", "header");
    assert_eq!(got.details, "", "details would repeat the header");
    assert_eq!(got.state, "2h 19m", "state");
    assert_eq!(got.large_text, "A quest", "large_text");

    // Long synopsis is capped in the tooltip, explicitly ellipsized; the body
    // lines are unaffected. Genres without a year still lead line 1.
    let syn = format!("{}finale", "epic ".repeat(40));
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune".to_string(),
            is_movie: true,
            genres: "Adventure".to_string(),
            length: "2h 46m".to_string(),
            synopsis: syn.clone(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune", "header");
    assert_eq!(got.details, "Adventure", "details carry the genres");
    assert_eq!(got.state, "2h 46m", "state");
    assert_eq!(got.large_text, crate::synopsis_snippet(&syn), "large_text");
    assert!(got.large_text.ends_with('\u{2026}'), "tooltip signals the cut");
    let n = java_units(&got.large_text);
    assert!(n <= 128, "large_text length {n} exceeds 128: {:?}", got.large_text);
    let n = java_units(&got.state);
    assert!(n <= 128, "state length {n} exceeds 128: {:?}", got.state);

    // paused movie keeps the Paused marker on the runtime line
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&Action {
            paused: true,
            ct: 1000,
            ..Action::default()
        }),
        &Resolved {
            series: "Dune".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.state, "2h 46m \u{b7} Paused",
        "paused state"
    );
    assert!(
        got.state.ends_with(PAUSED_MARKER),
        "paused state = {:?}, want suffix {PAUSED_MARKER:?}",
        got.state
    );
    let n = java_units(&got.state);
    assert!(
        n <= 128,
        "paused state length {n} exceeds 128: {:?}",
        got.state
    );

    // empty artwork sends no image: never the branded logo
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune".to_string(),
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.large_image, "", "large_image");
}

#[test]
fn build_activity_resolved_series_state() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Canonical Series".to_string(),
            episode_name: "Canonical Episode".to_string(),
            synopsis: "A brave new world unfolds".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.name, "Canonical Series",
        "header is the series title"
    );
    assert_eq!(
        got.details, "",
        "details blanks: the header carries the series title"
    );
    assert_eq!(
        got.state, "S1 E2 \u{b7} Canonical Episode",
        "state is code + episode"
    );
    assert!(
        !got.state.contains("brave"),
        "default state must not carry the synopsis: {:?}",
        got.state
    );
    assert_eq!(
        got.large_text, "",
        "episode-pinned prose never rides the tooltip: {:?}",
        got.large_text
    );
}

#[test]
fn build_activity_resolved_buttons() {
    // Pristine card: resolved buttons are always ignored, even valid ones.
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };

    // valid buttons stripped
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            buttons: vec![
                Button {
                    label: "Trailer".to_string(),
                    url: "https://example.com/trailer".to_string(),
                },
                Button {
                    label: "IMDb".to_string(),
                    url: "https://example.com/imdb".to_string(),
                },
            ],
            ..Resolved::default()
        },
    );
    assert!(
        got.buttons.is_empty(),
        "buttons = {:?}, want stripped (empty)",
        got.buttons
    );

    // build_activity carries no buttons
    assert!(
        build_activity(MOVIE_RAW, Some(&run_act)).buttons.is_empty(),
        "build_activity buttons, want empty"
    );
}

/// The clean-card proof: no buttons on any card. The legacy filename-only
/// builder keeps its pristine no-icon shape (it is not the shipped path); the
/// per-kind composer carries the registered logo asset plus the Playing/Paused
/// status text in the small slot, and the timer anchors stay intact.
#[test]
fn pristine_card_no_icons_no_buttons() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let pause_act = Action {
        paused: true,
        ct: 83000,
        ..Action::default()
    };
    let stop_act = Action {
        ct: 0,
        ..Action::default()
    };
    let res = Resolved {
        series: "Show".to_string(),
        episode_name: "Pilot".to_string(),
        artwork: "https://example.com/poster.jpg".to_string(),
        buttons: vec![Button {
            label: "Trailer".to_string(),
            url: "https://example.com/trailer".to_string(),
        }],
        ..Resolved::default()
    };
    let cards = [
        build_activity(TV_RAW, Some(&run_act)),
        build_activity(TV_RAW, Some(&pause_act)),
        build_activity(TV_RAW, Some(&stop_act)),
        build_activity("PotPlayer", Some(&Action::default())),
        build_activity_resolved(TV_RAW, Some(&run_act), &res),
        build_activity_resolved(TV_RAW, Some(&pause_act), &res),
    ];
    for (i, got) in cards.iter().enumerate() {
        assert!(
            got.buttons.is_empty(),
            "card {i}: buttons={:?}, want none",
            got.buttons
        );
    }
    // No card carries a corner slot: no status icon over the artwork.
    for got in &cards {
        assert!(
            got.small_image.is_empty() && got.small_text.is_empty(),
            "card: smallImage={:?} smallText={:?}, want none",
            got.small_image,
            got.small_text
        );
    }
    // Spot-check the intact lines: the legacy filename mapping and the resolved
    // card's media-title header with the series/episode roles.
    let run = &cards[0];
    assert!(
        run.name == "Show" && run.details == "Pilot" && run.state == "S1 E2",
        "running lines = {:?}/{:?}/{:?}, want Show/Pilot/S1 E2",
        run.name,
        run.details,
        run.state
    );
    assert!(
        cards[4].name == "Show"
            && cards[4].details.is_empty()
            && cards[4].state == "S1 E2 \u{b7} Pilot",
        "resolved lines = {:?}/{:?}/{:?}",
        cards[4].name,
        cards[4].details,
        cards[4].state
    );
    assert!(
        run.has_start && run.start == 12345,
        "running timer hasStart={} start={}, want true/12345",
        run.has_start,
        run.start
    );
    assert!(
        cards[1].paused && cards[5].paused,
        "paused cards paused=false, want true (the gate's park signal)"
    );
}

#[test]
fn smart_trim_synopsis_sentence_no_ellipsis() {
    let s = "Intro filler words to push length out a bit. Second sentence continues long enough to exceed the seventy two unit budget for sure and more.";
    let got = smart_trim_synopsis(s, STATE_SYNOPSIS_BUDGET);
    let want = "Intro filler words to push length out a bit.";
    assert_eq!(got, want, "sentence cut");
    assert!(
        !got.contains('\u{2026}'),
        "complete sentence needs no ellipsis: {got:?}"
    );
    assert!(!got.contains("..."), "never three dots: {got:?}");
    let n = java_units(&got);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {got:?}"
    );
}

#[test]
fn smart_trim_synopsis_clause_ellipsis() {
    let s = format!("{}first, {}", "word ".repeat(10), "second ".repeat(20));
    let got = smart_trim_synopsis(&s, STATE_SYNOPSIS_BUDGET);
    assert!(
        got.ends_with('\u{2026}'),
        "clause cut should end with U+2026: {got:?}"
    );
    assert!(!got.contains("..."), "never three dots: {got:?}");
    // The clause comma is a cut mark, not content: the live card carried it
    // into the ellipsis (`Buu,…`), so the cut strips trailing punctuation and
    // the base ends on the word (`first…`).
    let want = format!("{}first\u{2026}", "word ".repeat(10));
    assert_eq!(got, want, "clause cut");
    assert_clean_cut(&s, &got);
    let n = java_units(&got);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {got:?}"
    );
    // No sentence end in this input, so the comma path must win.
    assert!(
        !got.trim_end_matches('\u{2026}').contains('.'),
        "unexpected sentence cut: {got:?}"
    );
}

#[test]
fn smart_trim_synopsis_word_ellipsis() {
    let s = format!("{}finale", "epic ".repeat(30));
    let got = smart_trim_synopsis(&s, STATE_SYNOPSIS_BUDGET);
    assert!(
        got.ends_with('\u{2026}'),
        "word cut should end with U+2026: {got:?}"
    );
    assert!(!got.contains("..."), "never three dots: {got:?}");
    let body = got.trim_end_matches('\u{2026}');
    assert!(
        body.ends_with("epic"),
        "should land on a whole word: {got:?}"
    );
    let n = java_units(&got);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {got:?}"
    );
}

/// No boundary anywhere (single token, no spaces): the tail still signals the
/// cut with a single trailing U+2026 within budget, never a silent hard cut.
#[test]
fn smart_trim_synopsis_nospace_ellipsis() {
    let s = "y".repeat(300);
    let got = smart_trim_synopsis(&s, STATE_SYNOPSIS_BUDGET);
    assert!(
        got.ends_with('\u{2026}'),
        "nospace cut should end with U+2026: {got:?}"
    );
    assert!(!got.contains("..."), "never three dots: {got:?}");
    assert_eq!(
        got,
        format!("{}\u{2026}", "y".repeat(STATE_SYNOPSIS_BUDGET - 1)),
        "nospace cut"
    );
    let n = java_units(&got);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {got:?}"
    );
}

#[test]
fn smart_trim_synopsis_strips_connector() {
    let s = format!("{}and {}", "word ".repeat(13), "tailword ".repeat(20));
    let got = smart_trim_synopsis(&s, STATE_SYNOPSIS_BUDGET);
    assert!(
        got.ends_with('\u{2026}'),
        "connector cut should end with U+2026: {got:?}"
    );
    assert!(
        !got.ends_with("and\u{2026}") && !got.ends_with("and \u{2026}"),
        "dangling connector survived: {got:?}"
    );
    let body = got.trim_end_matches('\u{2026}');
    let fields: Vec<&str> = body.split_whitespace().collect();
    assert!(!fields.is_empty(), "empty body: {got:?}");
    let last = fields[fields.len() - 1].to_lowercase();
    for c in ["and", "or", "but", "the", "a", "of", "to"] {
        assert!(
            last != c,
            "last word {last:?} is a dangling connector: {got:?}"
        );
    }
    let n = java_units(&got);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {got:?}"
    );
}

#[test]
fn idle_card_budget() {
    let got = build_activity("PotPlayer", Some(&Action::default()));
    assert!(
        got.details == "Browsing" && got.state == "Idle",
        "idle card = {got:?}, want Browsing/Idle"
    );
    let n = java_units(&got.state);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "idle state length {n} exceeds {STATE_SYNOPSIS_BUDGET}"
    );
}

/// The synopsis-bearing State line (the explicit Synopsis preset) trims the
/// longest note well below its tooltip: the same synopsis fills the 128-unit
/// tooltip but lands on the card state inside the 72-unit budget.
#[test]
fn synopsis_before_after_longest() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let syn_spec = CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: CARD_STATE_SYNOPSIS.to_string(),
    };
    let long_syn = "Paul Atreides unites with the Fremen and wages war against House Harkonnen. As betrayal surrounds him, he faces a choice between the love of his life and the fate of the known universe, striving to prevent a terrible future only he can foresee and more beyond.";
    let before = trim_word(
        &format!("Sci-Fi \u{b7} 2h 46m \u{b7} {}", rp_text(long_syn, 1 << 28)),
        128,
    );
    let after = compose_card(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            is_movie: true,
            series: "Dune: Part Two".to_string(),
            year: 2024,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: long_syn.to_string(),
            ..Resolved::default()
        },
        true,
        &syn_spec,
    )
    .state;
    let n = java_units(&after);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "after length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {after:?}"
    );
    assert!(
        java_units(&before) > java_units(&after),
        "expected reduction: before={} after={}",
        java_units(&before),
        java_units(&after)
    );
}

/// The synopsis-bearing State line (the explicit Synopsis preset; the default
/// state no longer carries the synopsis) stays within the 72-unit
/// [`STATE_SYNOPSIS_BUDGET`] in every running/paused, movie/series shape, and
/// its structured prefixes survive whole.
#[test]
fn synopsis_state_budget_72() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let paused_act = Action {
        paused: true,
        ct: 1000,
        ..Action::default()
    };
    let long_syn = "A retired operative living quietly is pulled back when a ruthless syndicate burns his home. He crosses continents, gathers old allies, and faces a final choice that will decide everything he loves and more.";
    // The 72-unit synopsis budget lives on the explicit Synopsis preset: the
    // default state carries no synopsis, so compose through the preset.
    let syn_spec = CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: CARD_STATE_SYNOPSIS.to_string(),
    };
    let compose = |raw: &str, act: &Action, res: &Resolved| {
        compose_card(raw, Some(act), res, res.is_movie, &syn_spec)
    };
    let movie = |act: &Action| {
        compose(
            MOVIE_RAW,
            act,
            &Resolved {
                is_movie: true,
                genres: "Action, Drama".to_string(),
                length: "2h 19m".to_string(),
                synopsis: long_syn.to_string(),
                ..Resolved::default()
            },
        )
    };
    let series = |act: &Action| {
        compose(
            TV_RAW,
            act,
            &Resolved {
                synopsis: long_syn.to_string(),
                ..Resolved::default()
            },
        )
    };
    let paused_movie = compose(
        MOVIE_RAW,
        &paused_act,
        &Resolved {
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: long_syn.to_string(),
            ..Resolved::default()
        },
    );
    let paused_series = compose(
        TV_RAW,
        &paused_act,
        &Resolved {
            synopsis: long_syn.to_string(),
            ..Resolved::default()
        },
    );
    let cases: [(&str, Activity); 5] = [
        ("movie", movie(&run_act)),
        ("series tag+synopsis", series(&run_act)),
        ("movie paused", paused_movie.clone()),
        ("series paused tag+synopsis", paused_series),
        (
            "series synopsis only",
            compose(
                MOVIE_RAW,
                &run_act,
                &Resolved {
                    synopsis: long_syn.to_string(),
                    ..Resolved::default()
                },
            ),
        ),
    ];
    for (name, act) in &cases {
        let n = java_units(&act.state);
        assert!(
            !act.state.contains("..."),
            "{name}: never three dots: {:?}",
            act.state
        );
        assert!(
            n <= STATE_SYNOPSIS_BUDGET,
            "{name}: state length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {:?}",
            act.state
        );
        assert!(
            n <= 128,
            "{name}: state length {n} exceeds 128 ceiling: {:?}",
            act.state
        );
    }
    // Structured prefixes survive in full.
    let movie = movie(&run_act);
    assert!(
        movie.state.contains("Action, Drama") && movie.state.contains("2h 19m"),
        "movie prefix lost: {:?}",
        movie.state
    );
    let series = series(&run_act);
    assert!(
        series.state.starts_with("S1 E2"),
        "episode tag lost: {:?}",
        series.state
    );
    let paused = paused_movie;
    assert!(
        paused.state.ends_with(PAUSED_MARKER),
        "paused marker lost: {:?}",
        paused.state
    );
}

/// Proves the two surfaces agree on one synopsis under the explicit Synopsis
/// preset: the same Resolved input yields a State line within the 72-unit
/// budget and a LargeText hover tooltip within Discord's 128-unit asset-text
/// cap. The mid-length synopsis (~99 units) exceeds the State budget so the
/// preset trims it, but fits the tooltip whole.
#[test]
fn hover_tooltip_whole_and_state_budget_agree() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let syn_spec = CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: CARD_STATE_SYNOPSIS.to_string(),
    };
    let compose = |raw: &str, res: &Resolved| {
        compose_card(raw, Some(&run_act), res, res.is_movie, &syn_spec)
    };
    // ~99 units: over the 72 State budget, under the 128 tooltip cap.
    let mid_syn = "word ".repeat(20).trim().to_string();
    let n = java_units(&mid_syn);
    assert!(
        n > STATE_SYNOPSIS_BUDGET && n <= 128,
        "fixture setup: mid_syn units={n}, want 72 < n <= 128"
    );
    // Work-level synopsis rides the tooltip whole on both kinds; the
    // Synopsis-preset state line carries it within the 72-unit budget.
    let cases: [(&str, Activity, &str); 2] = [
        (
            "movie synopsis-only",
            compose(
                MOVIE_RAW,
                &Resolved {
                    is_movie: true,
                    series: "Mid Film".to_string(),
                    synopsis: mid_syn.clone(),
                    // Work-level: the tooltip mechanics are the point here.
                    synopsis_is_work: true,
                    ..Resolved::default()
                },
            ),
            &mid_syn,
        ),
        (
            "series tag+synopsis",
            compose(
                TV_RAW,
                &Resolved {
                    series: "Mid Series".to_string(),
                    synopsis: mid_syn.clone(),
                    // Work-level: the tooltip mechanics are the point here.
                    synopsis_is_work: true,
                    ..Resolved::default()
                },
            ),
            &mid_syn,
        ),
    ];
    for (name, act, syn) in &cases {
        let n = java_units(&act.state);
        assert!(
            n <= STATE_SYNOPSIS_BUDGET,
            "{name}: state length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {:?}",
            act.state
        );
        let n = java_units(&act.large_text);
        assert!(
            n <= 128,
            "{name}: large_text length {n} exceeds 128: {:?}",
            act.large_text
        );
        if name.starts_with("movie") {
            assert_eq!(
                act.large_text, *syn,
                "{name}: movie tooltip carries the short synopsis whole"
            );
            assert_eq!(
                act.name, "Mid Film",
                "{name}: the header carries the movie title"
            );
            assert!(
                !act.details.contains(*syn),
                "{name}: movie details must not carry the synopsis: {:?}",
                act.details
            );
            continue;
        }
        // The tooltip carries the short synopsis whole, not the State trim.
        assert_eq!(
            act.large_text,
            rp_text(syn, 128),
            "{name}: large_text, want whole synopsis"
        );
        assert_eq!(
            act.large_text, *syn,
            "{name}: large_text should arrive whole (no ellipsis)"
        );
        assert!(
            act.large_text != act.state,
            "{name}: tooltip must not equal the trimmed State line"
        );
    }
    // Over-cap synopsis still respects both ceilings on the same input.
    // T5-A: the movie tooltip is omitted; the Synopsis-preset state still
    // carries the synopsis within the 72 budget.
    let over_syn = "s".repeat(200);
    let over = compose(
        MOVIE_RAW,
        &Resolved {
            is_movie: true,
            series: "Over Film".to_string(),
            synopsis: over_syn.clone(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    let n = java_units(&over.state);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "over-cap: state length {n} exceeds {STATE_SYNOPSIS_BUDGET}"
    );
    assert_eq!(over.large_text, crate::synopsis_snippet(&over_syn), "over-cap: tooltip snippet");
    assert!(
        over.large_text.ends_with('\u{2026}'),
        "over-cap: tooltip must signal the cut"
    );
    assert_eq!(over.name, "Over Film", "over-cap: header is the movie title");
    assert_eq!(
        over.details, "",
        "over-cap: details blanks (it would repeat the header)"
    );
}

/// Proves LargeText no longer duplicates the series title (name, already the card
/// header) when a synopsis exists.
#[test]
fn hover_tooltip_not_series_name() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Canonical Series".to_string(),
            synopsis: "A brave synopsis".to_string(),
            // Work-level show note: it rides the tooltip.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.name, "Canonical Series",
        "name = {:?}, want the series title",
        got.name
    );
    assert!(
        got.large_text != got.name,
        "large_text {:?} duplicates name; want the synopsis",
        got.large_text
    );
    assert_eq!(
        got.large_text,
        rp_text("A brave synopsis", 128),
        "large_text"
    );
}

/// Without a synopsis the artwork tooltip is omitted (no title fallback): the
/// tooltip slot is the synopsis channel only. The idle card carries no assets.
#[test]
fn hover_tooltip_fallback_without_synopsis() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let with_series = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Canonical Show".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        with_series.large_text, "",
        "no synopsis means no series-title fallback"
    );
    let bare = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(bare.large_text, "", "movie tooltip is omitted");
    let movie_with_title = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Some Film".to_string(),
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(
        movie_with_title.large_text, "",
        "movie tooltip stays omitted with a title"
    );
    let idle = build_activity_resolved("PotPlayer", Some(&Action::default()), &Resolved::default());
    assert_eq!(idle.large_text, "", "idle card carries no asset text");
    assert_eq!(idle.large_image, "", "idle card carries no branded artwork");
}

/// A signed 396-char Kitsu-style URL that no backfill replaced is omitted: over
/// Discord's 300-char `large_image` budget, dropped whole (never truncated,
/// never replaced by the branded mark).
#[test]
fn unreplaced_signed_artwork_is_omitted() {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let mut art =
        "https://f000.backblazeb2.com/file/kitsu-prod/50497.jpg?Expires=900&X-Amz-Signature="
            .to_string();
    while art.len() < 396 {
        art.push('a');
    }
    assert_eq!(art.len(), 396, "fixture mirrors live Kitsu 50497");
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            artwork: art,
            ..Resolved::default()
        },
    );
    assert_eq!(got.large_image, "", "over-budget signed artwork");
}

/// T1: the one `SxxExx` formatter. Season+number renders `S01E02`; an
/// absolute-only episode renders `E02` and never fabricates a season -- and a
/// season-0 number never grows one either: the AniList absolute shape
/// `(season 0, number = absolute)` renders `E05`, not `S00E05`. When the two
/// numbers disagree (`season 0, number != absolute`) the absolute number wins
/// (`E05` for `(0, 7, 5)`). A tag
/// already matching `SxxExx` passes through; a missing number omits
/// the code.
#[test]
fn episode_code_formatter() {
    use crate::episode_code;
    assert_eq!(episode_code(1, 2, 0, ""), "S01E02");
    assert_eq!(episode_code(12, 34, 0, ""), "S12E34");
    assert_eq!(
        episode_code(2, 5, 27, ""),
        "S02E05",
        "season wins over absolute"
    );
    assert_eq!(episode_code(0, 0, 2, ""), "E02", "absolute only");
    assert!(
        !episode_code(0, 0, 2, "").contains('S'),
        "absolute must never fabricate a season"
    );
    assert_eq!(
        episode_code(0, 0, 0, ""),
        "",
        "nothing known omits the code"
    );
    assert_eq!(
        episode_code(1, 0, 0, ""),
        "",
        "missing number omits the code"
    );
    assert_eq!(
        episode_code(0, 0, 0, "S01E02"),
        "S01E02",
        "strict tag passes through"
    );
    assert_eq!(
        episode_code(0, 0, 0, "s1e2"),
        "S01E02",
        "case-insensitive passthrough"
    );
    assert_eq!(
        episode_code(0, 0, 0, "S1 E2"),
        "",
        "spaced tag keeps the current tag behavior"
    );
    assert_eq!(
        episode_code(0, 0, 0, "Pilot"),
        "",
        "non-code tag is no code"
    );
    // The real AniList absolute production shape: season 0 with the number
    // set to the absolute episode. No season is fabricated -- there is no
    // distinct signal for a real S00 special in this shape, so it renders
    // the bare absolute number the same way.
    assert_eq!(
        episode_code(0, 5, 5, ""),
        "E05",
        "absolute production shape (season 0, number = absolute)"
    );
    assert!(
        !episode_code(0, 5, 5, "").contains('S'),
        "season 0 must never fabricate S00"
    );
    assert_eq!(
        episode_code(0, 3, 0, ""),
        "E03",
        "season-0 number renders bare, never S00"
    );
    assert_eq!(
        episode_code(0, 7, 5, ""),
        "E05",
        "season 0 with disagreeing numbers: the absolute number wins"
    );
    assert!(
        !episode_code(0, 7, 5, "").contains('S'),
        "disagreeing season-0 numbers must never fabricate S00"
    );
    assert_eq!(
        episode_code(0, 0, 0, ""),
        "",
        "special with no number omits the code"
    );
}

/// Owner mapping: the header is the series title; the episode code and title
/// live together on the state line, and no body line repeats the header.
#[test]
fn series_default_details_show_plus_code() {
    const TV_RAW: &str = "Show.S01E02.Confrontation.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Confrontation".to_string(),
            synopsis: "A brave synopsis".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "header is the series");
    assert_eq!(
        got.details, "",
        "details blanks: the header carries the series"
    );
    assert!(
        !got.name.contains("Confrontation"),
        "the header must not carry the episode title: {:?}",
        got.name
    );
    assert!(
        !got.name.contains("S01E02") && !got.name.contains("S1 E2"),
        "the header must not carry the code: {:?}",
        got.name
    );
    assert_eq!(
        got.state, "S01E02 \u{b7} Confrontation",
        "state is the code and episode title"
    );
    let n = java_units(&got.name);
    assert!(
        n <= 128,
        "header length {n} exceeds 128: {:?}",
        got.name
    );
}

/// Owner mapping: the header is the series title, falling back to the clean
/// filename title when the lookup misses, and truncating at 128. The episode
/// title and code never appear in the header, and line 1 blanks.
#[test]
fn series_default_details_fallbacks_and_truncation() {
    const TV_RAW: &str = "Show.S01E02.Confrontation.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    // Canonical series title wins.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Confrontation".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "series title first");
    assert_eq!(got.details, "", "details blanks: the header carries it");
    assert!(
        !got.name.contains("Confrontation") && !got.name.contains("S01E02"),
        "episode identity must not leak into the header: {:?}",
        got.name
    );
    // No canonical series: the clean filename title stands in.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            episode_name: "Confrontation".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "clean title fallback");
    // Nothing resolved: still the clean filename title.
    let got = build_activity_resolved(TV_RAW, Some(&run_act), &Resolved::default());
    assert_eq!(got.name, "Show", "filename title fallback");
    // A long series title truncates at 128, never silently.
    let long = "D".repeat(200);
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: long.clone(),
            episode_name: "Confrontation".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    let n = java_units(&got.name);
    assert!(
        n <= 128,
        "header length {n} exceeds 128: {:?}",
        got.name
    );
    assert!(
        got.name.starts_with("D") && got.name.ends_with('\u{2026}'),
        "truncated header must keep the head and signal the cut: {:?}",
        got.name
    );
}

/// Unresolved `Show.S01E02.1080p.mkv` (no episode title, spaced tag `S1 E2`):
/// the header is the show title (and line 1 blanks); the state line carries the
/// tag alone.
#[test]
fn series_default_details_skips_spaced_tag() {
    const RAW: &str = "Show.S01E02.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let parsed = titleclean::clean("Show.S01E02.1080p.mkv");
    assert!(
        parsed.episode_name.is_empty(),
        "fixture has no episode name: {:?}",
        parsed.episode_name
    );
    assert_eq!(parsed.episode_tag, "S1 E2", "fixture tag");
    assert_eq!(parsed.title, "Show", "fixture clean title");
    let got = build_activity_resolved(RAW, Some(&run_act), &Resolved::default());
    assert_eq!(got.name, "Show", "header is the show title");
    assert_eq!(got.details, "", "details blanks");
    assert_eq!(got.state, "S1 E2", "state carries the tag");
    // The episode preset keeps the episode title alone: no code prefix.
    let ep_spec = CardSpec {
        details: crate::cardspec::CARD_DETAILS_EPISODE.to_string(),
        state: CARD_STATE_DEFAULT.to_string(),
    };
    let card = compose_card(
        RAW,
        Some(&run_act),
        &Resolved {
            episode_name: "Confrontation".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
        false,
        &ep_spec,
    );
    assert_eq!(
        card.details, "Confrontation",
        "episode preset is the title alone"
    );
}

/// T4: series default state is the synopsis line (`SxxExx` code plus the
/// synopsis, the synopsis-preset shape), keeps the paused marker, and the
/// synopsis preset keeps its `-` join. Details stays the episode title.
#[test]
fn series_default_state_prefers_episode_title() {
    const TV_RAW: &str = "Show.S01E02.Confrontation.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let res = Resolved {
        series: "Show".to_string(),
        episode_name: "Confrontation".to_string(),
        synopsis: "A brave synopsis".to_string(),
        season: 1,
        number: 2,
        ..Resolved::default()
    };
    let got = build_activity_resolved(TV_RAW, Some(&run_act), &res);
    assert_eq!(got.name, "Show", "header is the series");
    assert_eq!(got.details, "", "details blanks: the header carries it");
    assert_eq!(
        got.state, "S01E02 \u{b7} Confrontation",
        "state = code and episode title"
    );
    assert_eq!(
        got.state.matches("S01E02").count(),
        1,
        "the code appears once: {:?}",
        got.state
    );
    // Paused keeps the code and episode plus the marker.
    let paused = Action {
        paused: true,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(TV_RAW, Some(&paused), &res);
    assert_eq!(
        got.state, "S01E02 \u{b7} Confrontation \u{b7} Paused",
        "paused marker"
    );
    // Synopsis preset keeps its explicit `-` join.
    let syn_spec = CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: CARD_STATE_SYNOPSIS.to_string(),
    };
    let card = compose_card(TV_RAW, Some(&run_act), &res, false, &syn_spec);
    assert_eq!(
        card.state, "S01E02 - A brave synopsis",
        "synopsis state uses the code"
    );
}

/// The retired `episode` preset still renders the episode title alone for old
/// in-flight specs (it is byte-identical to the default) -- the `S01E02 · Title`
/// code prefix is gone. Without a canonical title it keeps the default line.
#[test]
fn episode_preset_is_title_only() {
    const TV_RAW: &str = "Show.S01E02.Confrontation.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let ep_spec = CardSpec {
        details: "episode".to_string(),
        state: CARD_STATE_DEFAULT.to_string(),
    };
    let card = compose_card(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Confrontation".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
        false,
        &ep_spec,
    );
    assert_eq!(
        card.details, "Confrontation",
        "episode title alone, never the code"
    );
    // No canonical episode title: the default line (the series title) would
    // repeat the header, so it blanks.
    let card = compose_card(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
        false,
        &ep_spec,
    );
    assert_eq!(
        card.details, "",
        "fallback would repeat the header and blanks"
    );
}

/// T1: absolute-only renders `E02`, a number-less special omits the code, and
/// a movie card ignores episode-code data entirely.
#[test]
fn absolute_special_and_movie_code_shapes() {
    const TV_RAW: &str = "Show.S01E02.Confrontation.1080p.mkv - PotPlayer";
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    // Absolute-only: the series title in the header, the absolute code and
    // episode on the state line.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "The Second".to_string(),
            absolute: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "absolute header");
    assert_eq!(got.details, "", "details blanks");
    assert_eq!(got.state, "E02 \u{b7} The Second", "absolute state");
    // Special with no number: the filename tag stands, with the filename's
    // episode name.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            season: 0,
            number: 0,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "special header");
    assert_eq!(got.details, "", "special details blank");
    assert_eq!(
        got.state, "S1 E2 \u{b7} Confrontation",
        "special state keeps the filename tag and episode"
    );
    // Movie branches ignore episode-code data.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "movie details");
    assert_eq!(
        got.state, "2h 46m",
        "movie state ignores the code"
    );
    assert_eq!(got.large_text, "", "movie tooltip empty without a synopsis");
}

/// T4 (red first): the series default State is the synopsis line -- the
/// `SxxExx` code plus the synopsis (the synopsis-preset shape) -- while the
/// header carries the show and line 1 blanks. The code appears once, in the
/// state only. Without a synopsis the line falls to the tag, then the
/// clean title; it never carries the episode title twice.
#[test]
fn series_default_state_carries_code_plus_title() {
    const TV_RAW: &str = "Death.Note.S01E03.Dealings.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let syn = "While the world's criminals fall silent";
    // Series: the header is the show, state is the code and episode title; the
    // synopsis is tooltip-only.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Death Note".to_string(),
            episode_name: "Dealings".to_string(),
            synopsis: syn.to_string(),
            season: 1,
            number: 3,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Death Note", "header");
    assert_eq!(got.details, "", "details blanks");
    assert_eq!(got.state, "S01E03 \u{b7} Dealings", "state");
    // Episode context: the stored note is episode prose, so the tooltip
    // hides it.
    assert_eq!(got.large_text, "", "tooltip hides episode prose");
    let n = java_units(&got.state);
    assert!(
        n <= 128,
        "state length {n} exceeds 128: {:?}",
        got.state
    );
    // No synopsis with a code: the same identity line.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Death Note".to_string(),
            episode_name: "Dealings".to_string(),
            season: 1,
            number: 3,
            ..Resolved::default()
        },
    );
    assert_eq!(got.state, "S01E03 \u{b7} Dealings", "no-synopsis state");
    // No episode title: the code alone, while the header still carries the show.
    let got = build_activity_resolved(
        "Show.S01E03.1080p.mkv - PotPlayer",
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            synopsis: syn.to_string(),
            season: 1,
            number: 3,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "series header");
    assert_eq!(got.details, "", "details blanks");
    assert_eq!(got.state, "S01E03", "code alone");
    // Absolute-only: no fabricated season.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Title".to_string(),
            synopsis: syn.to_string(),
            absolute: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.state, "E02 \u{b7} Title", "absolute code plus episode");
    assert!(
        !got.state.contains("S01") && !got.state.contains("S00"),
        "absolute must never fabricate a season: {:?}",
        got.state
    );
    // The real AniList absolute production shape (season 0, number set to the
    // absolute episode): still `E05`, never `S00E05`.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Title".to_string(),
            synopsis: syn.to_string(),
            season: 0,
            number: 5,
            absolute: 5,
            ..Resolved::default()
        },
    );
    assert_eq!(got.state, "E05 \u{b7} Title", "absolute production shape");
    assert!(
        !got.state.contains("S00"),
        "season 0 must never fabricate a season: {:?}",
        got.state
    );
    // One-season shows keep S01.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Death Note".to_string(),
            episode_name: "Dealings".to_string(),
            synopsis: syn.to_string(),
            season: 1,
            number: 1,
            ..Resolved::default()
        },
    );
    assert_eq!(got.state, "S01E01 \u{b7} Dealings", "one-season keeps S01");
    // Paused keeps the marker and stays within 128.
    let paused = Action {
        paused: true,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        TV_RAW,
        Some(&paused),
        &Resolved {
            series: "Death Note".to_string(),
            episode_name: "Dealings".to_string(),
            synopsis: syn.to_string(),
            season: 1,
            number: 3,
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.state, "S01E03 \u{b7} Dealings \u{b7} Paused",
        "paused marker"
    );
    let n = java_units(&got.state);
    assert!(
        n <= 128,
        "paused state length {n} exceeds 128: {:?}",
        got.state
    );
    // A long synopsis does not touch the body lines; episode-pinned prose
    // never rides the tooltip, so the tooltip stays empty.
    let long_syn = "A retired operative living quietly is pulled back when a ruthless syndicate burns his home. He crosses continents, gathers old allies, and faces a final choice that will decide everything he loves and more.";
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Death Note".to_string(),
            episode_name: "Dealings".to_string(),
            synopsis: long_syn.to_string(),
            season: 1,
            number: 3,
            ..Resolved::default()
        },
    );
    assert_eq!(got.state, "S01E03 \u{b7} Dealings", "long synopsis state");
    assert_eq!(
        got.large_text, "",
        "episode prose never rides, however long: {:?}",
        got.large_text
    );
    // Movie default ignores episode-code data.
    let got = build_activity_resolved(
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            season: 1,
            number: 3,
            episode_name: "Dealings".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "movie details");
    assert_eq!(got.state, "2h 46m", "movie state");
    // Custom presets are unchanged.
    for (preset, want) in [
        (crate::cardspec::CARD_STATE_TAG, "S1 E3"),
        (
            crate::cardspec::CARD_STATE_SYNOPSIS,
            "S01E03 - A brave synopsis",
        ),
    ] {
        let spec = CardSpec {
            details: CARD_DETAILS_DEFAULT.to_string(),
            state: preset.to_string(),
        };
        let card = compose_card(
            TV_RAW,
            Some(&run_act),
            &Resolved {
                series: "Death Note".to_string(),
                episode_name: "Dealings".to_string(),
                synopsis: "A brave synopsis".to_string(),
                season: 1,
                number: 3,
                ..Resolved::default()
            },
            false,
            &spec,
        );
        assert_eq!(card.state, want, "preset {preset} unchanged");
    }
    let spec = CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: crate::cardspec::CARD_STATE_NONE.to_string(),
    };
    let card = compose_card(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Death Note".to_string(),
            episode_name: "Dealings".to_string(),
            season: 1,
            number: 3,
            ..Resolved::default()
        },
        false,
        &spec,
    );
    assert_eq!(card.state, "", "minimal stays empty");
}

/// Owner mapping: movie details is year/genres, never the synopsis (which
/// is tooltip-only).
#[test]
fn movie_t5a_details_is_synopsis_never_title() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            // A movie synopsis describes the work, so it rides the tooltip.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "movie details");
    assert!(
        !got.details.contains("Paul unites the Fremen"),
        "details must never carry the synopsis: {:?}",
        got.details
    );
    assert_eq!(got.large_text, "Paul unites the Fremen", "tooltip");
}

#[test]
fn movie_t5a_state_is_year_genres_runtime() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.state, "2h 46m", "movie state");
    // The brief's shape example.
    let got = build_activity_resolved(
        "Movie.2019.1080p.mkv - PotPlayer",
        Some(&run_act),
        &Resolved {
            series: "Example".to_string(),
            year: 2019,
            is_movie: true,
            genres: "Crime, Drama, Thriller".to_string(),
            length: "2h 3m".to_string(),
            synopsis: "A synopsis".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.state, "2h 3m",
        "brief example shape"
    );
    assert_eq!(got.details, "2019 \u{b7} Crime, Drama, Thriller", "brief example details");
}

#[test]
fn movie_t5a_details_fallbacks_skip_missing_never_title() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    // No synopsis: year is on line 1, genres/runtime on line 2.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "details carry the year");
    assert_eq!(got.state, "2h 46m", "state");
    // No year: the title is the header and line 1 carries the genres.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune: Part Two", "header without a year");
    assert_eq!(got.details, "Sci-Fi", "details carry the genres without a year");
    assert_eq!(got.state, "2h 46m", "state");
    // Bare movie: the header is the title; line 1 blanks, never a duplicate.
    let got = build_activity_resolved(
        "Movie.mkv - PotPlayer",
        Some(&run_act),
        &Resolved {
            series: "Dune".to_string(),
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune", "bare movie header");
    assert_eq!(got.details, "", "bare movie details blank");
    assert_eq!(got.state, "", "bare movie state stays empty");
    // State skips missing parts.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune".to_string(),
            is_movie: true,
            length: "2h 19m".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune", "header");
    assert_eq!(got.details, "", "details blanks");
    assert_eq!(got.state, "2h 19m", "state carries the runtime");
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune".to_string(),
            year: 2024,
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024", "details carry the year");
    assert_eq!(got.state, "", "state stays empty with no runtime");
    // State with nothing: empty, never the title.
    let got = build_activity_resolved(
        "Movie.mkv - PotPlayer",
        Some(&run_act),
        &Resolved {
            series: "Dune".to_string(),
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.state, "", "bare movie state stays empty");
    assert!(
        !got.state.contains("Dune"),
        "bare state must never be the title: {:?}",
        got.state
    );
}

/// The movie default puts identity on line 1 and playback on line 2:
/// `<year> · <genres>` over the runtime alone, never the synopsis (which is
/// tooltip-only) and never the title repeated on line 2.
#[test]
fn movie_default_details_and_state_never_repeat() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    // With a synopsis, the body still carries identity + playback; the
    // synopsis is in the artwork tooltip.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            // A movie synopsis describes the work, so it rides the tooltip.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "movie details");
    assert_eq!(
        got.state,
        "2h 46m",
        "movie state is the runtime"
    );
    assert_eq!(got.large_text, "Paul unites the Fremen", "movie tooltip");
    // No synopsis: the same body lines.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "details keep the title");
    assert_eq!(
        got.state,
        "2h 46m",
        "state stays runtime"
    );
    assert_eq!(got.large_text, "", "no synopsis means no tooltip");
    // No year: the header is the title and line 1 carries the genres.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune: Part Two", "header without a year");
    assert_eq!(got.details, "Sci-Fi", "details carry the genres without a year");
    assert_eq!(got.state, "2h 46m", "state");
}

#[test]
fn movie_t5a_tooltip_empty_tv_unchanged() {
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Some Film".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            // A movie synopsis describes the work.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.large_text, "Paul unites the Fremen",
        "movie tooltip carries the synopsis snippet"
    );
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Some Film".to_string(),
            is_movie: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.large_text, "", "movie tooltip stays empty without synopsis");
    // TV carries the same snippet.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Canonical Series".to_string(),
            episode_name: "Canonical Episode".to_string(),
            synopsis: "A brave synopsis".to_string(),
            ..Resolved::default()
        },
    );
    // TV carries episode-pinned prose here, so the tooltip hides it: only a
    // work-level synopsis rides.
    assert_eq!(got.large_text, "", "TV tooltip hides episode prose");
}

#[test]
fn movie_t5a_no_title_in_details_or_state() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            ..Resolved::default()
        },
    );
    // Owner mapping: the header carries the title; line 1 keeps year/genres.
    assert_eq!(
        got.details, "2024 \u{b7} Sci-Fi",
        "year and genres must lead details"
    );
    assert!(
        !got.state.contains("Dune"),
        "title must not leak into state: {:?}",
        got.state
    );
    // The header is the media title, never the app name.
    assert_eq!(got.name, "Dune: Part Two", "header is the movie title");
}

#[test]
fn movie_t5a_details_128_word_aware() {
    const MOVIE_RAW: &str = "Movie.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let syn = format!("{}tail", "word ".repeat(40));
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Some Film".to_string(),
            is_movie: true,
            synopsis: syn.clone(),
            // Work-level: the tooltip mechanics are the point here.
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    let want = crate::synopsis_snippet(&syn);
    assert_eq!(got.large_text, want, "tooltip snippet trims at 128");
    assert!(
        got.large_text.ends_with('\u{2026}'),
        "truncated tooltip must signal the cut: {:?}",
        got.large_text
    );
    assert!(
        !got.large_text.trim_end_matches('\u{2026}').ends_with("wo"),
        "tooltip ended mid-word: {:?}",
        got.large_text
    );
    let n = java_units(&got.large_text);
    assert!(n <= 128, "large_text length {n} exceeds 128: {:?}", got.large_text);
    assert_eq!(got.name, "Some Film", "header stays the title");
    assert_eq!(got.details, "", "details blanks: it would repeat the header");
    // Spaceless run still signals within budget.
    let nospace = "s".repeat(200);
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Some Film".to_string(),
            is_movie: true,
            synopsis: nospace.clone(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.large_text,
        format!("{}\u{2026}", "s".repeat(127)),
        "spaceless tooltip cut"
    );
}

#[test]
fn movie_t5a_paused_keeps_marker_and_budgets() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let paused = Action {
        paused: true,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&paused),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites the Fremen".to_string(),
            ..Resolved::default()
        },
    );
    assert!(
        got.state.ends_with(PAUSED_MARKER),
        "paused movie state keeps the marker: {:?}",
        got.state
    );
    assert!(
        got.state.starts_with("2h 46m"),
        "paused state keeps the runtime head: {:?}",
        got.state
    );
    assert!(
        !got.state.contains("2024") && !got.state.contains("Dune"),
        "paused state must not carry the title/year: {:?}",
        got.state
    );
    let n = java_units(&got.state);
    assert!(n <= 128, "paused state length {n} exceeds 128: {:?}", got.state);
    let n = java_units(&got.details);
    assert!(
        n <= 128,
        "paused details length {n} exceeds 128: {:?}",
        got.details
    );
}

#[test]
fn movie_t5a_tv_t4_unchanged() {
    const TV_RAW: &str = "Show.S01E02.Confrontation.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Confrontation".to_string(),
            synopsis: "A brave synopsis".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "TV header is the series title");
    assert_eq!(
        got.details, "",
        "TV details blanks: the header carries the series title"
    );
    assert_eq!(got.state, "S01E02 \u{b7} Confrontation", "TV state is code · episode");
    // Episode-pinned prose never rides the tooltip.
    assert_eq!(got.large_text, "", "TV tooltip hides episode prose");
    // Absolute-only still renders E02, never a fake season; one-season keeps S01.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Title".to_string(),
            synopsis: "While the world's criminals fall silent".to_string(),
            absolute: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.state, "E02 \u{b7} Title",
        "absolute E02, no fake season"
    );
}

/// Owner regression (2026-10-28): the Discord header must read exactly
/// `Watching <media title>` -- the movie title for a film, the series title for
/// an episode -- and never `Watching Potflix`. The episode keeps
/// `SxxEyy · Episode Title` as its secondary state, and no body line repeats
/// the header (the composer's existing no-repeat invariant).
#[test]
fn activity_header_is_the_media_title_never_the_app_name() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    const EP_RAW: &str = "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer";
    let run = Action {
        running: true,
        has_start: true,
        start: 1000,
        ct: 1000,
        ..Action::default()
    };
    let movie = compose_card(
        MOVIE_RAW,
        Some(&run),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi, Adventure".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        },
        true,
        &crate::cardspec::default_card_spec(),
    );
    assert_eq!(
        crate::header_line(&movie),
        "Watching Dune: Part Two",
        "the movie header is the movie title"
    );
    assert_eq!(
        movie.name,
        "Dune: Part Two",
        "the header name is the movie title, not the app name"
    );
    assert_ne!(
        movie.name,
        player::window::APP_NAME,
        "the header name is never the application name"
    );

    let episode = compose_card(
        EP_RAW,
        Some(&run),
        &Resolved {
            series: "Breaking Bad".to_string(),
            episode_name: "Problem Dog".to_string(),
            season: 4,
            number: 7,
            ..Resolved::default()
        },
        false,
        &crate::cardspec::default_card_spec(),
    );
    assert_eq!(
        crate::header_line(&episode),
        "Watching Breaking Bad",
        "the episode header is the series title"
    );
    assert_ne!(
        episode.name,
        player::window::APP_NAME,
        "the header name is never the application name"
    );
    assert!(
        !crate::header_line(&episode).contains("Potflix"),
        "the header must never read Watching Potflix: {:?}",
        crate::header_line(&episode)
    );
    assert_eq!(
        episode.state,
        "S04E07 \u{b7} Problem Dog",
        "the secondary state keeps the episode code and title"
    );
    assert_ne!(
        episode.details, episode.name,
        "no body line repeats the header"
    );
}

/// One best card, movie leg: details is `<year> · <genres>` and carries no
/// title; state is the runtime alone.
#[test]
fn movie_card_is_year_and_genres_over_runtime() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi, Adventure".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Dune: Part Two", "header is the movie title");
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi, Adventure", "details");
    assert!(
        !got.details.contains("Dune"),
        "the title must never repeat on a body line: {:?}",
        got.details
    );
    assert_eq!(got.state, "2h 46m", "state is the runtime alone");
    assert!(
        !got.state.contains("Sci-Fi") && !got.state.contains("Dune"),
        "genres and title live on line 1, never line 2: {:?}",
        got.state
    );
}

/// Movie genre budget: over 128 units whole trailing genres drop first, never
/// an ellipsis -- a cut genre list reads as the complete list.
#[test]
fn movie_details_drops_whole_genres_over_budget() {
    const MOVIE_RAW: &str = "Movie.2019.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let all = [
        "Crime", "Drama", "Thriller", "Mystery", "Romance", "Comedy", "Horror", "Western",
        "Fantasy", "Science Fiction", "Documentary", "Animation", "Adventure", "Musical",
    ];
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Example".to_string(),
            year: 2019,
            is_movie: true,
            genres: all.join(", "),
            length: "2h 3m".to_string(),
            ..Resolved::default()
        },
    );
    assert!(
        got.details.starts_with("2019 \u{b7} Crime"),
        "the year leads and the first genres survive: {:?}",
        got.details
    );
    assert!(
        !got.details.contains('\u{2026}'),
        "genres drop whole, never an ellipsis: {:?}",
        got.details
    );
    let n = java_units(&got.details);
    assert!(
        n <= 128,
        "details length {n} exceeds 128: {:?}",
        got.details
    );
    let mut parts = got.details.split(", ");
    let first = parts.next().expect("year plus a genre");
    assert_eq!(first, "2019 \u{b7} Crime", "year and first genre: {first:?}");
    for part in parts {
        assert!(
            all.contains(&part),
            "every kept genre is whole: {part:?} in {:?}",
            got.details
        );
    }
}

/// One best card, series leg: details stays EMPTY (the header carries the
/// series); over budget the episode title drops whole before the code.
#[test]
fn series_card_keeps_code_and_drops_title_over_budget() {
    const TV_RAW: &str = "Show.S01E02.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let long_title = "e".repeat(200);
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: long_title,
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "header is the series title");
    assert_eq!(got.details, "", "details stays empty");
    assert_eq!(
        got.state, "S01E02",
        "the title drops whole before the code: {:?}",
        got.state
    );
    assert!(
        !got.state.contains('\u{2026}'),
        "a dropped title leaves no ellipsis: {:?}",
        got.state
    );
}

/// Paused is the same card: the `· Paused` marker rides the last populated
/// body line, and no timestamps ride at all (a start-only anchor renders as
/// a count-up that reads as playing).
#[test]
fn paused_marks_last_populated_line_without_timestamps() {
    let paused = Action {
        paused: true,
        ct: 1000,
        ..Action::default()
    };
    // Movie with a runtime: the marker rides the state; line 1 is untouched.
    let got = build_activity_resolved(
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        Some(&paused),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "2024 \u{b7} Sci-Fi", "line 1 untouched");
    assert_eq!(got.state, "2h 46m \u{b7} Paused", "marker on the state");
    // Movie with no runtime: the marker rides line 1, the last populated
    // body line; line 2 stays empty.
    let got = build_activity_resolved(
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        Some(&paused),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.details, "2024 \u{b7} Sci-Fi \u{b7} Paused",
        "marker on line 1"
    );
    assert_eq!(got.state, "", "line 2 stays empty");
    // Series: the marker rides the episode line; line 1 stays empty.
    let got = build_activity_resolved(
        "Show.S01E02.Confrontation.1080p.mkv - PotPlayer",
        Some(&paused),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Confrontation".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "", "line 1 stays empty");
    assert_eq!(
        got.state, "S01E02 \u{b7} Confrontation \u{b7} Paused",
        "marker on the episode line"
    );
    // No timestamps on any paused card.
    for raw in [
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        "Show.S01E02.Confrontation.1080p.mkv - PotPlayer",
    ] {
        let got = build_activity_resolved(raw, Some(&paused), &Resolved::default());
        assert!(
            !got.has_start && got.start == 0 && got.end == 0,
            "paused {raw:?} must carry no timestamps: {got:?}"
        );
    }
}

/// UNKNOWN: the cleaned title plus the parsed code if any; no art, no prose,
/// no timestamps.
#[test]
fn unknown_card_is_title_and_code_only() {
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let got = build_activity_resolved(
        "Show.S04E07.Some.Episode.1080p.mkv - PotPlayer",
        Some(&run_act),
        &Resolved {
            unknown: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.name, "Show", "header is the cleaned title");
    assert_eq!(got.details, "", "no details on an unresolved card");
    assert_eq!(got.state, "S4 E7", "the parsed code, nothing else");
    assert!(
        !got.state.contains("Some Episode"),
        "no prose on an unresolved card: {:?}",
        got.state
    );
    assert_eq!(got.large_image, "", "no art");
    assert_eq!(got.large_text, "", "no prose in the tooltip");
    assert!(
        !got.has_start && got.start == 0 && got.end == 0,
        "no timestamps: {:?}",
        (got.has_start, got.start, got.end)
    );
    // No tag at all: no state either, still no timestamps.
    let got = build_activity_resolved(
        "Some.Random.Video.1080p.mkv - PotPlayer",
        Some(&run_act),
        &Resolved {
            unknown: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.details, "", "no details");
    assert_eq!(got.state, "", "no code, no state");
    assert!(
        !got.has_start && got.start == 0 && got.end == 0,
        "no timestamps: {:?}",
        (got.has_start, got.start, got.end)
    );
    assert!(!got.name.is_empty(), "the cleaned title survives");
}

/// The tooltip rides only work-level prose: episode-pinned notes never ride,
/// however short; the 128-unit cap still applies to work notes.
#[test]
fn tooltip_only_rides_work_level_synopsis() {
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    const TV_RAW: &str = "Show.S01E02.Confrontation.1080p.mkv - PotPlayer";
    let run_act = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    // A work synopsis rides whole when short.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            synopsis: "Paul unites the Fremen".to_string(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.large_text, "Paul unites the Fremen",
        "work prose rides"
    );
    // The same note without provenance stays home.
    let got = build_activity_resolved(
        MOVIE_RAW,
        Some(&run_act),
        &Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            synopsis: "Paul unites the Fremen".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.large_text, "",
        "unmarked prose never rides: {:?}",
        got.large_text
    );
    // Episode-pinned prose never rides, however short.
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            episode_name: "Confrontation".to_string(),
            synopsis: "A very short pin.".to_string(),
            season: 1,
            number: 2,
            ..Resolved::default()
        },
    );
    assert_eq!(
        got.large_text, "",
        "episode prose never rides: {:?}",
        got.large_text
    );
    // A work-level series note rides, capped at 128 with the signal.
    let long = format!("{}tail", "word ".repeat(40));
    let got = build_activity_resolved(
        TV_RAW,
        Some(&run_act),
        &Resolved {
            series: "Show".to_string(),
            synopsis: long.clone(),
            synopsis_is_work: true,
            ..Resolved::default()
        },
    );
    assert_eq!(got.large_text, crate::synopsis_snippet(&long), "capped");
    assert!(
        got.large_text.ends_with('\u{2026}'),
        "the cut is signalled: {:?}",
        got.large_text
    );
    let n = java_units(&got.large_text);
    assert!(n <= 128, "tooltip length {n} exceeds 128");
}
