
use crate::cardspec::{
    CARD_DETAILS_DEFAULT, CARD_DETAILS_EPISODE, CARD_DETAILS_SERIES, CARD_DETAILS_TITLE,
    CARD_STATE_DEFAULT, CARD_STATE_NONE, CARD_STATE_SYNOPSIS, CARD_STATE_TAG, CardPreset, CardSpec,
    card_presets, default_card_spec, is_idle_title,
};
use crate::{
    Action, PAUSED_MARKER, Resolved, STATE_SYNOPSIS_BUDGET, build_activity_resolved, compose_card,
};

struct CardSpecCase {
    name: &'static str,
    raw: String,
    act: Option<Action>,
    res: Resolved,
}

fn run_action(start: i64, ct: i64) -> Option<Action> {
    Some(Action {
        running: true,
        has_start: true,
        start,
        ct,
        ..Action::default()
    })
}

fn paused_action() -> Option<Action> {
    Some(Action {
        paused: true,
        ct: 1000,
        ..Action::default()
    })
}

fn card_spec_cases() -> Vec<CardSpecCase> {
    const TV_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let resolved_series = Resolved {
        series: "Canonical Series".to_string(),
        episode_name: "Canonical Episode".to_string(),
        synopsis: "A brave synopsis".to_string(),
        ..Resolved::default()
    };
    vec![
        CardSpecCase {
            name: "idle-empty",
            raw: String::new(),
            act: None,
            res: Resolved::default(),
        },
        CardSpecCase {
            name: "idle-app",
            raw: "PotPlayer".to_string(),
            act: Some(Action::default()),
            res: Resolved::default(),
        },
        CardSpecCase {
            name: "series-run",
            raw: TV_RAW.to_string(),
            act: run_action(12345, 1000),
            res: Resolved::default(),
        },
        CardSpecCase {
            name: "series-tagonly",
            raw: "Show.S01E02.1080p.mkv - PotPlayer".to_string(),
            act: run_action(12345, 1000),
            res: Resolved::default(),
        },
        CardSpecCase {
            name: "series-paused",
            raw: TV_RAW.to_string(),
            act: paused_action(),
            res: Resolved::default(),
        },
        CardSpecCase {
            name: "series-resolved",
            raw: TV_RAW.to_string(),
            act: run_action(12345, 1000),
            res: resolved_series.clone(),
        },
        CardSpecCase {
            name: "series-resolved-paused",
            raw: TV_RAW.to_string(),
            act: paused_action(),
            res: resolved_series,
        },
        CardSpecCase {
            name: "series-synonly",
            raw: "Movie.mkv - PotPlayer".to_string(),
            act: run_action(1, 2),
            res: Resolved {
                synopsis: "Just a synopsis".to_string(),
                ..Resolved::default()
            },
        },
        CardSpecCase {
            name: "movie-full",
            raw: "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
            act: run_action(12345, 1000),
            res: Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: "Paul unites the Fremen".to_string(),
                ..Resolved::default()
            },
        },
        CardSpecCase {
            name: "movie-paused",
            raw: "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
            act: paused_action(),
            res: Resolved {
                series: "Dune".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: "Paul unites the Fremen".to_string(),
                ..Resolved::default()
            },
        },
        CardSpecCase {
            name: "movie-noyear",
            raw: "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
            act: run_action(1, 2),
            res: Resolved {
                series: "Dune: Part Two".to_string(),
                is_movie: true,
                ..Resolved::default()
            },
        },
        CardSpecCase {
            name: "artwork-buttons",
            raw: TV_RAW.to_string(),
            act: run_action(1, 2),
            res: Resolved {
                series: "S".to_string(),
                artwork: "https://example.com/poster.jpg".to_string(),
                buttons: vec![crate::Button {
                    label: "Trailer".to_string(),
                    url: "https://example.com/trailer".to_string(),
                }],
                ..Resolved::default()
            },
        },
        CardSpecCase {
            name: "long",
            raw: format!(
                "{}.S01E02.{}.mkv - PotPlayer",
                "L".repeat(200),
                "E".repeat(200)
            ),
            act: run_action(1, 2),
            res: Resolved {
                series: "y".repeat(200),
                episode_name: "z".repeat(200),
                synopsis: format!("{}tail", "word ".repeat(40)),
                ..Resolved::default()
            },
        },
        CardSpecCase {
            name: "stop",
            raw: TV_RAW.to_string(),
            act: Some(Action {
                ct: 5,
                ..Action::default()
            }),
            res: Resolved::default(),
        },
        CardSpecCase {
            name: "nil-action",
            raw: TV_RAW.to_string(),
            act: None,
            res: Resolved::default(),
        },
    ]
}

/// The safety gate for the Card view: for a table of inputs,
/// `default_card_spec().render` must equal what `build_activity_resolved`
/// produces today.
///
/// NOTE: this is a change-detector only (render calls `build_activity_resolved`
/// for the default case), so preview-is-the-payload is proven separately by the
/// card view's own wire test, which captures the real payload and asserts the
/// preview equals it.
#[test]
fn card_spec_default_equivalence() {
    let def = default_card_spec();
    for c in card_spec_cases() {
        let want = build_activity_resolved(&c.raw, c.act.as_ref(), &c.res);
        let (got_d, got_s) = def.render(&c.raw, c.act.as_ref(), &c.res);
        assert_eq!(
            (got_d.as_str(), got_s.as_str()),
            (want.details.as_str(), want.state.as_str()),
            "{}: render mismatch",
            c.name
        );
    }
}

/// Breaks the tautology: instead of comparing render to `build_activity_resolved`
/// (which render calls for the default case), it asserts hardcoded golden lines
/// computed independently. If render drifts from the shipped output, this fails
/// even though the change-detector above would still pass.
#[test]
fn card_spec_default_golden_independent() {
    let def = default_card_spec();
    let (d, s) = def.render(
        "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
        run_action(12345, 1000).as_ref(),
        &Resolved::default(),
    );
    assert!(
        d.is_empty() && s == "S1 E2 \u{b7} Pilot",
        "series render = ({d:?}, {s:?}), want (\"\", \"S1 E2 \u{b7} Pilot\")"
    );
    let (d, s) = def.render(
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        run_action(12345, 1000).as_ref(),
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
        d == "2024 \u{b7} Sci-Fi" && s == "2h 46m",
        "movie render = ({d:?}, {s:?}), want goldens"
    );
    let (d, s) = def.render("", Some(&Action::default()), &Resolved::default());
    assert!(
        d == "Browsing" && s == "Idle",
        "idle render = ({d:?}, {s:?}), want (\"Browsing\", \"Idle\")"
    );
}

