
use crate::parse::{date_year, parse, parse_raw};
use crate::types::{Kind, Query};

/// Shorthand for the Go table rows: a query with only the named fields set.
fn q(kind: Kind, title: &str) -> Query {
    Query {
        kind,
        title: title.to_string(),
        ..Query::default()
    }
}

#[test]
fn parse_cases() {
    let cases: &[(&str, &str, &str, &str, Query)] = &[
        (
            "season episode",
            "Show",
            "S1 E2",
            "",
            Query {
                kind: Kind::SERIES,
                title: "Show".into(),
                season: 1,
                episode: 2,
                ..Query::default()
            },
        ),
        (
            "no space lowercase",
            "Show",
            "s01e02",
            "",
            Query {
                kind: Kind::SERIES,
                title: "Show".into(),
                season: 1,
                episode: 2,
                ..Query::default()
            },
        ),
        (
            "absolute",
            "Show",
            "E12",
            "",
            Query {
                kind: Kind::SERIES,
                title: "Show".into(),
                episode: 12,
                absolute: true,
                ..Query::default()
            },
        ),
        (
            "op theme",
            "Zentaro",
            "OP 1",
            "",
            q(Kind::UNKNOWN, "Zentaro"),
        ),
        (
            "ed lowercase theme",
            "Zentaro",
            "ed 12",
            "",
            q(Kind::UNKNOWN, "Zentaro"),
        ),
        ("movie", "The Matrix", "", "", q(Kind::MOVIE, "The Matrix")),
        (
            "year stripped",
            "Harbor Lights (2008)",
            "S1 E1",
            "",
            Query {
                kind: Kind::SERIES,
                title: "Harbor Lights".into(),
                year: 2008,
                season: 1,
                episode: 1,
                ..Query::default()
            },
        ),
        (
            "movie year",
            "Inception (2010)",
            "",
            "",
            Query {
                kind: Kind::MOVIE,
                title: "Inception".into(),
                year: 2010,
                ..Query::default()
            },
        ),
        ("no signal", "Show", "Special", "", q(Kind::MOVIE, "Show")),
    ];
    for (name, title, tag, epi, want) in cases {
        assert_eq!(
            &parse(title, tag, epi),
            want,
            "Parse({title:?},{tag:?},{epi:?}) [{name}]"
        );
    }
}

#[test]
fn parse_titleclean_series() {
    let c = titleclean::clean("Show.S01E02.Pilot.1080p.mkv");
    assert_eq!(c.episode_tag, "S1 E2");
    assert_eq!(c.episode_name, "Pilot");
    let want = Query {
        kind: Kind::SERIES,
        title: "Show".into(),
        season: 1,
        episode: 2,
        ..Query::default()
    };
    assert_eq!(parse(&c.title, &c.episode_tag, &c.episode_name), want);
}

#[test]
fn parse_titleclean_movie() {
    let c = titleclean::clean("Inception.2010.1080p.BluRay.x264.mkv");
    assert_eq!(c.episode_tag, "", "movie, no tag");
    let got = parse(&c.title, &c.episode_tag, &c.episode_name);
    assert_eq!(got.kind, Kind::MOVIE);
    assert_eq!((got.title.as_str(), got.year), ("Inception", 2010));
}

/// `TestParseMovieMarker` pins the movie-marker rule: a "Movie"/"the Movie"
/// marker in the cleaned title, with its number or a trailing subtitle, means
/// the file is a film -- the marker (and its number) is stripped, the trailing
/// subtitle is composed back onto the base title, and the weak absolute tag
/// titleclean's ALT_EP split left behind is dropped. The last three rows are
/// REGRESSIONS asserted unchanged.
#[test]
fn parse_movie_marker() {
    let cases: &[(&str, Kind, &str, i32)] = &[
        (
            "[Anime Time] Dragon Ball Super Movie 01 - Broly.mkv",
            Kind::MOVIE,
            "Dragon Ball Super: Broly",
            0,
        ),
        (
            "[Anime Time] Dragon Ball Super Movie 01.mkv",
            Kind::MOVIE,
            "Dragon Ball Super",
            0,
        ),
        (
            "[Anime Time] Dragon Ball Super the Movie 01 - Broly.mkv",
            Kind::MOVIE,
            "Dragon Ball Super: Broly",
            0,
        ),
        (
            "[SubsPlease] Frieren - 01 (1080p).mkv",
            Kind::SERIES,
            "Frieren",
            1,
        ),
        (
            "Dragon Ball Super - 01.mkv",
            Kind::SERIES,
            "Dragon Ball Super",
            1,
        ),
        (
            "[JySzE] Dragon Ball Super - Broly [2018].mkv",
            Kind::MOVIE,
            "Dragon Ball Super - Broly",
            0,
        ),
    ];
    for (file, kind, title, ep) in cases {
        let got = parse_raw(file);
        assert_eq!(
            (got.kind, got.title.as_str(), got.season, got.episode),
            (*kind, *title, 0, *ep),
            "ParseRaw({file:?})"
        );
    }
}

