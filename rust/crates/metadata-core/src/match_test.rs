//! Ports `go\internal\metadata\match_test.go` (6 tests): the fold table, the
//! similarity range, the Re:ZERO weight tie, the confident picks, the unrelated
//! floor and the tie-breaks.

use crate::match_::{ShowCand, normalize_title, pick_best_show, title_similarity};

fn cand(name: &str, weight: i32) -> ShowCand {
    ShowCand {
        name: name.to_string(),
        weight,
        episode_count: 0,
        aliases: Vec::new(),
    }
}

#[test]
fn normalize_title_cases() {
    let cases: &[(&str, &str)] = &[
        (
            "Re:ZERO -Starting Life in Another World-",
            "re zero starting life in another world",
        ),
        ("Mr. Robot", "mr robot"),
        ("  Breaking   Bad  ", "breaking bad"),
        ("Café Pokémon", "cafe pokemon"),
        ("Cat's in the Bag...", "cat s in the bag"),
        ("[Anime Time] Re Zero", "anime time re zero"),
        ("", ""),
    ];
    for (input, want) in cases {
        assert_eq!(normalize_title(input), *want, "normalizeTitle({input:?})");
    }
}

#[test]
fn title_similarity_exact_and_range() {
    let exact: &[(&str, &str)] = &[
        ("Breaking Bad", "Breaking Bad"),
        ("Mr Robot", "Mr. Robot"),
        ("Black Bird", "Black Bird"),
        ("Naruto", "Naruto"),
    ];
    for (a, b) in exact {
        assert_eq!(title_similarity(a, b), 1.0, "titleSimilarity({a:?},{b:?})");
    }
    let unrelated: &[(&str, &str)] = &[
        ("Breaking Bad", "Re:ZERO -Starting Life in Another World-"),
        ("Re Zero", "Breaking Bad"),
        ("Black Bird", "Black Mirror"),
    ];
    for (a, b) in unrelated {
        let got = title_similarity(a, b);
        assert!(
            (0.0..=1.0).contains(&got),
            "titleSimilarity({a:?},{b:?}) = {got}, want in [0,1]"
        );
        assert!(
            got < 0.5,
            "titleSimilarity({a:?},{b:?}) = {got}, want < 0.5 (unrelated)"
        );
    }
}

/// TVmaze ranked the spinoff (weight 80) above the main show (weight 98);
/// similarity must land them in one tie group so weight decides.
#[test]
fn pick_best_show_re_zero() {
    let mut cands = vec![
        cand("Re:Zero kara Hajimeru Break Time", 80),
        cand("Re:ZERO -Starting Life in Another World-", 98),
    ];
    let best = pick_best_show("Re Zero", &cands).expect("want the main show");
    assert_eq!(best.name, "Re:ZERO -Starting Life in Another World-");
    // Order-independent: spinoff listed second still loses.
    cands.swap(0, 1);
    let best = pick_best_show("Re Zero", &cands).expect("swapped, want the main show");
    assert_eq!(best.name, "Re:ZERO -Starting Life in Another World-");
}

#[test]
fn pick_best_show_confident() {
    let cases: &[(&str, Vec<ShowCand>, &str)] = &[
        (
            "Breaking Bad",
            vec![cand("Breaking Bad", 99), cand("Breaking Good", 10)],
            "Breaking Bad",
        ),
        (
            "Mr Robot",
            vec![cand("Mr. Robot", 90), cand("Robot Wars", 50)],
            "Mr. Robot",
        ),
        (
            "Black Bird",
            vec![cand("Black Mirror", 95), cand("Black Bird", 70)],
            "Black Bird",
        ),
    ];
    for (query, cands, want) in cases {
        let best = pick_best_show(query, cands);
        assert_eq!(
            best.map(|b| b.name).as_deref(),
            Some(*want),
            "pickBestShow({query:?})"
        );
    }
}

#[test]
fn pick_best_show_unrelated() {
    let cands = vec![
        cand("Re:ZERO -Starting Life in Another World-", 98),
        cand("Naruto", 90),
    ];
    assert!(
        pick_best_show("Breaking Bad", &cands).is_none(),
        "Breaking Bad over anime, want None"
    );
    assert!(
        pick_best_show("Breaking Bad", &[]).is_none(),
        "empty candidate list, want None"
    );
    assert!(
        pick_best_show("", &[cand("Breaking Bad", 0)]).is_none(),
        "empty query, want None"
    );
}

/// Equal-similarity ties break by Weight, then by EpisodeCount.
#[test]
fn pick_best_show_tie_breaks() {
    let by_weight = vec![
        ShowCand {
            name: "Show Alpha".into(),
            weight: 10,
            episode_count: 100,
            aliases: Vec::new(),
        },
        ShowCand {
            name: "Show Beta".into(),
            weight: 90,
            episode_count: 1,
            aliases: Vec::new(),
        },
    ];
    let best = pick_best_show("Show", &by_weight).expect("weight tie-break");
    assert_eq!(best.name, "Show Beta");
    let by_eps = vec![
        ShowCand {
            name: "Show Alpha".into(),
            weight: 10,
            episode_count: 5,
            aliases: Vec::new(),
        },
        ShowCand {
            name: "Show Beta".into(),
            weight: 10,
            episode_count: 40,
            aliases: Vec::new(),
        },
    ];
    let best = pick_best_show("Show", &by_eps).expect("episode-count tie-break");
    assert_eq!(best.name, "Show Beta");
}

/// An entry's romaji alias scores like its display name: a romaji query
/// finds the English-titled main series instead of a romaji-named special,
/// and the exact-alias rule excludes the near-matching special.
#[test]
fn pick_best_show_scores_aliases() {
    let cands = vec![
        ShowCand {
            name: "Attack on Titan".into(),
            weight: 0,
            episode_count: 0,
            aliases: vec!["Shingeki no Kyojin".into()],
        },
        ShowCand {
            name: "Shingeki no Kyojin: Picture Drama".into(),
            weight: 0,
            episode_count: 0,
            aliases: Vec::new(),
        },
    ];
    let best = pick_best_show("Shingeki no Kyojin", &cands).expect("romaji alias must win");
    assert_eq!(best.name, "Attack on Titan");
}

/// Defect 6: sequel numerals fold (`Rocky 2` ≡ `Rocky II`) and unspaced
/// scripts get partial credit above the 0.5 floor (`你好，李焕英` ≡
/// `你好李焕英`).
#[test]
fn sequel_numerals_fold_and_unspaced_cjk_scores() {
    assert_eq!(
        normalize_title("Rocky II"),
        normalize_title("Rocky 2"),
        "a sequel numeral must fold to its digit"
    );
    let best = pick_best_show(
        "Rocky II",
        &[cand("Rocky 2", 0), cand("Totally Unrelated", 0)],
    )
    .expect("Rocky II must find Rocky 2");
    assert_eq!(best.name, "Rocky 2");
    let sim = title_similarity("你好，李焕英", "你好李焕英");
    assert!(
        sim > 0.5,
        "unspaced CJK must clear the floor, got {sim}"
    );
    let best = pick_best_show(
        "你好李焕英",
        &[cand("你好，李焕英", 0), cand("Breaking Bad", 0)],
    )
    .expect("unspaced CJK must pick the same work");
    assert_eq!(best.name, "你好，李焕英");
}