/// Proves no migration is needed: the zero value renders exactly the shipped
/// default on every input.
#[test]
fn card_spec_zero_falls_back() {
    let zero = CardSpec::default();
    let def = default_card_spec();
    assert!(
        zero.is_default(),
        "zero CardSpec is_default = false, want true"
    );
    assert_eq!(
        zero.normalized(),
        def,
        "zero normalized = {:?}, want {:?}",
        zero.normalized(),
        def
    );
    for c in card_spec_cases() {
        let (zd, zs) = zero.render(&c.raw, c.act.as_ref(), &c.res);
        let (dd, ds) = def.render(&c.raw, c.act.as_ref(), &c.res);
        assert_eq!(
            (zd.as_str(), zs.as_str()),
            (dd.as_str(), ds.as_str()),
            "{}: zero render mismatch",
            c.name
        );
    }
}

/// Proves a stored typo can never reach the card.
#[test]
fn card_spec_unknown_falls_back() {
    let weird = CardSpec {
        details: "fancy-template".to_string(),
        state: "\u{0}".to_string(),
    };
    let def = default_card_spec();
    for c in card_spec_cases() {
        let (gd, gs) = weird.render(&c.raw, c.act.as_ref(), &c.res);
        let (dd, ds) = def.render(&c.raw, c.act.as_ref(), &c.res);
        assert_eq!(
            (gd.as_str(), gs.as_str()),
            (dd.as_str(), ds.as_str()),
            "{}: unknown render mismatch",
            c.name
        );
    }
}

/// Proves the stored preset sets stay small, fixed, and default-first
/// (presets, not a template language). The stored Details value is Default
/// only -- the `title`/`series` presets repeated the activity header (T5-B)
/// and `episode` rendered byte-identical to Default for both kinds, so all
/// three are retired (stored ids migrate; see
/// `retired_details_ids_migrate_to_default`) and the Card view offers no
/// Details row at all. States are unchanged.
#[test]
fn card_presets_small() {
    let (d, s) = card_presets();
    assert!(
        d.len() == 1 && s.len() == 4,
        "presets = {} details + {} states, want 1+4",
        d.len(),
        s.len()
    );
    assert!(
        d[0].id == CARD_DETAILS_DEFAULT && s[0].id == CARD_STATE_DEFAULT,
        "first presets = {:?}/{:?}, want both {CARD_DETAILS_DEFAULT:?}",
        d[0].id,
        s[0].id
    );
    for p in d.iter().chain(s.iter()) {
        assert!(
            !p.id.is_empty() && !p.label.is_empty(),
            "preset {p:?} has an empty id or label"
        );
    }
}

/// The retired Details presets are gone from the stored set: it carries only
/// Default (the per-kind default line: episode title for TV, synopsis for
/// movies); `title`/`series` would repeat the activity header and `episode`
/// rendered byte-identical to Default. The Card view offers no Details row.
/// States keep all four presets.
#[test]
fn card_presets_drop_repeating_title_series_and_episode() {
    let (dets, states) = card_presets();
    assert_eq!(
        dets.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        [CARD_DETAILS_DEFAULT],
        "stored details presets"
    );
    assert!(
        dets.iter().all(|p| p.id != CARD_DETAILS_TITLE
            && p.id != CARD_DETAILS_SERIES
            && p.id != CARD_DETAILS_EPISODE),
        "a retired id is still offered: {dets:?}"
    );
    assert_eq!(
        states.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        [
            CARD_STATE_DEFAULT,
            CARD_STATE_TAG,
            CARD_STATE_SYNOPSIS,
            CARD_STATE_NONE
        ],
        "offered state presets"
    );
}

/// Proves every preset line stays within the 128-unit Discord cap and the paused
/// marker survives where expected.
#[test]
fn card_spec_presets_capped() {
    let (dets, states) = card_presets();
    for c in card_spec_cases() {
        for d in &dets {
            for st in &states {
                let spec = CardSpec {
                    details: d.id.clone(),
                    state: st.id.clone(),
                };
                let (gd, gs) = spec.render(&c.raw, c.act.as_ref(), &c.res);
                let n = java_units(&gd);
                assert!(
                    n <= 128,
                    "{} {}/{}: details {n} units > 128: {gd:?}",
                    c.name,
                    d.id,
                    st.id
                );
                let n = java_units(&gs);
                assert!(
                    n <= 128,
                    "{} {}/{}: state {n} units > 128: {gs:?}",
                    c.name,
                    d.id,
                    st.id
                );
                if st.id == CARD_STATE_NONE && !is_idle_title(&c.raw) && !gs.is_empty() {
                    panic!("{}: Empty state = {gs:?}, want empty", c.name);
                }
                if c.act.as_ref().is_some_and(|a| a.paused)
                    && !is_idle_title(&c.raw)
                    && st.id != CARD_STATE_NONE
                    && gs != "Paused"
                    && !gs.ends_with(PAUSED_MARKER)
                {
                    panic!(
                        "{} {}/{}: paused state = {gs:?}, want suffix {PAUSED_MARKER:?}",
                        c.name, d.id, st.id
                    );
                }
            }
        }
    }
}

