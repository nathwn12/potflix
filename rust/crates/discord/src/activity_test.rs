
use serde_json::{Map, Value};

use crate::activity::{new_activity, Activity, ActivityButton};
use crate::pipe_test::connect_pair;
use crate::{wire_json, Config, OP_FRAME};

fn decode_frame(payload: &[u8]) -> Map<String, Value> {
    serde_json::from_slice(payload)
        .unwrap_or_else(|e| panic!("unmarshal {}: {e}", String::from_utf8_lossy(payload)))
}

fn want_keys(where_: &str, m: &Map<String, Value>, keys: &[&str]) {
    if m.len() != keys.len() {
        panic!("{where_} keys = {:?}, want exactly {keys:?}", keys_of(m));
    }
    for k in keys {
        if !m.contains_key(*k) {
            panic!("{where_} missing key {k:?} (has {:?})", keys_of(m));
        }
    }
}

fn keys_of(m: &Map<String, Value>) -> Vec<String> {
    m.keys().cloned().collect()
}

fn as_map<'a>(v: &'a Value, where_: &str) -> &'a Map<String, Value> {
    v.as_object()
        .unwrap_or_else(|| panic!("{where_} = {v}, want object"))
}

/// `args.activity` of a decoded command frame.
fn activity_of(top: &Map<String, Value>) -> &Map<String, Value> {
    let args = as_map(&top["args"], "args");
    as_map(&args["activity"], "activity")
}

#[test]
fn test_set_activity_payload() {
    let (c, sc, _) = connect_pair(0, Config::default());

    let act = Activity {
        details: "Show".to_string(),
        state: r#"S1 E2 "Pilot""#.to_string(),
        small_image: "play".to_string(),
        has_start: true,
        start: 12345,
        ..Activity::default()
    };
    c.set_activity(act)
        .unwrap_or_else(|e| panic!("SetActivity: {e}"));
    let (op, payload) = sc
        .read_frame()
        .unwrap_or_else(|e| panic!("read activity frame: {e}"));
    assert_eq!(op, OP_FRAME, "activity opcode");

    let top = decode_frame(&payload);
    want_keys("command", &top, &["cmd", "args", "nonce"]);
    assert_eq!(top["cmd"].as_str(), Some("SET_ACTIVITY"), "cmd");
    assert!(
        !top["nonce"].as_str().unwrap_or("").is_empty(),
        "nonce missing in {}",
        String::from_utf8_lossy(&payload)
    );

    let args = as_map(&top["args"], "args");
    want_keys("args", args, &["pid", "activity"]);
    assert_eq!(
        args["pid"].as_i64(),
        Some(i64::from(std::process::id())),
        "pid"
    );

    let a = activity_of(&top);
    want_keys(
        "activity",
        a,
        &["type", "details", "state", "timestamps"],
    );
    assert_eq!(a["type"].as_i64(), Some(3), "type (WATCHING)");
    assert_eq!(a["details"].as_str(), Some("Show"), "details");
    assert_eq!(a["state"].as_str(), Some(r#"S1 E2 "Pilot""#), "state");

    let ts = as_map(&a["timestamps"], "timestamps");
    want_keys("timestamps", ts, &["start"]);
    assert_eq!(ts["start"].as_i64(), Some(12345), "timestamps.start");

    // Empty text is OMITTED, not sent as "" (Discord rejects an empty large_text
    // with code 4000). The caller set a small image and no resolvable artwork,
    // so no asset field has a value: no `assets` key at all. A caller-set small
    // icon must never reach the wire, and the branded `logo` is never a
    // missing-art fallback.
    assert!(
        !a.contains_key("assets"),
        "a caller-set small icon or a branded fallback reached the wire: {a:?}"
    );
    sc.close();
}

#[test]
fn test_set_activity_text_present_when_non_empty() {
    // The wire object omits empty large text but must still carry non-empty
    // large text when a caller sets it. Small text is never carried, even when
    // set: the pristine card has no status icon slot.
    let p = new_activity(&Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        large_text: "A synopsis".to_string(),
        small_image: "play".to_string(),
        ..Activity::default()
    });
    let b = serde_json::to_vec(&p).unwrap_or_else(|e| panic!("marshal: {e}"));

    let a = decode_frame(&b);
    let assets = as_map(&a["assets"], "assets");
    want_keys("assets", assets, &["large_text"]);
    assert_eq!(assets["large_text"].as_str(), Some("A synopsis"), "large_text");
    assert!(
        !assets.contains_key("small_image"),
        "small_image present, want omitted (pristine card): {assets:?}"
    );
}

#[test]
fn test_large_image_https_honored_verbatim() {
    // An https LargeImage is honored verbatim in the wire JSON. The caller also
    // sets SmallImage, which must not appear: no icon keys on the wire.
    const ART: &str = "https://static.tvmaze.com/uploads/images/medium_portrait/1/2.jpg";
    let got = wire_json(&Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        large_image: ART.to_string(),
        small_image: "play".to_string(),
        ..Activity::default()
    });
    let want = format!(
        r#"{{"type":3,"details":"Show","state":"S1 E2","assets":{{"large_image":"{ART}"}}}}"#
    );
    assert_eq!(got, want, "WireJSON");
}

