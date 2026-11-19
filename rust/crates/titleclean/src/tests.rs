
use crate::clean::{alt_head_bad, clean, has_year, year_lead, JWS};
use crate::javacompat::{
    is_java_word, java_boundary, java_dollar_replace_all, java_end_positions, java_trunc,
    java_upper_first, jdk_full_upper, jdk_is_letter_or_digit, jdk_is_mn, jdk_upper_simple,
    port_pattern,
};
use crate::regexlite::Regex;

/// Java's `\b` at every code point position of `r`, as `1` / `0` bits.
fn bits(r: &[char]) -> String {
    let mut out = String::with_capacity(r.len() + 1);
    for i in 0..=r.len() {
        out.push(if java_boundary(r, i) { '1' } else { '0' });
    }
    out
}

/// One cleaner expectation: `(input, (title, episode tag, episode name, display))`.
type CleanCase = (
    &'static str,
    (&'static str, &'static str, &'static str, &'static str),
);

/// A named expectation; `clean_title_mark_fold` carries its Go subtest names.
type NamedCleanCase = (
    &'static str,
    &'static str,
    (&'static str, &'static str, &'static str, &'static str),
);

fn assert_cleaned(input: &str, want: (&str, &str, &str, &str)) {
    let got = clean(input);
    assert_eq!(
        (
            got.title.as_str(),
            got.episode_tag.as_str(),
            got.episode_name.as_str(),
            got.display.as_str()
        ),
        want,
        "clean({input:?}) = {got:?}"
    );
}

#[test]
fn java_boundary_bits() {
    let cases: [(&str, &str); 16] = [
        ("a", "11"),
        ("a\u{301}", "101"),
        ("\u{301}", "00"),
        ("\u{E9}", "11"),
        ("\u{7248}", "11"),
        ("a\u{301}b", "1001"),
        ("\u{301}a", "011"),
        ("a\u{301}\u{301}b", "10001"),
        ("1\u{301}", "101"),
        ("A_1", "1001"),
        ("\u{17F}", "11"),
        ("\u{212A}", "11"),
        ("\u{131}", "11"),
        ("a\u{212A}", "101"),
        ("\u{DF}", "11"),
        ("_\u{301}", "110"),
    ];
    for (input, want) in cases {
        let r: Vec<char> = input.chars().collect();
        assert_eq!(bits(&r), want, "javaBoundary({input:?}) bits");
    }
}

#[test]
fn java_trunc_cuts_utf16_units() {
    let astral = "\u{1D54F}"; // one code point, two UTF-16 units
    let input = format!("AAAA{astral}B");
    let cases: [(&str, usize, &str); 6] = [
        (input.as_str(), 3, "AA\u{2026}"),
        (input.as_str(), 4, "AAA\u{2026}"),
        (input.as_str(), 5, "AAAA\u{2026}"),
        (input.as_str(), 6, "AAAA?\u{2026}"),
        ("short", 10, "short"),
        ("", 5, ""),
    ];
    for (subject, n, want) in cases {
        assert_eq!(java_trunc(subject, n), want, "javaTrunc({subject:?},{n})");
    }
}

#[test]
fn java_upper_first_expansions() {
    let cases: [(&str, &str); 6] = [
        ("\u{DF}eta", "SSeta"),
        ("\u{FB01}nal", "FInal"),
        ("\u{1F0}ack", "J\u{30C}ack"),
        ("\u{1D569}yle", "\u{1D569}yle"), // a leading surrogate pair is left as-is
        ("hello", "Hello"),
        ("", ""),
    ];
    for (input, want) in cases {
        assert_eq!(java_upper_first(input), want, "javaUpperFirst({input:?})");
    }
}

#[test]
fn ascii_fold_is_ascii_only() {
    let season = Regex::new(&port_pattern("(?i)season", true));
    for s in ["season", "SEASON", "Season", "sEaSoN"] {
        assert!(season.is_match(s), "ascii-folded season should match {s:?}");
    }
    assert!(
        !season.is_match("\u{17F}eason"),
        "Java CASE_INSENSITIVE is ASCII-only; ſeason must not match season"
    );
    let k = Regex::new(&port_pattern("(?i)4k", true));
    assert!(k.is_match("4K"), "ascii-folded 4k should match ASCII K");
    assert!(
        !k.is_match("4\u{212A}"),
        "Kelvin sign K must not fold to k under Java CASE_INSENSITIVE"
    );
    let i = Regex::new(&port_pattern("(?i)i", true));
    assert!(!i.is_match("\u{131}"), "dotless i must not fold to i");
}