fn java_units(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Pins kind-driven line selection through the active preset: the movie branch
/// (`is_movie`) appends the year on the series preset and keeps the base line on
/// the episode preset (movies have no episodes); the series branch serves
/// episodes; anime (`is_movie=false`, even with a year set) resolves through the
/// series branch, never the movie one.
#[test]
fn card_spec_kind_adapt() {
    let run = Action {
        running: true,
        has_start: true,
        start: 1,
        ct: 2,
        ..Action::default()
    };
    let m_raw = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let movie = Resolved {
        series: "Dune: Part Two".to_string(),
        year: 2024,
        is_movie: true,
        genres: "Sci-Fi".to_string(),
        length: "2h 46m".to_string(),
        synopsis: "Paul unites the Fremen".to_string(),
        ..Resolved::default()
    };
    let s_raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    let series = Resolved {
        series: "Canonical Series".to_string(),
        episode_name: "Canonical Episode".to_string(),
        synopsis: "A brave synopsis".to_string(),
        ..Resolved::default()
    };
    let a_raw = "Frieren.S01E01.The.Heros.Rest.1080p.mkv - PotPlayer";
    let anime = Resolved {
        series: "Frieren".to_string(),
        episode_name: "The Hero's Rest".to_string(),
        synopsis: "An elf mourns".to_string(),
        year: 2023,
        ..Resolved::default()
    };
    const MID: &str = " \u{b7} ";
    const DASH: &str = " - ";
    struct Case {
        name: &'static str,
        raw: &'static str,
        res: Resolved,
        spec: CardSpec,
        want_d: &'static str,
        want_s: String,
    }
    let mid = |a: &str, b: &str| format!("{a}{MID}{b}");
    let dash = |a: &str, b: &str| format!("{a}{DASH}{b}");
    let cases = vec![
        Case {
            name: "movie default",
            raw: m_raw,
            res: movie.clone(),
            spec: default_card_spec(),
            want_d: "2024 \u{b7} Sci-Fi",
            want_s: "2h 46m".to_string(),
        },
        Case {
            name: "movie episode keeps base",
            raw: m_raw,
            res: movie.clone(),
            spec: CardSpec {
                details: CARD_DETAILS_EPISODE.to_string(),
                state: CARD_STATE_DEFAULT.to_string(),
            },
            want_d: "2024 \u{b7} Sci-Fi",
            want_s: "2h 46m".to_string(),
        },
        Case {
            name: "movie series appends year",
            raw: m_raw,
            res: movie.clone(),
            spec: CardSpec {
                details: CARD_DETAILS_SERIES.to_string(),
                state: CARD_STATE_DEFAULT.to_string(),
            },
            want_d: "Dune: Part Two (2024)",
            want_s: "2h 46m".to_string(),
        },
        Case {
            name: "movie title drops year",
            raw: m_raw,
            res: movie,
            spec: CardSpec {
                details: CARD_DETAILS_TITLE.to_string(),
                state: CARD_STATE_TAG.to_string(),
            },
            want_d: "",
            want_s: mid("Sci-Fi", "2h 46m"),
        },
        Case {
            name: "movie synopsis",
            raw: m_raw,
            res: Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: "Paul unites the Fremen".to_string(),
                ..Resolved::default()
            },
            spec: CardSpec {
                details: CARD_DETAILS_DEFAULT.to_string(),
                state: CARD_STATE_SYNOPSIS.to_string(),
            },
            want_d: "2024 \u{b7} Sci-Fi",
            want_s: mid("Sci-Fi", "2h 46m") + MID + "Paul unites the Fremen",
        },
        Case {
            name: "series default",
            raw: s_raw,
            res: series.clone(),
            spec: default_card_spec(),
            want_d: "",
            want_s: mid("S1 E2", "Canonical Episode"),
        },
        Case {
            name: "series episode",
            raw: s_raw,
            res: series.clone(),
            spec: CardSpec {
                details: CARD_DETAILS_EPISODE.to_string(),
                state: CARD_STATE_TAG.to_string(),
            },
            want_d: "Canonical Episode",
            want_s: "S1 E2".to_string(),
        },
        Case {
            name: "series series keeps title",
            raw: s_raw,
            res: series,
            spec: CardSpec {
                details: CARD_DETAILS_SERIES.to_string(),
                state: CARD_STATE_SYNOPSIS.to_string(),
            },
            want_d: "",
            want_s: dash("S1 E2", "A brave synopsis"),
        },
        Case {
            name: "anime default is series branch",
            raw: a_raw,
            res: anime.clone(),
            spec: default_card_spec(),
            want_d: "",
            want_s: mid("S1 E1", "The Hero's Rest"),
        },
        Case {
            name: "anime series ignores year",
            raw: a_raw,
            res: anime.clone(),
            spec: CardSpec {
                details: CARD_DETAILS_SERIES.to_string(),
                state: CARD_STATE_TAG.to_string(),
            },
            want_d: "",
            want_s: "S1 E1".to_string(),
        },
        Case {
            name: "anime title",
            raw: a_raw,
            res: anime,
            spec: CardSpec {
                details: CARD_DETAILS_TITLE.to_string(),
                state: CARD_STATE_SYNOPSIS.to_string(),
            },
            want_d: "",
            want_s: dash("S1 E1", "An elf mourns"),
        },
    ];
    for c in &cases {
        let (d, s) = c.spec.render(c.raw, Some(&run), &c.res);
        assert!(
            d == c.want_d && s == c.want_s,
            "{}: render = ({d:?}, {s:?}), want ({:?}, {:?})",
            c.name,
            c.want_d,
            c.want_s
        );
    }
}

/// Pins the fill-ladder contract: a series state line blanks under `none`
/// (Minimal) or when its final rung would repeat the header (the clean-title
/// fallback equal to the header blanks to a deliberate one-line card,
/// `"Paused"` when paused); a details line stays empty only when no rung may
/// carry anything (series: no episode title and no filename episode name;
/// movie: no synopsis/year/genres/length) -- never by repeating the header.
/// `none` always hides, even with content and even when paused. The paused
/// marker path is preserved: paused-with-content keeps the suffix, paused
/// with nothing still reads "Paused" - except none, which hides even when
/// paused.
#[test]
fn card_spec_empty_hide() {
    let run = Action {
        running: true,
        has_start: true,
        start: 1,
        ct: 2,
        ..Action::default()
    };
    let paused = Action {
        paused: true,
        ct: 1000,
        ..Action::default()
    };
    let m_raw = "Movie.mkv - PotPlayer";
    let bare_movie = Resolved {
        series: "Dune".to_string(),
        is_movie: true,
        ..Resolved::default()
    };
    let s_raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    struct Case {
        name: &'static str,
        raw: &'static str,
        act: Action,
        res: Resolved,
        spec: CardSpec,
        want_d: &'static str,
        want_s: &'static str,
    }
    let spec = |details: &str, state: &str| CardSpec {
        details: details.to_string(),
        state: state.to_string(),
    };
    let cases = vec![
        Case {
            name: "movie default details stays empty, tag falls to clean title",
            raw: m_raw,
            act: run.clone(),
            res: bare_movie.clone(),
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_TAG),
            want_d: "",
            want_s: "Movie",
        },
        Case {
            name: "movie default details stays empty, synopsis falls to clean title",
            raw: m_raw,
            act: run.clone(),
            res: bare_movie.clone(),
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_SYNOPSIS),
            want_d: "",
            want_s: "Movie",
        },
        Case {
            name: "movie none hides",
            raw: m_raw,
            act: run.clone(),
            res: bare_movie.clone(),
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_NONE),
            want_d: "",
            want_s: "",
        },
        Case {
            name: "series preset falls back to base",
            raw: s_raw,
            act: run.clone(),
            res: Resolved::default(),
            spec: spec(CARD_DETAILS_SERIES, CARD_STATE_DEFAULT),
            want_d: "",
            want_s: "S1 E2 \u{b7} Pilot",
        },
        Case {
            name: "series tag blanks the header-equal clean fallback",
            raw: m_raw,
            act: run.clone(),
            res: Resolved::default(),
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_TAG),
            want_d: "",
            want_s: "",
        },
        Case {
            name: "synopsis preset blanks the header-equal clean fallback",
            raw: m_raw,
            act: run.clone(),
            res: Resolved::default(),
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_SYNOPSIS),
            want_d: "",
            want_s: "",
        },
        Case {
            name: "none hides with content",
            raw: s_raw,
            act: run.clone(),
            res: Resolved {
                series: "S".to_string(),
                episode_name: "E".to_string(),
                synopsis: "Syn".to_string(),
                ..Resolved::default()
            },
            spec: spec(CARD_DETAILS_SERIES, CARD_STATE_NONE),
            want_d: "",
            want_s: "",
        },
        Case {
            name: "paused tag with no tag reads clean title paused",
            raw: m_raw,
            act: paused.clone(),
            res: bare_movie.clone(),
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_TAG),
            want_d: "",
            want_s: "Movie \u{b7} Paused",
        },
        Case {
            name: "paused synopsis with none reads clean title paused",
            raw: m_raw,
            act: paused.clone(),
            res: bare_movie.clone(),
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_SYNOPSIS),
            want_d: "",
            want_s: "Movie \u{b7} Paused",
        },
        Case {
            name: "paused none still hides",
            raw: m_raw,
            act: paused,
            res: bare_movie,
            spec: spec(CARD_DETAILS_DEFAULT, CARD_STATE_NONE),
            want_d: "",
            want_s: "",
        },
    ];
    for c in &cases {
        let (d, s) = c.spec.render(c.raw, Some(&c.act), &c.res);
        assert!(
            d == c.want_d && s == c.want_s,
            "{}: render = ({d:?}, {s:?}), want ({:?}, {:?})",
            c.name,
            c.want_d,
            c.want_s
        );
    }
    let (dets, states) = card_presets();
    for c in card_spec_cases() {
        if is_idle_title(&c.raw) {
            continue;
        }
        for d in &dets {
            for st in &states {
                let (gd, _) = CardSpec {
                    details: d.id.clone(),
                    state: st.id.clone(),
                }
                .render(&c.raw, c.act.as_ref(), &c.res);
                assert_details_from_fixture(&c, &d.id, &gd);
            }
        }
        let (_, gs) = default_card_spec().render(&c.raw, c.act.as_ref(), &c.res);
        assert_state_from_fixture(&c, &gs);
    }
}

