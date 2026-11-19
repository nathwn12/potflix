
use crate::context::{context_from_path, resolve_query, under_extras_folder};
use crate::types::{Kind, Query};

const MR_HARBOR_PATH: &str = r"M:\1ARCHIVE\Mr.Harbor.COMPLETE.1080p.BluRay.AV1.DDP.5.1-dAV1nci\Mr.Harbor.S01.1080p.BluRay.AV1.DDP.5.1-dAV1nci\Mr.Harbor.S01E01.1080p.BluRay.AV1.DDP.5.1-dAV1nci.mkv";
const MR_HARBOR_FILE: &str = "Mr.Harbor.S01E01.1080p.BluRay.AV1.DDP.5.1-dAV1nci.mkv";
const ZENTARO_PATH: &str = r"M:\2SERIES\Zentaro\Season 1\01.mkv";

#[test]
fn context_from_path_cases() {
    let cases: &[(&str, &str, &str, i64)] = &[
        ("mr harbor release tree", MR_HARBOR_PATH, "Mr Harbor", 1),
        ("zentaro season folder", ZENTARO_PATH, "Zentaro", 1),
        ("short season leaf", r"M:\2FILMS\Show\S02\05.mkv", "Show", 2),
        (
            "generics only",
            r"M:\1ARCHIVE\Extras\Featurettes\Trailer.mkv",
            "",
            0,
        ),
        ("bare filename", "Trailer.mkv", "", 0),
        ("empty path", "", "", 0),
    ];
    for (name, path, show, season) in cases {
        let got = context_from_path(path);
        assert_eq!(
            (got.0.as_str(), got.1),
            (*show, *season),
            "ContextFromPath({path:?}) [{name}]"
        );
    }
}

/// A `Season <N>` FOLDER whose number exceeds `i32::MAX` keeps the whole
/// number, exactly as Go does.
///
/// Go's season-folder pattern is `^Season\s*(\d+)$` -- the digit run is
/// UNBOUNDED (`go\internal\metadata\context.go:21`) -- and `ContextFromPath`
/// reads it with `atoi` (`go\internal\metadata\context.go:127`), which is
/// `strconv.Atoi` (`go\internal\metadata\parse.go:200`) onto a 64-bit `int`.
/// `Query.Season` is likewise `int` (`go\internal\metadata\types.go:27`), so the
/// value survives to `ResolveQuery`'s query -- and `Absolute` stays false
/// because the season is non-zero (`go\internal\metadata\context.go:227`).
///
/// Verified against the frozen reference: a `go test` harness over an untouched
/// copy of `go\` prints `ContextFromPath("2SERIES\Show\Season 2147483648\
/// 01.mkv") = ("Show", 2147483648)` and `ResolveQuery(01.mkv, ...) = kind=1
/// title="Show" season=2147483648 episode=1 absolute=false` (the drive-less
/// spelling exercises the same pass: neither tree needs a volume component).
///
/// The port parsed that capture into `i32`, so `parse()` failed and the number
/// collapsed to 0 -- which ALSO flipped `Absolute` to true. Values above
/// `i64::MAX` clamp the way Go's dropped `ErrRange` clamps
/// (`strconv.ParseInt` returns `MaxInt64`).
#[test]
fn season_folder_above_i32_max_matches_go() {
    const PATH: &str = r"2SERIES\Show\Season 2147483648\01.mkv";
    // Go prints these with `%d`; the reference values are quoted above.
    const GO_SEASON: &str = "2147483648";
    const GO_CLAMPED: &str = "9223372036854775807";

    let (show, season) = context_from_path(PATH);
    assert_eq!(show, "Show", "ContextFromPath({PATH:?}) show");
    assert_eq!(
        season.to_string(),
        GO_SEASON,
        "ContextFromPath({PATH:?}) season: Go yields {GO_SEASON}, Rust yielded {season}"
    );

    let q = resolve_query("01.mkv", PATH);
    assert_eq!(
        (q.kind, q.title.as_str()),
        (Kind::SERIES, "Show"),
        "ResolveQuery(01.mkv, {PATH:?}) kind/title"
    );
    assert_eq!(
        q.season.to_string(),
        GO_SEASON,
        "ResolveQuery(01.mkv, {PATH:?}) season: Go yields {GO_SEASON}, Rust yielded {}",
        q.season
    );
    assert_eq!(q.episode, 1, "ResolveQuery(01.mkv, {PATH:?}) episode");
    assert!(
        !q.absolute,
        "ResolveQuery(01.mkv, {PATH:?}) absolute: Go is false (season != 0), Rust gave {}",
        q.absolute
    );

    // Above i64::MAX Go's dropped ErrRange clamps to MaxInt64, never 0.
    let over = r"2SERIES\Show\Season 99999999999999999999\01.mkv";
    let (show, season) = context_from_path(over);
    assert_eq!(show, "Show", "ContextFromPath({over:?}) show");
    assert_eq!(
        season.to_string(),
        GO_CLAMPED,
        "ContextFromPath({over:?}) season: Go clamps to {GO_CLAMPED}, Rust yielded {season}"
    );
}

