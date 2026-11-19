
use crate::alternate::alternate_query;
use crate::parse::parse_raw;
use crate::types::{Kind, Query};

#[test]
fn alternate_query_cases() {
    let cases: &[(&str, Query, &str, bool)] = &[
        (
            "arc title yields colon form",
            Query {
                kind: Kind::SERIES,
                title: "Dragon Ball Super - Beerus".into(),
                episode: 1,
                absolute: true,
                ..Query::default()
            },
            "Dragon Ball Super: Beerus",
            true,
        ),
        (
            "arc suffix with slashes",
            Query {
                kind: Kind::SERIES,
                title: "Fate/stay night - Unlimited Blade Works".into(),
                episode: 1,
                absolute: true,
                ..Query::default()
            },
            "Fate/stay night: Unlimited Blade Works",
            true,
        ),
        (
            "every separator normalized",
            Query {
                kind: Kind::SERIES,
                title: "Show - Arc - Part".into(),
                season: 1,
                episode: 2,
                ..Query::default()
            },
            "Show: Arc: Part",
            true,
        ),
        (
            "no separator yields none",
            Query {
                kind: Kind::SERIES,
                title: "Dragon Ball Super".into(),
                episode: 1,
                absolute: true,
                ..Query::default()
            },
            "",
            false,
        ),
        (
            "movie never alternates",
            Query {
                kind: Kind::MOVIE,
                title: "Dune - Part Two".into(),
                ..Query::default()
            },
            "",
            false,
        ),
        (
            "unknown never alternates",
            Query {
                kind: Kind::UNKNOWN,
                title: "Show - Arc".into(),
                ..Query::default()
            },
            "",
            false,
        ),
        (
            "empty title yields none",
            Query {
                kind: Kind::SERIES,
                title: String::new(),
                episode: 1,
                absolute: true,
                ..Query::default()
            },
            "",
            false,
        ),
        (
            "whitespace title yields none",
            Query {
                kind: Kind::SERIES,
                title: "   ".into(),
                episode: 1,
                absolute: true,
                ..Query::default()
            },
            "",
            false,
        ),
    ];
    for (name, q, want_title, want_ok) in cases {
        let alt = alternate_query(q);
        assert_eq!(alt.is_some(), *want_ok, "alternateQuery({q:?}) ok [{name}]");
        let Some(alt) = alt else {
            continue;
        };
        assert_eq!(alt.title, *want_title, "alternate title [{name}]");
        assert_eq!(
            (alt.kind, alt.year, alt.season, alt.episode, alt.absolute),
            (q.kind, q.year, q.season, q.episode, q.absolute),
            "alternate = {alt:?}, want non-title fields of {q:?}"
        );
    }
}

/// The marker-derived movie query from
/// "[Anime Time] Dragon Ball Super Movie 01 - Broly.mkv" is a movie with no
/// season/episode, and a movie never alternates -- so the series fallback is
/// untouched and the miss is a clean negative.
#[test]
fn movie_marker_query_never_alternates() {
    let q = parse_raw("[Anime Time] Dragon Ball Super Movie 01 - Broly.mkv");
    assert_eq!(
        (q.kind, q.title.as_str(), q.season, q.episode),
        (Kind::MOVIE, "Dragon Ball Super: Broly", 0, 0)
    );
    assert!(
        alternate_query(&q).is_none(),
        "alternateQuery({q:?}) = Some, want None (movies never alternate)"
    );
}