#[test]
fn parse_raw_strips_suffix() {
    let want = Query {
        kind: Kind::SERIES,
        title: "Show".into(),
        season: 1,
        episode: 2,
        ..Query::default()
    };
    assert_eq!(
        parse_raw("Show.S01E02.Pilot.1080p.mkv - PotPlayer"),
        want,
        "suffixed"
    );
    assert_eq!(parse_raw("Show.S01E02.Pilot.1080p.mkv"), want, "bare");
}

/// Addition: the ALT_EP year scars. A parenthesised year whose digit
/// titleclean's ALT_EP split ate is still a release year: `Now You See Me 2
/// (2016)` must classify as the MOVIE `Now You See Me 2` (sequel digit
/// restored, never an episode); a junk-bearing movie filename whose cleaned
/// title carries no year still queries with the raw's plausible year, the
/// title untouched. A strong SxxEyy tag outranks both.
#[test]
fn parse_raw_salvages_parenthesised_and_junk_years() {
    let q = parse_raw("Now You See Me 2 (2016)");
    assert_eq!(
        q,
        Query {
            kind: Kind::MOVIE,
            title: "Now You See Me 2".into(),
            year: 2016,
            ..Query::default()
        }
    );
    assert_eq!(
        q,
        parse_raw("Now You See Me 2 (2016).mkv"),
        "the extension must not change the salvage"
    );
    let q = parse_raw(
        "The.Blair.Witch.Project.1999.BluRay.1080p.REMUX.AVC.DTS-HD.MA.2.0-LEGi0N.mkv",
    );
    assert_eq!(q.kind, Kind::MOVIE);
    assert_eq!(q.year, 1999);
    assert_eq!(q.title, "The Blair Witch Project 1999 REMUX AVC MA 2 0");
    let q = parse_raw("Show S01E02 (2016).mkv");
    assert_eq!(
        (q.kind, q.title.as_str(), q.season, q.episode),
        (Kind::SERIES, "Show", 1, 2),
        "a strong SxxEyy tag must win"
    );
}

/// A dashed episode number before a parenthesised year is an episode, never a
/// sequel digit: `Show - 05 (2019)` (and its `-05`/`- 5` shapes) stays SERIES
/// E5, while the genuine `Title N (YYYY)` sequel still flips to MOVIE.
#[test]
fn parse_raw_dashed_episode_never_flips_to_movie() {
    for raw in ["Show - 05 (2019)", "Show-05 (2019)", "Show - 5 (2019)"] {
        let q = parse_raw(raw);
        assert_eq!(
            (q.kind, q.title.as_str(), q.season, q.episode, q.absolute),
            (Kind::SERIES, "Show", 0, 5, true),
            "ParseRaw({raw:?}) = {q:?}, want SERIES E5"
        );
    }
    let q = parse_raw("Now You See Me 2 (2016)");
    assert_eq!(
        (q.kind, q.title.as_str(), q.year),
        (Kind::MOVIE, "Now You See Me 2", 2016),
        "a genuine sequel digit still flips: {q:?}"
    );
}

/// A bare year titleclean mistook for the release year loses to the raw
/// parenthesised year: `Blade Runner 2049 (2017)` salvages 2017 with the 2049
/// restored to the title, while the junk-name fallback still salvages the
/// last standalone token (1999).
#[test]
fn parse_raw_prefers_parenthesised_year_over_bare_title_year() {
    let q = parse_raw("Blade Runner 2049 (2017)");
    assert_eq!(
        (q.kind, q.title.as_str(), q.year),
        (Kind::MOVIE, "Blade Runner 2049", 2017),
        "parenthesised year wins and the bare year returns to the title: {q:?}"
    );
    let q = parse_raw(
        "The.Blair.Witch.Project.1999.BluRay.1080p.REMUX.AVC.DTS-HD.MA.2.0-LEGi0N.mkv",
    );
    assert_eq!(q.year, 1999, "junk fallback still salvages 1999: {q:?}");
}