#[test]
fn java_end_positions_before_terminators() {
    let cases: [(&str, &[usize]); 7] = [
        ("abc", &[3]),
        ("a\n", &[2, 1]),
        ("a\r", &[2, 1]),
        ("a\r\n", &[3, 1]),
        ("a\u{2028}", &[4, 1]),
        ("a\u{0085}", &[3, 1]),
        ("a\u{2029}", &[4, 1]),
    ];
    for (input, want) in cases {
        assert_eq!(
            java_end_positions(input),
            want,
            "javaEndPositions({input:?})"
        );
    }
}

#[test]
fn java_dollar_replace_before_terminator() {
    let sep = Regex::new(&port_pattern(r"[. _\-]+$", false));
    assert_eq!(
        java_dollar_replace_all(&sep, "Name \u{2028}", ""),
        "Name\u{2028}"
    );
    let crc_pattern = String::from(JWS) + r"*\[[A-Fa-f0-9]{8}\]" + JWS + r"*$";
    let crc = Regex::new(&port_pattern(&crc_pattern, false));
    assert_eq!(
        java_dollar_replace_all(&crc, "Name [DEADBEEF]\u{2028}", ""),
        "Name\u{2028}"
    );
    assert_eq!(
        java_dollar_replace_all(&crc, "Name [DEADBEEF]\u{0085}", ""),
        "Name\u{0085}"
    );
}

#[test]
fn dot_excludes_line_terminators() {
    let re = Regex::new(&port_pattern("^a.b$", false));
    assert!(
        re.is_match("a\u{E9}b"),
        "dot must match non-terminator code points such as é"
    );
    for s in ["a\nb", "a\rb", "a\u{0085}b", "a\u{2028}b", "a\u{2029}b"] {
        assert!(
            !re.is_match(s),
            "Java dot must not match a line terminator in {s:?}"
        );
    }
}

#[test]
fn java_boundary_astral_marks() {
    // Java's hasBaseCharacter steps UTF-16 units, so an astral base behind a
    // mark run is not found; these bits are JDK truth (code point positions).
    let cases: [(&str, &str); 6] = [
        ("\u{1D400}\u{301}\u{301}", "1100"),
        ("a\u{1D400}\u{301}\u{301}", "10100"),
        ("\u{1D400}\u{301}", "110"),
        ("\u{1D400}x", "101"),
        ("\u{301}\u{1D400}", "011"),
        (
            "x265Chapter\u{301}\u{1D400}\u{301}\u{301}",
            "1000000000000100",
        ),
    ];
    for (input, want) in cases {
        let r: Vec<char> = input.chars().collect();
        assert_eq!(bits(&r), want, "javaBoundary({input:?}) bits");
    }
}

#[test]
fn jdk11_tables() {
    // Drift rows: the compiler's own Unicode is newer than JDK 11's.
    // U+1C90 Georgian Mtavruli is a letter in newer Unicode, not in JDK 11.
    assert!(
        !is_java_word('\u{1C90}'),
        "JDK 11 does not treat U+1C90 as a word character"
    );
    assert!(
        !jdk_is_letter_or_digit('\u{1E100}'),
        "JDK 11 does not treat U+1E100 as a letter"
    );
    assert!(is_java_word('\u{10D0}'), "U+10D0 is a letter in JDK 11");
    assert_eq!(
        jdk_upper_simple('\u{10D0}'),
        '\u{10D0}',
        "JDK 11 leaves U+10D0 unchanged"
    );
    assert_eq!(
        jdk_upper_simple('\u{019B}'),
        '\u{019B}',
        "JDK 11 leaves U+019B unchanged"
    );
    assert!(
        !jdk_is_mn('\u{07FD}'),
        "JDK 11 does not classify U+07FD as a non-spacing mark"
    );
    let c = clean("Movie\u{1C90}1080p.mkv");
    assert_eq!(c.title, "Movie\u{1C90}", "Movie<U+1C90>1080p.mkv = {c:?}");
    assert_eq!(c.episode_tag, "");
    let c = clean("\u{10D0}bc.mkv");
    assert_eq!(c.title, "\u{10D0}bc", "<U+10D0>bc.mkv = {c:?}");
    assert!(is_java_word('a') && is_java_word('_') && !is_java_word('\u{301}'));
    assert!(jdk_is_letter_or_digit('\u{E9}') && jdk_is_letter_or_digit('\u{7248}'));
    assert!(jdk_is_mn('\u{301}'), "U+0301 should be a non-spacing mark");
    assert_eq!(jdk_upper_simple('a'), 'A');
    assert_eq!(jdk_upper_simple('\u{B5}'), '\u{39C}');
    assert_eq!(jdk_upper_simple('\u{3C2}'), '\u{3A3}');
    assert_eq!(jdk_full_upper('\u{DF}'), Some("SS"));
    assert_eq!(jdk_full_upper('\u{FB01}'), Some("FI"));
    assert_eq!(jdk_full_upper('\u{1F0}'), Some("J\u{30C}"));
}