#[test]
fn test_large_image_missing_or_invalid_sends_no_image() {
    // Empty or non-http(s) LargeImage sends NO image: the branded `logo` is
    // never a missing-art fallback (owner decision 2026-10-28). The caller's
    // small icon must not keep the assets object alive either.
    for img in [
        "",
        "logo",
        "not-a-url",
        "ftp://example.com/a.jpg",
        "data:image/png;base64,xx",
    ] {
        let got = wire_json(&Activity {
            details: "Show".to_string(),
            small_image: "play".to_string(),
            large_image: img.to_string(),
            ..Activity::default()
        });
        let a = decode_frame(got.as_bytes());
        assert!(
            !a.contains_key("assets"),
            "LargeImage {img:?}: a caller-set small icon or branded fallback \
             reached the wire: {got}"
        );
    }
    let bare = wire_json(&Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        ..Activity::default()
    });
    assert!(
        !bare.contains(r#""assets""#),
        "a card with no artwork/text must send no assets key: {bare}"
    );
    assert!(!bare.contains("logo"), "branded fallback on the wire: {bare}");
    // http (non-s) is also honored verbatim, and still with no small icon.
    const HTTP_ART: &str = "http://example.com/cover.jpg";
    let got = wire_json(&Activity {
        details: "Show".to_string(),
        small_image: "play".to_string(),
        large_image: HTTP_ART.to_string(),
        ..Activity::default()
    });
    let a = decode_frame(got.as_bytes());
    let assets = as_map(&a["assets"], "assets");
    assert_eq!(
        assets["large_image"].as_str(),
        Some(HTTP_ART),
        "http LargeImage not honored: {got}"
    );
    assert!(
        !assets.contains_key("small_image"),
        "small_image present, want omitted (pristine card): {assets:?}"
    );
}

/// Discord rejects `large_image` longer than 300 chars (`code=4000`), which a
/// Kitsu-signed Backblaze URL can exceed. The wire composer must omit the
/// over-budget image entirely (no branded fallback), never truncate it and
/// never emit the oversized field.
#[test]
fn test_large_image_over_300_is_omitted() {
    let long_art = format!("https://example.com/{}", "x".repeat(400));
    assert!(long_art.len() > 300, "fixture must exceed the Discord cap");
    let got = wire_json(&Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        large_image: long_art.clone(),
        ..Activity::default()
    });
    let a = decode_frame(got.as_bytes());
    assert!(
        !a.contains_key("assets"),
        "over-budget LargeImage must not emit an assets object, got {got}"
    );
    assert!(!got.contains("logo"), "branded fallback on the wire: {got}");
    let edge = format!(
        "https://example.com/{}",
        "y".repeat(300 - "https://example.com/".len())
    );
    assert_eq!(edge.len(), 300, "edge fixture must be exactly 300 chars");
    let got = wire_json(&Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        large_image: edge.clone(),
        ..Activity::default()
    });
    let a = decode_frame(got.as_bytes());
    assert_eq!(
        as_map(&a["assets"], "assets")["large_image"].as_str(),
        Some(edge.as_str()),
        "300-char LargeImage must pass through, got {got}"
    );
}