/// `TestParseBonusVariantCoverage` is the executable parse table for the 34
/// synthetic bonus-variant negative-cache keys: 18 phrase-stem extras are
/// KindUnknown (each saves one ~3000 ms wasted iTunes lookup); the other 16
/// stay KindMovie because no phrase stem covers them without risking a real
/// title.
#[test]
fn parse_bonus_variant_coverage() {
    let cases: &[(&str, Kind)] = &[
        ("blooper reel.mkv", Kind::UNKNOWN),
        ("gag reel.mkv", Kind::UNKNOWN),
        ("outtakes.mkv", Kind::UNKNOWN),
        ("tv spots.mkv", Kind::UNKNOWN),
        ("extended scenes.mkv", Kind::UNKNOWN),
        ("alternate & extended scenes.mkv", Kind::UNKNOWN),
        ("behind the story - harbor tour with rex.mkv", Kind::UNKNOWN),
        (
            "behind the story - the quiet cadence of harlan marsh.mkv",
            Kind::UNKNOWN,
        ),
        ("cinder - the making of the drifters.mkv", Kind::UNKNOWN),
        (
            "charting the course - the making of harbor run.mkv",
            Kind::UNKNOWN,
        ),
        ("in search of driftland.mkv", Kind::UNKNOWN),
        ("how to survive the q tempest.mkv", Kind::UNKNOWN),
        ("meet the cast.mkv", Kind::UNKNOWN),
        ("previz - distant concepts.mkv", Kind::UNKNOWN),
        ("visual effects assembly reels.mkv", Kind::UNKNOWN),
        ("replacing milo - the secret auditions.mkv", Kind::UNKNOWN),
        ("4) the production design.mkv", Kind::UNKNOWN),
        (
            "5) building harborline - time lapse video.mkv",
            Kind::UNKNOWN,
        ),
        ("lateral thinking - the cutting process.mkv", Kind::MOVIE),
        ("becoming harlow.mkv", Kind::MOVIE),
        ("how to sketch.mkv", Kind::MOVIE),
        ("chart of ruin.mkv", Kind::MOVIE),
        ("testing the bounds.mkv", Kind::MOVIE),
        ("retrieval mode.mkv", Kind::MOVIE),
        ("ignition - harlan's story.mkv", Kind::MOVIE),
        ("the music of 1974.mkv", Kind::MOVIE),
        ("humbucker.mkv", Kind::MOVIE),
        ("tai chi walking.mkv", Kind::MOVIE),
        ("cantonia, usa.mkv", Kind::MOVIE),
        ("porto verro, usa.mkv", Kind::MOVIE),
        ("kestria, japan.mkv", Kind::MOVIE),
        ("the mimickers.mkv", Kind::MOVIE),
        ("2) milo announcement.mkv", Kind::MOVIE),
        ("3) the harbor brothers.mkv", Kind::MOVIE),
    ];
    let mut unknown = 0;
    for (file, want) in cases {
        let got = parse_raw(file);
        assert_eq!(got.kind, *want, "ParseRaw({file:?}) = {got:?}");
        if got.kind == Kind::UNKNOWN {
            unknown += 1;
        }
    }
    assert_eq!(unknown, 18, "table drift: a stem or titleclean changed");
}

/// `TestParseExtrasNoRealTitleCollision` is the collision guard: the synthetic
/// representative titles must still parse as Movie/Series, never Unknown.
#[test]
fn parse_extras_no_real_title_collision() {
    let cases: &[(&str, Kind)] = &[
        ("22.Warp.Avenue.2013.2160p.mkv", Kind::MOVIE),
        ("Iron.Circle.1999.mkv", Kind::MOVIE),
        ("Harbor.Island.2011.mkv", Kind::MOVIE),
        ("The.Wayfarer.2015.mkv", Kind::MOVIE),
        ("Driftfall.2011.mkv", Kind::MOVIE),
        ("Harlan.Unbound.2012.mkv", Kind::MOVIE),
        ("Drift.2011.1080p.mkv", Kind::MOVIE),
        ("Stone.Rider.2051.2017.mkv", Kind::MOVIE),
        ("The.Harbor.Trials.2012.mkv", Kind::MOVIE),
        ("We.re.the.Harlows.2013.mkv", Kind::MOVIE),
        ("Snap-Back.mkv", Kind::MOVIE),
        ("Quimbly.mkv", Kind::MOVIE),
        ("Atlantic.Edge.mkv", Kind::MOVIE),
        ("Marisol.mkv", Kind::MOVIE),
        ("Mimic.2019.1080p.mkv", Kind::MOVIE),
        ("Pivot.2015.1080p.mkv", Kind::MOVIE),
        ("Driftland.Double.Hit.2019.mkv", Kind::MOVIE),
        ("Harbor.Run.2.Styx.2018.mkv", Kind::SERIES),
        ("The.Drifters.3.2014.mkv", Kind::MOVIE),
        ("Welcome.to.Harbor.S01E01.mkv", Kind::SERIES),
        ("Time.Lapse.2014.mkv", Kind::MOVIE),
        ("Time.Lapse.2014.1080p.mkv", Kind::MOVIE),
    ];
    for (file, want) in cases {
        let got = parse_raw(file);
        assert_eq!(got.kind, *want, "ParseRaw({file:?}) = {got:?}");
    }
}