#[test]
fn jdk11_tables_are_ascending() {
    let sorted2 = |rs: &[(char, char)]| rs.windows(2).all(|w| w[0].1 < w[1].0);
    assert!(sorted2(crate::jdk11tables::JDK_WORD_RANGES));
    assert!(sorted2(crate::jdk11tables::JDK_MN_RANGES));
    assert!(
        crate::jdk11tables::JDK_UPPER_RANGES
            .windows(2)
            .all(|w| w[0].1 < w[1].0),
        "upper ranges must ascend"
    );
    assert!(
        crate::jdk11tables::JDK_FULL_UPPER
            .windows(2)
            .all(|w| w[0].0 < w[1].0),
        "full-upper keys must ascend"
    );
    for cp in 0xD800..=0xDFFF {
        let c = char::from_u32(cp).unwrap_or('\u{FFFD}');
        assert!(
            !jdk_is_letter_or_digit(c) && !jdk_is_mn(c) && jdk_upper_simple(c) == c,
            "no table may cover the surrogate U+{cp:04X}"
        );
    }
    assert!(!jdk_is_letter_or_digit('\u{FFFD}') && !jdk_is_mn('\u{FFFD}'));
}

#[test]
fn boundary_guards() {
    assert!(
        has_year("Show 2019"),
        "hasYear should accept a boundary-delimited year"
    );
    assert!(
        has_year("Show.2019.1080p"),
        "hasYear should accept a separator-delimited year"
    );
    assert!(
        !has_year("2019\u{5E74}"),
        "2019年: CJK letter is a word char, so Java sees no boundary after the year"
    );
    assert!(
        !has_year("2019\u{2028}"),
        "a line terminator blocks the surrounding .* in hasYear"
    );
    assert!(
        alt_head_bad("Show Season"),
        "altHeadBad should reject a trailing Season word"
    );
    assert!(
        !alt_head_bad("Show \u{17F}eason"),
        "altHeadBad fold is ASCII-only"
    );
    assert!(
        !alt_head_bad("Show Season\u{2028}"),
        "altHeadBad must fail on a line terminator"
    );
    assert!(
        year_lead("2019 x"),
        "yearLead should accept trailing content after a boundary"
    );
    assert!(
        !year_lead("2019\u{5E74}"),
        "yearLead boundary after 2019 fails before a CJK letter"
    );
}

