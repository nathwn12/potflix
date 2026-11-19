
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// The caller-facing subset: the poll loop fills it from `presence.BuildActivity`
/// and the fixed brand fields are added here.
#[derive(Debug, Clone, Default)]
pub struct Activity {
    /// Overrides the header line ("Watching <Name>"): the shipped composer sets
    /// the media title. Empty omits `name` (the idle card).
    pub name: String,
    pub details: String,
    pub state: String,
    /// https artwork URL within Discord's 300-char limit; empty/invalid/over-budget
    /// is omitted on the wire -- never replaced by the branded `logo`.
    pub large_image: String,
    /// The synopsis snippet; empty is omitted on the wire.
    pub large_text: String,
    /// Caller-compat only: the shipped composer sets no status icon. Empty omitted.
    pub small_image: String,
    /// Caller-compat only: the shipped composer sets no status text. Empty omitted.
    pub small_text: String,
    pub has_start: bool,
    /// epoch ms, set while running and paused
    pub start: i64,
    /// epoch ms, projected end from the gate; serialized while playing (deviation)
    pub end: i64,
    /// Legacy: accepted so existing callers keep compiling, but never
    /// serialized -- the card carries no click-through links.
    pub buttons: Vec<ActivityButton>,
}

/// One card click-through link (legacy: never serialized).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityButton {
    pub label: String,
    pub url: String,
}

/// Discord's ActivityType "Watching" (3). The Java reference hardcoded 0
/// (Playing). Discord documents the activity time bar as requiring Listening (2)
/// or Watching (3); Watching fits a video player and renders "Watching <title>".
/// Deliberate, user-visible deviation (see the file header).
const ACTIVITY_TYPE_WATCHING: i32 = 3;

/// Discord caps `large_image` at 300 chars (`code=4000` otherwise). An
/// over-budget URL is omitted whole (no branded fallback), never truncated (a
/// cut signed URL is a broken link). Local copy of the presence crate's cap:
/// the crates share no edge, so the value is duplicated, not imported.
const LARGE_IMAGE_BUDGET: usize = 300;

/// The wire activity object. Every empty string field is omitted so Discord
/// never sees `"large_text":""`. An activity with no image and no asset text
/// carries no `assets` key at all (the card is clean, never branded art in a
/// missing-art slot, and no corner icon).
#[derive(Debug, Serialize)]
pub(crate) struct ActivityPayload {
    #[serde(rename = "type")]
    pub(crate) activity_type: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) details: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) timestamps: Option<Timestamps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) assets: Option<ActivityAssets>,
}

/// Carries the elapsed anchor and, while playing, the projected end. A playing
/// card carries start + end (the time bar); a paused card carries start only,
/// anchored at the pause moment, so the client counts the pause UP. Go's
/// `omitempty` on an int omits 0, so `start`/`end` are skipped when zero.
#[derive(Debug, Serialize)]
pub(crate) struct Timestamps {
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) start: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) end: i64,
}

/// Mirrors the contract's asset field names exactly; empty text is omitted on
/// the wire (see the file header). Only the large image/text ride the wire: the
/// pristine card has no small icon.
#[derive(Debug, Serialize)]
pub(crate) struct ActivityAssets {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) large_image: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) large_text: String,
}

impl ActivityAssets {
    /// Whether no asset field carries a value: the card then sends no `assets`
    /// key at all.
    fn is_empty(&self) -> bool {
        self.large_image.is_empty() && self.large_text.is_empty()
    }
}

/// The args object; a `None` activity marshals to `null`.
#[derive(Debug, Serialize)]
pub(crate) struct SetActivityArgs {
    pub(crate) pid: i32,
    pub(crate) activity: Option<ActivityPayload>,
}

/// The outer IPC command envelope.
#[derive(Debug, Serialize)]
pub(crate) struct Command {
    pub(crate) cmd: &'static str,
    pub(crate) args: SetActivityArgs,
    pub(crate) nonce: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// Maps one card to the wire payload. `type` is Watching (3); `large_image` is
/// the caller's resolved artwork when it is an http(s) URL within
/// [`LARGE_IMAGE_BUDGET`], else EMPTY -- the branded app mark is never
/// substituted for missing artwork. Empty text stays omitted, the `assets` key
/// is omitted entirely when no asset field has a value, and `name` carries the
/// media-title header override.
pub(crate) fn new_activity(a: &Activity) -> ActivityPayload {
    let large_image = if (a.large_image.starts_with("http://")
        || a.large_image.starts_with("https://"))
        && a.large_image.len() <= LARGE_IMAGE_BUDGET
    {
        a.large_image.clone()
    } else {
        String::new()
    };
    let assets = ActivityAssets {
        large_image,
        large_text: a.large_text.clone(),
    };
    let assets = if assets.is_empty() { None } else { Some(assets) };
    let mut p = ActivityPayload {
        activity_type: ACTIVITY_TYPE_WATCHING,
        name: a.name.clone(),
        details: a.details.clone(),
        state: a.state.clone(),
        timestamps: None,
        assets,
    };
    if a.has_start {
        let mut ts = Timestamps {
            start: a.start,
            end: 0,
        };
        if a.end > 0 {
            ts.end = a.end;
        }
        p.timestamps = Some(ts);
    }
    p
}

/// Renders the exact activity object `SetActivity` puts on the wire, for the
/// push-attempt log line (the observation boundary).
pub fn wire_json(a: &Activity) -> String {
    serde_json::to_string(&new_activity(a)).unwrap_or_else(|_| "{}".to_string())
}

/// `std::process::id`, isolated so tests can reason about the one dynamic field.
pub(crate) fn pid() -> i32 {
    std::process::id() as i32
}

/// Makes the nonce unique within the process; the timestamp keeps it unique
/// across restarts.
static NONCE_SEQ: AtomicI64 = AtomicI64::new(0);

/// Returns a unique nonce string.
pub(crate) fn new_nonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64);
    format!("{}-{}", nanos, NONCE_SEQ.fetch_add(1, Ordering::SeqCst) + 1)
}