#[test]
fn test_set_activity_no_timer() {
    let (c, sc, _) = connect_pair(0, Config::default());

    c.set_activity(Activity {
        details: "Browsing".to_string(),
        state: "Idle".to_string(),
        small_image: "stop".to_string(),
        ..Activity::default()
    })
    .unwrap_or_else(|e| panic!("SetActivity: {e}"));
    let (_, payload) = sc
        .read_frame()
        .unwrap_or_else(|e| panic!("read activity frame: {e}"));

    let top = decode_frame(&payload);
    let a = activity_of(&top);
    want_keys("activity", a, &["type", "details", "state"]);
    assert_eq!(a["details"].as_str(), Some("Browsing"), "details");
    assert_eq!(a["state"].as_str(), Some("Idle"), "state");
    assert!(
        !a.contains_key("assets"),
        "a caller-set small icon reached the idle wire: {a:?}"
    );
    sc.close();
}

#[test]
fn test_set_activity_playing_carries_start_and_end() {
    let (c, sc, _) = connect_pair(0, Config::default());

    c.set_activity(Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        small_image: "play".to_string(),
        has_start: true,
        start: 111,
        end: 222,
        ..Activity::default()
    })
    .unwrap_or_else(|e| panic!("SetActivity: {e}"));
    let (_, payload) = sc
        .read_frame()
        .unwrap_or_else(|e| panic!("read activity frame: {e}"));

    let top = decode_frame(&payload);
    let a = activity_of(&top);
    assert_eq!(a["type"].as_i64(), Some(3), "type (WATCHING)");
    let ts = as_map(&a["timestamps"], "timestamps");
    want_keys("timestamps", ts, &["start", "end"]);
    assert_eq!(ts["start"].as_i64(), Some(111), "timestamps.start");
    assert_eq!(ts["end"].as_i64(), Some(222), "timestamps.end");
    sc.close();
}