/// Pins every row of `tools/naming-report.md` section 6. Each expectation was
/// verified byte-identical against the installed JDK (11.0.32) running the
/// frozen `Main.clean` over the same input.
#[test]
fn clean_section6() {
    let cases: [CleanCase; 10] = [
        (
            "Breaking.Bad.S02E08.Better.Call.Saul.1080p.BluRay.x265-CtrlHD.mkv",
            (
                "Breaking Bad",
                "S2 E8",
                "Better Call Saul",
                "Breaking Bad \u{2014} S2 E8 \"Better Call Saul\"",
            ),
        ),
        (
            "S01E03-The Rookie Fire Soldier Games [0CCBF9D4].mkv",
            (
                "The Rookie Fire Soldier Games",
                "S1 E3",
                "",
                "The Rookie Fire Soldier Games \u{2014} S1 E3",
            ),
        ),
        (
            "The Amazing World of Gumball S02E13E14  The Skull - The Bet.mkv",
            (
                "The Amazing World of Gumball",
                "S2 E13-14",
                "The Skull - The Bet",
                "The Amazing World of Gumball \u{2014} S2 E13-14 \"The Skull - The Bet\"",
            ),
        ),
        (
            "[Kosaka] Serial Experiments Lain - 01x09 - Layer09 PROTOCOL.mkv",
            (
                "Serial Experiments Lain",
                "S1 E9",
                "Layer09 PROTOCOL",
                "Serial Experiments Lain \u{2014} S1 E9 \"Layer09 PROTOCOL\"",
            ),
        ),
        (
            "Arcane. Season 01. Episode 05. By Wild_Cat.mkv",
            (
                "Arcane",
                "S1 E5",
                "By Wild Cat",
                "Arcane \u{2014} S1 E5 \"By Wild Cat\"",
            ),
        ),
        (
            "Monster - Chapter 01 - Herr Dr. Tenma.mkv",
            (
                "Monster",
                "E1",
                "Herr Dr Tenma",
                "Monster \u{2014} E1 \"Herr Dr Tenma\"",
            ),
        ),
        (
            "[DB]Saiki Kusuo no Psi-nan_-_NCOP01_(10bit_BD1080p_x265).mkv",
            (
                "Saiki Kusuo no Psi-nan",
                "OP 1",
                "",
                "Saiki Kusuo no Psi-nan \u{2014} OP 1",
            ),
        ),
        (
            "The.Boys.S05E04.1080p.WEB.h264-ETHEL.mkv",
            ("The Boys", "S5 E4", "", "The Boys \u{2014} S5 E4"),
        ),
        (
            "Deleted and Extended Scenes.mkv",
            (
                "Deleted and Extended Scenes",
                "",
                "",
                "Deleted and Extended Scenes",
            ),
        ),
        (
            "[2ndfire] Tengen Toppa Gurren Lagann - 26 - Let's Go, Comrades! [145E4A7F].mkv",
            (
                "Tengen Toppa Gurren Lagann",
                "E26",
                "Let's Go, Comrades!",
                "Tengen Toppa Gurren Lagann \u{2014} E26 \"Let's Go, Comrades!\"",
            ),
        ),
    ];
    for (input, want) in cases {
        assert_cleaned(input, want);
    }
}

/// The P2-P9 episode rules from `tools/naming-report.md` section 5, in report
/// order: MULTI before SERIES, EPWORD before ALT_EP, the CHAPTER / TRAIL year
/// guards and the ALT_EP connector / year guards.
#[test]
fn clean_episode_patterns() {
    let cases: [CleanCase; 13] = [
        (
            "Show S1950E04E05 Name.mkv",
            (
                "Show",
                "S1950 E4-5",
                "Name",
                "Show \u{2014} S1950 E4-5 \"Name\"",
            ),
        ),
        (
            "Show S1950E04.mkv",
            ("Show", "S1950 E4", "", "Show \u{2014} S1950 E4"),
        ),
        ("S02E07.mkv", ("S2 E7", "S2 E7", "", "S2 E7")),
        ("Episode 104.mkv", ("E104", "E104", "", "E104")),
        (
            "BLEACH Kai - Episode 38 - FEAR FOR FIGHT.mkv",
            (
                "BLEACH Kai",
                "E38",
                "FEAR FOR FIGHT",
                "BLEACH Kai \u{2014} E38 \"FEAR FOR FIGHT\"",
            ),
        ),
        (
            "Show Ep 12 Finale.mkv",
            ("Show", "E12", "Finale", "Show \u{2014} E12 \"Finale\""),
        ),
        (
            "Arcane Season 01 05 By Wild Cat.mkv",
            (
                "Arcane",
                "S1 E5",
                "By Wild Cat",
                "Arcane \u{2014} S1 E5 \"By Wild Cat\"",
            ),
        ),
        (
            "Season 03 - Stop-Motion Lego Intro.mkv",
            ("Stop-Motion Lego Intro", "", "", "Stop-Motion Lego Intro"),
        ),
        (
            "John Wick Chapter 1 2014 1080p BluRay HEVC x265 5.1 BONE.mkv",
            (
                "John Wick Chapter 1 2014 BONE",
                "",
                "",
                "John Wick Chapter 1 2014 BONE",
            ),
        ),
        (
            "Podcast 07.mkv",
            ("Podcast", "E7", "", "Podcast \u{2014} E7"),
        ),
        (
            "Show 05 2019.mkv",
            ("Show 05 (2019)", "", "", "Show 05 (2019)"),
        ),
        (
            "The Hunger Games Mockingjay Part 2 Extended.mkv",
            (
                "The Hunger Games Mockingjay Part 2 Extended",
                "",
                "",
                "The Hunger Games Mockingjay Part 2 Extended",
            ),
        ),
        ("Apollo 13.mkv", ("Apollo 13", "", "", "Apollo 13")),
    ];
    for (input, want) in cases {
        assert_cleaned(input, want);
    }
}