#[test]
fn resolve_query_cases() {
    // bare number takes folder show
    let got = resolve_query("01.mkv", ZENTARO_PATH);
    let want = Query {
        kind: Kind::SERIES,
        title: "Zentaro".into(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    assert_eq!(got, want, "ResolveQuery(01.mkv, zentaro)");

    // extra filename stays unknown
    let got = resolve_query(
        "Deleted Scenes.mkv",
        r"M:\1ARCHIVE\Show\Extras\Deleted Scenes.mkv",
    );
    assert_eq!(
        got.kind,
        Kind::UNKNOWN,
        "ResolveQuery(Deleted Scenes.mkv, extras path) = {got:?}"
    );

    // mr harbor filename and path
    let got = resolve_query(MR_HARBOR_FILE, MR_HARBOR_PATH);
    let want = Query {
        kind: Kind::SERIES,
        title: "Mr Harbor".into(),
        season: 1,
        episode: 1,
        ..Query::default()
    };
    assert_eq!(got, want, "ResolveQuery(mrharbor, mrharbor)");

    let got = resolve_query("01.mkv", "");
    assert_eq!(got.kind, Kind::UNKNOWN, "ResolveQuery(01.mkv, empty)");

    let got = resolve_query(
        "[Studio Hour] Zentaro - OP - 001 - XA?B?C?D.mkv",
        ZENTARO_PATH,
    );
    assert_eq!(got.kind, Kind::UNKNOWN, "ResolveQuery(Zentaro OP, zentaro)");

    let got = resolve_query(
        "1.mkv",
        r"M:\2SERIES\Harbor Lights\Featurettes\Season 5\Featurettes 5.1\Deleted Scenes\1.mkv",
    );
    assert_eq!(
        got.kind,
        Kind::UNKNOWN,
        "ResolveQuery(1.mkv, deleted-scenes)"
    );

    // short season leaf fills season
    let got = resolve_query("05.mkv", r"M:\2FILMS\Show\S02\05.mkv");
    let want = Query {
        kind: Kind::SERIES,
        title: "Show".into(),
        season: 2,
        episode: 5,
        ..Query::default()
    };
    assert_eq!(got, want, "ResolveQuery(05.mkv, S02 path)");

    // bonus phrase folder stays unknown; the show above the leaf still resolves
    for leaf in ["Making Of", "Gag Reel", "Behind the Story", "TV Spots"] {
        let path = format!(r"M:\1ARCHIVE\Show\{leaf}\01.mkv");
        let got = resolve_query("01.mkv", &path);
        assert_eq!(got.kind, Kind::UNKNOWN, "ResolveQuery(01.mkv, {path:?})");
        assert!(under_extras_folder(&path), "underExtrasFolder({path:?})");
    }
    let (show, _) = context_from_path(r"M:\1ARCHIVE\Show\Gag Reel\clip.mkv");
    assert_eq!(show, "Show", "ContextFromPath(Gag Reel leaf) show");
}