#[test]
fn test_set_activity_paused_start_only_no_end() {
    let (c, sc, _) = connect_pair(0, Config::default());

    c.set_activity(Activity {
        details: "Show".to_string(),
        state: r#"S1 E2 "Pilot" - Paused"#.to_string(),
        small_image: "pause".to_string(),
        has_start: true,
        start: 5000,
        ..Activity::default()
    })
    .unwrap_or_else(|e| panic!("SetActivity: {e}"));
    let (_, payload) = sc
        .read_frame()
        .unwrap_or_else(|e| panic!("read activity frame: {e}"));
    assert!(
        !String::from_utf8_lossy(&payload).contains(r#""end""#),
        "paused payload carries end: {}",
        String::from_utf8_lossy(&payload)
    );

    let top = decode_frame(&payload);
    let a = activity_of(&top);
    assert_eq!(a["type"].as_i64(), Some(3), "type (WATCHING)");
    let ts = as_map(&a["timestamps"], "timestamps");
    want_keys("timestamps", ts, &["start"]);
    assert_eq!(ts["start"].as_i64(), Some(5000), "timestamps.start");
    sc.close();
}

#[test]
fn test_wire_json_mapping() {
    // The Netflix-style card on the exact wire object: header `name`, two body
    // lines (`details`, `state`), brand text, and no buttons/party/secrets - and
    // no small icon keys. Callers still set SmallImage/SmallText; the wire drops
    // them.
    let cases: Vec<(&str, Activity, &str)> = vec![
        (
            "movie",
            Activity {
                name: "Movie".to_string(),
                details: "Movie".to_string(),
                large_text: "PotFlix".to_string(),
                small_image: "play".to_string(),
                small_text: "Playing".to_string(),
                has_start: true,
                start: 12345,
                end: 67890,
                ..Activity::default()
            },
            r#"{"type":3,"name":"Movie","details":"Movie","timestamps":{"start":12345,"end":67890},"assets":{"large_text":"PotFlix"}}"#,
        ),
        (
            "tv with episode name and tag",
            Activity {
                name: "Show".to_string(),
                details: "Pilot".to_string(),
                state: "S1 E2".to_string(),
                large_text: "PotFlix".to_string(),
                small_image: "play".to_string(),
                small_text: "Playing".to_string(),
                has_start: true,
                start: 111,
                end: 222,
                ..Activity::default()
            },
            r#"{"type":3,"name":"Show","details":"Pilot","state":"S1 E2","timestamps":{"start":111,"end":222},"assets":{"large_text":"PotFlix"}}"#,
        ),
        (
            "tv with tag only",
            Activity {
                name: "Show".to_string(),
                details: "S1 E2".to_string(),
                large_text: "PotFlix".to_string(),
                small_image: "play".to_string(),
                small_text: "Playing".to_string(),
                has_start: true,
                start: 111,
                end: 222,
                ..Activity::default()
            },
            r#"{"type":3,"name":"Show","details":"S1 E2","timestamps":{"start":111,"end":222},"assets":{"large_text":"PotFlix"}}"#,
        ),
        (
            "paused",
            Activity {
                name: "Show".to_string(),
                details: "Pilot".to_string(),
                state: "S1 E2 - Paused".to_string(),
                large_text: "PotFlix".to_string(),
                small_image: "pause".to_string(),
                small_text: "Paused".to_string(),
                has_start: true,
                start: 5000,
                ..Activity::default()
            },
            r#"{"type":3,"name":"Show","details":"Pilot","state":"S1 E2 - Paused","timestamps":{"start":5000},"assets":{"large_text":"PotFlix"}}"#,
        ),
        (
            "idle",
            Activity {
                details: "Browsing".to_string(),
                state: "Idle".to_string(),
                large_text: "PotFlix".to_string(),
                small_image: "stop".to_string(),
                small_text: "Idle".to_string(),
                ..Activity::default()
            },
            r#"{"type":3,"details":"Browsing","state":"Idle","assets":{"large_text":"PotFlix"}}"#,
        ),
    ];
    for (name, act, want) in cases {
        let got = wire_json(&act);
        assert_eq!(got, want, "{name}: WireJSON");
    }
}

#[test]
fn test_clear_activity_is_null() {
    let (c, sc, _) = connect_pair(0, Config::default());

    c.clear_activity()
        .unwrap_or_else(|e| panic!("ClearActivity: {e}"));
    let (op, payload) = sc
        .read_frame()
        .unwrap_or_else(|e| panic!("read clear frame: {e}"));
    assert_eq!(op, OP_FRAME, "clear opcode");
    assert!(
        String::from_utf8_lossy(&payload).contains(r#""activity":null"#),
        "clear payload = {}, want activity null",
        String::from_utf8_lossy(&payload)
    );

    let top = decode_frame(&payload);
    want_keys("command", &top, &["cmd", "args", "nonce"]);
    let args = as_map(&top["args"], "args");
    want_keys("args", args, &["pid", "activity"]);
    assert!(
        args["activity"].is_null(),
        "activity = {}, want null",
        args["activity"]
    );
    sc.close();
}

#[test]
fn test_wire_json_buttons_always_omitted() {
    // Pristine card: even fully valid buttons never reach the wire - the field is
    // omitted, not sent as null or [].
    let got = wire_json(&Activity {
        details: "Show".to_string(),
        state: "S1 E2".to_string(),
        small_image: "play".to_string(),
        buttons: vec![
            ActivityButton {
                label: "View Episode".to_string(),
                url: "https://example.com/ep/1".to_string(),
            },
            ActivityButton {
                label: "IMDb".to_string(),
                url: "https://example.com/imdb".to_string(),
            },
        ],
        ..Activity::default()
    });
    assert!(
        !got.contains(r#""buttons""#),
        "valid buttons present, want omitted: {got}"
    );
    for buttons in [Vec::new(), vec![]] {
        let got = wire_json(&Activity {
            details: "Show".to_string(),
            state: "S1 E2".to_string(),
            small_image: "play".to_string(),
            buttons,
            ..Activity::default()
        });
        assert!(
            !got.contains(r#""buttons""#),
            "empty buttons present, want omitted: {got}"
        );
    }
}

/// The clean-card proof: a full playing card with every legacy field set (icons,
/// texts, buttons) serializes to exactly the pristine object - header, two body
/// lines, time bar, brand text - with no buttons key and no small_image/small_text
/// keys anywhere.
#[test]
fn test_wire_json_pristine_golden() {
    let got = wire_json(&Activity {
        name: "Show".to_string(),
        details: "Pilot".to_string(),
        state: "S1 E2".to_string(),
        large_image: "https://example.com/poster.jpg".to_string(),
        large_text: "PotFlix".to_string(),
        small_image: "play".to_string(),
        small_text: "Playing".to_string(),
        has_start: true,
        start: 111,
        end: 222,
        buttons: vec![ActivityButton {
            label: "Trailer".to_string(),
            url: "https://example.com/trailer".to_string(),
        }],
    });
    let want = r#"{"type":3,"name":"Show","details":"Pilot","state":"S1 E2","timestamps":{"start":111,"end":222},"assets":{"large_image":"https://example.com/poster.jpg","large_text":"PotFlix"}}"#;
    assert_eq!(got, want, "WireJSON");
    for key in [r#""buttons""#, r#""small_image""#, r#""small_text""#] {
        assert!(!got.contains(key), "pristine payload carries {key}: {got}");
    }
}

/// Owner regression (2026-10-28): the payload header is the media title -- the
/// client renders `Watching <title>` -- so it can never read `Watching Potflix`,
/// and a card whose artwork did not resolve sends NO assets at all: no `logo`
/// fallback standing in for the poster, and no status icon over the art.
#[test]
fn test_header_is_the_media_title_and_missing_art_sends_no_assets() {
    let movie = wire_json(&Activity {
        name: "Dune: Part Two".to_string(),
        details: "Dune: Part Two (2024)".to_string(),
        state: "Sci-Fi, Adventure \u{b7} 2h 46m".to_string(),
        ..Activity::default()
    });
    let a = decode_frame(movie.as_bytes());
    assert_eq!(a["type"].as_i64(), Some(3), "type (WATCHING)");
    assert_eq!(
        a["name"].as_str(),
        Some("Dune: Part Two"),
        "header name: {movie}"
    );
    assert_eq!(
        format!("Watching {}", a["name"].as_str().unwrap_or("")),
        "Watching Dune: Part Two",
        "the client header: {movie}"
    );
    assert!(
        !movie.contains("Potflix"),
        "the header or assets mention the app name: {movie}"
    );
    assert!(
        !movie.contains(r#""assets""#),
        "a missing-art card must send no assets key: {movie}"
    );
    assert!(
        !movie.contains(r#""logo""#),
        "branded logo fallback on the wire: {movie}"
    );
    assert!(
        !movie.contains(r#""small_image""#) && !movie.contains(r#""small_text""#),
        "a status icon rode the wire: {movie}"
    );
}