/// Pins the P10 TAGS vocab additions/broadenings/removals and the P11
/// SCENE_HINT additions from `tools/naming-report.md` section 5.
#[test]
fn clean_tag_vocab_p10_p11() {
    let cases: [CleanCase; 9] = [
        (
            "The.Dictator.2012.UNRATED.1080p.BluRay.AV1.Opus.5.1-RAV1NE.mkv",
            ("The Dictator (2012)", "", "", "The Dictator (2012)"),
        ),
        (
            "Arcane.S01E03.HDR10+.AV1.WEB-DL.NF-SomniWare.mkv",
            ("Arcane", "S1 E3", "", "Arcane \u{2014} S1 E3"),
        ),
        (
            "Dune.Part.Two.2024.2160p.WEB-DL.HDR.DDP5.1.Atmos.x265.mkv",
            ("Dune Part Two (2024)", "", "", "Dune Part Two (2024)"),
        ),
        (
            "War.Dogs.2016.AAC5.1.BluRay.x264-YTS.mkv",
            ("War Dogs (2016)", "", "", "War Dogs (2016)"),
        ),
        (
            "The Final Battle.mkv",
            ("The Final Battle", "", "", "The Final Battle"),
        ),
        (
            "Avatar.2009.Extended.Cut.1080p.BluRay.x264.mkv",
            ("Avatar (2009)", "", "", "Avatar (2009)"),
        ),
        (
            "Avatar.2009.Theatrical.Cut.1080p.BluRay.x264.mkv",
            ("Avatar (2009)", "", "", "Avatar (2009)"),
        ),
        (
            "Original Japanese Theatrical Trailers.mkv",
            (
                "Original Japanese Theatrical Trailers",
                "",
                "",
                "Original Japanese Theatrical Trailers",
            ),
        ),
        (
            "Ma.2019.1080p.BluRay.mkv",
            ("Ma (2019)", "", "", "Ma (2019)"),
        ),
    ];
    for (input, want) in cases {
        assert_cleaned(input, want);
    }
}

/// Deviation P13: the character a scene release leaves where Windows forbids
/// `:` folds back to `:` inside an extracted title -- but a mark that ends the
/// title is kept, since no scene convention distinguishes it from a real
/// question mark once the parenthesised tail is gone.
#[test]
fn clean_title_mark_fold() {
    let cases: [NamedCleanCase; 5] = [
        (
            "live spider-man filename",
            "Spider-Man? Brand New Day (2026) (1080p MA WEB-DL x265 EAC3 Silence).mkv",
            (
                "Spider-Man: Brand New Day",
                "",
                "",
                "Spider-Man: Brand New Day",
            ),
        ),
        (
            "no space after the mark",
            "Spider-Man?Brand New Day (2026).mkv",
            (
                "Spider-Man:Brand New Day",
                "",
                "",
                "Spider-Man:Brand New Day",
            ),
        ),
        (
            "fullwidth lookalike",
            "Spider-Man\u{FF1F}Brand New Day (2026).mkv",
            (
                "Spider-Man:Brand New Day",
                "",
                "",
                "Spider-Man:Brand New Day",
            ),
        ),
        (
            "trailing mark",
            "Spider-Man? (2026) (1080p MA WEB-DL x265 EAC3 Silence).mkv",
            ("Spider-Man?", "", "", "Spider-Man?"),
        ),
        (
            "real question mark",
            "Who Framed Roger Rabbit? (1988).mkv",
            (
                "Who Framed Roger Rabbit?",
                "",
                "",
                "Who Framed Roger Rabbit?",
            ),
        ),
    ];
    for (name, input, want) in cases {
        let got = clean(input);
        assert_eq!(
            (
                got.title.as_str(),
                got.episode_tag.as_str(),
                got.episode_name.as_str(),
                got.display.as_str()
            ),
            want,
            "{name}: clean({input:?}) = {got:?}"
        );
    }
}