/// Asserts one case's details line against an expectation derived from
/// `res` alone -- the resolved title/year/episode fields -- with no call into
/// the production composer, so the check cannot pass on the composer's own
/// (possibly empty) output.
fn assert_details_from_fixture(c: &CardSpecCase, preset: &str, got: &str) {
    let r = &c.res;
    if preset == CARD_DETAILS_TITLE {
        assert_eq!(got, "", "{} {preset}: title preset must blank", c.name);
        return;
    }
    let episode_preset = !r.is_movie && preset == CARD_DETAILS_EPISODE;
    let due = if r.is_movie {
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
                _ => format!("{year} \u{b7} {}", genres[..keep].join(", ")),
            };
            if crate::utf16_units(&line) <= crate::DETAILS_BUDGET || keep == 0 {
                break line;
            }
            keep -= 1;
        }
    } else if episode_preset && !r.episode_name.is_empty() {
        r.episode_name.clone()
    } else {
        r.series.clone()
    };
    if due.is_empty() {
        assert_eq!(got, "", "{} {preset}: no fixture value must blank", c.name);
        return;
    }
    if crate::utf16_units(&due) > crate::DETAILS_BUDGET {
        if episode_preset {
            assert!(
                !got.is_empty(),
                "{} {preset}: details blank, want a cut of {due:?}",
                c.name
            );
            assert!(
                crate::utf16_units(got) <= crate::DETAILS_BUDGET,
                "{} {preset}: details over budget",
                c.name
            );
            let core = got.strip_suffix('\u{2026}').unwrap_or(got);
            assert!(
                due.starts_with(core),
                "{} {preset}: {got:?} is not a cut of {due:?}",
                c.name
            );
        } else {
            assert_eq!(
                got, "",
                "{} {preset}: an over-budget title cut repeats the header and blanks",
                c.name
            );
        }
        return;
    }
    let want = if episode_preset || r.is_movie {
        due.as_str()
    } else {
        ""
    };
    assert_eq!(
        got, want,
        "{} {preset}: details must carry the fixture's own line",
        c.name
    );
}

/// Asserts the default state line from `res` alone: a movie carries
/// `<Genres> · <Runtime>` (plus the paused marker), a series that has an
/// episode title carries `code · Episode`. A blank where those fields supply
/// a value fails.
fn assert_state_from_fixture(c: &CardSpecCase, got: &str) {
    const JOIN: &str = " \u{b7} ";
    let r = &c.res;
    let paused = c.act.as_ref().is_some_and(|a| a.paused);
    if r.is_movie {
        let base = r.length.trim();
        if !base.is_empty() {
            let want = if paused {
                format!("{base}{PAUSED_MARKER}")
            } else {
                base.to_string()
            };
            assert_eq!(
                got, want,
                "{}: movie state must carry the runtime",
                c.name
            );
        }
        return;
    }
    if !r.episode_name.is_empty() {
        let body = if paused {
            got.strip_suffix(PAUSED_MARKER).unwrap_or(got)
        } else {
            got
        };
        assert!(
            !body.is_empty(),
            "{}: series state blank, want `code{JOIN}episode`",
            c.name
        );
        assert!(
            crate::utf16_units(body) <= crate::STATE_BUDGET,
            "{}: series state over budget",
            c.name
        );
        if let Some((code, ep)) = body.split_once(JOIN) {
            let core = ep.strip_suffix('\u{2026}').unwrap_or(ep);
            assert!(
                !code.is_empty() && !core.is_empty() && r.episode_name.starts_with(core),
                "{}: series state {got:?} must carry `code{JOIN}episode`, episode {:?}",
                c.name,
                r.episode_name
            );
        }
    }
}

/// Proves the idle card keeps its shipped lines under any preset.
#[test]
fn card_spec_idle_stable() {
    let (dets, states) = card_presets();
    let no_action = Action::default();
    let no_res = Resolved::default();
    for raw in ["", "PotPlayer", "PotPlayer - PotPlayer"] {
        let base = build_activity_resolved(raw, Some(&no_action), &no_res);
        for d in &dets {
            for st in &states {
                let (gd, gs) = CardSpec {
                    details: d.id.clone(),
                    state: st.id.clone(),
                }
                .render(raw, Some(&no_action), &no_res);
                assert!(
                    gd == base.details && gs == base.state,
                    "idle {raw:?} {}/{} = ({gd:?}, {gs:?}), want ({:?}, {:?})",
                    d.id,
                    st.id,
                    base.details,
                    base.state
                );
            }
        }
    }
}

/// The preset type is part of the public surface the Card view iterates.
#[test]
fn card_preset_type_is_public() {
    let p = CardPreset {
        id: "default".to_string(),
        label: "Default".to_string(),
    };
    assert_eq!(p, CardPreset::clone(&p));
}