/// `TestParseTimeLapseStemConstrained` pins the time-lapse fix: the bare film
/// "Time Lapse" (2014) is fetched, while the observed "time lapse video" extras
/// phrasing is still skipped.
#[test]
fn parse_time_lapse_stem_constrained() {
    for file in ["Time.Lapse.2014.mkv", "Time.Lapse.2014.1080p.mkv"] {
        let got = parse_raw(file);
        assert_eq!(got.kind, Kind::MOVIE, "ParseRaw({file:?}) = {got:?}");
    }
    let got = parse_raw("5) building harborline - time lapse video.mkv");
    assert_eq!(got.kind, Kind::UNKNOWN, "time lapse video extras = {got:?}");
    let got = parse_raw("Time.Lapse.Video.mkv");
    assert_eq!(
        got.kind,
        Kind::UNKNOWN,
        "ParseRaw(\"Time.Lapse.Video.mkv\")"
    );
}

/// `TestParseAmbiguousExtras` states the four ambiguous classifications and why:
/// each stays fetched because the only stem that would catch it is a single
/// common word or a bare chapter number, both barred.
#[test]
fn parse_ambiguous_extras() {
    let cases: &[(&str, &str)] = &[
        (
            "2) harlan marsh as tom vane.mkv",
            "bare chapter number, no stem",
        ),
        ("action - the drifters.mkv", "single common word, barred"),
        (
            "within and without with harlan marsh.mkv",
            "no safe phrase stem",
        ),
        ("short - the causeway.mkv", "single common word, barred"),
    ];
    for (file, why) in cases {
        let got = parse_raw(file);
        assert_ne!(
            got.kind,
            Kind::UNKNOWN,
            "ParseRaw({file:?}) = {got:?} ({why}), want fetched"
        );
    }
}

/// Addition: `dateYear`'s four-byte read and 1900-2100 gate (no Go test covers
/// it directly; the providers exercise it through TVmaze/iTunes/Jikan dates).
#[test]
fn date_year_reads_the_leading_four_bytes() {
    assert_eq!(date_year("2008-01-20"), 2008);
    assert_eq!(date_year("2010-07-16T07:00:00Z"), 2010);
    assert_eq!(date_year("  2008-01-20  "), 2008);
    assert_eq!(date_year("1899-01-20"), 0);
    assert_eq!(date_year("2101-01-20"), 0);
    assert_eq!(date_year("200"), 0);
    assert_eq!(date_year("nope"), 0);
    assert_eq!(date_year(""), 0);
}

/// Addition: a direct negative control for the `\b` emulation. Go's `extraRe`
/// is `\b(...)\b`, so an extras phrase embedded in a longer word is NOT an
/// extra, while the same phrase standing alone is.
#[test]
fn extra_phrases_need_word_boundaries() {
    for file in ["MyExtrasMovie.mkv", "trailersx.mkv", "galleryland.mkv"] {
        let got = parse_raw(file);
        assert_ne!(
            got.kind,
            Kind::UNKNOWN,
            "ParseRaw({file:?}) = {got:?}, want fetched (no word boundary)"
        );
    }
    for file in ["extras.mkv", "trailers.mkv", "gallery.mkv"] {
        let got = parse_raw(file);
        assert_eq!(
            got.kind,
            Kind::UNKNOWN,
            "ParseRaw({file:?}) = {got:?}, want KindUnknown"
        );
    }
}