/// Pins the P1 trailing-separator strip and the P12 empty-title fallback. The
/// ED 03 row is the deliberate Go-side deviation: the frozen cleaner yields
/// "ED 3 - ED 3" there, the port shows the tag alone per the report.
#[test]
fn clean_p1_p12() {
    let cases: [CleanCase; 2] = [
        (
            "[DB]Saiki Kusuo no Psi-nan 2_-_NCED02_(10bit_BD1080p_x265).mkv",
            (
                "Saiki Kusuo no Psi-nan 2",
                "ED 2",
                "",
                "Saiki Kusuo no Psi-nan 2 \u{2014} ED 2",
            ),
        ),
        ("ED 03.mkv", ("ED 3", "ED 3", "", "ED 3")),
    ];
    for (input, want) in cases {
        assert_cleaned(input, want);
    }
}

/// `export_test.go`: the package's public surface. Java parses the numbers with
/// `Integer.parseInt`, so S01 -> S1.
#[test]
fn clean_entry_point() {
    let c = clean("Show.S01E02.Pilot.1080p.mkv");
    assert_eq!(
        (
            c.title.as_str(),
            c.episode_tag.as_str(),
            c.episode_name.as_str()
        ),
        ("Show", "S1 E2", "Pilot"),
        "{c:?}"
    );
    assert_eq!(c.display, "Show \u{2014} S1 E2 \"Pilot\"");
}

#[test]
fn trunc_exported() {
    let cases: [(&str, usize, &str); 4] = [
        ("short", 10, "short"),
        ("abcdef", 4, "abc\u{2026}"),
        ("", 5, ""),
        ("AAAA\u{1D54F}B", 6, "AAAA?\u{2026}"), // split surrogate pair -> '?'
    ];
    for (input, n, want) in cases {
        assert_eq!(crate::trunc(input, n), want, "Trunc({input:?},{n})");
    }
}

/// `tabledump_test.go`: a verification helper, not a parity test. It is skipped
/// unless `TABLEDUMP` is set, and writes to the temp directory rather than the
/// working directory so a manual run leaves no artifact in the repo.
#[test]
fn table_dump() {
    use std::io::Write;

    if std::env::var("TABLEDUMP").is_err() {
        return;
    }
    let dir = std::env::temp_dir();
    let mut is_word = std::io::BufWriter::new(
        std::fs::File::create(dir.join("td.isword.tsv")).expect("create td.isword.tsv"),
    );
    let mut mn = std::io::BufWriter::new(
        std::fs::File::create(dir.join("td.mn.tsv")).expect("create td.mn.tsv"),
    );
    let mut upper = std::io::BufWriter::new(
        std::fs::File::create(dir.join("td.upper.tsv")).expect("create td.upper.tsv"),
    );
    let mut exp = std::io::BufWriter::new(
        std::fs::File::create(dir.join("td.exp.tsv")).expect("create td.exp.tsv"),
    );
    for cp in 0u32..=0xFFFF {
        let c = char::from_u32(cp).unwrap_or('\u{FFFD}');
        let word = u8::from(jdk_is_letter_or_digit(c));
        writeln!(is_word, "{cp:04X}\t{word}").expect("write isword");
        let mark = u8::from(jdk_is_mn(c));
        writeln!(mn, "{cp:04X}\t{mark}").expect("write mn");
        if (0xD800..=0xDFFF).contains(&cp) {
            continue;
        }
        let s = match jdk_full_upper(c) {
            Some(full) => full.to_string(),
            None => jdk_upper_simple(c).to_string(),
        };
        let hex: String = s.bytes().map(|b| format!("{b:02X}")).collect();
        writeln!(upper, "{cp:04X}\t{hex}").expect("write upper");
    }
    for cp in 0u32..=0xFFFF {
        let Some(c) = char::from_u32(cp) else {
            continue;
        };
        if let Some(s) = jdk_full_upper(c) {
            let hex: String = s.bytes().map(|b| format!("{b:02X}")).collect();
            writeln!(exp, "{cp:04X}\t{hex}").expect("write exp");
        }
    }
}