/// The stored JSON contract the app crate embeds as `Card presence.CardSpec`
/// (`go\config.go:22-30`): Go's `json:"details,omitempty"` /
/// `json:"state,omitempty"`. An empty line is omitted, an absent line loads as
/// empty (and normalizes to the default on use).
#[test]
fn card_spec_serde_matches_go_omitempty() {
    assert_eq!(
        serde_json::to_string(&CardSpec::default()).unwrap(),
        "{}",
        "a zero CardSpec writes no keys (Go omitempty)"
    );
    assert_eq!(
        serde_json::to_string(&default_card_spec()).unwrap(),
        "{\"details\":\"default\",\"state\":\"default\"}",
        "both set lines write both keys"
    );
    assert_eq!(
        serde_json::to_string(&CardSpec {
            details: CARD_DETAILS_TITLE.to_string(),
            ..CardSpec::default()
        })
        .unwrap(),
        "{\"details\":\"title\"}",
        "an empty state line is omitted"
    );
    let parsed: CardSpec = serde_json::from_str("{\"details\":\"tag\"}").unwrap();
    assert_eq!(parsed.details, "tag");
    assert_eq!(parsed.state, "", "an absent line loads empty");
    let parsed: CardSpec = serde_json::from_str("{}").unwrap();
    assert_eq!(parsed, CardSpec::default(), "an empty object loads zero");
    assert!(parsed.is_default(), "and normalizes to the default");
}

/// The default body lines are identity/playback only: a series state is code
/// plus episode (never the synopsis, which rides the tooltip only when it is
/// work-level), the movie details is year/genres and the movie state the
/// runtime. Drives the real builder for a movie and
/// a series, and the default spec must agree byte-for-byte with the builder on
/// both. Also pins the tail: a bare movie default stays empty on both
/// lines rather than repeating the title.
#[test]
fn default_state_carries_synopsis_for_series() {
    let run = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let movie = Resolved {
        series: "Dune: Part Two".to_string(),
        year: 2024,
        is_movie: true,
        genres: "Sci-Fi".to_string(),
        length: "2h 46m".to_string(),
        synopsis: "Paul unites the Fremen".to_string(),
        synopsis_is_work: true,
        ..Resolved::default()
    };
    let got = build_activity_resolved(
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        Some(&run),
        &movie,
    );
    assert_eq!(got.state, "2h 46m", "movie state");
    assert!(
        !got.state.contains("Paul unites the Fremen"),
        "movie state duplicates the synopsis: {:?}",
        got.state
    );
    assert_eq!(
        got.details, "2024 \u{b7} Sci-Fi",
        "movie details is year and genres"
    );
    assert_eq!(
        got.large_text, "Paul unites the Fremen",
        "the movie tooltip carries the synopsis snippet"
    );
    let (d, s) = default_card_spec().render(
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        Some(&run),
        &movie,
    );
    assert_eq!(
        (d.as_str(), s.as_str()),
        (got.details.as_str(), got.state.as_str()),
        "default spec must agree with the builder"
    );
    let series = Resolved {
        series: "Canonical Series".to_string(),
        episode_name: "Canonical Episode".to_string(),
        synopsis: "A brave synopsis".to_string(),
        ..Resolved::default()
    };
    let got = build_activity_resolved(
        "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
        Some(&run),
        &series,
    );
    assert_eq!(
        got.details, "",
        "series details blanks: the header already carries the series title"
    );
    assert_eq!(got.state, "S1 E2 \u{b7} Canonical Episode", "series state");
    assert!(
        !got.state.contains("A brave synopsis"),
        "the shipped state line is playback only: {:?}",
        got.state
    );
    assert_eq!(
        got.large_text, "",
        "series tooltip hides episode prose"
    );
    let (d, s) = default_card_spec().render(
        "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
        Some(&run),
        &series,
    );
    assert_eq!(
        (d.as_str(), s.as_str()),
        (got.details.as_str(), got.state.as_str()),
        "default spec must agree with the builder"
    );
    let bare_movie = Resolved {
        is_movie: true,
        synopsis: "Paul unites the Fremen".to_string(),
        synopsis_is_work: true,
        ..Resolved::default()
    };
    let got = build_activity_resolved("Movie.mkv - PotPlayer", Some(&run), &bare_movie);
    assert_eq!(
        got.details, "",
        "bare movie details blanks: the clean title is the header"
    );
    assert_eq!(got.state, "", "bare movie state stays empty");
    assert!(
        !got.state.contains("Paul unites the Fremen"),
        "bare movie state must not duplicate the synopsis: {:?}",
        got.state
    );
    assert_eq!(
        got.large_text, "Paul unites the Fremen",
        "the synopsis lives in the tooltip"
    );
    let (d, s) = default_card_spec().render("Movie.mkv - PotPlayer", Some(&run), &bare_movie);
    assert_eq!(
        (d.as_str(), s.as_str()),
        (got.details.as_str(), got.state.as_str()),
        "default spec must agree with the builder"
    );
}

/// The explicitly chosen Synopsis preset still renders the synopsis inline --
/// and the series default now agrees with it -- joined after the tag for a
/// series, within the 72-unit [`STATE_SYNOPSIS_BUDGET`].
#[test]
fn synopsis_preset_renders_inline() {
    let run = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let spec = CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: CARD_STATE_SYNOPSIS.to_string(),
    };
    let (_, s) = spec.render(
        "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
        Some(&run),
        &Resolved {
            series: "Canonical Series".to_string(),
            episode_name: "Canonical Episode".to_string(),
            synopsis: "A brave synopsis".to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(s, "S1 E2 - A brave synopsis", "series preset state");
    let n = java_units(&s);
    assert!(
        n <= STATE_SYNOPSIS_BUDGET,
        "preset state length {n} exceeds {STATE_SYNOPSIS_BUDGET}: {s:?}"
    );
}

/// S3: the opt-in SYNOPSIS preset keeps the old synopsis-bearing line shapes --
/// clause cuts end on the word with the ellipsis terminator, long lines end at
/// a word boundary, all within the 128-unit ceiling.
#[test]
fn synopsis_preset_keeps_old_line_shapes() {
    let run = Action {
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
    let syn = "Years after the battle with Earth's greatest enemy, Majin Buu, the world faces a new threat from beyond the stars and more";
    let (_, s) = syn_spec.render(
        "Dragon.Ball.Movie.1990.1080p.mkv - PotPlayer",
        Some(&run),
        &Resolved {
            series: "Dragon Ball".to_string(),
            year: 1990,
            is_movie: true,
            synopsis: syn.to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        s,
        "Years after the battle with Earth's greatest enemy, Majin Buu\u{2026}",
        "comma-cut preset state"
    );
    let syn = "Allsafe is controlled; the Dark Army is ready to meet Elliot; the fsociety plan falls apart at midnight somehow";
    let (_, s) = syn_spec.render(
        "Show.S02E04.Episode.1080p.mkv - PotPlayer",
        Some(&run),
        &Resolved {
            series: "Show".to_string(),
            synopsis: syn.to_string(),
            ..Resolved::default()
        },
    );
    assert_eq!(
        s,
        "S2 E4 - Allsafe is controlled; the Dark Army is ready to meet Elliot\u{2026}",
        "semicolon-cut preset state"
    );
    let (_, s) = syn_spec.render(
        "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
        Some(&run),
        &Resolved {
            series: "Dune".to_string(),
            is_movie: true,
            genres: "Adventure".to_string(),
            length: "2h 46m".to_string(),
            synopsis: format!("{}finale", "epic ".repeat(40)),
            ..Resolved::default()
        },
    );
    assert!(
        s.ends_with('\u{2026}'),
        "long preset state = {s:?}, want trailing ellipsis"
    );
    let n = s.encode_utf16().count();
    assert!(
        n <= 128,
        "long preset state length {n} exceeds 128: {s:?}"
    );
    let body = s.trim_end_matches('\u{2026}');
    assert!(
        body.ends_with("epic") || body.ends_with("finale"),
        "long preset state ends mid-word: {s:?}"
    );
}

/// The slot-completeness gate: over {series, anime, movie} x {full with
/// synopsis, resolved without, partial (no genres/length), unresolved,
/// nospace (300-char single token), cjk (CJK run), sentence (`x`x120 + ". " +
/// `y`x20)} x {poster, no poster} x
/// {playing, paused} x {each of the 4 details presets} x {each of the 4 state
/// presets}, every slot of the composed card is non-empty for every preset
/// except `none` (whose state hides), every line stays within its budget (128;
/// 72 for a synopsis-bearing state), and a truncated line is always signalled:
/// either a single trailing U+2026 whose body ends on a word character
/// (pinned for the no-space and CJK cuts too) or a complete sentence keeping
/// its own `.`/`!`/`?` terminator with no ellipsis. A line is never silently
/// truncated.
#[test]
fn slot_completeness_gate() {
    const POSTER: &str = "https://example.com/poster.jpg";
    const LONG_SYN: &str = "epic ";
    let long_syn = format!("{}finale", LONG_SYN.repeat(40));
    let short_syn = "A brave teaser unfolds".to_string();
    let nospace_syn = "x".repeat(300);
    let cjk_syn = "\u{6f22}".repeat(300);
    let sentence_syn = format!("{}. {}", "x".repeat(120), "y".repeat(20));
    struct Kind {
        label: &'static str,
        raw: &'static str,
        is_movie: bool,
    }
    let kinds = [
        Kind {
            label: "series",
            raw: "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
            is_movie: false,
        },
        Kind {
            label: "anime",
            raw: "Frieren.S01E01.The.Heros.Rest.1080p.mkv - PotPlayer",
            is_movie: false,
        },
        Kind {
            label: "movie",
            raw: "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
            is_movie: true,
        },
    ];
    let variants_for = |kind: &Kind| -> Vec<(&'static str, Resolved)> {
        let full = match kind.label {
            "movie" => Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: long_syn.clone(),
                ..Resolved::default()
            },
            "anime" => Resolved {
                series: "Frieren".to_string(),
                episode_name: "The Hero's Rest".to_string(),
                synopsis: long_syn.clone(),
                year: 2023,
                ..Resolved::default()
            },
            _ => Resolved {
                series: "Canonical Series".to_string(),
                episode_name: "Canonical Episode".to_string(),
                synopsis: long_syn.clone(),
                ..Resolved::default()
            },
        };
        let nosyn = match kind.label {
            "movie" => Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                ..Resolved::default()
            },
            "anime" => Resolved {
                series: "Frieren".to_string(),
                episode_name: "The Hero's Rest".to_string(),
                year: 2023,
                ..Resolved::default()
            },
            _ => Resolved {
                series: "Canonical Series".to_string(),
                episode_name: "Canonical Episode".to_string(),
                ..Resolved::default()
            },
        };
        let partial = match kind.label {
            "movie" => Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                synopsis: short_syn.clone(),
                ..Resolved::default()
            },
            "anime" => Resolved {
                series: "Frieren".to_string(),
                synopsis: short_syn.clone(),
                year: 2023,
                ..Resolved::default()
            },
            _ => Resolved {
                series: "Canonical Series".to_string(),
                synopsis: short_syn.clone(),
                ..Resolved::default()
            },
        };
        let nospace = match kind.label {
            "movie" => Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: nospace_syn.clone(),
                ..Resolved::default()
            },
            "anime" => Resolved {
                series: "Frieren".to_string(),
                episode_name: "The Hero's Rest".to_string(),
                synopsis: nospace_syn.clone(),
                year: 2023,
                ..Resolved::default()
            },
            _ => Resolved {
                series: "Canonical Series".to_string(),
                episode_name: "Canonical Episode".to_string(),
                synopsis: nospace_syn.clone(),
                ..Resolved::default()
            },
        };
        let cjk = match kind.label {
            "movie" => Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: cjk_syn.clone(),
                ..Resolved::default()
            },
            "anime" => Resolved {
                series: "Frieren".to_string(),
                episode_name: "The Hero's Rest".to_string(),
                synopsis: cjk_syn.clone(),
                year: 2023,
                ..Resolved::default()
            },
            _ => Resolved {
                series: "Canonical Series".to_string(),
                episode_name: "Canonical Episode".to_string(),
                synopsis: cjk_syn.clone(),
                ..Resolved::default()
            },
        };
        let sentence = match kind.label {
            "movie" => Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: sentence_syn.clone(),
                ..Resolved::default()
            },
            "anime" => Resolved {
                series: "Frieren".to_string(),
                episode_name: "The Hero's Rest".to_string(),
                synopsis: sentence_syn.clone(),
                year: 2023,
                ..Resolved::default()
            },
            _ => Resolved {
                series: "Canonical Series".to_string(),
                episode_name: "Canonical Episode".to_string(),
                synopsis: sentence_syn.clone(),
                ..Resolved::default()
            },
        };
        vec![
            ("full", full),
            ("nosyn", nosyn),
            ("partial", partial),
            ("unresolved", Resolved::default()),
            ("nospace", nospace),
            ("cjk", cjk),
            ("sentence", sentence),
        ]
    };
    let state_presets = [
        CARD_STATE_DEFAULT,
        CARD_STATE_TAG,
        CARD_STATE_SYNOPSIS,
        CARD_STATE_NONE,
    ];
    let details_presets = [
        CARD_DETAILS_DEFAULT,
        CARD_DETAILS_TITLE,
        CARD_DETAILS_EPISODE,
        CARD_DETAILS_SERIES,
    ];
    for kind in &kinds {
        for (vlabel, base) in variants_for(kind) {
            for poster in [true, false] {
                let mut res = base.clone();
                res.synopsis_is_work = true;
                res.artwork = if poster {
                    POSTER.to_string()
                } else {
                    String::new()
                };
                for playing in [true, false] {
                    let act = if playing {
                        Action {
                            running: true,
                            has_start: true,
                            start: 1,
                            ct: 2,
                            ..Action::default()
                        }
                    } else {
                        Action {
                            paused: true,
                            ct: 1000,
                            ..Action::default()
                        }
                    };
                    for dpreset in details_presets {
                        for spreset in state_presets {
                            let spec = CardSpec {
                                details: dpreset.to_string(),
                                state: spreset.to_string(),
                            };
                            let card = compose_card(
                                kind.raw,
                                Some(&act),
                                &res,
                                kind.is_movie,
                                &spec,
                            );
                            let ctx = format!(
                                "{} {vlabel} poster={poster} {} details={dpreset} state={spreset}",
                                kind.label,
                                if playing { "playing" } else { "paused" }
                            );
                            assert!(
                                !card.name.is_empty(),
                                "{ctx}: name blank"
                            );
                            let raw_details =
                                crate::compose_details_for(kind.raw, &res, kind.is_movie, dpreset);
                            assert!(
                                !card.details.is_empty()
                                    || raw_details.is_empty()
                                    || raw_details == card.name,
                                "{ctx}: details blanked without repeating the header"
                            );
                            assert!(
                                card.details.is_empty() || card.details != card.name,
                                "{ctx}: details repeats the header"
                            );
                            assert!(
                                !card.large_text.is_empty()
                                    || res.synopsis.is_empty()
                                    || !res.synopsis_is_work,
                                "{ctx}: large_text blank"
                            );
                            if poster {
                                assert_eq!(
                                    card.large_image, POSTER,
                                    "{ctx}: large_image, want poster"
                                );
                            } else {
                                assert!(
                                    card.large_image.is_empty(),
                                    "{ctx}: large_image = {:?}, want empty (never the branded mark)",
                                    card.large_image
                                );
                            }
                            if spreset == CARD_STATE_NONE {
                                assert!(
                                    card.state.is_empty(),
                                    "{ctx}: none state = {:?}, want empty",
                                    card.state
                                );
                            } else if kind.is_movie
                                && spreset == CARD_STATE_DEFAULT
                                && card.state.is_empty()
                            {
                                if !playing {
                                    assert!(
                                        card.details.ends_with(PAUSED_MARKER),
                                        "{ctx}: paused movie with empty state must park the marker on the details, got {:?}/{:?}",
                                        card.details,
                                        card.state
                                    );
                                }
                            } else if kind.is_movie
                                && (spreset == CARD_STATE_TAG || spreset == CARD_STATE_SYNOPSIS)
                                && card.state.is_empty()
                                && res.synopsis.is_empty()
                                && res.year <= 0
                                && res.genres.is_empty()
                                && res.length.is_empty()
                            {
                                assert!(
                                    playing,
                                    "{ctx}: paused header-equal movie state = {:?}, want Paused",
                                    card.state
                                );
                            } else if !kind.is_movie && card.state.is_empty() {
                                assert!(
                                    playing,
                                    "{ctx}: paused series state = {:?}, want Paused",
                                    card.state
                                );
                            } else {
                                assert!(
                                    !card.state.is_empty(),
                                    "{ctx}: state blank"
                                );
                                if !playing {
                                    assert!(
                                        card.state.ends_with(PAUSED_MARKER)
                                            || card.state == "Paused",
                                        "{ctx}: paused state = {:?}, want the marker",
                                        card.state
                                    );
                                }
                            }
                            for (slot, line, budget) in [
                                ("name", card.name.as_str(), 128usize),
                                ("details", card.details.as_str(), 128usize),
                                ("large_text", card.large_text.as_str(), 128usize),
                            ] {
                                let n = line.encode_utf16().count();
                                assert!(
                                    n <= budget,
                                    "{ctx}: {slot} length {n} exceeds {budget}: {line:?}"
                                );
                                assert_gate_cut(slot, line, budget, &ctx);
                            }
                            let state_budget =
                                if state_carries_synopsis(&card.state, &res.synopsis) {
                                    72usize
                                } else {
                                    128usize
                                };
                            let n = card.state.encode_utf16().count();
                            assert!(
                                n <= state_budget,
                                "{ctx}: state length {n} exceeds {state_budget}: {:?}",
                                card.state
                            );
                            assert_gate_cut("state", &card.state, state_budget, &ctx);
                            if (vlabel == "nospace" || vlabel == "cjk")
                                && spreset != CARD_STATE_NONE
                            {
                                if !kind.is_movie {
                                    assert!(
                                        card.large_text.ends_with('\u{2026}'),
                                        "{ctx}: large_text = {:?}, want trailing ellipsis",
                                        card.large_text
                                    );
                                }
                                if state_carries_synopsis(&card.state, &res.synopsis) {
                                    if playing {
                                        assert!(
                                            card.state.ends_with('\u{2026}'),
                                            "{ctx}: state = {:?}, want trailing ellipsis",
                                            card.state
                                        );
                                    } else {
                                        assert!(
                                            card.state.contains('\u{2026}'),
                                            "{ctx}: state = {:?}, want the ellipsis before the marker",
                                            card.state
                                        );
                                    }
                                }
                            }
                            if vlabel == "sentence"
                                && spreset != CARD_STATE_NONE
                                && !kind.is_movie
                            {
                                let want = "x".repeat(120) + "\u{2026}";
                                assert_eq!(
                                    card.large_text, want,
                                    "{ctx}: large_text, want the cut sentence body plus the ellipsis"
                                );
                                assert!(
                                    card.large_text.contains('\u{2026}'),
                                    "{ctx}: large_text = {:?}, want the explicit ellipsis",
                                    card.large_text
                                );
                            }
                            if !res.synopsis.is_empty() {
                                if !kind.is_movie {
                                    assert_gate_signal(
                                        "large_text",
                                        &card.large_text,
                                        &res.synopsis,
                                        &ctx,
                                    );
                                }
                                if spreset != CARD_STATE_NONE
                                    && state_carries_synopsis(&card.state, &res.synopsis)
                                {
                                    assert_gate_signal(
                                        "state",
                                        &card.state,
                                        &res.synopsis,
                                        &ctx,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Whether `state` carries the synopsis: the first distinctive synopsis word
/// (longer than 3 chars, so "A"/"the" cannot false-positive) appears in it. A
/// truncated single-token or CJK run carries only a prefix of its long word,
/// so a missing whole word falls back to an 8-char prefix check.
fn state_carries_synopsis(state: &str, synopsis: &str) -> bool {
    let Some(word) = synopsis.split_whitespace().find(|w| w.len() > 3) else {
        let flat: String = synopsis.split_whitespace().collect();
        if flat.is_empty() {
            return false;
        }
        let prefix: String = flat.chars().take(8).collect();
        return !prefix.is_empty() && state.contains(&prefix);
    };
    if state.contains(word) {
        return true;
    }
    let prefix: String = word.chars().take(8).collect();
    !prefix.is_empty() && state.contains(&prefix)
}

/// A gate line is never silently truncated: never three dots, never more than
/// one ellipsis, and an ellipsis-terminated cut ends on a word character
/// (never `,` `;` `:` `-` `–` `—` `·` or blank). A complete sentence keeping
/// its own `.`/`!`/`?` with no ellipsis is the other valid signal; anything
/// else must fit below its budget (at cap without a signal is a silent cut).
fn assert_gate_cut(slot: &str, line: &str, budget: usize, ctx: &str) {
    assert!(
        !line.contains("..."),
        "{ctx}: {slot} never three dots: {line:?}"
    );
    let ell = line.chars().filter(|&c| c == '\u{2026}').count();
    assert!(
        ell <= 1,
        "{ctx}: {slot} multiple ellipsis: {line:?}"
    );
    if line.ends_with('\u{2026}') {
        let body = line.trim_end_matches('\u{2026}');
        assert!(
            !body.is_empty(),
            "{ctx}: {slot} empty cut body: {line:?}"
        );
        let last = body.chars().next_back().unwrap();
        assert!(
            last.is_alphanumeric(),
            "{ctx}: {slot} cut ends on {last:?}, want a word character: {line:?}"
        );
        return;
    }
    if line.contains('\u{2026}') {
        let idx = line.find('\u{2026}').unwrap();
        let body = &line[..idx];
        assert!(
            !body.is_empty(),
            "{ctx}: {slot} empty cut body: {line:?}"
        );
        let last = body.chars().next_back().unwrap();
        assert!(
            last.is_alphanumeric(),
            "{ctx}: {slot} cut ends on {last:?}, want a word character: {line:?}"
        );
        return;
    }
    if (line.ends_with('.') || line.ends_with('!') || line.ends_with('?'))
        && !line.contains('\u{2026}')
    {
        return;
    }
    let n = line.encode_utf16().count();
    assert!(
        line.is_empty() || n < budget,
        "{ctx}: {slot} at cap without a signal (silent truncation): {line:?}"
    );
}

/// A line shorter than its synopsis source must end signalled: either with a
/// single U+2026 (trailing, or before the paused marker) whose body ends on a
/// word character, or with a sentence terminator `.`/`!`/`?` and no ellipsis.
/// A line that already carries its source whole needs no signal.
fn assert_gate_signal(slot: &str, line: &str, synopsis: &str, ctx: &str) {
    let full: String = synopsis.split_whitespace().collect::<Vec<_>>().join(" ");
    if line == full || full.is_empty() {
        return;
    }
    let line_units = line.encode_utf16().count();
    let full_units = full.encode_utf16().count();
    if line_units >= full_units {
        return;
    }
    if line.ends_with('\u{2026}') {
        let body = line.trim_end_matches('\u{2026}');
        assert!(
            !body.is_empty(),
            "{ctx}: {slot} empty cut body: {line:?}"
        );
        let last = body.chars().next_back().unwrap();
        assert!(
            last.is_alphanumeric(),
            "{ctx}: {slot} cut ends on {last:?}, want a word character: {line:?}"
        );
        return;
    }
    if line.contains('\u{2026}') {
        return;
    }
    assert!(
        (line.ends_with('.') || line.ends_with('!') || line.ends_with('?'))
            && !line.contains('\u{2026}'),
        "{ctx}: {slot} shorter than its source without a signal: {line:?}"
    );
}

/// T5-B (red first), extended to the retired Episode preset: a stored retired
/// Details id migrates to the default line without touching anything else.
/// `title`/`series` details become `default` (they repeated the header);
/// `episode` becomes `default` (it rendered byte-identical to it for both
/// kinds); kept ids, unknown ids and every state line survive byte-for-byte,
/// so the migration can never lose an unrelated spec.
#[test]
fn retired_details_ids_migrate_to_default() {
    let migrated = |details: &str, state: &str| {
        CardSpec {
            details: details.to_string(),
            state: state.to_string(),
        }
        .migrated()
    };
    let def = |state: &str| CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: state.to_string(),
    };
    assert_eq!(migrated("title", "tag"), def(CARD_STATE_TAG));
    assert_eq!(migrated("series", "none"), def(CARD_STATE_NONE));
    assert_eq!(
        migrated("title", "synopsis"),
        def(CARD_STATE_SYNOPSIS),
        "a retired details id migrates even beside a kept state"
    );
    assert_eq!(migrated("episode", "tag"), def(CARD_STATE_TAG));
    assert_eq!(migrated("default", "default"), default_card_spec());
    assert_eq!(
        migrated("fancy-template", "tag"),
        CardSpec {
            details: "fancy-template".to_string(),
            state: CARD_STATE_TAG.to_string(),
        }
    );
    assert_eq!(def(CARD_STATE_TAG).migrated(), def(CARD_STATE_TAG));
}

/// T5-B: the four kept line combos still compose through the real composer --
/// the T4 TV shapes and the T5-A movie shapes. Series default, series
/// episode+synopsis, movie default, movie synopsis state: no body line repeats
/// the header title.
#[test]
fn kept_line_combos_compose_without_repeating_the_header() {
    const SERIES_RAW: &str = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
    const MOVIE_RAW: &str = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
    let run = Action {
        running: true,
        has_start: true,
        start: 12345,
        ct: 1000,
        ..Action::default()
    };
    let series = Resolved {
        series: "Canonical Series".to_string(),
        episode_name: "Canonical Episode".to_string(),
        synopsis: "A brave synopsis".to_string(),
        season: 1,
        number: 2,
        ..Resolved::default()
    };
    let movie = Resolved {
        series: "Dune: Part Two".to_string(),
        year: 2024,
        is_movie: true,
        genres: "Sci-Fi".to_string(),
        length: "2h 46m".to_string(),
        synopsis: "Paul unites the Fremen".to_string(),
        ..Resolved::default()
    };
    let card = compose_card(SERIES_RAW, Some(&run), &series, false, &default_card_spec());
    assert_eq!(card.details, "", "series default details");
    assert_eq!(card.state, "S01E02 \u{b7} Canonical Episode", "series default state");
    let kept = CardSpec {
        details: CARD_DETAILS_EPISODE.to_string(),
        state: CARD_STATE_SYNOPSIS.to_string(),
    };
    let card = compose_card(SERIES_RAW, Some(&run), &series, false, &kept);
    assert_eq!(
        (card.details.as_str(), card.state.as_str()),
        ("Canonical Episode", "S01E02 - A brave synopsis"),
        "series episode+synopsis"
    );
    let card = compose_card(MOVIE_RAW, Some(&run), &movie, true, &default_card_spec());
    assert_eq!(card.details, "2024 \u{b7} Sci-Fi", "movie default details");
    assert_eq!(card.state, "2h 46m", "movie default state");
    assert_eq!(card.name, "Dune: Part Two", "the header keeps the title");
    assert!(
        !card.state.contains("Dune") && card.details != card.name,
        "movie body lines must not repeat the header: {:?}",
        (&card.details, &card.state)
    );
    let kept = CardSpec {
        details: CARD_DETAILS_DEFAULT.to_string(),
        state: CARD_STATE_SYNOPSIS.to_string(),
    };
    let card = compose_card(MOVIE_RAW, Some(&run), &movie, true, &kept);
    assert_eq!(
        card.state,
        "Sci-Fi \u{b7} 2h 46m \u{b7} Paul unites the Fremen",
        "movie synopsis state"
    );
}