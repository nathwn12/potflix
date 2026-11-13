//! `window` -- mirrors the Go root files `go\window.go` + `go\settings_sections.go`.
//!
//! The tray-resident settings window: folders list, Scan / Refresh, the live
//! `indexed N of M files | metadata K of J` line with its `- catching up` state,
//! library export/import, `Clear metadata` / `Clear index`, `Report`, startup,
//! scan-speed segments. The scan progress is phase-aware: the bar is driven by
//! `Resolved` of `KeysTotal` so it cannot read full while metadata remains.
//!
//! # Owner-drawn, no child controls (`go\window.go:37-70`)
//!
//! There are no child controls: every rect-table id below is a key into a
//! hit-test table, the whole client area is owner-drawn, and there is no
//! `WM_CTLCOLOR*`/`WM_DRAWITEM` anywhere. Hit-testing and actions run through
//! ONE dispatch (`release_at` / `activate_focused` -> `do_button_action` /
//! `do_settings_action` / `do_card_action`), and the disabled state is enforced
//! AT INPUT via [`scan_blocked_reason`] - porting only the dim paint would lose
//! the guard.
//!
//! # Integer geometry (`go\window.go:3466-3484`)
//!
//! Every value is integer-scaled `(v*dpi+48)/96` from the 460x680 logical
//! design, and font heights are negative. An `f32`/`f64` scale would change
//! hit-tests versus paint, so the port keeps `i32` everywhere.
//!
//! # One OS thread
//!
//! The tray wndproc + main wndproc + `GetMessageW` loop must share one OS
//! thread; workers (poll, scan, report) may never touch the UI - their payloads
//! ride mutex-guarded snapshots and only ever repaint via `InvalidateRect` plus
//! an async `PostMessageW`. The UI state lives in [`UiState`], which the
//! tests construct directly (isolated) and production wraps in a process
//! global.

// The Win32 surface is inherently pointer-taking: every entry point accepts an
// `HWND`/`HDC` and hands it to `windows-sys`. The handles are opaque and the
// module never dereferences them itself, so the lint is silenced module-wide.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::{Arc, Mutex, MutexGuard};

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi;

use crate::brand;
use crate::win32::{Rect, utf16_z};

use library::{Progress, Stats};
use presence::{Action, Activity, CardSpec, Resolved};

use app_support::config::{CardKind, CardLine, CardSpecs};

// The picker's own two imports: the latch types the dialog reads/writes and the
// store's key derivations it keys those writes with.
use metadata::overrides::{season_key_for, series_key_for};
use metadata::{Candidate, Kind, Override, Scope};

/// Narrow chrome primitives (five explicit paint functions). Declared here so
/// the seam stays `window.rs` plus the one new module file.
#[path = "narrow.rs"]
mod narrow;

// ---------------------------------------------------------------------------
// Button ids (`go\window.go:39-70`): rect-table keys, not child control ids.
// ---------------------------------------------------------------------------

/// Main view "Add folder".
pub const BTN_ADD: i32 = 1;
/// Main view "Remove".
pub const BTN_REMOVE: i32 = 2;
/// Main view "Scan" / "Cancel".
pub const BTN_SCAN: i32 = 3;
/// Main view "Refresh".
pub const BTN_REFRESH: i32 = 4;
/// Main view "Close to tray".
pub const BTN_CLOSE: i32 = 5;
/// Main header "Settings" text link.
pub const BTN_SETTINGS: i32 = 6;
/// Settings view "Back".
pub const BTN_BACK: i32 = 7;
/// Main header "Card" text link.
pub const BTN_CARD: i32 = 8;
/// Card view "Back".
pub const BTN_CARD_BACK: i32 = 9;
/// Card view State presets, `+0..3` in `CardPresets` order.
pub const BTN_CARD_STATE_AT: i32 = 40;
/// Settings LIBRARY "Clear metadata".
pub const BTN_CLEAR_CACHE: i32 = 15;
/// Settings LIBRARY "Export...".
pub const BTN_EXPORT: i32 = 16;
/// Settings LIBRARY "Import...".
pub const BTN_IMPORT: i32 = 17;
/// Settings STARTUP toggle.
pub const BTN_STARTUP: i32 = 18;
/// Settings STARTUP "Open log folder".
pub const BTN_OPEN_LOG: i32 = 19;
/// Settings SCAN segment "Gentle".
pub const BTN_SCAN_GENTLE: i32 = 20;
/// Settings SCAN segment "Balanced".
pub const BTN_SCAN_BALANCED: i32 = 21;
/// Settings SCAN segment "Turbo".
pub const BTN_SCAN_TURBO: i32 = 22;
/// Main view "Report".
pub const BTN_REPORT: i32 = 23;
/// Settings LIBRARY "Clear index".
pub const BTN_CLEAR_INDEX: i32 = 24;
/// Settings BEHAVIOUR "Discord connection" toggle (approved redesign).
pub const BTN_DISCORD: i32 = 100;
/// Settings BEHAVIOUR "Theme" segments: AMOLED (default) and Dark. The two
/// segments share one 48px row; the selected one uses the text-colour
/// underline convention (`BTN_TOGGLE_ON`), never red.
pub const BTN_THEME_AMOLED: i32 = 103;
pub const BTN_THEME_DARK: i32 = 104;
/// Card view "Fix match..." -- the manual latch picker. Enabled only while
/// [`app_support::config::live_now_playing`] answers `Some`; dimmed and
/// input-refused otherwise (see [`fix_match_blocked_reason`]).
pub const BTN_FIX_MATCH: i32 = 10;
/// The Card view's synopsis affordance: a focusable text control whose hover
/// or keyboard focus reveals the full stored synopsis in the owner-drawn
/// tooltip. Not a stored preset.
pub const BTN_SYNOPSIS: i32 = 25;
/// The narrow Card view's surface switcher: one segment per Discord preview.
pub const BTN_SURFACE_MEMBER: i32 = 60;
pub const BTN_SURFACE_PROFILE: i32 = 61;
pub const BTN_SURFACE_ACTIVITY: i32 = 62;
/// The designer's field toggles. Each flips one [`presence::DesignerSpec`]
/// field and immediately publishes it to the live mirror the sender reads.
/// Covers are always on, so there is no artwork toggle.
pub const BTN_FIELD_DETAILS: i32 = 71;
pub const BTN_FIELD_STATE: i32 = 72;
pub const BTN_FIELD_SYNOPSIS: i32 = 73;
pub const BTN_FIELD_TIMERS: i32 = 74;
/// The Card workspace's title/year query field. It is the same single-field
/// contract as the picker dialog: a title, or a title with a trailing year.
/// Search is universal: every query runs both chains (regular and anime) with
/// no user-selected scope, and the list merges into one ranked result.
pub const BTN_PICKER_FIELD: i32 = 80;
/// The one primary picker action: Search. It never changes label or job: a
/// second verb (applying a match to the playing title) is its own inline
/// control, [`BTN_PICKER_APPLY`].
pub const BTN_PICKER_SEARCH: i32 = 83;
/// Apply the selected result to the playing title (the latch). Inline with the
/// results, and present only while a player exists and a row is selected.
pub const BTN_PICKER_APPLY: i32 = 86;
/// The result rows in the picker overlay, `+0..SEARCH_LIMIT-1` in candidate
/// order. The rows are hit-tested but are not part of the static focus ring;
/// the field owns text entry and Enter runs the primary action.
pub const BTN_PICKER_RESULT_AT: i32 = 90;
/// The overlay's dismiss control (Escape also closes it).
pub const BTN_PICKER_CLOSE: i32 = 89;
/// The one payload disclosure. Every field toggle, the kind override and the
/// line-2 state row live behind it; at rest the window shows the object, one
/// query field, one Search and this one control.
pub const BTN_ADVANCED: i32 = 84;

// ---------------------------------------------------------------------------
// Worker -> UI messages (`go\window.go:74-93`).
// ---------------------------------------------------------------------------

pub const WM_APP_SCAN_PROGRESS: u32 = crate::win32::WM_APP + 3;
pub const WM_APP_SCAN_DONE: u32 = crate::win32::WM_APP + 4;
pub const WM_APP_REPORT_DONE: u32 = crate::win32::WM_APP + 5;
/// The "Fix match" picker's worker -> dialog wakeup (`WM_APP + 6`): the worker
/// published its outcome into the dialog's slot and this message tells the
/// dialog's own message loop to read it.
pub const WM_APP_FIX_MATCH_DONE: u32 = crate::win32::WM_APP + 6;
/// The Card workspace picker's worker wakeup: a search or latch finished and
/// published into [`UiState`]'s picker slot.
pub const WM_APP_PICKER_DONE: u32 = crate::win32::WM_APP + 7;
/// A background artwork fetch finished; repaint so the cached cover appears
/// without waiting for the next live poll.
pub const WM_APP_ARTWORK_READY: u32 = crate::win32::WM_APP + 8;
/// A background preload finished: the UI thread applies the rows the playing
/// title's search already produced. `+9` because `+8` is the artwork wakeup.
pub const WM_APP_PRELOAD_DONE: u32 = crate::win32::WM_APP + 9;
/// The Win32 timer message (`go\window.go:83`).
pub const WM_TIMER: u32 = 0x0113;

/// `SetTimer` id for the live-scan repaint tick.
pub const SCAN_TIMER_ID: usize = 7;
/// Live repaints are capped at ~10/s (`go\window.go:92`).
pub const SCAN_PAINT_INTERVAL: Duration = Duration::from_millis(100);

// ---------------------------------------------------------------------------
// Settings-view message, key, and text constants (`go\window.go:220-239`).
// ---------------------------------------------------------------------------

pub const WM_CHAR: u32 = 0x0102;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_KILLFOCUS: u32 = 0x0008;

pub const VK_ESCAPE: u32 = 0x1B;
pub const VK_TAB: u32 = 0x09;
pub const VK_RETURN: u32 = 0x0D;
pub const VK_SPACE: u32 = 0x20;
pub const VK_SHIFT: u32 = 0x10;
pub const VK_PRIOR: u32 = 0x21;
pub const VK_NEXT: u32 = 0x22;
pub const VK_END: u32 = 0x23;
pub const VK_HOME: u32 = 0x24;
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;

pub const SW_SHOW_NORMAL: i32 = 1;
pub const DT_WORD_BREAK: u32 = 0x0010;

// ---------------------------------------------------------------------------
// Palette -- the repo's own brand system (`brand`, ported from
// `design\make_logo.py`).
//
// The incumbent ramp was an ad-hoc grey ladder with the Netflix red dropped in:
// it carried the red but none of the identity, because the identity lives in
// `design\` and nothing read it. Every value below is now a brand token, and
// every pair the app paints is measured in `brand`'s `brand_contrast_gate`.
//
// The roles are kept (canvas / chrome / panel / control / text) so no paint site
// had to change shape; only the values moved to the brand palette.
// ---------------------------------------------------------------------------

/// `rgb(r, g, b)` (`go\window.go:3487`).
#[must_use]
pub const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

// --- Surfaces: the approved narrow tokens (mock `potflix-preview/index.html`).

/// The canvas `#0F1012`. Also the window's own background brush, so
/// the DWM caption and the client area meet with no seam.
pub const COL_BG: u32 = brand::BG_V2;
/// The top bar / panels `#1A1B1E` (the identity sheet background).
pub const COL_SURFACE: u32 = brand::CHROME;
/// A raised panel / hero card `#2B2D33`.
pub const COL_CARD: u32 = brand::CARD_V2;
/// A control resting on a panel: the card surface itself.
pub const COL_ROW: u32 = brand::CARD_V2;
/// The same control, hovered or selected: `#33363D`.
pub const COL_ROW_HOVER: u32 = brand::ROW_HOVER_V2;
/// Hairline `#3A3D44`: panel dividers and secondary outlines in the mock.
pub const COL_BORDER: u32 = brand::HAIRLINE_V2;
/// Strong divider `#565A63` (selected underline weight lives here).
pub const COL_BORDER_STRONG: u32 = brand::STRONG_V2;

// --- Text: approved narrow roles (`#F2F3F5` / `#A7ABB4` / `#8A8F99`). ----

/// Body text `#F2F3F5`.
pub const COL_TEXT: u32 = brand::CARD_HDR;
/// Text ON an accent fill: brand `WHITE`.
pub const COL_ON_ACCENT: u32 = brand::WHITE;
/// Secondary text `#A7ABB4`: labels, hints, status.
pub const COL_MUTED: u32 = brand::MUTED_V2;
/// Disabled text `#8A8F99`.
pub const COL_MUTED_DIM: u32 = brand::DIM_V2;

// --- Accent: the approved red `#E5343F`, hover `#F04B55`. -------------------

pub const COL_ACCENT: u32 = brand::ACCENT_V2;
pub const COL_ACCENT_HOV: u32 = brand::ACCENT_HOV_V2;
pub const COL_ACCENT_DOWN: u32 = brand::DARK;
/// The selection bar's backing and the selected-row tint: the accent over the
/// card surface. Never carries text.
pub const COL_ACCENT_WASH: u32 = rgb(0x3A, 0x1C, 0x21);

/// Destructive text (Remove, and the clear actions' labels): the brand red
/// lifted until it clears the body gate on every surface it lands on
/// (`4.97:1` on the card surface, `6.31:1` on the canvas).
pub const COL_DANGER: u32 = brand::DANGER_TEXT;

// --- AMOLED theme: pure-black OLED surfaces. `Dark` is today's palette
// exactly; AMOLED remaps only the four surfaces, never borders/text/accent.

/// AMOLED canvas `#000000` (Dark `#0F1012`).
pub const COL_BG_AMOLED: u32 = rgb(0x00, 0x00, 0x00);
/// AMOLED chrome `#0A0A0B` (Dark `#1A1B1E`).
pub const COL_SURFACE_AMOLED: u32 = rgb(0x0A, 0x0A, 0x0B);
/// AMOLED card/row `#141416` (Dark `#2B2D33`).
pub const COL_CARD_AMOLED: u32 = rgb(0x14, 0x14, 0x16);
/// AMOLED row hover `#1C1C1F` (Dark `#33363D`).
pub const COL_ROW_HOVER_AMOLED: u32 = rgb(0x1C, 0x1C, 0x1F);

/// The live theme id (`amoled`|`dark`), default AMOLED. Published by
/// [`UiState`] on enter/change; read by the paint primitives so one map
/// covers every fill/stroke/text site without rewriting each `COL_*` use.
static LIVE_THEME: std::sync::LazyLock<Mutex<String>> =
    std::sync::LazyLock::new(|| Mutex::new(app_support::config::THEME_AMOLED.to_string()));

/// Publish the live theme id (normalised; unknown becomes AMOLED).
pub fn set_live_theme(theme: &str) {
    let normalized = app_support::config::normalize_theme(theme).to_string();
    *LIVE_THEME.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = normalized;
}

/// The live theme id.
pub fn live_theme() -> String {
    LIVE_THEME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Whether the AMOLED theme is live.
#[must_use]
pub fn is_amoled() -> bool {
    app_support::config::normalize_theme(
        &LIVE_THEME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    ) == app_support::config::THEME_AMOLED
}

/// The palette map: Dark values pass through, AMOLED values remap the four
/// surfaces. Borders, text, muted and accent are identical in both themes.
#[must_use]
pub fn themed(color: u32) -> u32 {
    if !is_amoled() {
        return color;
    }
    match color {
        c if c == COL_BG => COL_BG_AMOLED,
        c if c == COL_SURFACE => COL_SURFACE_AMOLED,
        c if c == COL_CARD || c == COL_ROW => COL_CARD_AMOLED,
        c if c == COL_ROW_HOVER => COL_ROW_HOVER_AMOLED,
        _ => color,
    }
}

// --- The named state set: every control paints from these, never a literal. --

/// Control, default.
pub const STATE_BG: u32 = COL_ROW;
/// Control, hover.
pub const STATE_BG_HOVER: u32 = COL_ROW_HOVER;
/// Control, pressed: the receding plane, one step down from the card surface.
pub const STATE_BG_PRESSED: u32 = brand::CHROME;
/// Control, disabled.
pub const STATE_BG_DISABLED: u32 = brand::CHROME;
/// Control outline, default / hover / focused.
pub const STATE_BORDER: u32 = COL_BORDER;
pub const STATE_BORDER_HOVER: u32 = COL_BORDER_STRONG;
/// Text, default / hover / pressed / disabled.
pub const STATE_TEXT: u32 = COL_TEXT;
pub const STATE_TEXT_DISABLED: u32 = COL_MUTED_DIM;
/// The keyboard focus ring: muted `#A7ABB4`, never accent red. The selection
/// bar keeps the accent; focus must never read as selection.
pub const STATE_FOCUS: u32 = COL_MUTED;

// --- Geometry tokens: a 4px grid, and the house squircle. -------------------

/// Spacing grid, logical px at 96 DPI. Every layout number is one of these.
pub const SP_1: i32 = 4;
pub const SP_2: i32 = 8;
pub const SP_3: i32 = 12;
pub const SP_4: i32 = 16;
pub const SP_5: i32 = 20;
pub const SP_6: i32 = 24;
pub const SP_8: i32 = 32;

/// The control height every radius below is derived from.
pub const BTN_H: i32 = 36;

/// Corner radii: the approved narrow values (control 10, panel 14).
pub const R_CTRL: i32 = 10;
pub const R_ROW: i32 = 10;
pub const R_PANEL: i32 = 14;
pub const R_BAR: i32 = brand::squircle(18);
/// The mark tile in the top bar: a 20px mark.
pub const R_TILE: i32 = brand::squircle(32);

/// Approved narrow band geometry, logical px at 96 DPI.
pub const TOPBAR_H_L: i32 = 48;
pub const HERO_MAX_H_L: i32 = 124;
pub const RESULT_ROW_H_L: i32 = 56;
pub const BAND_GAP_L: i32 = 12;
pub const STATUS_BAR_H_L: i32 = 24;
/// The clear between the status bar's right-aligned version and the window's
/// right edge: the version must never touch the edge at any DPI.
pub const STATUS_VERSION_INSET_L: i32 = 16;
pub const HERO_POSTER_W_L: i32 = 64;
pub const HERO_POSTER_H_L: i32 = 96;
pub const SEARCH_BTN_W_L: i32 = 96;
pub const GEAR_SIDE_L: i32 = 32;

/// Stroke widths, logical px at 96 DPI.
pub const STROKE_HAIR: i32 = 1;
pub const STROKE_FOCUS: i32 = 2;
/// The focus ring's inset from its control's edge: the ring never touches the
/// control outline, so a focused control reads as focused and not as re-outlined.
pub const FOCUS_INSET: i32 = 3;
/// The selection bar down the left edge of a selected row.
pub const SELECT_BAR_W: i32 = 3;
/// The list scrollbar: track/thumb width, its inset from the panel's right edge,
/// and the minimum thumb length (a long list must still show a grabbable thumb).
pub const SCROLLBAR_W: i32 = 5;
pub const SCROLLBAR_INSET: i32 = 6;
pub const SCROLLBAR_MIN_THUMB: i32 = 24;

// --- Type scale: logical px at 96 DPI, one size per role. ------------------

/// The wordmark (`POTFLIX`), in Archivo Black, tracked.
pub const FS_WORDMARK: i32 = brand::FS_WORDMARK;
/// A view's own heading.
pub const FS_TITLE: i32 = brand::FS_TITLE;
/// Rows, labels, values.
pub const FS_BODY: i32 = brand::FS_BODY;
/// A control's label.
pub const FS_CONTROL: i32 = brand::FS_CONTROL;
/// Field hints and the status line.
pub const FS_HINT: i32 = brand::FS_HINT;
/// A section's micro-label: tiny, uppercase, letter-spaced.
pub const FS_LABEL: i32 = brand::FS_LABEL;
/// The count badge's numeral.
pub const FS_BADGE: i32 = brand::FS_BADGE;
/// Approved narrow type sizes.
pub const FS_TOPBAR_APP: i32 = 13;
pub const FS_CHIP: i32 = 12;
pub const FS_HERO_TITLE: i32 = 20;
pub const FS_HERO_META: i32 = 12;
pub const FS_ROW_TITLE: i32 = 13;
pub const FS_EMPTY_TITLE: i32 = 15;
pub const FS_EMPTY_SUB: i32 = 13;
pub const FS_SETTINGS_TITLE: i32 = 15;
pub const FS_SET_CAP: i32 = 11;
pub const FS_EXPLAINER: i32 = 12;

// Button paint styles (`go\window.go:194-200`).
pub const BTN_SUBTLE: i32 = 0;
pub const BTN_ACCENT: i32 = 1;
pub const BTN_OUTLINE: i32 = 2;
pub const BTN_TEXT: i32 = 3;
pub const BTN_DANGER: i32 = 4;
/// A segment of a segmented control or a row of mutually-exclusive choices (the
/// tab strip, the scan-speed segments, the card presets): an unlit segment.
pub const BTN_TAB: i32 = 5;
/// The same segment, lit. The lit state is a fill step plus a red indicator under
/// the label -- never a second accent FILL, because each view has exactly one
/// primary control (`Scan`) and selection is not a primary action.
pub const BTN_TAB_SEL: i32 = 6;
/// A field toggle, off: the segment anatomy with a hairline and muted text.
pub const BTN_TOGGLE: i32 = 7;
/// A field toggle, on: a quiet lit segment with a WHITE indicator. The
/// designer can have several fields on at once, so this state deliberately
/// avoids the red selection indicator (minimal dark workspace, white controls).
pub const BTN_TOGGLE_ON: i32 = 8;

// ---------------------------------------------------------------------------
// Types.
// ---------------------------------------------------------------------------

/// The visible view (`go\window.go:141-150`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum View {
    #[default]
    Main,
    Settings,
    Card,
}

/// One actionable rect in the paint-time rect table (`go\window.go:241-247`).
#[derive(Clone, Debug)]
pub struct UiButton {
    pub id: i32,
    pub label: String,
    pub style: i32,
    pub rc: Rect,
}

/// The full hit-test table for one paint/mouse pass (`go\window.go:249-261`).
#[derive(Clone, Debug, Default)]
pub struct UiLayout {
    pub card: Rect,
    pub rows: Vec<Rect>,
    pub xbtns: Vec<Rect>,
    pub btns: Vec<UiButton>,
    pub status: Rect,
    pub overflow: i32,
    pub settings: Rect,
    pub card_link: Rect,
    /// The chrome header band (mark, wordmark, subtitle).
    pub header: Rect,
    /// The tab strip's outer rect; the two segments are [`UiLayout::card_link`]
    /// (Card) and [`UiLayout::settings`] (Settings).
    pub tab_strip: Rect,
    /// The section micro-label's line (the folder count's badge sits at its
    /// right end).
    pub section: Rect,
    /// The first visible folder index (`0` when the list fits).
    pub list_top: i32,
    /// How many folders the viewport can show.
    pub list_capacity: i32,
    /// The scrollbar track in the card's right gutter; empty when the list fits.
    pub scrollbar: Rect,
    /// The scrollbar thumb inside [`UiLayout::scrollbar`]; empty when it fits.
    pub thumb: Rect,
}

impl UiLayout {
    /// Whether the list has anything hidden and therefore shows a scrollbar.
    #[must_use]
    pub fn scrolled(&self) -> bool {
        self.scrollbar.right > self.scrollbar.left && self.scrollbar.bottom > self.scrollbar.top
    }
}

/// The settings-view hit-test table (`go\window.go:263-269`).
#[derive(Clone, Debug, Default)]
pub struct SettingsLayout {
    pub status: Rect,
    pub btns: Vec<UiButton>,
    /// The section micro-labels, in paint order. Owned by the layout so the
    /// label a paint call draws and the rect the audit measures cannot drift.
    pub labels: Vec<NamedTextRect>,
    /// The scan-speed hint's wrapped text block.
    pub hint: Rect,
    /// The Theme row's title ("Theme") and its 12px muted description. The
    /// row itself is the two 36px segments in `btns` (`BTN_THEME_*`); these
    /// rects are the left text column sharing the same 48px band.
    pub theme_title: Rect,
    pub theme_desc: Rect,
}

impl SettingsLayout {
    /// `settingsBtn` (`go\window.go:272`).
    #[must_use]
    pub fn settings_btn(&self, id: i32) -> Option<Rect> {
        self.btns.iter().find(|b| b.id == id).map(|b| b.rc)
    }
}

/// The Card-view hit-test table (`go\window.go:1436-1441`).
#[derive(Clone, Debug, Default)]
pub struct CardLayout {
    pub preview: Rect,
    /// The chrome header band; zero-height in the compact composition.
    pub header: Rect,
    /// The tab strip's outer rect; the two segments are [`CardLayout::card_link`]
    /// (Card) and [`CardLayout::settings`] (Settings).
    pub tab_strip: Rect,
    pub card_link: Rect,
    pub settings: Rect,
    /// The wide workspace's editor rail and preview stage. In the narrow
    /// composition both are the full content column and the rail holds the
    /// search/summary band above the preview.
    pub rail: Rect,
    pub stage: Rect,
    /// The three Discord surfaces: member list, profile/expanded card, and
    /// Activity tab. In the wide composition all three have distinct rects; in
    /// the narrow composition each is the one preview rect (only the selected
    /// surface paints).
    pub surfaces: [Rect; 3],
    /// The media rows of the profile surface: title, metadata, synopsis.
    pub rows: Vec<Rect>,
    /// The brand art tile inside the profile surface.
    pub art: Rect,
    /// The media title (`Title (Year)` for a movie, the series for an episode).
    pub title: Rect,
    /// The playback line (genres/runtime or `SxxEyy / Episode`).
    pub meta: Rect,
    /// The full-row synopsis affordance (hover + keyboard focus).
    pub synopsis: Rect,
    /// The tooltip panel, clamped inside the client; empty while hidden.
    pub tooltip: Rect,
    /// The designer's wire summary block (field values as they reach Discord).
    pub wire: Rect,
    /// The title/year + Movie/TV + Search band. It is in the first viewport at
    /// every size: above the previews in the wide split, above the one preview
    /// in the narrow stack, and a single compact row in the short client.
    pub picker_bar: Rect,
    /// The picker's result overlay. It drops from the bar over the previews
    /// while open; empty when no search result is shown.
    pub picker_results: Rect,
    /// The picker's one-line status readout (empty when the band is too narrow
    /// for it).
    pub picker_status: Rect,
    /// The one object card: cover, title, year/kind and the connection/presence
    /// status. It is the first thing under the tab strip in every composition.
    pub object: Rect,
    /// The object cover thumbnail inside [`CardLayout::object`].
    pub obj_cover: Rect,
    pub obj_title: Rect,
    pub obj_meta: Rect,
    pub obj_status: Rect,
    /// The Advanced disclosure's panel; empty while closed.
    pub advanced_panel: Rect,
    /// Compact composition (no chrome header, no field rows).
    pub compact: bool,
    /// Wide composition (the split workspace: rail beside the three surfaces).
    pub wide: bool,
    pub status: Rect,
    pub btns: Vec<UiButton>,
    /// The section micro-labels, in paint order.
    pub labels: Vec<NamedTextRect>,
    /// The explanatory line under the rows.
    pub hint: Rect,
}

impl CardLayout {
    /// `cardBtn` (`go\window.go:1444`).
    #[must_use]
    pub fn card_btn(&self, id: i32) -> Option<Rect> {
        self.btns.iter().find(|b| id == b.id).map(|b| b.rc)
    }
}

/// The full-row synopsis affordance as a hit-test button: the `synopsis` rect
/// is hoverable, clickable and a keyboard focus stop, so the tooltip the
/// paint shows on hover/focus/pin is reachable by mouse and keyboard alike.
pub fn push_synopsis_button(l: &mut CardLayout) {
    if l.synopsis.right <= l.synopsis.left || l.synopsis.bottom <= l.synopsis.top {
        return;
    }
    if l.btns.iter().any(|b| b.id == BTN_SYNOPSIS) {
        return;
    }
    l.btns.push(UiButton {
        id: BTN_SYNOPSIS,
        label: String::new(),
        style: BTN_TEXT,
        rc: l.synopsis,
    });
}

/// One painted text rect with its stable audit id (`go\window.go:666`).
#[derive(Clone, Debug)]
pub struct NamedTextRect {
    pub id: String,
    pub rc: Rect,
}

// Layout metrics (`go\window.go:495-499`).
pub const MAIN_CLIENT_W: i32 = 460;
pub const MAIN_CLIENT_H: i32 = 680;
pub const MAIN_MARGIN: i32 = 20;

/// The content column never grows past this logical width; wide windows centre
/// it so the media card keeps a readable measure instead of stretching a 128px
/// body line across 1440px.
pub const CONTENT_MAX_W: i32 = 980;

/// The Card view's supported minimum client size: the compact media row, the
/// State preset row and the footer must all fit at 96 DPI.
pub const CARD_MIN_W: i32 = 360;
pub const CARD_MIN_H: i32 = 270;

/// The Main and Settings views need their full vertical stack (folder list,
/// action grid, status, the settings groups); they refuse to shrink below the
/// shipped 460x680 composition's height.
pub const APP_MIN_W: i32 = 420;
pub const APP_MIN_H: i32 = 680;

// --- The main view's vertical stack, logical px at 96 DPI. ------------------
//
// These are the composition's skeleton, shared by `compute_layout_total` (the
// hit-test table) and the paint path, so a rect can never disagree with what is
// drawn in it.

/// The chrome header band: mark, wordmark, subtitle.
pub const HEADER_H_L: i32 = 64;
/// The tab strip's top edge and height.
pub const TAB_TOP_L: i32 = 76;
pub const TAB_H_L: i32 = 34;
/// A section micro-label's line height.
pub const SECTION_H_L: i32 = 18;
/// The folder panel's inner padding.
pub const PAD_L: i32 = 10;
/// A folder row's height and the gap between rows.
pub const ROW_H_L: i32 = 36;
pub const ROW_GAP_L: i32 = 8;
/// The `+N more` line's reserved height.
pub const OVERFLOW_H_L: i32 = 22;
/// The action grid: one column gap, one row gap, three rows.
pub const BTN_GAP_L: i32 = 12;
pub const FOOTER_ROWS_L: i32 = 3;
/// The status line's height and the bottom margin below it.
pub const STATUS_H_L: i32 = 20;
pub const BOTTOM_L: i32 = 14;
/// The header mark tile's side.
pub const MARK_TILE_L: i32 = 32;

// ---------------------------------------------------------------------------
// Pure geometry (`go\window.go:3466-3472, :470-492`).
// ---------------------------------------------------------------------------

/// `scalePx` (`go\window.go:3467`): scale a 96-DPI logical px to `dpi`.
#[must_use]
pub fn scale_px(dpi: i32, v: i32) -> i32 {
    let dpi = if dpi <= 0 { 96 } else { dpi };
    (v * dpi + 48) / 96
}

/// `ptInRect` (`go\window.go:470`): left/top inclusive, right/bottom exclusive.
#[must_use]
pub fn pt_in_rect(rc: Rect, x: i32, y: i32) -> bool {
    (rc.left..rc.right).contains(&x) && (rc.top..rc.bottom).contains(&y)
}

/// `buttonAtPoint` (`go\window.go:475`).
#[must_use]
pub fn button_at_point(btns: &[UiButton], x: i32, y: i32) -> i32 {
    for b in btns {
        if pt_in_rect(b.rc, x, y) {
            return b.id;
        }
    }
    0
}

/// `rowAtPoint` (`go\window.go:485`).
#[must_use]
pub fn row_at_point(rows: &[Rect], x: i32, y: i32) -> i32 {
    for (i, r) in rows.iter().enumerate() {
        if pt_in_rect(*r, x, y) {
            return i as i32;
        }
    }
    -1
}

/// Clip `rc` to the band above `status_top` (the status bar owns every pixel
/// at/after it). `None` when nothing is visible: fully inside the status bar
/// or clipped empty. A row straddling the bar paints and hits only above it.
#[must_use]
pub fn clip_rect_above_status(rc: Rect, status_top: i32) -> Option<Rect> {
    if rc.bottom <= rc.top || rc.top >= status_top {
        return None;
    }
    let bottom = rc.bottom.min(status_top);
    if bottom <= rc.top {
        return None;
    }
    Some(Rect {
        bottom,
        ..rc
    })
}

/// `button_at_point` that never hits inside the status bar: a point at/below
/// `status_top` is chrome, never a row. Straddling rows still hit above it.
#[must_use]
pub fn button_at_point_above_status(btns: &[UiButton], status_top: i32, x: i32, y: i32) -> i32 {
    if y >= status_top {
        return 0;
    }
    for b in btns {
        if clip_rect_above_status(b.rc, status_top).is_some_and(|rc| pt_in_rect(rc, x, y)) {
            return b.id;
        }
    }
    0
}

/// `row_at_point` that never hits inside the status bar.
#[must_use]
pub fn row_at_point_above_status(rows: &[Rect], status_top: i32, x: i32, y: i32) -> i32 {
    if y >= status_top {
        return -1;
    }
    for (i, r) in rows.iter().enumerate() {
        if clip_rect_above_status(*r, status_top).is_some_and(|rc| pt_in_rect(rc, x, y)) {
            return i as i32;
        }
    }
    -1
}

/// `rectsIntersect` (`go\window.go:3572`): nonzero-area overlap only.
#[must_use]
pub fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

// ---------------------------------------------------------------------------
// computeLayout (`go\window.go:503-604`).
// ---------------------------------------------------------------------------

/// Build the main-view hit-test table for `dirs`, client size and DPI, with the
/// list viewport at its top (`list_top = 0`).
#[must_use]
pub fn compute_layout(dirs: &[String], cw: i32, ch: i32, dpi: i32) -> UiLayout {
    compute_layout_from(dirs, cw, ch, dpi, 0)
}

/// The scrollbar thumb inside `track`, or an empty rect when nothing is hidden.
///
/// Pure so it can be pinned: the thumb's length is the visible fraction of the
/// list, never shorter than `min_thumb` (a 200-folder list must still offer a
/// grabbable thumb), and its offset is the scroll fraction of the thumb's travel.
#[must_use]
pub fn scrollbar_thumb(track: Rect, total: i32, capacity: i32, top: i32, min_thumb: i32) -> Rect {
    let h = track.bottom - track.top;
    if total <= capacity || capacity <= 0 || h <= 0 {
        return Rect::default();
    }
    let mut th = h * capacity / total;
    if th > h {
        th = h;
    }
    if th < min_thumb {
        th = min_thumb.min(h);
    }
    let travel = h - th;
    let max_top = total - capacity;
    let off = if max_top > 0 {
        (top.clamp(0, max_top) * travel) / max_top
    } else {
        0
    };
    Rect {
        left: track.left,
        top: track.top + off,
        right: track.right,
        bottom: track.top + off + th,
    }
}

/// The content column every view centres: the window minus its margins, capped
/// at [`CONTENT_MAX_W`] and centred when the window is wider. At the default
/// 460px client this is exactly the old 20px margin and 420px column, so the
/// shipped geometry is unchanged.
#[must_use]
pub fn content_span(cw: i32, dpi: i32) -> (i32, i32) {
    let margin = scale_px(dpi, MAIN_MARGIN);
    let avail = (cw - margin * 2).max(scale_px(dpi, 100));
    let width = avail.min(scale_px(dpi, CONTENT_MAX_W));
    ((cw - width) / 2, width)
}

/// The tab strip's two segments as `(card, settings)`, in every view.
///
/// One function owns the strip's geometry so the three views cannot drift: the
/// strip is the same control, in the same place, on every view, and switching
/// views never moves the control the user just clicked.
#[must_use]
pub fn tab_strip_rects(cw: i32, dpi: i32) -> (Rect, Rect) {
    let sc = |v: i32| scale_px(dpi, v);
    let (left, content_w) = content_span(cw, dpi);
    let top = sc(TAB_TOP_L);
    let bottom = top + sc(TAB_H_L);
    // Equal segments with one gap: the same rule the action grid uses, so both
    // segments are exactly equal at every DPI.
    let seg_w = (content_w - sc(SP_2)) / 2;
    (
        Rect {
            left,
            top,
            right: left + seg_w,
            bottom,
        },
        Rect {
            left: left + seg_w + sc(SP_2),
            top,
            right: left + seg_w + sc(SP_2) + seg_w,
            bottom,
        },
    )
}

/// The two tab-strip segments as controls, for any view's button table.
#[must_use]
pub fn tab_strip_buttons(cw: i32, dpi: i32) -> Vec<UiButton> {
    let (card, settings) = tab_strip_rects(cw, dpi);
    vec![
        UiButton {
            id: BTN_CARD,
            label: "Card".into(),
            style: BTN_TAB,
            rc: card,
        },
        UiButton {
            id: BTN_SETTINGS,
            label: "Settings".into(),
            style: BTN_TAB,
            rc: settings,
        },
    ]
}

/// The same table with the list scrolled to `top` (the index of the first visible
/// folder). `top` is clamped to the real scroll range, and the card keeps the
/// height of a full viewport once the list overflows, so scrolling never resizes
/// the card under the pointer.
#[must_use]
pub fn compute_layout_from(dirs: &[String], cw: i32, ch: i32, dpi: i32, top: i32) -> UiLayout {
    compute_layout_total(dirs.len() as i32, cw, ch, dpi, top)
}

/// The same table from a folder COUNT instead of the folders: the scroll
/// clamp for [`UiState::scroll_list`], which owns no folder slice. Pure.
#[must_use]
pub fn compute_layout_total(total: i32, cw: i32, ch: i32, dpi: i32, top: i32) -> UiLayout {
    let sc = |v: i32| scale_px(dpi, v);
    let (margin, content_w) = content_span(cw, dpi);

    let mut l = UiLayout::default();
    // --- The vertical stack -------------------------------------------------
    //
    // One column, top to bottom: chrome header, tab strip, section micro-label,
    // the folder panel, then the action grid and the status line. Every number
    // is a token or a sum of tokens, so the composition moves as one piece and
    // the panel's height is whatever the grid leaves it.
    let header_h = sc(HEADER_H_L);
    let tab_top = sc(TAB_TOP_L);
    let tab_h = sc(TAB_H_L);
    let section_top = tab_top + tab_h + sc(SP_3);
    let section_h = sc(SECTION_H_L);
    let panel_top = section_top + section_h + sc(SP_2);

    let pad = sc(PAD_L);
    let row_h = sc(ROW_H_L);
    let gap = sc(ROW_GAP_L);

    let btn_h = sc(BTN_H);
    let btn_gap = sc(BTN_GAP_L);
    let footer_h = FOOTER_ROWS_L * btn_h + (FOOTER_ROWS_L - 1) * btn_gap;
    let status_h = sc(STATUS_H_L);
    let bottom = sc(BOTTOM_L);
    let status_top = ch - bottom - status_h;
    let footer_top = status_top - sc(SP_3) - footer_h;
    let panel_max_h = (footer_top - sc(SP_4) - panel_top).max(sc(80));

    l.header = Rect {
        left: 0,
        top: 0,
        right: cw,
        bottom: header_h,
    };

    // --- The tab strip ------------------------------------------------------
    let (card_tab, settings_tab) = tab_strip_rects(cw, dpi);
    l.tab_strip = Rect {
        left: card_tab.left,
        top: card_tab.top,
        right: settings_tab.right,
        bottom: card_tab.bottom,
    };
    l.card_link = card_tab;
    l.settings = settings_tab;
    l.section = Rect {
        left: margin,
        top: section_top,
        right: margin + content_w,
        bottom: section_top + section_h,
    };

    // --- The folder panel ---------------------------------------------------
    let avail = panel_max_h - pad * 2;
    let mut max_rows = (avail + gap) / (row_h + gap);
    if max_rows < 1 {
        max_rows = 1;
    }

    let top = top.clamp(0, (total - max_rows).max(0));
    let mut vis = total - top;
    if vis > max_rows {
        l.overflow = vis - max_rows;
        vis = max_rows;
    }
    if vis < 0 {
        vis = 0;
    }
    l.list_top = top;
    l.list_capacity = max_rows;
    // The viewport is full-height once anything is hidden, whichever offset the
    // list is scrolled to: the `+N more` line's height is reserved whenever the
    // list overflows at all, so scrolling never resizes the panel (and its
    // scrollbar track) under the pointer. The line itself still paints only
    // while something hides below (`l.overflow > 0` at the paint site).
    let viewport = if total > max_rows { max_rows } else { vis };
    let mut panel_h = pad * 2 + row_h;
    if viewport > 0 {
        panel_h = pad * 2 + viewport * row_h + (viewport - 1) * gap;
        if total > max_rows {
            panel_h += sc(OVERFLOW_H_L);
        }
    }
    if panel_h > panel_max_h {
        panel_h = panel_max_h;
    }
    l.card = Rect {
        left: margin,
        top: panel_top,
        right: margin + content_w,
        bottom: panel_top + panel_h,
    };
    let row_x = l.card.left + pad;
    let row_w = content_w - pad * 2;
    for i in 0..vis {
        let y = panel_top + pad + i * (row_h + gap);
        l.rows.push(Rect {
            left: row_x,
            top: y,
            right: row_x + row_w,
            bottom: y + row_h,
        });
        let xw = sc(30);
        l.xbtns.push(Rect {
            left: row_x + row_w - xw,
            top: y,
            right: row_x + row_w,
            bottom: y + row_h,
        });
    }
    // The scrollbar lives inside the panel's right padding gutter, so it never
    // overlaps a row or the `+N more` line. Gated on real overflow (`total >
    // max_rows`), not on `l.overflow` (hidden-below): the track must persist
    // at the last viewport too, or it blinks out exactly when reached.
    if total > max_rows {
        let sb_w = sc(SCROLLBAR_W);
        let inset = sc(SCROLLBAR_INSET);
        let track = Rect {
            left: l.card.right - inset,
            top: l.card.top + pad,
            right: l.card.right - inset + sb_w,
            bottom: l.card.bottom - pad,
        };
        l.scrollbar = track;
        l.thumb = scrollbar_thumb(track, total, max_rows, top, sc(SCROLLBAR_MIN_THUMB));
    }

    // --- The action grid ----------------------------------------------------
    //
    // ONE grid for every action, all three rows of it. Equal columns
    // (`col_w = (content_w - gap) / 2`), one gap between columns and one between
    // rows, and exactly one primary control (`Scan`, accent) -- the rest are
    // secondary. This replaces the incumbent's two off-grid columns plus a
    // right-aligned bare `Close to tray` text and a stray left-aligned `Report`
    // on a fourth, unaligned row.
    let col_w = (content_w - btn_gap) / 2;
    let (x0, x1) = (margin, margin + col_w + btn_gap);
    let y_of = |row: i32| footer_top + row * (btn_h + btn_gap);
    let cell = |x: i32, y: i32| Rect {
        left: x,
        top: y,
        right: x + col_w,
        bottom: y + btn_h,
    };
    l.btns = vec![
        // The tab strip's two segments are controls too, so they hover, press,
        // focus and paint through the same machinery as the grid above.
        UiButton {
            id: BTN_CARD,
            label: "Card".into(),
            style: BTN_TAB,
            rc: l.card_link,
        },
        UiButton {
            id: BTN_SETTINGS,
            label: "Settings".into(),
            style: BTN_TAB,
            rc: l.settings,
        },
        // No run/folder controls: Potflix does not scan a library (owner order
        // 2026-11-03). The wide view keeps only the window action below.
        UiButton {
            id: BTN_REPORT,
            label: report_button_label().into(),
            style: BTN_OUTLINE,
            rc: cell(x0, y_of(2)),
        },
        UiButton {
            id: BTN_CLOSE,
            label: "Close to tray".into(),
            style: BTN_OUTLINE,
            rc: cell(x1, y_of(2)),
        },
    ];

    let st_h = sc(20);
    l.status = Rect {
        left: margin,
        top: status_top,
        right: margin + content_w,
        bottom: status_top + st_h,
    };
    l
}

/// `computeSettingsLayout` — approved narrow redesign.
///
/// Pinned 48px top bar + 15pt title, five 48px body rows under 11px caps
/// labels: BEHAVIOUR (Discord connection, Close to tray, Run at startup,
/// plus the Theme segments) and LIBRARY (Clear metadata). The defaults are
/// the product: no row rebuilds a default. 24px status bar. Body rows may
/// extend past `ch`: the view scrolls, the title and status stay pinned.
#[must_use]
pub fn compute_settings_layout(
    cw: i32,
    ch: i32,
    dpi: i32,
    startup: bool,
    scan_speed: &str,
    theme: &str,
) -> SettingsLayout {
    let sc = |v: i32| scale_px(dpi, v);
    let (margin, content_w) = content_span(cw, dpi);

    let mut l = SettingsLayout::default();
    let topbar_h = sc(TOPBAR_H_L);
    let title_h = sc(20);
    let row_h = sc(48);
    let cap_h = sc(14);
    let gap = sc(8);
    let status_h = sc(STATUS_BAR_H_L);
    let status_top = (ch - status_h).max(0);

    // Top bar gear returns to the Card; the self tab stays wired but hidden.
    let gear_side = sc(GEAR_SIDE_L);
    let gear = Rect {
        left: (cw - margin - gear_side).max(0),
        top: (topbar_h - gear_side) / 2,
        right: (cw - margin).max(0),
        bottom: (topbar_h + gear_side) / 2,
    };

    l.status = Rect {
        left: 0,
        top: status_top,
        right: cw,
        bottom: status_top + status_h,
    };

    let cur_theme = app_support::config::normalize_theme(theme);
    let spec = crate::settings::startup_toggle_label(startup);
    let _ = scan_speed;

    // Stack cursor below the pinned title.
    let mut y = topbar_h + title_h + gap;
    let push_label = |l: &mut SettingsLayout, id: &str, y: &mut i32| {
        let rc = Rect {
            left: margin,
            top: *y,
            right: margin + content_w,
            bottom: *y + cap_h,
        };
        l.labels.push(NamedTextRect {
            id: id.into(),
            rc,
        });
        *y += cap_h + sc(SP_2);
    };
    let push_row = |l: &mut SettingsLayout, id: i32, label: String, style: i32, y: &mut i32| {
        let rc = Rect {
            left: margin,
            top: *y,
            right: margin + content_w,
            bottom: *y + row_h,
        };
        l.btns.push(UiButton { id, label, style, rc });
        *y += row_h;
    };

    // Title is pinned: owned by the layout as the first label so the audit
    // cannot drift from what the paint draws.
    l.labels.push(NamedTextRect {
        id: "settings.title".into(),
        rc: Rect {
            left: margin,
            top: topbar_h,
            right: margin + content_w,
            bottom: topbar_h + title_h,
        },
    });

    // No media-folders group: Potflix never scans a library. Everything it
    // knows is built on the fly from what is actually playing, so the folder
    // manager and its scan controls are gone by owner order (2026-11-03).
    push_label(&mut l, "settings.behaviour", &mut y);
    push_row(&mut l, BTN_DISCORD, "Discord connection".into(), BTN_OUTLINE, &mut y);
    push_row(&mut l, BTN_CLOSE, "Close to tray".into(), BTN_OUTLINE, &mut y);
    push_row(&mut l, BTN_STARTUP, spec.into(), BTN_OUTLINE, &mut y);
    // Theme row: one 48px band under BEHAVIOUR. Left column carries the
    // "Theme" title plus the 12px muted description; right column carries
    // the two 36px segments (`AMOLED` | `Dark`). Selected uses the
    // text-colour underline (`BTN_TOGGLE_*`), never red.
    {
        let theme_top = y;
        let theme_h = sc(48);
        let seg_h = sc(BTN_H);
        let seg_gap = sc(SP_2);
        let text_gap = sc(SP_3);
        // Right-aligned control: two equal 36px-height segments.
        let seg_w = sc(72);
        let control_w = seg_w * 2 + seg_gap;
        let control_left = (margin + content_w - control_w).max(margin);
        let seg_top = theme_top + (theme_h - seg_h) / 2;
        l.btns.push(UiButton {
            id: BTN_THEME_AMOLED,
            label: "AMOLED".into(),
            style: crate::settings::theme_segment_style(
                app_support::config::THEME_AMOLED,
                cur_theme,
            ),
            rc: Rect {
                left: control_left,
                top: seg_top,
                right: control_left + seg_w,
                bottom: seg_top + seg_h,
            },
        });
        l.btns.push(UiButton {
            id: BTN_THEME_DARK,
            label: "Dark".into(),
            style: crate::settings::theme_segment_style(
                app_support::config::THEME_DARK,
                cur_theme,
            ),
            rc: Rect {
                left: control_left + seg_w + seg_gap,
                top: seg_top,
                right: control_left + control_w,
                bottom: seg_top + seg_h,
            },
        });
        let text_w = (control_left - text_gap - margin).max(sc(80));
        l.theme_title = Rect {
            left: margin,
            top: theme_top + sc(6),
            right: margin + text_w,
            bottom: theme_top + sc(6) + sc(18),
        };
        l.theme_desc = Rect {
            left: margin,
            top: theme_top + sc(6) + sc(18) + sc(2),
            right: margin + text_w,
            bottom: theme_top + sc(6) + sc(18) + sc(2) + sc(14),
        };
        y += theme_h;
    }
    y += gap;

    push_label(&mut l, "settings.lib", &mut y);
    push_row(&mut l, BTN_CLEAR_CACHE, "Clear metadata".into(), BTN_DANGER, &mut y);

    // Top-bar controls: gear back to Card, self hidden but wired.
    l.btns.push(UiButton {
        id: BTN_CARD,
        label: "⚙".into(),
        style: BTN_TEXT,
        rc: gear,
    });
    l.btns.push(UiButton {
        id: BTN_SETTINGS,
        label: "Settings".into(),
        style: BTN_TAB_SEL,
        rc: Rect::default(),
    });
    // Legacy footer Back stays wired for Esc/keyboard, hidden in the new body.
    l.btns.push(UiButton {
        id: BTN_BACK,
        label: "Back".into(),
        style: BTN_OUTLINE,
        rc: Rect::default(),
    });
    let _ = startup;
    l
}

/// The Settings scroll origin: the first scrollable pixel below the pinned
/// title. The top bar, gear and `settings.title` never move; everything from
/// here to the status bar shares [`UiState::settings_scroll`].
#[must_use]
pub fn settings_scroll_origin(dpi: i32) -> i32 {
    let sc = |v: i32| scale_px(dpi, v);
    sc(TOPBAR_H_L) + sc(20) + sc(8)
}

/// The bottom of the Settings content in an unscrolled table: the lowest
/// scrolled rect (rows, segments, hint). Pure so paint, hit test
/// and focus clamp against the same range.
#[must_use]
pub fn settings_content_bottom(l: &SettingsLayout, dpi: i32) -> i32 {
    let mut bottom = settings_scroll_origin(dpi);
    for b in &l.btns {
        if b.id == BTN_CARD || b.id == BTN_SETTINGS || b.id == BTN_BACK {
            continue;
        }
        bottom = bottom.max(b.rc.bottom);
    }
    for lab in &l.labels {
        if lab.id == "settings.title" {
            continue;
        }
        bottom = bottom.max(lab.rc.bottom);
    }
    bottom = bottom.max(l.hint.bottom);
    bottom = bottom.max(l.theme_title.bottom).max(l.theme_desc.bottom);
    bottom
}

/// The last Settings scroll offset that keeps content reachable. Pure, so the
/// wheel, PageUp/PageDown, Home/End and the focus reveal clamp against the
/// same range the paint reads.
#[must_use]
pub fn settings_max_scroll(l: &SettingsLayout, ch: i32, dpi: i32) -> i32 {
    let sc = |v: i32| scale_px(dpi, v);
    let status_top = (ch - sc(STATUS_BAR_H_L)).max(0);
    (settings_content_bottom(l, dpi) - status_top).max(0)
}

/// The scrolled Settings table: the pinned chrome (top bar, title, status)
/// stays, everything else moves up by `scroll` (clamped). Paint, hit test and
/// focus order all read this one table, so a painted row and a clickable row
/// cannot drift.
#[must_use]
pub fn apply_settings_scroll(mut l: SettingsLayout, scroll: i32, ch: i32, dpi: i32) -> SettingsLayout {
    let max = settings_max_scroll(&l, ch, dpi);
    let scroll = scroll.clamp(0, max);
    if scroll == 0 {
        return l;
    }
    let origin = settings_scroll_origin(dpi);
    let shift = |rc: &mut Rect| {
        if rc.bottom > rc.top && rc.top >= origin {
            rc.top -= scroll;
            rc.bottom -= scroll;
        }
    };
    for b in &mut l.btns {
        if b.id == BTN_CARD || b.id == BTN_SETTINGS || b.id == BTN_BACK {
            continue;
        }
        shift(&mut b.rc);
    }
    for lab in &mut l.labels {
        if lab.id == "settings.title" {
            continue;
        }
        shift(&mut lab.rc);
    }
    shift(&mut l.hint);
    shift(&mut l.theme_title);
    shift(&mut l.theme_desc);
    l
}

/// The scrolled Settings composition: [`compute_settings_layout`] plus the
/// one shared scroll offset. Every consumer (paint, hit test, focus) reads
/// this, never the raw table.
#[must_use]
pub fn settings_layout_scrolled(
    cw: i32,
    ch: i32,
    dpi: i32,
    startup: bool,
    scan_speed: &str,
    theme: &str,
    scroll: i32,
) -> SettingsLayout {
    let l = compute_settings_layout(cw, ch, dpi, startup, scan_speed, theme);
    apply_settings_scroll(l, scroll, ch, dpi)
}

// ---------------------------------------------------------------------------
// Text-rect tables (`go\window.go:666-727`, `:1602-1635`).
// ---------------------------------------------------------------------------

/// `mainTextRects` (`go\window.go:674`): the stable audit id for every string
/// the main view paints, in the same rect the paint path draws it in.
#[must_use]
pub fn main_text_rects(
    cw: i32,
    ch: i32,
    dpi: i32,
    l: &UiLayout,
    dir_count: i32,
) -> Vec<NamedTextRect> {
    let sc = |v: i32| scale_px(dpi, v);
    let mut out = vec![
        NamedTextRect {
            id: "main.wordmark".into(),
            rc: Rect {
                left: l.header.left + sc(MAIN_MARGIN) + sc(MARK_TILE_L) + sc(SP_3),
                top: l.header.top + sc(SP_3),
                right: l.header.right - sc(MAIN_MARGIN),
                bottom: l.header.top + sc(SP_3) + sc(26),
            },
        },
        NamedTextRect {
            id: "main.subtitle".into(),
            rc: Rect {
                left: l.header.left + sc(MAIN_MARGIN) + sc(MARK_TILE_L) + sc(SP_3),
                top: l.header.top + sc(SP_3) + sc(26),
                right: l.header.right - sc(MAIN_MARGIN),
                bottom: l.header.bottom - sc(SP_2),
            },
        },
        NamedTextRect {
            id: "main.section".into(),
            rc: Rect {
                left: l.section.left,
                top: l.section.top,
                right: l.section.right - sc(18) - sc(STROKE_FOCUS) - sc(SP_2),
                bottom: l.section.bottom,
            },
        },
        NamedTextRect {
            id: "main.badge".into(),
            rc: Rect {
                left: l.section.right - sc(18) - sc(STROKE_FOCUS),
                top: l.section.top + (l.section.bottom - l.section.top - sc(18)) / 2,
                right: l.section.right,
                bottom: l.section.top + (l.section.bottom - l.section.top - sc(18)) / 2 + sc(18),
            },
        },
    ];
    let _ = dir_count;
    if l.rows.is_empty() {
        out.push(NamedTextRect {
            id: "main.empty".into(),
            rc: l.card,
        });
    } else {
        for (i, r) in l.rows.iter().enumerate() {
            let text_right = r.right - sc(SP_3);
            out.push(NamedTextRect {
                id: format!("main.row-{i}"),
                rc: Rect {
                    left: r.left + sc(SP_3),
                    top: r.top,
                    right: text_right,
                    bottom: r.bottom,
                },
            });
        }
        if l.overflow > 0 {
            out.push(NamedTextRect {
                id: "main.overflow".into(),
                rc: Rect {
                    left: l.card.left + sc(SP_3),
                    top: l.card.bottom - sc(OVERFLOW_H_L),
                    right: l.card.right - sc(SP_3),
                    bottom: l.card.bottom - sc(SP_1),
                },
            });
        }
    }
    for b in &l.btns {
        out.push(NamedTextRect {
            id: format!("main.btn-{}", b.id),
            rc: b.rc,
        });
    }
    out.push(NamedTextRect {
        id: "main.status".into(),
        rc: l.status,
    });
    let _ = (cw, ch);
    out
}

/// `settingsTextRects` (`go\window.go:711`).
#[must_use]
pub fn settings_text_rects(cw: i32, ch: i32, dpi: i32, l: &SettingsLayout) -> Vec<NamedTextRect> {
    let mut out = l.labels.clone();
    out.push(NamedTextRect {
        id: "settings.theme".into(),
        rc: l.theme_title,
    });
    out.push(NamedTextRect {
        id: "settings.theme-desc".into(),
        rc: l.theme_desc,
    });
    out.push(NamedTextRect {
        id: "settings.hint".into(),
        rc: l.hint,
    });
    for b in &l.btns {
        out.push(NamedTextRect {
            id: format!("settings.btn-{}", b.id),
            rc: b.rc,
        });
    }
    out.push(NamedTextRect {
        id: "settings.status".into(),
        rc: l.status,
    });
    let _ = (cw, ch, dpi);
    out
}

// ---------------------------------------------------------------------------
// Focus model (`go\window.go:1244-1316`).
// ---------------------------------------------------------------------------

/// `mainFocusOrder` (`go\window.go:1244`), in visual order: the tab strip first
/// (it is the topmost control row), then the action grid row by row.
#[must_use]
pub fn main_focus_order() -> Vec<i32> {
    vec![BTN_CARD, BTN_SETTINGS, BTN_REPORT, BTN_CLOSE]
}

/// `settingsFocusOrder` — approved narrow visual order: top-bar Card, then the
/// five surviving rows in layout order.
#[must_use]
pub fn settings_focus_order() -> Vec<i32> {
    vec![
        BTN_CARD,
        BTN_DISCORD,
        BTN_CLOSE,
        BTN_STARTUP,
        BTN_THEME_AMOLED,
        BTN_THEME_DARK,
        BTN_CLEAR_CACHE,
    ]
}

/// `cardStateButton` (`go\window.go:1459`).
#[must_use]
pub fn card_state_button(i: i32) -> i32 {
    BTN_CARD_STATE_AT + i
}

/// `cardFocusOrder` — approved narrow visual order: gear, field, Search, hero
/// Fix, pinned Apply. The wide workspace keeps its own Advanced disclosure
/// (mouse-reachable); the narrow layout carries no Change-filters control.
#[must_use]
pub fn card_focus_order() -> Vec<i32> {
    vec![
        BTN_SETTINGS,
        BTN_PICKER_FIELD,
        BTN_PICKER_SEARCH,
        BTN_FIX_MATCH,
        BTN_PICKER_APPLY,
        BTN_SYNOPSIS,
    ]
}

/// The Card focus order for the current disclosure state with no result row
/// selected: the one shape every at-rest card has.
#[must_use]
pub fn card_focus_order_with(advanced: bool) -> Vec<i32> {
    card_focus_order_for(advanced, false, false)
}

/// The Card focus order for the state actually on screen: the narrow redesign
/// has no disclosure — every scope, Fix, Apply and Change filters is always
/// laid out — so the order is the static narrow order. `advanced`,
/// `has_selection` and `can_apply` are accepted for call-compat and ignored:
/// Apply is pinned (disabled until picked) and Fix lives in the hero.
#[must_use]
pub fn card_focus_order_for(advanced: bool, has_selection: bool, can_apply: bool) -> Vec<i32> {
    let _ = (advanced, has_selection, can_apply);
    card_focus_order()
}

/// `focusOrderForView` (`go\window.go:1268`).
///
/// The view's static order. The Card view's result actions come and go with the
/// results list, so the window takes those from [`card_focus_order_for`] with
/// the live layout state; this returns the at-rest shape.
#[must_use]
pub fn focus_order_for_view(v: View) -> Vec<i32> {
    match v {
        View::Settings => settings_focus_order(),
        View::Card => card_focus_order(),
        View::Main => main_focus_order(),
    }
}

/// `moveFocus` (`go\window.go:1282`): step `cur` by `dir` with wraparound.
#[must_use]
pub fn move_focus(cur: i32, dir: i32, order: &[i32]) -> i32 {
    if order.is_empty() {
        return 0;
    }
    if let Some(i) = order.iter().position(|&id| id == cur) {
        let n = order.len() as i32;
        let mut j = (i as i32 + dir) % n;
        if j < 0 {
            j += n;
        }
        return order[j as usize];
    }
    if dir < 0 {
        return order[order.len() - 1];
    }
    order[0]
}

/// `mainFocusRect` (`go\window.go:1303`). The tab strip's two segments are
/// controls in the same table as the action grid, so one lookup covers every
/// focus stop -- there is no second, rect-less focus vocabulary any more.
#[must_use]
pub fn main_focus_rect(l: &UiLayout, id: i32) -> Option<Rect> {
    l.btns.iter().find(|b| b.id == id).map(|b| b.rc)
}

// ---------------------------------------------------------------------------
// Card view (`go\window.go:1453-1731`).
// ---------------------------------------------------------------------------

/// `cardPresetForButton` (`go\window.go:1463`). Only the State row offers
/// presets: the Details row is removed (line 1 is fixed by the shipped
/// mapping and keeps its stored `Default` value for compatibility), so the
/// retired Details ids (30..33) map to nothing, as do the retired
/// kind-toggle ids (50/51).
#[must_use]
pub fn card_preset_for_button(id: i32) -> Option<(i32, String)> {
    let (_, states) = presence::cardspec::card_presets();
    let i = id - BTN_CARD_STATE_AT;
    if i >= 0 && (i as usize) < states.len() {
        return Some((1, states[i as usize].id.clone()));
    }
    None
}

/// `cardPresetStyle` (`go\window.go:1476`): the active preset is a LIT segment,
/// not an accent fill. Each view has exactly one primary control, and a preset
/// selection is not an action. Only the State row (line 1) offers presets --
/// the Details row is removed, so there is no line-0 control and no line-0
/// branch: any other line reads unlit.
#[must_use]
pub fn card_preset_style(line: i32, id: &str, spec: &CardSpec) -> i32 {
    let spec = spec.normalized();
    if line == 1 && id == spec.state {
        return BTN_TAB_SEL;
    }
    BTN_TAB
}

/// `computeCardLayout`: the one-object composition.
///
/// The window shows exactly one object -- the title PotPlayer is playing -- and
/// everything on screen is either that object, its presence preview, or a
/// correction to it:
///
/// 1. the object card: cover, title, year/kind, and the plain connection state;
/// 2. one query field (title, optional trailing year) and one **Search** -- the
///    only search-like control at rest in the whole window;
/// 3. one **Advanced** disclosure holding every payload knob (field toggles,
///    line-2 modes, and the kind override that exists only for a wrong guess);
/// 4. the presence surfaces: three at 1180x700, the object's own profile
///    preview at 463x703;
/// 5. the results list, which owns the only repair affordance and shows it only
///    while a row is selected.
///
/// `advanced` opens the disclosure and `has_selection` adds the inline repair
/// row. Both are model state, so crossing 1180x700 <-> 463x703 keeps the query,
/// the result set, the selection and the disclosure exactly as they were.
#[must_use]
pub fn compute_card_layout(
    cw: i32,
    ch: i32,
    dpi: i32,
    specs: &CardSpecs,
    kind: CardKind,
    surface: PreviewSurface,
) -> CardLayout {
    compute_card_layout_with(cw, ch, dpi, specs, kind, surface, false, false)
}

/// The picker's model state the Card composition needs beyond the resolved
/// kind: the payload disclosure, whether a result row is selected (the only
/// state in which repair/apply exist), whether applying to the playing title is
/// possible at all, whether a live title is playing (the hero Fix exists only
/// then, never disabled-and-lying), and the explicit search scope.
#[derive(Clone, Copy, Debug)]
pub struct CardLayoutState {
    pub advanced: bool,
    pub has_selection: bool,
    pub can_apply: bool,
    pub playing: bool,
    pub filter: crate::search::KindFilter,
}

impl Default for CardLayoutState {
    fn default() -> Self {
        Self {
            advanced: false,
            has_selection: false,
            can_apply: false,
            playing: false,
            filter: crate::search::KindFilter::All,
        }
    }
}

/// [`compute_card_layout`] plus the disclosure and selection flags. The apply
/// control is never laid out here: this form is the "rest / no player" shape
/// the pinned tests measure.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn compute_card_layout_with(
    cw: i32,
    ch: i32,
    dpi: i32,
    specs: &CardSpecs,
    kind: CardKind,
    surface: PreviewSurface,
    advanced: bool,
    has_selection: bool,
) -> CardLayout {
    compute_card_layout_state(
        cw,
        ch,
        dpi,
        specs,
        kind,
        surface,
        &CardLayoutState {
            advanced,
            has_selection,
            can_apply: false,
            playing: false,
            filter: crate::search::KindFilter::All,
        },
    )
}

/// The approved narrow Card composition (460x680): 48px top bar, hero
/// <=124px, 36px search, results panel to the 24px status bar. No tab strip,
/// no scope row, no Advanced disclosure, no payload knobs — those live in
/// Settings under CARD. Wide (`>=1180x700`) keeps the split workspace below
/// untouched.
#[must_use]
pub fn compute_card_narrow(
    cw: i32,
    ch: i32,
    dpi: i32,
    _specs: &CardSpecs,
    _kind: CardKind,
    _surface: PreviewSurface,
    state: &CardLayoutState,
) -> CardLayout {
    let sc = |v: i32| scale_px(dpi, v);
    let (margin, content_w) = content_span(cw, dpi);
    let mut l = CardLayout {
        compact: ch < sc(480),
        wide: false,
        ..CardLayout::default()
    };
    let topbar_h = sc(TOPBAR_H_L);
    let gap = sc(BAND_GAP_L);
    let status_h = sc(STATUS_BAR_H_L);
    let status_top = (ch - status_h).max(topbar_h);
    l.header = Rect { left: 0, top: 0, right: cw, bottom: topbar_h };
    // No tab strip in the redesign: the top bar replaces it. Keep the rects
    // empty so the old tab paint has nothing to draw twice.
    l.tab_strip = Rect::default();
    l.card_link = Rect::default();
    let gear_side = sc(GEAR_SIDE_L);
    let gear = Rect {
        left: (margin + content_w - gear_side).max(margin),
        top: (topbar_h - gear_side) / 2,
        right: margin + content_w,
        bottom: (topbar_h + gear_side) / 2,
    };
    l.settings = gear;

    // Hero: 64x96 poster, title/meta/status, ghost Fix match on the status row.
    // Tiny viewports cannot fit hero + search + footer above the
    // status bar: when the hero would collide with the pinned footer, the
    // hero band collapses (search and footer stay reachable instead of
    // overlapping). The collapse is all-or-nothing so a half-height hero can
    // never clip the poster.
    let hero_top = topbar_h + gap;
    let hero_pad = sc(SP_3);
    let poster_w = sc(HERO_POSTER_W_L);
    let poster_h = sc(HERO_POSTER_H_L);
    let hero_h_full = (poster_h + hero_pad * 2).min(sc(HERO_MAX_H_L));
    let hero_room = status_top
        - sc(BTN_H)
        - sc(SP_1)
        - sc(32)
        - sc(SP_2)
        - sc(SP_3)
        - sc(BTN_H)
        - sc(SP_2)
        - hero_top;
    let hide_hero = hero_room < hero_h_full;
    let hero_h = if hide_hero { 0 } else { hero_h_full };
    l.object = Rect {
        left: margin,
        top: hero_top,
        right: margin + content_w,
        bottom: hero_top + hero_h,
    };
    if hide_hero {
        l.obj_cover = Rect::default();
        l.obj_title = Rect::default();
        l.obj_meta = Rect::default();
        l.obj_status = Rect::default();
    } else {
        l.obj_cover = Rect {
            left: l.object.left + hero_pad,
            top: l.object.top + hero_pad,
            right: l.object.left + hero_pad + poster_w,
            bottom: l.object.top + hero_pad + poster_h,
        };
        let text_l = l.obj_cover.right + hero_pad;
        let text_r = l.object.right - hero_pad;
        l.obj_title = Rect {
            left: text_l,
            top: l.object.top + hero_pad,
            right: text_r,
            bottom: l.object.top + hero_pad + sc(24),
        };
        l.obj_meta = Rect {
            left: text_l,
            top: l.obj_title.bottom + sc(SP_1),
            right: text_r,
            bottom: l.obj_title.bottom + sc(SP_1) + sc(18),
        };
        l.obj_status = Rect {
            left: text_l,
            top: l.obj_meta.bottom + sc(SP_2),
            right: text_r,
            bottom: (l.obj_meta.bottom + sc(SP_2) + sc(18)).min(l.object.bottom - hero_pad),
        };
    }

    // Search row: field + 96px red Search.
    let search_top = l.object.bottom + gap;
    let search_h = sc(BTN_H);
    let search_w = sc(SEARCH_BTN_W_L);
    l.picker_bar = Rect {
        left: margin,
        top: search_top,
        right: margin + content_w,
        bottom: search_top + search_h,
    };
    let field_rc = Rect {
        left: margin,
        top: search_top,
        right: margin + content_w - search_w - sc(SP_2),
        bottom: search_top + search_h,
    };
    let search_rc = Rect {
        left: field_rc.right + sc(SP_2),
        top: search_top,
        right: margin + content_w,
        bottom: search_top + search_h,
    };

    // Results panel fills to the status bar; tiny viewports scroll below it.
    // No scope row: universal search runs both chains for every query, so the
    // list starts directly under the search row.
    let panel_top = l.picker_bar.bottom + gap;
    let panel = if panel_top > status_top {
        // Pinned bands already exceed the client: extend the panel below the
        // fold so footer controls never overlap the search row. The
        // pinned status bar overlays it (chrome, skipped in overlap checks).
        Rect {
            left: margin,
            top: panel_top,
            right: margin + content_w,
            bottom: panel_top + sc(200),
        }
    } else {
        Rect {
            left: margin,
            top: panel_top,
            right: margin + content_w,
            bottom: status_top,
        }
    };
    l.stage = panel;
    // List area above the pinned footer (Apply 36 + gaps). The footer anchors
    // to the visible bottom (`status_top`), never to `panel.bottom`: when the
    // pinned bands already exceed a tiny client the panel extends below the
    // fold, but Apply stays inside the client above the status bar.
    let foot = status_top.min(panel.bottom);
    let apply_top = foot - sc(SP_2) - sc(32) - sc(SP_1) - sc(BTN_H);
    l.picker_results = Rect {
        left: panel.left,
        top: panel_top,
        right: panel.right,
        bottom: (apply_top - sc(SP_1)).max(panel_top).min(foot),
    };
    // Preview surfaces unused in narrow redesign: empty so old paint skips.
    l.preview = Rect::default();
    l.surfaces = [Rect::default(), Rect::default(), Rect::default()];
    l.rail = Rect {
        left: margin,
        top: hero_top,
        right: margin + content_w,
        bottom: panel_top,
    };
    l.title = l.obj_title;
    l.meta = l.obj_meta;
    l.synopsis = l.obj_status;
    l.art = l.obj_cover;
    l.rows = vec![l.title, l.meta];
    l.picker_status = Rect::default();
    l.advanced_panel = Rect::default();
    l.hint = Rect::default();
    l.wire = Rect::default();
    l.tooltip = Rect::default();
    l.labels = Vec::new();

    l.btns = vec![
        UiButton {
            id: BTN_SETTINGS,
            label: "⚙".into(),
            style: BTN_TEXT,
            rc: gear,
        },
        UiButton {
            id: BTN_PICKER_FIELD,
            label: String::new(),
            style: BTN_OUTLINE,
            rc: field_rc,
        },
        UiButton {
            id: BTN_PICKER_SEARCH,
            label: "Search".into(),
            style: BTN_ACCENT,
            rc: search_rc,
        },
    ];
    // Hero ghost on the status row, right-aligned — present only while a live
    // title is playing (and the hero band is shown), absent (not
    // disabled) with nothing playing or when tiny clients collapse the hero.
    if state.playing && !hide_hero {
        let ghost_w = sc(96);
        let text_l = l.obj_cover.right + hero_pad;
        let text_r = l.object.right - hero_pad;
        l.btns.push(UiButton {
            id: BTN_FIX_MATCH,
            label: FIX_MATCH_LABEL.into(),
            style: BTN_OUTLINE,
            rc: Rect {
                left: (text_r - ghost_w).max(text_l),
                top: l.obj_status.top,
                right: text_r,
                bottom: (l.obj_status.top + sc(32)).min(l.object.bottom - hero_pad),
            },
        });
    }
    // Apply pinned to the visible bottom (one 32px text row of breathing room
    // to the status bar). `foot` is the visible bottom, so Apply stays inside
    // the client even when the panel overflows a tiny viewport.
    l.btns.push(UiButton {
        id: BTN_PICKER_APPLY,
        label: "Apply to playing title".into(),
        style: if state.can_apply { BTN_ACCENT } else { BTN_OUTLINE },
        rc: Rect {
            left: panel.left + sc(SP_3),
            top: apply_top,
            right: panel.right - sc(SP_3),
            bottom: apply_top + sc(BTN_H),
        },
    });
    // Back stays wired for Esc/keyboard, hidden in the new anatomy.
    l.btns.push(UiButton {
        id: BTN_CARD_BACK,
        label: "Back".into(),
        style: BTN_OUTLINE,
        rc: Rect::default(),
    });
    l.btns.push(UiButton {
        id: BTN_CARD,
        label: "Card".into(),
        style: BTN_TAB_SEL,
        rc: Rect::default(),
    });
    l.status = Rect {
        left: 0,
        top: status_top,
        right: cw,
        bottom: status_top + status_h,
    };
    push_synopsis_button(&mut l);
    let _ = gap;
    l
}

/// The full Card composition.
#[must_use]
pub fn compute_card_layout_state(
    cw: i32,
    ch: i32,
    dpi: i32,
    specs: &CardSpecs,
    kind: CardKind,
    surface: PreviewSurface,
    state: &CardLayoutState,
) -> CardLayout {
    let wide = cw >= scale_px(dpi, 1180) && ch >= scale_px(dpi, 700);
    if !wide {
        return compute_card_narrow(cw, ch, dpi, specs, kind, surface, state);
    }
    let advanced = state.advanced;
    let has_selection = state.has_selection;    let sc = |v: i32| scale_px(dpi, v);
    let spec = specs.for_kind(kind);
    let compact = ch < sc(480);
    // Wide branch only: narrow returns above via `compute_card_narrow`.
    let header_h = if compact { 0 } else { sc(HEADER_H_L) };
    let tab_top = if compact { sc(SP_2) } else { sc(TAB_TOP_L) };
    let tab_h = sc(TAB_H_L);
    let tab_bottom = tab_top + tab_h;
    let status_h = if compact { 0 } else { sc(STATUS_H_L) };
    let status_top = ch - sc(BOTTOM_L) - status_h;
    let (_, states) = presence::cardspec::card_presets();
    let seg_gap = sc(SP_2);
    let designer = presence::live_designer();

    let mut l = CardLayout {
        compact,
        wide,
        ..CardLayout::default()
    };
    l.header = Rect {
        left: 0,
        top: 0,
        right: cw,
        bottom: header_h,
    };
    let (mut card_tab, mut settings_tab) = tab_strip_rects(cw, dpi);
    for r in [&mut card_tab, &mut settings_tab] {
        r.top = tab_top;
        r.bottom = tab_bottom;
    }
    l.tab_strip = Rect {
        left: card_tab.left,
        top: card_tab.top,
        right: settings_tab.right,
        bottom: card_tab.bottom,
    };
    l.card_link = card_tab;
    l.settings = settings_tab;

    // One segmented row over `items`; the last cell lands exactly on `right`.
    let segments = |left: i32,
                    right: i32,
                    top: i32,
                    h: i32,
                    items: &[(i32, String, i32)]|
     -> Vec<UiButton> {
        let n = items.len() as i32;
        if n == 0 {
            return Vec::new();
        }
        let w = ((right - left) - seg_gap * (n - 1)) / n;
        items
            .iter()
            .enumerate()
            .map(|(i, (id, label, style))| {
                let i = i as i32;
                let x = left + i * (w + seg_gap);
                UiButton {
                    id: *id,
                    label: label.clone(),
                    style: *style,
                    rc: Rect {
                        left: x,
                        top,
                        right: if i == n - 1 { right } else { x + w },
                        bottom: top + h,
                    },
                }
            })
            .collect()
    };

    let (margin, content_w) = content_span(cw, dpi);
    let rail_w = if wide { sc(400).min(content_w / 2) } else { content_w };
    let col_left = margin;
    let col_right = col_left + rail_w;
    let stage_left = if wide { col_right + sc(SP_6) } else { margin };
    let stage_right = margin + content_w;

    // --- 1. the object card ------------------------------------------------
    let top = tab_bottom + sc(SP_3);
    let obj_h = sc(if compact { 96 } else { 128 });
    l.object = Rect {
        left: col_left,
        top,
        right: col_right,
        bottom: top + obj_h,
    };
    let opad = sc(SP_3);
    let cover_w = sc(if compact { 48 } else { 68 });
    l.obj_cover = Rect {
        left: l.object.left + opad,
        top: l.object.top + opad,
        right: (l.object.left + opad + cover_w).min(l.object.right - opad),
        bottom: l.object.bottom - opad,
    };
    let text_l = l.obj_cover.right + sc(SP_3);
    let text_r = l.object.right - opad;
    l.obj_title = Rect {
        left: text_l,
        top: l.object.top + opad,
        right: text_r,
        bottom: l.object.top + opad + sc(24),
    };
    l.obj_meta = Rect {
        left: text_l,
        top: l.obj_title.bottom + sc(SP_1),
        right: text_r,
        bottom: l.obj_title.bottom + sc(SP_1) + sc(18),
    };
    l.obj_status = Rect {
        left: text_l,
        top: l.obj_meta.bottom + sc(SP_2),
        right: text_r,
        bottom: l.obj_meta.bottom + sc(SP_2) + sc(18),
    };

    // --- 2. one field, one verb -------------------------------------------
    let field_h = sc(BTN_H);
    let search_top = l.object.bottom + sc(SP_3);
    let search_w = sc(84);
    let gap = sc(SP_2);
    l.picker_bar = Rect {
        left: col_left,
        top: search_top,
        right: col_right,
        bottom: search_top + field_h,
    };
    let field_right = col_right - search_w - gap;
    let field_rc = Rect {
        left: col_left,
        top: search_top,
        right: field_right,
        bottom: search_top + field_h,
    };
    let search_rc = Rect {
        left: field_right + gap,
        top: search_top,
        right: col_right,
        bottom: search_top + field_h,
    };

    // --- 3. one disclosure -------------------------------------------------
    let adv_top = l.picker_bar.bottom + sc(SP_1);
    let adv_h = sc(32);
    let adv_w = sc(132);
    let advanced_rc = Rect {
        left: col_left,
        top: adv_top,
        right: col_left + adv_w,
        bottom: adv_top + adv_h,
    };
    // The search's own readout shares the disclosure row: it never costs a line
    // of its own, so the field can explain a failure without moving the list.
    l.picker_status = Rect {
        left: advanced_rc.right + gap,
        top: adv_top + sc(SP_2),
        right: col_right,
        bottom: adv_top + adv_h,
    };
    let panel_top = adv_top + adv_h + sc(SP_2);
    let panel_h = sc(166);
    let footer_top = status_top - sc(SP_3) - sc(BTN_H);
    // Very short clients have no footer row of their own: Back shares the
    // disclosure row, and a disclosure with nowhere to open stays shut.
    let share_row = footer_top < adv_top + adv_h + sc(SP_2) + sc(BTN_H);
    let advanced = advanced && !share_row;
    if advanced {
        l.advanced_panel = Rect {
            left: col_left,
            top: panel_top,
            right: col_right,
            bottom: panel_top + panel_h,
        };
    }

    let stage_top = if wide {
        top
    } else if advanced {
        l.advanced_panel.bottom + sc(SP_3)
    } else {
        panel_top
    };
    let stage_bottom = if share_row {
        ch - sc(SP_2)
    } else {
        footer_top - sc(SP_2)
    };
    let stage = Rect {
        left: stage_left,
        top: stage_top,
        right: stage_right,
        bottom: stage_bottom.max(stage_top + sc(60)).min(ch - sc(SP_1)).max(stage_top),
    };
    l.stage = stage;
    l.tooltip = Rect::default();
    // The results list occupies the stage: the two are painted exclusively, so
    // one rect cannot be both a preview and a result set.
    l.picker_results = stage;

    // --- 4. the presence surfaces -----------------------------------------
    if wide {
        let stage_h = stage.bottom - stage.top;
        let member_h = sc(140).min((stage_h - sc(SP_2)).max(sc(60)));
        let member = Rect {
            left: stage.left,
            top: stage.top,
            right: stage.right,
            bottom: stage.top + member_h,
        };
        let lower_top = member.bottom + sc(SP_2);
        let half = (stage.right - stage.left - sc(SP_4)) / 2;
        let profile = Rect {
            left: stage.left,
            top: lower_top,
            right: stage.left + half,
            bottom: stage.bottom,
        };
        let activity = Rect {
            left: profile.right + sc(SP_4),
            top: lower_top,
            right: stage.right,
            bottom: stage.bottom,
        };
        l.surfaces = [member, profile, activity];
        l.preview = profile;
        let pad = sc(SP_4);
        let banner_h = sc(30);
        let avatar_h = sc(54);
        let title_top = profile.top + banner_h + sc(18) + avatar_h + sc(SP_2);
        l.title = Rect {
            left: profile.left + pad,
            top: title_top,
            right: profile.right - pad,
            bottom: title_top + sc(26),
        };
        l.meta = Rect {
            left: l.title.left,
            top: l.title.bottom + sc(SP_1),
            right: l.title.right,
            bottom: l.title.bottom + sc(SP_1) + sc(18),
        };
        let activity_top = l.meta.bottom + sc(SP_3);
        let art_w = sc(72).min((profile.right - profile.left) / 3);
        l.art = Rect {
            left: l.title.left,
            top: activity_top,
            right: l.title.left + art_w,
            bottom: (activity_top + sc(96)).min(profile.bottom - pad),
        };
        l.synopsis = Rect {
            left: (l.art.right + sc(SP_3)).min(profile.right - pad),
            top: activity_top + sc(72),
            right: profile.right - pad,
            bottom: (activity_top + sc(72) + sc(18)).min(profile.bottom - pad),
        };
    } else {
        let stage_h = stage.bottom - stage.top;
        l.preview = stage;
        l.surfaces = [stage, stage, stage];
        if stage_h >= sc(120) {
            let pad = sc(SP_3);
            let art_w = sc(64);
            let art_h = sc(96);
            let art_top = stage.top + pad;
            l.art = Rect {
                left: stage.left + pad,
                top: art_top,
                right: (stage.left + pad + art_w).min(stage.right - pad),
                bottom: (art_top + art_h).min(stage.bottom - pad),
            };
            let text_l = l.art.right + sc(SP_3);
            l.title = Rect {
                left: text_l,
                top: art_top,
                right: stage.right - pad,
                bottom: art_top + sc(24),
            };
            l.meta = Rect {
                left: text_l,
                top: l.title.bottom + sc(SP_1),
                right: stage.right - pad,
                bottom: l.title.bottom + sc(SP_1) + sc(18),
            };
            l.synopsis = Rect {
                left: text_l,
                top: l.meta.bottom + sc(SP_2),
                right: stage.right - pad,
                bottom: (l.meta.bottom + sc(SP_2) + sc(18)).min(stage.bottom - pad),
            };
        }
    }
    l.rows = vec![l.title, l.meta, l.synopsis];
    // The rail is the whole column the object and its query live in; the stage
    // is what is left over for the preview or the results.
    l.rail = Rect {
        left: col_left,
        top: l.object.top,
        right: col_right,
        bottom: footer_top.max(l.object.bottom),
    };
    let _ = surface;

    // --- the control table -------------------------------------------------
    l.btns = vec![
        UiButton {
            id: BTN_CARD,
            label: "Card".into(),
            style: BTN_TAB,
            rc: l.card_link,
        },
        UiButton {
            id: BTN_SETTINGS,
            label: "Settings".into(),
            style: BTN_TAB,
            rc: l.settings,
        },
        UiButton {
            id: BTN_PICKER_FIELD,
            label: String::new(),
            style: BTN_OUTLINE,
            rc: field_rc,
        },
        UiButton {
            id: BTN_PICKER_SEARCH,
            label: "Search".into(),
            style: BTN_ACCENT,
            rc: search_rc,
        },
        UiButton {
            id: BTN_ADVANCED,
            label: if advanced { "Advanced \u{25be}" } else { "Advanced \u{25b8}" }.into(),
            style: BTN_TEXT,
            rc: advanced_rc,
        },
    ];
    if advanced {
        // Universal search: no scope segments and no category words. The
        // disclosure keeps the designer field toggles (covers are always on,
        // so no artwork row) and the line-2 state presets.
        let field_items: Vec<(i32, String, i32)> = vec![
            (
                BTN_FIELD_DETAILS,
                "Details".to_string(),
                if designer.details { BTN_TOGGLE_ON } else { BTN_TOGGLE },
            ),
            (
                BTN_FIELD_STATE,
                "State".to_string(),
                if designer.state { BTN_TOGGLE_ON } else { BTN_TOGGLE },
            ),
            (
                BTN_FIELD_SYNOPSIS,
                "Synopsis".to_string(),
                if designer.synopsis { BTN_TOGGLE_ON } else { BTN_TOGGLE },
            ),
            (
                BTN_FIELD_TIMERS,
                "Timers".to_string(),
                if designer.timers { BTN_TOGGLE_ON } else { BTN_TOGGLE },
            ),
        ];
        l.btns.extend(segments(
            l.advanced_panel.left,
            l.advanced_panel.right,
            l.advanced_panel.top,
            sc(32),
            &field_items,
        ));
        let si: Vec<(i32, String, i32)> = states
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    card_state_button(i as i32),
                    p.label.clone(),
                    card_preset_style(1, &p.id, &spec),
                )
            })
            .collect();
        l.labels.push(NamedTextRect {
            id: "card.line2".into(),
            rc: Rect {
                left: l.advanced_panel.left,
                top: l.advanced_panel.top + (sc(32) + sc(SP_2)),
                right: l.advanced_panel.right,
                bottom: l.advanced_panel.top + (sc(32) + sc(SP_2)) + sc(18),
            },
        });
        l.btns.extend(segments(
            l.advanced_panel.left,
            l.advanced_panel.right,
            l.advanced_panel.top + (sc(32) + sc(SP_2)) + sc(18),
            sc(32),
            &si,
        ));
    }
    l.btns.push(UiButton {
        id: BTN_CARD_BACK,
        label: "Back".into(),
        style: BTN_OUTLINE,
        rc: if share_row {
            Rect {
                left: (col_right - sc(84)).max(l.picker_status.left),
                top: adv_top,
                right: col_right,
                bottom: adv_top + adv_h,
            }
        } else {
            Rect {
                left: col_left,
                top: footer_top,
                right: col_left + if wide { rail_w } else { content_w },
                bottom: footer_top + sc(BTN_H),
            }
        },
    });
    // The list's own actions. Both exist ONLY with a selection and only inside
    // the results list -- never as a peer of the field, never at rest.
    if has_selection {
        let row_h = sc(32);
        let gap = sc(SP_1);
        let mut bottom = l.picker_results.bottom - sc(SP_2);
        if state.can_apply {
            // Applying a match changes what the CURRENTLY PLAYING title is;
            // with nothing playing there is nothing to apply it to, so the
            // control is absent rather than disabled-and-lying.
            l.btns.push(UiButton {
                id: BTN_PICKER_APPLY,
                label: "Apply to playing title".into(),
                style: BTN_ACCENT,
                rc: Rect {
                    left: l.picker_results.left + sc(SP_2),
                    top: (bottom - row_h).max(l.picker_results.top),
                    right: l.picker_results.right - sc(SP_2),
                    bottom,
                },
            });
            bottom -= row_h + gap;
        }
        if !advanced {
            // With the Advanced disclosure already open this link is redundant,
            // and its row would crowd out the result the user is looking at.
            l.btns.push(UiButton {
                id: BTN_FIX_MATCH,
                label: "Not this one? Open advanced match\u{2026}".into(),
                style: BTN_TEXT,
                rc: Rect {
                    left: l.picker_results.left + sc(SP_2),
                    top: (bottom - row_h).max(l.picker_results.top),
                    right: l.picker_results.right - sc(SP_2),
                    bottom,
                },
            });
        }
    }
    l.status = Rect {
        left: margin,
        top: status_top,
        right: margin + content_w,
        bottom: status_top + status_h,
    };
    l.wire = Rect::default();
    let _ = tab_h;
    push_synopsis_button(&mut l);
    l
}
/// The **superseded** composition (`v1`): the designer split workspace with the
/// Movie/TV choice in the primary band, `Fix match…` as a peer footer button and
/// twenty interactive controls at rest. It is no longer painted or hit-tested.
///
/// It survives only as the pinned "before" arm of the two red-capable delta
/// tests (`card_kind_never_renders_movie_for_a_series_match` and
/// `advanced_disclosure_keeps_the_rest_state_at_three_controls`, both in this
/// file's test module): a threshold test that cannot show the pre-fix numbers
/// going red proves nothing. Nothing outside those tests calls it.
#[allow(dead_code)]
pub fn compute_card_layout_shipped_v1(
    cw: i32,
    ch: i32,
    dpi: i32,
    specs: &CardSpecs,
    kind: CardKind,
    surface: PreviewSurface,
) -> CardLayout {
    let sc = |v: i32| scale_px(dpi, v);
    let spec = specs.for_kind(kind);
    let compact = ch < sc(480);
    // The split workspace needs a readable stage for three surfaces; below
    // this width (or height) the narrow stack shows one surface at full width.
    let wide = cw >= sc(1180) && ch >= sc(700);
    let header_h = if compact { 0 } else { sc(HEADER_H_L) };
    let tab_top = if compact { sc(SP_2) } else { sc(TAB_TOP_L) };
    let tab_h = sc(TAB_H_L);
    let tab_bottom = tab_top + tab_h;
    let label_h = sc(SECTION_H_L);
    let status_h = if compact { 0 } else { sc(STATUS_H_L) };
    let status_top = ch - sc(BOTTOM_L) - status_h;
    let (_, states) = presence::cardspec::card_presets();
    let seg_gap = sc(SP_2);
    let designer = presence::live_designer();

    let mut l = CardLayout {
        compact,
        wide,
        ..CardLayout::default()
    };
    l.header = Rect {
        left: 0,
        top: 0,
        right: cw,
        bottom: header_h,
    };
    let (mut card_tab, mut settings_tab) = tab_strip_rects(cw, dpi);
    for r in [&mut card_tab, &mut settings_tab] {
        r.top = tab_top;
        r.bottom = tab_bottom;
    }
    l.tab_strip = Rect {
        left: card_tab.left,
        top: card_tab.top,
        right: settings_tab.right,
        bottom: card_tab.bottom,
    };
    l.card_link = card_tab;
    l.settings = settings_tab;

    // One segmented row over `items`; the last cell lands exactly on `right`.
    let segments = |left: i32,
                    right: i32,
                    top: i32,
                    h: i32,
                    items: &[(i32, String, i32)]|
     -> Vec<UiButton> {
        let n = items.len() as i32;
        if n == 0 {
            return Vec::new();
        }
        let w = ((right - left) - seg_gap * (n - 1)) / n;
        items
            .iter()
            .enumerate()
            .map(|(i, (id, label, style))| {
                let i = i as i32;
                let x = left + i * (w + seg_gap);
                UiButton {
                    id: *id,
                    label: label.clone(),
                    style: *style,
                    rc: Rect {
                        left: x,
                        top,
                        right: if i == n - 1 { right } else { x + w },
                        bottom: top + h,
                    },
                }
            })
            .collect()
    };
    let surface_items = |sel: PreviewSurface| -> Vec<(i32, String, i32)> {
        [
            (BTN_SURFACE_MEMBER, "Members", PreviewSurface::Member),
            (BTN_SURFACE_PROFILE, "Profile", PreviewSurface::Profile),
            (BTN_SURFACE_ACTIVITY, "Activity", PreviewSurface::Activity),
        ]
        .into_iter()
        .map(|(id, label, s)| {
            (
                id,
                label.to_string(),
                if s == sel { BTN_TAB_SEL } else { BTN_TAB },
            )
        })
        .collect()
    };
    // The picker band is one row in every composition: the query field, the
    // explicit Movie/TV choice, the primary Search/Match action, and (when the
    // width allows) a status readout. The field keeps the same trailing-year
    // contract as the Fix match dialog; no second Year control exists.
    let picker_items = |left: i32, right: i32, top: i32| -> (Rect, Rect, Rect, Rect, Rect) {
        let gap = sc(6);
        let wide_enough = right - left >= sc(520);
        let search_w = sc(if wide_enough { 76 } else { 58 });
        let kind_w = sc(if wide_enough { 60 } else { 46 });
        let status_w = if wide_enough { sc(110) } else { 0 };
        let field_w = (right - left - search_w - 2 * kind_w - status_w - 4 * gap).max(sc(80));
        let h = sc(32);
        let field = Rect {
            left,
            top: top + sc(4),
            right: left + field_w,
            bottom: top + sc(4) + h,
        };
        let movie = Rect {
            left: field.right + gap,
            top: field.top,
            right: field.right + gap + kind_w,
            bottom: field.bottom,
        };
        let tv = Rect {
            left: movie.right + gap,
            top: field.top,
            right: movie.right + gap + kind_w,
            bottom: field.bottom,
        };
        let search = Rect {
            left: tv.right + gap,
            top: field.top,
            right: tv.right + gap + search_w,
            bottom: field.bottom,
        };
        let status = Rect {
            left: search.right + gap,
            top: field.top,
            right: if status_w > 0 { right } else { search.right },
            bottom: field.bottom,
        };
        (field, movie, tv, search, status)
    };
    let field_items = |short: bool| -> Vec<(i32, String, i32)> {
        let name = |on: bool, long: &str, brief: &str| {
            (
                if on { BTN_TOGGLE_ON } else { BTN_TOGGLE },
                if short { brief } else { long }.to_string(),
            )
        };
        let (s_det, det) = name(designer.details, "Details", "Details");
        let (s_sta, sta) = name(designer.state, "State", "State");
        let (s_syn, syn) = name(designer.synopsis, "Synopsis", "Synopsis");
        let (s_tim, tim) = name(designer.timers, "Timers", "Time");
        // Covers are always on: no artwork entry.
        vec![
            (BTN_FIELD_DETAILS, det, s_det),
            (BTN_FIELD_STATE, sta, s_sta),
            (BTN_FIELD_SYNOPSIS, syn, s_syn),
            (BTN_FIELD_TIMERS, tim, s_tim),
        ]
    };

    if wide {
        // Split workspace: the editor rail on the left, the three live Discord
        // surfaces on the right, all visible at once.
        let m = sc(32);
        let content_w = (cw - 2 * m).max(sc(600));
        let content_top = tab_bottom + sc(SP_4);
        let content_bottom = (status_top - sc(SP_3)).max(content_top + sc(320));
        let rail_w = sc(400).min(content_w / 2);
        l.rail = Rect {
            left: m,
            top: content_top,
            right: m + rail_w,
            bottom: content_bottom,
        };
        let stage_left = l.rail.right + sc(SP_6);
        l.picker_bar = Rect {
            left: stage_left,
            top: content_top,
            right: m + content_w,
            bottom: content_top + sc(40),
        };
        l.stage = Rect {
            left: stage_left,
            top: l.picker_bar.bottom + sc(SP_2),
            right: m + content_w,
            bottom: content_bottom,
        };
        l.picker_results = Rect {
            left: l.stage.left,
            top: l.stage.top,
            right: l.stage.right,
            bottom: (l.stage.top + sc(250)).min(l.stage.bottom),
        };
        let stage_w = l.stage.right - l.stage.left;
        let stage_h = l.stage.bottom - l.stage.top;
        // At the 1180x700 workspace the profile surface needs the lower band
        // to keep its 132px cover and the timer labels inside the panel; at
        // taller clients the member list takes its full 180px.
        let member_h = if stage_h < sc(520) {
            sc(140)
        } else {
            sc(180).min(stage_h / 3)
        };
        let member = Rect {
            left: l.stage.left,
            top: l.stage.top,
            right: l.stage.right,
            bottom: l.stage.top + member_h,
        };
        let lower_top = member.bottom + sc(SP_2);
        let half = (stage_w - sc(SP_4)) / 2;
        let profile = Rect {
            left: l.stage.left,
            top: lower_top,
            right: l.stage.left + half,
            bottom: l.stage.bottom,
        };
        let activity = Rect {
            left: profile.right + sc(SP_4),
            top: lower_top,
            right: l.stage.right,
            bottom: l.stage.bottom,
        };
        l.surfaces = [member, profile, activity];
        l.preview = profile;

        // The profile surface's media rows.
        let pad = sc(SP_4);
        let banner_h = sc(30);
        let avatar_h = sc(54);
        let title_top = profile.top + banner_h + sc(18) + avatar_h + sc(SP_2);
        l.title = Rect {
            left: profile.left + pad,
            top: title_top,
            right: profile.right - pad,
            bottom: title_top + sc(28),
        };
        l.meta = Rect {
            left: l.title.left,
            top: l.title.bottom + sc(SP_1),
            right: l.title.right,
            bottom: l.title.bottom + sc(SP_1) + sc(18),
        };
        let activity_top = l.meta.bottom + sc(SP_3);
        let art_w = sc(96).min((profile.right - profile.left) / 3);
        let art_h = sc(132);
        l.art = Rect {
            left: l.title.left,
            top: activity_top,
            right: l.title.left + art_w,
            bottom: (activity_top + art_h).min(profile.bottom - pad),
        };
        let text_left = l.art.right + sc(SP_3);
        l.synopsis = Rect {
            left: text_left,
            top: activity_top + sc(96),
            right: profile.right - pad,
            bottom: (activity_top + sc(96) + sc(20)).min(profile.bottom - pad),
        };
        l.rows = vec![l.title, l.meta, l.synopsis];
        l.tooltip = Rect::default();

        // The rail's stack: heading, context, preview switch, fields, state
        // presets, wire summary, footer actions.
        l.labels = vec![
            NamedTextRect {
                id: "card.designer".into(),
                rc: Rect {
                    left: l.rail.left + sc(SP_4),
                    top: l.rail.top,
                    right: l.rail.right - sc(SP_4),
                    bottom: l.rail.top + label_h,
                },
            },
            NamedTextRect {
                id: "card.context".into(),
                rc: Rect {
                    left: l.rail.left + sc(SP_4),
                    top: l.rail.top + label_h,
                    right: l.rail.right - sc(SP_4),
                    bottom: l.rail.top + label_h + sc(18),
                },
            },
            NamedTextRect {
                id: "card.fields".into(),
                rc: Rect {
                    left: l.rail.left + sc(SP_4),
                    top: l.rail.top + sc(120),
                    right: l.rail.right - sc(SP_4),
                    bottom: l.rail.top + sc(120) + label_h,
                },
            },
            NamedTextRect {
                id: "card.line2".into(),
                rc: Rect {
                    left: l.rail.left + sc(SP_4),
                    top: l.rail.top + sc(230),
                    right: l.rail.right - sc(SP_4),
                    bottom: l.rail.top + sc(230) + label_h,
                },
            },
            NamedTextRect {
                id: "card.wire".into(),
                rc: Rect {
                    left: l.rail.left + sc(SP_4),
                    top: l.rail.top + sc(350),
                    right: l.rail.right - sc(SP_4),
                    bottom: l.rail.top + sc(350) + label_h,
                },
            },
        ];
        l.wire = Rect {
            left: l.rail.left + sc(SP_4),
            top: l.rail.top + sc(350) + label_h + sc(SP_1),
            right: l.rail.right - sc(SP_4),
            bottom: l.rail.top + sc(350) + label_h + sc(SP_1) + sc(46),
        };
        let rail_inner_l = l.rail.left + sc(SP_4);
        let rail_inner_r = l.rail.right - sc(SP_4);
        let footer_top = l.rail.bottom - sc(BTN_H);
        l.btns = vec![
            UiButton {
                id: BTN_CARD,
                label: "Card".into(),
                style: BTN_TAB,
                rc: l.card_link,
            },
            UiButton {
                id: BTN_SETTINGS,
                label: "Settings".into(),
                style: BTN_TAB,
                rc: l.settings,
            },
        ];
        // The picker band: one query field and the one primary action. The
        // field is the same single-input contract as the dialog; the layout
        // carries no second year control and no kind choice: universal
        // search runs both chains for every query.
        let (pf, _pm, _pt, ps, pstat) = picker_items(
            l.picker_bar.left,
            l.picker_bar.right,
            l.picker_bar.top,
        );
        l.picker_status = pstat;
        l.btns.push(UiButton {
            id: BTN_PICKER_FIELD,
            label: String::new(),
            style: BTN_OUTLINE,
            rc: pf,
        });
        l.btns.push(UiButton {
            id: BTN_PICKER_SEARCH,
            label: "Search".into(),
            style: BTN_ACCENT,
            rc: ps,
        });
        // Preview switch row (all three already visible; the choice marks the
        // focused surface and keeps the keyboard path identical to narrow).
        l.btns.extend(segments(
            rail_inner_l,
            rail_inner_r,
            l.rail.top + sc(56),
            sc(30),
            &surface_items(surface),
        ));
        // Fields: 3 + 2 rows.
        let fi = field_items(false);
        l.btns.extend(segments(
            rail_inner_l,
            rail_inner_r,
            l.rail.top + sc(120) + label_h + sc(SP_1),
            sc(34),
            &fi[..3],
        ));
        l.btns.extend(segments(
            rail_inner_l,
            rail_inner_r,
            l.rail.top + sc(120) + label_h + sc(SP_1) + sc(42),
            sc(34),
            &fi[3..],
        ));
        // State presets: 2 + 2 rows.
        let mut si: Vec<(i32, String, i32)> = states
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    card_state_button(i as i32),
                    p.label.clone(),
                    card_preset_style(1, &p.id, &spec),
                )
            })
            .collect();
        let si2 = si.split_off(2);
        l.btns.extend(segments(
            rail_inner_l,
            rail_inner_r,
            l.rail.top + sc(230) + label_h + sc(SP_1),
            sc(BTN_H),
            &si,
        ));
        l.btns.extend(segments(
            rail_inner_l,
            rail_inner_r,
            l.rail.top + sc(230) + label_h + sc(SP_1) + sc(44),
            sc(BTN_H),
            &si2,
        ));
        let col_w = (rail_inner_r - rail_inner_l - sc(BTN_GAP_L)) / 2;
        l.btns.push(UiButton {
            id: BTN_CARD_BACK,
            label: "Back".into(),
            style: BTN_OUTLINE,
            rc: Rect {
                left: rail_inner_l,
                top: footer_top,
                right: rail_inner_l + col_w,
                bottom: footer_top + sc(BTN_H),
            },
        });
        l.btns.push(UiButton {
            id: BTN_FIX_MATCH,
            label: FIX_MATCH_LABEL.into(),
            style: BTN_OUTLINE,
            rc: Rect {
                left: rail_inner_l + col_w + sc(BTN_GAP_L),
                top: footer_top,
                right: rail_inner_r,
                bottom: footer_top + sc(BTN_H),
            },
        });
        l.status = Rect {
            left: m,
            top: status_top,
            right: m + content_w,
            bottom: status_top + status_h,
        };
        l.hint = Rect {
            left: rail_inner_l,
            top: l.wire.bottom + sc(SP_1),
            right: rail_inner_r,
            bottom: (l.wire.bottom + sc(SP_1) + sc(16)).min(footer_top - sc(SP_1)),
        };
    } else {
        // Narrow stack: preview, then the field rows, the footer and status.
        let (margin, content_w) = content_span(cw, dpi);
        l.rail = Rect {
            left: margin,
            top: tab_bottom + sc(SP_2),
            right: margin + content_w,
            bottom: status_top,
        };
        l.picker_bar = Rect {
            left: margin,
            top: tab_bottom + sc(SP_2),
            right: margin + content_w,
            bottom: tab_bottom + sc(SP_2) + sc(40),
        };
        let label_top = l.picker_bar.bottom + sc(SP_2);
        let label_band_h = if compact { 0 } else { sc(24) };
        let preview_top = label_top + label_band_h + sc(SP_2);
        let footer_top = status_top - sc(SP_3) - sc(BTN_H);
        let hint_h = sc(16);
        let hint_top = footer_top - sc(SP_2) - hint_h;
        let state_row_top = hint_top - sc(SP_2) - sc(BTN_H);
        let state_label_top = state_row_top - sc(SP_1) - label_h;
        let field_h = sc(32);
        let fields_row_top = state_label_top - sc(SP_3) - field_h;
        let fields_label_top = fields_row_top - sc(SP_1) - label_h;
        let preview_bottom = if compact {
            footer_top - sc(SP_2)
        } else {
            fields_label_top - sc(SP_4)
        };
        let preview = Rect {
            left: margin,
            top: preview_top,
            right: margin + content_w,
            bottom: preview_bottom.max(preview_top + sc(80)),
        };
        l.preview = preview;
        l.surfaces = [preview, preview, preview];
        l.stage = preview;
        l.picker_results = Rect {
            left: preview.left,
            top: l.picker_bar.bottom + sc(SP_1),
            right: preview.right,
            bottom: preview.bottom.max(l.picker_bar.bottom + sc(120)),
        };
        let pad = sc(if compact { SP_2 } else { SP_3 });
        let art_w = if compact { sc(48) } else { sc(72) };
        let art_h = if compact { sc(64) } else { sc(112) };
        // Centre the media block in the panel: the narrow stack gives the
        // preview the leftover height, so a top-aligned block would float.
        let art_top = preview.top
            + ((preview.bottom - preview.top - art_h).max(0) / 2).max(pad);
        l.art = Rect {
            left: preview.left + pad,
            top: art_top,
            right: (preview.left + pad + art_w).min(preview.right - pad),
            bottom: (art_top + art_h).min(preview.bottom - pad),
        };
        let text_left = l.art.right + sc(SP_3);
        let title_h = if compact { sc(20) } else { sc(26) };
        l.title = Rect {
            left: text_left,
            top: art_top,
            right: preview.right - pad,
            bottom: art_top + title_h,
        };
        l.meta = Rect {
            left: text_left,
            top: l.title.bottom + sc(SP_1),
            right: preview.right - pad,
            bottom: l.title.bottom + sc(SP_1) + sc(18),
        };
        l.synopsis = Rect {
            left: text_left,
            top: l.meta.bottom + sc(SP_2),
            right: preview.right - pad,
            bottom: (l.meta.bottom + sc(SP_2) + sc(20)).min(preview.bottom - pad),
        };
        l.rows = vec![l.title, l.meta, l.synopsis];
        l.tooltip = Rect::default();

        l.labels = Vec::new();
        if !compact {
            l.labels.push(NamedTextRect {
                id: "card.preview-label".into(),
                rc: Rect {
                    left: margin,
                    top: label_top,
                    right: margin + content_w / 3,
                    bottom: label_top + label_band_h,
                },
            });
            l.labels.push(NamedTextRect {
                id: "card.fields".into(),
                rc: Rect {
                    left: margin,
                    top: fields_label_top,
                    right: margin + content_w,
                    bottom: fields_label_top + label_h,
                },
            });
            l.labels.push(NamedTextRect {
                id: "card.line2".into(),
                rc: Rect {
                    left: margin,
                    top: state_label_top,
                    right: margin + content_w,
                    bottom: state_label_top + label_h,
                },
            });
            l.hint = Rect {
                left: margin,
                top: hint_top,
                right: margin + content_w,
                bottom: hint_top + hint_h,
            };
        }

        l.btns = vec![
            UiButton {
                id: BTN_CARD,
                label: "Card".into(),
                style: BTN_TAB,
                rc: l.card_link,
            },
            UiButton {
                id: BTN_SETTINGS,
                label: "Settings".into(),
                style: BTN_TAB,
                rc: l.settings,
            },
        ];
        let (pf, _pm, _pt, ps, pstat) = picker_items(
            l.picker_bar.left,
            l.picker_bar.right,
            l.picker_bar.top,
        );
        l.picker_status = pstat;
        l.btns.push(UiButton {
            id: BTN_PICKER_FIELD,
            label: String::new(),
            style: BTN_OUTLINE,
            rc: pf,
        });
        l.btns.push(UiButton {
            id: BTN_PICKER_SEARCH,
            label: "Search".into(),
            style: BTN_ACCENT,
            rc: ps,
        });
        if !compact {
            let sw_left = margin + content_w / 3;
            l.btns.extend(segments(
                sw_left,
                margin + content_w,
                label_top,
                label_band_h,
                &surface_items(surface),
            ));
            l.btns.extend(segments(
                margin,
                margin + content_w,
                fields_row_top,
                field_h,
                &field_items(true),
            ));
            let si: Vec<(i32, String, i32)> = states
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        card_state_button(i as i32),
                        p.label.clone(),
                        card_preset_style(1, &p.id, &spec),
                    )
                })
                .collect();
            l.btns.extend(segments(
                margin,
                margin + content_w,
                state_row_top,
                sc(BTN_H),
                &si,
            ));
        }
        let col_w = (content_w - sc(BTN_GAP_L)) / 2;
        l.btns.push(UiButton {
            id: BTN_CARD_BACK,
            label: "Back".into(),
            style: BTN_OUTLINE,
            rc: Rect {
                left: margin,
                top: footer_top,
                right: margin + col_w,
                bottom: footer_top + sc(BTN_H),
            },
        });
        l.btns.push(UiButton {
            id: BTN_FIX_MATCH,
            label: FIX_MATCH_LABEL.into(),
            style: BTN_OUTLINE,
            rc: Rect {
                left: margin + col_w + sc(BTN_GAP_L),
                top: footer_top,
                right: margin + content_w,
                bottom: footer_top + sc(BTN_H),
            },
        });
        l.status = Rect {
            left: margin,
            top: status_top,
            right: margin + content_w,
            bottom: status_top + status_h,
        };
    }
    l
}

/// `cardTextRects` (`go\window.go:1606`). One audit table for the media rows,
/// the labels, the tooltip panel, the controls and the status line.
#[must_use]
pub fn card_text_rects(cw: i32, ch: i32, dpi: i32, l: &CardLayout) -> Vec<NamedTextRect> {
    let _ = (cw, dpi);
    let mut out = l.labels.clone();
    out.push(NamedTextRect {
        id: "card.title".into(),
        rc: l.title,
    });
    out.push(NamedTextRect {
        id: "card.meta".into(),
        rc: l.meta,
    });
    out.push(NamedTextRect {
        id: "card.art".into(),
        rc: l.art,
    });
    if l.hint.bottom > l.hint.top {
        out.push(NamedTextRect {
            id: "card.hint".into(),
            rc: l.hint,
        });
    }
    for b in &l.btns {
        out.push(NamedTextRect {
            id: format!("card.btn-{}", b.id),
            rc: b.rc,
        });
    }
    out.push(NamedTextRect {
        id: "card.status".into(),
        rc: l.status,
    });
    let _ = ch;
    out
}

/// The object card's one status sentence for a local, unsent preview. It is
/// true whatever PotPlayer is doing, so it can sit beside the preview surfaces
/// without contradicting them.
pub const LOCAL_PREVIEW_STATUS: &str = "Local preview \u{b7} not sent to Discord";

/// What a preview surface says when there is no object to preview at all
/// (cold start, nothing recorded, nothing selected). The synopsis boilerplate
/// belongs to a title, so it is never painted with this line.
pub const PREVIEW_EMPTY_TEXT: &str = "Nothing to preview";

/// Whether the preview surfaces have nothing to show: no recorded push and no
/// local selection, or a record that resolved to no title. The three surfaces
/// read this one predicate, so none of them can paint a title-less body.
#[must_use]
pub fn preview_is_empty(have: bool, p: &SurfacePreview) -> bool {
    !have || p.header.trim().is_empty()
}

/// `previewRows` (`go\window.go:1490`): the recorded payload verbatim.
#[must_use]
pub fn preview_rows(act: &Activity, have: bool) -> [String; 5] {
    if !have {
        return [
            "Header: -".into(),
            "Details: -".into(),
            "State: -".into(),
            "Images: -".into(),
            "Buttons: -".into(),
        ];
    }
    let mut header = act.name.clone();
    if header.is_empty() {
        header = "(Discord app name)".into();
    }
    // The small slot is never populated: the wire carries no `small_image`
    // (the composer clears it and the serializer drops it), so the preview
    // reports the cover alone and never paints a corner icon.
    let images = act.large_image.clone();
    let mut buttons = "none".to_string();
    if !act.buttons.is_empty() {
        let parts: Vec<String> = act
            .buttons
            .iter()
            .map(|b| format!("{} -> {}", b.label, b.url))
            .collect();
        buttons = parts.join(" | ");
    }
    [
        format!("Header: {header}"),
        format!("Details: {}", act.details),
        format!("State: {}", act.state),
        format!("Images: {images}"),
        format!("Buttons: {buttons}"),
    ]
}

/// `previewTooltip` (`go\window.go:1532`).
#[must_use]
pub fn preview_tooltip(act: &Activity, have: bool) -> String {
    if !have || act.large_text.is_empty() {
        return "Tooltip: -".into();
    }
    format!("Tooltip: {}", act.large_text)
}

// ---------------------------------------------------------------------------
// RPC designer: one preview model for the three Discord surfaces.
// ---------------------------------------------------------------------------

/// Which Discord surface the narrow Card view shows. The wide workspace shows
/// all three at once; the narrow composition keeps one at a time so no panel
/// is clipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PreviewSurface {
    Member,
    #[default]
    Profile,
    Activity,
}

impl PreviewSurface {
    /// Parses `/0/1/2`; any other value falls back to the profile.
    #[must_use]
    pub fn of_button(id: i32) -> Option<Self> {
        match id {
            BTN_SURFACE_MEMBER => Some(Self::Member),
            BTN_SURFACE_PROFILE => Some(Self::Profile),
            BTN_SURFACE_ACTIVITY => Some(Self::Activity),
            _ => None,
        }
    }
}

/// Which designer field a toggle button edits. Covers are always on, so
/// artwork has no toggle and no button id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesignerField {
    Details,
    State,
    Synopsis,
    Timers,
}

/// The field a designer toggle button id names, or `None`.
#[must_use]
pub fn designer_field_for_button(id: i32) -> Option<DesignerField> {
    match id {
        BTN_FIELD_DETAILS => Some(DesignerField::Details),
        BTN_FIELD_STATE => Some(DesignerField::State),
        BTN_FIELD_SYNOPSIS => Some(DesignerField::Synopsis),
        BTN_FIELD_TIMERS => Some(DesignerField::Timers),
        _ => None,
    }
}

/// The toggle's short status label.
#[must_use]
pub fn designer_field_label(field: DesignerField) -> &'static str {
    match field {
        DesignerField::Details => "Details",
        DesignerField::State => "State",
        DesignerField::Synopsis => "Synopsis",
        DesignerField::Timers => "Time bar",
    }
}

/// The time strip one preview shows. `timer` is false when the designer's
/// timer field is off or the card carries no start anchor; the fraction is
/// clamped to 0..=1 and drives the bar fill.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreviewClock {
    pub timer: bool,
    pub paused: bool,
    pub fraction: f64,
    pub left: String,
    pub right: String,
}

/// One preview projection of the composed (and designer-filtered) activity.
/// Every surface renders from THIS struct, so the member list, the profile
/// card and the Activity tab can never disagree about the header, the two
/// body lines, the cover, the local synopsis or the time strip.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SurfacePreview {
    /// `Watching <media title>`, from the one presence definition.
    pub header: String,
    pub details: String,
    pub state: String,
    /// The complete local synopsis (the Activity tab body). Empty when the
    /// designer's synopsis field is off; never sent as a Discord field.
    pub synopsis: String,
    /// Whether a real cover is shown. Covers are always on: only a decoded
    /// local image matters now; a missing/not-yet-fetched URL collapses the
    /// slot instead of painting a blank "poster" tile.
    pub art: bool,
    /// The resolved https cover URL when the wire carries one. Empty when the
    /// card has no artwork.
    pub art_url: String,
    pub clock: PreviewClock,
}

/// Parses a resolved runtime string into milliseconds. The providers emit
/// `166 min`, `2h 19m`, `2h`, `45 min` and the iTunes `2h 19m`; a `H:MM` or
/// `H:MM:SS` shape is accepted too. Anything unparseable is `0` (the preview
/// then shows elapsed time with no projected end).
#[must_use]
pub fn parse_length_ms(length: &str) -> i64 {
    let s = length.trim().to_lowercase();
    if s.is_empty() {
        return 0;
    }
    // Providers spell the units with a space (`166 min`); fold that one space
    // so the token pass below sees `166min`.
    let s = s.replace(" min", "min").replace(" h", "h").replace(" m", "m");
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        let mut nums = Vec::new();
        for part in &parts {
            let Ok(n) = part.trim().parse::<i64>() else {
                return 0;
            };
            if n < 0 {
                return 0;
            }
            nums.push(n);
        }
        let (h, m, sec) = match nums.as_slice() {
            [h, m, sec] => (*h, *m, *sec),
            [h, m] => (*h, *m, 0),
            _ => return 0,
        };
        return (h * 3600 + m * 60 + sec) * 1_000;
    }
    let mut ms: i64 = 0;
    let mut any = false;
    for token in s.split_whitespace() {
        let token = token.trim();
        let (num, unit_ms) = if let Some(h) = token.strip_suffix('h') {
            (h, 3_600_000)
        } else if let Some(m) = token.strip_suffix("min") {
            (m, 60_000)
        } else if let Some(m) = token.strip_suffix('m') {
            (m, 60_000)
        } else {
            continue;
        };
        if let Ok(n) = num.trim().parse::<i64>() {
            ms += n.max(0) * unit_ms;
            any = true;
        }
    }
    if any { ms } else { 0 }
}

/// The preview time strip from the payload's own anchor: while playing the bar
/// fills toward the resolved runtime and the right label counts down; paused
/// cards hold the bar and count the pause up (the same start anchor the client
/// uses). No anchor, or a zero runtime, still shows elapsed time when running.
#[must_use]
pub fn preview_clock(act: &Activity, now_ms: i64, duration_ms: i64) -> PreviewClock {
    if !act.has_start || act.start <= 0 {
        return PreviewClock::default();
    }
    let elapsed = (now_ms - act.start).max(0);
    if act.paused {
        return PreviewClock {
            timer: true,
            paused: true,
            fraction: 0.0,
            left: "Paused".to_string(),
            right: presence::fmt_ms(elapsed),
        };
    }
    let mut clock = PreviewClock {
        timer: true,
        paused: false,
        fraction: 0.0,
        left: presence::fmt_ms(elapsed),
        right: String::new(),
    };
    if duration_ms > 0 {
        let used = elapsed.min(duration_ms);
        clock.fraction = used as f64 / duration_ms as f64;
        clock.left = presence::fmt_ms(used);
        clock.right = format!("-{}", presence::fmt_ms(duration_ms - used));
    }
    clock
}

/// Projects one composed card into the shared preview model. `synopsis` is the
/// complete local note (already gated by the designer's synopsis toggle by the
/// caller); `now_ms`/`duration_ms` drive the time strip.
#[must_use]
pub fn surface_preview(
    act: &Activity,
    have: bool,
    synopsis: &str,
    now_ms: i64,
    duration_ms: i64,
    art_on: bool,
) -> SurfacePreview {
    if !have {
        return SurfacePreview::default();
    }
    let art_url = if art_on && !act.large_image.is_empty() {
        act.large_image.clone()
    } else {
        String::new()
    };
    SurfacePreview {
        header: presence::header_line(act),
        details: act.details.clone(),
        state: act.state.clone(),
        synopsis: synopsis.to_string(),
        art: !art_url.is_empty(),
        art_url,
        clock: preview_clock(act, now_ms, duration_ms),
    }
}

/// The non-empty lines one surface draws, in order. The three surfaces share
/// the header, details and state verbatim; the profile and Activity tab add
/// the time strip, and only the Activity tab carries the local synopsis body.
#[must_use]
pub fn surface_lines(p: &SurfacePreview, surface: PreviewSurface) -> Vec<String> {
    let mut out: Vec<String> = vec![p.header.clone()];
    for line in [&p.details, &p.state] {
        if !line.is_empty() {
            out.push(line.clone());
        }
    }
    if surface != PreviewSurface::Member && p.clock.timer {
        out.push(p.clock.left.clone());
        if !p.clock.right.is_empty() {
            out.push(p.clock.right.clone());
        }
    }
    if surface == PreviewSurface::Activity && !p.synopsis.is_empty() {
        out.push(p.synopsis.clone());
    }
    out.retain(|s| !s.is_empty());
    out
}

// ---------------------------------------------------------------------------
// Scan status strings (`go\window.go:1809-2024`).
// ---------------------------------------------------------------------------

/// `maxScanStatusLen` (`go\window.go:1809`).
pub const MAX_SCAN_STATUS_LEN: usize = 160;

/// `scanBusyText` (`go\window.go:1984`).
pub const SCAN_BUSY_TEXT: &str = "Scan running - Cancel first.";

/// `formatScanCount` (`go\window.go:1815`).
#[must_use]
pub fn format_scan_count(n: i64) -> String {
    if (-100_000..100_000).contains(&n) {
        return n.to_string();
    }
    format_scan_count_unit(n)
}

/// `formatScanCountUnit` (`go\window.go:1825`).
#[must_use]
pub fn format_scan_count_unit(n: i64) -> String {
    let f = n as f64;
    let divs = [1e3_f64, 1e6, 1e9, 1e12, 1e15, 1e18];
    let sufs = ["k", "M", "B", "T", "P", "E"];
    let mut idx = 0usize;
    while idx < divs.len() - 1 && f / divs[idx] >= 999.95 {
        idx += 1;
    }
    let s = format!("{:.1}{}", f / divs[idx], sufs[idx]);
    s.replacen(&format!(".0{}", sufs[idx]), sufs[idx], 1)
}

/// `formatScanCountCompact` (`go\window.go:1841`).
#[must_use]
pub fn format_scan_count_compact(n: i64) -> String {
    if (-1000..1000).contains(&n) {
        return n.to_string();
    }
    format_scan_count_unit(n)
}

/// `fitScanStatus` (`go\window.go:1852`): cap to 160 preserving "folders".
#[must_use]
pub fn fit_scan_status(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() <= MAX_SCAN_STATUS_LEN {
        return s.to_string();
    }
    if let Some(i) = s.find("folders") {
        let mut j = i;
        while j > 0 && i - j < 24 {
            let c = b[j - 1];
            if c != b'/' && c != b' ' && !c.is_ascii_digit() {
                break;
            }
            j -= 1;
        }
        let tail = &s[j..];
        let keep = MAX_SCAN_STATUS_LEN
            .saturating_sub(tail.len())
            .saturating_sub(3);
        if keep > 20 {
            return format!("{}...{}", &s[..keep], tail);
        }
    }
    format!("{}...", &s[..MAX_SCAN_STATUS_LEN - 3])
}

/// `scanCatchingUp` (`go\window.go:1881`).
#[must_use]
pub fn scan_catching_up(p: &Progress) -> bool {
    p.total > 0 && p.done == p.total && p.resolved < p.keys_total
}

/// `scanIndexMetaText` (`go\window.go:1897`).
#[must_use]
pub fn scan_index_meta_text(p: &Progress) -> String {
    if p.total <= 0 {
        return String::new();
    }
    let mut s = format!("indexed {} of {} files", p.done, p.total);
    if p.keys_total > 0 {
        s += &format!(" | metadata {} of {}", p.resolved, p.keys_total);
    }
    if scan_catching_up(p) {
        s += " - catching up";
    }
    s
}

/// `scanProgressText` (`go\window.go:1918`).
#[must_use]
pub fn scan_progress_text(p: &Progress) -> String {
    let failed = if p.keys_failed > 0 {
        format!(", {} failed", format_scan_count(p.keys_failed))
    } else {
        String::new()
    };
    let mut line = format!(
        "Scan {}/{} keys | {} fetched, {} cached{} | {}/{} folders",
        format_scan_count(p.keys_done),
        format_scan_count(p.keys_total),
        format_scan_count(p.keys_fetched),
        format_scan_count(p.keys_cached),
        failed,
        format_scan_count(p.folders_scanned),
        format_scan_count(p.folders_skipped)
    );
    let seg = scan_index_meta_text(p);
    if !seg.is_empty() {
        line = format!("{seg} | {line}");
    }
    fit_scan_status(&line)
}

/// `scanDoneText` (`go\window.go:1942`).
///
/// Reads the per-KEY counters (`keys_fetched` / `keys_not_found` / `keys_failed`),
/// not the per-FILE `stats.not_found` the completion log line carries below: the
/// two agree only when a file yields exactly one key. (`scan.rs:180`/`183` carries
/// a third meaning under the same name — `not_found` = `files_pending`, the
/// index-only pending count.) Done-text wording is unchanged.
#[must_use]
pub fn scan_done_text(kind: &str, s: &Stats) -> String {
    if s.keys_fetched == 0 && s.keys_not_found == 0 && s.keys_failed == 0 && s.folders_skipped > 0 {
        return fit_scan_status(&format!(
            "{kind} done: nothing to fetch ({} cached) - {} folders skipped",
            format_scan_count(s.keys_cached),
            format_scan_count(s.folders_skipped)
        ));
    }
    let failed = if s.keys_failed > 0 {
        format!(", {} failed", format_scan_count(s.keys_failed))
    } else {
        String::new()
    };
    fit_scan_status(&format!(
        "{kind} done: {} keys - {} cached, {} fetched{} - {}/{} folders",
        format_scan_count(s.keys),
        format_scan_count(s.keys_cached),
        format_scan_count(s.keys_fetched),
        failed,
        format_scan_count(s.folders_scanned),
        format_scan_count(s.folders_skipped)
    ))
}

/// `scanCancelledText` (`go\window.go:1960`).
#[must_use]
pub fn scan_cancelled_text(done: i64, total: i64) -> String {
    fit_scan_status(&format!(
        "Scan cancelled - {}/{} keys done, progress kept",
        format_scan_count(done),
        format_scan_count(total)
    ))
}

/// `scanButtonLabel` (`go\window.go:1968`).
#[must_use]
pub fn scan_button_label(active: bool) -> &'static str {
    if active { "Cancel" } else { "Scan" }
}

/// `scanRefreshEnabled` (`go\window.go:1978`).
#[must_use]
pub fn scan_refresh_enabled(active: bool) -> bool {
    !active
}

/// `scanBlockedReason` (`go\window.go:1992`): reads the running-scan flag.
#[must_use]
pub fn scan_blocked_reason(scan_active: bool, id: i32) -> &'static str {
    if !scan_active {
        return "";
    }
    if matches!(id, BTN_ADD | BTN_REMOVE | BTN_REFRESH) {
        return SCAN_BUSY_TEXT;
    }
    ""
}

/// `removeHasTarget` (`go\window.go:2006`).
#[must_use]
pub fn remove_has_target(sel_row: i32, hover_row: i32, dirs_len: usize) -> bool {
    let n = dirs_len as i32;
    (sel_row >= 0 && sel_row < n) || (hover_row >= 0 && hover_row < n)
}

/// The Main list's Up/Down step: the absolute row `sel` moves by `delta`
/// inside a list of `len` rows, clamped to `[0, len - 1]`. Nothing selected
/// (`-1`) lands on row 0 first; an empty list stays `-1` (no row to select).
/// Pure, so the keyboard and the click hit-test (`row_at_point` + `list_top`)
/// both speak absolute folder indices and cannot disagree about the current row.
#[must_use]
pub fn step_sel_row(sel: i32, len: usize, delta: i32) -> i32 {
    if len == 0 {
        return -1;
    }
    if sel < 0 {
        return 0;
    }
    (sel + delta).clamp(0, len as i32 - 1)
}

/// Home/End: the first (`last = false`) or last (`last = true`) row of a `len`
/// row list; `-1` when the list is empty.
#[must_use]
pub fn edge_sel_row(len: usize, last: bool) -> i32 {
    if len == 0 {
        return -1;
    }
    if last { len as i32 - 1 } else { 0 }
}

/// Clamp an absolute selection to a list of `len` rows -- the guard
/// `refreshDirList` runs when rows are removed: a shrunk list must never leave
/// the selection pointing past its end. `-1` for an empty list and for an
/// existing no-selection (nothing to clamp to). Pure, so the shrink guard is
/// testable with no config I/O.
#[must_use]
pub fn clamp_sel_row(sel: i32, len: usize) -> i32 {
    if len == 0 || sel < 0 {
        return -1;
    }
    sel.min(len as i32 - 1)
}

/// The scan error the terminal path distinguishes (`go\window.go:2018`: a bare
/// context cancellation is a cancel, not a failure).
#[derive(Clone, Debug)]
pub struct UiScanError {
    pub cancelled: bool,
    pub message: String,
}

impl UiScanError {
    #[must_use]
    pub fn other(message: &str) -> Self {
        Self {
            cancelled: false,
            message: message.to_string(),
        }
    }

    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            cancelled: true,
            message: String::new(),
        }
    }
}

/// The worker-published scan snapshot (`go\window.go:161-163`): `scanMu` plus
/// the progress/stats/err package vars. The worker owns THIS lock and posts
/// `WM_APP_SCAN_PROGRESS` / `WM_APP_SCAN_DONE`; it must never lock the
/// process-wide `UI` mutex, which tests hold while a worker runs.
#[derive(Clone, Debug, Default)]
pub struct ScanSnapshot {
    pub progress: Progress,
    pub stats: Stats,
    pub err: Option<UiScanError>,
}

/// `scanResultText` (`go\window.go:2014`).
#[must_use]
pub fn scan_result_text(kind: &str, s: &Stats, err: Option<&UiScanError>) -> String {
    if s.cancelled {
        return scan_cancelled_text(s.keys_done, s.keys);
    }
    if let Some(e) = err {
        if e.cancelled {
            return scan_cancelled_text(s.keys_done, s.keys);
        }
        return fit_scan_status(&format!("{kind} failed: {}", e.message));
    }
    scan_done_text(kind, s)
}

/// `scanTerminalText` (`go\window.go:2705`).
#[must_use]
pub fn scan_terminal_text(force: bool, s: &Stats, err: Option<&UiScanError>) -> String {
    let kind = if force { "Refresh" } else { "Scan" };
    scan_result_text(kind, s, err)
}

// ---------------------------------------------------------------------------
// Responsive scan display (`go\window.go:2038-2596`).
// ---------------------------------------------------------------------------

/// `scanKeyFraction` (`go\window.go:2041`).
#[must_use]
pub fn scan_key_fraction(done: i64, total: i64) -> Option<f64> {
    if total <= 0 {
        return None;
    }
    let done = done.clamp(0, total);
    Some(done as f64 / total as f64)
}

/// `scanPaintGate` (`go\window.go:2060`): a backward clock paints, never stalls.
#[must_use]
pub fn scan_paint_gate(last: Instant, now: Instant) -> bool {
    match now.checked_duration_since(last) {
        None => true,
        Some(d) => d >= SCAN_PAINT_INTERVAL,
    }
}

/// One UI-thread lifecycle input (`go\window.go:2068-2078`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanLifecycleEvent {
    Start,
    Progress,
    Tick,
    Done,
}

/// The pure run model behind the scan state (`go\window.go:2084-2087`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLifecycle {
    pub active: bool,
    pub last_paint: Instant,
}

impl Default for ScanLifecycle {
    fn default() -> Self {
        Self {
            active: false,
            last_paint: Instant::now(),
        }
    }
}

/// The explicit effect set one lifecycle step requests (`go\window.go:2096-2101`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanFx {
    pub arm: bool,
    pub kill: bool,
    pub paint: bool,
    pub terminal: bool,
}

/// `scanLifecycleNext` (`go\window.go:2115`).
#[must_use]
pub fn scan_lifecycle_next(
    st: ScanLifecycle,
    ev: ScanLifecycleEvent,
    now: Instant,
) -> (ScanLifecycle, ScanFx) {
    match ev {
        ScanLifecycleEvent::Start => {
            if st.active {
                return (st, ScanFx::default());
            }
            (
                ScanLifecycle {
                    active: true,
                    last_paint: now,
                },
                ScanFx {
                    arm: true,
                    paint: true,
                    ..ScanFx::default()
                },
            )
        }
        ScanLifecycleEvent::Progress | ScanLifecycleEvent::Tick => {
            if !st.active || !scan_paint_gate(st.last_paint, now) {
                return (st, ScanFx::default());
            }
            (
                ScanLifecycle {
                    active: st.active,
                    last_paint: now,
                },
                ScanFx {
                    paint: true,
                    ..ScanFx::default()
                },
            )
        }
        ScanLifecycleEvent::Done => {
            if !st.active {
                return (
                    ScanLifecycle::default(),
                    ScanFx {
                        kill: true,
                        ..ScanFx::default()
                    },
                );
            }
            (
                ScanLifecycle::default(),
                ScanFx {
                    kill: true,
                    terminal: true,
                    ..ScanFx::default()
                },
            )
        }
    }
}

/// `minElapsedForETA` / `minKeysForETA` (`go\window.go:2142-2145`).
pub const MIN_ELAPSED_FOR_ETA: Duration = Duration::from_secs(2);
pub const MIN_KEYS_FOR_ETA: i64 = 5;

/// `scanObservedDone` (`go\window.go:2151`).
#[must_use]
pub fn scan_observed_done(p: &Progress) -> i64 {
    (p.keys_done - p.keys_cached).max(0)
}

/// `scanETA` (`go\window.go:2168`).
#[must_use]
pub fn scan_eta(p: &Progress, observed_base: i64, resolve_elapsed: Duration) -> Option<Duration> {
    if p.keys_total <= 0 || p.keys_done >= p.keys_total {
        return None;
    }
    let observed = scan_observed_done(p) - observed_base;
    if observed < MIN_KEYS_FOR_ETA || resolve_elapsed < MIN_ELAPSED_FOR_ETA {
        return None;
    }
    let remaining = p.keys_total - p.keys_done;
    if remaining <= 0 {
        return None;
    }
    let secs = resolve_elapsed.as_secs_f64() * remaining as f64 / observed as f64;
    if secs < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

/// `scanResolveElapsed` (`go\window.go:2194`).
#[must_use]
pub fn scan_resolve_elapsed(origin: Option<Instant>, now: Instant) -> Duration {
    match origin {
        None => Duration::ZERO,
        Some(o) => now.saturating_duration_since(o),
    }
}

/// `formatScanDuration` (`go\window.go:2206`).
#[must_use]
pub fn format_scan_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m{:02}s", s % 60);
    }
    format!("{}h{:02}m", m / 60, m % 60)
}

/// `scanLiveStatusParts` (`go\window.go:2226`).
#[must_use]
pub fn scan_live_status_parts(
    p: &Progress,
    elapsed: Duration,
    resolve_elapsed: Duration,
    observed_base: i64,
) -> (String, String, String) {
    let base = scan_progress_text(p);
    let elapsed_sfx = format!(" | {}", format_scan_duration(elapsed));
    let mut eta_sfx = String::new();
    if let Some(eta) = scan_eta(p, observed_base, resolve_elapsed) {
        eta_sfx = format!(", eta {} left", format_scan_duration(eta));
    }
    (base, elapsed_sfx, eta_sfx)
}

/// `scanStatusAvailPx` (`go\window.go:2239`).
#[must_use]
pub fn scan_status_avail_px(dpi: i32) -> i32 {
    scale_px(dpi, MAIN_CLIENT_W - 2 * MAIN_MARGIN)
}

/// `statusAvailPx` (`go\window.go:2249`).
#[must_use]
pub fn status_avail_px(cw: i32, dpi: i32) -> i32 {
    cw - 2 * scale_px(dpi, MAIN_MARGIN)
}

/// `scanProgressShort` (`go\window.go:2306`).
#[must_use]
pub fn scan_progress_short(p: &Progress) -> String {
    let failed = if p.keys_failed > 0 {
        format!(", {} failed", format_scan_count_compact(p.keys_failed))
    } else {
        String::new()
    };
    fit_scan_status(&format!(
        "Scan {}/{} keys{} | {}/{} folders",
        format_scan_count_compact(p.keys_done),
        format_scan_count_compact(p.keys_total),
        failed,
        format_scan_count_compact(p.folders_scanned),
        format_scan_count_compact(p.folders_skipped)
    ))
}

/// `scanProgressFloor` (`go\window.go:2325`).
#[must_use]
pub fn scan_progress_floor(p: &Progress) -> String {
    fit_scan_status(&format!(
        "Scan {}/{} keys",
        format_scan_count_compact(p.keys_done),
        format_scan_count_compact(p.keys_total)
    ))
}

/// `scanProgressMinimal` (`go\window.go:2334`).
#[must_use]
pub fn scan_progress_minimal(p: &Progress) -> String {
    let failed = if p.keys_failed > 0 {
        format!(", {} failed", format_scan_count_compact(p.keys_failed))
    } else {
        String::new()
    };
    fit_scan_status(&format!("{}{failed}", scan_progress_floor(p)))
}

/// `scanStatusCandidates` (`go\window.go:2357`).
#[must_use]
pub fn scan_status_candidates(
    p: &Progress,
    elapsed: Duration,
    resolve_elapsed: Duration,
    observed_base: i64,
) -> Vec<String> {
    let (base, elapsed_sfx, eta_sfx) =
        scan_live_status_parts(p, elapsed, resolve_elapsed, observed_base);
    let mut cands = Vec::new();
    if !eta_sfx.is_empty() {
        cands.push(format!("{base}{elapsed_sfx}{eta_sfx}"));
    }
    cands.push(format!("{base}{elapsed_sfx}"));
    let seg = scan_index_meta_text(p);
    if !seg.is_empty() {
        if !eta_sfx.is_empty() {
            cands.push(format!("{seg}{elapsed_sfx}{eta_sfx}"));
        }
        cands.push(format!("{seg}{elapsed_sfx}"));
        cands.push(seg);
    }
    let short = scan_progress_short(p);
    if !eta_sfx.is_empty() {
        cands.push(format!("{short}{elapsed_sfx}{eta_sfx}"));
    }
    cands.push(format!("{short}{elapsed_sfx}"));
    cands.push(short);
    cands.push(scan_progress_minimal(p));
    cands.push(scan_progress_floor(p));
    cands
}

/// `composeScanLiveStatus` (`go\window.go:2387`).
#[must_use]
pub fn compose_scan_live_status(
    p: &Progress,
    elapsed: Duration,
    resolve_elapsed: Duration,
    observed_base: i64,
    avail_px: i32,
    measure: impl Fn(&str) -> i32,
) -> String {
    let cands = scan_status_candidates(p, elapsed, resolve_elapsed, observed_base);
    for c in &cands {
        if measure(c) <= avail_px {
            return c.clone();
        }
    }
    cands[cands.len() - 1].clone()
}

/// `scanBarRect` (`go\window.go:2425`): the strip BELOW the status rect.
#[must_use]
pub fn scan_bar_rect(status: Rect, dpi: i32) -> Rect {
    let sc = |v: i32| scale_px(dpi, v);
    let top = status.bottom + sc(3);
    Rect {
        left: status.left,
        top,
        right: status.right,
        bottom: top + sc(5),
    }
}

/// `scanIndeterminatePos` (`go\window.go:2433`).
#[must_use]
pub fn scan_indeterminate_pos(elapsed: Duration) -> f64 {
    const LOOP: Duration = Duration::from_millis(1200);
    let ms = elapsed.as_millis();
    let loop_ms = LOOP.as_millis();
    (ms % loop_ms) as f64 / loop_ms as f64
}

/// `scanBarModel` (`go\window.go:2489`).
#[must_use]
pub fn scan_bar_model(active: bool, done: i64, total: i64) -> (bool, bool, f64) {
    if !active {
        return (false, false, 0.0);
    }
    match scan_key_fraction(done, total) {
        Some(f) => (true, true, f),
        None => (true, false, 0.0),
    }
}

/// `scanBarProgress` (`go\window.go:2507`): the METADATA pair, never the file pair.
#[must_use]
pub fn scan_bar_progress(p: &Progress, active: bool) -> (bool, bool, f64) {
    scan_bar_model(active, p.resolved, p.keys_total)
}

// ---------------------------------------------------------------------------
// Native measurement + DPI helpers (`go\window.go:2256-2304`, `:3442-3464`).
// ---------------------------------------------------------------------------

/// Client rect of `hwnd` (`go\window.go:2818`).
#[must_use]
pub fn get_client_rect(hwnd: HWND) -> Rect {
    use windows_sys::Win32::Foundation::RECT;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `rc` is a live RECT; GetClientRect only writes it.
    unsafe {
        crate::win32::GetClientRect(hwnd, &mut rc);
    }
    Rect {
        left: rc.left,
        top: rc.top,
        right: rc.right,
        bottom: rc.bottom,
    }
}

/// `windowDpi` (`go\window.go:3448`): 96 when the API is absent or fails.
#[must_use]
pub fn window_dpi(hwnd: HWND) -> i32 {
    if hwnd.is_null() {
        return 96;
    }
    // SAFETY: GetDpiForWindow reads `hwnd` only.
    let d = unsafe { crate::win32::GetDpiForWindow(hwnd) };
    if d != 0 { d as i32 } else { 96 }
}

/// `systemDpi` (`go\window.go:3459`).
#[must_use]
pub fn system_dpi() -> i32 {
    // SAFETY: GetDpiForSystem takes no arguments.
    let d = unsafe { crate::win32::GetDpiForSystem() };
    if d != 0 { d as i32 } else { 96 }
}

/// `liveStatusAvailPx` (`go\window.go:2256`): the live client rect budget.
#[must_use]
pub fn live_status_avail_px(hwnd: HWND) -> i32 {
    let rc = get_client_rect(hwnd);
    let cw = rc.right - rc.left;
    if cw > 0 {
        return status_avail_px(cw, window_dpi(hwnd));
    }
    scan_status_avail_px(96)
}

/// The status line's font: the ONE source both the fit measurement and the
/// paint read. The measure path used to build its own "Segoe UI" at 12px while
/// the paint drew the brand face at `FS_HINT`, so a fitted line's width was
/// measured in a face and size nothing rendered. Both callers now build here
/// (`status_fit_measures_the_painted_font` pins them to it).
#[must_use]
fn status_font(dpi: i32) -> Gdi::HFONT {
    #[cfg(test)]
    STATUS_FONT_BUILDS.with(|n| n.set(n.get() + 1));
    create_ui_font(scale_px(dpi, FS_HINT), brand::UI_WEIGHT)
}

#[cfg(test)]
thread_local! {
    /// Per-thread count of [`status_font`] builds, so a test can see exactly
    /// which path built the font without racing the sibling tests that also
    /// call the measure.
    static STATUS_FONT_BUILDS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The status bar's right-aligned version, read from the same constant the
/// window already uses ([`crate::settings::APP_VERSION`], `x.y.z` -> `vx.y`).
/// Pure so the paint and its tests agree on the exact short form.
#[must_use]
pub fn app_version_short() -> String {
    let v = crate::settings::APP_VERSION;
    let mut parts = v.split('.');
    let major = parts.next().unwrap_or("").trim();
    let minor = parts.next().unwrap_or("").trim();
    if major.is_empty() {
        return format!("v{v}");
    }
    if minor.is_empty() {
        return format!("v{major}");
    }
    format!("v{major}.{minor}")
}

/// The narrow hero meta, derived from the record only: a movie never carries
/// an episode marker. Movies render `year · Film · length`
/// (`2024 · Film · 2h 46m`); series render `year · kind · episode code`
/// (`2024 · Anime series · S02E01`, `2024 · Series · S02E01`). Pure so the
/// paint and its tests pin the same string.
#[must_use]
pub fn hero_meta_for(
    year: i32,
    is_movie: bool,
    anime: bool,
    length: &str,
    season: i64,
    number: i32,
    absolute: i32,
) -> String {
    let code = if is_movie {
        String::new()
    } else {
        hero_episode_code(season, number, absolute)
    };
    narrow::hero_meta(year, is_movie, anime, length, &code)
}

/// The hero's episode code from the record's own numbers: `SxxExx` when the
/// season and number are known, else `Exx` from the absolute or bare number,
/// else empty (the marker is omitted, never fabricated). Mirrors
/// `presence`'s code without crossing the crate seam.
#[must_use]
pub fn hero_episode_code(season: i64, number: i32, absolute: i32) -> String {
    if season > 0 && number > 0 {
        return format!("S{season:02}E{number:02}");
    }
    if absolute > 0 {
        return format!("E{absolute:02}");
    }
    if number > 0 {
        return format!("E{number:02}");
    }
    String::new()
}

/// `measureScanStatusWidth` (`go\window.go:2271`): real GDI font metrics in
/// [`status_font`] -- the very font the status line paints, so the fitted width
/// is the rendered width. `None` when no DC/font is available.
#[must_use]
pub fn measure_scan_status_width(dpi: i32, s: &str) -> Option<i32> {
    use windows_sys::Win32::Foundation::SIZE;
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject,
    };
    if s.is_empty() {
        return Some(0);
    }
    // SAFETY: a screen DC + a font, both released below; extents do not depend
    // on the target surface.
    unsafe {
        let hdc = GetDC(std::ptr::null_mut());
        if hdc.is_null() {
            return None;
        }
        let font = status_font(dpi);
        if font.is_null() {
            ReleaseDC(std::ptr::null_mut(), hdc);
            return None;
        }
        let old = SelectObject(hdc, font);
        let u: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE { cx: 0, cy: 0 };
        let r = GetTextExtentPoint32W(hdc, u.as_ptr(), u.len() as i32, &mut sz);
        SelectObject(hdc, old);
        DeleteObject(font);
        ReleaseDC(std::ptr::null_mut(), hdc);
        if r == 0 { None } else { Some(sz.cx) }
    }
}

/// `scanLiveStatusText` (`go\window.go:2404`).
#[must_use]
pub fn scan_live_status_text(
    p: &Progress,
    elapsed: Duration,
    resolve_elapsed: Duration,
    observed_base: i64,
    dpi: i32,
    avail_px: i32,
) -> String {
    let (base, elapsed_sfx, _) = scan_live_status_parts(p, elapsed, resolve_elapsed, observed_base);
    if measure_scan_status_width(dpi, &format!("{base}{elapsed_sfx}")).is_none() {
        return fit_scan_status(&format!("{}{elapsed_sfx}", scan_progress_floor(p)));
    }
    fit_scan_status(&compose_scan_live_status(
        p,
        elapsed,
        resolve_elapsed,
        observed_base,
        avail_px,
        |s| measure_scan_status_width(dpi, s).unwrap_or(avail_px + 1),
    ))
}

// ---------------------------------------------------------------------------
// Report button (`go\window.go:1032-1203`).
// ---------------------------------------------------------------------------

/// `reportButtonLabel` (`go\window.go:1034`).
pub const REPORT_BUTTON_LABEL: &str = "Report";
/// `reportRepo` (`go\window.go:1036`).
pub const REPORT_REPO: &str = "nathwn12/potflix";
/// `reportLabel` (`go\window.go:1038`).
pub const REPORT_LABEL: &str = "needs-repair";
/// `reportNothingText` (`go\window.go:1042`).
pub const REPORT_NOTHING_TEXT: &str = "Nothing to report - no needs-repair entries.";
/// `reportFailedText` (`go\window.go:1045`).
pub const REPORT_FAILED_TEXT: &str = "Report failed - see the debug log.";
/// `reportPendingText` (`go\window.go:1049`).
pub const REPORT_PENDING_TEXT: &str = "Filing report...";

/// `reportButtonLabel` accessor (used by `compute_layout`).
#[must_use]
pub const fn report_button_label() -> &'static str {
    REPORT_BUTTON_LABEL
}

/// `repairReportDir` (`go\window.go:1074`).
#[must_use]
pub fn repair_report_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("LOCALAPPDATA")
        && !d.is_empty()
    {
        return std::path::Path::new(&d).join("Potflix").join("inventory");
    }
    app_support::debuglog::exe_dir_or_dot().join("inventory")
}

/// `repairReportPath` (`go\window.go:1083`).
#[must_use]
pub fn repair_report_path() -> std::path::PathBuf {
    repair_report_dir().join(repair::report::REPORT_JSON_NAME)
}

/// `reportDecision` (`go\window.go:1092`).
#[must_use]
pub fn report_decision(r: &repair::report::Report, err: bool) -> (bool, &'static str) {
    if err || r.entries.is_empty() {
        return (false, REPORT_NOTHING_TEXT);
    }
    (true, "")
}

/// `reportOpenedText` (`go\window.go:1102`).
#[must_use]
pub fn report_opened_text(gh_failed: bool) -> &'static str {
    if gh_failed {
        "Report: gh failed - opened a prefilled issue instead."
    } else {
        "Report: issue opened in your browser."
    }
}

/// `reportLaunchText` (`go\window.go:1112`).
#[must_use]
pub fn report_launch_text(url: &str) -> String {
    format!("Report issue: {url}")
}

/// `reportIssueStatus` (`go\window.go:1177`).
///
/// The failure is made visible twice: the `(gh)` log line carries the FULL
/// error (message plus cause), and the status strip names the reason while the
/// prefilled issue stays in reach -- `open` is still handed the URL so the
/// browser can offer the manual path, and the URL itself is shown when that
/// open failed. A URL gh filed (including a retry that dropped the labels)
/// shows the issue URL.
#[must_use]
pub fn report_issue_status(
    url: &str,
    err: Option<&repair::Error>,
    open: &dyn Fn(&str) -> bool,
) -> String {
    if url.is_empty() {
        app_logf(&format!(
            "report: create issue: {}",
            err.map_or(String::new(), report_error_text)
        ));
        return REPORT_FAILED_TEXT.to_string();
    }
    if let Some(e) = err {
        app_logf(&format!(
            "report: create issue (gh): {}",
            report_error_text(e)
        ));
    }
    let opened = open(url);
    if !opened {
        app_logf(&format!("report: open {url} failed"));
    }
    match err {
        None => report_launch_text(url),
        Some(_) if !is_prefilled_issue_url(url) => report_launch_text(url),
        Some(e) => report_failed_text(&report_error_text(e), url, opened),
    }
}

/// The full text of a failed filing for the debug log: the message plus, when
/// there is one, the cause's text. Logging only [`repair::Error::message`] is
/// what hid today's label failure.
#[must_use]
fn report_error_text(e: &repair::Error) -> String {
    match e.cause_text() {
        Some(cause) if !cause.is_empty() => format!("{}: {cause}", e.message()),
        _ => e.message().to_string(),
    }
}

/// The prefilled URL [`repair::issue::create_issue`] falls back to always
/// carries the `issues/new` path; a URL gh printed after filing never does.
#[must_use]
fn is_prefilled_issue_url(url: &str) -> bool {
    url.contains("/issues/new")
}

/// The short failure line: the reason, plus the manual path still in reach --
/// opened in the browser when that worked, shown as the URL when it did not.
#[must_use]
fn report_failed_text(reason: &str, url: &str, opened: bool) -> String {
    let said = if reason.is_empty() {
        "Report failed".to_string()
    } else {
        format!("Report failed: {reason}")
    };
    if opened {
        format!("{said}. Opened a prefilled issue instead.")
    } else {
        format!("{said}. Prefilled issue: {url}")
    }
}

// ---------------------------------------------------------------------------
// Process-wide debug log (`go\window.go:3491`).
// ---------------------------------------------------------------------------

static APP_LOG: std::sync::OnceLock<&'static app_support::debuglog::Logger> =
    std::sync::OnceLock::new();

/// Install the process log the UI writes to (`appLog = log`).
///
/// Takes a `&'static` reference -- the same logger the tray holds via
/// `tray::set_logger` -- so both seams write to one logger with no second
/// file handle. Returns the rejected logger if one is already installed.
pub fn set_app_log(
    log: &'static app_support::debuglog::Logger,
) -> Result<(), &'static app_support::debuglog::Logger> {
    APP_LOG.set(log)
}

/// `appLogf` (`go\window.go:3491`).
pub fn app_logf(message: &str) {
    if let Some(l) = APP_LOG.get() {
        l.log(message);
    }
}

// ---------------------------------------------------------------------------
// UiState: the UI-thread state (`go\window.go:97-176`, `:1427-1431`).
// ---------------------------------------------------------------------------

/// One recorded preview input: what a preview needs (the composer inputs), not
/// the finished payload. The preview composes on demand through the same
/// composer the send path uses, so picking a line re-renders immediately.
#[derive(Clone, Debug, Default)]
struct CardPreviewInput {
    raw_title: String,
    action: Option<Action>,
    resolved: Resolved,
    /// Whether the anime legs answered this record (the hero kind word). Set
    /// from the picked candidate's provider; live pushes carry `false`.
    anime: bool,
}

/// The picker's own preload: the rows the playing title's search already
/// produced, keyed on title|year|scope (never the file path), so the next
/// episode of the same show is a hit instead of a re-search.
#[derive(Clone, Debug, Default)]
pub struct PreloadCache {
    /// `title|year|filter` (see [`crate::search::search_cache_key`]).
    pub key: String,
    pub title: String,
    pub year: i32,
    pub filter: crate::search::KindFilter,
    /// The playing file this entry was prefetched for. The next episode keeps
    /// the key but changes the path, and that is a re-latch: the entry stops
    /// counting as consumed so the card re-seeds from the new file.
    pub path: String,
    pub rows: Vec<Candidate>,
    /// True once a finished preload wrote this entry. An empty prefetch leaves
    /// it false, so the list stays closed for a manual search.
    pub ready: bool,
    /// True once the card consumed this entry (seeded the query and/or filled
    /// the list), so a poll tick never re-primes a query the user cleared.
    pub applied: bool,
}

/// One finished background preload, published into [`UiState`]'s slot.
#[derive(Clone, Debug, Default)]
pub struct PreloadResult {
    pub key: String,
    pub title: String,
    pub year: i32,
    pub filter: crate::search::KindFilter,
    /// The playing file this pass was for, carried through so a poll that
    /// lands after a switch cannot mark the new file's entry consumed.
    pub path: String,
    pub rows: Vec<Candidate>,
    pub elapsed_ms: u128,
}

/// What one picker search pass reports back: the rows it produced and the
/// providers that failed. A retry is asked for exactly `failed`, so a working
/// provider is never refetched for a title it already answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PickerReport {
    pub rows: Vec<Candidate>,
    pub failed: Vec<String>,
}

/// The provider-search seam: one query, one explicit scope, the providers to
/// retry (`only`; empty = the filter's own provider list), a sink that takes
/// each batch as it arrives, and a failure. The default runs the real provider
/// pass through the persistent search cache; tests inject a fixture so no test
/// touches the network or the owner's cache.
pub type PickerSearch = Arc<
    dyn Fn(
            &str,
            i32,
            crate::search::KindFilter,
            &[String],
            &(dyn Fn(Vec<Candidate>) + Send + Sync),
        ) -> Result<PickerReport, String>
        + Send
        + Sync,
>;

/// The real search pass through the persistent search cache.
///
/// `only` names the providers to retry (the failed list from a cached entry);
/// empty means the filter's own provider list. The cache is checked first: a
/// complete entry is served with no network, a partial entry is served and only
/// its failed providers are retried. A finished pass stores its merged rows and
/// the providers that still failed, so a later Search retries exactly those and
/// never refetches one that already answered.
fn default_picker_search(
    title: &str,
    year: i32,
    filter: crate::search::KindFilter,
    only: &[String],
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
) -> Result<PickerReport, String> {
    picker_search_run(title, year, filter, only, sink, true)
}

/// [`default_picker_search`] with the cache disabled: the plain provider pass,
/// used by a default [`UiState`] so unit tests never read or write the owner's
/// cache. The app installs [`default_picker_search`] in `create_main_window`.
fn default_picker_search_uncached(
    title: &str,
    year: i32,
    filter: crate::search::KindFilter,
    only: &[String],
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
) -> Result<PickerReport, String> {
    picker_search_run(title, year, filter, only, sink, false)
}

/// The one local seam over the metadata search cache: `search_cache_get` /
/// `search_cache_put`. One edit here follows any signature change there.
///
/// Tests arm [`TEST_SEARCH_CACHE`] instead, so a unit test never reads or
/// writes the owner's real cache file.
fn cached_search(key: &str) -> Option<metadata::CachedSearch> {
    #[cfg(test)]
    if let Some(map) = TEST_SEARCH_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
    {
        return map.get(key).cloned();
    }
    metadata::search_cache_get(key)
}

fn store_search(key: &str, rows: &[Candidate], failed: &[String]) {
    #[cfg(test)]
    if let Some(map) = TEST_SEARCH_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        map.insert(
            key.to_string(),
            metadata::CachedSearch {
                rows: rows.to_vec(),
                failed: failed.to_vec(),
            },
        );
        return;
    }
    metadata::search_cache_put(key, rows, failed);
}

/// Test-only in-memory search cache; `None` (the default) means the real
/// metadata store. Armed by the cache tests under [`TEST_CACHE_SERIAL`].
#[cfg(test)]
static TEST_SEARCH_CACHE: Mutex<Option<std::collections::HashMap<String, metadata::CachedSearch>>> =
    Mutex::new(None);

/// Serializes the tests that arm [`TEST_SEARCH_CACHE`].
#[cfg(test)]
static TEST_CACHE_SERIAL: Mutex<()> = Mutex::new(());

/// Narrow rows to the typed year when there is one; a yearless query keeps
/// every row. Used for the instant paint from a cached (possibly yearless)
/// answer before the stream refines it.
fn rows_for_year(rows: Vec<Candidate>, year: i32) -> Vec<Candidate> {
    if year <= 0 {
        return rows;
    }
    rows.into_iter().filter(|cand| cand.year == year).collect()
}

/// The provider pass itself, optionally cache-backed.
fn picker_search_run(
    title: &str,
    year: i32,
    filter: crate::search::KindFilter,
    only: &[String],
    sink: &(dyn Fn(Vec<Candidate>) + Send + Sync),
    use_cache: bool,
) -> Result<PickerReport, String> {
    let key = crate::search::search_cache_key(title, year, filter);
    let cached = if use_cache {
        cached_search(&key)
    } else {
        None
    };
    if only.is_empty()
        && let Some(cached) = &cached
        && !cached.rows.is_empty()
        && cached.failed.is_empty()
    {
        app_logf(&format!(
            "picker: cache hit \"{title}\" ({} rows, 0 failed)",
            cached.rows.len()
        ));
        sink(cached.rows.clone());
        return Ok(PickerReport {
            rows: cached.rows.clone(),
            failed: Vec::new(),
        });
    }
    // A retry starts from the cached rows so its result is the whole answer,
    // not just the legs it re-asked. An initial partial entry retries its own
    // failed legs for the same reason.
    let providers: Vec<String> = if only.is_empty() {
        let from_cache = cached
            .as_ref()
            .map(|c| c.failed.clone())
            .unwrap_or_default();
        if from_cache.is_empty() {
            filter
                .providers()
                .iter()
                .map(|p| (*p).to_string())
                .collect()
        } else {
            from_cache
        }
    } else {
        only.to_vec()
    };
    // A retry never includes a provider the scope does not use.
    let providers: Vec<String> = providers
        .into_iter()
        .filter(|p| filter.providers().contains(&p.as_str()))
        .collect();
    if !only.is_empty() || cached.as_ref().is_some_and(|c| !c.failed.is_empty()) {
        app_logf(&format!(
            "picker: retrying {} for \"{title}\"",
            providers.join(", ")
        ));
    }
    let prior: Vec<Candidate> = cached.map(|c| c.rows).unwrap_or_default();
    let cancel = metadata_http::cancel::Cancel::with_deadline(FIX_MATCH_DEADLINE);
    let emit = |rows: Vec<Candidate>| {
        let rows: Vec<Candidate> = rows
            .into_iter()
            .filter(|cand| filter.includes_candidate(cand))
            .collect();
        if !rows.is_empty() {
            sink(rows);
        }
    };
    let outcomes: std::sync::Mutex<Vec<metadata::SearchOutcome>> =
        std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for kind in filter.kinds() {
            let cancel = &cancel;
            let emit = &emit;
            let providers = &providers;
            let outcomes = &outcomes;
            scope.spawn(move || {
                let out = metadata::search_stream_only_with_log(
                    cancel,
                    title,
                    year,
                    *kind,
                    providers,
                    emit,
                    &app_logf,
                );
                outcomes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(out);
            });
        }
    });
    if cancel.err().is_some() {
        return Err("search cancelled".to_string());
    }
    let mut rows: Vec<Candidate> = prior;
    let mut failed: Vec<String> = Vec::new();
    for out in outcomes
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        rows.extend(out.rows);
        for provider in out.failed {
            if !failed.contains(&provider) {
                failed.push(provider);
            }
        }
    }
    rows.retain(|cand| filter.includes_candidate(cand));
        // The dialog's stored rows use the same ranking the live list paints
        // ([`crate::search::merge_candidates_for`], which the streamed batches
        // merge through): a finished live search and its cache replay then
        // paint identical rows in the identical order, whatever provider
        // answered first.
        let rows = crate::search::merge_candidates_for(title, year, None, vec![rows]);
    let rows: Vec<Candidate> = rows
        .into_iter()
        .take(metadata::search::SEARCH_LIMIT)
        .collect();
    if use_cache && (!rows.is_empty() || !failed.is_empty()) {
        store_search(&key, &rows, &failed);
    }
    Ok(PickerReport { rows, failed })
}

/// The scanner the UI seam owns. Every mutable value the Go root package held as
/// a process global lives here, so Rust tests construct an isolated instance
/// instead of racing a global.
pub struct UiState {
    pub cur_view: View,
    pub status_text: String,
    pub ui_dirs: Vec<String>,
    pub sel_row: i32,
    pub hover_btn: i32,
    pub hover_row: i32,
    pub hover_x: bool,
    pub pressed_btn: i32,
    pub pressed_x: i32,
    pub focus_id: i32,
    /// First visible folder index in the Main list (`0` when the list fits).
    /// Selection (`sel_row`) and hover (`hover_row`) stay ABSOLUTE folder
    /// indices; only the viewport moves, so scrolling never retargets them.
    pub scroll_top: i32,
    /// First-visible offset of the Settings body in logical px (`0` at the
    /// top). Paint, hit test and focus share this one offset: every Settings
    /// layout passes through the scrolled table, so a row below the fold is
    /// reachable by scrolling instead of unreachable.
    pub settings_scroll: i32,

    pub scan_active: bool,
    pub scan_cancel: Option<library::Cancel>,
    pub scan_force: bool,
    pub scan_progress: Progress,
    pub scan_stats: Stats,
    pub scan_err: Option<UiScanError>,
    pub scan_start: Instant,
    pub scan_last_paint: Instant,
    pub scan_resolve_start: Option<Instant>,
    pub scan_resolve_base: i64,
    /// The worker-published snapshot slot: the worker writes here under this
    /// lock and the wndproc arms publish it onto `UiState`.
    pub scan_shared: Arc<Mutex<ScanSnapshot>>,

    pub startup_state: bool,
    pub scan_speed_state: String,
    pub theme_state: String,
    /// Behaviour toggles the redesign paints as 36x20 pills. Each flips its
    /// own stored bool through `do_settings_action`; the paint reads the same
    /// field, so a painted pill and its click can never disagree.
    pub discord_enabled: bool,
    pub close_to_tray: bool,
    /// Test-only config-path override: when `Some`, theme/scan persistence
    /// writes there instead of the owner's store, so click tests prove
    /// persistence without touching the real file.
    pub test_config_path: Option<std::path::PathBuf>,
    /// Injectable startup registry seam: production uses the real HKCU Run
    /// key; tests arm these to flip in-memory without touching the registry.
    pub startup_set: Arc<dyn Fn(bool) -> Result<(), String> + Send + Sync>,
    pub startup_get: Arc<dyn Fn() -> bool + Send + Sync>,
    pub card_specs_state: CardSpecs,
    /// The narrow Card view's selected preview surface (the wide workspace
    /// shows all three at once). Defaults to the profile card.
    pub preview_surface: PreviewSurface,
    /// The designer's field choices. The UI owns them; every toggle publishes
    /// the same value to the live mirror the sender reads.
    pub designer: presence::DesignerSpec,
    /// The Card workspace's single search model: one title/year field, one
    /// explicit Movie/TV choice, one result set. The Fix match dialog seeds
    /// from and writes back to this model, so the main surface is canonical.
    pub picker: crate::search::MediaPicker,
    /// The playing file the picker's latch targets.
    pub picker_path: String,
    /// The picker worker's outcome queue and generation handshake. Streaming
    /// batches append; the UI drains them all on one wakeup, so a burst of
    /// results costs one repaint, not one per batch.
    picker_slot: Arc<Mutex<Vec<(u64, FixOutcome)>>>,
    /// The provider-search seam (see [`PickerSearch`]).
    pub picker_search: PickerSearch,
    /// Whether the picker may read and write the persistent metadata search
    /// cache. Off by default so a test's stub search never touches the owner's
    /// cache file; `create_main_window` turns it on.
    pub cache_enabled: bool,
    /// The playing title's preloaded results (see [`PreloadCache`]).
    pub preload: PreloadCache,
    /// The background preload's one-slot publish.
    preload_slot: Arc<Mutex<Option<(u64, PreloadResult)>>>,
    /// Monotonic preload generation: a stale finish is discarded.
    preload_generation: u64,
    /// The in-flight preload's scope, cancelled the moment the playing work
    /// changes so stale providers are not left being asked.
    preload_cancel: Option<metadata_http::cancel::Cancel>,
    /// The live card's kind, derived from the last [`UiState::record_preview`]
    /// push — never a manual toggle. `None` until the first successful send
    /// records one; the view falls back to Series meanwhile.
    preview_kind: Option<CardKind>,
    /// The local, unsent preview of a selected search result: the record the
    /// user picked, composed through the same composer the send path uses. It
    /// is shown only while no live preview exists, and it never sends Discord
    /// activity — the object card says so in words.
    local_preview: Option<(CardKind, CardPreviewInput)>,

    preview_series: Option<CardPreviewInput>,
    preview_movie: Option<CardPreviewInput>,
    /// True while the Card view's synopsis tooltip is pinned open by Enter or
    /// a click; hover and keyboard focus show it without pinning.
    pub synopsis_pinned: bool,
    /// Whether the one payload disclosure (`Advanced`) is open. At rest the
    /// window shows the object, one field, one Search and this disclosure; every
    /// field toggle, line-2 mode and the kind override live behind it.
    pub advanced_open: bool,
    pub ui_icon: usize,
    pub main_hwnd: usize,
    pub ui_centered: bool,
    pub last_mx: i32,
    pub last_my: i32,
    pub have_mouse: bool,
    /// A blocking UI action requested by the last dispatch, run by the wndproc
    /// AFTER the `UI` guard is released (`run_blocking_ui`); see [`BlockingUi`].
    pub pending_blocking: Option<BlockingUi>,
    pub report_done_text: Arc<Mutex<String>>,

    /// Injectable seams (`go\window.go:2581-2586`, `:1055-1061`).
    pub arm_timer: Arc<dyn Fn(HWND) + Send + Sync>,
    pub kill_timer: Arc<dyn Fn(HWND) + Send + Sync>,
    #[allow(clippy::type_complexity)]
    pub create_issue:
        Arc<dyn Fn(&repair::report::Report) -> (String, Option<repair::Error>) + Send + Sync>,
    pub open_url: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// `startLibraryScan` runner seam (`go\window.go:2662`
    /// `library.ScanWithOptions`): the default runs the real scan; tests inject
    /// a fake so no network is touched.
    #[allow(clippy::type_complexity)]
    pub scan_run: Arc<
        dyn Fn(&library::Cancel, library::Options) -> Result<library::Stats, library::Error>
            + Send
            + Sync,
    >,
    /// `startLibraryScan` dirs seam (`go\window.go:2646` `loadConfig().Dirs`):
    /// the default reads the live store; tests inject a temp dir.
    pub scan_dirs: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            cur_view: View::Main,
            status_text: "Ready".into(),
            ui_dirs: Vec::new(),
            sel_row: -1,
            hover_btn: 0,
            hover_row: -1,
            hover_x: false,
            pressed_btn: 0,
            pressed_x: -1,
            focus_id: 0,
            scroll_top: 0,
            settings_scroll: 0,
            scan_active: false,
            scan_cancel: None,
            scan_force: false,
            scan_progress: Progress::default(),
            scan_stats: Stats::default(),
            scan_err: None,
            advanced_open: false,
            scan_start: Instant::now(),
            scan_last_paint: Instant::now(),
            scan_resolve_start: None,
            scan_resolve_base: 0,
            scan_shared: Arc::new(Mutex::new(ScanSnapshot::default())),
            startup_state: false,
            discord_enabled: true,
            close_to_tray: true,
            test_config_path: None,
            startup_set: Arc::new(|on| {
                crate::settings::set_startup_enabled(on).map_err(|e| e.to_string())
            }),
            startup_get: Arc::new(crate::settings::query_startup_enabled),
            scan_speed_state: app_support::config::SCAN_SPEED_TURBO.to_string(),
            theme_state: app_support::config::THEME_AMOLED.to_string(),
            card_specs_state: CardSpecs::default(),
            preview_surface: PreviewSurface::default(),
            designer: presence::DesignerSpec::default(),
            picker: crate::search::MediaPicker::default(),
            picker_path: String::new(),
            picker_slot: Arc::new(Mutex::new(Vec::new())),
            picker_search: Arc::new(default_picker_search_uncached),
            cache_enabled: false,
            preload: PreloadCache::default(),
            preload_slot: Arc::new(Mutex::new(None)),
            preload_generation: 0,
            preload_cancel: None,
            preview_kind: None,
            local_preview: None,
            preview_series: None,
            preview_movie: None,
            synopsis_pinned: false,
            ui_icon: 0,
            main_hwnd: 0,
            ui_centered: false,
            last_mx: 0,
            last_my: 0,
            have_mouse: false,
            pending_blocking: None,
            report_done_text: Arc::new(Mutex::new(String::new())),
            arm_timer: Arc::new(default_arm_timer),
            kill_timer: Arc::new(default_kill_timer),
            create_issue: Arc::new(default_create_issue),
            open_url: Arc::new(default_open_url),
            scan_run: Arc::new(default_scan_run),
            scan_dirs: Arc::new(default_scan_dirs),
        }
    }
}

/// `scanArmTimer` (`go\window.go:2581`).
fn default_arm_timer(hwnd: HWND) {
    // SAFETY: SetTimer registers a timer on `hwnd`; no callback.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetTimer(
            hwnd,
            SCAN_TIMER_ID,
            SCAN_PAINT_INTERVAL.as_millis() as u32,
            None,
        );
    }
}

/// `scanKillTimer` (`go\window.go:2585`).
fn default_kill_timer(hwnd: HWND) {
    // SAFETY: KillTimer stops the registered id on `hwnd`.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::KillTimer(hwnd, SCAN_TIMER_ID);
    }
}

/// `reportCreateIssue` default (`go\window.go:1055`).
fn default_create_issue(r: &repair::report::Report) -> (String, Option<repair::Error>) {
    let opts = repair::issue::Options {
        repo: REPORT_REPO.to_string(),
        runner: None,
        labels: vec![REPORT_LABEL.to_string()],
        gh: repair::issue::Gh::default(),
    };
    repair::issue::create_issue(r, &opts)
}

/// `reportOpenURL` default (`go\window.go:1061`).
fn default_open_url(url: &str) -> bool {
    open_external_url(url)
}

/// `library.ScanWithOptions` default for the [`UiState::scan_run`] seam.
fn default_scan_run(
    cancel: &library::Cancel,
    opts: library::Options,
) -> Result<library::Stats, library::Error> {
    library::scan_with_options(cancel, opts)
}

/// `loadConfig().Dirs` default for the [`UiState::scan_dirs`] seam.
fn default_scan_dirs() -> Vec<String> {
    app_support::config::load_config().dirs
}

/// `openExternalURL` (`go\window.go:1120`).
#[must_use]
pub fn open_external_url(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    let target = utf16_z(url);
    let op = utf16_z("open");
    // SAFETY: two live NUL-terminated UTF-16 buffers; ShellExecuteW reads them.
    let ret = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOW_NORMAL,
        )
    };
    ret as isize > 32
}

// ---------------------------------------------------------------------------
// "Fix match" -- the manual latch picker's pure half.
//
// Everything in this section is UI-free and unit-tested: the copy (frozen), the
// enable rule, the pre-fill, the scope/keys table and the status strings. The
// dialog window and its worker live further down; nothing here touches an HWND
// except through the [`Rect`] table the paint/creation paths both read.
//
// # Scope -> key rules (the store's own derivations; spelled out here once)
//
//   * `Scope::Movie`  -> `movie_key(path)`    (`overrides::movie_key`): the
//     media file's FULL path. A movie offers NO scope radios -- there is only
//     one target -- and the dialog pins it directly.
//   * `Scope::Season` -> `season_key_for(path)`: the folder that directly
//     holds the episode (the `Season N` / `SNN` folder itself, otherwise the
//     release-style folder that IS that season).
//   * `Scope::Series` -> `series_key_for(path)`: the show folder, one level
//     above a `Season N` folder when there is one.
//
// When the two derived keys are EQUAL the folder is both the season and the
// show (a release-style folder like `Atri S01 1080p BDRip`, or an
// absolute-numbered show folder with the episodes inside it): the dialog then
// offers ONE radio for that folder instead of two identical targets, and says
// so in its note. The choice is `Scope::Series` because the label describes the
// folder as the show; the keys are identical either way.
// ---------------------------------------------------------------------------

/// The Card hero's ghost repair verb. Approved copy drops the ellipsis.
pub const FIX_MATCH_LABEL: &str = "Fix match";
/// The dialog's title line. Frozen copy.
pub const FIX_MATCH_TITLE: &str = "Fix match";
/// The busy static while a provider pass runs. Frozen copy.
pub const FIX_MATCH_SEARCHING: &str = "Searching\u{2026}";
/// The busy panel's per-phase lines: one per job kind the worker runs. New
/// copy for the owner-drawn empty panel (the old searching static stays).
pub const FIX_MATCH_MATCHING: &str = "Matching\u{2026}";
pub const FIX_MATCH_REFRESHING: &str = "Refreshing\u{2026}";
pub const FIX_MATCH_RESTORING: &str = "Restoring automatic matching\u{2026}";
/// The non-blocking match status: the provisional correction already saved,
/// so the card shows the corrected series/movie while the full episode table
/// still fetches. Set on the main status line the moment the provisional
/// saves (before the network pin runs) and kept as the dialog's closing
/// status until the full record replaces it. Frozen copy.
pub const FIX_MATCH_FETCHING_STATUS: &str = "Matched; fetching episodes\u{2026}";
/// The pre-search empty panel: a heading plus a one-line hint. New copy.
pub const FIX_MATCH_EMPTY_TITLE: &str = "Find the right match";
pub const FIX_MATCH_EMPTY_HINT: &str = "Type a title and year, then Search.";
/// The no-results empty panel heading. New copy; the body stays
/// [`FIX_MATCH_NO_RESULTS`].
pub const FIX_MATCH_NO_MATCHES_TITLE: &str = "No matches";
/// Empty-results copy. Frozen copy.
pub const FIX_MATCH_NO_RESULTS: &str =
    "No results. Try the title without the episode number, or add the year.";
/// The provider-free latch button. Frozen copy.
pub const FIX_MATCH_USE_TEXT: &str = "Use my text as the title";
/// The failure copy for a provider/store error: the dialog stays open. Frozen
/// copy; `Nothing was saved` is the promise every failure path keeps.
pub const FIX_MATCH_ERROR: &str = "Couldn't reach the metadata providers. Nothing was saved.";
/// The reason a dimmed picker reports when it is clicked with nothing playing
/// (the enable rule's second half; there is no tooltip to lie in).
pub const FIX_MATCH_IDLE_REASON: &str = "Nothing is playing - start a video to fix its match.";
/// The scope section's label. Frozen copy.
pub const FIX_MATCH_SCOPE_LABEL: &str = "Apply to";
/// The name field's label. Frozen copy (two spaces before the parenthetical).
/// The one query field's label: a title, or a title and year in the same
/// field. There is no separate Year control (owner direction 2026-10-29).
pub const FIX_MATCH_NAME_LABEL: &str = "Title  (or title + year)";
/// The picker's explicit kind choice (owner direction 2026-10-29): the choice
/// drives the provider search and the latch scope, instead of deriving the
/// kind from the filename or the selected candidate.
pub const FIX_MATCH_KIND_LABEL: &str = "Kind";
pub const FIX_MATCH_KIND_MOVIE: &str = "Movie";
pub const FIX_MATCH_KIND_TV: &str = "TV";
/// The buttons. Frozen copy.
pub const FIX_MATCH_SEARCH_LABEL: &str = "Search";
pub const FIX_MATCH_MATCH_LABEL: &str = "Match";
pub const FIX_MATCH_CANCEL_LABEL: &str = "Cancel";
pub const FIX_MATCH_REFRESH_LABEL: &str = "Refresh metadata";
pub const FIX_MATCH_CHANGE_LABEL: &str = "Change match\u{2026}";
pub const FIX_MATCH_AUTO_LABEL: &str = "Use automatic matching";
/// The "Use automatic matching" status line. Frozen copy.
pub const FIX_MATCH_AUTO_STATUS: &str = "Back to automatic matching.";
/// The already-open guard: a second open while the modal picker lives is
/// refused with this status instead of stacking a nested loop.
pub const FIX_MATCH_ALREADY_OPEN_REASON: &str = "Fix match is already open.";
/// The visible reason when the playing file yields no latch target: the path
/// is empty or unresolvable, so both scope keys are `None` and `Match` would
/// otherwise die silently.
pub const FIX_MATCH_NO_TARGET_REASON: &str =
    "Nothing to match — the playing file has no folder to latch.";
/// The visible reason when the whole-show target would be a bare volume: a
/// file directly under a drive root (`C:\Season 2\...`) must never latch the
/// drive, so the show key is refused and the dialog says so.
pub const FIX_MATCH_DRIVE_ROOT_REASON: &str =
    "Whole-show match refused — the file sits directly under a drive root.";

/// The enable rule: the picker is offered only while something plays. `None`
/// when the player is gone (or the poll loop has not published yet).
#[must_use]
pub fn fix_match_enabled(now: Option<&app_support::config::NowPlaying>) -> bool {
    now.is_some()
}

/// The picker's blocked reason for `id`: non-empty exactly when `id` is the
/// picker button and nothing plays. Any other id never blocks. The paint path
/// dims on a non-empty reason and the click path refuses on the same value, so
/// the two can never disagree.
#[must_use]
pub fn fix_match_blocked_reason(playing: bool, id: i32) -> &'static str {
    if id == BTN_FIX_MATCH && !playing {
        return FIX_MATCH_IDLE_REASON;
    }
    ""
}

/// Whether the playing path sits directly under a drive root, so the
/// whole-show key would be a bare volume (`c:`) and must be refused. Pure so
/// the picker and its tests pin it without a window: a file whose parent is
/// a drive, or whose season folder's parent is a drive.
#[must_use]
pub fn fix_match_drive_rooted(path: &str) -> bool {
    let key = metadata::overrides::normalize_key(path);
    let Some((parent, _)) = key.rsplit_once('\\') else {
        return false;
    };
    if metadata::overrides::is_volume_key(parent) {
        return true;
    }
    let Some((above, folder)) = parent.rsplit_once('\\') else {
        return false;
    };
    let lower = folder.to_lowercase();
    let is_season = {
        if let Some(rest) = lower.strip_prefix("season") {
            let digits = rest.trim();
            !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
        } else if let Some(digits) = lower.strip_prefix('s') {
            !digits.is_empty() && digits.len() <= 2 && digits.bytes().all(|b| b.is_ascii_digit())
        } else {
            false
        }
    };
    is_season && metadata::overrides::is_volume_key(above)
}

/// The visible reason when the search screen has no latch target: the
/// drive-root refusal wins over the generic empty-path reason, and `None`
/// means there IS a target (the dialog stays quiet). Pure so tests pin the
/// copy without a window.
#[must_use]
pub fn fix_match_no_target_reason(path: &str, is_movie: bool) -> Option<&'static str> {
    if is_movie {
        if metadata::overrides::movie_key(path).is_empty() {
            return Some(FIX_MATCH_NO_TARGET_REASON);
        }
        return None;
    }
    if fix_match_drive_rooted(path) {
        return Some(FIX_MATCH_DRIVE_ROOT_REASON);
    }
    if metadata::overrides::season_key_for(path).is_none()
        || metadata::overrides::series_key_for(path).is_none()
    {
        return Some(FIX_MATCH_NO_TARGET_REASON);
    }
    None
}

/// The name field's starting text: the folder-aware query for what's playing,
/// the same [`metadata_core::context::resolve_query`] merge the repair path
/// and the card path use.
///
/// A series filename rarely carries the show (an episode-only file like
/// `S01E02-A Warm View Together [..].mkv` parses to the EPISODE title), so for
/// a series the folder's show name wins -- but only when the resolver's title
/// is NOT the same work as the folder show. A release-junk folder
/// (`Severance.S01.1080p.WEBRip.DS4K.AV1...`) cleans to `Severance S01 DS4K`,
/// and the resolver's own `Severance` is the searchable show, so the shorter,
/// overlapping name wins; a folder whose show is absent from the filename
/// (`Atri`) still supplies it.
///
/// A movie filename already carries its own title, so it keeps the resolver's
/// parse (`Dune Part Two`, never the dotted filename), with the release-junk
/// tail a dotted name leaves behind removed: a release year followed by more
/// words ends the title (`A Silent Voice 2016 Opus2 0` -> `A Silent Voice`),
/// and a word repeated back to back collapses (`2012 2012` -> `2012`). The
/// parsed year is carried in the field as `Title (Year)`, because the search's
/// own year signal is what ranks the right release above a remake.
///
/// With no usable folder show the prefill falls back to the resolver's own
/// parse of the raw window title, and then to the card path's titleclean pass.
/// [`metadata_core::context::context_from_path`] yields no show for a bare
/// filename, so a movie with no folder context still prefills its own title.
#[must_use]
pub fn fix_match_prefill_name(raw_title: &str, path: &str) -> String {
    let q = metadata_core::context::resolve_query(raw_title, path);
    if q.kind == Kind::SERIES {
        let (show, _) = metadata_core::context::context_from_path(path);
        let folder = if show.trim().is_empty() {
            String::new()
        } else {
            let cleaned = titleclean::clean(&show).title.trim().to_string();
            if cleaned.is_empty() {
                show.trim().to_string()
            } else {
                cleaned
            }
        };
        let parsed = q.title.trim();
        if !parsed.is_empty() && !folder.is_empty() && same_work_name(parsed, &folder) {
            return parsed.to_string();
        }
        if !folder.is_empty() {
            return folder;
        }
    }
    let title = if q.kind == Kind::MOVIE {
        movie_search_title(&q.title, q.year)
    } else {
        q.title.trim().to_string()
    };
    if !title.is_empty() {
        if q.kind == Kind::MOVIE && q.year > 0 {
            return format!("{title} ({})", q.year);
        }
        return title;
    }
    titleclean::clean(raw_title).title.trim().to_string()
}

/// The name field's starting text when the dialog opens: the card's own query
/// only when that query belongs to the file that is playing now. A query the
/// user typed for another file (a switch, or the next episode of the same show)
/// is not this file's query -- the file seeds itself, carrying its own year,
/// and its own season/episode comes from the path.
#[must_use]
pub fn fix_match_seed_query(
    card_query: &str,
    card_is_this_file: bool,
    raw_title: &str,
    path: &str,
) -> String {
    if card_is_this_file && !card_query.trim().is_empty() {
        return card_query.trim().to_string();
    }
    fix_match_prefill_name(raw_title, path)
}

/// Whether two cleaned names are the same work: one's compacted alphanumeric
/// form contains the other's. `Re Zero`/`ReZero` and `Severance`/`Severance
/// S01 DS4K` are the same work, so the file's own parse is the title; `Atri`
/// and `A Warm View Together` are not, so the folder's show wins.
fn same_work_name(a: &str, b: &str) -> bool {
    fn compact(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    }
    let (a, b) = (compact(a), compact(b));
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

/// A movie's searchable title from the resolver's parse, with the release-junk
/// tail a dotted release name leaves behind removed: a token equal to the known
/// release year followed by more words ends the title there (`A Silent Voice
/// 2016 Opus2 0` -> `A Silent Voice`; `Blade Runner 2049` is untouched because
/// its year is 2017), and a word repeated back to back collapses (`2012 2012`
/// -> `2012`).
fn movie_search_title(title: &str, year: i32) -> String {
    let mut words: Vec<&str> = title.split_whitespace().collect();
    if year > 0
        && let Some(at) = words.iter().position(|w| w.parse::<i32>().ok() == Some(year))
        && at + 1 < words.len()
    {
        words.truncate(at);
    }
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    for word in words {
        if out
            .last()
            .is_some_and(|last| last.eq_ignore_ascii_case(word))
        {
            continue;
        }
        out.push(word);
    }
    out.join(" ").trim().to_string()
}

/// The provider id as the dialog shows it ("anilist" -> "AniList").
#[must_use]
pub fn provider_display_name(provider: &str) -> String {
    match provider {
        "anilist" => "AniList".to_string(),
        "tvmaze" => "TVmaze".to_string(),
        "cinemeta" => "Cinemeta".to_string(),
        "wikipedia" => "Wikipedia".to_string(),
        "justwatch" => "JustWatch".to_string(),
        "tenrai" => "Tenrai".to_string(),
        "kitsu" => "Kitsu".to_string(),
        "episodate" => "Episodate".to_string(),
        "bangumi" => "Bangumi".to_string(),
        "livechart" => "LiveChart".to_string(),
        "ann" => "ANN".to_string(),
        other => other.to_string(),
    }
}

/// One result row: `Attack on Titan (2013) · AniList · 25 episodes`. A movie
/// omits ` · N episodes` (it has no episode table); a series with no table
/// omits it too, and a zero year is dropped rather than printed `(0)`.
#[must_use]
pub fn candidate_row_text(c: &Candidate) -> String {
    let mut s = c.title.trim().to_string();
    if c.year > 0 {
        s.push_str(&format!(" ({})", c.year));
    }
    let provider = provider_display_name(&c.provider);
    if !provider.is_empty() {
        s.push_str(" · ");
        s.push_str(&provider);
    }
    if c.kind != Kind::MOVIE && c.episode_count > 0 {
        let plural = if c.episode_count == 1 { "" } else { "s" };
        s.push_str(&format!(" · {} episode{}", c.episode_count, plural));
    }
    s
}

/// One owner-drawn result row as two lines: the title/year on line one, the
/// provider/type/episode count on line two. Pure so the list paint and its
/// tests pin the same split: a movie omits the episode clause, a series with
/// no table omits it too, and a zero year is dropped.
#[must_use]
pub fn fix_match_row_lines(c: &Candidate) -> (String, String) {
    let mut title = c.title.trim().to_string();
    if c.year > 0 {
        title.push_str(&format!(" ({})", c.year));
    }
    let mut sub = provider_display_name(&c.provider);
    let kind_word = crate::search::category_label(c);
    if sub.is_empty() {
        sub = kind_word.to_string();
    } else {
        sub.push_str(" · ");
        sub.push_str(kind_word);
    }
    if c.kind != Kind::MOVIE && c.episode_count > 0 {
        let plural = if c.episode_count == 1 { "" } else { "s" };
        sub.push_str(&format!(" · {} episode{}", c.episode_count, plural));
    }
    (title, sub)
}

/// Whether one picked [`Candidate`] is a movie: the selected Fix-match
/// candidate drives the correction scope, overriding the filename query.
///
/// The leg-reported [`Kind`] wins; an unknown kind falls back to the episode
/// table tell (a table means series) and then to the provider tell (TVmaze,
/// AniList and the other series-only legs stay series; a movie has no
/// episodes). No network, no fetch.
#[must_use]
pub fn fix_candidate_is_movie(cand: &Candidate) -> bool {
    if cand.kind == Kind::MOVIE {
        return true;
    }
    if cand.kind == Kind::SERIES {
        return false;
    }
    if cand.episode_count > 0 {
        return false;
    }
    let provider = cand.provider.to_lowercase();
    !matches!(
        provider.as_str(),
        "tvmaze"
            | "anilist"
            | "tenrai"
            | "kitsu"
            | "episodate"
            | "bangumi"
            | "livechart"
            | "ann"
            | "jikan"
    )
}

/// One owner-drawn results row's height, logical px at 96 DPI: two text lines
/// plus padding. A layout token, not a free-hand number.
pub const FIX_MATCH_ROW_H_L: i32 = 56;
/// The indeterminate progress strip's height, logical px.
pub const FIX_MATCH_PROGRESS_H_L: i32 = 4;

/// The dialog push buttons' styles in the main window's vocabulary: Search is
/// the secondary outline, Match is the single accent primary, Cancel is
/// subtle, the provider-free latch is a quiet text button, and the matched
/// screen's three actions are secondaries. Pure so the creation path and the
/// draw path cannot disagree.
#[must_use]
pub fn fix_match_button_style(id: i32) -> i32 {
    match id {
        FM_ID_MATCH => BTN_ACCENT,
        FM_ID_SEARCH | FM_ID_REFRESH | FM_ID_CHANGE => BTN_OUTLINE,
        FM_ID_CANCEL => BTN_SUBTLE,
        FM_ID_USE_TEXT | FM_ID_AUTO => BTN_TEXT,
        _ => BTN_OUTLINE,
    }
}

/// The `WM_CTLCOLORSTATIC` text color for one static: the title in `COL_TEXT`,
/// labels/sub/status/empty/searching in `COL_MUTED`, and the error status in
/// `COL_DANGER`. Pure so the paint and its tests agree.
#[must_use]
pub fn fix_match_static_color(id: i32, status_error: bool) -> u32 {
    if id == FM_ID_TITLE || id == FM_ID_APPLIED {
        return COL_TEXT;
    }
    if id == FM_ID_STATUS && status_error {
        return COL_DANGER;
    }
    COL_MUTED
}

/// The enable rules, pure so tests pin them without a window: Search needs a
/// non-empty name and no running job; Match needs a selected candidate plus a
/// latch target and no running job; the provider-free secondary is enabled
/// only while its no-results copy shows.
#[must_use]
pub fn fix_match_search_allowed(name_empty: bool, busy: bool) -> bool {
    !name_empty && !busy
}

/// The newest year the one field accepts as a release year. Build-stamped: two
/// years past this build. A trailing future number is an id in the title
/// (`Blade Runner 2049`), not a release year, and the search gates hard on the
/// year it is given.
pub const LATEST_RELEASE_YEAR: i32 = 2028;

/// Parses the one query field into `(title, year)`: a trailing release year
/// token is the year and the text before it is the title; a bare year is a
/// year-only query (`""` title); anything else is the title alone. A year is
/// four digits in `1900..=1999` (always) or `2000..=LATEST_RELEASE_YEAR`;
/// `Blade Runner 2049` is all title. The rules match the approved prototype:
/// `Dune 2021` -> `("Dune", 2021)`, `1984` -> `("", 1984)`.
/// Only the trailing token is considered, so a year-looking word inside a
/// title survives when another word follows it. The Card renders a match as
/// `Title (Year)`, so the parse inverts exactly that: `Dune (2021)`,
/// `Dune, 2021` and `Dune: 2021` (comma/colon with or without the space) all
/// yield the typed year, and full-width digits/parens/commas/colons fold to
/// their ASCII halves before the match.
#[must_use]
pub fn parse_title_year(input: &str) -> (String, i32) {
    fn fold_fullwidth(s: &str) -> String {
        s.chars()
            .map(|c| match c {
                '\u{FF10}'..='\u{FF19}' => (c as u32 - 0xFF10 + b'0' as u32) as u8 as char,
                '\u{FF08}' => '(',
                '\u{FF09}' => ')',
                '\u{FF0C}' => ',',
                '\u{FF1A}' => ':',
                '\u{3000}' => ' ',
                _ => c,
            })
            .collect()
    }
    let text = fold_fullwidth(input.trim());
    let text = text.trim();
    if text.is_empty() {
        return (String::new(), 0);
    }
    let is_year = |s: &str| {
        s.len() == 4
            && s.bytes().all(|b| b.is_ascii_digit())
            && s.parse::<i32>().is_ok_and(|y| {
                (1900..=1999).contains(&y) || (2000..=LATEST_RELEASE_YEAR).contains(&y)
            })
    };
    // A parenthesized suffix inverts the `Title (Year)` the Card paints:
    // `Dune (2021)` -> `("Dune", 2021)`, `(2021)` alone -> year-only.
    if text.ends_with(')')
        && let Some(open) = text.rfind('(')
    {
        let inner = text[open + 1..text.len() - 1].trim();
        if is_year(inner) {
            let head = text[..open].trim_end().to_string();
            let head = head
                .trim_end_matches([',', ':'])
                .trim_end()
                .to_string();
            let year = inner.parse::<i32>().unwrap_or(0);
            return (head, year);
        }
    }
    // A comma/colon glued to the year (`Dune,2021`, `Dune:2021`) splits
    // before the whitespace rule below sees one token.
    let t = text.trim_end().to_string();
    {
        let comma = t.rfind(',');
        let colon = t.rfind(':');
        let pos = match (comma, colon) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        if let Some(pos) = pos {
            let after = t[pos + 1..].trim();
            if is_year(after) {
                let head = t[..pos].trim_end().to_string();
                let year = after.parse::<i32>().unwrap_or(0);
                return (head, year);
            }
        }
    }
    let mut parts = t.rsplitn(2, char::is_whitespace);
    let last = parts.next().unwrap_or("");
    // The whitespace split leaves `Dune,` as the head for `Dune, 2021`: strip
    // the one trailing separator so the title is `Dune`.
    let last_clean = last.trim_end_matches([',', ':', ')']).trim_start_matches('(');
    if !is_year(last_clean) {
        return (t, 0);
    }
    let mut head = parts.next().unwrap_or("").trim_end().to_string();
    head = head.trim_end_matches([',', ':']).trim_end().to_string();
    // A bare `(2021)` leaves head `(` after the split; that is year-only.
    if head == "(" {
        head.clear();
    }
    let year = last_clean.parse::<i32>().unwrap_or(0);
    (head, year)
}

/// Whether the one query field may run Search: a title or a bare year is
/// enough (a bare year is a valid filter), a blank is not, and a running job
/// freezes the button.
#[must_use]
pub fn search_query_allowed(input: &str, busy: bool) -> bool {
    let (title, year) = parse_title_year(input);
    !busy && (!title.is_empty() || year > 0)
}

/// Whether `Match` may run: a candidate is selected, a latch target exists,
/// and no job runs.
#[must_use]
pub fn fix_match_match_allowed(has_selection: bool, has_target: bool, busy: bool) -> bool {
    has_selection && has_target && !busy
}

/// Whether `Use my text as the title` may run: only while its no-results copy
/// shows (and no job runs).
#[must_use]
pub fn fix_match_use_text_allowed(no_results: bool, busy: bool) -> bool {
    no_results && !busy
}

/// The search screen's contextual sub-line: `Matching "<media title>"` (the raw
/// title minus the player suffix, quoted). It names the file being matched,
/// never a verdict: before a search there is nothing to report as unmatched.
/// The strip is the card path's own: `presence` strips `player::window::SUFFIX`
/// for the idle check and `metadata_core::parse::parse_raw` strips its
/// `POT_PLAYER_SUFFIX` twin for the prefill — neither constant is reachable
/// from this crate's deps, so the value is spelled at this one call site.
#[must_use]
pub fn fix_match_sub_line(raw_title: &str) -> String {
    let media = raw_title.strip_suffix(" - PotPlayer").unwrap_or(raw_title);
    format!("Matching \"{media}\"")
}

/// The matched sub-line: `Matched by you · AniList — Attack on Titan, all 25
/// episodes`. A provider-free latch (the typed title) drops the provider
/// segment; a movie drops the episode clause (it has no pins).
#[must_use]
pub fn fix_match_matched_sub_line(over: &Override) -> String {
    let provider = provider_display_name(&over.provider);
    let mut s = String::from("Matched by you");
    if !provider.is_empty() {
        s.push_str(" · ");
        s.push_str(&provider);
    }
    s.push_str(" — ");
    s.push_str(&over.title);
    if over.scope != Scope::Movie && !over.episodes.is_empty() {
        s.push_str(&format!(", all {} episodes", over.episodes.len()));
    }
    s
}

/// The matched line: `Applied to  Attack on Titan \ Season 2` (two spaces after
/// `to`; frozen copy). A movie reports its file, a show its title, a season its
/// title plus the season component of the stored key (case restored for the
/// `Season N` / `SNN` shapes; a release-style folder keeps its stored form).
#[must_use]
pub fn fix_match_applied_text(over: &Override, path: &str) -> String {
    let label = match over.scope {
        Scope::Movie => path_last(path).to_string(),
        Scope::Series => over.title.clone(),
        Scope::Season => format!(
            "{} \\ {}",
            over.title,
            season_component_display(key_last(&over.key))
        ),
    };
    format!("Applied to  {label}")
}

/// The match status: `Matched to <title> — <N> episodes now show their own
/// title and cover.` A movie (and any latch with no pinned episodes) is
/// `Matched to <title>.` -- never a fabricated episode count.
#[must_use]
pub fn fix_match_matched_status(title: &str, episodes: usize) -> String {
    if episodes == 0 {
        return format!("Matched to {title}.");
    }
    format!("Matched to {title} — {episodes} episodes now show their own title and cover.")
}

/// The refresh status: `Refreshed <title>.`
#[must_use]
pub fn fix_match_refreshed_status(title: &str) -> String {
    format!("Refreshed {title}.")
}

/// The pending copy after the full pin failed but the provisional correction
/// already saved: the latch stays usable (the card keeps the corrected title
/// with no episode table yet), so this never carries [`FIX_MATCH_ERROR`]'s
/// `Nothing was saved` promise, which would lie.
#[must_use]
pub fn fix_match_episodes_pending_status(title: &str) -> String {
    format!("Matched to {title}; episode details unavailable \u{2014} the card keeps the corrected title.")
}

/// One scope radio: the store target it writes plus the frozen-copy label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeChoice {
    /// The scope a pick writes.
    pub scope: Scope,
    /// The normalised store key ([`season_key_for`]/[`series_key_for`]).
    pub key: String,
    /// The radio's label.
    pub label: String,
    /// A plain one-line explanation, only where two targets would otherwise be
    /// identical (the release-style folder).
    pub note: Option<String>,
    /// Whether this radio starts selected.
    pub default: bool,
}

/// The scope targets the picker offers for one playing file. A movie offers
/// none (one target, pinned directly); a file in a `Season N` folder offers the
/// season (default) and the whole show; a folder that is both at once offers
/// the one real target and says so. A bare-volume key (a file directly under
/// a drive root, whose whole-show key would be `c:`) offers nothing: latching
/// the drive is refused and [`fix_match_no_target_reason`] says so in the UI.
/// See the section comment for the key rules.
#[must_use]
pub fn fix_match_scope_choices(path: &str, is_movie: bool) -> Vec<ScopeChoice> {
    if is_movie {
        // No radios: the one target is the movie file's own key.
        return Vec::new();
    }
    let (Some(season_key), Some(series_key)) = (season_key_for(path), series_key_for(path)) else {
        return Vec::new();
    };
    if metadata::overrides::is_volume_key(&season_key)
        || metadata::overrides::is_volume_key(&series_key)
    {
        return Vec::new();
    }
    let folder = path_parent_name(path).to_string();
    if season_key == series_key {
        // One folder is both the season and the show: one radio, labelled with
        // that folder, plus the plain note the brief asks for.
        let note = if picker_is_season_folder(&folder) || folder_reads_as_season(&folder) {
            Some(
                "This folder is both the show folder and its season, so one match covers every \
                 episode in it."
                    .to_string(),
            )
        } else {
            None
        };
        return vec![ScopeChoice {
            scope: Scope::Series,
            key: series_key,
            label: format!("Whole show  — {folder}"),
            note,
            default: true,
        }];
    }
    let show = path_grandparent_name(path).to_string();
    vec![
        ScopeChoice {
            scope: Scope::Season,
            key: season_key,
            label: format!("This season — {show} \\ {folder}"),
            note: None,
            default: true,
        },
        ScopeChoice {
            scope: Scope::Series,
            key: series_key,
            label: format!("Whole show  — {show}"),
            note: None,
            default: false,
        },
    ]
}

/// The plugin-free latch `Use my text as the title` writes: the typed name as
/// the title, the typed year (0 for none), the chosen scope and key -- and NO
/// provider identity, cover or episode pins. The ladder keeps every field the
/// latch does not carry.
#[must_use]
pub fn fix_match_text_override(title: &str, year: i32, scope: Scope, key: &str) -> Override {
    Override {
        scope,
        key: key.to_string(),
        title: title.trim().to_string(),
        year: year.max(0),
        provider: String::new(),
        provider_id: String::new(),
        poster: String::new(),
        media: None,
        episodes: Vec::new(),
        created_at_ms: metadata::overrides::now_ms(),
    }
}

/// The last `\`- or `/`-separated component of a path (`""` when none).
fn path_last(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or("")
}

/// The containing folder's display name (second-to-last component).
fn path_parent_name(path: &str) -> &str {
    let mut parts = path.rsplit(['\\', '/']).filter(|s| !s.is_empty());
    let _file = parts.next();
    parts.next().unwrap_or("")
}

/// The show folder's display name (third-to-last component), used when the
/// containing folder is the season folder.
fn path_grandparent_name(path: &str) -> &str {
    let mut parts = path.rsplit(['\\', '/']).filter(|s| !s.is_empty());
    let _file = parts.next();
    let _parent = parts.next();
    parts.next().unwrap_or("")
}

/// The last component of a normalised store key.
fn key_last(key: &str) -> &str {
    key.rsplit('\\').next().unwrap_or(key)
}

/// `season 2` -> `Season 2`, `s02` -> `S02`; anything else is kept as stored.
fn season_component_display(component: &str) -> String {
    let lower = component.to_lowercase();
    if let Some(rest) = lower.strip_prefix("season") {
        let digits = rest.trim();
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return format!("Season {digits}");
        }
    }
    if let Some(digits) = lower.strip_prefix('s')
        && !digits.is_empty()
        && digits.len() <= 2
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        return format!("S{digits}");
    }
    component.to_string()
}

/// Whether one folder name is exactly a season folder (`Season N` / `SNN`), the
/// same rule `metadata::overrides` applies internally. Only the LABEL/note uses
/// it; every store key still comes from `season_key_for`/`series_key_for`.
fn picker_is_season_folder(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    if let Some(rest) = lower.strip_prefix("season") {
        let digits = rest.trim();
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    if let Some(digits) = lower.strip_prefix('s') {
        return !digits.is_empty()
            && digits.len() <= 2
            && digits.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

/// Whether a folder name READS as a season release (`Atri S01 1080p BDRip`,
/// `Show Season 2 [1080p]`): an `sNN` token or `season` followed by a number.
/// This only decides whether the one-target case carries its plain note.
fn folder_reads_as_season(name: &str) -> bool {
    let lower = name.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    for (i, token) in tokens.iter().enumerate() {
        if let Some(digits) = token.strip_prefix('s')
            && !digits.is_empty()
            && digits.len() <= 2
            && digits.bytes().all(|b| b.is_ascii_digit())
        {
            return true;
        }
        if (*token == "season" || *token == "seasons")
            && tokens
                .get(i + 1)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            return true;
        }
    }
    false
}

/// The picker's window class.
pub const FIX_MATCH_CLASS_NAME: &str = "PotflixFixMatchWnd";

/// The picker's stock-control ids (WM_COMMAND ids; distinct from the rect-table
/// button ids which are another vocabulary entirely).
const FM_ID_TITLE: i32 = 200;
const FM_ID_SUB: i32 = 201;
const FM_ID_NAME_LABEL: i32 = 202;
const FM_ID_NAME: i32 = 203;
/// The kind row's label (the retired Year label's slot).
const FM_ID_KIND_LABEL: i32 = 204;
/// The explicit Movie/TV radios (the retired Year edit's slot).
const FM_ID_KIND_MOVIE: i32 = 205;
const FM_ID_KIND_TV: i32 = 226;
const FM_ID_SEARCH: i32 = 206;
const FM_ID_LIST: i32 = 207;
const FM_ID_SEARCHING: i32 = 208;
const FM_ID_SCOPE_LABEL: i32 = 209;
const FM_ID_RADIO_A: i32 = 210;
const FM_ID_RADIO_B: i32 = 211;
const FM_ID_NOTE: i32 = 212;
const FM_ID_STATUS: i32 = 213;
const FM_ID_MATCH: i32 = 214;
const FM_ID_CANCEL: i32 = 215;
const FM_ID_USE_TEXT: i32 = 216;
const FM_ID_APPLIED: i32 = 217;
const FM_ID_REFRESH: i32 = 218;
const FM_ID_CHANGE: i32 = 219;
const FM_ID_AUTO: i32 = 220;
/// The centered empty/busy panel: an owner-drawn static covering the list
/// area so it is never a blank box.
const FM_ID_EMPTY: i32 = 221;
/// The 4px indeterminate strip shown while a provider job runs.
const FM_ID_PROGRESS: i32 = 222;
/// The 1px header rule's owner-drawn static.
const FM_ID_RULE: i32 = 223;
/// The Name field's owner-drawn frame: a rounded `COL_BORDER` field behind
/// the borderless edit, lit `COL_ACCENT` while the edit has focus.
const FM_ID_NAME_FRAME: i32 = 224;

/// `SetTimer` id for the Fix match indeterminate strip. Distinct from the
/// main window's `SCAN_TIMER_ID` (a different window, but one id space per
/// thread keeps the captures unambiguous).
pub const FIX_MATCH_TIMER_ID: usize = 8;
/// The strip's repaint cadence while busy.
pub const FIX_MATCH_TIMER_INTERVAL: Duration = Duration::from_millis(100);

/// The live picker window, or null: set while the modal dialog exists. The
/// capture harness reaches the dialog through this instead of guessing a
/// handle; the real UI never needs it.
static FIX_MATCH_HWND: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The picker's window handle while it is open (null otherwise).
#[must_use]
pub fn fix_match_dialog_hwnd() -> HWND {
    FIX_MATCH_HWND.load(std::sync::atomic::Ordering::SeqCst) as HWND
}

/// One picker screen: the search form or the already-latched state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixScreen {
    Search,
    Matched,
}

/// The picker's control rects at one DPI for one screen. Pure; the dialog
/// window and the layout tests both read this one table.
#[derive(Clone, Debug, Default)]
pub struct FixMatchLayout {
    pub client_w: i32,
    pub client_h: i32,
    pub title: Rect,
    pub sub: Rect,
    /// The 1px `COL_BORDER` rule under the header.
    pub rule: Rect,
    pub name_label: Rect,
    pub name_edit: Rect,
    /// The Name field's owner-drawn frame (the edit sits 2px inside it).
    pub name_frame: Rect,
    /// The explicit Movie/TV choice: its label and the two radio rects.
    pub kind_label: Rect,
    pub kind_radios: Vec<Rect>,
    pub search: Rect,
    pub list: Rect,
    pub searching: Rect,
    /// The centered empty/busy panel (covers the list area when the list is
    /// hidden so the box is never blank).
    pub empty: Rect,
    /// The 4px indeterminate strip shown while busy.
    pub progress: Rect,
    pub scope_label: Rect,
    pub radios: Vec<Rect>,
    pub note: Rect,
    pub applied: Rect,
    pub status: Rect,
    /// `(control id, rect)` in creation order.
    pub buttons: Vec<(i32, Rect)>,
}

/// `fixMatchLayout`: the dialog's geometry, in logical px scaled by `dpi`.
#[must_use]
pub fn fix_match_layout(dpi: i32, screen: FixScreen, radios: usize) -> FixMatchLayout {
    let sc = |v: i32| scale_px(dpi, v);
    let m = sc(20);
    let right = sc(520) - m;
    let mut l = FixMatchLayout {
        client_w: sc(520),
        ..FixMatchLayout::default()
    };
    let title = Rect {
        left: m,
        top: sc(14),
        right,
        bottom: sc(38),
    };
    let sub = Rect {
        left: m,
        top: sc(44),
        right,
        bottom: sc(64),
    };
    l.title = title;
    l.sub = sub;
    // The 1px `COL_BORDER` rule under the header, full client width.
    l.rule = Rect {
        left: 0,
        top: sc(70),
        right: sc(520),
        bottom: sc(70) + sc(STROKE_HAIR).max(1),
    };
    match screen {
        FixScreen::Search => {
            l.name_label = Rect {
                left: m,
                top: sc(78),
                right,
                bottom: sc(96),
            };
            l.name_edit = Rect {
                left: m,
                top: sc(98),
                right: m + sc(300),
                bottom: sc(126),
            };
            // The field frame is the border: the edit geometry expanded by
            // 2px on every side, so the edit itself never moves.
            let pad = sc(2);
            l.name_frame = Rect {
                left: m - pad,
                top: sc(98) - pad,
                right: m + sc(300) + pad,
                bottom: sc(126) + pad,
            };
            l.search = Rect {
                left: m + sc(312),
                top: sc(98),
                right,
                bottom: sc(126),
            };
            l.kind_label = Rect {
                left: m,
                top: sc(136),
                right,
                bottom: sc(154),
            };
            l.kind_radios = vec![
                Rect {
                    left: m,
                    top: sc(156),
                    right: m + sc(120),
                    bottom: sc(184),
                },
                Rect {
                    left: m + sc(132),
                    top: sc(156),
                    right: m + sc(252),
                    bottom: sc(184),
                },
            ];
            l.progress = Rect {
                left: m,
                top: sc(190),
                right,
                bottom: sc(190) + sc(FIX_MATCH_PROGRESS_H_L),
            };
            l.list = Rect {
                left: m,
                top: sc(198),
                right,
                bottom: sc(398),
            };
            l.searching = l.list;
            l.empty = l.list;
            l.scope_label = Rect {
                left: m,
                top: sc(410),
                right,
                bottom: sc(428),
            };
            let mut top = sc(430);
            for _ in 0..radios.min(2) {
                l.radios.push(Rect {
                    left: m,
                    top,
                    right,
                    bottom: top + sc(22),
                });
                top += sc(24);
            }
            // A movie offers no scope radios: collapse the dead strip and
            // pull the status and buttons up, so no blank band remains.
            // A series keeps the full stack.
            let (note_top, status_top, bt, bb, client_h) = if radios == 0 {
                (sc(410), sc(410), sc(440), sc(476), sc(496))
            } else {
                (sc(480), sc(502), sc(532), sc(568), sc(588))
            };
            l.client_h = client_h;
            l.note = Rect {
                left: m,
                top: note_top,
                right,
                bottom: note_top + sc(18),
            };
            l.status = Rect {
                left: m,
                top: status_top,
                right,
                bottom: status_top + sc(20),
            };
            l.buttons = vec![
                (
                    FM_ID_USE_TEXT,
                    Rect {
                        left: m,
                        top: bt,
                        right: m + sc(260),
                        bottom: bb,
                    },
                ),
                (
                    FM_ID_MATCH,
                    Rect {
                        left: m + sc(280),
                        top: bt,
                        right: m + sc(380),
                        bottom: bb,
                    },
                ),
                (
                    FM_ID_CANCEL,
                    Rect {
                        left: m + sc(390),
                        top: bt,
                        right,
                        bottom: bb,
                    },
                ),
            ];
        }
        FixScreen::Matched => {
            l.client_h = sc(244);
            l.applied = Rect {
                left: m,
                top: sc(74),
                right,
                bottom: sc(94),
            };
            let (bt, bb) = (sc(112), sc(148));
            l.buttons = vec![
                (
                    FM_ID_REFRESH,
                    Rect {
                        left: m,
                        top: bt,
                        right: m + sc(230),
                        bottom: bb,
                    },
                ),
                (
                    FM_ID_CHANGE,
                    Rect {
                        left: m + sc(250),
                        top: bt,
                        right,
                        bottom: bb,
                    },
                ),
                (
                    FM_ID_AUTO,
                    Rect {
                        left: m,
                        top: sc(158),
                        right,
                        bottom: sc(194),
                    },
                ),
            ];
            l.status = Rect {
                left: m,
                top: sc(204),
                right,
                bottom: sc(224),
            };
        }
    }
    l
}

/// How long one provider pass may take: the same 90 s budget the CLIs hand
/// their scopes. The scope is built and used on the WORKER thread only.
pub const FIX_MATCH_DEADLINE: Duration = Duration::from_secs(90);

/// One job the dialog hands its worker thread.
enum FixJob {
    /// The typed name/year over the kind's legs. `only` retries exactly those
    /// providers (a cached partial's failed list); `cache_key` is `Some` when
    /// this pass may store its merged rows under that key.
    Search {
        name: String,
        /// The year the FIELD carried (ranking evidence and the cache key).
        year: i32,
        /// The year the ENGINE may gate on: the field's year only when the
        /// user typed it, else `0`, so a filename-derived year ranks but never
        /// hides a provider whose year differs.
        gate_year: i32,
        /// The playing file's shape/kind as a SOFT prior (`None` when unknown).
        prefer: Option<Kind>,
        /// The playing file's local evidence (folder show, sibling prefix,
        /// file counts): SOFT rank terms, never filters.
        evidence: crate::search::LocalEvidence,
        only: Vec<String>,
        cache_key: Option<String>,
    },
    /// Pin one picked candidate into the store. `path` is the playing file
    /// the correction is for: the dialog already saved D's provisional
    /// identity correction for it, and the worker's full record replaces that
    /// provisional through the same correction path.
    Latch {
        cand: Candidate,
        scope: Scope,
        key: String,
        path: String,
    },
    /// Re-fetch a stored latch's identity. Boxed: `Override` is the enum's
    /// largest variant by far, and an unboxed field makes every `FixJob`
    /// that size.
    Refresh { over: Box<Override>, path: String },
    /// Drop a stored latch (the `Use automatic matching` path).
    Unmatch { scope: Scope, key: String },
    /// The plugin-free latch: the typed title with no provider identity.
    UseText {
        title: String,
        year: i32,
        scope: Scope,
        key: String,
        path: String,
    },
}

/// What a worker published for the dialog to apply.
#[derive(Debug)]
enum FixOutcome {
    /// One streamed search batch: the rows to merge into the list (empty is
    /// never pushed).
    Search(Vec<Candidate>),
    /// The stream is over: the count is final, the list stays on what arrived.
    SearchDone,
    /// The latch is written and saved; the dialog closes with this status.
    Latched(String),
    /// The job failed: the dialog shows [`FIX_MATCH_ERROR`] and stays open.
    Failed,
    /// The full pin failed AFTER the dialog already saved the provisional
    /// correction: the latch stays (usable without the episode table, and the
    /// next poll already renders it through the preview path), so the dialog
    /// stays open on this pending copy instead of [`FIX_MATCH_ERROR`] -- whose
    /// `Nothing was saved` promise would lie -- and nothing is rolled back.
    ProvisionalKept(String),
}

/// The worker body: one bounded pass, then publish. Never touches the UI.
fn fix_match_worker(job: FixJob) -> FixOutcome {
    let cancel = metadata_http::cancel::Cancel::with_deadline(FIX_MATCH_DEADLINE);
    match job {
        FixJob::Search {
            name,
            year,
            gate_year,
            prefer,
            evidence,
            only,
            cache_key,
        } => {
            let prior: Vec<Candidate> = cache_key
                .as_deref()
                .and_then(cached_search)
                .map(|c| c.rows)
                .unwrap_or_default();
            // Universal search: both kinds, every leg `All` names, ONE list.
            // The file's shape is only a ranking prior from here on -- a movie
            // answer is still a row for a series file, never filtered out.
            let providers: Vec<String> = if only.is_empty() {
                crate::search::KindFilter::All
                    .providers()
                    .iter()
                    .map(|p| (*p).to_string())
                    .collect()
            } else {
                only.into_iter()
                    .filter(|p| {
                        crate::search::KindFilter::All
                            .providers()
                            .contains(&p.as_str())
                    })
                    .collect()
            };
            let mut rows = prior;
            let outcomes: std::sync::Mutex<Vec<metadata::SearchOutcome>> =
                std::sync::Mutex::new(Vec::new());
            std::thread::scope(|scope| {
                for kind in crate::search::KindFilter::All.kinds() {
                    let cancel = &cancel;
                    let name = &name;
                    let providers = &providers;
                    let outcomes = &outcomes;
                    scope.spawn(move || {
                        let out = metadata::search_stream_only_with_log(
                            cancel,
                            name,
                            gate_year,
                            *kind,
                            providers,
                            &|_| {},
                            &app_logf,
                        );
                        outcomes
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(out);
                    });
                }
            });
            let mut failed: Vec<String> = Vec::new();
            for out in outcomes
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
            {
                rows.extend(out.rows);
                for provider in out.failed {
                    if !failed.contains(&provider) {
                        failed.push(provider);
                    }
                }
            }
            failed.sort();
            // Same ranking the Card list paints, so the dialog's live rows and
            // a later cache replay agree on provider and order; the local
            // evidence is the same one the dialog opened with.
            let rows = crate::search::merge_candidates_evidenced(
                &name,
                year,
                prefer,
                Some(&evidence),
                vec![rows],
            );
            if let Some(key) = cache_key.as_deref()
                && !rows.is_empty()
            {
                store_search(key, &rows, &failed);
            }
            FixOutcome::Search(rows)
        }
        FixJob::Latch {
            cand,
            scope,
            key,
            path,
        } => {
            match metadata::pin_from_candidate(&cancel, &cand, scope, &key) {
                Ok(over) => match fix_match_put(&path, &over) {
                    Ok(()) => FixOutcome::Latched(fix_match_matched_status(
                        &over.title,
                        over.episodes.len(),
                    )),
                    Err(_) => fix_match_latch_failure(true, &over.title),
                },
                Err(_) => fix_match_latch_failure(true, &cand.title),
            }
        }
        FixJob::Refresh { over, path } => match metadata::refresh(&cancel, &over) {
            Ok(fresh) => match fix_match_put(&path, &fresh) {
                Ok(()) => FixOutcome::Latched(fix_match_refreshed_status(&fresh.title)),
                Err(_) => FixOutcome::Failed,
            },
            Err(_) => FixOutcome::Failed,
        },
        FixJob::UseText {
            title,
            year,
            scope,
            key,
            path,
        } => {
            let over = fix_match_text_override(&title, year, scope, &key);
            match fix_match_put(&path, &over) {
                Ok(()) => FixOutcome::Latched(fix_match_matched_status(&over.title, 0)),
                Err(_) => FixOutcome::Failed,
            }
        }
        FixJob::Unmatch { scope, key } => match metadata::overrides::load() {
            Ok(mut store) => match store.remove(scope, &key) {
                Ok(()) => FixOutcome::Latched(FIX_MATCH_AUTO_STATUS.to_string()),
                Err(_) => FixOutcome::Failed,
            },
            Err(_) => FixOutcome::Failed,
        },
    }
}

/// Load + correction-save in one call, so no caller can write a half latch:
/// a failed load carries on with the documented empty recovery store, and a
/// failed save is reported ([`FixOutcome::Failed`] or
/// [`FixOutcome::ProvisionalKept`]) instead of silently dropping the latch.
///
/// Every write goes through [`metadata::overrides::Store::put_correction`]
/// (never plain `put`): the full record replaces the provisional identity
/// the dialog saved before the network pin ran, and any conflicting latch for
/// the file is removed in the same atomic save.
fn fix_match_put(path: &str, over: &Override) -> Result<(), metadata::Error> {
    let mut store = metadata::overrides::load().map_err(|err| {
        app_logf(&format!(
            "overrides: load failed, refusing to clobber matches: {err}"
        ));
        err
    })?;
    store.put_correction(path, over.clone())
}

/// Build D's provisional identity override for the picked candidate and save
/// it as the correction BEFORE the network pin runs: the card becomes the
/// corrected series/movie on the next poll tick (through the preview path,
/// behind the B rate gate -- no second push mechanism) without waiting for
/// the provider identity and the whole episode table under the 90 s
/// deadline. The full pin later replaces this record through
/// [`fix_match_put`]. Pure D construction plus C's atomic correction save; no
/// network, no fetch.
///
/// A save error returns `Err` and the caller must not spawn: nothing was
/// applied, so there is no half-applied correction to reconcile.
fn fix_match_save_provisional(
    cand: &Candidate,
    scope: Scope,
    key: &str,
    path: &str,
) -> Result<Override, metadata::Error> {
    let over = crate::search::provisional_override(cand, scope, key.to_string());
    let mut store = metadata::overrides::load().map_err(|err| {
        app_logf(&format!(
            "overrides: load failed, refusing to clobber matches: {err}"
        ));
        err
    })?;
    store.put_correction(path, over.clone())?;
    Ok(over)
}

/// Test-only shared fixture: the canonical Fix-match series file path. The
/// `search` test lane derives its fixture path from here instead of carrying
/// a parallel hand-built literal, so the two lanes can never drift apart.
#[cfg(test)]
pub(crate) fn fix_series_path() -> String {
    format!("{}:{}", 'M', r"\2SERIES\Show\Season 1\s01e01.mkv")
}

/// The latch worker's failure outcome for one picked title: a failed full pin
/// after the dialog already saved the provisional correction keeps it (usable
/// without the episode table) and reports the pending copy; with no
/// provisional saved there is nothing to keep, so the old error stands.
fn fix_match_latch_failure(provisional_saved: bool, title: &str) -> FixOutcome {
    if provisional_saved {
        FixOutcome::ProvisionalKept(fix_match_episodes_pending_status(title))
    } else {
        FixOutcome::Failed
    }
}

/// The dialog's live state: owned by the stack frame running the modal loop,
/// reached from the wndproc through `GWLP_USERDATA`.
struct FixMatchState {
    owner: HWND,
    dlg: HWND,
    dpi: i32,
    raw_title: String,
    path: String,
    /// The text the name field was SEEDED with (the playing file's prefill, or
    /// the card's own query). A year that only came from this seed is the
    /// file's, not the user's, so it ranks but never gates.
    seed: String,
    /// The explicit Movie/TV choice. Seeded from the filename query at `open`;
    /// the kind radios are its only later writer, and it drives the provider
    /// search and the latch scope (owner direction 2026-10-29).
    is_movie: bool,
    /// The latch already resolving for `path`, if any: selects the matched
    /// screen (`None` = the search screen).
    matched: Option<Override>,
    /// The scope targets the search screen offers (the key rules above).
    choices: Vec<ScopeChoice>,
    /// The rows currently listed.
    candidates: Vec<Candidate>,
    /// The row the user explicitly clicked, keyed by provider + provider id
    /// (mirrors [`MediaPicker::picked`): a new batch that still carries the
    /// row keeps the highlight on it instead of reselecting index 0.
    picked: Option<String>,
    /// Every created control: `(id, hwnd)`.
    ctl: Vec<(i32, HWND)>,
    /// Fonts the dialog owns; freed on teardown, after the window is destroyed.
    fonts: Vec<Gdi::HFONT>,
    /// The dark canvas brush returned from `WM_CTLCOLORSTATIC`.
    bg_brush: Gdi::HBRUSH,
    /// The in-flight job's outcome slot; the generation rejects a stale post.
    slot: Arc<Mutex<Option<(u64, FixOutcome)>>>,
    generation: u64,
    /// True while a provider job runs (the action controls are disabled and the
    /// busy static reads `Searching…`).
    busy: bool,
    /// The busy panel's current line (`Searching…`, `Matching…`,
    /// `Refreshing…`, `Restoring automatic matching…`): set per job in
    /// [`Self::spawn_job`], painted by the owner-drawn empty panel.
    busy_label: String,
    /// The indeterminate strip's phase, advanced by `WM_TIMER` while busy.
    progress_phase: u32,
    screen: FixScreen,
    /// The status this run reports when it closes; `None` = cancelled.
    status: Option<String>,
    /// Whether a search has finished at least once on this screen: tells "not
    /// searched yet" from "nothing found" (the no-results copy).
    searched: bool,
    /// True while the status static carries the error copy (paint color).
    status_error: bool,
    layout: FixMatchLayout,
}

impl FixMatchState {
    /// One control's handle by id (null when not created).
    fn h(&self, id: i32) -> HWND {
        self.ctl
            .iter()
            .find(|(i, _)| *i == id)
            .map_or(std::ptr::null_mut(), |(_, h)| *h)
    }

    /// `SetWindowTextW` from a Rust string.
    fn set_text(&self, id: i32, text: &str) {
        let u = utf16_z(text);
        unsafe { crate::win32::SetWindowTextW(self.h(id), u.as_ptr()) };
    }

    /// The static's current text.
    fn text(&self, id: i32) -> String {
        let mut buf = [0u16; 512];
        let n = unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW(
                self.h(id),
                buf.as_mut_ptr(),
                buf.len() as i32,
            )
        };
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }

    /// The checked radio's index, or `None` when neither is checked.
    fn checked_scope_index(&self) -> Option<usize> {
        for (i, id) in [FM_ID_RADIO_A, FM_ID_RADIO_B].iter().enumerate() {
            let on = unsafe {
                crate::win32::SendMessageW(self.h(*id), BM_GETCHECK, 0, 0) as u32 == BST_CHECKED
            };
            if on {
                return Some(i);
            }
        }
        None
    }

    /// The listbox's selected candidate, if the selection names a real row.
    /// Headless (null listbox, unit tests) falls back to the explicitly
    /// picked row when it is still listed, else the first row: the live
    /// dialog always selects the first row on populate, so a headless
    /// state with candidates behaves as the dialog does after a search.
    fn selected_candidate(&self) -> Option<Candidate> {
        if self.h(FM_ID_LIST).is_null() {
            if let Some(picked) = self.picked.as_deref()
                && let Some(c) = self.candidates.iter().find(|c| fix_pick_key(c) == picked)
            {
                return Some(c.clone());
            }
            return self.candidates.first().cloned();
        }
        self.list_selection()
            .and_then(|i| self.candidates.get(i).cloned())
    }

    /// Record an explicit pick of the listbox's current selection, mirroring
    /// [`MediaPicker::pick_row`]: the next batch keeps this row when it is
    /// still listed instead of reselecting index 0.
    fn pick_selected_row(&mut self) {
        let key = self
            .list_selection()
            .and_then(|i| self.candidates.get(i))
            .map(fix_pick_key);
        if key.is_some() {
            self.picked = key;
        }
    }

    /// The effective movie flag: the user's explicit Movie/TV choice. The
    /// choice is seeded from the filename query when the dialog opens and from
    /// then on owns the search kind and the latch scope; the selected
    /// candidate no longer overrides it (owner direction 2026-10-29).
    fn effective_is_movie(&self) -> bool {
        self.is_movie
    }

    /// The effective scope table: recomputed from the user's kind choice, so a
    /// TV choice always offers Series/Season even for a movie-classified file
    /// and a Movie choice always offers the single movie target.
    fn effective_choices(&self) -> Vec<ScopeChoice> {
        fix_match_scope_choices(&self.path, self.effective_is_movie())
    }

    /// The checked radio against `choices`, falling back to the table default
    /// when headless (null radios, unit tests) or when the checked index left
    /// the table (a kind change invalidated the old radio). A surviving
    /// Series-vs-Season index is returned as-is, so a user pick within the
    /// candidate kind survives.
    fn checked_scope_index_or_default(&self, choices: &[ScopeChoice]) -> Option<usize> {
        if let Some(idx) = self.checked_scope_index()
            && choices.get(idx).is_some()
        {
            return Some(idx);
        }
        if self.h(FM_ID_RADIO_A).is_null() && self.h(FM_ID_RADIO_B).is_null() {
            return choices
                .iter()
                .position(|c| c.default)
                .or(choices.first().map(|_| 0));
        }
        choices.iter().position(|c| c.default)
    }

    /// Sync the stored flag/table to the selected candidate, overriding the
    /// filename classification. Returns true when the table changed (the
    /// caller refreshes the radios/layout, resetting an invalid radio to the
    /// candidate default: Series for a series pick, Movie for a movie pick).
    /// A same-kind pick leaves the table untouched, so a Series-vs-Season
    /// user pick survives. With no candidate selected the stored flag/table
    /// resets to the immutable filename baseline, so an empty search clears
    /// a stale cross-kind pick before `Use my text`/actions run.
    /// Recompute the scope table from the user's kind choice. The kind itself
    /// never changes here: the explicit Movie/TV control is the only writer of
    /// `is_movie` after `open`. Returns whether the table moved.
    fn sync_scope_to_selection(&mut self) -> bool {
        let choices = self.effective_choices();
        if self.choices == choices {
            return false;
        }
        self.choices = choices;
        true
    }

    /// The (scope, key) the current selection would write: the candidate kind
    /// decides. A selected movie candidate pins the movie file's own key (no
    /// radios); a selected series candidate uses the checked radio's target
    /// against the candidate-derived table (headless falls back to its
    /// default). With no candidate selected the filename fallback applies.
    /// `None` when neither exists (an odd path, or no radio).
    fn selected_target(&self) -> Option<(Scope, String)> {
        if self.effective_is_movie() {
            let key = metadata::overrides::movie_key(&self.path);
            return (!key.is_empty()).then_some((Scope::Movie, key));
        }
        let choices = self.effective_choices();
        let index = self.checked_scope_index_or_default(&choices)?;
        choices.get(index).map(|c| (c.scope, c.key.clone()))
    }
}

/// Stock-control class names (`CreateWindowExW` classes, not our own).
const CLASS_STATIC: &str = "STATIC";
const CLASS_EDIT: &str = "EDIT";
const CLASS_BUTTON: &str = "BUTTON";
const CLASS_LISTBOX: &str = "LISTBOX";

// Control styles the picker composes with (stable Win32 values).
const WS_POPUP_W: u32 = 0x8000_0000;
const WS_CAPTION_W: u32 = crate::win32::WS_CAPTION;
const WS_SYSMENU_W: u32 = crate::win32::WS_SYS_MENU;
const WS_GROUP_STYLE: u32 = 0x0002_0000;
const ES_AUTOHSCROLL: u32 = 0x0080;
const BS_DEFPUSHBUTTON: u32 = 0x0001;
const BS_AUTORADIOBUTTON: u32 = 0x0009;
const BM_GETCHECK: u32 = 0x00F0;
const BM_SETCHECK: u32 = 0x00F1;
const BST_CHECKED: u32 = 1;
const BST_UNCHECKED: u32 = 0;
const LBN_SELCHANGE: u32 = 1;
const LBN_DBLCLK: u32 = 2;
const WM_SETFONT: u32 = 0x0030;
const WM_NCCREATE: u32 = 0x0081;
const WM_CTLCOLORSTATIC: u32 = 0x0138;
/// `IDCANCEL`: the id the dialog manager sends for Esc (and the id a stock
/// dialog's Cancel button would carry; the picker's own id is `FM_ID_CANCEL`).
const IDCANCEL: i32 = 2;

/// Register the picker's window class once per process.
fn ensure_fix_match_class(hinst: windows_sys::Win32::Foundation::HINSTANCE) -> bool {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REGISTERED.get_or_init(|| {
        use windows_sys::Win32::UI::WindowsAndMessaging::{RegisterClassExW, WNDCLASSEXW};
        let class = utf16_z(FIX_MATCH_CLASS_NAME);
        let cursor = unsafe {
            crate::win32::LoadCursorW(
                std::ptr::null_mut(),
                crate::win32::IDC_ARROW as usize as *const u16,
            )
        };
        let mut wc: WNDCLASSEXW = unsafe { std::mem::zeroed() };
        wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(fix_match_wnd_proc);
        wc.hInstance = hinst;
        wc.hCursor = cursor;
        // The class brush erases the dialog canvas; kept for the process life
        // like the main window's own class brush.
        wc.hbrBackground =
            unsafe { crate::win32::CreateSolidBrush(themed(COL_BG)) } as usize as *mut core::ffi::c_void;
        wc.lpszClassName = class.as_ptr();
        let atom = unsafe { RegisterClassExW(&wc) };
        atom != 0
            || unsafe { windows_sys::Win32::Foundation::GetLastError() }
                == ERROR_CLASS_ALREADY_EXISTS
    })
}

/// `CreateWindowExW` one child control of the picker.
fn fix_match_add_control(
    parent: HWND,
    hinst: windows_sys::Win32::Foundation::HINSTANCE,
    class: &str,
    text: &str,
    style: u32,
    rc: Rect,
    id: i32,
) -> HWND {
    let class = utf16_z(class);
    let text = utf16_z(text);
    unsafe {
        crate::win32::CreateWindowExW(
            0,
            class.as_ptr(),
            text.as_ptr(),
            style,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            parent,
            id as usize as *mut core::ffi::c_void,
            hinst,
            std::ptr::null(),
        )
    }
}

const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;

impl FixMatchState {
    /// Create the modal popup, its controls and its fonts for `now`, owned by
    /// `owner`. Hands ownership of the state box to the dialog window (via
    /// `GWLP_USERDATA`; `WM_DESTROY` frees it) and returns the raw pointer for
    /// the run loop's loop-level work. Does not run the loop and does not
    /// touch the owner's enabled state -- [`fix_match_run`] owns both.
    fn open(owner: HWND, now: &app_support::config::NowPlaying) -> Result<*mut Self, String> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, WS_CAPTION, WS_POPUP, WS_SYSMENU,
        };
        let hinst = unsafe { crate::win32::GetModuleHandleW(std::ptr::null()) };
        if !ensure_fix_match_class(hinst) {
            return Err("fix match: RegisterClassExW failed".into());
        }
        let q = metadata_core::context::resolve_query(&now.raw_title, &now.path);
        let (seed_query, seed_from_card, seed_movie) = {
            let st = ui();
            // The dialog opens on the kind of the record that is actually
            // selected (else the shown card's kind, else the filename's), never
            // on a stale search choice. The card's query belongs to the file it
            // was seeded for: playing a DIFFERENT file (a switch, or a next
            // episode) re-seeds from that file instead of reusing the old one.
            let same_file = st.picker_path == now.path;
            let movie = st
                .picker
                .selected_candidate()
                .map(crate::search::matched_kind_is_movie)
                .unwrap_or_else(|| st.displayed_card_kind().is_movie());
            (st.picker.query.clone(), same_file, movie)
        };
        let is_movie = if !seed_from_card || seed_query.trim().is_empty() {
            // No selected record for THIS file: the folder decides the
            // movie-vs-series intent (a file beside its own episode siblings is
            // an episode), and only a folder with no episode siblings falls
            // back to the filename's own kind.
            match crate::search::detect_shape(&now.path) {
                crate::search::MediaShape::Series => false,
                crate::search::MediaShape::Movie => q.kind == Kind::MOVIE,
            }
        } else {
            seed_movie
        };
        let choices = fix_match_scope_choices(&now.path, is_movie);
        let matched = metadata::overrides::load()
            .ok()
            .and_then(|store| store.resolve_for(&now.path).cloned());
        let screen = if matched.is_some() {
            FixScreen::Matched
        } else {
            FixScreen::Search
        };
        let bg_brush = unsafe { crate::win32::CreateSolidBrush(themed(COL_BG)) };
        let mut state = Box::new(FixMatchState {
            owner,
            dlg: std::ptr::null_mut(),
            dpi: 96,
            raw_title: now.raw_title.clone(),
            path: now.path.clone(),
            seed: String::new(),
            is_movie,
            matched,
            choices,
            candidates: Vec::new(),
            picked: None,
            ctl: Vec::new(),
            fonts: Vec::new(),
            bg_brush: bg_brush as Gdi::HBRUSH,
            slot: Arc::new(Mutex::new(None)),
            generation: 0,
            busy: false,
            busy_label: FIX_MATCH_SEARCHING.to_string(),
            progress_phase: 0,
            screen,
            status: None,
            searched: false,
            status_error: false,
            layout: FixMatchLayout::default(),
        });
        let class = utf16_z(FIX_MATCH_CLASS_NAME);
        let title = utf16_z(FIX_MATCH_TITLE);
        let dlg = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP | WS_CAPTION | WS_SYSMENU,
                crate::win32::CW_USEDEFAULT,
                crate::win32::CW_USEDEFAULT,
                100,
                100,
                owner,
                std::ptr::null_mut(),
                hinst,
                (&mut *state) as *mut FixMatchState as *const core::ffi::c_void,
            )
        };
        if dlg.is_null() {
            // SAFETY: the brush was created above and never handed out.
            unsafe { Gdi::DeleteObject(state.bg_brush) };
            return Err("fix match: CreateWindowExW failed".into());
        }
        state.dlg = dlg;
        state.dpi = window_dpi(dlg).max(96);
        if let Err(message) = state.create_controls(hinst) {
            // Tear the half-built window down NOW: its wndproc's userdata
            // still points at `state`, which drops with this frame -- so clear
            // the slot first, or `WM_DESTROY` would free the box out from
            // under this return. The teardown in `WM_DESTROY` is skipped once
            // the slot is cleared, so free the GDI objects here or the brush
            // plus up to 3 fonts leak.
            // SAFETY: `dlg` is this thread's popup; the brush and every font
            // in `state.fonts` were created above and are freed exactly once
            // here (the wndproc frees nothing once userdata is zero).
            for f in &state.fonts {
                unsafe { Gdi::DeleteObject(*f) };
            }
            unsafe { Gdi::DeleteObject(state.bg_brush) };
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                    dlg,
                    windows_sys::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                    0,
                );
                crate::win32::DestroyWindow(dlg)
            };
            return Err(message);
        }
        // The name field starts from the resolver's own parse of the raw
        // title; the user edits from there. A query typed for ANOTHER file is
        // not this file's query, so a switch re-seeds.
        let seed = fix_match_seed_query(&seed_query, seed_from_card, &now.raw_title, &now.path);
        state.set_text(FM_ID_NAME, &seed);
        state.seed = seed.clone();
        state.apply_screen();
        // Same cache path as the card: a title the poll already prefetched
        // opens the dialog populated, with no network wait.
        if ui().cache_enabled {
            state.seed_from_cache(&seed);
        }
        // SAFETY: `dlg` is this thread's popup; the calls only affect it.
        unsafe {
            crate::win32::apply_dark_caption(dlg, themed(COL_BG), COL_TEXT);
            crate::win32::ShowWindow(dlg, crate::win32::SW_SHOW);
            crate::win32::UpdateWindow(dlg);
        }
        let focus = if screen == FixScreen::Search {
            state.h(FM_ID_NAME)
        } else {
            state.h(FM_ID_REFRESH)
        };
        // SAFETY: SetFocus only moves the thread's keyboard focus.
        fix_match_focus(focus);
        FIX_MATCH_HWND.store(dlg as usize, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::into_raw(state))
    }

    /// Create every control once (all at 1x1; [`Self::apply_screen`] owns
    /// geometry) and apply the dialog's fonts to each.
    fn create_controls(
        &mut self,
        hinst: windows_sys::Win32::Foundation::HINSTANCE,
    ) -> Result<(), String> {
        let sc = |v: i32| scale_px(self.dpi, v);
        let title_font = brand::display(sc(FS_TITLE));
        let body_font = brand::ui(sc(FS_BODY), brand::UI_WEIGHT);
        let hint_font = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
        for f in [title_font, body_font, hint_font] {
            if f.is_null() {
                // The half-created fonts never reach `self.fonts`, so free
                // them here: the `open` failure path only frees `self.fonts`
                // plus the brush, and must not leak this trio.
                // SAFETY: each handle was just created above and is non-null
                // when freed; null handles are never freed.
                for live in [title_font, body_font, hint_font] {
                    if !live.is_null() {
                        unsafe { Gdi::DeleteObject(live) };
                    }
                }
                return Err("fix match: CreateFontW failed".into());
            }
        }
        self.fonts = vec![title_font, body_font, hint_font];
        let base = crate::win32::WS_CHILD | crate::win32::WS_VISIBLE;
        let tab = crate::win32::WS_TAB_STOP;
        let zero = Rect {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        };
        let mut add = |id: i32, class: &str, style: u32| -> Result<(), String> {
            let h = fix_match_add_control(self.dlg, hinst, class, "", style, zero, id);
            if h.is_null() {
                return Err(format!("fix match: control {id} failed"));
            }
            self.ctl.push((id, h));
            let font = match id {
                FM_ID_TITLE => title_font,
                FM_ID_NAME | FM_ID_KIND_MOVIE | FM_ID_KIND_TV | FM_ID_LIST | FM_ID_SEARCH
                | FM_ID_MATCH | FM_ID_CANCEL | FM_ID_USE_TEXT | FM_ID_REFRESH | FM_ID_CHANGE
                | FM_ID_AUTO => {
                    body_font
                }
                _ => hint_font,
            };
            // SAFETY: `font` lives in `self.fonts` for the dialog's whole life.
            unsafe { crate::win32::SendMessageW(h, WM_SETFONT, font as usize, 1) };
            Ok(())
        };
        // Statics: the header rule, the labels, the status, the legacy busy
        // static (kept, hidden; the owner-drawn empty panel covers it), the
        // applied line, plus the owner-drawn empty panel and progress strip.
        for id in [
            FM_ID_TITLE,
            FM_ID_SUB,
            FM_ID_NAME_LABEL,
            FM_ID_KIND_LABEL,
            FM_ID_SCOPE_LABEL,
            FM_ID_NOTE,
            FM_ID_STATUS,
            FM_ID_SEARCHING,
            FM_ID_APPLIED,
        ] {
            add(id, CLASS_STATIC, base)?;
        }
        for id in [FM_ID_RULE, FM_ID_EMPTY, FM_ID_PROGRESS] {
            add(id, CLASS_STATIC, base | crate::win32::SS_OWNERDRAW)?;
        }
        // The field frame first (it sits behind the borderless edit).
        add(FM_ID_NAME_FRAME, CLASS_STATIC, base | crate::win32::SS_OWNERDRAW)?;
        // The one query edit, borderless: the owner-drawn frame above is the
        // border. It keeps `ES_AUTOHSCROLL`; the year is parsed from its text.
        add(FM_ID_NAME, CLASS_EDIT, base | tab | ES_AUTOHSCROLL)?;
        // The explicit Movie/TV choice: the first radio opens the group, so
        // the arrow keys move within it without leaving the dialog tab order.
        add(
            FM_ID_KIND_MOVIE,
            CLASS_BUTTON,
            base | tab | BS_AUTORADIOBUTTON | WS_GROUP_STYLE | crate::win32::BS_OWNERDRAW,
        )?;
        add(
            FM_ID_KIND_TV,
            CLASS_BUTTON,
            base | tab | BS_AUTORADIOBUTTON | crate::win32::BS_OWNERDRAW,
        )?;
        // The results list: fixed-height owner-drawn rows with strings kept,
        // so `LB_ADD_STRING` still stores the row text and `WM_DRAWITEM`
        // paints the two-line chip from the candidates table. No `WS_BORDER`:
        // the owner-drawn rows and the empty panel carry the surface.
        add(
            FM_ID_LIST,
            CLASS_LISTBOX,
            base | crate::win32::WS_V_SCROLL
                | crate::win32::LBS_NOTIFY
                | crate::win32::LBS_OWNERDRAWFIXED
                | crate::win32::LBS_HASSTRINGS
                | tab,
        )?;
        // Owner-drawn push buttons: every one paints through the main
        // window's button anatomy (`paint_button_face`) in `WM_DRAWITEM`.
        // Search is the secondary outline, Match the single accent primary,
        // Cancel subtle, Use-my-text quiet text; the matched screen's three
        // are secondaries. The scope radios are owner-drawn autoradios: the
        // system keeps the `BM_GETCHECK` state, `WM_DRAWITEM` paints the
        // circle, and clicks re-check explicitly (see `set_radio_checked`).
        let owner = crate::win32::BS_OWNERDRAW;
        add(FM_ID_SEARCH, CLASS_BUTTON, base | tab | owner)?;
        add(
            FM_ID_RADIO_A,
            CLASS_BUTTON,
            base | tab | BS_AUTORADIOBUTTON | owner | WS_GROUP_STYLE,
        )?;
        add(
            FM_ID_RADIO_B,
            CLASS_BUTTON,
            base | tab | BS_AUTORADIOBUTTON | owner,
        )?;
        add(FM_ID_USE_TEXT, CLASS_BUTTON, base | tab | owner)?;
        add(
            FM_ID_MATCH,
            CLASS_BUTTON,
            base | tab | owner | BS_DEFPUSHBUTTON,
        )?;
        add(FM_ID_CANCEL, CLASS_BUTTON, base | tab | owner)?;
        // The matched screen's three actions.
        add(FM_ID_REFRESH, CLASS_BUTTON, base | tab | owner)?;
        add(FM_ID_CHANGE, CLASS_BUTTON, base | tab | owner)?;
        add(FM_ID_AUTO, CLASS_BUTTON, base | tab | owner)?;
        Ok(())
    }

    /// Position every control, set every text and resize/center the dialog for
    /// the current screen. One function owns which control is where, so the
    /// two screens cannot drift from each other.
    fn apply_screen(&mut self) {
        use windows_sys::Win32::Foundation::RECT;
        if self.screen == FixScreen::Search {
            // The selected candidate overrides the filename classification, so
            // the radios/layout always reflect the candidate kind. A same-kind
            // pick is a no-op, preserving a Series-vs-Season user pick.
            self.sync_scope_to_selection();
        }
        self.layout = fix_match_layout(self.dpi, self.screen, self.choices.len());
        let l = self.layout.clone();
        let place = |id: i32, rc: Rect, show: bool| {
            let h = self.h(id);
            unsafe {
                crate::win32::MoveWindow(
                    h,
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    1,
                );
                crate::win32::ShowWindow(
                    h,
                    if show {
                        crate::win32::SW_SHOW
                    } else {
                        crate::win32::SW_HIDE
                    },
                );
            }
        };
        // The window's own outer size follows the screen's client size.
        let mut wrc = wr(Rect {
            left: 0,
            top: 0,
            right: l.client_w,
            bottom: l.client_h,
        });
        let style = WS_POPUP_W | WS_CAPTION_W | WS_SYSMENU_W;
        unsafe { crate::win32::AdjustWindowRectEx(&mut wrc, style, 0, 0) };
        let (ow, oh) = (wrc.right - wrc.left, wrc.bottom - wrc.top);
        let mut orc = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `orc` is a live RECT; the owner is this thread's window.
        let have_owner = unsafe { crate::win32::GetWindowRect(self.owner, &mut orc) } != 0;
        let (x, y) = if have_owner {
            (
                orc.left + (orc.right - orc.left - ow) / 2,
                orc.top + (orc.bottom - orc.top - oh) / 2,
            )
        } else {
            (crate::win32::CW_USEDEFAULT, crate::win32::CW_USEDEFAULT)
        };
        // SAFETY: MoveWindow only moves/resizes this dialog.
        unsafe { crate::win32::MoveWindow(self.dlg, x, y, ow, oh, 1) };
        match self.screen {
            FixScreen::Search => {
                place(FM_ID_TITLE, l.title, true);
                place(FM_ID_SUB, l.sub, true);
                place(FM_ID_RULE, l.rule, true);
                place(FM_ID_NAME_LABEL, l.name_label, true);
                place(FM_ID_NAME_FRAME, l.name_frame, true);
                place(FM_ID_NAME, l.name_edit, true);
                place(FM_ID_SEARCH, l.search, true);
                place(FM_ID_KIND_LABEL, l.kind_label, true);
                let kind_radios = [FM_ID_KIND_MOVIE, FM_ID_KIND_TV];
                for (i, id) in kind_radios.iter().enumerate() {
                    let rc = l.kind_radios.get(i).copied().unwrap_or(Rect {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    });
                    place(*id, rc, true);
                }
                place(FM_ID_PROGRESS, l.progress, self.busy);
                // The list shows only when it has rows and no job runs; the
                // owner-drawn empty panel covers the same area in every other
                // case so the box is never blank.
                let show_list = !self.busy && !self.candidates.is_empty();
                let show_empty = !show_list;
                place(FM_ID_EMPTY, l.empty, show_empty);
                place(FM_ID_LIST, l.list, show_list);
                // Legacy busy static: kept for the control table, hidden — the
                // empty panel carries the busy line now.
                place(FM_ID_SEARCHING, l.searching, false);
                place(FM_ID_SCOPE_LABEL, l.scope_label, !self.choices.is_empty());
                let radios = [FM_ID_RADIO_A, FM_ID_RADIO_B];
                for (i, id) in radios.iter().enumerate() {
                    let rc = l.radios.get(i).copied().unwrap_or(Rect {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    });
                    place(*id, rc, i < self.choices.len());
                }
                place(FM_ID_NOTE, l.note, self.note_text().is_some());
                let status = self.status_text_for_screen();
                place(FM_ID_STATUS, l.status, !status.is_empty());
                for (id, rc) in l.buttons.clone() {
                    // The quiet secondary appears with the no-results copy only;
                    // Match and Cancel are always available.
                    place(id, rc, id != FM_ID_USE_TEXT || self.no_results_shown());
                }
                // The matched screen's controls are hidden here.
                for id in [FM_ID_APPLIED, FM_ID_REFRESH, FM_ID_CHANGE, FM_ID_AUTO] {
                    place(
                        id,
                        Rect {
                            left: 0,
                            top: 0,
                            right: 1,
                            bottom: 1,
                        },
                        false,
                    );
                }
                self.set_text(FM_ID_TITLE, FIX_MATCH_TITLE);
                self.set_text(FM_ID_SUB, &fix_match_sub_line(&self.raw_title));
                self.set_text(FM_ID_NAME_LABEL, FIX_MATCH_NAME_LABEL);
                self.set_text(FM_ID_KIND_LABEL, FIX_MATCH_KIND_LABEL);
                self.set_text(FM_ID_KIND_MOVIE, FIX_MATCH_KIND_MOVIE);
                self.set_text(FM_ID_KIND_TV, FIX_MATCH_KIND_TV);
                self.reset_kind_state();
                // While busy the disabled Search button doubles as a loading
                // indicator; the empty panel and the strip carry it too.
                let search_label = if self.busy {
                    self.busy_label.clone()
                } else {
                    FIX_MATCH_SEARCH_LABEL.to_string()
                };
                self.set_text(FM_ID_SEARCH, &search_label);
                self.set_text(FM_ID_SCOPE_LABEL, FIX_MATCH_SCOPE_LABEL);
                self.set_text(FM_ID_SEARCHING, FIX_MATCH_SEARCHING);
                let radios = [FM_ID_RADIO_A, FM_ID_RADIO_B];
                for (i, id) in radios.iter().enumerate() {
                    let label = self
                        .choices
                        .get(i)
                        .map_or(String::new(), |c| c.label.clone());
                    self.set_text(*id, &label);
                }
                self.set_text(FM_ID_NOTE, self.note_text().as_deref().unwrap_or(""));
                self.set_text(FM_ID_STATUS, &self.status_text_for_screen());
                self.set_text(FM_ID_MATCH, FIX_MATCH_MATCH_LABEL);
                self.set_text(FM_ID_CANCEL, FIX_MATCH_CANCEL_LABEL);
                self.set_text(FM_ID_USE_TEXT, FIX_MATCH_USE_TEXT);
                self.reset_radio_state();
            }
            FixScreen::Matched => {
                place(FM_ID_TITLE, l.title, true);
                place(FM_ID_SUB, l.sub, true);
                place(FM_ID_RULE, l.rule, true);
                place(FM_ID_APPLIED, l.applied, true);
                for (id, rc) in l.buttons.clone() {
                    place(id, rc, true);
                }
                let status = self.status_text_for_screen();
                place(FM_ID_STATUS, l.status, !status.is_empty());
                // The search screen's controls are hidden here.
                for id in [
                    FM_ID_NAME_LABEL,
                    FM_ID_NAME_FRAME,
                    FM_ID_NAME,
                    FM_ID_SEARCH,
                    FM_ID_KIND_LABEL,
                    FM_ID_KIND_MOVIE,
                    FM_ID_KIND_TV,
                    FM_ID_LIST,
                    FM_ID_SEARCHING,
                    FM_ID_EMPTY,
                    FM_ID_PROGRESS,
                    FM_ID_SCOPE_LABEL,
                    FM_ID_RADIO_A,
                    FM_ID_RADIO_B,
                    FM_ID_NOTE,
                    FM_ID_MATCH,
                    FM_ID_CANCEL,
                    FM_ID_USE_TEXT,
                ] {
                    place(
                        id,
                        Rect {
                            left: 0,
                            top: 0,
                            right: 1,
                            bottom: 1,
                        },
                        false,
                    );
                }
                self.set_text(FM_ID_TITLE, FIX_MATCH_TITLE);
                let sub = self
                    .matched
                    .as_ref()
                    .map_or(String::new(), fix_match_matched_sub_line);
                let applied = self
                    .matched
                    .as_ref()
                    .map_or(String::new(), |o| fix_match_applied_text(o, &self.path));
                self.set_text(FM_ID_SUB, &sub);
                self.set_text(FM_ID_APPLIED, &applied);
                self.set_text(FM_ID_STATUS, &status);
                self.set_text(FM_ID_REFRESH, FIX_MATCH_REFRESH_LABEL);
                self.set_text(FM_ID_CHANGE, FIX_MATCH_CHANGE_LABEL);
                self.set_text(FM_ID_AUTO, FIX_MATCH_AUTO_LABEL);
            }
        }
        self.update_actions();
    }
}

/// An `LB_*` value the crate's `win32` mirror does not carry: `LB_SETCURSEL`
/// (`0x0186`), the selection the results list opens with so `Match` has a
/// target without a click.
const LB_SETCURSEL_W: u32 = 0x0186;
/// `WM_CTLCOLOREDIT` / `WM_CTLCOLORLISTBOX`: the edit/listbox dark-theme
/// queries, next to [`WM_CTLCOLORSTATIC`].
const WM_CTLCOLOREDIT_W: u32 = 0x0133;
const WM_CTLCOLORLISTBOX_W: u32 = 0x0134;

/// The modal run's outcome slot: `WM_DESTROY` moves the closing status here
/// before freeing the state box, and [`fix_match_run`] reads it after its
/// loop ends. The slot is per-dialog: it carries the destroyed dialog's own
/// `HWND` with the status, so a nested open can neither steal nor overwrite
/// the outer run's outcome. The singleton guard in [`open_fix_match`] means
/// only one picker runs at a time (it is modal), so one slot is exact.
static FIX_MATCH_RESULT: Mutex<(usize, Option<String>)> = Mutex::new((0, None));
/// The dialog's last query/kind: written back to the Card workspace's one
/// [`crate::search::MediaPicker`] when the dialog closes, so the two views
/// share the canonical search state instead of drifting apart.
static FIX_MATCH_LAST_QUERY: Mutex<(String, bool)> = Mutex::new((String::new(), false));

/// Move the keyboard focus; null-safe (the matched screen has no name field).
fn fix_match_focus(h: HWND) {
    if h.is_null() {
        return;
    }
    // SAFETY: SetFocus only moves the thread's keyboard focus.
    unsafe {
        windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(h);
    }
}

/// The dialog's button id for an owner-drawn `hwndItem`, if it names one of
/// this dialog's controls. Pure lookup over the created control table.
fn fix_match_button_id_for(state: &FixMatchState, hwnd_item: HWND) -> Option<i32> {
    state
        .ctl
        .iter()
        .find(|(_, h)| *h == hwnd_item)
        .map(|(id, _)| *id)
}

/// Paint one owner-drawn dialog push button through the main window's button
/// anatomy. `ODS_SELECTED` is pressed, `ODS_HOTLIGHT` hovered,
/// `ODS_DISABLED` disabled, `ODS_FOCUS` draws the focus ring.
fn paint_fix_match_button(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    font: Gdi::HFONT,
    id: i32,
    label: &str,
    item_state: u32,
) {
    let hovered = item_state & crate::win32::ODS_HOTLIGHT != 0;
    let pressed = item_state & crate::win32::ODS_SELECTED != 0;
    let disabled = item_state & crate::win32::ODS_DISABLED != 0;
    let focused = item_state & crate::win32::ODS_FOCUS != 0;
    let b = UiButton {
        id,
        label: label.to_string(),
        style: fix_match_button_style(id),
        rc,
    };
    paint_button_face(hdc, &b, dpi, font, hovered, pressed, disabled, disabled);
    if focused && !disabled {
        draw_focus_ring(hdc, rc, dpi);
    }
}

/// Paint one owner-drawn scope radio: a 16px circle (accent ring plus dot
/// when checked, `COL_BORDER` ring when unchecked), the label beside it, and
/// the focus ring. `checked` is read from the control itself (`BM_GETCHECK`):
/// an owner-drawn button carries no check bit in `itemState`.
#[allow(clippy::too_many_arguments)]
fn paint_fix_match_radio(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    font: Gdi::HFONT,
    label: &str,
    checked: bool,
    disabled: bool,
    focused: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    fill_rect_solid(hdc, rc, COL_BG);
    // A square circle is a rounded rect with half-diameter corners.
    let d = sc(16);
    let cy = (rc.top + rc.bottom) / 2;
    let top = cy - d / 2;
    let circle = Rect {
        left: rc.left + sc(SP_2),
        top,
        right: rc.left + sc(SP_2) + d,
        bottom: top + d,
    };
    let r = d / 2;
    let ring = if checked && !disabled {
        COL_ACCENT
    } else if checked {
        COL_MUTED_DIM
    } else {
        COL_BORDER
    };
    fill_round_rect(hdc, circle, ring, r);
    let inset = sc(2).max(1);
    fill_round_rect(
        hdc,
        Rect {
            left: circle.left + inset,
            top: circle.top + inset,
            right: circle.right - inset,
            bottom: circle.bottom - inset,
        },
        COL_BG,
        (r - inset).max(0),
    );
    if checked {
        let dot_d = d / 2;
        let cx = (circle.left + circle.right) / 2;
        let dot_cy = (circle.top + circle.bottom) / 2;
        let dot_r = dot_d / 2;
        fill_round_rect(
            hdc,
            Rect {
                left: cx - dot_r,
                top: dot_cy - dot_r,
                right: cx + dot_r,
                bottom: dot_cy + dot_r,
            },
            if disabled { COL_MUTED_DIM } else { COL_ACCENT },
            dot_r,
        );
    }
    let fmt = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;
    let tc = if disabled {
        COL_MUTED_DIM
    } else if checked {
        COL_TEXT
    } else {
        COL_MUTED
    };
    draw_ui_text(
        hdc,
        label,
        Rect {
            left: circle.right + sc(SP_2),
            top: rc.top,
            right: rc.right,
            bottom: rc.bottom,
        },
        fmt,
        font,
        tc,
    );
    if focused && !disabled {
        draw_focus_ring(hdc, rc, dpi);
    }
}

/// Paint one owner-drawn results row: a `COL_CARD` rounded chip, `COL_TEXT`
/// title/year on line one, muted provider/type/episode line two, and when
/// selected `COL_ROW_HOVER` plus the 3px `COL_ACCENT` left bar.
#[allow(clippy::too_many_arguments)]
fn paint_fix_match_row(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    body_font: Gdi::HFONT,
    hint_font: Gdi::HFONT,
    title: &str,
    sub: &str,
    selected: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    fill_rect_solid(hdc, rc, COL_BG);
    let pad = sc(SP_2);
    let chip = Rect {
        left: rc.left + pad,
        top: rc.top + sc(2),
        right: rc.right - pad,
        bottom: rc.bottom - sc(2),
    };
    if chip.right <= chip.left || chip.bottom <= chip.top {
        return;
    }
    fill_round_rect(
        hdc,
        chip,
        if selected { COL_ROW_HOVER } else { COL_CARD },
        sc(R_ROW),
    );
    if selected {
        fill_round_rect(
            hdc,
            Rect {
                left: chip.left,
                top: chip.top,
                right: chip.left + sc(SELECT_BAR_W),
                bottom: chip.bottom,
            },
            COL_ACCENT,
            sc(R_ROW),
        );
    }
    let text_left = chip.left + sc(SP_3);
    let text_right = chip.right - sc(SP_3);
    let mid = (chip.top + chip.bottom) / 2;
    let fmt = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_END_ELLIPSIS
        | crate::win32::DT_V_CENTER;
    draw_ui_text(
        hdc,
        title,
        Rect {
            left: text_left,
            top: chip.top + sc(2),
            right: text_right,
            bottom: mid,
        },
        fmt,
        body_font,
        COL_TEXT,
    );
    draw_ui_text(
        hdc,
        sub,
        Rect {
            left: text_left,
            top: mid,
            right: text_right,
            bottom: chip.bottom - sc(2),
        },
        fmt,
        hint_font,
        COL_MUTED,
    );
}

/// Paint the centered empty/busy panel: the busy line while a job runs, the
/// pre-search heading/hint before the first search, the no-results
/// heading/copy after an empty search. Never blank while shown.
#[allow(clippy::too_many_arguments)]
fn paint_fix_match_empty(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    title_font: Gdi::HFONT,
    body_font: Gdi::HFONT,
    title: &str,
    body: &str,
    is_error: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    fill_rect_solid(hdc, rc, COL_BG);
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    fill_round_rect(hdc, rc, COL_SURFACE, sc(R_PANEL));
    let center = crate::win32::DT_CENTER
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;
    let h = rc.bottom - rc.top;
    if !title.is_empty() && !body.is_empty() {
        let mid = rc.top + h / 2;
        draw_ui_text(
            hdc,
            title,
            Rect {
                left: rc.left + sc(SP_3),
                top: rc.top + sc(SP_2),
                right: rc.right - sc(SP_3),
                bottom: mid,
            },
            center,
            title_font,
            COL_TEXT,
        );
        draw_ui_text_wrapped_centered(
            hdc,
            body,
            Rect {
                left: rc.left + sc(SP_3),
                top: mid,
                right: rc.right - sc(SP_3),
                bottom: rc.bottom - sc(SP_2),
            },
            body_font,
            if is_error { COL_DANGER } else { COL_MUTED },
        );
    } else if !title.is_empty() {
        draw_ui_text(hdc, title, rc, center, title_font, COL_MUTED);
    } else if !body.is_empty() {
        draw_ui_text_wrapped_centered(
            hdc,
            body,
            Rect {
                left: rc.left + sc(SP_3),
                top: rc.top + sc(SP_2),
                right: rc.right - sc(SP_3),
                bottom: rc.bottom - sc(SP_2),
            },
            body_font,
            if is_error { COL_DANGER } else { COL_MUTED },
        );
    }
}

/// Paint the 4px indeterminate strip: a `TRACK` well with the brand-red chunk
/// at the busy phase. No second accent, no gradient.
fn paint_fix_match_progress(hdc: Gdi::HDC, rc: Rect, dpi: i32, phase: u32) {
    let sc = |v: i32| scale_px(dpi, v);
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    fill_round_rect(hdc, rc, brand::TRACK, sc(R_BAR));
    let w = rc.right - rc.left;
    let mut chunk = w / 4;
    let min = sc(24);
    if chunk < min {
        chunk = min.min(w);
    }
    if chunk <= 0 || w <= chunk {
        return;
    }
    const CYCLE: u32 = 20;
    let pos = f64::from(phase % CYCLE) / f64::from(CYCLE);
    let x = rc.left + (pos * f64::from(w - chunk)) as i32;
    fill_round_rect(
        hdc,
        Rect {
            left: x,
            top: rc.top,
            right: x + chunk,
            bottom: rc.bottom,
        },
        COL_ACCENT,
        sc(R_BAR),
    );
}

/// Paint one edit-field frame: a `COL_CARD` rounded field with the
/// `COL_BORDER` hairline, lit `COL_ACCENT` while its edit has focus. The
/// borderless edit sits 2px inside and keeps its dark background and caret.
fn paint_fix_match_field_frame(hdc: Gdi::HDC, rc: Rect, dpi: i32, focused: bool) {
    let sc = |v: i32| scale_px(dpi, v);
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    fill_round_rect(hdc, rc, COL_CARD, sc(R_CTRL));
    stroke_round_rect(
        hdc,
        rc,
        if focused { COL_ACCENT } else { COL_BORDER },
        sc(R_CTRL),
    );
}

/// One `WM_DRAWITEM` paint for the dialog's owner-drawn controls. Returns
/// whether the request was this dialog's (handled = 1, else `DefWindowProcW`).
/// Buttons paint through the main window's anatomy, the list paints 56px
/// two-line chips, and the statics paint the rule/empty/progress/fields.
fn fix_match_paint_item(
    state: &FixMatchState,
    dis: &windows_sys::Win32::UI::Controls::DRAWITEMSTRUCT,
) -> bool {
    use windows_sys::Win32::Graphics::Gdi as gdi;
    let rc = Rect {
        left: dis.rcItem.left,
        top: dis.rcItem.top,
        right: dis.rcItem.right,
        bottom: dis.rcItem.bottom,
    };
    let dpi = state.dpi.max(96);
    let sc = |v: i32| scale_px(dpi, v);
    if dis.CtlType == crate::win32::ODT_BUTTON {
        let id = dis.CtlID as i32;
        if !matches!(
            id,
            FM_ID_SEARCH
                | FM_ID_MATCH
                | FM_ID_CANCEL
                | FM_ID_USE_TEXT
                | FM_ID_REFRESH
                | FM_ID_CHANGE
                | FM_ID_AUTO
                | FM_ID_RADIO_A
                | FM_ID_RADIO_B
                | FM_ID_KIND_MOVIE
                | FM_ID_KIND_TV
        ) {
            return false;
        }
        if matches!(
            id,
            FM_ID_RADIO_A | FM_ID_RADIO_B | FM_ID_KIND_MOVIE | FM_ID_KIND_TV
        ) {
            // SAFETY: font created below and deleted after the paint.
            unsafe {
                let font = brand::ui(sc(FS_CONTROL), brand::UI_WEIGHT);
                if font.is_null() {
                    return true;
                }
                let label = state.text(id);
                let checked = match id {
                    FM_ID_KIND_MOVIE => state.is_movie,
                    FM_ID_KIND_TV => !state.is_movie,
                    _ => {
                        crate::win32::SendMessageW(dis.hwndItem, BM_GETCHECK, 0, 0) as u32
                            == BST_CHECKED
                    }
                };
                with_antialias(dis.hDC, || {
                    paint_fix_match_radio(
                        dis.hDC,
                        rc,
                        dpi,
                        font,
                        &label,
                        checked,
                        dis.itemState & crate::win32::ODS_DISABLED != 0,
                        dis.itemState & crate::win32::ODS_FOCUS != 0,
                    );
                });
                gdi::DeleteObject(font);
            }
            return true;
        }
        // SAFETY: font created below and deleted after the paint.
        unsafe {
            let font = brand::ui(sc(FS_CONTROL), brand::UI_WEIGHT);
            if font.is_null() {
                return true;
            }
            let label = state.text(id);
            with_antialias(dis.hDC, || {
                paint_fix_match_button(dis.hDC, rc, dpi, font, id, &label, dis.itemState);
            });
            gdi::DeleteObject(font);
        }
        return true;
    }
    if dis.CtlType == crate::win32::ODT_LISTBOX && dis.CtlID as i32 == FM_ID_LIST {
        let idx = dis.itemID as i32;
        // SAFETY: fonts created below and deleted after the paint.
        unsafe {
            let body = brand::ui(sc(FS_BODY), brand::UI_WEIGHT);
            let hint = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
            if body.is_null() || hint.is_null() {
                if !body.is_null() {
                    gdi::DeleteObject(body);
                }
                if !hint.is_null() {
                    gdi::DeleteObject(hint);
                }
                return true;
            }
            with_antialias(dis.hDC, || {
                if idx < 0 {
                    fill_rect_solid(dis.hDC, rc, COL_BG);
                } else if let Some(c) = state.candidates.get(idx as usize) {
                    let (title, sub) = fix_match_row_lines(c);
                    let selected = dis.itemState & crate::win32::ODS_SELECTED != 0;
                    paint_fix_match_row(dis.hDC, rc, dpi, body, hint, &title, &sub, selected);
                } else {
                    fill_rect_solid(dis.hDC, rc, COL_BG);
                }
            });
            gdi::DeleteObject(body);
            gdi::DeleteObject(hint);
        }
        return true;
    }
    if dis.CtlType == crate::win32::ODT_STATIC {
        let id = dis.CtlID as i32;
        if id == FM_ID_RULE {
            fill_rect_solid(dis.hDC, rc, COL_BORDER);
            return true;
        }
        if id == FM_ID_NAME_FRAME {
            let edit = FM_ID_NAME;
            // SAFETY: reads only which control of this thread has focus.
            let focused = unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus() == state.h(edit)
            };
            with_antialias(dis.hDC, || {
                paint_fix_match_field_frame(dis.hDC, rc, dpi, focused);
            });
            return true;
        }
        if id == FM_ID_PROGRESS {
            with_antialias(dis.hDC, || {
                paint_fix_match_progress(dis.hDC, rc, dpi, state.progress_phase);
            });
            return true;
        }
        if id == FM_ID_EMPTY {
            // SAFETY: fonts created below and deleted after the paint.
            unsafe {
                let title_font = brand::display(sc(FS_TITLE));
                // The body/hint reads at body size (13), not the 11px hint
                // size: the 200px panel has room and the hint is guidance.
                let body_font = brand::ui(sc(FS_BODY), brand::UI_WEIGHT);
                if title_font.is_null() || body_font.is_null() {
                    if !title_font.is_null() {
                        gdi::DeleteObject(title_font);
                    }
                    if !body_font.is_null() {
                        gdi::DeleteObject(body_font);
                    }
                    return true;
                }
                let (title, body) = state.empty_panel_text();
                let is_error = state.status_error && body == FIX_MATCH_ERROR;
                with_antialias(dis.hDC, || {
                    paint_fix_match_empty(
                        dis.hDC, rc, dpi, title_font, body_font, &title, &body, is_error,
                    );
                });
                gdi::DeleteObject(title_font);
                gdi::DeleteObject(body_font);
            }
            return true;
        }
    }
    false
}

/// The state pointer stashed in `GWLP_USERDATA`: null before `WM_NCCREATE`
/// installs it or after `WM_DESTROY` clears it.
fn fix_match_state_ptr(hwnd: HWND) -> *mut FixMatchState {
    // SAFETY: reads only the window's own userdata slot.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows_sys::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *mut FixMatchState
    }
}

impl FixMatchState {
    /// The name edit's trimmed text: the ONE query field (a title, or a title
    /// and year in the same field).
    fn find_text(&self) -> String {
        self.text(FM_ID_NAME).trim().to_string()
    }

    /// The one query field parsed into `(title, year)`; the provider search and
    /// the provider-free latch both consume this pair.
    fn parsed_query(&self) -> (String, i32) {
        parse_title_year(&self.find_text())
    }

    /// The provider kind the manual Movie/TV choice names. Kept for the latch
    /// scope's own vocabulary; the SEARCH is universal and no longer asks one
    /// kind, so nothing here reaches the provider pass.
    #[cfg(test)]
    fn search_want(&self) -> Kind {
        if self.is_movie {
            Kind::MOVIE
        } else {
            Kind::SERIES
        }
    }

    /// Apply the manual Movie/TV choice (owner direction 2026-10-29). The
    /// choice scopes the LATCH and picks the default radio; it does not scope
    /// the search, which is universal, so the rows on screen stay and the list
    /// is re-ranked with the new kind as its soft prior. Returns whether
    /// anything changed. Headless-safe: the radio check is skipped when the
    /// controls do not exist.
    fn apply_pick_kind(&mut self, is_movie: bool) -> bool {
        if self.is_movie == is_movie {
            return false;
        }
        self.is_movie = is_movie;
        self.choices = fix_match_scope_choices(&self.path, is_movie);
        // A Movie/TV switch re-latches, it never re-searches: the same rows are
        // re-ordered with the newly chosen kind as the soft prior.
        if !self.candidates.is_empty() {
            let (name, year) = self.parsed_query();
            let evidence = crate::search::LocalEvidence::from_path(&self.path);
            let rows = std::mem::take(&mut self.candidates);
            self.candidates = crate::search::merge_candidates_evidenced(
                &name,
                year,
                Some(self.latch_kind()),
                Some(&evidence),
                vec![rows],
            );
            self.repopulate_list();
        }
        if self.screen == FixScreen::Search {
            self.apply_screen();
        } else {
            self.update_actions();
        }
        true
    }

    /// The kind the Movie/TV radios choose, as the row kind the list prefers.
    fn latch_kind(&self) -> Kind {
        if self.is_movie {
            Kind::MOVIE
        } else {
            Kind::SERIES
        }
    }

    /// The one-target note: the single choice's note when the folder is both
    /// the season and the show, else `None` (two radios need no explanation).
    fn note_text(&self) -> Option<String> {
        self.choices.iter().find_map(|c| c.note.clone())
    }

    /// The status static's text for the current screen. The busy panel covers
    /// a running job, the empty panel carries the failure and the no-results
    /// copy prominently, and the no-target reason covers a playing file with
    /// no latch target (empty/unresolvable path or a drive-root whole-show
    /// refusal) so `Match` never dies silently; otherwise the transient
    /// close status (which only exists in the instant before the dialog closes
    /// with it).
    fn status_text_for_screen(&self) -> String {
        if self.busy {
            return String::new();
        }
        if self.screen == FixScreen::Search
            && let Some(reason) = fix_match_no_target_reason(&self.path, self.effective_is_movie())
        {
            return reason.to_string();
        }
        self.status.clone().unwrap_or_default()
    }

    /// Whether the no-results copy (and its `Use my text` secondary) shows.
    fn no_results_shown(&self) -> bool {
        !self.busy
            && !self.status_error
            && self.screen == FixScreen::Search
            && self.searched
            && self.candidates.is_empty()
    }

    /// Explicitly check one scope radio and clear the other after a click,
    /// then repaint both and re-arm the actions. Owner-drawn radios paint
    /// from `BM_GETCHECK`, so the paint and the `Match` target
    /// ([`Self::selected_target`]) cannot drift. [`Self::reset_radio_state`]
    /// stays the `apply_screen` default path.
    fn set_radio_checked(&self, id: i32) {
        for other in [FM_ID_RADIO_A, FM_ID_RADIO_B] {
            // SAFETY: check-set plus repaint of this dialog's own radios only.
            unsafe {
                crate::win32::SendMessageW(
                    self.h(other),
                    BM_SETCHECK,
                    if other == id {
                        BST_CHECKED as usize
                    } else {
                        BST_UNCHECKED as usize
                    },
                    0,
                );
                crate::win32::InvalidateRect(self.h(other), std::ptr::null(), 1);
            }
        }
        self.update_actions();
    }

    /// Check the default radio (the season when there are two), uncheck the
    /// other. Runs after every `apply_screen` on the search screen, so the
    /// default the scope table declares and the checked radio cannot drift.
    fn reset_radio_state(&self) {        for (i, id) in [FM_ID_RADIO_A, FM_ID_RADIO_B].iter().enumerate() {
            let on = self.choices.get(i).is_some_and(|c| c.default);
            // SAFETY: async check-set on this dialog's own radio.
            unsafe {
                crate::win32::SendMessageW(
                    self.h(*id),
                    BM_SETCHECK,
                    if on {
                        BST_CHECKED as usize
                    } else {
                        BST_UNCHECKED as usize
                    },
                    0,
                );
            }
        }
    }

    /// Check the explicit Movie/TV radio that matches `is_movie` and clear the
    /// other. Headless-safe: null controls are skipped.
    fn reset_kind_state(&self) {
        for (id, on) in [
            (FM_ID_KIND_MOVIE, self.is_movie),
            (FM_ID_KIND_TV, !self.is_movie),
        ] {
            let h = self.h(id);
            if h.is_null() {
                continue;
            }
            // SAFETY: check-set on this dialog's own radio only.
            unsafe {
                crate::win32::SendMessageW(
                    h,
                    BM_SETCHECK,
                    if on {
                        BST_CHECKED as usize
                    } else {
                        BST_UNCHECKED as usize
                    },
                    0,
                );
                crate::win32::InvalidateRect(h, std::ptr::null(), 1);
            }
        }
    }

    /// The busy line for one job kind: the empty panel's centered line while
    /// that job runs. Pure so the spawn path and its tests agree.
    fn busy_label_for_job(job: &FixJob) -> &'static str {
        match job {
            FixJob::Search { .. } => FIX_MATCH_SEARCHING,
            FixJob::Latch { .. } | FixJob::UseText { .. } => FIX_MATCH_MATCHING,
            FixJob::Refresh { .. } => FIX_MATCH_REFRESHING,
            FixJob::Unmatch { .. } => FIX_MATCH_RESTORING,
        }
    }

    /// Arm the indeterminate strip's timer. No-op on a null dialog (unit
    /// tests): `SetTimer` on null would arm a thread timer that outlives us.
    fn arm_progress_timer(&self) {
        if self.dlg.is_null() {
            return;
        }
        // SAFETY: registers a timer on this dialog; no callback.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SetTimer(
                self.dlg,
                FIX_MATCH_TIMER_ID,
                FIX_MATCH_TIMER_INTERVAL.as_millis() as u32,
                None,
            );
        }
    }

    /// Kill the strip's timer. Safe on a null or destroyed dialog.
    fn kill_progress_timer(&self) {
        if self.dlg.is_null() {
            return;
        }
        // SAFETY: stops the registered id on this dialog.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::KillTimer(self.dlg, FIX_MATCH_TIMER_ID);
        }
    }

    /// The listbox's selected candidate index, if the selection names a real
    /// row. `None` when nothing is selected or the index left the table.
    fn list_selection(&self) -> Option<usize> {
        // SAFETY: synchronous query of this dialog's own listbox.
        let idx = unsafe {
            crate::win32::SendMessageW(self.h(FM_ID_LIST), crate::win32::LB_GET_CUR_SEL, 0, 0)
        } as i32;
        (idx >= 0)
            .then(|| self.candidates.get(idx as usize).map(|_| idx as usize))
            .flatten()
    }

    /// The empty panel's two lines for the current state: the busy line while
    /// a job runs, the pre-search heading/hint before the first search, the
    /// no-results heading/copy after an empty search, and the error copy on
    /// failure. Never both empty while the panel shows (the list is hidden
    /// exactly then), so the box is never blank.
    fn empty_panel_text(&self) -> (String, String) {
        if self.busy {
            return (self.busy_label.clone(), String::new());
        }
        if !self.candidates.is_empty() {
            return (String::new(), String::new());
        }
        if self.status_error {
            return (String::new(), FIX_MATCH_ERROR.to_string());
        }
        if !self.searched {
            return (
                FIX_MATCH_EMPTY_TITLE.to_string(),
                FIX_MATCH_EMPTY_HINT.to_string(),
            );
        }
        (
            FIX_MATCH_NO_MATCHES_TITLE.to_string(),
            FIX_MATCH_NO_RESULTS.to_string(),
        )
    }

    /// Freeze the action controls while a provider job runs (`Cancel` stays
    /// live); the busy panel covers the list meanwhile. Arms the strip timer
    /// while busy, kills it when idle.
    fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        if busy {
            self.arm_progress_timer();
        } else {
            self.kill_progress_timer();
        }
        for id in [
            FM_ID_NAME,
            FM_ID_KIND_MOVIE,
            FM_ID_KIND_TV,
            FM_ID_SEARCH,
            FM_ID_LIST,
            FM_ID_RADIO_A,
            FM_ID_RADIO_B,
            FM_ID_USE_TEXT,
            FM_ID_MATCH,
            FM_ID_REFRESH,
            FM_ID_CHANGE,
            FM_ID_AUTO,
        ] {
            let h = self.h(id);
            if !h.is_null() {
                // SAFETY: enabling/disabling this dialog's own control only.
                unsafe {
                    crate::win32::EnableWindow(h, i32::from(!busy));
                }
            }
        }
        if !busy {
            self.update_actions();
        }
    }

    /// Re-arm Search/Match/Use-my-text from the current state: Search needs a
    /// non-empty name, Match a selected candidate plus a latch target, the
    /// provider-free secondary only while its no-results copy shows. `Cancel`
    /// stays live; everything stays disabled while busy. Keeps the radio
    /// choice and the `Match` target in sync by reading the checked radio
    /// (the same [`Self::selected_target`] the latch runs).
    fn update_actions(&self) {
        if self.screen != FixScreen::Search {
            return;
        }
        let enable = |id: i32, on: bool| {
            let h = self.h(id);
            if !h.is_null() {
                // SAFETY: enabling/disabling this dialog's own control only.
                unsafe {
                    crate::win32::EnableWindow(h, i32::from(on));
                }
            }
        };
        let search_on = search_query_allowed(&self.find_text(), self.busy);
        enable(FM_ID_SEARCH, search_on);
        enable(
            FM_ID_MATCH,
            fix_match_match_allowed(
                self.list_selection().is_some(),
                self.selected_target().is_some(),
                self.busy,
            ),
        );
        enable(
            FM_ID_USE_TEXT,
            fix_match_use_text_allowed(self.no_results_shown(), self.busy),
        );
    }

    /// Refill the results listbox from [`Self::candidates`], keeping the
    /// explicitly picked row when it is still listed and selecting the first
    /// row otherwise, so a new batch never moves the highlight off the row
    /// the user clicked.
    fn repopulate_list(&self) {
        let h = self.h(FM_ID_LIST);
        if h.is_null() {
            return;
        }
        // SAFETY: listbox messages on this dialog's own control; `u` outlives
        // the add call.
        unsafe {
            crate::win32::SendMessageW(h, crate::win32::LB_RESET_CONTENT, 0, 0);
            for c in &self.candidates {
                let u = utf16_z(&candidate_row_text(c));
                crate::win32::SendMessageW(h, crate::win32::LB_ADD_STRING, 0, u.as_ptr() as isize);
            }
            if !self.candidates.is_empty() {
                let sel = self
                    .picked
                    .as_deref()
                    .and_then(|key| {
                        self.candidates.iter().position(|c| fix_pick_key(c) == key)
                    })
                    .unwrap_or(0);
                crate::win32::SendMessageW(h, LB_SETCURSEL_W, sel, 0);
            }
        }
    }

    /// Hand `job` to a worker thread and mark the dialog busy: the worker
    /// never touches the UI, it publishes into [`Self::slot`] and posts
    /// [`WM_APP_FIX_MATCH_DONE`] for the dialog's own loop to apply.
    fn spawn_job(&mut self, job: FixJob) {
        self.busy_label = Self::busy_label_for_job(&job).to_string();
        self.progress_phase = 0;
        self.generation += 1;
        let job_gen = self.generation;
        *self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.status_error = false;
        self.set_busy(true);
        self.apply_screen();
        let slot = self.slot.clone();
        let dlg = self.dlg as usize;
        std::thread::spawn(move || {
            let out = fix_match_worker(job);
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((job_gen, out));
            // SAFETY: async post only; the worker never touches a control.
            unsafe {
                crate::win32::PostMessageW(dlg as HWND, WM_APP_FIX_MATCH_DONE, 0, 0);
            }
        });
    }

    /// `Search`: one bounded pass over the one query field (title, or title and
    /// year in the same field).
    ///
    /// Universal: both kinds are searched and both are KEPT, so a movie answer
    /// and a series answer meet in one closeness-ranked list. The file's own
    /// shape is a ranking prior only, and the Movie/TV radios scope the latch,
    /// never the search. The field's year ranks every row; it GATES the
    /// provider pass only when the user typed it, because a year the prefill
    /// took from the filename must not hide a provider whose release year
    /// differs (`Fallout` is 2023 on disk and 2024 at TVmaze).
    ///
    /// The persistent cache answers first, keyed on `All`: a complete entry
    /// fills the list with the cached status and no network; a partial entry
    /// fills it at once and retries only its failed providers, so the legs that
    /// already answered are never re-asked.
    fn start_search(&mut self) {
        if self.busy {
            return;
        }
        let (name, year) = self.parsed_query();
        if name.is_empty() && year <= 0 {
            return;
        }
        let evidence = crate::search::LocalEvidence::from_path(&self.path);
        let cache_on = ui().cache_enabled;
        let filter = crate::search::KindFilter::All;
        let key = crate::search::search_cache_key(&name, year, filter);
        let cache_key = cache_on.then(|| key.clone());
        let mut only: Vec<String> = Vec::new();
        if cache_on && let Some(cached) = cached_search(&key) {
            // An `All` entry already carries both kinds: every row is listed,
            // never narrowed to the radio's kind, and re-ranked with this
            // file's own shape and local evidence so the instant paint matches
            // the worker's.
            let rows = crate::search::merge_candidates_evidenced(
                &name,
                year,
                self.shape_prior(),
                Some(&evidence),
                vec![cached.rows],
            );
            if !rows.is_empty() {
                self.candidates = rows;
                self.searched = true;
                self.status_error = false;
                self.repopulate_list();
                self.status = Some(crate::search::match_count(self.candidates.len()));
                self.apply_screen();
                if cached.failed.is_empty() {
                    return;
                }
            }
            only = cached.failed;
        }
        self.spawn_job(FixJob::Search {
            name,
            year,
            gate_year: self.gate_year(),
            prefer: self.shape_prior(),
            evidence,
            only,
            cache_key,
        });
    }

    /// The year the ENGINE may gate on: the field's year only when the user
    /// produced it. The seeded text is the filename's own prefill, so its
    /// trailing `(Year)` is evidence to rank by, not an instruction to filter.
    fn gate_year(&self) -> i32 {
        let (_, year) = self.parsed_query();
        if year <= 0 {
            return 0;
        }
        let typed = self.find_text();
        let seeded = self.seed.trim();
        if seeded.is_empty() || !typed.eq_ignore_ascii_case(seeded) {
            year
        } else {
            0
        }
    }

    /// The playing file's own shape as the search's soft kind prior: a series
    /// file's list leads with shows, a movie file's with movies, and neither
    /// list ever drops the other kind.
    fn shape_prior(&self) -> Option<Kind> {
        if self.path.trim().is_empty() {
            return None;
        }
        Some(match crate::search::detect_shape(&self.path) {
            crate::search::MediaShape::Series => Kind::SERIES,
            crate::search::MediaShape::Movie => Kind::MOVIE,
        })
    }

    /// Fill the dialog from the persistent cache before any network: the same
    /// `All` key the card uses. A hit shows its rows and the cached status; a
    /// partial entry leaves Search to retry only the failed legs.
    fn seed_from_cache(&mut self, seeded: &str) {
        let (title, year) = parse_title_year(seeded);
        if title.is_empty() {
            return;
        }
        let filter = crate::search::KindFilter::All;
        let key = crate::search::search_cache_key(&title, year, filter);
        let Some(cached) = cached_search(&key) else {
            return;
        };
        let evidence = crate::search::LocalEvidence::from_path(&self.path);
        let rows: Vec<Candidate> = crate::search::merge_candidates_evidenced(
            &title,
            year,
            self.shape_prior(),
            Some(&evidence),
            vec![cached.rows],
        );
        if rows.is_empty() {
            return;
        }
        self.candidates = rows;
        self.searched = true;
        self.status_error = false;
        self.status = Some(crate::search::match_count(self.candidates.len()));
        self.repopulate_list();
        self.apply_screen();
    }

    /// `Match`: pin the selected candidate into the selected scope. A missing
    /// candidate or a missing scope target reports the visible reason instead
    /// of dying silently (the empty-path/no-radio case). The picked identity
    /// saves FIRST as the provisional correction (D built, C saved), so the
    /// card and the status already reflect the match while the network pin
    /// still fetches the episode table; a provisional-save error reports and
    /// never spawns, leaving no half-applied correction.
    fn start_latch(&mut self) {
        if self.busy {
            return;
        }
        // The candidate drives the scope: sync the stored flag/table first so
        // the provisional save, the full pin and the radios all see the
        // candidate kind, not the filename fallback.
        self.sync_scope_to_selection();
        let cand = self.selected_candidate();
        let (Some(cand), Some((scope, key))) = (cand, self.selected_target()) else {
            let reason = fix_match_no_target_reason(&self.path, self.effective_is_movie())
                .unwrap_or(FIX_MATCH_NO_TARGET_REASON);
            self.status = Some(reason.to_string());
            self.apply_screen();
            return;
        };
        let path = self.path.clone();
        match fix_match_save_provisional(&cand, scope, &key, &path) {
            Ok(_) => {
                // Non-blocking: the dialog stays busy for the full fetch, but
                // the correction is already on disk -- the card refreshes on
                // the next poll tick and the status already says matched.
                ui().set_status_text(FIX_MATCH_FETCHING_STATUS);
                self.status = Some(FIX_MATCH_FETCHING_STATUS.to_string());
                self.spawn_job(FixJob::Latch {
                    cand,
                    scope,
                    key,
                    path,
                });
            }
            Err(err) => {
                app_logf(&format!("fix match: provisional save failed: {err}"));
                self.status_error = true;
                self.apply_screen();
            }
        }
    }

    /// `Refresh metadata`: re-fetch the stored latch's own identity.
    fn start_refresh(&mut self) {
        if self.busy {
            return;
        }
        let Some(over) = self.matched.clone() else {
            return;
        };
        let path = self.path.clone();
        self.spawn_job(FixJob::Refresh {
            over: Box::new(over),
            path,
        });
    }

    /// `Use my text as the title`: the plugin-free latch (typed title only).
    /// A missing scope target reports the visible reason instead of dying
    /// silently, mirroring [`Self::start_latch`].
    fn start_use_text(&mut self) {
        if self.busy {
            return;
        }
        let (title, year) = self.parsed_query();
        if title.is_empty() {
            return;
        }
        self.sync_scope_to_selection();
        let Some((scope, key)) = self.selected_target() else {
            let reason = fix_match_no_target_reason(&self.path, self.effective_is_movie())
                .unwrap_or(FIX_MATCH_NO_TARGET_REASON);
            self.status = Some(reason.to_string());
            self.apply_screen();
            return;
        };
        let path = self.path.clone();
        self.spawn_job(FixJob::UseText {
            title,
            year,
            scope,
            key,
            path,
        });
    }

    /// `Use automatic matching`: drop the stored latch on a worker, then close
    /// with the auto status. A failure keeps the dialog open on the error copy.
    fn start_auto(&mut self) {
        if self.busy {
            return;
        }
        let Some(over) = self.matched.clone() else {
            return;
        };
        self.spawn_job(FixJob::Unmatch {
            scope: over.scope,
            key: over.key,
        });
    }

    /// Apply the worker's published outcome (the `WM_APP_FIX_MATCH_DONE` arm):
    /// a finished search repopulates the list, a saved latch closes with its
    /// status, a failure keeps the dialog open on the error copy, and a
    /// post-provisional latch failure keeps the dialog open on the pending
    /// copy with the provisional latch untouched (never rolled back).
    fn on_job_done(&mut self) {
        let got = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some((job_gen, out)) = got else {
            return;
        };
        if job_gen != self.generation {
            return;
        }
        self.set_busy(false);
        match out {
            FixOutcome::Search(rows) => {
                self.searched = true;
                self.candidates = rows;
                // The picked row belongs to the listed set: a batch that no
                // longer carries it drops the pick (the next list's
                // auto-highlight is not a choice), one that does keeps it.
                if let Some(picked) = self.picked.as_deref()
                    && !self.candidates.iter().any(|c| fix_pick_key(c) == picked)
                {
                    self.picked = None;
                }
                self.status_error = false;
                self.repopulate_list();
                self.apply_screen();
            }
            FixOutcome::SearchDone => {
                // The dialog's own Search is one shot (it publishes
                // `Search`), so a stream end never reaches this screen.
            }
            FixOutcome::Latched(status) => {
                self.status = Some(status);
                // SAFETY: destroys this dialog; returns without touching `self`
                // (the wndproc's `WM_DESTROY` arm frees the box).
                unsafe {
                    crate::win32::DestroyWindow(self.dlg);
                }
            }
            FixOutcome::Failed => {
                self.status_error = true;
                self.apply_screen();
            }
            FixOutcome::ProvisionalKept(status) => {
                // The full table never arrived, but the correction did: the
                // card already shows it (next poll rendered the provisional),
                // so the dialog reports the pending copy -- informational, not
                // the red error -- and stays open. The store is not touched.
                self.status_error = false;
                self.status = Some(status);
                self.apply_screen();
            }
        }
    }

    /// `Change match…`: back to the search screen, keeping the typed text.
    fn on_change(&mut self) {
        if self.busy {
            return;
        }
        self.screen = FixScreen::Search;
        self.apply_screen();
        fix_match_focus(self.h(FM_ID_NAME));
    }
}

/// The picker's window procedure: stock-control routing only. `WM_CLOSE` is
/// cancel (no status), `WM_COMMAND` routes the `FM_ID_*` space, a finished job
/// applies on `WM_APP_FIX_MATCH_DONE`, and `WM_DESTROY` frees the state box
/// after moving its closing status to the run slot.
///
/// INVARIANT: `WM_DESTROY` never posts a thread quit (`PostQuitMessage`). The
/// modal loop in [`fix_match_run`] exits on the dialog ceasing to exist, so a
/// quit posted here would outlive the dialog and be consumed by the app's own
/// main loop (`tray::message_loop` in `run_ui`), ending the whole process.
/// The app's Exit path (`tray::on_exit`) never needs such a quit.
///
/// # Safety
/// Called by the OS as the dialog's window procedure for `FIX_MATCH_CLASS`;
/// `hwnd` must be a dialog window whose `GWLP_USERDATA` holds (between
/// `WM_NCCREATE` and `WM_DESTROY`) a pointer to a live `FixMatchState` box.
pub unsafe extern "system" fn fix_match_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    // SAFETY: every arm upholds the wndproc contract; the state pointer is
    // only dereferenced while the dialog owns it (between `WM_NCCREATE` and
    // `WM_DESTROY`).
    unsafe {
        if msg == WM_NCCREATE {
            #[allow(clippy::cast_ptr_alignment)]
            let cs = lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            let ptr = if cs.is_null() {
                0
            } else {
                (*cs).lpCreateParams as isize
            };
            windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                hwnd,
                windows_sys::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                ptr,
            );
            return 1;
        }
        if msg == crate::win32::WM_CLOSE {
            crate::win32::DestroyWindow(hwnd);
            return 0;
        }
        if msg == crate::win32::WM_DESTROY {
            let ptr = fix_match_state_ptr(hwnd);
            // No thread quit here, ever (see the invariant on
            // [`fix_match_wnd_proc`]): the modal loop exits when this dialog
            // ceases to exist, so every close route -- Cancel, the titlebar
            // X, Esc, Match, Refresh, Use-automatic -- ends at the
            // `IsWindow(dlg) == 0` check with nothing left queued. A nested
            // or stale destroy (the `create_controls` failure path zeroes
            // userdata first, or a second open racing the first) must not
            // clear the outer dialog's handle.
            let is_tracked =
                hwnd as usize == FIX_MATCH_HWND.load(std::sync::atomic::Ordering::SeqCst);
            if !ptr.is_null() {
                // Move the closing status out BEFORE freeing the box: the
                // modal run reads it after its loop ends.
                let state = Box::from_raw(ptr);
                // The dialog's edited query/kind is the same canonical search
                // state the Card workspace owns: write it back before the box
                // is freed.
                *FIX_MATCH_LAST_QUERY
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    (state.find_text(), state.is_movie);
                // Stop the strip timer while the dialog still exists; the
                // no-quit invariant below still holds.
                state.kill_progress_timer();
                if is_tracked {
                    *FIX_MATCH_RESULT
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        (hwnd as usize, state.status.clone());
                }
                for f in &state.fonts {
                    Gdi::DeleteObject(*f);
                }
                Gdi::DeleteObject(state.bg_brush);
                drop(state);
                if is_tracked {
                    FIX_MATCH_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
                }
                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                    hwnd,
                    windows_sys::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                    0,
                );
            } else if is_tracked {
                FIX_MATCH_HWND.store(0, std::sync::atomic::Ordering::SeqCst);
            }
            // Deliberately no `PostQuitMessage`: see the invariant above.
            return 0;
        }
        if msg == WM_TIMER {
            let ptr = fix_match_state_ptr(hwnd);
            if !ptr.is_null() && wparam == FIX_MATCH_TIMER_ID && (*ptr).busy {
                (*ptr).progress_phase = (*ptr).progress_phase.wrapping_add(1);
                // Invalidates this dialog's own controls only.
                crate::win32::InvalidateRect((*ptr).h(FM_ID_PROGRESS), std::ptr::null(), 1);
                crate::win32::InvalidateRect((*ptr).h(FM_ID_EMPTY), std::ptr::null(), 1);
            }
            return 0;
        }
        if msg == crate::win32::WM_MEASUREITEM {
            let ptr = fix_match_state_ptr(hwnd);
            if !ptr.is_null() && lparam != 0 {
                #[allow(clippy::cast_ptr_alignment)]
                let mis =
                    &mut *(lparam as *mut windows_sys::Win32::UI::Controls::MEASUREITEMSTRUCT);
                if mis.CtlType == crate::win32::ODT_LISTBOX && mis.CtlID as i32 == FM_ID_LIST {
                    mis.itemHeight = scale_px((*ptr).dpi, FIX_MATCH_ROW_H_L) as u32;
                    return 1;
                }
            }
            return crate::win32::DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == crate::win32::WM_DRAWITEM {
            let ptr = fix_match_state_ptr(hwnd);
            if !ptr.is_null() && lparam != 0 {
                #[allow(clippy::cast_ptr_alignment)]
                let dis = &*(lparam as *const windows_sys::Win32::UI::Controls::DRAWITEMSTRUCT);
                if fix_match_paint_item(&*ptr, dis) {
                    return 1;
                }
            }
            return crate::win32::DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        if msg == WM_CTLCOLORSTATIC || msg == WM_CTLCOLOREDIT_W || msg == WM_CTLCOLORLISTBOX_W {
            let ptr = fix_match_state_ptr(hwnd);
            if ptr.is_null() {
                return crate::win32::DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let hdc = wparam as Gdi::HDC;
            let color = if msg == WM_CTLCOLORSTATIC {
                let ctl_hwnd = lparam as HWND;
                let id = fix_match_button_id_for(&*ptr, ctl_hwnd).unwrap_or(0);
                fix_match_static_color(id, (*ptr).status_error)
            } else {
                COL_TEXT
            };
            windows_sys::Win32::Graphics::Gdi::SetTextColor(hdc, themed(color));
            windows_sys::Win32::Graphics::Gdi::SetBkColor(hdc, themed(COL_BG));
            return (*ptr).bg_brush as isize;
        }
        if msg == WM_APP_FIX_MATCH_DONE {
            let ptr = fix_match_state_ptr(hwnd);
            if !ptr.is_null() {
                (*ptr).on_job_done();
            }
            return 0;
        }
        if msg == crate::win32::WM_COMMAND {
            let ptr = fix_match_state_ptr(hwnd);
            if ptr.is_null() {
                return crate::win32::DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let state = &mut *ptr;
            let id = (wparam & 0xFFFF) as i32;
            let code = ((wparam >> 16) & 0xFFFF) as u32;
            match id {
                FM_ID_SEARCH if code == crate::win32::BN_CLICKED => state.start_search(),
                FM_ID_MATCH if code == crate::win32::BN_CLICKED => state.start_latch(),
                FM_ID_CANCEL | IDCANCEL => {
                    crate::win32::DestroyWindow(hwnd);
                }
                FM_ID_USE_TEXT if code == crate::win32::BN_CLICKED => state.start_use_text(),
                FM_ID_REFRESH if code == crate::win32::BN_CLICKED => state.start_refresh(),
                FM_ID_CHANGE if code == crate::win32::BN_CLICKED => state.on_change(),
                FM_ID_AUTO if code == crate::win32::BN_CLICKED => state.start_auto(),
                FM_ID_LIST if code == LBN_DBLCLK => state.start_latch(),
                FM_ID_LIST if code == LBN_SELCHANGE => {
                    // A new pick can flip the kind (movie <-> series): refresh
                    // the radios/layout on a change, otherwise keep the
                    // Series-vs-Season user pick untouched. The pick itself is
                    // recorded so the next batch keeps this row.
                    state.pick_selected_row();
                    if state.sync_scope_to_selection() {
                        state.apply_screen();
                    } else {
                        state.update_actions();
                    }
                }
                FM_ID_RADIO_A | FM_ID_RADIO_B if code == crate::win32::BN_CLICKED => {
                    state.set_radio_checked(id);
                }
                FM_ID_KIND_MOVIE if code == crate::win32::BN_CLICKED => {
                    state.apply_pick_kind(true);
                }
                FM_ID_KIND_TV if code == crate::win32::BN_CLICKED => {
                    state.apply_pick_kind(false);
                }
                FM_ID_NAME if code == crate::win32::EN_CHANGE => {
                    state.update_actions();
                }
                FM_ID_NAME
                    if code == crate::win32::EN_SETFOCUS || code == crate::win32::EN_KILLFOCUS =>
                {
                    // The field frame lights on focus: repaint this dialog's
                    // own frame static only.
                    crate::win32::InvalidateRect(state.h(FM_ID_NAME_FRAME), std::ptr::null(), 1);
                }
                _ => {}
            }
            return 0;
        }
        crate::win32::DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// One modal-loop step for the picker: pure so tests pin the quit contract
/// without a window.
///
/// The loop exits on its own condition only -- its dialog window ceasing to
/// exist (`dialog_exists == false`: every close route funnels through
/// `DestroyWindow`, so Cancel, the titlebar X, Esc, Match, Refresh metadata,
/// Use-automatic and Change-match all end here) -- and that exit posts NO
/// thread quit (`repost_quit == false`), so the app's main loop
/// (`tray::message_loop`) never sees a quit from the dialog. A thread quit
/// already sitting in the queue (`got_quit`, i.e. `GetMessageW` returning 0)
/// is never ours: the loop still exits, but with `repost_quit == true` so the
/// caller reposts it and the app's own Exit path keeps working. A `GetMessageW`
/// error (`got_error`) exits quietly. Otherwise the loop keeps pumping.
///
/// Returns `(should_exit, repost_quit)`.
#[must_use]
pub fn fix_match_pump_step(dialog_exists: bool, got_quit: bool, got_error: bool) -> (bool, bool) {
    if got_quit {
        return (true, true);
    }
    if got_error {
        return (true, false);
    }
    if !dialog_exists {
        return (true, false);
    }
    (false, false)
}

/// Run the picker modal for `now`: disable `owner`, pump this thread's queue
/// until the dialog closes, then re-enable `owner`, restore its focus, and
/// report the closing status (`None` = cancelled). The owner's enabled state
/// is per-dialog: only a run that disabled the owner re-enables it, so a
/// nested open can never re-enable the owner under the outer picker. The loop
/// posts no thread quit of its own -- it ends when the dialog window is gone
/// (see [`fix_match_pump_step`]) and reposts a quit it did not create, so the
/// app's Exit path is untouched.
fn fix_match_run(owner: HWND, now: &app_support::config::NowPlaying) -> Option<String> {
    *FIX_MATCH_RESULT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = (0, None);
    let ptr = match FixMatchState::open(owner, now) {
        Ok(ptr) => ptr,
        Err(message) => {
            app_logf(&message);
            return None;
        }
    };
    // SAFETY: `ptr` is the live dialog state until `WM_DESTROY` frees it.
    let dlg = unsafe { (*ptr).dlg };
    if dlg.is_null() {
        return None;
    }
    // Per-dialog owner state: remember whether THIS run disabled the owner.
    // SAFETY: `IsWindowEnabled` only reads the window's style.
    let owner_was_enabled = if !owner.is_null() {
        let enabled =
            unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(owner) != 0 };
        if enabled {
            // SAFETY: enabling/disabling the owner window only.
            unsafe {
                crate::win32::EnableWindow(owner, 0);
            }
        }
        enabled
    } else {
        false
    };
    // SAFETY: the standard modal loop on this thread; `IsDialogMessageW`
    // gives the stock controls their Tab travel. Esc cancels at this level
    // (an EDIT with focus would otherwise swallow it). Enter starts Search
    // from the name/year edits and Match from the results list, before the
    // default-button path runs. The loop stops pumping
    // once its own dialog is gone ([`fix_match_pump_step`]), so it never
    // dispatches into a destroyed window -- and it never posts a thread quit
    // of its own, so the app's main loop cannot mistake a dialog close for an
    // exit request.
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, IsDialogMessageW, IsWindow, MSG, TranslateMessage,
        };
        let mut msg: MSG = std::mem::zeroed();
        loop {
            if fix_match_pump_step(IsWindow(dlg) != 0, false, false).0 {
                break;
            }
            let r = crate::win32::GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            let (exit, repost) = fix_match_pump_step(true, r == 0, r == -1);
            if exit {
                if repost {
                    // Not ours: hand the app's own quit back to the queue so
                    // `tray::message_loop` still sees it.
                    crate::win32::PostQuitMessage(msg.wParam as i32);
                }
                break;
            }
            if fix_match_pump_step(IsWindow(dlg) != 0, false, false).0 {
                break;
            }
            if msg.message == WM_KEYDOWN && msg.wParam == VK_ESCAPE as usize {
                crate::win32::DestroyWindow(dlg);
                continue;
            }
            // Enter in the name/year edits starts Search; Enter on the results
            // list starts Match. Checked here because the edits swallow
            // `VK_RETURN` before the parent wndproc ever sees it.
            if msg.message == WM_KEYDOWN && msg.wParam == VK_RETURN as usize {
                let focus = windows_sys::Win32::UI::Input::KeyboardAndMouse::GetFocus();
                let state = &mut *ptr;
                if focus == state.h(FM_ID_NAME) {
                    state.start_search();
                    continue;
                }
                if focus == state.h(FM_ID_LIST) {
                    state.start_latch();
                    continue;
                }
            }
            if IsDialogMessageW(dlg, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !owner.is_null() && owner_was_enabled {
            crate::win32::EnableWindow(owner, 1);
            windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(owner);
        }
    }
    let (dlg_done, status) = FIX_MATCH_RESULT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if dlg_done == dlg as usize {
        status
    } else {
        None
    }
}

/// The public entry: the manual latch picker for whatever is playing now.
/// Singleton-safe: a second open while the modal dialog lives is refused with
/// [`FIX_MATCH_ALREADY_OPEN_REASON`] instead of stacking a nested message
/// loop (the live tray menu pumps every thread message, so it could otherwise
/// re-enter here). Seeded from [`app_support::config::live_now_playing`];
/// with nothing playing it reports the idle reason on the main status line
/// and opens nothing. The closing status (match, refresh, unmatch) lands on
/// the main status line; the card itself refreshes on the next poll tick --
/// the UI never forces a push.
pub fn open_fix_match() {
    if !fix_match_dialog_hwnd().is_null() {
        ui().set_status_text(FIX_MATCH_ALREADY_OPEN_REASON);
        return;
    }
    let Some(now) = app_support::config::live_now_playing() else {
        ui().set_status_text(FIX_MATCH_IDLE_REASON);
        return;
    };
    let owner = ui().main_hwnd as HWND;
    let status = fix_match_run(owner, &now);
    {
        let (query, _movie) = FIX_MATCH_LAST_QUERY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut st = ui();
        if !query.trim().is_empty() {
            st.picker.set_query(query);
        }
        // The dialog's own kind shaped the override IT saved; it must not
        // rewrite the main search's explicit scope (that write path is the
        // Advanced filter, and a dialog close is not a search edit).
    }
    if let Some(status) = status {
        ui().set_status_text(status);
    }
}
/// `MessageBoxW`, `GetOpen/SaveFileNameW`) or shells out (`ShellExecuteW`) /
/// shows a window (`ShowWindow`). Each pumps the message queue, so the OS
/// re-enters `main_wnd_proc` on this same UI thread; its handlers
/// (`paint_main_window`, ...) re-lock `UI` via `ui()`. The wndproc therefore
/// returns one of these out of the `ui()` scope and runs it with the guard
/// RELEASED (`run_blocking_ui`): under the non-reentrant `UI` guard it
/// self-deadlocks the UI thread, the shipped "(Not Responding)" hang.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockingUi {
    AddDir,
    ClearMetadata,
    ClearIndex,
    ExportBundle,
    ImportBundle,
    OpenLogFolder,
    /// Open the selected Main folder in Explorer (the list's Enter key): the
    /// same `ShellExecuteW` path the report URL and the log-folder button use.
    OpenFolder,
    HideWindow,
    /// Open the "Fix match" picker (the manual latch UI): a modal popup owned
    /// by the main window with its own message loop. Same re-entrancy law as
    /// every other arm -- it pumps, so `UI` must be free when it runs.
    FixMatch,
}

impl UiState {
    /// `invalidateMain` (`go\window.go:1745`).
    pub fn invalidate(&self) {
        if self.main_hwnd != 0 {
            // SAFETY: InvalidateRect only marks the window.
            unsafe {
                crate::win32::InvalidateRect(self.main_hwnd as HWND, std::ptr::null(), 0);
            }
        }
    }

    /// `setStatusText` (`go\window.go:2764`).
    pub fn set_status_text(&mut self, s: impl Into<String>) {
        self.status_text = s.into();
        self.invalidate();
    }

    /// `openSettings` (`go\window.go:1211`).
    pub fn open_settings(&mut self) {
        self.cur_view = View::Settings;
        self.hover_btn = 0;
        self.pressed_btn = 0;
        self.hover_row = -1;
        self.hover_x = false;
        self.pressed_x = -1;
        self.focus_id = 0;
        self.invalidate();
    }

    /// `closeSettings` (`go\window.go:1220`).
    pub fn close_settings(&mut self) {
        self.cur_view = View::Main;
        self.hover_btn = 0;
        self.pressed_btn = 0;
        self.focus_id = 0;
        self.invalidate();
    }

    /// `openCard` (`go\window.go:1652`).
    pub fn open_card(&mut self) {
        self.cur_view = View::Card;
        self.hover_btn = 0;
        self.pressed_btn = 0;
        self.hover_row = -1;
        self.hover_x = false;
        self.pressed_x = -1;
        self.focus_id = 0;
        self.synopsis_pinned = false;
        // A preload that landed while the card was closed is applied before
        // priming, so the list opens on the playing title's rows with no
        // network wait.
        self.on_preload_done();
        self.prime_picker();
        self.invalidate();
    }

    /// Seed the one query field from the playing file and -- when the preload
    /// cache holds this title/year -- fill the list from it immediately.
    /// Universal search: the backend still classifies the playing file (the
    /// shape × class inference below), but every query runs both chains, so
    /// the filter is always `All` and there is no user-selected scope.
    /// Without a cache hit nothing is opened: the user runs Search.
    fn prime_picker(&mut self) {
        if self.picker.busy {
            return;
        }
        let Some(now) = app_support::config::live_now_playing() else {
            return;
        };
        // The field follows the playing file: an empty field is seeded, and a
        // path change (a switch, or the next episode) re-seeds even when the
        // title is unchanged. A query the user typed for THIS file survives
        // every later poll; a query for the previous file does not.
        let switched = !self.picker_path.is_empty() && self.picker_path != now.path;
        if self.picker.query.trim().is_empty() || switched {
            self.picker
                .set_query(fix_match_prefill_name(&now.raw_title, &now.path));
        }
        self.picker_path = now.path.clone();
        self.picker
            .set_local(Some(crate::search::LocalEvidence::from_path(&now.path)));
        let _inferred = crate::search::auto_filter(
            crate::search::detect_shape(&now.path),
            crate::search::detect_class(&now.path),
        );
        let filter = crate::search::KindFilter::All;
        self.picker.set_filter(filter);
        let (title, year) = self.picker.parsed();
        if let Some(rows) = self.preload_rows_for(&title, year) {
            let generation = self.picker.generation;
            self.picker.busy = true;
            self.picker.push_search(generation, rows);
            self.picker.finish_search_now(generation);
            // Approved copy: no "(cached)" marker on the Card.
            self.preload.applied = true;
        }
    }

    /// The cached rows for `title`/`year` under the current scope, if the
    /// preload holds them. The preload's own year is a wildcard: a filename
    /// carries no year, and a typed year narrows the rows instead of missing
    /// the entry. An unknown or mismatched scope never matches.
    fn preload_rows_for(&self, title: &str, year: i32) -> Option<Vec<Candidate>> {
        let pre = &self.preload;
        if !pre.ready || pre.rows.is_empty() || pre.filter != self.picker.filter {
            return None;
        }
        if !pre.title.trim().eq_ignore_ascii_case(title.trim()) {
            return None;
        }
        if !(pre.year == 0 || year == 0 || pre.year == year) {
            return None;
        }
        let narrowed = rows_for_year(pre.rows.clone(), year);
        // A year the file carried must not empty the instant paint: providers
        // disagree about release years, so a strict narrow is allowed to fall
        // back to the preload's own rows. Either way the local evidence
        // re-ranks them, so the instant paint matches the dialog's order.
        let prefer = if self.picker_path.trim().is_empty() {
            None
        } else {
            Some(match crate::search::detect_shape(&self.picker_path) {
                crate::search::MediaShape::Series => Kind::SERIES,
                crate::search::MediaShape::Movie => Kind::MOVIE,
            })
        };
        let base = if narrowed.is_empty() && year > 0 {
            pre.rows.clone()
        } else {
            narrowed
        };
        let evidence = crate::search::LocalEvidence::from_path(&self.picker_path);
        let rows = crate::search::merge_candidates_evidenced(
            title,
            year,
            prefer,
            Some(&evidence),
            vec![base],
        );
        (!rows.is_empty()).then_some(rows)
    }

    /// Prefetch the likely result set for what is playing, so "Fix match" is
    /// already populated when the card opens.
    ///
    /// The key is title|year|scope, never the file path: the next episode of
    /// the same show is a hit, and a path change re-latches the card to the
    /// new file without re-searching. A changed work cancels the in-flight
    /// preload (stale providers stop being asked) and, while no search runs,
    /// drops the stale query so [`Self::prime_picker`] can seed the new title. The
    /// persistent cache answers first; only a miss or a partial entry with
    /// failed providers starts the one background thread.
    pub fn prefetch_now_playing(&mut self) {
        if !self.cache_enabled {
            return;
        }
        let Some(now) = app_support::config::live_now_playing() else {
            return;
        };
        let name = fix_match_prefill_name(&now.raw_title, &now.path);
        let (title, year) = parse_title_year(&name);
        if title.trim().is_empty() {
            return;
        }
        // Universal search: the backend still classifies the playing file,
        // but the search itself always runs both chains under `All`.
        let _inferred = crate::search::auto_filter(
            crate::search::detect_shape(&now.path),
            crate::search::detect_class(&now.path),
        );
        let filter = crate::search::KindFilter::All;
        let key = crate::search::search_cache_key(&title, year, filter);
        // A path change is a re-latch even when the work's key is unchanged:
        // the next episode keeps the show identity and re-derives its own
        // season/episode, and a switch to another file behind the same title
        // must not keep a consumed preload.
        let path_changed = !self.preload.path.is_empty() && self.preload.path != now.path;
        if path_changed {
            self.preload.path = now.path.clone();
            self.preload.applied = false;
            if self.cur_view == View::Card && !self.picker.busy {
                self.prime_picker();
            }
        }
        // A card opened before the poll loop published the playing title gets
        // its seed on the first poll that has one. One prime per preload key;
        // a query the user clears afterwards is never re-seeded.
        if self.cur_view == View::Card
            && !self.picker.busy
            && self.preload.key == key
            && self.preload.ready
            && !self.preload.applied
        {
            self.prime_picker();
        }
        if self.preload.key == key && (self.preload.ready || self.preload_cancel.is_some()) {
            return;
        }
        // A different work: the query belonged to the previous title.
        let changed = !self.preload.title.is_empty()
            && (!self.preload.title.trim().eq_ignore_ascii_case(title.trim())
                || (self.preload.year > 0 && year > 0 && self.preload.year != year));
        if changed && !self.picker.busy {
            self.picker.set_query(String::new());
        }
        if let Some(cancel) = self.preload_cancel.take() {
            cancel.cancel();
        }
        self.preload = PreloadCache {
            key: key.clone(),
            title: title.clone(),
            year,
            filter,
            path: now.path.clone(),
            rows: Vec::new(),
            ready: false,
            applied: false,
        };
        self.preload_generation += 1;
        let generation = self.preload_generation;
        let cached = cached_search(&key).filter(|c| !c.rows.is_empty());
        let mut only: Vec<String> = Vec::new();
        if let Some(cached) = &cached {
            let rows: Vec<Candidate> = cached
                .rows
                .iter()
                .filter(|cand| filter.includes_candidate(cand))
                .cloned()
                .collect();
            if !rows.is_empty() {
                self.preload.rows = rows;
                self.preload.ready = true;
                app_logf(&format!(
                    "picker: cache hit \"{title}\" ({} rows, {} failed)",
                    cached.rows.len(),
                    cached.failed.len()
                ));
                if cached.failed.is_empty() {
                    self.invalidate();
                    return;
                }
            }
            only = cached.failed.clone();
        }
        let slot = self.preload_slot.clone();
        let search = self.picker_search.clone();
        let cancel = metadata_http::cancel::Cancel::with_deadline(FIX_MATCH_DEADLINE);
        self.preload_cancel = Some(cancel.clone());
        let hwnd = self.main_hwnd;
        let path = now.path.clone();
        std::thread::spawn(move || {
            let start = Instant::now();
            // A failed or cancelled pass still publishes (with no rows): that
            // is what clears the cancel and lets a later poll retry, instead of
            // leaving the key stuck as in-flight forever.
            let rows = match search(&title, year, filter, &only, &|_| {}) {
                Ok(report) if cancel.err().is_none() => report.rows,
                _ => Vec::new(),
            };
            let result = PreloadResult {
                key,
                title,
                year,
                filter,
                path,
                rows,
                elapsed_ms: start.elapsed().as_millis(),
            };
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((generation, result));
            if hwnd != 0 {
                // SAFETY: async post only; the worker touches no window state.
                unsafe {
                    crate::win32::PostMessageW(hwnd as HWND, WM_APP_PRELOAD_DONE, 0, 0);
                }
            }
        });
    }

    /// Apply a finished preload. A stale generation is ignored (the work
    /// changed), and an empty preload leaves the list closed so a manual
    /// search is the only way to open it.
    pub fn on_preload_done(&mut self) {
        let got = self
            .preload_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some((generation, result)) = got else {
            return;
        };
        if generation != self.preload_generation {
            return;
        }
        self.preload_cancel = None;
        let row_count = result.rows.len();
        self.preload = PreloadCache {
            key: result.key,
            title: result.title.clone(),
            year: result.year,
            filter: result.filter,
            path: result.path,
            rows: result.rows,
            ready: row_count > 0,
            applied: false,
        };
        if self.preload.ready {
            app_logf(&format!(
                "picker: preloaded {row_count} rows in {}ms for \"{}\"",
                result.elapsed_ms, result.title
            ));
            self.invalidate();
        }
    }
    /// The view a cold launch opens: the search surface while nothing is
    /// playing, the library surface while something is (the shipped behaviour
    /// for a playing title). Pure so the rule is pinned, and so the launch path
    /// and its test read one predicate.
    #[must_use]
    pub fn cold_open_view(playing: bool) -> View {
        if playing { View::Main } else { View::Card }
    }

    /// Open the cold-launch view. Called once by [`create_main_window`]; the
    /// Card branch runs the same `enter_card` the tab runs, so the first screen
    /// is the real search surface and not a mock.
    pub fn open_cold_view_with(&mut self, playing: bool) {
        match Self::cold_open_view(playing) {
            View::Card => {
                self.enter_card();
                if !playing {
                    // The caller's `false` is authoritative even if the global
                    // now-playing mirror still carries a previous title: no
                    // player means no filename-derived scope.
                    self.picker.set_filter(crate::search::KindFilter::All);
                    self.picker.set_query(String::new());
                }
            }
            View::Main => self.enter_main(),
            View::Settings => self.open_settings(),
        }
    }

    /// `closeCard` (`go\window.go:1661`).
    pub fn close_card(&mut self) {
        self.cur_view = View::Main;
        self.hover_btn = 0;
        self.pressed_btn = 0;
        self.focus_id = 0;
        self.synopsis_pinned = false;
        self.invalidate();
    }

    /// `enterSettings` (`go\window.go:1230`).
    pub fn enter_settings(&mut self) {
        // The registry is the source of truth; the config only caches it.
        self.startup_state =
            crate::settings::sync_startup_cache(&app_support::config::config_path());
        let cfg = app_support::config::load_config();
        self.scan_speed_state = cfg.effective_scan_speed().to_string();
        self.theme_state = cfg.effective_theme().to_string();
        set_live_theme(&self.theme_state);
        self.open_settings();
    }

    /// `enterMain` (`go\window.go:1238`).
    pub fn enter_main(&mut self) {
        self.close_settings();
    }

    /// `enterCard` (`go\window.go:1670`).
    pub fn enter_card(&mut self) {
        self.card_specs_state = app_support::config::load_config().effective_card_specs();
        self.open_card();
    }

    /// The tab-strip target for a button id: the Card/Settings tabs switch
    /// views, everything else is not a tab. Pure so the mouse and keyboard
    /// routes share one vocabulary.
    #[must_use]
    pub fn tab_target(id: i32) -> Option<View> {
        match id {
            BTN_CARD => Some(View::Card),
            BTN_SETTINGS => Some(View::Settings),
            _ => None,
        }
    }

    /// Switch to the tab `id` names, if it names one. Calls the existing
    /// `enter_card`/`enter_settings` only when the view actually changes, so
    /// the per-view hover/press reset those functions own runs exactly on a
    /// switch and a same-view click is a no-op. Returns whether `id` was a tab.
    pub fn switch_tab(&mut self, id: i32) -> bool {
        let Some(target) = Self::tab_target(id) else {
            return false;
        };
        if target == self.cur_view {
            return true;
        }
        match target {
            View::Card => self.enter_card(),
            View::Settings => self.enter_settings(),
            View::Main => self.enter_main(),
        }
        true
    }

    /// Rows per wheel notch (the `SPI_GETWHEELSCROLLLINES` default): one notch
    /// moves three folder rows.
    pub const WHEEL_SCROLL_ROWS: i32 = 3;

    /// Scroll the Main folder list by `rows` (positive = toward the end),
    /// clamped to the real scroll range; clears the transient row hover so a
    /// highlight never silently points at a different folder after the
    /// viewport moves. Selection (`sel_row`) is an absolute folder index and
    /// survives scrolling. Returns whether the viewport moved. Only
    /// `InvalidateRect` (asynchronous, never dispatches `WM_PAINT`) runs here,
    /// so this is NOT a re-entrant site: no guard or regression-test change.
    pub fn scroll_list(&mut self, hwnd: HWND, rows: i32) -> bool {
        if self.cur_view != View::Main || rows == 0 {
            return false;
        }
        let rc = get_client_rect(hwnd);
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
        if cw <= 0 || ch <= 0 {
            return false;
        }
        let total = self.ui_dirs.len() as i32;
        let want = self.scroll_top.saturating_add(rows);
        let top = compute_layout_total(total, cw, ch, window_dpi(hwnd), want).list_top;
        if top == self.scroll_top {
            return false;
        }
        self.scroll_top = top;
        self.hover_row = -1;
        self.hover_x = false;
        self.invalidate();
        true
    }

    /// Scroll the picker result list by `rows` (positive = toward the end),
    /// clamped to the real scroll range. Only `InvalidateRect` runs here, so
    /// this is NOT a re-entrant site.
    pub fn scroll_picker(&mut self, hwnd: HWND, rows: i32) -> bool {
        if self.cur_view != View::Card || !self.picker.open || rows == 0 {
            return false;
        }
        let Some(l) = self.layout_card(hwnd) else {
            // Headless (unit tests): clamp against the logical capacity alone.
            let capacity = 1usize;
            let max = picker_max_top(self.picker.candidates.len(), capacity);
            let want = (self.picker.top as i32).saturating_add(rows).clamp(0, max as i32);
            if want as usize == self.picker.top {
                return false;
            }
            self.picker.top = want as usize;
            self.invalidate();
            return true;
        };
        let capacity = picker_capacity(&l, window_dpi(hwnd));
        if capacity == 0 {
            return false;
        }
        let max = picker_max_top(self.picker.candidates.len(), capacity);
        let want = (self.picker.top as i32).saturating_add(rows).clamp(0, max as i32);
        if want as usize == self.picker.top {
            return false;
        }
        self.picker.top = want as usize;
        self.invalidate();
        true
    }

    /// Scroll the Settings body by logical `px` (positive = toward the end),
    /// clamped to the real scroll range. Headless-safe: a null HWND clamps
    /// against the 460x680 composition. Only `InvalidateRect` runs here, so
    /// this is NOT a re-entrant site.
    pub fn scroll_settings(&mut self, hwnd: HWND, px: i32) -> bool {
        if self.cur_view != View::Settings || px == 0 {
            return false;
        }
        let (cw, ch, dpi) = if hwnd.is_null() {
            (MAIN_CLIENT_W, MAIN_CLIENT_H, 96)
        } else {
            let rc = get_client_rect(hwnd);
            let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
            if cw <= 0 || ch <= 0 {
                return false;
            }
            (cw, ch, window_dpi(hwnd))
        };
        let raw = compute_settings_layout(
            cw,
            ch,
            dpi,
            self.startup_state,
            &self.scan_speed_state,
            &self.theme_state,
        );
        let max = settings_max_scroll(&raw, ch, dpi);
        let want = (self.settings_scroll + px).clamp(0, max);
        if want == self.settings_scroll {
            return false;
        }
        self.settings_scroll = want;
        self.hover_btn = 0;
        self.invalidate();
        true
    }

    /// Headless Settings scroll for tests: the same clamp [`UiState::scroll_settings`]
    /// applies, without an HWND.
    pub fn scroll_settings_for(&mut self, cw: i32, ch: i32, dpi: i32, px: i32) -> bool {
        if self.cur_view != View::Settings || px == 0 {
            return false;
        }
        let raw = compute_settings_layout(
            cw,
            ch,
            dpi,
            self.startup_state,
            &self.scan_speed_state,
            &self.theme_state,
        );
        let max = settings_max_scroll(&raw, ch, dpi);
        let want = (self.settings_scroll + px).clamp(0, max);
        if want == self.settings_scroll {
            return false;
        }
        self.settings_scroll = want;
        self.hover_btn = 0;
        self.invalidate();
        true
    }

    /// One viewport of Settings rows at the window's DPI: the PageUp/PageDown
    /// step. Headless-safe (null HWND reads the 460x680 composition).
    fn settings_page_px(&self, hwnd: HWND) -> i32 {
        let (ch, dpi) = if hwnd.is_null() {
            (MAIN_CLIENT_H, 96)
        } else {
            let rc = get_client_rect(hwnd);
            let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
            if cw <= 0 || ch <= 0 {
                return 1;
            }
            let _ = cw;
            (ch, window_dpi(hwnd))
        };
        let sc = |v: i32| scale_px(dpi, v);
        let status_top = (ch - sc(STATUS_BAR_H_L)).max(0);
        (status_top - settings_scroll_origin(dpi)).max(sc(48))
    }

    /// Keep the focused Settings control inside the viewport after focus
    /// moves: the focus order shares the paint/hit-test scroll offset, so Tab
    /// never strands focus on a row below the fold. Headless-safe.
    fn reveal_settings_focused(&mut self, hwnd: HWND) {
        if self.cur_view != View::Settings || self.focus_id == 0 {
            return;
        }
        let (cw, ch, dpi) = if hwnd.is_null() {
            (MAIN_CLIENT_W, MAIN_CLIENT_H, 96)
        } else {
            let rc = get_client_rect(hwnd);
            let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
            if cw <= 0 || ch <= 0 {
                return;
            }
            (cw, ch, window_dpi(hwnd))
        };
        let sc = |v: i32| scale_px(dpi, v);
        let status_top = (ch - sc(STATUS_BAR_H_L)).max(0);
        let origin = settings_scroll_origin(dpi);
        let raw = compute_settings_layout(
            cw,
            ch,
            dpi,
            self.startup_state,
            &self.scan_speed_state,
            &self.theme_state,
        );
        let Some(rc) = raw.btns.iter().find(|b| b.id == self.focus_id).map(|b| b.rc) else {
            return;
        };
        if rc.bottom <= rc.top || rc.top < origin {
            return;
        }
        let max = settings_max_scroll(&raw, ch, dpi);
        let mut want = self.settings_scroll;
        if rc.top - want < origin {
            want = rc.top - origin;
        } else if rc.bottom - want > status_top {
            want = rc.bottom - status_top;
        }
        self.settings_scroll = want.clamp(0, max);
    }

    /// Keep the picker's highlight inside the viewport after it moves.
    fn reveal_picker_selected(&mut self, hwnd: HWND) {
        if !self.picker.open {
            return;
        }
        let Some(l) = self.layout_card(hwnd) else {
            return;
        };
        let capacity = picker_capacity(&l, window_dpi(hwnd));
        if capacity == 0 {
            return;
        }
        let max = picker_max_top(self.picker.candidates.len(), capacity);
        let top = self.picker.top.min(max);
        let sel = self.picker.selected.min(self.picker.candidates.len().saturating_sub(1));
        let next = if sel < top {
            sel
        } else if sel >= top + capacity {
            sel + 1 - capacity
        } else {
            top
        };
        self.picker.top = next.min(max);
    }

    /// One viewport of folder rows at the window's DPI: the PageUp/PageDown
    /// step.
    fn page_rows(&self, hwnd: HWND) -> i32 {
        let rc = get_client_rect(hwnd);
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
        if cw <= 0 || ch <= 0 {
            return 1;
        }
        compute_layout_total(
            self.ui_dirs.len() as i32,
            cw,
            ch,
            window_dpi(hwnd),
            self.scroll_top,
        )
        .list_capacity
        .max(1)
    }

    /// The Main folder list's keyboard map: Up/Down step the absolute
    /// selection, Home/End jump to its first/last row, and Enter stashes the
    /// selected folder's open ([`BlockingUi::OpenFolder`]) for the wndproc to
    /// run outside the `UI` guard. The list owns these keys only while it has
    /// rows: with an empty list Up/Down/Home/End fall through to the focus
    /// order and the scroll they drove before, and the Settings/Card views
    /// never reach it (`sel_row` is Main-only state). A list move clears
    /// `focus_id`, so Enter after arrowing targets the row, not a stale button,
    /// and the keyboard offers no Remove/rename/delete path on the list.
    fn on_main_list_key(&mut self, hwnd: HWND, vk: u32) -> bool {
        if self.cur_view != View::Main {
            return false;
        }
        let n = self.ui_dirs.len();
        match vk {
            VK_UP | VK_DOWN | VK_HOME | VK_END if n > 0 => {
                let want = match vk {
                    VK_UP => step_sel_row(self.sel_row, n, -1),
                    VK_DOWN => step_sel_row(self.sel_row, n, 1),
                    VK_HOME => edge_sel_row(n, false),
                    _ => edge_sel_row(n, true),
                };
                self.focus_id = 0;
                self.sel_row = want;
                self.reveal_sel_row(hwnd);
                true
            }
            VK_RETURN
                if n > 0
                    && self.focus_id == 0
                    && self.sel_row >= 0
                    && (self.sel_row as usize) < n =>
            {
                // Stash the blocking shell-out; the wndproc runs it with `UI`
                // released (`run_blocking_ui`), never under the guard.
                self.pending_blocking = Some(BlockingUi::OpenFolder);
                true
            }
            _ => false,
        }
    }

    /// Keep the keyboard selection inside the viewport: when a list key moves
    /// `sel_row` off the visible band, the viewport follows by the same row
    /// step the wheel uses. The band is the [`UiLayout`] the click hit-test and
    /// the paint both read, so a shown row and a selectable row cannot
    /// disagree. No-op on a null/hidden window (no layout).
    fn reveal_sel_row(&mut self, hwnd: HWND) {
        let Some(l) = self.layout(hwnd) else {
            return;
        };
        let last = l.list_top + l.list_capacity.max(1) - 1;
        if self.sel_row < l.list_top {
            self.scroll_list(hwnd, self.sel_row - l.list_top);
        } else if self.sel_row > last {
            self.scroll_list(hwnd, self.sel_row - last);
        }
    }

    /// `layoutForHwnd` (`go\window.go:731`).
    #[must_use]
    pub fn layout(&self, hwnd: HWND) -> Option<UiLayout> {
        let rc = get_client_rect(hwnd);
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
        if cw <= 0 || ch <= 0 {
            return None;
        }
        Some(compute_layout_from(
            &self.ui_dirs,
            cw,
            ch,
            window_dpi(hwnd),
            self.scroll_top,
        ))
    }

    /// `layoutForSettings` (`go\window.go:744`).
    #[must_use]
    pub fn layout_settings(&self, hwnd: HWND) -> Option<SettingsLayout> {
        let rc = get_client_rect(hwnd);
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
        if cw <= 0 || ch <= 0 {
            return None;
        }
        Some(settings_layout_scrolled(
            cw,
            ch,
            window_dpi(hwnd),
            self.startup_state,
            &self.scan_speed_state,
            &self.theme_state,
            self.settings_scroll,
        ))
    }

    /// `layoutForCard` (`go\window.go:1639`). One table: the base composition
    /// plus one button per visible result row (`BTN_PICKER_RESULT_AT + idx`)
    /// using the same `picker_row_rects` the paint reads, so hit-test and
    /// pixels share one rect per row.
    #[must_use]
    pub fn layout_card(&self, hwnd: HWND) -> Option<CardLayout> {
        let rc = get_client_rect(hwnd);
        let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
        if cw <= 0 || ch <= 0 {
            return None;
        }
        let dpi = window_dpi(hwnd);
        let mut l = compute_card_layout_state(
            cw,
            ch,
            dpi,
            &self.card_specs_state,
            self.displayed_card_kind(),
            self.preview_surface,
            &self.card_layout_state(),
        );
        if self.picker.open {
            let top = self
                .picker
                .top
                .min(picker_max_top(self.picker.candidates.len(), picker_capacity(&l, dpi)));
            for (pos, rc) in picker_row_rects(&l, self.picker.candidates.len(), top, dpi)
                .into_iter()
                .enumerate()
            {
                let idx = top + pos;
                l.btns.push(UiButton {
                    id: BTN_PICKER_RESULT_AT + idx as i32,
                    label: String::new(),
                    style: BTN_OUTLINE,
                    rc,
                });
            }
        }
        Some(l)
    }

    /// The picker's layout state: the one place the composition learns what the
    /// picker model is doing, so the paint pass and the hit-test cannot disagree.
    #[must_use]
    pub fn card_layout_state(&self) -> CardLayoutState {
        let has_selection = self.picker.open && self.picker.selected_candidate().is_some();
        let playing = app_support::config::live_now_playing().is_some();
        CardLayoutState {
            advanced: self.advanced_open,
            has_selection,
            // Applying changes the playing title, so it needs a playing title
            // AND a row the user picked: an auto highlight is not a choice.
            can_apply: self.has_picked_candidate() && playing,
            playing,
            filter: self.picker.filter,
        }
    }

    /// Headless layout for tests: the same composition `layout_card` builds,
    /// without an HWND. Every click test takes its rect from here, so the
    /// rect and the hit-test share one table by construction.
    #[must_use]
    pub fn layout_card_for(&self, cw: i32, ch: i32, dpi: i32) -> CardLayout {
        let mut l = compute_card_layout_state(
            cw,
            ch,
            dpi,
            &self.card_specs_state,
            self.displayed_card_kind(),
            self.preview_surface,
            &self.card_layout_state(),
        );
        if self.picker.open {
            let top = self
                .picker
                .top
                .min(picker_max_top(self.picker.candidates.len(), picker_capacity(&l, dpi)));
            for (pos, rc) in picker_row_rects(&l, self.picker.candidates.len(), top, dpi)
                .into_iter()
                .enumerate()
            {
                let idx = top + pos;
                l.btns.push(UiButton {
                    id: BTN_PICKER_RESULT_AT + idx as i32,
                    label: String::new(),
                    style: BTN_OUTLINE,
                    rc,
                });
            }
        }
        l
    }

    /// Headless Settings layout for tests: the same table `layout_settings`
    /// hit-tests, without an HWND.
    #[must_use]
    pub fn layout_settings_for(&self, cw: i32, ch: i32, dpi: i32) -> SettingsLayout {
        settings_layout_scrolled(
            cw,
            ch,
            dpi,
            self.startup_state,
            &self.scan_speed_state,
            &self.theme_state,
            self.settings_scroll,
        )
    }

    /// The window's own click entry point for tests: `WM_LBUTTONDOWN` +
    /// `WM_LBUTTONUP` at `(x, y)` through `press_at`/`release_at` semantics,
    /// but against an explicit layout (no HWND). Press arms the id under the
    /// point, release fires it iff the same id is still under the point —
    /// exactly what the wndproc does. Returns the blocking action, if any.
    pub fn click_layout(
        &mut self,
        layout: &SettingsLayout,
        x: i32,
        y: i32,
    ) -> Option<BlockingUi> {
        // The status bar is chrome: a release at/below it never fires a row.
        let pressed = button_at_point_above_status(&layout.btns, layout.status.top, x, y);
        if pressed == 0 {
            return None;
        }
        self.focus_id = pressed;
        // Tabs route through the same `switch_tab` the wndproc uses.
        if self.switch_tab(pressed) {
            return None;
        }
        self.do_settings_action(std::ptr::null_mut(), pressed)
    }

    /// Card twin of [`UiState::click_layout`]: DOWN+UP at `(x, y)` against an
    /// explicit [`CardLayout`], through `activate_focused` (the same call the
    /// wndproc's `release_at` makes). Result rows select without applying;
    /// Apply only acts with a pick (see `card_picker_match`). Row hits resolve
    /// through [`picker_result_at`], the same table the paint reads.
    pub fn click_card_layout(
        &mut self,
        layout: &CardLayout,
        x: i32,
        y: i32,
    ) -> Option<BlockingUi> {
        self.click_card_layout_at(layout, x, y, 96)
    }

    /// [`UiState::click_card_layout`] with an explicit DPI: DOWN+UP as one
    /// paired press so headless clicks share the press-match rule the wndproc
    /// enforces. `dpi` must be the dpi `layout` was built with.
    pub fn click_card_layout_at(
        &mut self,
        layout: &CardLayout,
        x: i32,
        y: i32,
        dpi: i32,
    ) -> Option<BlockingUi> {
        self.press_card_layout_at(layout, x, y, dpi);
        self.release_card_layout_at(layout, x, y, dpi)
    }

    /// Arm the press for a headless Card click: the id under the point, rows
    /// resolved through [`picker_result_at`].
    pub fn press_card_layout_at(&mut self, layout: &CardLayout, x: i32, y: i32, dpi: i32) {
        self.pressed_btn = 0;
        self.pressed_x = -1;
        if self.picker.open && pt_in_rect(layout.picker_results, x, y) {
            if let Some(idx) = picker_result_at(layout, &self.picker, x, y, dpi) {
                self.pressed_btn = BTN_PICKER_RESULT_AT + idx as i32;
            } else {
                let hit = button_at_point_above_status(&layout.btns, layout.status.top, x, y);
                if hit == BTN_FIX_MATCH || hit == BTN_PICKER_APPLY {
                    self.pressed_btn = hit;
                }
            }
        } else {
            let id = button_at_point_above_status(&layout.btns, layout.status.top, x, y);
            if id != 0 {
                self.pressed_btn = id;
            }
        }
        self.invalidate();
    }

    /// Fire a headless Card release: only the armed press activates, exactly
    /// as `release_at` enforces -- a button-up with no matching press
    /// activates nothing.
    pub fn release_card_layout_at(
        &mut self,
        layout: &CardLayout,
        x: i32,
        y: i32,
        dpi: i32,
    ) -> Option<BlockingUi> {
        // Inside an open picker the button table wins, exactly as `release_at`
        // does: Apply/Fix are actions, result rows are selections.
        if self.picker.open && pt_in_rect(layout.picker_results, x, y) {
            let pb = self.pressed_btn;
            self.pressed_btn = 0;
            self.pressed_x = -1;
            let hit = button_at_point_above_status(&layout.btns, layout.status.top, x, y);
            if hit == BTN_FIX_MATCH || hit == BTN_PICKER_APPLY {
                if pb == hit {
                    return self.do_card_action(std::ptr::null_mut(), hit);
                }
                self.invalidate();
                return None;
            }
            if let Some(idx) = picker_result_at(layout, &self.picker, x, y, dpi) {
                if pb == BTN_PICKER_RESULT_AT + idx as i32 {
                    self.select_candidate(idx);
                    self.focus_id = BTN_PICKER_SEARCH;
                    if app_support::config::live_now_playing().is_some() {
                        self.card_picker_match();
                    }
                } else {
                    self.invalidate();
                }
                return None;
            }
            return None;
        }
        let pb = self.pressed_btn;
        self.pressed_btn = 0;
        self.pressed_x = -1;
        if pb != 0 && button_at_point_above_status(&layout.btns, layout.status.top, x, y) == pb {
            self.focus_id = pb;
            return self.activate_focused(std::ptr::null_mut(), pb);
        }
        if pb != 0 {
            self.invalidate();
        }
        None
    }

    /// `scanBlockedReason` (`go\window.go:1992`) reading this instance's flag.
    #[must_use]
    pub fn scan_blocked_reason(&self, id: i32) -> &'static str {
        let reason = scan_blocked_reason(self.scan_active, id);
        if !reason.is_empty() || id != BTN_FIX_MATCH {
            return reason;
        }
        // The picker's own enable rule (no tooltip lie: the same rule dims the
        // button and refuses the click).
        fix_match_blocked_reason(app_support::config::live_now_playing().is_some(), id)
    }

    /// `removeHasTarget` (`go\window.go:2006`).
    #[must_use]
    pub fn remove_has_target(&self) -> bool {
        remove_has_target(self.sel_row, self.hover_row, self.ui_dirs.len())
    }

    /// `updateHover` (`go\window.go:757`).
    pub fn update_hover(&mut self, hwnd: HWND, x: i32, y: i32) -> bool {
        match self.cur_view {
            View::Card => {
                let Some(l) = self.layout_card(hwnd) else {
                    return false;
                };
                let nb = button_at_point_above_status(&l.btns, l.status.top, x, y);
                if nb == self.hover_btn && self.hover_row == -1 && !self.hover_x {
                    return false;
                }
                self.hover_btn = nb;
                self.hover_row = -1;
                self.hover_x = false;
                true
            }
            View::Settings => {
                let Some(l) = self.layout_settings(hwnd) else {
                    return false;
                };
                let nb = button_at_point_above_status(&l.btns, l.status.top, x, y);
                if nb == self.hover_btn && self.hover_row == -1 && !self.hover_x {
                    return false;
                }
                self.hover_btn = nb;
                self.hover_row = -1;
                self.hover_x = false;
                true
            }
            View::Main => {
                let Some(l) = self.layout(hwnd) else {
                    return false;
                };
                let mut nb = button_at_point_above_status(&l.btns, l.status.top, x, y);
                if pt_in_rect(l.settings, x, y) {
                    nb = BTN_SETTINGS;
                }
                if pt_in_rect(l.card_link, x, y) {
                    nb = BTN_CARD;
                }
                let nr = row_at_point_above_status(&l.rows, l.status.top, x, y);
                let mut nx = false;
                if nr >= 0
                    && (nr as usize) < l.xbtns.len()
                    && clip_rect_above_status(l.xbtns[nr as usize], l.status.top)
                        .is_some_and(|rc| pt_in_rect(rc, x, y))
                {
                    nx = true;
                }
                // Absolute: the viewport row plus the scroll offset, so the
                // highlight tracks the folder, not the slot.
                let want = if nr >= 0 { l.list_top + nr } else { -1 };
                if nb == self.hover_btn && want == self.hover_row && nx == self.hover_x {
                    return false;
                }
                self.hover_btn = nb;
                self.hover_row = want;
                self.hover_x = nx;
                true
            }
        }
    }

    /// `pressAt` (`go\window.go:806`). One table: the pressed id comes from
    /// the same layout button table `release_at` hit-tests, including the
    /// picker result rows.
    pub fn press_at(&mut self, hwnd: HWND, x: i32, y: i32) {
        match self.cur_view {
            View::Card => {
                let Some(l) = self.layout_card(hwnd) else {
                    return;
                };
                self.pressed_btn = 0;
                self.pressed_x = -1;
                let id = button_at_point_above_status(&l.btns, l.status.top, x, y);
                if id != 0 {
                    self.pressed_btn = id;
                }
                self.invalidate();
            }
            View::Settings => {
                let Some(l) = self.layout_settings(hwnd) else {
                    return;
                };
                self.pressed_btn = 0;
                self.pressed_x = -1;
                let id = button_at_point_above_status(&l.btns, l.status.top, x, y);
                if id != 0 {
                    self.pressed_btn = id;
                }
                self.invalidate();
            }
            View::Main => {
                let Some(l) = self.layout(hwnd) else {
                    return;
                };
                self.pressed_btn = 0;
                self.pressed_x = -1;
                if let Some(id) = Some(button_at_point_above_status(&l.btns, l.status.top, x, y))
                    .filter(|id| *id != 0)
                {
                    self.pressed_btn = id;
                } else if pt_in_rect(l.settings, x, y) {
                    self.pressed_btn = BTN_SETTINGS;
                } else if pt_in_rect(l.card_link, x, y) {
                    self.pressed_btn = BTN_CARD;
                } else {
                    let r = row_at_point_above_status(&l.rows, l.status.top, x, y);
                    if r >= 0
                        && (r as usize) < l.xbtns.len()
                        && clip_rect_above_status(l.xbtns[r as usize], l.status.top)
                            .is_some_and(|rc| pt_in_rect(rc, x, y))
                    {
                        self.pressed_x = l.list_top + r;
                    }
                }
                self.invalidate();
            }
        }
    }

    /// `releaseAt` (`go\window.go:852`).
    ///
    /// Returns a [`BlockingUi`] the wndproc must run with `UI` RELEASED: the
    /// dispatched action opens a modal dialog / shows a window, which pumps the
    /// queue and re-enters `main_wnd_proc`; holding the non-reentrant `UI`
    /// guard across it self-deadlocks the UI thread.
    #[must_use]
    pub fn release_at(&mut self, hwnd: HWND, x: i32, y: i32) -> Option<BlockingUi> {
        match self.cur_view {
            View::Card => {
                let Some(l) = self.layout_card(hwnd) else {
                    self.pressed_btn = 0;
                    self.pressed_x = -1;
                    return None;
                };
                if self.picker.open && pt_in_rect(l.picker_results, x, y) {
                    // One table: actions win over selection. Row hits resolve
                    // through `picker_result_at` (the same `picker_row_rects`
                    // the paint reads), and a release only selects when its
                    // press armed the same row -- a button-up with no matching
                    // press activates nothing.
                    let pb = self.pressed_btn;
                    self.pressed_btn = 0;
                    self.pressed_x = -1;
                    let hit = button_at_point(&l.btns, x, y);
                    if hit == BTN_FIX_MATCH || hit == BTN_PICKER_APPLY {
                        if pb == hit {
                            return self.do_card_action(hwnd, hit);
                        }
                        self.invalidate();
                        return None;
                    }
                    if let Some(idx) = picker_result_at(&l, &self.picker, x, y, window_dpi(hwnd)) {
                        if pb == BTN_PICKER_RESULT_AT + idx as i32 {
                            // Selecting a row previews it locally; it sends nothing.
                            // With something playing, the pick IS the latch: the
                            // same click applies it, so a user never has to hunt for
                            // the Apply row after choosing.
                            self.select_candidate(idx);
                            self.reveal_picker_selected(hwnd);
                            self.focus_id = BTN_PICKER_SEARCH;
                            if app_support::config::live_now_playing().is_some() {
                                self.card_picker_match();
                            }
                            self.invalidate();
                        } else {
                            self.invalidate();
                        }
                        return None;
                    }
                    return None;
                }
                let pb = self.pressed_btn;
                self.pressed_btn = 0;
                self.pressed_x = -1;
                if pb != 0 {
                    if button_at_point_above_status(&l.btns, l.status.top, x, y) == pb {
                        return self.activate_focused(hwnd, pb);
                    } else {
                        self.invalidate();
                    }
                    return None;
                }
                self.invalidate();
                None
            }
            View::Settings => {
                let Some(l) = self.layout_settings(hwnd) else {
                    self.pressed_btn = 0;
                    self.pressed_x = -1;
                    return None;
                };
                let pb = self.pressed_btn;
                self.pressed_btn = 0;
                self.pressed_x = -1;
                if pb != 0 {
                    if button_at_point_above_status(&l.btns, l.status.top, x, y) == pb {
                        return self.activate_focused(hwnd, pb);
                    }
                    self.invalidate();
                    return None;
                }
                self.invalidate();
                None
            }
            View::Main => {
                let Some(l) = self.layout(hwnd) else {
                    self.pressed_btn = 0;
                    self.pressed_x = -1;
                    return None;
                };
                let (pb, px) = (self.pressed_btn, self.pressed_x);
                self.pressed_btn = 0;
                self.pressed_x = -1;
                if pb != 0 {
                    if pb == BTN_SETTINGS {
                        if pt_in_rect(l.settings, x, y) {
                            self.enter_settings();
                        } else {
                            self.invalidate();
                        }
                        return None;
                    }
                    if pb == BTN_CARD {
                        if pt_in_rect(l.card_link, x, y) {
                            self.enter_card();
                        } else {
                            self.invalidate();
                        }
                        return None;
                    }
                    if button_at_point_above_status(&l.btns, l.status.top, x, y) == pb {
                        self.focus_id = pb;
                        return self.do_button_action(hwnd, pb);
                    }
                    self.invalidate();
                    return None;
                }
                if px >= 0 {
                    if self.scan_active {
                        self.set_status_text(SCAN_BUSY_TEXT);
                        return None;
                    }
                    let r = row_at_point_above_status(&l.rows, l.status.top, x, y);
                    if l.list_top + r == px
                        && (r as usize) < l.xbtns.len()
                        && clip_rect_above_status(l.xbtns[r as usize], l.status.top)
                            .is_some_and(|rc| pt_in_rect(rc, x, y))
                    {
                        self.remove_dir_at(px);
                    } else {
                        self.invalidate();
                    }
                    return None;
                }
                let r = row_at_point_above_status(&l.rows, l.status.top, x, y);
                if r >= 0 {
                    let abs = l.list_top + r;
                    // The list takes focus on click, mirroring the keyboard: a
                    // stale button ring would otherwise keep Enter on a button
                    // instead of the row just clicked.
                    let was_focused = self.focus_id != 0;
                    self.focus_id = 0;
                    if self.sel_row != abs || was_focused {
                        self.sel_row = abs;
                        self.invalidate();
                    }
                    return None;
                }
                let id = button_at_point_above_status(&l.btns, l.status.top, x, y);
                if id != 0 {
                    self.focus_id = id;
                    return self.do_button_action(hwnd, id);
                }
                None
            }
        }
    }

    /// `hitTestCode` (`go\window.go:956`).
    #[must_use]
    pub fn hit_test_code(&self, hwnd: HWND, x: i32, y: i32) -> i32 {
        match self.cur_view {
            View::Card => self.layout_card(hwnd).map_or(0, |l| {
                if self.picker.open && pt_in_rect(l.picker_results, x, y) {
                    return 1;
                }
                i32::from(button_at_point_above_status(&l.btns, l.status.top, x, y) != 0)
            }),
            View::Settings => self
                .layout_settings(hwnd)
                .map_or(0, |l| {
                    i32::from(button_at_point_above_status(&l.btns, l.status.top, x, y) != 0)
                }),
            View::Main => {
                let Some(l) = self.layout(hwnd) else {
                    return 0;
                };
                if button_at_point_above_status(&l.btns, l.status.top, x, y) != 0
                    || pt_in_rect(l.settings, x, y)
                    || pt_in_rect(l.card_link, x, y)
                    || row_at_point_above_status(&l.rows, l.status.top, x, y) >= 0
                {
                    return 1;
                }
                0
            }
        }
    }

    /// `doButtonAction` (`go\window.go:998`).
    ///
    /// Returns a [`BlockingUi`] the wndproc must run with `UI` RELEASED; `None`
    /// when the action is pure state work.
    #[must_use]
    pub fn do_button_action(&mut self, hwnd: HWND, id: i32) -> Option<BlockingUi> {
        let reason = self.scan_blocked_reason(id);
        if !reason.is_empty() {
            self.set_status_text(reason);
            return None;
        }
        match id {
            BTN_ADD => Some(BlockingUi::AddDir),
            BTN_REMOVE => {
                self.on_remove_selected();
                None
            }
            BTN_SCAN => {
                if self.scan_active {
                    self.cancel_library_scan();
                } else {
                    self.start_library_scan(hwnd, false);
                }
                None
            }
            BTN_REFRESH => {
                self.start_library_scan(hwnd, true);
                None
            }
            BTN_REPORT => {
                let path = repair_report_path();
                self.on_report_at(hwnd, &path);
                None
            }
            BTN_CLOSE => Some(BlockingUi::HideWindow),
            _ => None,
        }
    }

    /// `doSettingsAction` (`go\window.go:1395`). One table: every id the
    /// Settings layout paints is handled here, so a painted row is never dead.
    /// Action rows reuse the Main command ids; pills flip their stored bool.
    ///
    /// Returns a [`BlockingUi`] the wndproc must run with `UI` RELEASED.
    #[must_use]
    pub fn do_settings_action(&mut self, hwnd: HWND, id: i32) -> Option<BlockingUi> {
        // Media action rows: the same commands as Main.
        if id == BTN_ADD {
            return Some(BlockingUi::AddDir);
        }
        if id == BTN_SCAN {
            if self.scan_active {
                self.cancel_library_scan();
            } else {
                self.start_library_scan(hwnd, false);
            }
            return None;
        }
        if id == BTN_REFRESH {
            self.start_library_scan(hwnd, true);
            return None;
        }
        if id == BTN_REMOVE {
            self.on_remove_selected();
            return None;
        }
        if id == BTN_REPORT {
            let path = repair_report_path();
            self.on_report_at(hwnd, &path);
            return None;
        }
        // Behaviour pills: each flips its stored bool.
        if id == BTN_DISCORD {
            self.on_toggle_discord();
            return None;
        }
        if id == BTN_CLOSE {
            self.on_toggle_close_to_tray();
            return None;
        }
        match id {
            BTN_BACK => {
                self.enter_main();
                None
            }
            BTN_THEME_AMOLED | BTN_THEME_DARK => {
                let theme = crate::settings::theme_profile_for_button(id);
                if !theme.is_empty() {
                    self.on_select_theme(theme);
                }
                None
            }
            BTN_CLEAR_CACHE => Some(BlockingUi::ClearMetadata),
            BTN_STARTUP => {
                self.on_toggle_startup();
                None
            }
            _ => None,
        }
    }

    /// `doCardAction` (`go\window.go:1721`).
    ///
    /// Returns a [`BlockingUi`] the wndproc must run with `UI` RELEASED
    /// ([`BlockingUi::FixMatch`] opens the manual latch picker).
    #[must_use]
    pub fn do_card_action(&mut self, _hwnd: HWND, id: i32) -> Option<BlockingUi> {
        // One table: result rows are buttons too. Selecting previews locally
        // and never sends; Apply (or playing auto-latch) is the only writer.
        if id >= BTN_PICKER_RESULT_AT
            && ((id - BTN_PICKER_RESULT_AT) as usize) < self.picker.candidates.len()
        {
            let idx = (id - BTN_PICKER_RESULT_AT) as usize;
            if self.picker.open {
                self.select_candidate(idx);
                self.focus_id = BTN_PICKER_SEARCH;
                self.invalidate();
            }
            return None;
        }
        if id == BTN_FIX_MATCH {
            // Singleton-safe: a second open while the modal dialog lives is
            // refused here as well as in `open_fix_match` (the tray menu
            // pumps every thread message while the loop runs).
            if !fix_match_dialog_hwnd().is_null() {
                self.set_status_text(FIX_MATCH_ALREADY_OPEN_REASON);
                return None;
            }
            // The enable rule, enforced AT INPUT as well as in the paint: a
            // dimmed button that still opened the picker would be a lie.
            if fix_match_enabled(app_support::config::live_now_playing().as_ref()) {
                return Some(BlockingUi::FixMatch);
            }
            self.set_status_text(FIX_MATCH_IDLE_REASON);
            return None;
        }
        if id == BTN_PICKER_FIELD {
            self.focus_picker_field();
            return None;
        }
        if id == BTN_ADVANCED {
            self.advanced_open = !self.advanced_open;
            self.focus_id = BTN_ADVANCED;
            self.invalidate();
            return None;
        }
        if id == BTN_PICKER_CLOSE {
            self.picker.close_results();
            self.invalidate();
            return None;
        }
        if id == BTN_PICKER_APPLY {
            self.card_picker_match();
            return None;
        }
        if id == BTN_PICKER_SEARCH {
            // One verb, one job: Search always runs the query. Selecting a row
            // is what previews it, and applying a match is its own control.
            if self.picker.search_allowed() {
                self.card_picker_search();
            } else if self.picker.busy {
                self.picker.status = "Searching\u{2026}".to_string();
                self.invalidate();
            } else if let Some(issue) = crate::search::query_issue(&self.picker.query) {
                self.picker.status = issue.message().to_string();
                self.invalidate();
            }
            return None;
        }
        if id == BTN_SYNOPSIS {
            if self.preview_synopsis().is_empty() {
                self.set_status_text("No synopsis available.");
                return None;
            }
            self.synopsis_pinned = !self.synopsis_pinned;
            self.set_status_text(if self.synopsis_pinned {
                "Synopsis pinned - Enter or click to hide."
            } else {
                "Synopsis hidden."
            });
            return None;
        }
        if id == BTN_CARD_BACK {
            self.close_card();
        } else if let Some(surface) = PreviewSurface::of_button(id) {
            self.preview_surface = surface;
            self.invalidate();
        } else if let Some(field) = designer_field_for_button(id) {
            self.toggle_designer_field(field);
        } else if let Some((line, preset)) = card_preset_for_button(id) {
            debug_assert_eq!(line, 1, "only the State row offers presets");
            self.on_select_card_preset(&preset);
        }
        None
    }

    /// The focus stops of the current view. The Card view's list grows with its
    /// disclosure AND with the results list, so Tab reaches every action that is
    /// on screen -- Apply and the advanced-match control included, whether or
    /// not `Advanced` happens to be open.
    #[must_use]
    fn focus_order_now(&self) -> Vec<i32> {
        if self.cur_view == View::Card {
            let state = self.card_layout_state();
            let mut order =
                card_focus_order_for(state.advanced, state.has_selection, state.can_apply);
            if !state.playing {
                order.retain(|&id| id != BTN_FIX_MATCH);
            }
            order
        } else {
            focus_order_for_view(self.cur_view)
        }
    }

    /// Focus the one title/year field. The field is the only text control on
    /// the Card workspace; clicks and Tab both land here.
    pub fn focus_picker_field(&mut self) {        self.focus_id = BTN_PICKER_FIELD;
        self.invalidate();
    }

    /// Type one character into the picker field (WM_CHAR). Backspace removes
    /// the last character. The query is the single canonical input the
    /// provider search and the dialog both read. The field is capped at
    /// [`crate::search::MAX_QUERY_CHARS`] scalar values: a refused character
    /// says why instead of being silently dropped.
    pub fn on_card_char(&mut self, ch: u32) -> bool {
        if self.cur_view != View::Card
            || self.focus_id != BTN_PICKER_FIELD
            || self.picker.busy
        {
            return false;
        }
        if ch == 8 {
            let mut q = self.picker.query.clone();
            q.pop();
            self.picker.set_query(q);
            self.invalidate();
            return true;
        }
        let Some(c) = char::from_u32(ch) else {
            return false;
        };
        if c.is_control() {
            return false;
        }
        if !self.picker.accepts_more() {
            self.picker.status = crate::search::QueryIssue::TooLong.message().to_string();
            self.invalidate();
            return false;
        }
        let mut q = self.picker.query.clone();
        q.push(c);
        self.picker.set_query(q);
        self.invalidate();
        true
    }

    /// Insert a whole string into the query (the paste path). The same cap and
    /// the same refusal copy as the typed path apply, and the query stops at
    /// the cap rather than accepting a truncated-and-different title silently:
    /// the field shows what it kept, the counter shows the limit, and the status
    /// line says the paste was clipped.
    pub fn paste_into_picker(&mut self, text: &str) -> bool {
        if self.cur_view != View::Card || self.focus_id != BTN_PICKER_FIELD || self.picker.busy {
            return false;
        }
        let typed: String = text.chars().filter(|c| !c.is_control()).collect();
        if typed.is_empty() {
            return false;
        }
        let mut q = self.picker.query.clone();
        let mut clipped = false;
        for c in typed.chars() {
            if q.chars().count() >= crate::search::MAX_QUERY_CHARS {
                clipped = true;
                break;
            }
            q.push(c);
        }
        self.picker.set_query(q);
        if clipped {
            self.picker.status = crate::search::QueryIssue::TooLong.message().to_string();
        }
        self.invalidate();
        true
    }

    /// Apply one search result to the LOCAL preview: the selected record's own
    /// title, year, kind and poster compose the card through the same composer
    /// the send path uses. Nothing is sent, no Discord activity is written, and
    /// no override is stored -- the object card says `Preview only` in words.
    pub fn select_candidate(&mut self, idx: usize) -> bool {
        let Some(cand) = self.picker.candidates.get(idx).cloned() else {
            return false;
        };
        self.picker.selected = idx;
        // The viewport follows the selection, so a selected row is always the
        // row the paint shows highlighted and the click resolves.
        self.picker.top = idx.min(self.picker.candidates.len().saturating_sub(1));
        // The pick lives in the picker model, so every query edit, scope change
        // or new search clears it with the result set it belonged to.
        self.picker.pick_row(picked_row_key(&cand));
        self.set_local_preview(&cand);
        self.invalidate();
        true
    }

    /// Compose the local preview from one resolved record. A movie candidate
    /// composes a movie card, a series candidate a series card: the record's
    /// kind owns the preview, so a series can never inherit a stale `Movie`.
    fn set_local_preview(&mut self, cand: &Candidate) {
        let is_movie = crate::search::matched_kind_is_movie(cand);
        let kind = if is_movie {
            CardKind::Movie
        } else {
            CardKind::Series
        };
        let raw_title = if cand.year > 0 {
            format!("{} ({})", cand.title.trim(), cand.year)
        } else {
            cand.title.trim().to_string()
        };
        let resolved = Resolved {
            series: cand.title.trim().to_string(),
            year: cand.year,
            is_movie,
            artwork: cand.poster.clone(),
            synopsis: cand.synopsis.clone(),
            genres: cand.genres.clone(),
            length: cand.length.clone(),
            ..Resolved::default()
        };
        let anime = crate::search::is_anime(cand);
        self.local_preview = Some((
            kind,
            CardPreviewInput {
                raw_title,
                action: None,
                resolved,
                anime,
            },
        ));
    }

    /// Run the one search over the existing provider pipeline. The worker
    /// streams each batch into `picker_slot` and posts
    /// [`WM_APP_PICKER_DONE`] per batch; the UI thread never blocks on the
    /// network.
    ///
    /// The persistent cache decides the first move: a complete entry is shown
    /// with the cached status and NO network; a partial entry is shown at once
    /// and only its failed providers are retried. A yearless entry is shown as
    /// soon as it exists, narrowed to the typed year, so `edgerunners 2026`
    /// paints instantly. The streamed result stays authoritative and replaces
    /// what is on screen as it lands.
    pub fn card_picker_search(&mut self) -> bool {
        let Some((generation, title, year, filter)) = self.picker.begin_search() else {
            // Either busy (already says so) or the query itself is refused:
            // say which, in the field's own line.
            if !self.picker.busy
                && let Some(issue) = crate::search::query_issue(&self.picker.query)
            {
                self.picker.status = issue.message().to_string();
                self.invalidate();
            }
            return false;
        };
        self.picker_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let key = crate::search::search_cache_key(&title, year, filter);
        let cached = if self.cache_enabled {
            cached_search(&key)
        } else {
            None
        };
        let mut only: Vec<String> = Vec::new();
        if let Some(cached) = &cached {
            let rows = rows_for_year(cached.rows.clone(), year);
            if !rows.is_empty() {
                self.picker.push_search(generation, rows);
            }
            if cached.failed.is_empty() && !cached.rows.is_empty() {
                app_logf(&format!(
                    "picker: cache hit \"{title}\" ({} rows, 0 failed)",
                    cached.rows.len()
                ));
                self.picker.finish_search_now(generation);
                // Approved copy: no "(cached)" marker on the Card.
                self.invalidate();
                return true;
            }
            only = cached.failed.clone();
        } else if self.cache_enabled && year > 0 {
            // A yearless entry still paints the typed year's rows at once.
            let yearless = crate::search::search_cache_key(&title, 0, filter);
            if let Some(cached) = cached_search(&yearless) {
                let rows = rows_for_year(cached.rows, year);
                if !rows.is_empty() {
                    self.picker.push_search(generation, rows);
                }
            }
        }
        let slot = self.picker_slot.clone();
        let search = self.picker_search.clone();
        let hwnd = self.main_hwnd;
        self.invalidate();
        std::thread::spawn(move || {
            let push = |out: FixOutcome| {
                slot.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((generation, out));
                if hwnd != 0 {
                    // SAFETY: async post only; the worker touches no window state.
                    unsafe {
                        crate::win32::PostMessageW(hwnd as HWND, WM_APP_PICKER_DONE, 0, 0);
                    }
                }
            };
            let emit = |rows: Vec<metadata::Candidate>| push(FixOutcome::Search(rows));
            let out = match search(
                &title,
                year,
                filter,
                &only,
                &emit as &(dyn Fn(Vec<metadata::Candidate>) + Send + Sync),
            ) {
                // The finish repaints the final merged answer over the last
                // batch: the painted list and the stored cache entry are the
                // same rows in the same order, not the arrival order of the
                // last leg to answer.
                Ok(report) => {
                    if !report.rows.is_empty() {
                        push(FixOutcome::Search(report.rows));
                    }
                    FixOutcome::SearchDone
                }
                Err(err) => {
                    app_logf(&format!("picker: search failed: {err}"));
                    FixOutcome::Failed
                }
            };
            push(out);
        });
        true
    }

    /// Whether the highlighted row is one the user picked. The search seeds
    /// `selected = 0`, so a row is highlighted before any choice; that auto
    /// highlight is not a choice and must not arm Apply. The pick itself is
    /// cleared by the picker on every query edit, scope change and new search,
    /// so an identical record coming back in a fresh list is still not a pick.
    #[must_use]
    pub fn has_picked_candidate(&self) -> bool {
        if !self.picker.open {
            // Nothing on screen can be a choice: the list is closed.
            return false;
        }
        match (self.picker.selected_candidate(), self.picker.picked.as_deref()) {
            (Some(cand), Some(picked)) => picked_row_key(cand) == picked,
            _ => false,
        }
    }

    /// Pin the selected picker candidate through the same provisional + full
    /// pin path the dialog uses. The explicit Movie/TV choice owns the target:
    /// a movie latches its file, TV latches the default season/show scope.
    ///
    /// A picked row is a target even while the search still streams: the list
    /// is filled as batches arrive, so Apply must not wait for the job to
    /// finish. The latch then advances the generation, which makes every later
    /// batch of that search stale -- a straggler can never repopulate the list
    /// over the latch that just started.
    pub fn card_picker_match(&mut self) -> bool {
        if !self.has_picked_candidate() {
            if self.picker.busy {
                // Nothing picked yet and the job is running: the old wait.
                return false;
            }
            // A row the user never picked is not a target, even if it is
            // highlighted by the search's initial index.
            self.picker.status = "Pick a result row first.".to_string();
            self.invalidate();
            return false;
        }
        let Some(cand) = self.picker.selected_candidate().cloned() else {
            self.picker.status = "Run a search and pick a result first.".to_string();
            self.invalidate();
            return false;
        };
        let Some(now) = app_support::config::live_now_playing() else {
            self.picker.status = FIX_MATCH_IDLE_REASON.to_string();
            self.invalidate();
            return false;
        };
        let path = now.path;
        let matched_movie = crate::search::matched_kind_is_movie(&cand);
        // The matched record's own kind owns the latch target: a series is
        // never latched as a movie because the search was seeded the other way.
        let (scope, key) = if matched_movie {
            let key = metadata::overrides::movie_key(&path);
            if key.is_empty() {
                self.picker.status = FIX_MATCH_NO_TARGET_REASON.to_string();
                self.invalidate();
                return false;
            }
            (Scope::Movie, key)
        } else {
            let choices = fix_match_scope_choices(&path, false);
            let Some(choice) = choices
                .iter()
                .find(|c| c.default)
                .or_else(|| choices.first())
            else {
                self.picker.status = fix_match_no_target_reason(&path, false)
                    .unwrap_or(FIX_MATCH_NO_TARGET_REASON)
                    .to_string();
                self.invalidate();
                return false;
            };
            (choice.scope, choice.key.clone())
        };
        match fix_match_save_provisional(&cand, scope, &key, &path) {
            Ok(_) => {
                self.picker.busy = true;
                self.picker.open = false;
                self.picker.status = FIX_MATCH_FETCHING_STATUS.to_string();
                self.picker_path = path.clone();
                self.picker.generation += 1;
                let generation = self.picker.generation;
                // Every batch of the search that was still streaming carries
                // the old generation, so it is stale from here on; the queue is
                // cleared so none of it is even applied.
                self.picker_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
                let slot = self.picker_slot.clone();
                let hwnd = self.main_hwnd;
                self.invalidate();
                std::thread::spawn(move || {
                    let out = fix_match_worker(FixJob::Latch {
                        cand,
                        scope,
                        key,
                        path,
                    });
                    slot.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((generation, out));
                    if hwnd != 0 {
                        // SAFETY: async post only; the worker touches no window state.
                        unsafe {
                            crate::win32::PostMessageW(hwnd as HWND, WM_APP_PICKER_DONE, 0, 0);
                        }
                    }
                });
                true
            }
            Err(err) => {
                app_logf(&format!("picker: provisional save failed: {err}"));
                self.picker.status = FIX_MATCH_ERROR.to_string();
                self.invalidate();
                false
            }
        }
    }

    /// Apply one worker outcome. A stale generation is ignored, so an older
    /// search can never replace a newer one; one wakeup drains every batch that
    /// has landed, so a burst costs one repaint.
    pub fn on_picker_job_done(&mut self) {
        let batch: Vec<(u64, FixOutcome)> = std::mem::take(
            &mut *self
                .picker_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        if batch.is_empty() {
            return;
        }
        let mut changed = false;
        for (generation, out) in batch {
            if generation != self.picker.generation {
                continue;
            }
            match out {
                FixOutcome::Search(rows) => {
                    changed |= self.picker.push_search(generation, rows);
                }
                FixOutcome::SearchDone => {
                    changed |= self.picker.finish_search_now(generation);
                }
                FixOutcome::Latched(status) | FixOutcome::ProvisionalKept(status) => {
                    self.picker.busy = false;
                    self.picker.status = status;
                    changed = true;
                }
                FixOutcome::Failed => {
                    changed |= self.picker.fail_search(generation);
                }
            }
        }
        if changed {
            self.invalidate();
        }
    }

    /// Flip one designer field and publish it to the live mirror the sender
    /// reads. The preview recomposes through the same mirror, so the panel and
    /// the next wire frame change together. A save is not involved: the
    /// designer choice is a live view setting, not stored card config.
    pub fn toggle_designer_field(&mut self, field: DesignerField) {
        let mut spec = self.designer;
        let on = match field {
            DesignerField::Details => {
                spec.details = !spec.details;
                spec.details
            }
            DesignerField::State => {
                spec.state = !spec.state;
                spec.state
            }
            DesignerField::Synopsis => {
                spec.synopsis = !spec.synopsis;
                spec.synopsis
            }
            DesignerField::Timers => {
                spec.timers = !spec.timers;
                spec.timers
            }
        };
        self.designer = spec;
        presence::set_live_designer(spec);
        self.set_status_text(format!(
            "{} {}",
            designer_field_label(field),
            if on { "on" } else { "off" }
        ));
        self.invalidate();
    }

    /// `activateFocused` (`go\window.go:1328`).
    ///
    /// Returns a [`BlockingUi`] the wndproc must run with `UI` RELEASED.
    #[must_use]
    pub fn activate_focused(&mut self, hwnd: HWND, id: i32) -> Option<BlockingUi> {
        self.focus_id = id;
        if self.switch_tab(id) {
            return None;
        }
        match self.cur_view {
            View::Settings => self.do_settings_action(hwnd, id),
            View::Card => self.do_card_action(hwnd, id),
            View::Main => self.do_button_action(hwnd, id),
        }
    }

    /// `onSettingsKeyDown` (`go\window.go:1353`).
    pub fn on_settings_key_down(&mut self, hwnd: HWND, vk: u32) -> bool {
        self.on_settings_key_down_ex(hwnd, vk, shift_held())
    }

    /// `onSettingsKeyDownEx` (`go\window.go:1358`).
    pub fn on_settings_key_down_ex(&mut self, hwnd: HWND, vk: u32, shift: bool) -> bool {
        // The Card picker owns its text-entry keys before the generic focus
        // travel. Backspace arrives as WM_CHAR; Enter searches (or matches an
        // open result); Escape closes the result overlay; Up/Down move the
        // result selection while it is open.
        if self.cur_view == View::Card {
            if self.picker.open && vk == VK_ESCAPE {
                self.picker.close_results();
                return true;
            }
            if self.focus_id == BTN_PICKER_FIELD && vk == VK_RETURN {
                // Enter on the field is the one search verb; while results are
                // open it previews the highlighted row instead of running a
                // second, differently-named action.
                if self.picker.open {
                    let idx = self.picker.selected;
                    self.select_candidate(idx);
                } else {
                    self.card_picker_search();
                }
                return true;
            }
            if self.picker.open && matches!(vk, VK_UP | VK_DOWN) {
                let n = self.picker.candidates.len();
                if n > 0 {
                    let next = if vk == VK_UP {
                        self.picker.selected.saturating_sub(1)
                    } else {
                        (self.picker.selected + 1).min(n - 1)
                    };
                    // Moving the highlight previews the row it lands on: the
                    // keyboard and the mouse select the same way. The viewport
                    // follows the selection (`select_candidate` owns `top`).
                    self.select_candidate(next);
                    self.reveal_picker_selected(hwnd);
                    self.invalidate();
                }
                return true;
            }
            if self.picker.open && matches!(vk, VK_PRIOR | VK_NEXT | VK_HOME | VK_END) {
                let n = self.picker.candidates.len();
                if n > 0 {
                    let step = self
                        .layout_card(hwnd)
                        .map(|l| picker_capacity(&l, window_dpi(hwnd)).max(1) as i32)
                        .unwrap_or(1);
                    let next = match vk {
                        VK_PRIOR => self.picker.selected.saturating_sub(step as usize),
                        VK_NEXT => (self.picker.selected + step as usize).min(n - 1),
                        VK_HOME => 0,
                        _ => n - 1,
                    };
                    self.select_candidate(next);
                    self.reveal_picker_selected(hwnd);
                    self.invalidate();
                }
                return true;
            }
        }
        // The Main folder list owns its navigation keys while it has rows;
        // every other key keeps the ported focus/scroll/activate behaviour.
        if self.on_main_list_key(hwnd, vk) {
            return true;
        }
        match vk {
            VK_ESCAPE => {
                if self.cur_view == View::Card {
                    self.close_card();
                    return true;
                }
                if self.cur_view != View::Settings {
                    return false;
                }
                self.enter_main();
                true
            }
            VK_TAB => {
                let dir = if shift { -1 } else { 1 };
                self.focus_id =
                    move_focus(self.focus_id, dir, &self.focus_order_now());
                if self.cur_view == View::Settings {
                    self.reveal_settings_focused(hwnd);
                    self.invalidate();
                }
                true
            }
            VK_LEFT | VK_UP => {
                self.focus_id = move_focus(self.focus_id, -1, &self.focus_order_now());
                if self.cur_view == View::Settings {
                    self.reveal_settings_focused(hwnd);
                    self.invalidate();
                }
                true
            }
            VK_PRIOR => {
                if self.cur_view == View::Settings {
                    self.scroll_settings(hwnd, -self.settings_page_px(hwnd))
                } else {
                    self.scroll_list(hwnd, -self.page_rows(hwnd))
                }
            }
            VK_NEXT => {
                if self.cur_view == View::Settings {
                    self.scroll_settings(hwnd, self.settings_page_px(hwnd))
                } else {
                    self.scroll_list(hwnd, self.page_rows(hwnd))
                }
            }
            VK_HOME => {
                if self.cur_view == View::Settings {
                    self.scroll_settings(hwnd, -i32::MAX)
                } else {
                    self.scroll_list(hwnd, -i32::MAX)
                }
            }
            VK_END => {
                if self.cur_view == View::Settings {
                    self.scroll_settings(hwnd, i32::MAX)
                } else {
                    self.scroll_list(hwnd, i32::MAX)
                }
            }
            VK_RIGHT | VK_DOWN => {
                self.focus_id = move_focus(self.focus_id, 1, &self.focus_order_now());
                if self.cur_view == View::Settings {
                    self.reveal_settings_focused(hwnd);
                    self.invalidate();
                }
                true
            }
            VK_RETURN | VK_SPACE => {
                if self.focus_id == 0 {
                    return false;
                }
                let id = self.focus_id;
                // Stash the blocking action for the wndproc to run with `UI`
                // RELEASED (`run_blocking_ui`); never run it under this guard.
                self.pending_blocking = self.activate_focused(hwnd, id);
                true
            }
            _ => false,
        }
    }

    /// `refreshDirList` (`go\window.go:2770`).
    pub fn refresh_dir_list(&mut self) {
        self.ui_dirs = app_support::config::load_config().dirs;
        let n = self.ui_dirs.len() as i32;
        // A shrunk list must never leave the selection pointing past its end.
        self.sel_row = clamp_sel_row(self.sel_row, self.ui_dirs.len());
        if self.hover_row >= n {
            self.hover_row = -1;
            self.hover_x = false;
        }
        self.invalidate();
    }

    /// `removeDirAt` (`go\window.go:1788`).
    pub fn remove_dir_at(&mut self, idx: i32) {
        if idx < 0 || (idx as usize) >= self.ui_dirs.len() {
            self.set_status_text("Select a folder first.");
            return;
        }
        let Some(mut cfg) = self.load_config_for_write("config: load failed") else {
            return;
        };
        if !cfg.remove_dir(&self.ui_dirs[idx as usize]) {
            return;
        }
        if let Err(e) = app_support::config::save_config_to(&self.cfg_path(), &cfg) {
            app_logf(&format!("config: save failed: {e}"));
            return;
        }
        self.refresh_dir_list();
        app_logf(&format!("config: dir removed (count={})", cfg.dirs.len()));
    }

    /// `onRemoveSelected` (`go\window.go:1777`).
    pub fn on_remove_selected(&mut self) {
        let mut idx = self.sel_row;
        if idx < 0 {
            idx = self.hover_row;
        }
        self.remove_dir_at(idx);
    }

    /// `onAddDir` (`go\window.go:1757`), post-dialog half only: the modal
    /// `browse_for_folder` runs OUTSIDE the `UI` guard in `run_blocking_ui`.
    fn apply_add_dir(&mut self, path: &str) {
        let Some(mut cfg) = self.load_config_for_write("config: load failed") else {
            return;
        };
        if !cfg.add_dir(path) {
            app_logf(&format!(
                "config: dir already watched (count={})",
                cfg.dirs.len()
            ));
            return;
        }
        if let Err(e) = app_support::config::save_config_to(&self.cfg_path(), &cfg) {
            app_logf(&format!("config: save failed: {e}"));
            return;
        }
        self.refresh_dir_list();
        app_logf(&format!("config: dir added (count={})", cfg.dirs.len()));
    }

    /// `recordPreview` (`go\main.go:60`): records what a preview needs (the
    /// composer inputs) under `kind`, not the finished payload. The preview
    /// re-composes on demand through the same composer the send path uses.
    /// Recording also derives the displayed kind from the live push and
    /// repaints the Card view. Callers record only after a successful send.
    pub fn record_preview(
        &mut self,
        kind: CardKind,
        raw_title: String,
        action: Option<Action>,
        resolved: Resolved,
    ) {
        let input = CardPreviewInput {
            raw_title,
            action,
            resolved,
            anime: false,
        };
        match kind {
            CardKind::Series => self.preview_series = Some(input),
            CardKind::Movie => self.preview_movie = Some(input),
        }
        self.preview_kind = Some(kind);
        self.invalidate();
    }

    /// The kind the Card view displays and edits: the live-recorded kind, else
    /// the kind of the locally previewed selection, else Series.
    #[must_use]
    pub fn displayed_card_kind(&self) -> CardKind {
        if let Some(kind) = self.preview_kind {
            return kind;
        }
        if let Some((kind, _)) = self.local_preview.as_ref() {
            return *kind;
        }
        CardKind::Series
    }

    /// Whether the object card is showing a real, sent presence (a live push)
    /// rather than a local preview or the empty state. The object card's status
    /// line and the review both read this one predicate.
    #[must_use]
    pub fn preview_is_live(&self) -> bool {
        if self.local_preview.is_some() {
            return false;
        }
        self.preview_kind.is_some()
            && match self.preview_kind {
                Some(CardKind::Series) => self.preview_series.is_some(),
                Some(CardKind::Movie) => self.preview_movie.is_some(),
                None => false,
            }
    }

    /// The preview this window is showing: the record the user just picked
    /// first, else the last live push, else nothing. A pick is the newest
    /// intent and must not be masked by a stale live push for a different
    /// title; the status line below says which one is on screen.
    fn shown_preview(&self) -> Option<(CardKind, &CardPreviewInput)> {
        if let Some((kind, input)) = self.local_preview.as_ref() {
            return Some((*kind, input));
        }
        if self.preview_is_live() {
            let kind = self.preview_kind?;
            let input = match kind {
                CardKind::Series => self.preview_series.as_ref(),
                CardKind::Movie => self.preview_movie.as_ref(),
            }?;
            return Some((kind, input));
        }
        None
    }

    /// The object card's status line: the one sentence that says whether what
    /// is below is a real presence or a preview that has not been sent.
    #[must_use]
    pub fn object_status_text(&self, playing: bool) -> String {
        if self.preview_is_live() {
            return "Playing in PotPlayer".to_string();
        }
        if self.local_preview.is_some() {
            // One true sentence, whatever PotPlayer is doing: this preview has
            // not been sent. The old "nothing playing" claimed playback was
            // idle even while a title played, which the neighbouring previews
            // contradicted in the same frame.
            let _ = playing;
            return LOCAL_PREVIEW_STATUS.to_string();
        }
        "Preview is local; Apply needs playback.".to_string()
    }

    /// The one object this window is about: the resolved record's own title,
    /// year and kind for the playing title. `None` when nothing has been
    /// recorded, which is the explicit empty state -- not an empty search.
    #[must_use]
    pub fn preview_object(&self) -> Option<(String, i32, bool)> {
        let (kind, input) = self.shown_preview()?;
        let title = if input.resolved.series.trim().is_empty() {
            input.raw_title.clone()
        } else {
            input.resolved.series.clone()
        };
        Some((title, input.resolved.year, kind.is_movie()))
    }

    /// The narrow hero meta for the shown preview, derived from the record:
    /// movies show the runtime, series show the episode code, and a Film never
    /// carries an `SxxExx` marker. `None` when there is no preview (the hero
    /// paints its empty copy instead).
    #[must_use]
    pub fn preview_hero_meta(&self) -> Option<(String, String)> {
        let (kind, input) = self.shown_preview()?;
        let title = if input.resolved.series.trim().is_empty() {
            input.raw_title.clone()
        } else {
            input.resolved.series.clone()
        };
        let r = &input.resolved;
        let meta = hero_meta_for(
            r.year,
            kind.is_movie(),
            input.anime,
            &r.length,
            r.season,
            r.number,
            r.absolute,
        );
        Some((title, meta))
    }

    /// `previewActivity` (`go\main.go:68`): composes on demand for the shown
    /// preview (live push first, else the locally selected record). Returns
    /// `(default, false)` when neither exists.
    #[must_use]
    pub fn preview_activity(&self) -> (Activity, bool) {
        let Some((kind, input)) = self.shown_preview() else {
            return (Activity::default(), false);
        };
        let card = presence::compose_card(
            &input.raw_title,
            input.action.as_ref(),
            &input.resolved,
            kind.is_movie(),
            &self.card_specs_state.for_kind(kind),
        );
        // One source of truth: the preview applies the same designer choices
        // the UI publishes to the sender, so the panel and the wire cannot
        // drift.
        (self.designer.apply(card), true)
    }

    /// The complete stored synopsis for the displayed kind, or empty when
    /// nothing is recorded. This is the local tooltip's source; `large_text`
    /// is only the <=128-unit Discord snippet.
    #[must_use]
    pub fn preview_synopsis(&self) -> String {
        self.shown_preview()
            .map(|(_, i)| i.resolved.synopsis.clone())
            .unwrap_or_default()
    }

    /// The resolved runtime string for the displayed kind (`2h 46m`,
    /// `45 min`), used by the preview time strip. Empty when nothing is
    /// recorded or the lookup carried no runtime.
    #[must_use]
    pub fn preview_length(&self) -> String {
        self.shown_preview()
            .map(|(_, i)| i.resolved.length.clone())
            .unwrap_or_default()
    }

    /// Whether the Card view's synopsis tooltip should be painted: hovered,
    /// keyboard-focused, or pinned by Enter/click.
    #[must_use]
    pub fn synopsis_visible(&self) -> bool {
        self.synopsis_pinned
            || self.hover_btn == BTN_SYNOPSIS
            || self.focus_id == BTN_SYNOPSIS
    }

    /// Probe-compat for the removed manual kind toggle: drives the derived
    /// preview kind directly. No UI writes here — the Card view has no kind
    /// toggle any more, and production sets the kind only through
    /// [`UiState::record_preview`]. The app-crate preview probes still sweep
    /// both slots through this; it goes when their update lands.
    pub fn set_card_kind(&mut self, kind: CardKind) {
        if self.preview_kind != Some(kind) {
            self.preview_kind = Some(kind);
            self.invalidate();
        }
    }

    /// `onSelectCardPreset` (`go\window.go:1679`): writes the State preset to
    /// the DISPLAYED kind — the live-recorded kind, Series while nothing live
    /// has been recorded. The Card view offers no Details button (line 1 is
    /// fixed by the shipped mapping; stored retired ids migrate at the config
    /// seam via `migrated()`), so this is the State-only write path.
    pub fn on_select_card_preset(&mut self, preset: &str) {
        let kind = self.displayed_card_kind();
        let Some(mut cfg) = self.load_config_for_write("card: load failed") else {
            return;
        };
        if !cfg.set_card_line(kind, CardLine::State, preset) {
            self.card_specs_state = cfg.effective_card_specs();
            self.invalidate();
            return;
        }
        if let Some(p) = self.test_config_path.clone() {
            if let Err(e) = app_support::config::save_config_to(&p, &cfg) {
                app_logf(&format!("card: save failed: {e}"));
                self.set_status_text("Save failed.");
                return;
            }
        } else if let Err(e) = app_support::config::save_config(&cfg) {
            app_logf(&format!("card: save failed: {e}"));
            self.set_status_text("Save failed.");
            return;
        }
        self.card_specs_state = cfg.effective_card_specs();
        // The sender composes with the mirror at its next poll, so the new
        // preset takes effect with no restart. A failed save publishes
        // nothing: the mirror keeps the last good specs.
        app_support::config::set_live_card_specs(self.card_specs_state.clone());
        let cur = self.card_specs_state.for_kind(kind);
        let (_, states) = presence::cardspec::card_presets();
        let mut name = preset.to_string();
        for p in &states {
            if p.id == cur.state {
                name = p.label.clone();
            }
        }
        self.set_status_text(format!("Line 2: {name}"));
        app_logf(&format!("card: line 2 -> {preset}"));
    }

    /// `onSelectScanSpeed` (`go\settings_sections.go:177`). Persistence and
    /// live rate-profile application live in the settings seam; this method
    /// keeps the view's own state, status line, and log.
    /// The config file this instance persists through (test override or owner store).
    #[must_use]
    pub fn cfg_path(&self) -> std::path::PathBuf {
        self.test_config_path
            .clone()
            .unwrap_or_else(app_support::config::config_path)
    }

    /// Loads the store for a write: a missing file yields defaults, but an
    /// unparsable or unreadable file (already quarantined by the load) sets
    /// the unreadable status and yields `None` so the caller refuses instead
    /// of saving defaults over the user's data.
    fn load_config_for_write(&mut self, log_scope: &str) -> Option<app_support::config::Config> {
        match app_support::config::load_config_for_update(&self.cfg_path()) {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                app_logf(&format!("{log_scope}: {e}"));
                self.set_status_text("Settings store unreadable - nothing saved.");
                None
            }
        }
    }

    /// Flip the Discord-connection pill. Pure memory: the paint reads this
    /// same field, so the pill and its click agree.
    pub fn on_toggle_discord(&mut self) {
        self.discord_enabled = !self.discord_enabled;
        self.set_status_text(if self.discord_enabled {
            "Discord connection: on"
        } else {
            "Discord connection: off"
        });
    }

    /// Flip the Close-to-tray pill (Settings view only; Main's Close still
    /// hides via `BlockingUi::HideWindow`).
    pub fn on_toggle_close_to_tray(&mut self) {
        self.close_to_tray = !self.close_to_tray;
        self.set_status_text(if self.close_to_tray {
            "Close to tray: on"
        } else {
            "Close to tray: off"
        });
    }

    pub fn on_select_scan_speed(&mut self, profile: &str) {
        let profile = app_support::config::normalize_scan_speed(profile);
        self.scan_speed_state = profile.to_string();
        match crate::settings::on_select_scan_speed(&self.cfg_path(), profile) {
            Ok(status) => {
                self.set_status_text(status);
                app_logf(&format!("settings: scan speed {profile}"));
            }
            Err(e) => {
                app_logf(&format!("settings: scan speed save failed: {e}"));
                self.set_status_text("Save failed.");
            }
        }
    }

    /// `onSelectTheme`: persist the choice, publish it live, and repaint
    /// immediately. It never touches the search cache or match state — only
    /// the `theme` key moves.
    pub fn on_select_theme(&mut self, theme: &str) {
        let theme = app_support::config::normalize_theme(theme);
        self.theme_state = theme.to_string();
        set_live_theme(theme);
        match crate::settings::on_select_theme(&self.cfg_path(), theme) {
            Ok(status) => {
                self.set_status_text(status);
                app_logf(&format!("settings: theme {theme}"));
            }
            Err(e) => {
                app_logf(&format!("settings: theme save failed: {e}"));
                self.set_status_text("Save failed.");
            }
        }
        self.invalidate();
    }

    /// `resetScanSnapshot` (`go\window.go:2447`).
    pub fn reset_scan_snapshot(&mut self) {
        self.scan_progress = Progress::default();
        self.scan_resolve_start = None;
        self.scan_resolve_base = 0;
    }

    /// Publish the worker's progress snapshot onto the UI state. The
    /// `WM_APP_SCAN_PROGRESS` arm runs this before `refresh_scan_live_text`,
    /// exactly as Go's arm reads the mutex-guarded package var.
    pub fn publish_scan_progress(&mut self) {
        self.scan_progress = self
            .scan_shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .progress
            .clone();
    }

    /// Publish the worker's terminal snapshot onto the UI state. The
    /// `WM_APP_SCAN_DONE` arm runs this before `on_scan_done`, exactly as Go's
    /// arm reads the mutex-guarded package vars.
    pub fn publish_scan_done(&mut self) {
        let snap = self
            .scan_shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.scan_stats = snap.stats;
        self.scan_err = snap.err;
    }

    /// `beginScanRun` (`go\window.go:2598`): the UI-thread half of scan start.
    pub fn begin_scan_run(
        &mut self,
        hwnd: HWND,
        force: bool,
        now: Instant,
    ) -> Option<library::Cancel> {
        let (st, fx) = scan_lifecycle_next(
            ScanLifecycle {
                active: self.scan_active,
                last_paint: self.scan_last_paint,
            },
            ScanLifecycleEvent::Start,
            now,
        );
        if !fx.arm {
            return None;
        }
        self.scan_active = true;
        let cancel = library::Cancel::background();
        self.scan_cancel = Some(cancel.clone());
        self.scan_start = now;
        self.scan_last_paint = st.last_paint;
        self.reset_scan_snapshot();
        self.scan_force = force;
        self.invalidate();
        let kind = if force { "Refresh" } else { "Scan" };
        if fx.paint {
            self.set_status_text(format!("{kind} starting..."));
        }
        (self.arm_timer)(hwnd);
        Some(cancel)
    }

    /// `refreshScanLiveText` (`go\window.go:2459`).
    pub fn refresh_scan_live_text(&mut self, hwnd: HWND, ev: ScanLifecycleEvent, now: Instant) {
        if self.scan_active && self.scan_cancel.is_none() {
            return;
        }
        let (st, fx) = scan_lifecycle_next(
            ScanLifecycle {
                active: self.scan_active,
                last_paint: self.scan_last_paint,
            },
            ev,
            now,
        );
        if !fx.paint {
            return;
        }
        self.scan_last_paint = st.last_paint;
        let p = self.scan_progress.clone();
        if self.scan_resolve_start.is_none() && p.keys_total > 0 {
            self.scan_resolve_start = Some(now);
            self.scan_resolve_base = scan_observed_done(&p);
        }
        let text = scan_live_status_text(
            &p,
            now.saturating_duration_since(self.scan_start),
            scan_resolve_elapsed(self.scan_resolve_start, now),
            self.scan_resolve_base,
            window_dpi(hwnd),
            live_status_avail_px(hwnd),
        );
        self.set_status_text(text);
    }

    /// `onScanDone` (`go\window.go:2716`).
    pub fn on_scan_done(&mut self, hwnd: HWND) {
        let (_, fx) = scan_lifecycle_next(
            ScanLifecycle {
                active: self.scan_active,
                last_paint: self.scan_last_paint,
            },
            ScanLifecycleEvent::Done,
            Instant::now(),
        );
        if fx.kill {
            (self.kill_timer)(hwnd);
        }
        let stats = self.scan_stats;
        let err = self.scan_err.clone();
        let force = self.scan_force;
        self.scan_active = false;
        if let Some(c) = self.scan_cancel.take() {
            c.cancel();
        }
        self.invalidate();
        let lname = if force { "refresh" } else { "scan" };
        if !fx.terminal {
            app_logf(&format!("{lname}: stray done with no run, line untouched"));
            return;
        }
        let text = scan_terminal_text(force, &stats, err.as_ref());
        self.set_status_text(text);
        if stats.cancelled || err.as_ref().is_some_and(|e| e.cancelled) {
            app_logf(&format!(
                "{lname}: cancelled ({}/{} keys done, progress kept)",
                stats.keys_done, stats.keys
            ));
        } else if let Some(e) = &err {
            app_logf(&format!("{lname}: failed: {}", e.message));
        } else {
            // `onScanDone` (`go\window.go:2749`). The `unresolved-at-scan-time` field
            // is the per-FILE `stats.not_found`, not the per-KEY `keys_not_found`
            // the done-text gate above reads — the two agree only when a file
            // yields exactly one key (`scan.rs:180`/`183` carries a third meaning
            // under the same name: `not_found` = `files_pending`). The summary is
            // sampled before the async catch-up pass runs, so an honest nonzero
            // here can coexist with a later successful fetch. Deliberate
            // departure from Go's `not-found` field name, which reads as a
            // verdict; this one states the sampling instant.
            app_logf(&format!(
                "{lname}: complete files={} skipped={} unique={} resolved={} cached={} fetched={} unresolved-at-scan-time={} errors={} denied={} foldersScanned={} foldersSkipped={} filesSkipped={}",
                stats.files,
                stats.skipped,
                stats.unique,
                stats.resolved,
                stats.cached,
                stats.fetched,
                stats.not_found,
                stats.errors,
                stats.denied,
                stats.folders_scanned,
                stats.folders_skipped,
                stats.files_skipped
            ));
        }
    }

    /// `cancelLibraryScan` (`go\window.go:2682`).
    pub fn cancel_library_scan(&mut self) {
        if !self.scan_active || self.scan_cancel.is_none() {
            return;
        }
        if let Some(c) = self.scan_cancel.take() {
            c.cancel();
        }
        let p = self.scan_progress.clone();
        self.set_status_text(scan_cancelled_text(p.keys_done, p.keys_total));
        app_logf(&format!(
            "scan: cancel requested ({}/{} keys done)",
            p.keys_done, p.keys_total
        ));
    }

    /// `startLibraryScan` (`go\window.go:2642`).
    pub fn start_library_scan(&mut self, hwnd: HWND, force: bool) {
        if self.scan_active {
            return;
        }
        let dirs = (self.scan_dirs)();
        let lname = if force { "refresh" } else { "scan" };
        if dirs.is_empty() {
            self.set_status_text("No folders configured - Add folder first.");
            app_logf(&format!("{lname}: no dirs configured"));
            return;
        }
        let Some(cancel) = self.begin_scan_run(hwnd, force, Instant::now()) else {
            return;
        };
        app_logf(&format!(
            "{lname}: started (dirs={} force={force})",
            dirs.len()
        ));
        // Worker thread: it may never touch the UI nor lock the process-wide
        // `UI` mutex; it publishes a snapshot and posts async messages
        // (production wiring posts to `hwnd`).
        let cancel_clone = cancel.clone();
        let shared = self.scan_shared.clone();
        let run = self.scan_run.clone();
        let hwnd_raw = hwnd as usize;
        std::thread::spawn(move || {
            let shared_progress = shared.clone();
            let opts = build_scan_options(
                dirs,
                force,
                Some(Arc::new(move |p: Progress| {
                    shared_progress
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .progress = p;
                    if hwnd_raw != 0 {
                        // SAFETY: async post only; the worker never touches a control.
                        unsafe {
                            crate::win32::PostMessageW(
                                hwnd_raw as HWND,
                                WM_APP_SCAN_PROGRESS,
                                0,
                                0,
                            );
                        }
                    }
                })),
            );
            let outcome = run(&cancel_clone, opts);
            {
                let mut snap = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match outcome {
                    Ok(stats) => {
                        snap.stats = stats;
                        snap.err = None;
                    }
                    Err(e) => {
                        let message = e.message().to_string();
                        // `library::Error` carries only a message and no
                        // cancel flag, so a failed run is a cancel exactly
                        // when the cancel fired.
                        let cancelled = cancel_clone.is_cancelled();
                        snap.stats = Stats {
                            cancelled,
                            ..Stats::default()
                        };
                        snap.err = Some(if cancelled {
                            UiScanError::cancelled()
                        } else {
                            UiScanError::other(&message)
                        });
                    }
                }
            }
            // SAFETY: async post only; the worker never touches a control.
            unsafe {
                crate::win32::PostMessageW(hwnd_raw as HWND, WM_APP_SCAN_DONE, 0, 0);
            }
        });
    }

    /// `onReportAt` (`go\window.go:1147`).
    pub fn on_report_at(&mut self, hwnd: HWND, path: &std::path::Path) {
        let (r, read_err) = match repair::report::read_report(path) {
            Ok(r) => (r, false),
            Err(e) => {
                app_logf(&format!("report: read {}: {}", path.display(), e.message()));
                (repair::report::Report::default(), true)
            }
        };
        let (proceed, status) = report_decision(&r, read_err);
        if !proceed {
            self.set_status_text(status);
            return;
        }
        self.set_status_text(REPORT_PENDING_TEXT);
        let create = self.create_issue.clone();
        let open = self.open_url.clone();
        let done = self.report_done_text.clone();
        let report = r;
        let hwnd_raw = hwnd as usize;
        std::thread::spawn(move || {
            let (url, cerr) = create(&report);
            let text = report_issue_status(&url, cerr.as_ref(), &*open);
            *done
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
            if hwnd_raw != 0 {
                // SAFETY: async post only; the worker never touches a control.
                unsafe {
                    crate::win32::PostMessageW(hwnd_raw as HWND, WM_APP_REPORT_DONE, 0, 0);
                }
            }
        });
    }

    /// `onReportDone` (`go\window.go:1194`).
    pub fn on_report_done(&mut self) {
        let text = {
            let mut g = self
                .report_done_text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let t = g.clone();
            g.clear();
            t
        };
        if text.is_empty() {
            return;
        }
        self.set_status_text(text);
    }

    // -- settings handlers (`go\settings_sections.go:594-694`) -----------------

    /// `onClearMetadata` (`go\settings_sections.go:594`), post-confirm half only:
    /// the modal `confirm_clear` runs OUTSIDE the `UI` guard in `run_blocking_ui`.
    fn apply_clear_metadata(&mut self) {
        if let Err(e) = crate::settings::clear_metadata_cache(
            &crate::settings::metadata_dir(),
            &metadata::search_cache_path(),
        ) {
            app_logf(&format!("library: clear metadata failed: {}", e.message()));
            self.set_status_text(fit_scan_status(&format!(
                "Clear metadata failed: {}",
                e.message()
            )));
            return;
        }
        app_logf("library: cleared metadata cache");
        self.set_status_text(crate::settings::CLEAR_METADATA_STATUS);
    }

    /// `onClearIndex` (`go\settings_sections.go:611`), post-confirm half only:
    /// the modal `confirm_clear` runs OUTSIDE the `UI` guard in `run_blocking_ui`.
    fn apply_clear_index(&mut self) {
        let (records, err) = crate::settings::clear_scan_index(&crate::settings::scan_index_path());
        if let Err(e) = err {
            app_logf(&format!("library: clear index failed: {}", e.message()));
            self.set_status_text(fit_scan_status(&format!(
                "Clear index failed: {}",
                e.message()
            )));
            return;
        }
        app_logf(&format!(
            "library: cleared file index (folder records={records})"
        ));
        self.set_status_text(crate::settings::clear_index_status(records));
    }

    /// `onExportBundle` (`go\settings_sections.go:628`), post-dialog half only:
    /// the modal `save_bundle_dialog` runs OUTSIDE the `UI` guard in
    /// `run_blocking_ui`.
    fn apply_export_bundle(&mut self, dest: &str) {
        match crate::settings::export_bundle_to(
            &crate::settings::metadata_dir(),
            dest,
            metadata::cache::CACHE_SCHEMA,
            crate::settings::APP_VERSION,
        ) {
            Ok(n) => {
                app_logf(&format!("library: exported (entries={n})"));
                self.set_status_text(crate::settings::export_bundle_status(n, dest));
            }
            Err(e) => {
                app_logf(&format!("library: export failed: {}", e.message()));
                self.set_status_text(fit_scan_status(&format!("Export failed: {}", e.message())));
            }
        }
    }

    /// `onImportBundle` (`go\settings_sections.go:647`), post-dialog half only:
    /// the modal `open_bundle_dialog` runs OUTSIDE the `UI` guard in
    /// `run_blocking_ui`.
    fn apply_import_bundle(&mut self, hwnd: HWND, src: &str) {
        match crate::settings::import_bundle_from(
            &crate::settings::metadata_dir(),
            src,
            metadata::cache::CACHE_SCHEMA,
        ) {
            Ok(st) => {
                app_logf(&format!(
                    "library: import (added={} present={} skipped-schema={} invalid={} total={})",
                    st.added, st.present, st.skipped_schema, st.invalid, st.total
                ));
                self.set_status_text(crate::settings::import_bundle_status(&st));
                self.start_library_scan(hwnd, false);
            }
            Err(e) => {
                app_logf(&format!("library: import failed: {}", e.message()));
                self.set_status_text(fit_scan_status(&format!("Import failed: {}", e.message())));
            }
        }
    }

    /// `onToggleStartup` (`go\settings_sections.go:666`). Production flips the
    /// real HKCU Run key through the `startup_set`/`startup_get` seams; tests
    /// arm those seams to flip in-memory without touching the registry.
    pub fn on_toggle_startup(&mut self) {
        if let Err(e) = (self.startup_set)(!self.startup_state) {
            app_logf(&format!("startup: toggle failed: {e}"));
            self.set_status_text("Start with Windows failed.");
            return;
        }
        self.startup_state = (self.startup_get)();
        let mut cfg =
            app_support::config::load_config_for_update(&self.cfg_path()).unwrap_or_default();
        cfg.start_with_windows = self.startup_state;
        if let Err(e) = app_support::config::save_config_to(&self.cfg_path(), &cfg) {
            app_logf(&format!("startup: save failed: {e}"));
        }
        if self.startup_state {
            self.set_status_text("Start with Windows: on");
        } else {
            self.set_status_text("Start with Windows: off");
        }
        app_logf(&format!("startup: enabled={}", self.startup_state));
    }
}

/// `shiftHeld` (`go\window.go:1319`).
#[must_use]
pub fn shift_held() -> bool {
    // SAFETY: GetKeyState reads the current key state only.
    let r =
        unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState(VK_SHIFT as i32) };
    (r as u16 & 0x8000) != 0
}

/// `buildScanOptions` (`go\window.go:2567`).
#[must_use]
pub fn build_scan_options(
    dirs: Vec<String>,
    force: bool,
    progress: Option<library::ProgressFn>,
) -> library::Options {
    library::Options {
        dirs,
        force,
        cache_dir: crate::settings::metadata_dir(),
        index_path: crate::settings::scan_index_path(),
        use_index: true,
        progress,
        skip_catch_up: false,
        app_version: crate::settings::APP_VERSION.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Painting (`go\window.go:2812-3422`): double-buffered owner-drawn dark UI.
// ---------------------------------------------------------------------------

/// `wr` helper: `Rect` -> Win32 `RECT`.
#[must_use]
fn wr(rc: Rect) -> windows_sys::Win32::Foundation::RECT {
    windows_sys::Win32::Foundation::RECT {
        left: rc.left,
        top: rc.top,
        right: rc.right,
        bottom: rc.bottom,
    }
}

/// The app's one font factory: the brand UI face (Inter, `design\fonts\
/// Inter-Variable.ttf`, embedded and installed by `brand::install_fonts`) at a
/// negative pixel height, `OUT_TT_PRECIS` + `CLEARTYPE_QUALITY`.
///
/// The port originally asked for Segoe UI at `OUT_DEFAULT_PRECIS` +
/// `DEFAULT_QUALITY` ("appearance of the font does not matter"). The identity
/// pass ships the app's own face, so the client area now renders in it -- and
/// the quality pair keeps subpixel antialiasing on the TrueType outline.
/// (learn.microsoft.com/windows/win32/api/wingdi/nf-wingdi-createfontw,
/// accessed 2026-10-14.)
#[must_use]
pub fn create_ui_font(height: i32, weight: i32) -> Gdi::HFONT {
    brand::ui(height.abs(), weight)
}

// ---------------------------------------------------------------------------
// Antialiased geometry (GDI+).
//
// The whole client area is already painted into one memory DC and blitted, so
// one GDI+ `Graphics` wrapped around that DC's HDC smooths every rounded fill
// and ring in the frame. It lives in a thread-local because the paint is
// single-threaded on the UI thread and because `fill_round_rect` is called from
// a dozen paint sites that have no business threading a context through.
// ---------------------------------------------------------------------------

thread_local! {
    /// The current frame's `GpGraphics*` as a raw address (0 = none).
    static PAINT_GRAPHICS: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

/// Start GDI+ once per process. `false` (no GDI+ available) is a supported
/// state: every helper below falls back to plain GDI.
pub(crate) fn gdiplus_start() -> bool {
    use std::sync::OnceLock;
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        let mut token: usize = 0;
        let input = crate::win32::GdiplusStartupInput::default();
        // SAFETY: `input`/`token` outlive the call; GDI+ copies the input and
        // writes only the token. The output struct is optional.
        let status = unsafe {
            crate::win32::GdiplusStartup(
                std::ptr::from_mut(&mut token),
                std::ptr::from_ref(&input).cast(),
                std::ptr::null_mut(),
            )
        };
        status == 0
    })
}

/// Wrap `hdc` in a smoothing `Graphics` for the duration of `f`, then restore
/// the previous context. Nested calls (there are none today) restore correctly.
fn with_antialias<R>(hdc: Gdi::HDC, f: impl FnOnce() -> R) -> R {
    use windows_sys::Win32::Graphics::GdiPlus as gp;
    if !gdiplus_start() || hdc.is_null() {
        return f();
    }
    let mut g: *mut gp::GpGraphics = std::ptr::null_mut();
    // SAFETY: `g` is written only by GDI+ on success and checked before use.
    let ok = unsafe { gp::GdipCreateFromHDC(hdc, std::ptr::from_mut(&mut g)) } == 0 && !g.is_null();
    if !ok {
        return f();
    }
    // SAFETY: a live Graphics from the call above; the mode is a documented const.
    unsafe {
        gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias);
    }
    let prev = PAINT_GRAPHICS.with(|c| c.replace(g as isize));
    let out = f();
    PAINT_GRAPHICS.with(|c| c.set(prev));
    // SAFETY: the Graphics was created above and is released exactly once.
    unsafe {
        gp::GdipDeleteGraphics(g);
    }
    out
}

/// A `COLORREF` (`0x00BBGGRR`) as GDI+'s `ARGB` (`0xAARRGGBB`), fully opaque.
fn argb(color: u32) -> u32 {
    let r = color & 0xFF;
    let g = (color >> 8) & 0xFF;
    let b = (color >> 16) & 0xFF;
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// Fill a rounded rect through the current GDI+ context, if there is one.
/// Returns `false` when the caller must fall back to GDI.
fn gdip_fill_round_rect(rc: Rect, color: u32, radius: i32) -> bool {
    use windows_sys::Win32::Graphics::GdiPlus as gp;
    let g = PAINT_GRAPHICS.with(|c| c.get()) as *mut gp::GpGraphics;
    if g.is_null() {
        return false;
    }
    let r = radius.clamp(0, (rc.right - rc.left).min(rc.bottom - rc.top) / 2); // SAFETY: every handle is created and released within this call, and `g` is
    // the frame's live Graphics while the paint is in flight.
    unsafe {
        let mut path: *mut gp::GpPath = std::ptr::null_mut();
        if gp::GdipCreatePath(gp::FillModeAlternate, std::ptr::from_mut(&mut path)) != 0 {
            return false;
        }
        if r <= 0 {
            let pts = [
                gp::Point {
                    X: rc.left,
                    Y: rc.top,
                },
                gp::Point {
                    X: rc.right,
                    Y: rc.top,
                },
                gp::Point {
                    X: rc.right,
                    Y: rc.bottom,
                },
                gp::Point {
                    X: rc.left,
                    Y: rc.bottom,
                },
            ];
            let kinds = [0u8, 1, 1, 3];
            gp::GdipDeletePath(path);
            let mut poly: *mut gp::GpPath = std::ptr::null_mut();
            if gp::GdipCreatePath2I(
                pts.as_ptr(),
                kinds.as_ptr(),
                pts.len() as i32,
                gp::FillModeAlternate,
                std::ptr::from_mut(&mut poly),
            ) != 0
            {
                return false;
            }
            let mut br: *mut gp::GpSolidFill = std::ptr::null_mut();
            let ok = gp::GdipCreateSolidFill(argb(color), std::ptr::from_mut(&mut br)) == 0;
            if ok {
                gp::GdipFillPath(g, br as *mut gp::GpBrush, poly);
            }
            if !br.is_null() {
                gp::GdipDeleteBrush(br as *mut gp::GpBrush);
            }
            gp::GdipDeletePath(poly);
            return true;
        }
        let (l, t, rt, b) = (rc.left, rc.top, rc.right, rc.bottom);
        let d = r * 2;
        gp::GdipAddPathArcI(path, l, t, d, d, 180.0, 90.0);
        gp::GdipAddPathArcI(path, rt - d, t, d, d, 270.0, 90.0);
        gp::GdipAddPathArcI(path, rt - d, b - d, d, d, 0.0, 90.0);
        gp::GdipAddPathArcI(path, l, b - d, d, d, 90.0, 90.0);
        gp::GdipClosePathFigure(path);
        let mut br: *mut gp::GpSolidFill = std::ptr::null_mut();
        let ok = gp::GdipCreateSolidFill(argb(color), std::ptr::from_mut(&mut br)) == 0;
        if ok {
            gp::GdipFillPath(g, br as *mut gp::GpBrush, path);
        }
        if !br.is_null() {
            gp::GdipDeleteBrush(br as *mut gp::GpBrush);
        }
        gp::GdipDeletePath(path);
    }
    true
}

/// `fillRoundRect` (`go\window.go:3397`), antialiased when GDI+ is live.
fn fill_round_rect(hdc: Gdi::HDC, rc: Rect, color: u32, radius: i32) {
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    let color = themed(color);
    if gdip_fill_round_rect(rc, color, radius) {
        return;
    }
    // SAFETY: brushes/pens created and deleted within the call.
    unsafe {
        let br = Gdi::CreateSolidBrush(color);
        let pen = Gdi::CreatePen(crate::win32::PS_SOLID, 1, color);
        let old_br = Gdi::SelectObject(hdc, br);
        let old_pen = Gdi::SelectObject(hdc, pen);
        Gdi::RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
        Gdi::SelectObject(hdc, old_br);
        Gdi::SelectObject(hdc, old_pen);
        Gdi::DeleteObject(pen);
        Gdi::DeleteObject(br);
    }
}

/// `strokeRoundRect` (`go\window.go:3410`) at the hairline width.
fn stroke_round_rect(hdc: Gdi::HDC, rc: Rect, color: u32, radius: i32) {
    stroke_round_rect_w(hdc, rc, color, radius, STROKE_HAIR);
}

/// A rounded outline of `width` logical px, antialiased when GDI+ is live.
fn stroke_round_rect_w(hdc: Gdi::HDC, rc: Rect, color: u32, radius: i32, width: i32) {
    use windows_sys::Win32::Graphics::GdiPlus as gp;
    if rc.right <= rc.left || rc.bottom <= rc.top || width <= 0 {
        return;
    }
    let color = themed(color);
    let g = PAINT_GRAPHICS.with(|c| c.get()) as *mut gp::GpGraphics;
    if !g.is_null() {
        let r = radius.clamp(0, (rc.right - rc.left).min(rc.bottom - rc.top) / 2);
        // SAFETY: path/pen created and released within the call.
        unsafe {
            let mut path: *mut gp::GpPath = std::ptr::null_mut();
            if gp::GdipCreatePath(gp::FillModeAlternate, std::ptr::from_mut(&mut path)) != 0 {
                return;
            }
            let (l, t, rt, b) = (rc.left, rc.top, rc.right, rc.bottom);
            if r <= 0 {
                let pts = [
                    gp::Point { X: l, Y: t },
                    gp::Point { X: rt, Y: t },
                    gp::Point { X: rt, Y: b },
                    gp::Point { X: l, Y: b },
                ];
                let kinds = [0u8, 1, 1, 3];
                gp::GdipDeletePath(path);
                let mut poly: *mut gp::GpPath = std::ptr::null_mut();
                if gp::GdipCreatePath2I(
                    pts.as_ptr(),
                    kinds.as_ptr(),
                    pts.len() as i32,
                    gp::FillModeAlternate,
                    std::ptr::from_mut(&mut poly),
                ) != 0
                {
                    return;
                }
                let mut pen: *mut gp::GpPen = std::ptr::null_mut();
                if gp::GdipCreatePen1(
                    argb(color),
                    width as f32,
                    gp::UnitPixel,
                    std::ptr::from_mut(&mut pen),
                ) == 0
                {
                    gp::GdipDrawPath(g, pen, poly);
                }
                if !pen.is_null() {
                    gp::GdipDeletePen(pen);
                }
                gp::GdipDeletePath(poly);
                return;
            }
            let d = r * 2;
            gp::GdipAddPathArcI(path, l, t, d, d, 180.0, 90.0);
            gp::GdipAddPathArcI(path, rt - d, t, d, d, 270.0, 90.0);
            gp::GdipAddPathArcI(path, rt - d, b - d, d, d, 0.0, 90.0);
            gp::GdipAddPathArcI(path, l, b - d, d, d, 90.0, 90.0);
            gp::GdipClosePathFigure(path);
            let mut pen: *mut gp::GpPen = std::ptr::null_mut();
            if gp::GdipCreatePen1(
                argb(color),
                width as f32,
                gp::UnitPixel,
                std::ptr::from_mut(&mut pen),
            ) == 0
            {
                gp::GdipDrawPath(g, pen, path);
            }
            if !pen.is_null() {
                gp::GdipDeletePen(pen);
            }
            gp::GdipDeletePath(path);
        }
        return;
    }
    // SAFETY: pen created/deleted; NULL_BRUSH keeps the interior.
    unsafe {
        let pen = Gdi::CreatePen(crate::win32::PS_SOLID, width, color);
        let old_pen = Gdi::SelectObject(hdc, pen);
        Gdi::SetBkMode(hdc, crate::win32::TRANSPARENT);
        let null_brush = Gdi::GetStockObject(crate::win32::NULL_BRUSH_STOCK);
        let old_br = Gdi::SelectObject(hdc, null_brush);
        Gdi::RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
        Gdi::SelectObject(hdc, old_br);
        Gdi::SelectObject(hdc, old_pen);
        Gdi::DeleteObject(pen);
    }
}

/// The keyboard focus ring: a 2px accent rounded outline inset inside `rc` so it
/// never collides with the control's own edge. Drawn for mouse focus too -- the
/// ring is the app's single focus affordance.
fn draw_focus_ring(hdc: Gdi::HDC, rc: Rect, dpi: i32) {
    let inset = scale_px(dpi, FOCUS_INSET);
    let ring = Rect {
        left: rc.left + inset,
        top: rc.top + inset,
        right: rc.right - inset,
        bottom: rc.bottom - inset,
    };
    if ring.right <= ring.left || ring.bottom <= ring.top {
        return;
    }
    stroke_round_rect_w(
        hdc,
        ring,
        STATE_FOCUS,
        scale_px(dpi, R_CTRL),
        scale_px(dpi, STROKE_FOCUS),
    );
}

/// `drawUIText` (`go\window.go:3429`).
fn draw_ui_text(hdc: Gdi::HDC, s: &str, rc: Rect, format: u32, font: Gdi::HFONT, color: u32) {
    let color = themed(color);
    // SAFETY: font/color selected then restored; `u` outlives the call.
    unsafe {
        Gdi::SetTextColor(hdc, color);
        // Transparent text background: the `WM_DRAWITEM` HDC defaults to
        // OPAQUE, which would paint a system-white block behind every
        // owner-drawn label on the dark fills.
        Gdi::SetBkMode(hdc, crate::win32::TRANSPARENT);
        let old = Gdi::SelectObject(hdc, font);
        let u = utf16_z(s);
        let mut bx = wr(rc);
        Gdi::DrawTextW(hdc, u.as_ptr(), -1, &mut bx, format);
        Gdi::SelectObject(hdc, old);
    }
}

/// `drawUITextWrapped` (`go\window.go:3302`).
fn draw_ui_text_wrapped(hdc: Gdi::HDC, s: &str, rc: Rect, font: Gdi::HFONT, color: u32) {
    let color = themed(color);
    // SAFETY: font/color selected then restored; `u` outlives the call.
    unsafe {
        Gdi::SetTextColor(hdc, color);
        Gdi::SetBkMode(hdc, crate::win32::TRANSPARENT);
        let old = Gdi::SelectObject(hdc, font);
        let u = utf16_z(s);
        let mut bx = wr(rc);
        Gdi::DrawTextW(
            hdc,
            u.as_ptr(),
            -1,
            &mut bx,
            crate::win32::DT_LEFT
                | DT_WORD_BREAK
                | crate::win32::DT_NO_PREFIX
                | crate::win32::DT_END_ELLIPSIS,
        );
        Gdi::SelectObject(hdc, old);
    }
}

/// The rect the Activity surface draws its media title into. The paint and the
/// wide-viewport proof read this one function, so a proof cannot disagree with
/// the pixels about where the title was allowed to go.
fn activity_title_band(rc: Rect, dpi: i32, art: bool) -> Rect {
    let sc = |v: i32| scale_px(dpi, v);
    let pad = sc(SP_4);
    let hero_top = rc.top + sc(SP_3) + sc(18) + sc(SP_3);
    Rect {
        left: activity_text_left(rc, dpi, art),
        top: hero_top,
        right: rc.right - pad,
        bottom: hero_top + sc(48),
    }
}

/// The left edge of the Activity surface's text column: past the cover when
/// there is one, at the panel pad when there is not.
fn activity_text_left(rc: Rect, dpi: i32, art: bool) -> i32 {
    let sc = |v: i32| scale_px(dpi, v);
    let pad = sc(SP_4);
    if art {
        (rc.left + pad + sc(76)).min(rc.right - pad) + sc(SP_3)
    } else {
        rc.left + pad
    }
}

pub(crate) fn picked_row_key(cand: &Candidate) -> String {
    format!(
        "{}\u{1}{}\u{1}{}",
        cand.provider, cand.provider_id, cand.title
    )
}

/// The Fix-match dialog's explicit-pick key: the provider identity of one
/// listed candidate. A new batch that still carries the key keeps the user's
/// row instead of reselecting index 0.
#[must_use]
pub(crate) fn fix_pick_key(cand: &Candidate) -> String {
    format!("{}\u{1}{}", cand.provider, cand.provider_id)
}

/// Paint a media title so every character of it is readable inside `rc`: the
/// display face when the text fits, else the body face, else one ellipsized
/// line in the smallest face. A title that loses an edge glyph reads as a
/// corrupt string, so the face gives way before the text does.
fn paint_media_title(
    hdc: Gdi::HDC,
    s: &str,
    rc: Rect,
    dpi: i32,
    color: u32,
    display: Gdi::HFONT,
    wrap: bool,
) {
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    let sc = |v: i32| scale_px(dpi, v);
    let width = rc.right - rc.left;
    let height = rc.bottom - rc.top;
    let body = brand::ui(sc(FS_BODY), brand::UI_WEIGHT);
    let hint = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
    let single = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER;
    // Two pixels of slack: a descender that lands exactly on the last row is
    // already clipped, and the proof reads ink, not metrics.
    let room = height - sc(3);
    for font in [display, body, hint] {
        // SAFETY: measurement only; the previous font is restored.
        let fits = unsafe {
            let old = Gdi::SelectObject(hdc, font);
            let fits = if wrap {
                let need = wrapped_need(hdc, s, width);
                need > 0 && need <= room
            } else {
                single_line_fits(hdc, s, width)
            };
            Gdi::SelectObject(hdc, old);
            fits
        };
        if fits {
            if wrap {
                draw_ui_text_wrapped(hdc, s, rc, font, color);
            } else {
                draw_ui_text(hdc, s, rc, single, font, color);
            }
            return;
        }
    }
    draw_ui_text(
        hdc,
        s,
        rc,
        single | crate::win32::DT_END_ELLIPSIS,
        hint,
        color,
    );
}

/// `drawUITextWrapped`, centered: the empty panel's hint/body under its
/// centered title. Same wrap/ellipsis/no-prefix behavior, centered so the
/// panel reads as one block.
fn draw_ui_text_wrapped_centered(hdc: Gdi::HDC, s: &str, rc: Rect, font: Gdi::HFONT, color: u32) {
    let color = themed(color);
    // SAFETY: font/color selected then restored; `u` outlives the call.
    unsafe {
        Gdi::SetTextColor(hdc, color);
        Gdi::SetBkMode(hdc, crate::win32::TRANSPARENT);
        let old = Gdi::SelectObject(hdc, font);
        let u = utf16_z(s);
        let mut bx = wr(rc);
        Gdi::DrawTextW(
            hdc,
            u.as_ptr(),
            -1,
            &mut bx,
            crate::win32::DT_CENTER
                | DT_WORD_BREAK
                | crate::win32::DT_NO_PREFIX
                | crate::win32::DT_EDITCONTROL,
        );
        Gdi::SelectObject(hdc, old);
    }
}

/// `singleLineFits` (`go\window.go:3153`).
#[must_use]
pub fn single_line_fits(hdc: Gdi::HDC, s: &str, w: i32) -> bool {
    use windows_sys::Win32::Foundation::SIZE;
    if s.is_empty() {
        return true;
    }
    // SAFETY: extents read only.
    unsafe {
        let u: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE { cx: 0, cy: 0 };
        let r = Gdi::GetTextExtentPoint32W(hdc, u.as_ptr(), u.len() as i32, &mut sz);
        if r == 0 {
            return true;
        }
        sz.cx <= w
    }
}

/// `wrappedNeed` (`go\window.go:3177`).
#[must_use]
pub fn wrapped_need(hdc: Gdi::HDC, s: &str, w: i32) -> i32 {
    // SAFETY: DT_CALCRECT writes only into `box`.
    unsafe {
        let u = utf16_z(s);
        let mut bx = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: 0,
        };
        Gdi::DrawTextW(
            hdc,
            u.as_ptr(),
            -1,
            &mut bx,
            crate::win32::DT_LEFT
                | DT_WORD_BREAK
                | crate::win32::DT_NO_PREFIX
                | crate::win32::DT_END_ELLIPSIS
                | 0x0400,
        );
        bx.bottom - bx.top
    }
}

/// `breakRow` (`go\window.go:3191`): display-only wrap with hard breaks.
#[must_use]
pub fn break_row(hdc: Gdi::HDC, s: &str, w: i32) -> String {
    if single_line_fits(hdc, s, w) {
        return s.to_string();
    }
    let runes: Vec<char> = s.chars().collect();
    let mut lines: Vec<String> = Vec::new();
    let mut start = 0usize;
    while start < runes.len() {
        let (mut lo, mut hi) = (start + 1, runes.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if single_line_fits(hdc, &runes[start..mid].iter().collect::<String>(), w) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let mut end = lo;
        if end <= start {
            end = start + 1;
        }
        let mut cut = end;
        let mut i = end - 1;
        while i > start {
            if runes[i] == ' ' {
                cut = i;
                break;
            }
            i -= 1;
        }
        lines.push(runes[start..cut].iter().collect());
        start = cut;
        if start < runes.len() && runes[start] == ' ' {
            start += 1;
        }
    }
    lines.join("\n")
}

/// `paintButton` (`go\window.go:3319`).
/// A solid rectangular fill, no rounding (the chrome band, the panel).
fn fill_rect_solid(hdc: Gdi::HDC, rc: Rect, color: u32) {
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    let color = themed(color);
    // SAFETY: brush created and released within the call; FillRect takes it as a
    // borrowed handle.
    unsafe {
        let br = Gdi::CreateSolidBrush(color);
        let r = wr(rc);
        Gdi::FillRect(hdc, &r, br);
        Gdi::DeleteObject(br);
    }
}

/// `DrawTextW` with GDI's own inter-character spacing (`SetTextCharacterExtra`),
/// then the previous spacing restored. The brand's wordmark and micro-labels are
/// letter-spaced (`design\make_logo.py` tracks the lockup at 0.08em); this is the
/// only letter-spacing control GDI offers, and it is uniform, which is exactly
/// what the brand uses.
fn draw_ui_text_tracked(
    hdc: Gdi::HDC,
    s: &str,
    rc: Rect,
    format: u32,
    font: Gdi::HFONT,
    color: u32,
    extra_px: i32,
) {
    // SAFETY: the spacing value is restored before returning.
    let old = unsafe { crate::win32::SetTextCharacterExtra(hdc, extra_px) };
    // Transparent text background (see `draw_ui_text`); the delegated call
    // sets it again, harmlessly.
    unsafe {
        Gdi::SetBkMode(hdc, crate::win32::TRANSPARENT);
    }
    draw_ui_text(hdc, s, rc, format, font, color);
    // SAFETY: restores the caller's spacing.
    unsafe {
        crate::win32::SetTextCharacterExtra(hdc, old);
    }
}

/// Tracking in logical px for a face of `size_px` at `em` em.
#[must_use]
fn tracked_px(size_px: i32, em: f32) -> i32 {
    (size_px as f32 * em).round() as i32
}

/// The chrome header band: the Ribbon-P mark on its tile, the Archivo Black
/// wordmark, and the Inter subtitle.
///
/// The mark is drawn from the SAME `HICON` the window applies to its class and
/// `WM_SETICON` slots (`brand::mark_icon`), so the mark in the header, in the
/// titlebar and in the taskbar is one asset, not three approximations.
fn paint_header(hdc: Gdi::HDC, rc: Rect, dpi: i32) {
    let sc = |v: i32| scale_px(dpi, v);
    let (left, content_w) = content_span(rc.right - rc.left, dpi);
    fill_rect_solid(hdc, rc, COL_SURFACE);
    // A hairline under the band so the chrome and the canvas meet on a line, not
    // a guess.
    fill_rect_solid(
        hdc,
        Rect {
            left: rc.left,
            top: rc.bottom - sc(STROKE_HAIR),
            right: rc.right,
            bottom: rc.bottom,
        },
        COL_CARD,
    );

    let tile = sc(MARK_TILE_L);
    let tile_rc = Rect {
        left,
        top: rc.top + sc(SP_4),
        right: left + tile,
        bottom: rc.top + sc(SP_4) + tile,
    };
    let icon = brand::mark_icon();
    if !icon.is_null() {
        // SAFETY: `icon` is the process's live mark handle; DrawIconEx reads it.
        unsafe {
            crate::win32::DrawIconEx(
                hdc,
                tile_rc.left,
                tile_rc.top,
                icon,
                tile,
                tile,
                0,
                std::ptr::null_mut(),
                crate::win32::DI_NORMAL,
            );
        }
    }

    let word_font = brand::display(sc(FS_WORDMARK));
    let sub_font = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
    let fmt = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER;
    let text_left = tile_rc.right + sc(SP_3);
    draw_ui_text_tracked(
        hdc,
        "POTFLIX",
        Rect {
            left: text_left,
            top: rc.top + sc(SP_3),
            right: left + content_w,
            bottom: rc.top + sc(SP_3) + sc(26),
        },
        fmt,
        word_font,
        COL_TEXT,
        tracked_px(sc(FS_WORDMARK), brand::TRACK_WORDMARK),
    );
    draw_ui_text(
        hdc,
        "Discord Rich Presence for PotPlayer",
        Rect {
            left: text_left,
            top: rc.top + sc(SP_3) + sc(26),
            right: left + content_w,
            bottom: rc.bottom - sc(SP_2),
        },
        fmt,
        sub_font,
        COL_MUTED,
    );

    // SAFETY: fonts created above are deleted here.
    unsafe {
        Gdi::DeleteObject(word_font);
        Gdi::DeleteObject(sub_font);
    }
}

/// A section micro-label -- tiny, uppercase, letter-spaced, in **Archivo
/// Black** (the brand's display face, and the brief's own instruction that it
/// drives the wordmark and section headings) -- with the count badge V1 at the
/// line's right end.
///
/// The badge is the brand's own disc system (`design\preview\badges-variants.png`
/// V1: black disc + red ring): a black disc with a 2px red ring and the count
/// centred in it. The numeral is `COL_TEXT` rather than V1's red glyph, because
/// red on the disc measures `4.11:1` and a NUMERAL is text, which the gate holds
/// at `4.5:1`; the ring keeps the red, so V1's structure survives and the gate
/// passes. Both numbers are in `brand_contrast_gate`.
fn paint_section_label(hdc: Gdi::HDC, rc: Rect, dpi: i32, label: &str, count: i32) {
    let sc = |v: i32| scale_px(dpi, v);
    let label_font = brand::display(sc(FS_LABEL));
    let fmt = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER;

    let disc = sc(18);
    let ring = sc(STROKE_FOCUS);
    let gap = sc(SP_2);
    let badge_left = rc.right - disc - ring;
    let text_right = badge_left - gap;

    draw_ui_text_tracked(
        hdc,
        &label.to_uppercase(),
        Rect {
            left: rc.left,
            top: rc.top,
            right: text_right,
            bottom: rc.bottom,
        },
        fmt,
        label_font,
        COL_MUTED,
        tracked_px(sc(FS_LABEL), brand::TRACK_LABEL),
    );

    // A count of zero has nothing to report; the badge appears with the first
    // folder and the section header alone is the empty state.
    if count > 0 {
        let badge = Rect {
            left: badge_left,
            top: rc.top + (rc.bottom - rc.top - disc) / 2,
            right: badge_left + disc,
            bottom: rc.top + (rc.bottom - rc.top - disc) / 2 + disc,
        };
        let r = disc / 2;
        fill_round_rect(hdc, badge, brand::BLACK, r);
        stroke_round_rect_w(hdc, badge, brand::RED, r, ring);
        let num_font = brand::display(sc(FS_BADGE));
        let num = count.to_string();
        draw_ui_text(
            hdc,
            &num,
            badge,
            crate::win32::DT_CENTER
                | crate::win32::DT_SINGLE_LINE
                | crate::win32::DT_NO_PREFIX
                | crate::win32::DT_V_CENTER,
            num_font,
            COL_TEXT,
        );
        // SAFETY: font created above.
        unsafe {
            Gdi::DeleteObject(num_font);
        }
    }

    // SAFETY: font created above.
    unsafe {
        Gdi::DeleteObject(label_font);
    }
}

/// The folder panel: a `CHROME` well holding one `CARD`-surface chip per folder,
/// the `+N more` line, and the proportional scrollbar in its right gutter.
fn paint_panel(hdc: Gdi::HDC, l: &UiLayout, dpi: i32, st: &UiState) {
    let sc = |v: i32| scale_px(dpi, v);
    let body_font = brand::ui(sc(FS_BODY), brand::UI_WEIGHT);
    let hint_font = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
    let fmt_left = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;
    let fmt_right = fmt_left | crate::win32::DT_RIGHT;
    let fmt_center = fmt_left | crate::win32::DT_CENTER;

    fill_round_rect(hdc, l.card, COL_SURFACE, sc(R_PANEL));

    if l.rows.is_empty() {
        draw_ui_text(
            hdc,
            "No folders yet - add one below",
            l.card,
            fmt_center,
            hint_font,
            COL_MUTED,
        );
    } else {
        for (i, r) in l.rows.iter().enumerate() {
            // Absolute folder index: the viewport row plus the scroll offset.
            // `abs` is below `ui_dirs.len()` by construction (`vis` rows from
            // `list_top` cover `list_top..list_top + vis`, clamped to `total`).
            let abs = l.list_top + i as i32;
            let selected = abs == st.sel_row;
            let mut fill = COL_ROW;
            if abs == st.hover_row || selected {
                fill = COL_ROW_HOVER;
            }
            fill_round_rect(hdc, *r, fill, sc(R_ROW));
            if selected {
                // The selection indicator is the accent bar down the row's left
                // edge (`4.11:1` on the panel) plus the fill step -- two cues, so
                // selection never rests on colour alone.
                fill_round_rect(
                    hdc,
                    Rect {
                        left: r.left,
                        top: r.top,
                        right: r.left + sc(SELECT_BAR_W),
                        bottom: r.bottom,
                    },
                    COL_ACCENT,
                    sc(R_ROW),
                );
            }
            let mut text_right = r.right - sc(SP_3);
            if abs == st.hover_row && i < l.xbtns.len() {
                draw_ui_text(hdc, "Remove", l.xbtns[i], fmt_center, hint_font, COL_DANGER);
                text_right = l.xbtns[i].left - sc(SP_1);
            }
            draw_ui_text(
                hdc,
                &st.ui_dirs[abs as usize],
                Rect {
                    left: r.left + sc(SP_3),
                    top: r.top,
                    right: text_right,
                    bottom: r.bottom,
                },
                fmt_left,
                body_font,
                COL_TEXT,
            );
        }
        if l.overflow > 0 {
            draw_ui_text(
                hdc,
                &format!("+{} more", l.overflow),
                Rect {
                    left: l.card.left + sc(SP_3),
                    top: l.card.bottom - sc(OVERFLOW_H_L),
                    right: l.card.right - sc(SP_3),
                    bottom: l.card.bottom - sc(SP_1),
                },
                fmt_right,
                hint_font,
                COL_MUTED,
            );
        }
        if l.scrolled() {
            // Position-only scrollbar in the panel's right gutter: the track
            // plus the proportional thumb mirroring `list_top`. Wheel,
            // PageUp/PageDown, Home/End scroll; the thumb is paint, not drag.
            fill_round_rect(hdc, l.scrollbar, brand::TRACK, sc(R_BAR));
            if l.thumb.right > l.thumb.left && l.thumb.bottom > l.thumb.top {
                fill_round_rect(hdc, l.thumb, COL_MUTED, sc(R_BAR));
            }
        }
    }

    // SAFETY: fonts created above.
    unsafe {
        Gdi::DeleteObject(body_font);
        Gdi::DeleteObject(hint_font);
    }
}

/// The scan progress bar over the status line: an unfilled `TRACK` well with the
/// brand red as the fill, both rounded by the house squircle.
#[allow(clippy::too_many_arguments)]
fn paint_button_face(
    hdc: Gdi::HDC,
    b: &UiButton,
    dpi: i32,
    button_font: Gdi::HFONT,
    hovered: bool,
    pressed: bool,
    disabled: bool,
    dim: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    let fmt = crate::win32::DT_CENTER
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;
    let r = sc(R_CTRL);
    match b.style {
        BTN_ACCENT => {
            // The view's one primary control. Resting: the brand red. Hover: one
            // step up. Pressed: the receding plane INSIDE a red edge -- the
            // Ribbon P's own two-facet language, and the reason the silhouette
            // stays red (`4.11:1` on the canvas) while the interior recedes.
            let fill = if disabled {
                COL_SURFACE
            } else if pressed {
                COL_ACCENT_DOWN
            } else if hovered {
                COL_ACCENT_HOV
            } else {
                COL_ACCENT
            };
            fill_round_rect(hdc, b.rc, fill, r);
            if pressed && !disabled {
                stroke_round_rect_w(hdc, b.rc, COL_ACCENT, r, sc(STROKE_FOCUS));
            }
            let tc = if disabled {
                COL_MUTED_DIM
            } else {
                COL_ON_ACCENT
            };
            draw_ui_text(hdc, &b.label, b.rc, fmt, button_font, tc);
        }
        BTN_TOGGLE | BTN_TOGGLE_ON => {
            // A field toggle. On: quiet lit fill and a WHITE indicator (several
            // can be on at once); off: a hairline segment with muted text.
            let on = b.style == BTN_TOGGLE_ON;
            if on || (hovered && !disabled) {
                fill_round_rect(hdc, b.rc, COL_ROW, r);
            }
            if !on {
                stroke_round_rect(hdc, b.rc, COL_BORDER, r);
            }
            let tc = if disabled {
                COL_MUTED_DIM
            } else if on || hovered || pressed {
                COL_TEXT
            } else {
                COL_MUTED
            };
            draw_ui_text(hdc, &b.label, b.rc, fmt, button_font, tc);
            if on {
                let ind = Rect {
                    left: b.rc.left + sc(SP_4),
                    top: b.rc.bottom - sc(SP_1),
                    right: b.rc.right - sc(SP_4),
                    bottom: b.rc.bottom - sc(SP_1) + sc(STROKE_FOCUS),
                };
                fill_round_rect(hdc, ind, COL_TEXT, sc(R_BAR));
            }
        }
        BTN_TAB | BTN_TAB_SEL => {            // A segment of a segmented control: one anatomy for the tab strip,
            // the scan-speed segments and the card presets. Lit = the card fill
            // plus the red indicator under the label; unlit = flat on the canvas
            // with the boundary hairline and muted text.
            let lit = b.style == BTN_TAB_SEL;
            if lit || (hovered && !disabled) {
                fill_round_rect(hdc, b.rc, COL_ROW, r);
            }
            if !lit {
                stroke_round_rect(hdc, b.rc, COL_BORDER, r);
            }
            let tc = if disabled {
                COL_MUTED_DIM
            } else if lit || hovered || pressed {
                COL_TEXT
            } else {
                COL_MUTED
            };
            draw_ui_text(hdc, &b.label, b.rc, fmt, button_font, tc);
            if lit {
                let ind = Rect {
                    left: b.rc.left + sc(SP_4),
                    top: b.rc.bottom - sc(SP_1),
                    right: b.rc.right - sc(SP_4),
                    bottom: b.rc.bottom - sc(SP_1) + sc(STROKE_FOCUS),
                };
                fill_round_rect(hdc, ind, COL_ACCENT, sc(R_BAR));
            }
        }
        BTN_DANGER => {
            // A destructive action is still a real control on the grid; only its
            // label carries the warning, in a red that clears the body gate.
            let fill = if pressed {
                COL_SURFACE
            } else if hovered {
                COL_ROW_HOVER
            } else {
                COL_CARD
            };
            fill_round_rect(hdc, b.rc, fill, r);
            stroke_round_rect(hdc, b.rc, COL_BORDER, r);
            let tc = if disabled { COL_MUTED_DIM } else { COL_DANGER };
            draw_ui_text(hdc, &b.label, b.rc, fmt, button_font, tc);
        }
        BTN_TEXT => {
            // A quiet text action, not an outline button: no fill at rest, no
            // outline ever. Hover carries the fill step; the label is
            // left-aligned and vertically centered, muted at rest.
            if (hovered || pressed) && !disabled {
                fill_round_rect(hdc, b.rc, COL_ROW_HOVER, r);
            }
            let left_fmt = crate::win32::DT_LEFT
                | crate::win32::DT_SINGLE_LINE
                | crate::win32::DT_NO_PREFIX
                | crate::win32::DT_V_CENTER
                | crate::win32::DT_END_ELLIPSIS;
            let label_rc = Rect {
                left: b.rc.left + sc(SP_3),
                top: b.rc.top,
                right: b.rc.right - sc(SP_2),
                bottom: b.rc.bottom,
            };
            let tc = if disabled {
                COL_MUTED_DIM
            } else if hovered || pressed {
                COL_TEXT
            } else {
                COL_MUTED
            };
            draw_ui_text(hdc, &b.label, label_rc, left_fmt, button_font, tc);
        }
        _ => {
            // BTN_OUTLINE / BTN_SUBTLE: the secondary control. The fill is the
            // card surface and the EDGE hairline is the boundary that carries the
            // non-text gate (`4.09:1` on the canvas).
            let fill = if disabled || pressed {
                COL_SURFACE
            } else if hovered {
                COL_ROW_HOVER
            } else {
                COL_CARD
            };
            fill_round_rect(hdc, b.rc, fill, r);
            stroke_round_rect(hdc, b.rc, COL_BORDER, r);
            let tc = if dim { COL_MUTED_DIM } else { COL_TEXT };
            draw_ui_text(hdc, &b.label, b.rc, fmt, button_font, tc);
        }
    }
}

fn paint_button(hdc: Gdi::HDC, b: &UiButton, dpi: i32, button_font: Gdi::HFONT, st: &UiState) {
    let hovered = st.hover_btn == b.id;
    let pressed = st.pressed_btn == b.id;
    let picker_disabled = b.id == BTN_PICKER_SEARCH
        && !st.picker.open
        && !st.picker.search_allowed();
    let disabled = !st.scan_blocked_reason(b.id).is_empty() || picker_disabled;
    let dim = disabled || (b.id == BTN_REMOVE && !st.remove_has_target());
    paint_button_face(hdc, b, dpi, button_font, hovered, pressed, disabled, dim);
}

// `paintPreviewRows` lived here for the developer-dump preview; the media
// composition draws its own title/meta/synopsis rows.

/// `paintScanBar` (`go\window.go:2517`).
fn paint_scan_bar(hdc: Gdi::HDC, status: Rect, dpi: i32, st: &UiState) {
    if !st.scan_active {
        return;
    }
    let bar = scan_bar_rect(status, dpi);
    if bar.right <= bar.left || bar.bottom <= bar.top {
        return;
    }
    let sc = |v: i32| scale_px(dpi, v);
    fill_round_rect(hdc, bar, brand::TRACK, sc(R_BAR));
    let (show, determinate, frac) = scan_bar_progress(&st.scan_progress, st.scan_active);
    if !show {
        return;
    }
    let bar_w = bar.right - bar.left;
    if determinate {
        let mut w = (frac * f64::from(bar_w)) as i32;
        if w <= 0 {
            return;
        }
        if w < sc(8) {
            w = sc(8);
        }
        if w > bar_w {
            w = bar_w;
        }
        fill_round_rect(
            hdc,
            Rect {
                left: bar.left,
                top: bar.top,
                right: bar.left + w,
                bottom: bar.bottom,
            },
            COL_ACCENT,
            sc(4),
        );
        return;
    }
    let mut chunk = bar_w / 4;
    if chunk < sc(24) {
        chunk = sc(24);
    }
    if chunk >= bar_w {
        chunk = bar_w;
    }
    let pos = scan_indeterminate_pos(Instant::now().saturating_duration_since(st.scan_start));
    let x = bar.left + (pos * f64::from(bar_w - chunk)) as i32;
    fill_round_rect(
        hdc,
        Rect {
            left: x,
            top: bar.top,
            right: x + chunk,
            bottom: bar.bottom,
        },
        COL_ACCENT,
        sc(4),
    );
}

/// The status bar's right-aligned version (`v<major>.<minor>` from
/// [`crate::settings::APP_VERSION`]): 12px muted, vertically centred in the
/// 24px bar, inset 16px from the window's right edge. Pure text, no control.
///
/// The version's draw rect: the status bar with the 16px right inset applied.
/// Pure so the paint and its tests agree on the exact rect.
#[must_use]
pub fn status_version_rect(status: Rect, dpi: i32) -> Rect {
    let inset = scale_px(dpi, STATUS_VERSION_INSET_L);
    Rect {
        left: status.left,
        top: status.top,
        right: (status.right - inset).max(status.left),
        bottom: status.bottom,
    }
}

/// The status version's font: the ONE source both the fit measurement and the
/// paint read, so the measured short version is the rendered short version.
fn version_font(dpi: i32) -> Gdi::HFONT {
    brand::ui(scale_px(dpi, 12), brand::UI_WEIGHT)
}

/// `GetTextExtent` width of the short version in the painted font: the proof
/// the brief demands. `None` when no DC/font is available.
#[must_use]
pub fn measure_version_width(dpi: i32) -> Option<i32> {
    use windows_sys::Win32::Foundation::SIZE;
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetTextExtentPoint32W, ReleaseDC, SelectObject,
    };
    let s = app_version_short();
    if s.is_empty() {
        return Some(0);
    }
    // SAFETY: a screen DC + a font, both released below; extents do not depend
    // on the target surface.
    unsafe {
        let hdc = GetDC(std::ptr::null_mut());
        if hdc.is_null() {
            return None;
        }
        let font = version_font(dpi);
        if font.is_null() {
            ReleaseDC(std::ptr::null_mut(), hdc);
            return None;
        }
        let old = SelectObject(hdc, font);
        let u: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE { cx: 0, cy: 0 };
        let r = GetTextExtentPoint32W(hdc, u.as_ptr(), u.len() as i32, &mut sz);
        SelectObject(hdc, old);
        DeleteObject(font);
        ReleaseDC(std::ptr::null_mut(), hdc);
        if r == 0 { None } else { Some(sz.cx) }
    }
}

fn paint_status_version(hdc: Gdi::HDC, status: Rect, dpi: i32) {
    if status.right <= status.left || status.bottom <= status.top {
        return;
    }
    // The 16px inset: the version never touches the window's right edge.
    let rc = status_version_rect(status, dpi);
    let ver_font = version_font(dpi);
    // GetTextExtent proof at this DPI: the short form must fit inside the
    // inset rect, or the right edge clips again.
    let need = narrow::text_width_px(hdc, ver_font, &app_version_short());
    debug_assert!(
        need <= (rc.right - rc.left).max(1),
        "version {need}px exceeds inset rect {}px at dpi {dpi}",
        rc.right - rc.left
    );
    draw_ui_text(
        hdc,
        &app_version_short(),
        rc,
        crate::win32::DT_RIGHT
            | crate::win32::DT_SINGLE_LINE
            | crate::win32::DT_NO_PREFIX
            | crate::win32::DT_V_CENTER
            | crate::win32::DT_END_ELLIPSIS,
        ver_font,
        COL_MUTED,
    );
    // SAFETY: font created above.
    unsafe {
        Gdi::DeleteObject(ver_font);
    }
}

/// `auditSingleLineVerdict` (`go\window.go:3582`).
#[must_use]
pub fn audit_single_line_verdict(
    need_w: i32,
    need_h: i32,
    rect_w: i32,
    rect_h: i32,
    ellipsis: bool,
) -> &'static str {
    if need_w <= rect_w && need_h <= rect_h {
        return "";
    }
    if ellipsis { "trimmed" } else { "overflow" }
}

/// `uiAuditEnabled` (`go\window.go:3536`).
#[must_use]
pub fn ui_audit_enabled() -> bool {
    std::env::var("POTFLIX_UI_AUDIT").is_ok_and(|v| v == "1")
}

/// `paintMainWindow` (`go\window.go:2812`): double-buffered, paints the visible
/// view.
pub fn paint_main_window(hwnd: HWND) {
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        EndPaint, SelectObject,
    };
    let mut ps: Gdi::PAINTSTRUCT = unsafe { std::mem::zeroed() };
    // SAFETY: BeginPaint/EndPaint bracket the paint.
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if hdc.is_null() {
        return;
    }
    let rc = get_client_rect(hwnd);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let dpi = window_dpi(hwnd);
    let st = ui();
    // SAFETY: a memory DC + bitmap created and released within the call.
    unsafe {
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        if !mem.is_null() && !bmp.is_null() {
            let old = SelectObject(mem, bmp);
            with_antialias(mem, || paint_view(mem, rc, dpi, &st));
            BitBlt(hdc, 0, 0, w, h, mem, 0, 0, crate::win32::SRCCOPY);
            SelectObject(mem, old);
            DeleteObject(bmp);
            DeleteDC(mem);
        } else {
            if !mem.is_null() {
                DeleteDC(mem);
            }
            if !bmp.is_null() {
                DeleteObject(bmp);
            }
            with_antialias(hdc, || paint_view(hdc, rc, dpi, &st));
        }
        EndPaint(hwnd, &ps);
    }
}

/// Paint the current view into any memory DC at any DPI: the offscreen twin of
/// [`paint_main_window`] minus `BeginPaint`/`EndPaint`, for captures and
/// high-DPI proof renders. Holds `UI` only across GDI blits (never across a
/// dispatching call), exactly like `paint_main_window`: NOT a re-entrant site.
pub fn paint_to_hdc(hdc: Gdi::HDC, rc: Rect, dpi: i32) {
    if hdc.is_null() {
        return;
    }
    let st = ui();
    with_antialias(hdc, || paint_view(hdc, rc, dpi, &st));
}

fn paint_view(hdc: Gdi::HDC, rc: Rect, dpi: i32, st: &UiState) {
    match st.cur_view {
        View::Card => paint_card_view(hdc, rc, dpi, st),
        View::Settings => paint_settings_card(hdc, rc, dpi, st),
        View::Main => paint_main_card(hdc, rc, dpi, st),
    }
}

fn fill_background(hdc: Gdi::HDC, rc: Rect) {
    // SAFETY: brush created/deleted within the call.
    unsafe {
        let bg = Gdi::CreateSolidBrush(themed(COL_BG));
        let fill = wr(rc);
        Gdi::FillRect(hdc, &fill, bg);
        Gdi::DeleteObject(bg);
        Gdi::SetBkMode(hdc, crate::win32::TRANSPARENT);
    }
}

/// `paintMainCard` (`go\window.go:2849`).
fn paint_main_card(hdc: Gdi::HDC, rc: Rect, dpi: i32, st: &UiState) {
    let sc = |v: i32| scale_px(dpi, v);
    let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
    fill_background(hdc, rc);

    let mut l = compute_layout_from(&st.ui_dirs, cw, ch, dpi, st.scroll_top);
    // The tab strip's lit segment is a function of the CURRENT view, not of the
    // layout, so it is resolved in one place for both segments.
    for b in l.btns.iter_mut() {
        if b.style == BTN_TAB || b.style == BTN_TAB_SEL {
            let lit = (b.id == BTN_CARD && st.cur_view == View::Card)
                || (b.id == BTN_SETTINGS && st.cur_view == View::Settings);
            b.style = if lit { BTN_TAB_SEL } else { BTN_TAB };
        }
    }

    let button_font = brand::ui(sc(FS_CONTROL), brand::UI_WEIGHT);
    let status_font = status_font(dpi);
    let fmt_left = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;

    paint_header(hdc, l.header, dpi);
    // The folder count lives badged on the section header it counts, which is
    // where it belongs; the incumbent painted it as a bare right-aligned
    // numeral 340px from the label it described.
    paint_section_label(
        hdc,
        l.section,
        dpi,
        "Media folders",
        st.ui_dirs.len() as i32,
    );
    paint_panel(hdc, &l, dpi, st);

    for b in &l.btns {
        let mut label = b.label.clone();
        if b.id == BTN_SCAN && st.scan_active {
            label = scan_button_label(true).to_string();
        }
        let bb = UiButton { label, ..b.clone() };
        paint_button(hdc, &bb, dpi, button_font, st);
    }
    if let Some(frc) = main_focus_rect(&l, st.focus_id) {
        draw_focus_ring(hdc, frc, dpi);
    }
    draw_ui_text(
        hdc,
        &st.status_text,
        l.status,
        fmt_left,
        status_font,
        COL_MUTED,
    );
    paint_scan_bar(hdc, l.status, dpi, st);

    // SAFETY: fonts created above are deleted here.
    unsafe {
        Gdi::DeleteObject(button_font);
        Gdi::DeleteObject(status_font);
    }
}

/// The narrow bar's chip text: the one status the app shows, and it tells the
/// truth. A healthy link shows nothing (no chrome for a healthy default); a
/// down link names the reason in one short muted label. The link half reads
/// the same live mirror the publisher writes
/// ([`app_support::config::live_discord_up`]), never a constant -- so a build
/// with no Discord running cannot claim one.
#[must_use]
pub fn discord_bar_text(st: &UiState) -> String {
    if !st.discord_enabled {
        return "Discord off".to_string();
    }
    if app_support::config::live_discord_up() {
        return String::new();
    }
    "Discord not running".to_string()
}

/// `paintSettingsCard` (`go\window.go:2998`).
/// The narrow bar's one caller: both narrow views route their 48px chrome
/// through here so `narrow::bar` has exactly one caller.
fn paint_narrow_bar(
    hdc: Gdi::HDC,
    bar_rc: Rect,
    dpi: i32,
    st: &UiState,
    gear_rc: Rect,
    gear_hovered: bool,
) {
    // No live Discord handle on a snapshot/desktop run: the chip names the
    // down link; the connected state paints no chrome at all.
    narrow::bar(hdc, bar_rc, dpi, &discord_bar_text(st), gear_rc, gear_hovered);
}

/// Narrow Card: bar, hero, search, results panel to the 24px
/// status bar. No wordmark band, no tab strip, no scope row, no Advanced
/// disclosure, no field toggles/presets (they live in Settings under CARD).
fn paint_narrow_card(hdc: Gdi::HDC, rc: Rect, dpi: i32, st: &UiState, l: &CardLayout) {
    let sc = |v: i32| scale_px(dpi, v);
    let button_font = brand::ui(sc(FS_CONTROL), brand::UI_WEIGHT);
    let body_font = brand::ui(sc(FS_BODY), brand::UI_WEIGHT);
    let hint_font = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
    let status_font = status_font(dpi);
    let fmt_left = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;

    // Bar: gear is the measured 32px rect.
    if let Some(gear) = l.card_btn(BTN_SETTINGS) {
        paint_narrow_bar(
            hdc,
            Rect {
                left: 0,
                top: 0,
                right: rc.right - rc.left,
                bottom: scale_px(dpi, TOPBAR_H_L),
            },
            dpi,
            st,
            gear,
            st.hover_btn == BTN_SETTINGS || st.pressed_btn == BTN_SETTINGS,
        );
    }
    // Hero.
    let (mut model, _have) = card_preview_model(st);
    #[cfg(not(test))]
    if !model.art_url.is_empty() && !crate::artwork::is_cached(&model.art_url) {
        crate::artwork::ensure_async(&model.art_url);
    }
    let cover = if model.art {
        crate::artwork::Cover::load(&model.art_url)
    } else {
        None
    };
    if model.art && cover.is_none() {
        model.art = false;
    }
    let (title, meta, status) = match st.preview_hero_meta() {
        Some((t, m)) => {
            let playing = app_support::config::live_now_playing().is_some();
            (t, m, st.object_status_text(playing))
        }
        None => (
            "Nothing playing".to_string(),
            "Search a title, or start it in PotPlayer.".to_string(),
            "Preview is local; Apply needs playback.".to_string(),
        ),
    };
    // The hero always paints; its ghost exists only while a live title is
    // playing (the layout omits it otherwise, so an empty rect paints no
    // ghost but keeps the object card).
    {
        let fix_rc = l.card_btn(BTN_FIX_MATCH).unwrap_or_default();
        narrow::hero(
            hdc,
            l.object,
            l.obj_cover,
            l.obj_title,
            l.obj_meta,
            l.obj_status,
            fix_rc,
            FIX_MATCH_LABEL,
            dpi,
            &title,
            &meta,
            &status,
            cover.as_ref(),
            st.hover_btn == BTN_FIX_MATCH,
            st.pressed_btn == BTN_FIX_MATCH,
        );
    }
    // Search row: field + 96px red Search.
    for b in &l.btns {
        if b.id == BTN_PICKER_FIELD {
            paint_picker_field(
                hdc,
                b.rc,
                dpi,
                body_font,
                &st.picker,
                st.focus_id == BTN_PICKER_FIELD,
            );
        } else if b.id == BTN_PICKER_SEARCH {
            let mut bb = b.clone();
            bb.label = "Search".to_string();
            bb.style = if st.picker.search_allowed() {
                BTN_ACCENT
            } else {
                BTN_OUTLINE
            };
            paint_button(hdc, &bb, dpi, button_font, st);
        }
    }
    // Results panel filling to the 24px status bar.
    fill_round_rect(hdc, l.stage, COL_CARD, sc(R_PANEL));
    stroke_round_rect_w(hdc, l.stage, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    let n = st.picker.candidates.len();
    let head = Rect {
        left: l.picker_results.left + sc(SP_3),
        top: l.picker_results.top + sc(SP_2),
        right: l.picker_results.right - sc(SP_3),
        bottom: l.picker_results.top + sc(SP_2) + sc(16),
    };
    // The match-count header exists only after a search ran; before the first
    // search the panel keeps only the centred teaching state.
    if st.picker.searched {
        draw_ui_text(
            hdc,
            &format!("{n} match{}", if n == 1 { "" } else { "es" }),
            head,
            crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE,
            hint_font,
            COL_MUTED,
        );
    }
    if n == 0 {
        let title_font = brand::ui(sc(FS_EMPTY_TITLE), brand::UI_WEIGHT);
        let sub_font = brand::ui(sc(FS_EMPTY_SUB), brand::UI_WEIGHT);
        let mid = (l.picker_results.top + l.picker_results.bottom) / 2;
        draw_ui_text(
            hdc,
            "Find the right match",
            Rect {
                left: l.picker_results.left + sc(SP_3),
                top: mid - sc(18),
                right: l.picker_results.right - sc(SP_3),
                bottom: mid,
            },
            crate::win32::DT_CENTER | crate::win32::DT_SINGLE_LINE,
            title_font,
            COL_TEXT,
        );
        draw_ui_text(
            hdc,
            "Type a title and year, then Search.",
            Rect {
                left: l.picker_results.left + sc(SP_3),
                top: mid,
                right: l.picker_results.right - sc(SP_3),
                bottom: mid + sc(18),
            },
            crate::win32::DT_CENTER | crate::win32::DT_SINGLE_LINE,
            sub_font,
            COL_MUTED,
        );
        unsafe {
            Gdi::DeleteObject(title_font);
            Gdi::DeleteObject(sub_font);
        }
    } else {
        // One table: rows paint from the same `picker_row_rects` the hit test
        // and the layout's result buttons read, so pixels and targets agree.
        let top = st
            .picker
            .top
            .min(picker_max_top(n, picker_capacity(l, dpi)));
        for (pos, rc_row) in picker_row_rects(l, n, top, dpi).into_iter().enumerate() {
            let idx = top + pos;
            let Some(c) = st.picker.candidates.get(idx) else {
                break;
            };
            let is_movie = c.kind == Kind::MOVIE;
            let anime = crate::search::is_anime(c);
            let meta = narrow::result_meta(is_movie, anime, c.year, c.episode_count as i32);
            // Covers are always on: kick one background fetch per poster URL,
            // then paint whatever bytes have landed. The hit geometry still
            // reads from the layout table (`picker_row_rects` above).
            #[cfg(not(test))]
            if !c.poster.is_empty() && !crate::artwork::is_cached(&c.poster) {
                crate::artwork::ensure_async(&c.poster);
            }
            let cover = if c.poster.is_empty() {
                None
            } else {
                crate::artwork::Cover::load(&c.poster)
            };
            narrow::result_row(
                hdc,
                rc_row,
                dpi,
                c.title.trim(),
                &meta,
                cover.as_ref(),
                idx == st.picker.selected,
            );
        }
    }
    // Apply pinned to the panel bottom (disabled with no pick).
    for b in &l.btns {
        if b.id == BTN_PICKER_APPLY {
            let mut bb = b.clone();
            bb.style = if st.card_layout_state().can_apply {
                BTN_ACCENT
            } else {
                BTN_OUTLINE
            };
            paint_button(hdc, &bb, dpi, button_font, st);
        }
    }
    if st.focus_id != 0
        && let Some(b) = l.btns.iter().find(|b| b.id == st.focus_id)
    {
        draw_focus_ring(hdc, b.rc, dpi);
    }
    draw_ui_text(
        hdc,
        &st.status_text,
        l.status,
        fmt_left,
        status_font,
        COL_MUTED,
    );
    paint_status_version(hdc, l.status, dpi);
    paint_scan_bar(hdc, l.status, dpi, st);

    // SAFETY: fonts created above.
    unsafe {
        Gdi::DeleteObject(button_font);
        Gdi::DeleteObject(body_font);
        Gdi::DeleteObject(hint_font);
        Gdi::DeleteObject(status_font);
    }
}

/// Narrow Settings: bar, pinned title, scrollable 48px body rows. The last row
/// never paints under the status bar: any row at/below `status_top` is
/// skipped (scroll-reached, not overlapped).
fn paint_narrow_settings(hdc: Gdi::HDC, rc: Rect, dpi: i32, st: &UiState, l: &SettingsLayout) {
    let sc = |v: i32| scale_px(dpi, v);
    let (cw, _ch) = (rc.right - rc.left, rc.bottom - rc.top);
    let button_font = brand::ui(sc(FS_CONTROL), brand::UI_WEIGHT);
    let cap_font = brand::ui(sc(FS_SET_CAP), brand::UI_WEIGHT);
    let hint_font = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
    let title_font = brand::ui(sc(FS_SETTINGS_TITLE), brand::UI_WEIGHT);
    let status_font = status_font(dpi);
    let fmt_left = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;
    let status_top = l.status.top;

    if let Some(gear) = l.settings_btn(BTN_CARD) {
        paint_narrow_bar(
            hdc,
            Rect {
                left: 0,
                top: 0,
                right: cw,
                bottom: sc(TOPBAR_H_L),
            },
            dpi,
            st,
            gear,
            st.hover_btn == BTN_CARD || st.pressed_btn == BTN_CARD,
        );
    }
    // Pinned title.
    if let Some(t) = l.labels.iter().find(|x| x.id == "settings.title") {
        draw_ui_text(hdc, "Settings", t.rc, fmt_left, title_font, COL_TEXT);
    }
    // Caps labels: 11px caps.
    for lab in &l.labels {
        if lab.id == "settings.title" {
            continue;
        }
        let Some(vis) = clip_rect_above_status(lab.rc, status_top) else {
            continue;
        };
        let text = match lab.id.as_str() {
            "settings.lib" => "LIBRARY",
            "settings.behaviour" => "BEHAVIOUR",
            _ => continue,
        };
        draw_ui_text_tracked(
            hdc,
            text,
            vis,
            crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_NO_PREFIX | crate::win32::DT_V_CENTER,
            cap_font,
            COL_MUTED,
            tracked_px(sc(FS_SET_CAP), brand::TRACK_LABEL),
        );
    }
    // Body rows via the one settings_row primitive. The toggle pills read the
    // same stored bools the click flips, so paint and hit-test share one state.
    let discord_on = st.discord_enabled;
    let close_on = st.close_to_tray;
    let row_info = |id: i32| -> Option<(&'static str, &'static str, narrow::NarrowCtl)> {
        match id {
            BTN_ADD => Some(("Add folder", "Choose a folder to watch for new titles", narrow::NarrowCtl::Chevron)),
            BTN_SCAN => Some(("Scan now", "Look for new titles in your folders", narrow::NarrowCtl::Chevron)),
            BTN_REFRESH => Some(("Refresh", "Check your folders for changed titles", narrow::NarrowCtl::Chevron)),
            BTN_REMOVE => Some(("Remove", "Stop watching the selected folder", narrow::NarrowCtl::Chevron)),
            BTN_REPORT => Some(("Report", "Save a report of the last scan", narrow::NarrowCtl::Chevron)),
            BTN_DISCORD => Some((
                "Discord connection",
                "Show what is playing on your profile",
                narrow::NarrowCtl::Toggle(discord_on),
            )),
            BTN_CLOSE => Some((
                "Close to tray",
                "Keep running when the window is closed",
                narrow::NarrowCtl::Toggle(close_on),
            )),
            BTN_STARTUP => Some((
                "Run at startup",
                "Launch Potflix when Windows starts",
                narrow::NarrowCtl::Toggle(st.startup_state),
            )),
            BTN_CLEAR_CACHE => Some(("Clear metadata", "Drop every saved match", narrow::NarrowCtl::Chevron)),
            _ => None,
        }
    };
    // Theme row: label+desc via settings_row(None), segments via buttons.
    // No row paints inside the status bar: a row straddling `status_top`
    // paints only its clipped band above it.
    let mut theme_done = false;
    for b in &l.btns {
        if b.id == BTN_THEME_AMOLED || b.id == BTN_THEME_DARK {
            continue;
        }
        if b.id == BTN_CARD || b.id == BTN_SETTINGS || b.id == BTN_BACK {
            continue;
        }
        let Some((label, desc, ctl)) = row_info(b.id) else {
            continue;
        };
        // Theme band is painted once below; skip its two segment ids here.
        let Some(vis) = clip_rect_above_status(b.rc, status_top) else {
            continue;
        };
        narrow::settings_row(hdc, vis, dpi, label, desc, ctl);
        if b.id == st.hover_btn {
            draw_focus_ring(hdc, vis, dpi);
        }
    }
    if !theme_done
        && let (Some(amoled), Some(dark)) = (l.settings_btn(BTN_THEME_AMOLED), l.settings_btn(BTN_THEME_DARK))
    {
        let top = amoled.top.min(dark.top);
        let band = Rect {
            left: amoled.left.min(dark.left).min(l.theme_title.left),
            top,
            right: amoled.right.max(dark.right),
            bottom: top + sc(48),
        };
        if let Some(vis) = clip_rect_above_status(band, status_top) {
            narrow::settings_row(
                hdc,
                vis,
                dpi,
                "Theme",
                "Pure black for OLED panels",
                narrow::NarrowCtl::None,
            );
            // Two 36px segments right-aligned.
            for b in &l.btns {
                if (b.id == BTN_THEME_AMOLED || b.id == BTN_THEME_DARK)
                    && let Some(seg) = clip_rect_above_status(b.rc, status_top)
                {
                    paint_button(hdc, &UiButton { rc: seg, ..b.clone() }, dpi, button_font, st);
                }
            }
            theme_done = true;
        }
        let _ = theme_done;
    }
    if let Some(vis) = clip_rect_above_status(l.hint, status_top)
        && vis.bottom > vis.top
    {
        draw_ui_text_wrapped(
            hdc,
            crate::settings::SCAN_HINT_TEXT,
            vis,
            hint_font,
            COL_MUTED,
        );
    }
    if let Some(b) = l.btns.iter().find(|b| b.id == st.focus_id)
        && b.rc.bottom > b.rc.top
        && b.rc.top < status_top
    {
        draw_focus_ring(hdc, b.rc, dpi);
    }
    draw_ui_text(
        hdc,
        &st.status_text,
        l.status,
        fmt_left,
        status_font,
        COL_MUTED,
    );
    paint_status_version(hdc, l.status, dpi);
    paint_scan_bar(hdc, l.status, dpi, st);

    // SAFETY: fonts created above.
    unsafe {
        Gdi::DeleteObject(button_font);
        Gdi::DeleteObject(cap_font);
        Gdi::DeleteObject(hint_font);
        Gdi::DeleteObject(title_font);
        Gdi::DeleteObject(status_font);
    }
}

fn paint_settings_card(hdc: Gdi::HDC, rc: Rect, dpi: i32, st: &UiState) {
    let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
    fill_background(hdc, rc);
    let l = settings_layout_scrolled(
        cw,
        ch,
        dpi,
        st.startup_state,
        &st.scan_speed_state,
        &st.theme_state,
        st.settings_scroll,
    );
    paint_narrow_settings(hdc, rc, dpi, st, &l);
}

/// `paintCardView` (`go\window.go:3078`). The media composition: brand art,
/// the exact body lines Discord receives, and a focusable synopsis affordance
/// whose hover/focus tooltip carries the complete stored note. The compact
/// composition drops the chrome band and labels; the wide composition centres
/// the content and enlarges the art/title.
/// Wall-clock milliseconds for the preview time strip.
fn preview_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The shared preview model for the current Card view: the composed (and
/// designer-filtered) activity, the complete local synopsis when the synopsis
/// field is on, and the resolved runtime for the time strip.
fn card_preview_model(st: &UiState) -> (SurfacePreview, bool) {
    let (act, have) = st.preview_activity();
    let synopsis = if st.designer.synopsis {
        st.preview_synopsis()
    } else {
        String::new()
    };
    let duration = parse_length_ms(&st.preview_length());
    // Covers are always on: the artwork toggle is gone, so the model always
    // carries art and the cached-bytes gate below decides the tile.
    let mut model = surface_preview(
        &act,
        have,
        &synopsis,
        preview_now_ms(),
        duration,
        true,
    );
    // A URL is not a cover. Only a locally decodable image turns the slot on,
    // so the blank-tile failure cannot recur while a fetch is pending.
    model.art &= crate::artwork::is_cached(&model.art_url);
    (model, have)
}

/// One clean cover tile: the decoded artwork plus its hairline, never a branded
/// mark, badge or status glyph (owner decision 2026-10-28). The caller only
/// reaches this with a decoded image; a missing cover collapses the slot.
fn paint_cover(hdc: Gdi::HDC, rc: Rect, dpi: i32, cover: Option<&crate::artwork::Cover>) {
    use windows_sys::Win32::Graphics::GdiPlus as gp;
    let sc = |v: i32| scale_px(dpi, v);
    fill_round_rect(hdc, rc, COL_SURFACE, sc(R_TILE));
    if let Some(cover) = cover {
        let g = PAINT_GRAPHICS.with(|c| c.get()) as *mut gp::GpGraphics;
        if !g.is_null() {
            cover.draw(g, rc, sc(R_TILE));
        }
    }
    stroke_round_rect_w(hdc, rc, COL_BORDER, sc(R_TILE), sc(STROKE_HAIR));
}

/// How many result rows the picker viewport can show: the visible fraction of
/// the list, so the scroll offset has something to clamp against. Pure, so the
/// paint, the hit test and the wheel share one capacity. `dpi` scales the
/// geometry; the hit test and the paint pass the same dpi, so pixels and
/// targets share one table.
fn picker_capacity(l: &CardLayout, dpi: i32) -> usize {
    let sc = |v: i32| scale_px(dpi, v);
    if l.picker_results.bottom <= l.picker_results.top {
        return 0;
    }
    let top = l.picker_results.top + sc(24);
    let row_h = sc(44);
    let action_rows = l
        .btns
        .iter()
        .filter(|b| b.id == BTN_FIX_MATCH || b.id == BTN_PICKER_APPLY)
        .count() as i32;
    let reserve = action_rows * (sc(32) + sc(SP_1));
    (((l.picker_results.bottom - top - reserve) / row_h).max(0) as usize).min(4)
}

/// The last scroll offset that still shows a full viewport. Pure, so the wheel
/// and the tests clamp against the same range the paint reads.
fn picker_max_top(count: usize, capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    count.saturating_sub(capacity.min(count.max(1)))
}

/// The visible result rows of the picker overlay. The paint and the hit test
/// read this one table, so a painted row and a clickable row cannot drift.
/// `top` is the scrolled viewport offset; only the visible band is returned so
/// a row can never paint underneath the list's own actions, and every row
/// stays reachable by scrolling `top` through `0..=picker_max_top`. `dpi`
/// must be the same dpi the layout and the paint use.
fn picker_row_rects(l: &CardLayout, count: usize, top: usize, dpi: i32) -> Vec<Rect> {
    let sc = |v: i32| scale_px(dpi, v);
    if count == 0 || l.picker_results.bottom <= l.picker_results.top {
        return Vec::new();
    }
    let capacity = picker_capacity(l, dpi);
    if capacity == 0 {
        return Vec::new();
    }
    let top = top.min(picker_max_top(count, capacity));
    let first = top;
    let last = (first + capacity).min(count);
    let base = l.picker_results.top + sc(24);
    let row_h = sc(44);
    (first..last)
        .enumerate()
        .map(|(pos, _)| Rect {
            left: l.picker_results.left + sc(SP_2),
            top: base + pos as i32 * row_h,
            right: l.picker_results.right - sc(SP_2),
            bottom: base + pos as i32 * row_h + row_h - sc(2),
        })
        .collect()
}

/// The result row under `(x, y)`, or `None`. Returns the absolute candidate
/// index (`top` + viewport position), so a scrolled row still resolves to the
/// record it shows. Reads only `picker_row_rects` with the same `dpi` the
/// layout and the paint use.
fn picker_result_at(
    l: &CardLayout,
    picker: &crate::search::MediaPicker,
    x: i32,
    y: i32,
    dpi: i32,
) -> Option<usize> {
    if !picker.open {
        return None;
    }
    let capacity = picker_capacity(l, dpi);
    let top = picker.top.min(picker_max_top(picker.candidates.len(), capacity));
    picker_row_rects(l, picker.candidates.len(), top, dpi)
        .into_iter()
        .position(|r| pt_in_rect(r, x, y))
        .map(|pos| top + pos)
}

/// The one-field editor. It paints the query left-aligned with a caret at the
/// end, and the placeholder when empty. It is not a stock edit control, so the
/// main window's custom paint path still owns every visible pixel.
fn paint_picker_field(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    font: Gdi::HFONT,
    picker: &crate::search::MediaPicker,
    focused: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    fill_round_rect(hdc, rc, COL_CARD, sc(R_CTRL));
    stroke_round_rect_w(
        hdc,
        rc,
        if focused { STATE_FOCUS } else { COL_BORDER },
        sc(R_CTRL),
        sc(STROKE_HAIR),
    );
    let pad = sc(SP_2);
    // The counter reserves its own band at the right end of the field, so the
    // typed text ellipsizes before it instead of running under it.
    let counter_w = if picker.query.is_empty() { 0 } else { sc(46) };
    let text = if picker.query.is_empty() {
        "Title or title + year"
    } else {
        &picker.query
    };
    let fmt = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;
    draw_ui_text(
        hdc,
        text,
        Rect {
            left: rc.left + pad,
            top: rc.top,
            right: rc.right - pad - counter_w,
            bottom: rc.bottom,
        },
        fmt,
        font,
        if picker.query.is_empty() { COL_MUTED } else { COL_TEXT },
    );
    if counter_w > 0 {
        let at_limit = picker.query_len() >= crate::search::MAX_QUERY_CHARS;
        draw_ui_text(
            hdc,
            &picker.counter_text(),
            Rect {
                left: rc.right - pad - counter_w,
                top: rc.top,
                right: rc.right - pad,
                bottom: rc.bottom,
            },
            crate::win32::DT_RIGHT
                | crate::win32::DT_SINGLE_LINE
                | crate::win32::DT_NO_PREFIX
                | crate::win32::DT_V_CENTER,
            font,
            if at_limit { COL_TEXT } else { COL_MUTED },
        );
    }
    if focused && !picker.busy {
        let mut caret_x = rc.left + pad;
        if !picker.query.is_empty() {
            // Measure the query in the same font the field paints, so the
            // caret sits after the last glyph instead of at a guessed offset.
            unsafe {
                let old = Gdi::SelectObject(hdc, font);
                let u: Vec<u16> = picker.query.encode_utf16().collect();
                let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
                if Gdi::GetTextExtentPoint32W(hdc, u.as_ptr(), u.len() as i32, &mut sz) != 0 {
                    caret_x += sz.cx;
                }
                Gdi::SelectObject(hdc, old);
            }
        }
        caret_x = caret_x.min(rc.right - pad - 1);
        fill_rect_solid(
            hdc,
            Rect {
                left: caret_x,
                top: rc.top + sc(7),
                right: caret_x + sc(1).max(1),
                bottom: rc.bottom - sc(7),
            },
            COL_TEXT,
        );
    }
}

/// The result overlay: a raised panel with one clean row per candidate, the
/// selected row lit, and a one-line instruction. It never paints over the
/// picker band, so the field and the explicit Movie/TV choice stay live while
/// results are open.
fn paint_picker_results(
    hdc: Gdi::HDC,
    l: &CardLayout,
    dpi: i32,
    picker: &crate::search::MediaPicker,
    body: Gdi::HFONT,
    hint: Gdi::HFONT,
) {
    if !picker.open || picker.candidates.is_empty() || l.picker_results.bottom <= l.picker_results.top {
        return;
    }
    let sc = |v: i32| scale_px(dpi, v);
    fill_round_rect(hdc, l.picker_results, COL_CARD, sc(R_PANEL));
    stroke_round_rect_w(hdc, l.picker_results, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    let head = Rect {
        left: l.picker_results.left + sc(SP_3),
        top: l.picker_results.top + sc(SP_2),
        right: l.picker_results.right - sc(SP_3),
        bottom: l.picker_results.top + sc(SP_2) + sc(16),
    };
    draw_ui_text(
        hdc,
        "Pick a match",
        head,
        crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE,
        hint,
        COL_MUTED,
    );
    let rows = picker_row_rects(l, picker.candidates.len(), picker.top.min(picker_max_top(picker.candidates.len(), picker_capacity(l, dpi))), dpi);
    let top = picker.top.min(picker_max_top(picker.candidates.len(), picker_capacity(l, dpi)));
    for (pos, rc) in rows.iter().enumerate() {
        let Some(c) = picker.candidates.get(top + pos) else {
            break;
        };
        let (title, sub) = fix_match_row_lines(c);
        paint_fix_match_row(
            hdc,
            *rc,
            dpi,
            body,
            hint,
            &title,
            &sub,
            top + pos == picker.selected,
        );
    }
    // Position-only scrollbar in the panel's right gutter when rows hide below
    // the viewport: the same track/thumb vocabulary as the Main folder list.
    // The thumb is paint, not drag.
    let capacity = picker_capacity(l, dpi);
    let count = picker.candidates.len() as i32;
    if count > capacity as i32 && capacity > 0 {
        let sb_w = sc(SCROLLBAR_W);
        let inset = sc(SCROLLBAR_INSET);
        let track = Rect {
            left: l.picker_results.right - inset,
            top: l.picker_results.top + sc(SP_2),
            right: l.picker_results.right - inset + sb_w,
            bottom: rows.last().map_or(l.picker_results.top + sc(SP_2), |r| r.bottom),
        };
        let thumb = scrollbar_thumb(
            track,
            count,
            capacity as i32,
            top as i32,
            sc(SCROLLBAR_MIN_THUMB),
        );
        fill_round_rect(hdc, track, crate::brand::TRACK, sc(R_BAR));
        if thumb.right > thumb.left && thumb.bottom > thumb.top {
            fill_round_rect(hdc, thumb, COL_MUTED, sc(R_BAR));
        }
    }
    let foot = Rect {
        left: l.picker_results.left + sc(SP_3),
        top: rows.last().map_or(head.bottom, |r| r.bottom + sc(SP_1)),
        // The list's own buttons sit on the panel's last 32/64 px; the hint
        // must not paint over them.
        right: l.picker_results.right - sc(SP_3),
        bottom: l
            .card_btn(BTN_PICKER_APPLY)
            .or_else(|| l.card_btn(BTN_FIX_MATCH))
            .map_or(l.picker_results.bottom - sc(SP_2), |rc| rc.top - sc(SP_1)),
    };
    if foot.bottom > foot.top {
        let hint_text = if l.card_btn(BTN_PICKER_APPLY).is_some() {
            "Up/Down previews \u{b7} Apply changes the playing title \u{b7} Esc closes"
        } else {
            "Up/Down previews the row \u{b7} nothing is sent \u{b7} Esc closes"
        };
        draw_ui_text(
            hdc,
            hint_text,
            foot,
            crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS,
            hint,
            COL_MUTED,
        );
    }
}

/// The narrow time strip: a 4px track, the elapsed fraction in white, and the
/// two labels under it. Used by the profile and Activity surfaces.
fn paint_preview_timer(
    hdc: Gdi::HDC,
    rect: Rect,
    dpi: i32,
    clock: &PreviewClock,
    hint: Gdi::HFONT,
) {
    if !clock.timer || rect.bottom <= rect.top {
        return;
    }
    let sc = |v: i32| scale_px(dpi, v);
    let bar_h = sc(4);
    let track = Rect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.top + bar_h,
    };
    fill_round_rect(hdc, track, crate::brand::TRACK, bar_h / 2);
    if clock.fraction > 0.0 {
        let w = ((track.right - track.left) as f64 * clock.fraction.clamp(0.0, 1.0)) as i32;
        if w > 0 {
            let fill = Rect {
                right: track.left + w,
                ..track
            };
            fill_round_rect(hdc, fill, COL_TEXT, bar_h / 2);
        }
    }
    let labels = Rect {
        top: track.bottom + sc(2),
        bottom: rect.bottom,
        ..rect
    };
    let fmt = crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER | crate::win32::DT_END_ELLIPSIS;
    draw_ui_text(hdc, &clock.left, labels, fmt, hint, COL_MUTED);
    let right = Rect {
        left: labels.left + (labels.right - labels.left) / 2,
        ..labels
    };
    draw_ui_text(
        hdc,
        &clock.right,
        right,
        crate::win32::DT_RIGHT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_NO_PREFIX
            | crate::win32::DT_V_CENTER | crate::win32::DT_END_ELLIPSIS,
        hint,
        COL_MUTED,
    );
}

/// The member-list surface: the documented `Watching <title>` line under the
/// member name, the details/state echo, and a progress line. The placement is
/// the inferred mock, not a Discord pixel contract.
#[allow(clippy::too_many_arguments)]
fn paint_member_surface(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    p: &SurfacePreview,
    have: bool,
    body: Gdi::HFONT,
    hint: Gdi::HFONT,
    label: Gdi::HFONT,
) {
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    let sc = |v: i32| scale_px(dpi, v);
    fill_round_rect(hdc, rc, COL_CARD, sc(R_PANEL));
    stroke_round_rect_w(hdc, rc, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    let pad = sc(SP_3);
    let head = Rect {
        left: rc.left + pad,
        top: rc.top + sc(SP_2),
        right: rc.right - pad,
        bottom: rc.top + sc(SP_2) + sc(16),
    };
    draw_ui_text(hdc, "Members", head, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE, label, COL_MUTED);
    if preview_is_empty(have, p) {
        // No object: say there is nothing to preview instead of painting an
        // empty member row that looks like a failed render.
        draw_ui_text(
            hdc,
            PREVIEW_EMPTY_TEXT,
            Rect {
                top: head.bottom + sc(SP_2),
                bottom: (head.bottom + sc(SP_2) + sc(18)).min(rc.bottom - sc(SP_2)),
                ..head
            },
            crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS,
            body,
            COL_MUTED,
        );
        return;
    }
    let row_top = head.bottom + sc(SP_2);
    let avatar = Rect {
        left: rc.left + pad,
        top: row_top,
        right: rc.left + pad + sc(30),
        bottom: row_top + sc(30),
    };
    fill_round_rect(hdc, avatar, COL_SURFACE, sc(R_CTRL));
    let fmt_c = crate::win32::DT_CENTER | crate::win32::DT_SINGLE_LINE | crate::win32::DT_V_CENTER;
    draw_ui_text(hdc, "You", avatar, fmt_c, hint, COL_MUTED);
    let text_left = avatar.right + sc(SP_2);
    let name = Rect {
        left: text_left,
        top: row_top,
        right: rc.right - pad,
        bottom: row_top + sc(16),
    };
    draw_ui_text(hdc, "You", name, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE, hint, COL_MUTED);
    let watching = Rect {
        left: text_left,
        top: name.bottom + sc(1),
        right: rc.right - pad,
        bottom: name.bottom + sc(1) + sc(18),
    };
    draw_ui_text(hdc, &p.header, watching, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS, body, COL_TEXT);
    let mut echo = String::new();
    for line in [&p.details, &p.state] {
        if !line.is_empty() {
            if !echo.is_empty() {
                echo.push_str(" \u{b7} ");
            }
            echo.push_str(line);
        }
    }
    if !echo.is_empty() {
        let e = Rect {
            left: text_left,
            top: watching.bottom,
            right: rc.right - pad,
            bottom: watching.bottom + sc(14),
        };
        draw_ui_text(hdc, &echo, e, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS, hint, COL_MUTED);
    }
    if p.clock.timer {
        let bar = Rect {
            left: text_left,
            top: watching.bottom + sc(18),
            right: rc.right - pad,
            bottom: (watching.bottom + sc(18) + sc(30)).min(rc.bottom - sc(SP_2)),
        };
        paint_preview_timer(hdc, bar, dpi, &p.clock, hint);
    }
}

/// The profile/expanded-card surface: banner, identity, then the activity block
/// with the cover, `Watching <title>`, the two body lines and the time strip.
#[allow(clippy::too_many_arguments)]
fn paint_profile_surface(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    p: &SurfacePreview,
    have: bool,
    art: Rect,
    cover: Option<&crate::artwork::Cover>,
    header: Rect,
    details: Rect,
    state: Rect,
    body: Gdi::HFONT,
    hint: Gdi::HFONT,
    label: Gdi::HFONT,
    title: Gdi::HFONT,
    identity: bool,
) {
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    let sc = |v: i32| scale_px(dpi, v);
    fill_round_rect(hdc, rc, COL_CARD, sc(R_PANEL));
    stroke_round_rect_w(hdc, rc, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    let pad = sc(SP_4);
    if preview_is_empty(have, p) {
        // Nothing recorded and nothing selected: the surface states that, so a
        // cold card never renders as a blank panel under the identity block.
        draw_ui_text(
            hdc,
            PREVIEW_EMPTY_TEXT,
            Rect {
                left: rc.left + pad,
                top: rc.top + sc(SP_4),
                right: rc.right - pad,
                bottom: (rc.top + sc(SP_4) + sc(18)).min(rc.bottom - sc(SP_2)),
            },
            crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS,
            body,
            COL_MUTED,
        );
        return;
    }
    if identity {        let banner = Rect {
            left: rc.left,
            top: rc.top,
            right: rc.right,
            bottom: (rc.top + sc(30)).min(rc.bottom),
        };
        fill_rect_solid(hdc, banner, COL_SURFACE);
        let avatar = Rect {
            left: rc.left + pad,
            top: rc.top + sc(18),
            right: rc.left + pad + sc(46),
            bottom: rc.top + sc(18) + sc(46),
        };
        fill_round_rect(hdc, avatar, COL_CARD, sc(R_CTRL));
        stroke_round_rect_w(hdc, avatar, COL_MUTED, sc(R_CTRL), sc(STROKE_HAIR));
        draw_ui_text(hdc, "You", avatar, crate::win32::DT_CENTER | crate::win32::DT_SINGLE_LINE | crate::win32::DT_V_CENTER, hint, COL_MUTED);
        let name_top = avatar.bottom + sc(SP_1);
        let name = Rect {
            left: rc.left + pad,
            top: name_top,
            right: rc.right - pad,
            bottom: name_top + sc(18),
        };
        draw_ui_text(hdc, "You", name, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE, label, COL_TEXT);
        let handle = Rect {
            top: name.bottom,
            bottom: (name.bottom + sc(14)).min(rc.bottom),
            ..name
        };
        // The handle line stays empty: this build exposes no connected Discord
        // identity to the window, so nothing is invented and `You` stands in.
        let _ = handle;
    }
    if p.art {
        paint_cover(hdc, art, dpi, cover);
    }
    // With no artwork the cover column collapses: the text takes the full
    // panel width instead of leaving a dead gutter.
    let (header, details, state) = if p.art {
        (header, details, state)
    } else {
        let full = |r: Rect| Rect {
            left: rc.left + pad,
            right: rc.right - pad,
            ..r
        };
        (full(header), full(details), full(state))
    };
    paint_media_title(hdc, &p.header, header, dpi, COL_TEXT, title, false);
    if !p.details.is_empty() {
        draw_ui_text(hdc, &p.details, details, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS, body, COL_MUTED);
    }
    if !p.state.is_empty() {
        draw_ui_text(hdc, &p.state, state, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS, body, COL_MUTED);
    }
    if p.clock.timer {
        let bar = Rect {
            left: header.left,
            top: state.bottom + sc(SP_2),
            right: rc.right - pad,
            bottom: (state.bottom + sc(SP_2) + sc(30)).min(rc.bottom - sc(SP_3)),
        };
        paint_preview_timer(hdc, bar, dpi, &p.clock, hint);
    }
}

/// The Activity-tab surface: the hero (cover + lines + time) and the complete
/// local synopsis body. Discord has no synopsis field; this body never leaves
/// Potflix.
#[allow(clippy::too_many_arguments)]
fn paint_activity_surface(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    p: &SurfacePreview,
    have: bool,
    cover: Option<&crate::artwork::Cover>,
    body: Gdi::HFONT,
    hint: Gdi::HFONT,
    label: Gdi::HFONT,
    title: Gdi::HFONT,
    synopsis_on: bool,
) {
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    let sc = |v: i32| scale_px(dpi, v);
    fill_round_rect(hdc, rc, COL_CARD, sc(R_PANEL));
    stroke_round_rect_w(hdc, rc, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    let pad = sc(SP_4);
    let head = Rect {
        left: rc.left + pad,
        top: rc.top + sc(SP_3),
        right: rc.right - pad,
        bottom: rc.top + sc(SP_3) + sc(18),
    };
    draw_ui_text(hdc, "Activity", head, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE, label, COL_MUTED);
    if preview_is_empty(have, p) {
        // The synopsis block belongs to a title. With no title there is no
        // "No synopsis for this title." to print and no hero to draw: the
        // surface says the one true thing instead.
        draw_ui_text(
            hdc,
            PREVIEW_EMPTY_TEXT,
            Rect {
                top: head.bottom + sc(SP_3),
                bottom: (head.bottom + sc(SP_3) + sc(18)).min(rc.bottom - sc(SP_2)),
                ..head
            },
            crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS,
            body,
            COL_MUTED,
        );
        return;
    }
    let hero_top = head.bottom + sc(SP_3);
    let art_w = sc(76);
    let art = Rect {
        left: rc.left + pad,
        top: hero_top,
        right: (rc.left + pad + art_w).min(rc.right - pad),
        bottom: (hero_top + sc(112)).min(rc.bottom - pad),
    };
    if p.art {
        paint_cover(hdc, art, dpi, cover);
    }
    let text_left = activity_text_left(rc, dpi, p.art);
    let hdr = activity_title_band(rc, dpi, p.art);
    paint_media_title(hdc, &p.header, hdr, dpi, COL_TEXT, title, true);
    let det = Rect {
        top: hdr.bottom + sc(1),
        bottom: hdr.bottom + sc(1) + sc(18),
        ..hdr
    };
    if !p.details.is_empty() {
        draw_ui_text(hdc, &p.details, det, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS, body, COL_MUTED);
    }
    let sta = Rect {
        top: det.bottom,
        bottom: det.bottom + sc(18),
        ..hdr
    };
    if !p.state.is_empty() {
        draw_ui_text(hdc, &p.state, sta, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE | crate::win32::DT_END_ELLIPSIS, body, COL_MUTED);
    }
    if p.clock.timer {
        let bar = Rect {
            left: text_left,
            top: sta.bottom + sc(SP_1),
            right: rc.right - pad,
            bottom: sta.bottom + sc(SP_1) + sc(28),
        };
        paint_preview_timer(hdc, bar, dpi, &p.clock, hint);
    }
    if !synopsis_on {
        return;
    }
    let syn_top = if p.art {
        (art.bottom + sc(SP_3)).min(rc.bottom - pad)
    } else {
        (sta.bottom + sc(SP_3)).max(hero_top + sc(SP_3))
    };
    let syn_label = Rect {
        left: rc.left + pad,
        top: syn_top,
        right: rc.right - pad,
        bottom: (syn_top + sc(16)).min(rc.bottom),
    };
    draw_ui_text(hdc, "Synopsis \u{b7} local only", syn_label, crate::win32::DT_LEFT | crate::win32::DT_SINGLE_LINE, hint, COL_MUTED);
    let syn_body = Rect {
        left: rc.left + pad,
        top: syn_label.bottom + sc(SP_1),
        right: rc.right - pad,
        bottom: rc.bottom - sc(SP_3),
    };
    if syn_body.bottom > syn_body.top {
        let text = if p.synopsis.is_empty() {
            "No synopsis for this title."
        } else {
            &p.synopsis
        };
        draw_ui_text_wrapped(hdc, text, syn_body, body, COL_MUTED);
    }
}

fn paint_card_view(hdc: Gdi::HDC, rc: Rect, dpi: i32, st: &UiState) {
    let sc = |v: i32| scale_px(dpi, v);
    let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
    fill_background(hdc, rc);

    let mut l = compute_card_layout_state(
        cw,
        ch,
        dpi,
        &st.card_specs_state,
        st.displayed_card_kind(),
        st.preview_surface,
        &st.card_layout_state(),
    );
    if !l.wide {
        paint_narrow_card(hdc, rc, dpi, st, &l);
        return;
    }
    for b in l.btns.iter_mut() {
        if b.id == BTN_CARD || b.id == BTN_SETTINGS {
            b.style = if b.id == BTN_CARD {
                BTN_TAB_SEL
            } else {
                BTN_TAB
            };
        }
        if b.id == BTN_ADVANCED {
            b.label = if st.advanced_open {
                "Advanced \u{25be}".to_string()
            } else {
                "Advanced \u{25b8}".to_string()
            };
        }
        if b.id == BTN_PICKER_SEARCH {
            // One verb, one label, always: Search searches. A second name on
            // the same control was the "second match action" the owner named.
            b.label = "Search".to_string();
            b.style = if st.picker.search_allowed() {
                BTN_ACCENT
            } else {
                BTN_OUTLINE
            };
        }
    }

    let (mut model, have) = card_preview_model(st);
    // Artwork is lazy and off-thread: a visible card starts at most one fetch
    // for its URL, and the worker posts back when the cover is cached. The
    // render tests compile this out, so no test touches the network.
    #[cfg(not(test))]
    if !model.art_url.is_empty() && !crate::artwork::is_cached(&model.art_url) {
        crate::artwork::ensure_async(&model.art_url);
    }
    let cover = if model.art {
        crate::artwork::Cover::load(&model.art_url)
    } else {
        None
    };
    if model.art && cover.is_none() {
        // A cache file that no longer decodes is an absence, not a blank tile.
        model.art = false;
    }
    let button_font = brand::ui(sc(FS_CONTROL), brand::UI_WEIGHT);
    let body_font = brand::ui(sc(FS_BODY), brand::UI_WEIGHT);
    let hint_font = brand::ui(sc(FS_HINT), brand::UI_WEIGHT);
    let status_font = status_font(dpi);
    let title_font = brand::display(sc(FS_TITLE));
    let label_font = brand::display(sc(FS_LABEL));
    let fmt_left = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER
        | crate::win32::DT_END_ELLIPSIS;

    // The rail surface first: its labels are painted on top of it, and the
    // object card inside it must not be covered by its fill.
    if l.wide {
        fill_round_rect(hdc, l.rail, COL_SURFACE, sc(R_PANEL));
        stroke_round_rect_w(hdc, l.rail, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    }
    // --- The one object this window is about ------------------------------
    // Cover, title, year/kind and the plain connection state. Everything below
    // is either this object's presence preview or a correction to it.
    fill_round_rect(hdc, l.object, COL_SURFACE, sc(R_PANEL));
    stroke_round_rect_w(hdc, l.object, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    paint_cover(hdc, l.obj_cover, dpi, cover.as_ref());
    match st.preview_hero_meta() {
        Some((title, meta)) => {
            draw_ui_text(hdc, &title, l.obj_title, fmt_left, title_font, COL_TEXT);
            // The meta line is the narrow hero's own string: one formatter
            // ([`hero_meta_for`] via [`UiState::preview_hero_meta`]), so the
            // wide card cannot drop the runtime the narrow card shows.
            draw_ui_text(hdc, &meta, l.obj_meta, fmt_left, body_font, COL_MUTED);
            // The one sentence that decides whether this is a real presence or
            // a preview that was never sent. A local preview says so in words.
            let playing = app_support::config::live_now_playing().is_some();
            draw_ui_text(
                hdc,
                &st.object_status_text(playing),
                l.obj_status,
                fmt_left,
                hint_font,
                COL_MUTED,
            );
        }
        None => {
            draw_ui_text(
                hdc,
                "Nothing playing",
                l.obj_title,
                fmt_left,
                title_font,
                COL_TEXT,
            );
            draw_ui_text(
                hdc,
                "Search a title, or start it in PotPlayer.",
                l.obj_meta,
                fmt_left,
                body_font,
                COL_MUTED,
            );
            draw_ui_text(
                hdc,
                "Preview is local; Apply needs playback.",
                l.obj_status,
                fmt_left,
                hint_font,
                COL_MUTED,
            );
        }
    }

    if l.header.bottom > l.header.top {
        paint_header(hdc, l.header, dpi);
    }
    // The rail surface is painted above the object card so the card is not
    // covered; nothing to fill here.
    for lab in &l.labels {
        match lab.id.as_str() {
            "card.preview-label" => draw_ui_text(hdc, "Discord preview", lab.rc, fmt_left, hint_font, COL_MUTED),
            "card.designer" => draw_ui_text(hdc, "RPC designer", lab.rc, fmt_left, title_font, COL_TEXT),
            "card.context" => {
                let context = if have {
                    let kind = if st.displayed_card_kind().is_movie() {
                        "Film"
                    } else {
                        "TV episode"
                    };
                    format!("{} \u{b7} {}", model.header.trim_start_matches("Watching "), kind)
                } else {
                    "Nothing playing".to_string()
                };
                draw_ui_text(hdc, &context, lab.rc, fmt_left, hint_font, COL_MUTED);
            }
            "card.fields" => paint_section_label(hdc, lab.rc, dpi, "Fields to send", 0),
            "card.line2" => paint_section_label(hdc, lab.rc, dpi, "Line 2 - state", 0),
            "card.wire" => paint_section_label(hdc, lab.rc, dpi, "Wire", 0),
            _ => {}
        }
    }

    if l.wide {
        let [member, profile, activity] = l.surfaces;
        paint_member_surface(hdc, member, dpi, &model, have, body_font, hint_font, label_font);
        paint_profile_surface(
            hdc,
            profile,
            dpi,
            &model,
            have,
            l.art,
            cover.as_ref(),
            l.title,
            l.meta,
            l.synopsis,
            body_font,
            hint_font,
            label_font,
            title_font,
            true,
        );
        paint_activity_surface(
            hdc,
            activity,
            dpi,
            &model,
            have,
            cover.as_ref(),
            body_font,
            hint_font,
            label_font,
            title_font,
            st.designer.synopsis,
        );
        if have {
            // The selected surface carries a quiet accent edge; the segment
            // row above is the control that moves it.
            let sel = match st.preview_surface {
                PreviewSurface::Member => member,
                PreviewSurface::Profile => profile,
                PreviewSurface::Activity => activity,
            };
            stroke_round_rect_w(hdc, sel, COL_ACCENT, sc(R_PANEL), sc(STROKE_FOCUS));
        }
        // The wire summary: exactly the fields the composer will send.
        let wire_fmt = crate::win32::DT_LEFT
            | crate::win32::DT_SINGLE_LINE
            | crate::win32::DT_NO_PREFIX
            | crate::win32::DT_END_ELLIPSIS;
        let w = l.wire;
        let row_h = (w.bottom - w.top) / 3;
        let mut rows = Vec::new();
        if have {
            let (act, _) = st.preview_activity();
            rows.push(format!("name: {}", act.name));
            rows.push(format!(
                "details: {}  \u{b7}  state: {}",
                if act.details.is_empty() { "-" } else { &act.details },
                if act.state.is_empty() { "-" } else { &act.state }
            ));
            let art = if act.large_image.is_empty() {
                "none"
            } else if model.art {
                "poster"
            } else {
                "loading"
            };
            rows.push(format!(
                "art: {art}  \u{b7}  timers: {}",
                if act.has_start { "on" } else { "off" }
            ));
        } else {
            rows.push("name: -".to_string());
            rows.push("details: -  \u{b7}  state: -".to_string());
            rows.push("art: -  \u{b7}  timers: -".to_string());
        }
        for (i, row) in rows.iter().enumerate() {
            let r = Rect {
                left: w.left,
                top: w.top + i as i32 * row_h,
                right: w.right,
                bottom: w.top + (i as i32 + 1) * row_h,
            };
            draw_ui_text(hdc, row, r, wire_fmt, hint_font, COL_MUTED);
        }
    } else {
        match st.preview_surface {
            PreviewSurface::Member => {
                paint_member_surface(hdc, l.preview, dpi, &model, have, body_font, hint_font, label_font)
            }
            PreviewSurface::Profile => paint_profile_surface(
                hdc,
                l.preview,
                dpi,
                &model,
                have,
                l.art,
                cover.as_ref(),
                l.title,
                l.meta,
                l.synopsis,
                body_font,
                hint_font,
                label_font,
                title_font,
                false,
            ),
            PreviewSurface::Activity => paint_activity_surface(
                hdc,
                l.preview,
                dpi,
                &model,
                have,
                cover.as_ref(),
                body_font,
                hint_font,
                label_font,
                title_font,
                st.designer.synopsis,
            ),
        }
    }

    paint_picker_results(hdc, &l, dpi, &st.picker, body_font, hint_font);

    if l.picker_status.right > l.picker_status.left && !st.picker.status.is_empty() {
        draw_ui_text(
            hdc,
            &st.picker.status,
            l.picker_status,
            fmt_left,
            hint_font,
            if st.picker.status.starts_with("Couldn't") {
                COL_DANGER
            } else {
                COL_MUTED
            },
        );
    }

    for b in &l.btns {
        if b.id == BTN_PICKER_FIELD {
            paint_picker_field(
                hdc,
                b.rc,
                dpi,
                body_font,
                &st.picker,
                st.focus_id == BTN_PICKER_FIELD,
            );
        } else {
            paint_button(hdc, b, dpi, button_font, st);
        }
    }

    if st.focus_id != 0
        && let Some(b) = l.btns.iter().find(|b| b.id == st.focus_id)
    {
        draw_focus_ring(hdc, b.rc, dpi);
    }
    if l.status.bottom > l.status.top {
        draw_ui_text(
            hdc,
            &st.status_text,
            l.status,
            fmt_left,
            status_font,
            COL_MUTED,
        );
        paint_scan_bar(hdc, l.status, dpi, st);
    }
    if l.hint.bottom > l.hint.top {
        draw_ui_text(
            hdc,
            "Toggles change the payload; every preview shares it.",
            l.hint,
            fmt_left,
            hint_font,
            COL_MUTED,
        );
    }

    // SAFETY: fonts created above are deleted here.
    unsafe {
        Gdi::DeleteObject(button_font);
        Gdi::DeleteObject(body_font);
        Gdi::DeleteObject(hint_font);
        Gdi::DeleteObject(status_font);
        Gdi::DeleteObject(title_font);
        Gdi::DeleteObject(label_font);
    }
}

// ---------------------------------------------------------------------------
// Window creation and the main wndproc (`go\window.go:284-462`).
// ---------------------------------------------------------------------------

/// `mainClassName` (`go\window.go:35`).
pub const MAIN_CLASS_NAME: &str = "PotflixMainWnd";

static UI: std::sync::LazyLock<Mutex<UiState>> =
    std::sync::LazyLock::new(|| Mutex::new(UiState::default()));

/// The process-wide UI state the wndproc drives.
pub fn ui() -> MutexGuard<'static, UiState> {
    UI.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The app's poll-loop entry: prefetch whatever is playing now, so the "Fix
/// match" list is already populated when the card opens. A repeated call for
/// the same title|year|scope returns before any network.
pub fn prefetch_now_playing() {
    ui().prefetch_now_playing();
}

/// Test-only seam: when armed, [`run_blocking_ui`] hands the action to this
/// callback instead of opening the real modal (`SHBrowseForFolderW` /
/// `MessageBoxW`) or shelling out. The callback runs at exactly the point the
/// real blocking call would, so it can observe whether the process-wide `UI`
/// mutex is free there.
#[cfg(test)]
static TEST_BLOCKING_PROBE: Mutex<Option<fn(BlockingUi)>> = Mutex::new(None);

/// Test-only seam for the one blocking action NOT routed through
/// [`run_blocking_ui`]: [`show_main_window`]'s `center_window`/`ShowWindow`
/// step. When armed, the callback runs at that exact blocking point instead of
/// making the Win32 calls, so a test can observe whether `UI` is free there.
#[cfg(test)]
static TEST_SHOW_PROBE: Mutex<Option<fn()>> = Mutex::new(None);

/// Serializes the tests that drive the process-wide `UI` global through the
/// real wndproc: they set `scan_active` and the shared snapshot on `ui()`,
/// which any concurrent global-state test would observe mid-flight.
#[cfg(test)]
static UI_GLOBAL_SERIAL: Mutex<()> = Mutex::new(());

/// Runs a [`BlockingUi`] action with `UI` RELEASED. Every arm either opens a
/// modal dialog or makes a Win32 call that can pump/dispatch a message and
/// re-enter `main_wnd_proc`; the guard must be dropped (never `ui()`) across
/// that call, then re-taken only for the short, non-blocking apply step.
fn run_blocking_ui(hwnd: HWND, action: BlockingUi) {
    #[cfg(test)]
    if let Some(probe) = *TEST_BLOCKING_PROBE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        probe(action);
        return;
    }
    match action {
        BlockingUi::AddDir => {
            let path = crate::settings::browse_for_folder(hwnd);
            if !path.is_empty() {
                ui().apply_add_dir(&path);
            }
        }
        BlockingUi::ClearMetadata => {
            if crate::settings::confirm_clear(
                hwnd,
                "Potflix - Clear metadata",
                crate::settings::clear_metadata_confirm_text(),
            ) {
                ui().apply_clear_metadata();
            }
        }
        BlockingUi::ClearIndex => {
            if crate::settings::confirm_clear(
                hwnd,
                "Potflix - Clear index",
                crate::settings::clear_index_confirm_text(),
            ) {
                ui().apply_clear_index();
            }
        }
        BlockingUi::ExportBundle => {
            let dest = crate::settings::save_bundle_dialog(hwnd);
            if !dest.is_empty() {
                ui().apply_export_bundle(&dest);
            }
        }
        BlockingUi::ImportBundle => {
            let src = crate::settings::open_bundle_dialog(hwnd);
            if !src.is_empty() {
                ui().apply_import_bundle(hwnd, &src);
            }
        }
        BlockingUi::OpenLogFolder => crate::settings::on_open_log_folder(),
        BlockingUi::OpenFolder => {
            // The row is read under a short guard, then the shell call runs
            // with the guard released, like every other arm.
            let dir = {
                let st = ui();
                let idx = clamp_sel_row(st.sel_row, st.ui_dirs.len());
                if idx < 0 {
                    None
                } else {
                    st.ui_dirs.get(idx as usize).cloned()
                }
            };
            if let Some(dir) = dir {
                let _ = open_external_url(&dir);
            }
        }
        BlockingUi::HideWindow => {
            // SAFETY: ShowWindow only hides the window.
            unsafe {
                crate::win32::ShowWindow(hwnd, crate::win32::SW_HIDE);
            }
        }
        BlockingUi::FixMatch => open_fix_match(),
    }
}

/// Pre-22000 fallback for the immersive dark caption: attribute 19 was
/// `DWMWA_USE_IMMERSIVE_DARK_MODE` before it became 20 on Windows 11 build
/// 22000. Best-effort like [`crate::win32::apply_dark_caption`]: returns the
/// `DwmSetWindowAttribute` HRESULT for the proof, discarded by the caller.
pub const DWMWA_DARK_MODE_FALLBACK_19: i32 = 19;

/// Best-effort pre-22000 immersive-dark flag; returns the HRESULT.
#[must_use]
pub fn apply_dark_mode_fallback_19(hwnd: HWND) -> i32 {
    let value: u32 = 1;
    // SAFETY: DWM copies the 4-byte value; the HRESULT needs no cleanup.
    unsafe {
        windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            DWMWA_DARK_MODE_FALLBACK_19 as u32,
            std::ptr::from_ref(&value).cast(),
            size_of::<u32>() as u32,
        )
    }
}

/// The one window style: a normal resizable caption window. `WS_THICKFRAME`
/// and `WS_MAXIMIZEBOX` let the owner size the media card from its compact
/// minimum to a maximized desktop; `WM_GETMINMAXINFO` below enforces the
/// view-aware minimum.
fn main_window_style() -> u32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{WS_MAXIMIZEBOX, WS_THICKFRAME};
    crate::win32::WS_CAPTION
        | crate::win32::WS_SYS_MENU
        | crate::win32::WS_MINIMIZE_BOX
        | WS_THICKFRAME
        | WS_MAXIMIZEBOX
}

/// The outer track minimum for the current view at `dpi`: the Card view can
/// shrink to the compact media row; Main/Settings keep their full stack.
fn min_track_size(view: View, dpi: i32) -> (i32, i32) {
    let (cw, ch) = if view == View::Card {
        (CARD_MIN_W, CARD_MIN_H)
    } else {
        (APP_MIN_W, APP_MIN_H)
    };
    let mut r = wr(Rect {
        left: 0,
        top: 0,
        right: scale_px(dpi, cw),
        bottom: scale_px(dpi, ch),
    });
    // SAFETY: AdjustWindowRectEx only writes the rect.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::AdjustWindowRectEx(
            &mut r,
            main_window_style(),
            0,
            0,
        );
    }
    (r.right - r.left, r.bottom - r.top)
}

/// `createMainWindow` (`go\window.go:284`).
pub fn create_main_window() -> Result<(), String> {    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AdjustWindowRectEx, CreateWindowExW, RegisterClassExW, WNDCLASSEXW,
    };
    // The theme is live before the first brush/paint: AMOLED when absent or
    // unreadable, Dark only when stored.
    set_live_theme(app_support::config::load_config().effective_theme());
    // SAFETY: COM init for the folder picker; a failure is non-fatal.
    unsafe {
        crate::win32::OleInitialize(std::ptr::null());
    }
    let hinst = unsafe { crate::win32::GetModuleHandleW(std::ptr::null()) };
    let class = utf16_z(MAIN_CLASS_NAME);
    let cursor = unsafe {
        crate::win32::LoadCursorW(
            std::ptr::null_mut(),
            crate::win32::IDC_ARROW as usize as *const u16,
        )
    };
    let bg = unsafe { crate::win32::CreateSolidBrush(themed(COL_BG)) };
    // The Ribbon P, loaded from the embedded `.ico` BEFORE the class is
    // registered: a class's icon slots are read at registration, and a class
    // that declares none gets the generic application icon from the window
    // manager -- which is exactly the defect this fixes.
    let mark = brand::mark_icon();
    let mark_small = brand::mark_icon_small();

    let mut wc: WNDCLASSEXW = unsafe { std::mem::zeroed() };
    wc.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    wc.lpfnWndProc = Some(main_wnd_proc);
    wc.hInstance = hinst;
    wc.hCursor = cursor;
    wc.hbrBackground = bg;
    wc.hIcon = mark;
    wc.hIconSm = if mark_small.is_null() {
        mark
    } else {
        mark_small
    };
    wc.lpszClassName = class.as_ptr();
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err("RegisterClassExW failed".into());
    }

    let dpi = system_dpi();
    let cw = scale_px(dpi, MAIN_CLIENT_W);
    let ch = scale_px(dpi, MAIN_CLIENT_H);
    let style = main_window_style();
    let mut wrc = wr(Rect {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    });
    unsafe {
        AdjustWindowRectEx(&mut wrc, style, 0, 0);
    }
    let title = utf16_z("Potflix");
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            style,
            crate::win32::CW_USEDEFAULT,
            crate::win32::CW_USEDEFAULT,
            wrc.right - wrc.left,
            wrc.bottom - wrc.top,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        return Err("CreateWindowExW failed".into());
    }
    // Rust-only chrome: pin the OS caption, title text and 1px frame to the
    // dark palette so the stock white title bar never appears. `WS_CAPTION`
    // stays (custom-drawn chrome would re-open the reentrancy class of
    // `3503ddb`): `DwmSetWindowAttribute` copies four bytes and dispatches
    // nothing to this wndproc, so this is NOT a re-entrant site -- no `UI`
    // guard or regression-test change (cf. `run_blocking_ui`).
    crate::win32::apply_dark_caption(hwnd, themed(COL_BG), COL_TEXT);
    // Pre-22000 DWM reads the same flag at 19 instead of 20; best-effort.
    let _ = apply_dark_mode_fallback_19(hwnd);
    // Apply the mark to the WINDOW slots as well as the class slots. The class
    // slots are what `GetClassLongPtrW(GCLP_HICON)` reports; `WM_SETICON` is
    // what the titlebar, alt-tab and the taskbar actually read, and the two are
    // separate. Both are set from the one asset, and the handle the write
    // produced is recorded so the proof can quote it.
    let class_icon = brand::apply_window_icon(hwnd);
    {
        let mut st = ui();
        st.main_hwnd = hwnd as usize;
        st.ui_icon = class_icon as usize;
        // The stored theme is live from the first frame (not only after the
        // Settings view loads it).
        st.theme_state = app_support::config::load_config()
            .effective_theme()
            .to_string();
        set_live_theme(&st.theme_state);
        // Production is the only place the persistent search cache is live: a
        // unit test's stub search never reads or writes the owner's file.
        st.cache_enabled = true;
        st.picker_search = Arc::new(default_picker_search);
        st.refresh_dir_list();
        // The first frame a user sees: search while nothing is playing, the
        // library while something is (the shipped behaviour for a player).
        st.open_cold_view_with(app_support::config::live_now_playing().is_some());
    }
    if class_icon.is_null() {
        return Err("the Ribbon P could not be applied to the window".into());
    }
    app_logf(&format!(
        "window icon: class HICON={class_icon:p} brand={} fonts={}",
        brand::DISPLAY_FAMILY,
        brand::fonts_installed()
    ));
    Ok(())
}

/// `showMainWindow` (`go\window.go:346`).
pub fn show_main_window() {
    // `UI` is RELEASED before every Win32 call below. `center_window`'s
    // `MoveWindow` and `UpdateWindow` both synchronously re-enter
    // `main_wnd_proc` (`WM_WINDOWPOSCHANGED`/`WM_SIZE`, `WM_PAINT`), whose
    // handlers re-lock `UI` via `ui()`. Holding this non-reentrant guard across
    // them self-deadlocks the UI thread -- the shipped blank-white "(Not
    // Responding)" startup hang. Go's `showMainWindow` (`go\window.go:346`) holds
    // no lock, so this is a port-introduced regression.
    let (hwnd, needs_center) = {
        let mut st = ui();
        if st.main_hwnd == 0 {
            return;
        }
        let hwnd = st.main_hwnd as HWND;
        let needs_center = !st.ui_centered;
        if needs_center {
            st.ui_centered = true;
        }
        (hwnd as HWND, needs_center)
    };
    #[cfg(test)]
    if let Some(probe) = *TEST_SHOW_PROBE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        probe();
        return;
    }
    if needs_center {
        center_window(hwnd);
    }
    // SAFETY: ShowWindow/UpdateWindow/SetForegroundWindow only affect `hwnd`.
    unsafe {
        crate::win32::ShowWindow(hwnd, crate::win32::SW_SHOW);
        crate::win32::UpdateWindow(hwnd);
        crate::win32::SetForegroundWindow(hwnd);
    }
}

/// `centerWindow` (`go\window.go:360`).
pub fn center_window(hwnd: HWND) {
    use windows_sys::Win32::Foundation::RECT;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: read-only geometry queries + a MoveWindow.
    unsafe {
        if crate::win32::GetWindowRect(hwnd, &mut rc) == 0 {
            return;
        }
        let win_w = rc.right - rc.left;
        let win_h = rc.bottom - rc.top;
        let sw = crate::win32::GetSystemMetrics(crate::win32::SM_CX_SCREEN);
        let sh = crate::win32::GetSystemMetrics(crate::win32::SM_CY_SCREEN);
        if sw == 0 || sh == 0 {
            return;
        }
        let x = (sw - win_w) / 2;
        let mut y = (sh - win_h) / 2;
        if y < 0 {
            y = 0;
        }
        crate::win32::MoveWindow(hwnd, x, y, win_w, win_h, 1);
    }
}

/// `mouseXY` (`go\window.go:3442`).
#[must_use]
pub fn mouse_xy(lparam: usize) -> (i32, i32) {
    let x = (lparam & 0xFFFF) as u16 as i16;
    let y = ((lparam >> 16) & 0xFFFF) as u16 as i16;
    (i32::from(x), i32::from(y))
}

/// `mainWndProc` (`go\window.go:380`).
///
/// # Safety
/// Called by the OS as the window procedure for `MAIN_CLASS_NAME`; `hwnd` must be
/// the live main window handle passed at class registration.
pub unsafe extern "system" fn main_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    match msg {
        crate::win32::WM_CLOSE => {
            // Closing parks the window back to the tray -- but only while there
            // IS a tray icon to come back from. A tray-less process (the
            // notify-icon add failed) keeps the window as its only way in and
            // its only way out, so hiding it there is a zombie: no icon, no
            // window, and the single-instance port still held, so a relaunch
            // answers "already running" and exits. `QA7-G1`.
            if crate::tray::tray_icon_added() {
                unsafe {
                    crate::win32::ShowWindow(hwnd, crate::win32::SW_HIDE);
                }
            } else {
                // Quit the way the tray Exit does: the flag stops the poll loop
                // and the quit message ends the message loop, then the app tail
                // clears, deletes the icon and closes Discord in order.
                crate::tray::request_quit();
                unsafe {
                    crate::win32::DestroyWindow(hwnd);
                }
            }
            return 0;
        }
        crate::win32::WM_ERASE_BKGND => return 1,
        crate::win32::WM_PAINT => {
            paint_main_window(hwnd);
            return 0;
        }
        crate::win32::WM_PRINT | crate::win32::WM_PRINTCLIENT => {
            // `PrintWindow` renders through `WM_PRINT`; `DefWindowProcW` draws
            // only the frame, so the client area is ours to paint into the
            // caller's DC. Without this every headless frame is an empty
            // bitmap, whatever the theme.
            let hdc = wparam as windows_sys::Win32::Graphics::Gdi::HDC;
            if !hdc.is_null() {
                paint_to_hdc(hdc, get_client_rect(hwnd), window_dpi(hwnd));
            }
            return 0;
        }
        crate::win32::WM_SIZE | crate::win32::WM_DPI_CHANGED => {
            ui().invalidate();
            return 0;
        }
        windows_sys::Win32::UI::WindowsAndMessaging::WM_GETMINMAXINFO => {
            if lparam != 0 {
                #[allow(clippy::cast_ptr_alignment)]
                let mmi = unsafe {
                    &mut *(lparam
                        as *mut windows_sys::Win32::UI::WindowsAndMessaging::MINMAXINFO)
                };
                let (w, h) = min_track_size(ui().cur_view, window_dpi(hwnd));
                mmi.ptMinTrackSize.x = w;
                mmi.ptMinTrackSize.y = h;
            }
            return 0;
        }
        crate::win32::WM_MOUSE_MOVE => {
            let (x, y) = mouse_xy(lparam as usize);
            let mut st = ui();
            st.last_mx = x;
            st.last_my = y;
            st.have_mouse = true;
            track_mouse_leave(hwnd);
            if st.update_hover(hwnd, x, y) {
                st.invalidate();
            }
            return 0;
        }
        crate::win32::WM_MOUSE_LEAVE => {
            let mut st = ui();
            if st.hover_btn != 0 || st.hover_row != -1 || st.hover_x {
                st.hover_btn = 0;
                st.hover_row = -1;
                st.hover_x = false;
                st.invalidate();
            }
            return 0;
        }
        crate::win32::WM_L_BUTTON_DOWN => {
            let (x, y) = mouse_xy(lparam as usize);
            ui().press_at(hwnd, x, y);
            return 0;
        }
        crate::win32::WM_L_BUTTON_UP => {
            let (x, y) = mouse_xy(lparam as usize);
            // The action dispatch may open a modal dialog / show the window,
            // which pumps the queue and re-enters this wndproc: the `UI` guard
            // is dropped before `run_blocking_ui` (never held across it).
            let blocking = ui().release_at(hwnd, x, y);
            if let Some(b) = blocking {
                run_blocking_ui(hwnd, b);
            }
            return 0;
        }
        crate::win32::WM_MOUSEWHEEL => {
            // Wheel over the picker list scrolls it; over the folder list it
            // scrolls that; anywhere else (or an unscrollable list) it is a
            // no-op. Both scrolls only invalidate (async), so this is NOT a
            // re-entrant site.
            let notches = crate::win32::wheel_delta(wparam) / crate::win32::WHEEL_DELTA;
            let card_picker = {
                let st = ui();
                st.cur_view == View::Card && st.picker.open
            };
            if card_picker {
                ui().scroll_picker(hwnd, -notches);
                return 0;
            }
            if ui().cur_view == View::Settings {
                let px = scale_px(window_dpi(hwnd), 48) * 3;
                ui().scroll_settings(hwnd, -notches * px);
                return 0;
            }
            ui().scroll_list(hwnd, -notches * UiState::WHEEL_SCROLL_ROWS);
            return 0;
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            ui().invalidate();
            return 0;
        }
        WM_CHAR => {
            if ui().on_card_char(wparam as u32) {
                return 0;
            }
            return 0;
        }
        WM_KEYDOWN => {
            let blocking = {
                let mut st = ui();
                let vk = wparam as u32;
                if st.on_settings_key_down(hwnd, vk) {
                    st.invalidate();
                }
                // `VK_RETURN` may focus a button whose action is blocking; run
                // it after the guard drops (`run_blocking_ui`).
                st.pending_blocking.take()
            };
            if let Some(b) = blocking {
                run_blocking_ui(hwnd, b);
            }
            return 0;
        }
        crate::win32::WM_SET_CURSOR => {
            let st = ui();
            if st.have_mouse && st.hit_test_code(hwnd, st.last_mx, st.last_my) != 0 {
                unsafe {
                    let hand = crate::win32::LoadCursorW(
                        std::ptr::null_mut(),
                        crate::win32::IDC_HAND as usize as *const u16,
                    );
                    if !hand.is_null() {
                        crate::win32::SetCursor(hand);
                        return 1;
                    }
                }
            }
            return unsafe { crate::win32::DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        WM_APP_SCAN_PROGRESS => {
            let mut st = ui();
            st.publish_scan_progress();
            st.refresh_scan_live_text(hwnd, ScanLifecycleEvent::Progress, Instant::now());
            return 0;
        }
        WM_TIMER => {
            if wparam == SCAN_TIMER_ID {
                let mut st = ui();
                if st.scan_active {
                    st.refresh_scan_live_text(hwnd, ScanLifecycleEvent::Tick, Instant::now());
                }
                return 0;
            }
        }
        WM_APP_SCAN_DONE => {
            let mut st = ui();
            st.publish_scan_done();
            st.on_scan_done(hwnd);
            return 0;
        }
        WM_APP_REPORT_DONE => {
            ui().on_report_done();
            return 0;
        }
        WM_APP_PICKER_DONE => {
            ui().on_picker_job_done();
            return 0;
        }
        WM_APP_ARTWORK_READY => {
            ui().invalidate();
            return 0;
        }
        WM_APP_PRELOAD_DONE => {
            ui().on_preload_done();
            return 0;
        }
        _ => {}
    }
    unsafe { crate::win32::DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// `trackMouseLeave` (`go\window.go:1735`).
fn track_mouse_leave(hwnd: HWND) {
    let mut tm = crate::win32::TrackMouseEvent {
        cb_size: std::mem::size_of::<crate::win32::TrackMouseEvent>() as u32,
        dw_flags: crate::win32::TME_LEAVE,
        hwnd_track: hwnd as usize,
        dw_hover_time: 0,
    };
    // SAFETY: struct is live for the call (same ABI as TRACKMOUSEEVENT).
    unsafe {
        crate::win32::TrackMouseEvent(
            std::ptr::from_mut(&mut tm)
                .cast::<windows_sys::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>(),
        );
    }
}

// ===TESTS===

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;

    struct Scratch {
        dir: std::path::PathBuf,
    }
    impl Scratch {
        fn new(tag: &str) -> Self {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir =
                std::env::temp_dir().join(format!("potflix-win-{tag}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
        fn path(&self) -> &std::path::Path {
            &self.dir
        }
        fn join(&self, rel: &str) -> std::path::PathBuf {
            self.dir.join(rel)
        }
        fn write(&self, name: &str, contents: &str) -> std::path::PathBuf {
            let p = self.dir.join(name);
            std::fs::write(&p, contents).unwrap();
            p
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn r_eq(a: Rect, b: Rect) -> bool {
        a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
    }

    fn dirs(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("{}:\\Media", char::from(b'A' + (i % 26) as u8)))
            .collect()
    }

    const NULL: HWND = std::ptr::null_mut();

    // -- window_test.go -----------------------------------------------------

    // -- window_test.go -----------------------------------------------------

    #[test]
    fn pt_in_rect_boundaries() {
        let r = Rect {
            left: 10,
            top: 20,
            right: 30,
            bottom: 40,
        };
        for p in [(10, 20), (29, 39), (15, 25)] {
            assert!(pt_in_rect(r, p.0, p.1), "inside {p:?}");
        }
        for p in [(9, 20), (10, 19), (30, 20), (10, 40), (0, 0), (100, 100)] {
            assert!(!pt_in_rect(r, p.0, p.1), "outside {p:?}");
        }
    }

    #[test]
    fn button_at_point_lookup() {
        let btns = vec![
            UiButton {
                id: BTN_ADD,
                label: "Add folder".into(),
                style: BTN_SUBTLE,
                rc: Rect {
                    left: 20,
                    top: 400,
                    right: 224,
                    bottom: 436,
                },
            },
            UiButton {
                id: BTN_REMOVE,
                label: "Remove".into(),
                style: BTN_SUBTLE,
                rc: Rect {
                    left: 236,
                    top: 400,
                    right: 440,
                    bottom: 436,
                },
            },
            UiButton {
                id: BTN_SCAN,
                label: "Scan".into(),
                style: BTN_ACCENT,
                rc: Rect {
                    left: 20,
                    top: 448,
                    right: 224,
                    bottom: 484,
                },
            },
        ];
        assert_eq!(button_at_point(&btns, 100, 410), BTN_ADD);
        assert_eq!(button_at_point(&btns, 300, 410), BTN_REMOVE);
        assert_eq!(button_at_point(&btns, 100, 460), BTN_SCAN);
        assert_eq!(button_at_point(&btns, 230, 410), 0);
        assert_eq!(button_at_point(&btns, 10, 10), 0);
        assert_eq!(button_at_point(&[], 100, 410), 0);
    }

    #[test]
    fn row_at_point_lookup() {
        let rows = vec![
            Rect {
                left: 30,
                top: 128,
                right: 430,
                bottom: 164,
            },
            Rect {
                left: 30,
                top: 172,
                right: 430,
                bottom: 208,
            },
        ];
        assert_eq!(row_at_point(&rows, 100, 140), 0);
        assert_eq!(row_at_point(&rows, 100, 180), 1);
        assert_eq!(row_at_point(&rows, 100, 168), -1);
        assert_eq!(row_at_point(&[], 100, 140), -1);
    }

    #[test]
    fn compute_layout_buttons() {
        let l = compute_layout(&dirs(2), MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        // Two tab segments plus the two remaining window actions (Report,
        // Close to tray). The scan and folder controls are gone by owner
        // order (2026-11-03).
        assert_eq!(l.btns.len(), 4);
        let mut seen = std::collections::HashSet::new();
        for b in &l.btns {
            assert!(seen.insert(b.id), "duplicate id {}", b.id);
            assert!(
                b.rc.left >= 0
                    && b.rc.top >= 0
                    && b.rc.right <= MAIN_CLIENT_W
                    && b.rc.bottom <= MAIN_CLIENT_H,
                "button {} escapes the client: {:?}",
                b.id,
                b.rc
            );
        }
        for id in [BTN_REPORT, BTN_CLOSE] {
            assert!(seen.contains(&id), "button {id} missing");
        }
        for (i, a) in l.btns.iter().enumerate() {
            for b in l.btns.iter().skip(i + 1) {
                assert!(
                    !rects_intersect(a.rc, b.rc),
                    "buttons {} and {} overlap",
                    a.id,
                    b.id
                );
            }
        }
        assert_eq!(l.rows.len(), 2);
        for (i, r) in l.rows.iter().enumerate() {
            assert_eq!(
                row_at_point(&l.rows, (r.left + r.right) / 2, (r.top + r.bottom) / 2),
                i as i32
            );
        }
        for b in &l.btns {
            assert_eq!(
                button_at_point(
                    &l.btns,
                    (b.rc.left + b.rc.right) / 2,
                    (b.rc.top + b.rc.bottom) / 2
                ),
                b.id
            );
        }
    }

    /// The action grid is ONE grid: equal columns, one column gap, one row gap,
    /// all six actions on it, and exactly one primary control.
    #[test]
    fn action_grid_is_one_grid() {
        for dpi in [96, 120, 144, 192] {
            let cw = scale_px(dpi, MAIN_CLIENT_W);
            let ch = scale_px(dpi, MAIN_CLIENT_H);
            let l = compute_layout(&dirs(2), cw, ch, dpi);
            let sc = |v: i32| scale_px(dpi, v);
            let margin = sc(MAIN_MARGIN);
            let want_w = ((cw - margin * 2) - sc(BTN_GAP_L)) / 2;
            let want_h = sc(BTN_H);
            let gap = sc(BTN_GAP_L);

            let footer: Vec<&UiButton> = l
                .btns
                .iter()
                .filter(|b| b.style != BTN_TAB && b.style != BTN_TAB_SEL)
                .collect();
            assert_eq!(footer.len(), 2, "dpi {dpi}: folder run controls are gone");
            let (a, b) = (footer[0], footer[1]);
            assert_eq!(a.rc.top, b.rc.top, "dpi {dpi}: one level row");
            assert_eq!(a.rc.bottom, b.rc.bottom);
            assert_eq!(a.rc.right - a.rc.left, want_w, "dpi {dpi}: col 0 width");
            assert_eq!(b.rc.right - b.rc.left, want_w, "dpi {dpi}: col 1 width");
            assert_eq!(a.rc.bottom - a.rc.top, want_h);
            assert_eq!(a.rc.left, margin, "dpi {dpi}: col 0 left");
            assert_eq!(b.rc.left, margin + want_w + gap, "dpi {dpi}: col 1 left");
            assert!(
                l.btns.iter().all(|x| x.style != BTN_ACCENT),
                "dpi {dpi}: nothing scans, so nothing is the primary action"
            );
        }
    }

    /// The tab strip is a segmented control: two equal segments spanning the
    /// content width, contiguous, in the strip band -- not the incumbent's two
    /// bare right-aligned text links.
    #[test]
    fn tab_strip_is_a_segmented_control() {
        for dpi in [96, 120, 144, 192] {
            let cw = scale_px(dpi, MAIN_CLIENT_W);
            let ch = scale_px(dpi, MAIN_CLIENT_H);
            let l = compute_layout(&dirs(2), cw, ch, dpi);
            let sc = |v: i32| scale_px(dpi, v);
            let margin = sc(MAIN_MARGIN);
            let _ = cw;
            let card = l.card_link;
            let settings = l.settings;
            assert_eq!(card.left, margin, "dpi {dpi}");
            assert_eq!(
                settings.right,
                margin + (card.right - card.left) * 2 + sc(SP_2),
                "dpi {dpi}: the strip spans both segments plus the one gap"
            );
            assert_eq!(
                card.right + sc(SP_2),
                settings.left,
                "dpi {dpi}: one gap between the segments"
            );
            assert_eq!(
                card.right - card.left,
                settings.right - settings.left,
                "dpi {dpi}: segments are equal"
            );
            assert_eq!(card.top, sc(TAB_TOP_L), "dpi {dpi}");
            assert_eq!(card.bottom, sc(TAB_TOP_L) + sc(TAB_H_L), "dpi {dpi}");
            // The strip sits below the header band and above the section label.
            assert!(card.top >= l.header.bottom, "dpi {dpi}");
            assert!(card.bottom <= l.section.top, "dpi {dpi}");
            for b in l
                .btns
                .iter()
                .filter(|b| b.style == BTN_TAB || b.style == BTN_TAB_SEL)
            {
                let want = if b.id == BTN_CARD { card } else { settings };
                assert!(
                    r_eq(b.rc, want),
                    "dpi {dpi}: segment {} off the strip",
                    b.id
                );
            }
            // The folder count's badge shares the section line, at its right end.
            let rects = main_text_rects(cw, ch, dpi, &l, 2);
            let sec = rects.iter().find(|r| r.id == "main.section").unwrap();
            let badge = rects.iter().find(|r| r.id == "main.badge").unwrap();
            assert_eq!(badge.rc.right, l.section.right, "dpi {dpi}");
            assert!(
                sec.rc.right < badge.rc.left,
                "dpi {dpi}: label and badge overlap"
            );
            assert!(!rects_intersect(sec.rc, badge.rc));
        }
    }

    #[test]
    fn compute_layout_card_follows_rows() {
        let card_h = |n: usize| {
            let d = dirs(n);
            let l = compute_layout(&d, MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
            let h = l.card.bottom - l.card.top;
            (l, h)
        };
        let (l1, h1) = card_h(1);
        let (l4, h4) = card_h(4);
        assert!(h1 < h4, "1-row {h1} vs 4-row {h4}");
        assert!(l1.rows.len() == 1 && l4.rows.len() == 4);
        let (l20, h20) = card_h(20);
        let (_, h5) = card_h(5);
        // The panel grows to the height the action grid leaves it, then clamps:
        // the viewport holds `list_capacity` rows and the rest is `+N more`.
        assert!(
            h20 > h5,
            "the 20-folder panel {h20} must exceed the 5-folder {h5}"
        );
        assert_eq!(l20.rows.len(), l20.list_capacity as usize);
        assert_eq!(l20.list_capacity, 7, "the grid leaves room for 7 rows");
        assert_eq!(l20.overflow, 20 - 7);
        let grid_top = l20
            .btns
            .iter()
            .filter(|b| b.style != BTN_TAB && b.style != BTN_TAB_SEL)
            .map(|b| b.rc.top)
            .min()
            .unwrap();
        assert!(l20.card.bottom <= grid_top, "panel overruns the grid");
        for (i, r) in l20.rows.iter().enumerate() {
            assert_eq!(
                row_at_point(&l20.rows, (r.left + r.right) / 2, (r.top + r.bottom) / 2),
                i as i32
            );
        }
        for (a, b) in l1.btns.iter().zip(l20.btns.iter()) {
            assert!(r_eq(a.rc, b.rc), "button {} moved", a.id);
        }
        assert!(r_eq(l1.status, l20.status));
    }

    #[test]
    fn main_settings_affordance() {
        let l = compute_layout(&dirs(2), MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(l.btns.len(), 4);
        let s = l.settings;
        assert!(s.right <= MAIN_CLIENT_W && s.bottom <= MAIN_CLIENT_H && s.left >= 0 && s.top >= 0);
        assert!(s.right > s.left && s.bottom > s.top);
        assert!(
            s.top >= TAB_TOP_L && s.bottom <= TAB_TOP_L + TAB_H_L,
            "settings tab {s:?} leaves the tab strip"
        );
        assert!(
            s.top >= l.header.bottom,
            "the tab strip must sit below the header band"
        );
        // The tab segment is itself a control, so it is excluded from the
        // overlap scan the way every other control is compared.
        for b in l
            .btns
            .iter()
            .filter(|b| b.style != BTN_TAB && b.style != BTN_TAB_SEL)
        {
            assert!(
                !rects_intersect(s, b.rc),
                "settings overlaps button {}",
                b.id
            );
        }
        for r in &l.rows {
            assert!(!rects_intersect(s, *r));
        }
        assert!(!rects_intersect(s, l.status));
        assert!(!rects_intersect(s, l.card));
    }

    #[test]
    fn view_switch() {
        let mut st = UiState::default();
        assert_eq!(st.cur_view, View::Main);
        st.open_settings();
        assert_eq!(st.cur_view, View::Settings);
        st.close_settings();
        assert_eq!(st.cur_view, View::Main);
    }

    #[test]
    fn tab_target_names_only_the_tabs() {
        assert_eq!(UiState::tab_target(BTN_CARD), Some(View::Card));
        assert_eq!(UiState::tab_target(BTN_SETTINGS), Some(View::Settings));
        for id in [0, BTN_ADD, BTN_SCAN, BTN_BACK, BTN_CARD_BACK] {
            assert_eq!(UiState::tab_target(id), None, "id {id} is not a tab");
        }
    }

    #[test]
    fn tab_switching_shares_one_route() {
        // From every view, activating a tab lands on its view through the one
        // `activate_focused` route (the keyboard path); the mouse path in
        // `release_at` funnels Settings/Card through the same call.
        for view in [View::Main, View::Settings, View::Card] {
            let mut st = UiState::default();
            st.cur_view = view;
            let _ = st.activate_focused(NULL, BTN_CARD);
            assert_eq!(st.cur_view, View::Card, "from {view:?} BTN_CARD");
        }
        for view in [View::Main, View::Settings, View::Card] {
            let mut st = UiState::default();
            st.cur_view = view;
            let _ = st.activate_focused(NULL, BTN_SETTINGS);
            assert_eq!(st.cur_view, View::Settings, "from {view:?} BTN_SETTINGS");
        }
        // Same-view clicks are no-ops: the view stays and the per-view
        // transient state the `enter_*` functions would reset is untouched.
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.hover_btn = 99;
        st.pressed_btn = 99;
        st.status_text = "sentinel".into();
        let _ = st.activate_focused(NULL, BTN_CARD);
        assert_eq!(st.cur_view, View::Card);
        assert_eq!(
            st.hover_btn, 99,
            "same-view Card click must not reset hover"
        );
        assert_eq!(st.pressed_btn, 99);
        assert_eq!(st.status_text, "sentinel");
        let mut st = UiState::default();
        st.cur_view = View::Settings;
        st.hover_btn = 77;
        st.pressed_btn = 77;
        st.status_text = "sentinel".into();
        let _ = st.activate_focused(NULL, BTN_SETTINGS);
        assert_eq!(st.cur_view, View::Settings);
        assert_eq!(
            st.hover_btn, 77,
            "same-view Settings click must not reset hover"
        );
        assert_eq!(st.pressed_btn, 77);
        assert_eq!(st.status_text, "sentinel");
    }

    #[test]
    fn settings_hit_test() {
        let l = compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, false, "balanced", "amoled");
        // Approved narrow, five rows: Discord connection, Close to tray, Run
        // at startup, Theme (AMOLED|Dark segments), Clear metadata -- plus the
        // Card gear and the 2 hidden wiring rects. Everything else (Metadata
        // cache, Card layout, the Discord field toggles, the line-2 presets,
        // Clear index, Export, Import, Open log, Scan speed) is gone by owner
        // order (2026-11-03).
        assert_eq!(l.btns.len(), 9);
        for id in [
            BTN_DISCORD,
            BTN_CLOSE,
            BTN_STARTUP,
            BTN_THEME_AMOLED,
            BTN_THEME_DARK,
            BTN_CLEAR_CACHE,
            BTN_CARD,
        ] {
            let rc = l
                .settings_btn(id)
                .unwrap_or_else(|| panic!("id {id} missing"));
            // Scrollable body: horizontal bounds hold, vertical may extend past
            // the client; hidden wiring rects are empty and skipped here.
            if rc.right <= rc.left || rc.bottom <= rc.top {
                continue;
            }
            assert!(
                rc.right <= MAIN_CLIENT_W + 1 && rc.left >= -1 && rc.top >= 0,
                "id {id} escapes horizontally: {rc:?}"
            );
            // Below-the-fold rows are scroll-reached: no hit-test at the folded
            // viewport (the clamped point would hit a different visible row).
            if rc.top >= MAIN_CLIENT_H {
                continue;
            }
            assert_eq!(
                button_at_point(&l.btns, (rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2),
                id
            );
        }
        // Visible buttons never overlap (empty wiring rects excluded: they have
        // no area and `rects_intersect` already ignores them, but filter for
        // clarity).
        let visible: Vec<_> = l
            .btns
            .iter()
            .filter(|b| b.rc.right > b.rc.left && b.rc.bottom > b.rc.top)
            .collect();
        for (i, a) in visible.iter().enumerate() {
            for b in visible.iter().skip(i + 1) {
                // Rows below the fold share the scroll body; only same-viewport
                // overlap is a defect. Both below the client is not overlap.
                if a.rc.top >= MAIN_CLIENT_H && b.rc.top >= MAIN_CLIENT_H {
                    continue;
                }
                assert!(
                    !rects_intersect(a.rc, b.rc),
                    "{} and {} overlap",
                    a.id,
                    b.id
                );
            }
        }
        let off = compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, false, "balanced", "amoled");
        let on = compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, true, "balanced", "amoled");
        assert!(r_eq(
            off.settings_btn(BTN_STARTUP).unwrap(),
            on.settings_btn(BTN_STARTUP).unwrap()
        ));
        assert_eq!(
            off.btns.iter().find(|b| b.id == BTN_STARTUP).unwrap().label,
            "[ ] Start with Windows"
        );
        assert_eq!(
            on.btns.iter().find(|b| b.id == BTN_STARTUP).unwrap().label,
            "[x] Start with Windows"
        );
        // LIBRARY group holds exactly one row: Clear metadata.
        let lib_idx = l.labels.iter().position(|x| x.id == "settings.lib").unwrap();
        let lib_top = l.labels[lib_idx].rc.bottom;
        let rc = l.settings_btn(BTN_CLEAR_CACHE).unwrap();
        assert!(rc.top >= lib_top, "Clear metadata left the LIBRARY group: {rc:?}");
        for id in [BTN_CLEAR_INDEX, BTN_EXPORT, BTN_IMPORT, BTN_OPEN_LOG] {
            assert!(
                l.settings_btn(id).is_none(),
                "library id {id} is gone from Settings"
            );
        }
        // The startup toggle spans the content width, one row under its label.
        let beh_idx = l.labels.iter().position(|x| x.id == "settings.behaviour").unwrap();
        let rc = l.settings_btn(BTN_STARTUP).unwrap();
        assert_eq!(rc.left, l.labels[beh_idx].rc.left);
        assert_eq!(rc.right, l.labels[beh_idx].rc.right);
        assert!(rc.top >= l.labels[beh_idx].rc.bottom);
        // Status bar is pinned full-bleed 24px.
        assert_eq!(l.status.bottom - l.status.top, STATUS_BAR_H_L);
        assert_eq!(l.status.bottom, MAIN_CLIENT_H);
    }

    #[test]
    fn layout_text_invariants() {
        for dpi in [96, 120, 144, 192] {
            let cw = scale_px(dpi, MAIN_CLIENT_W);
            let ch = scale_px(dpi, MAIN_CLIENT_H);
            for n in [0usize, 2, 20] {
                let l = compute_layout(&dirs(n), cw, ch, dpi);
                let rects = main_text_rects(cw, ch, dpi, &l, n as i32);
                for r in &rects {
                    assert!(
                        r.rc.right - r.rc.left > 0 && r.rc.bottom - r.rc.top > 0,
                        "{} non-positive",
                        r.id
                    );
                    assert!(
                        r.rc.left >= 0 && r.rc.top >= 0 && r.rc.right <= cw && r.rc.bottom <= ch,
                        "{} escapes",
                        r.id
                    );
                }
                for (i, a) in rects.iter().enumerate() {
                    for b in rects.iter().skip(i + 1) {
                        assert!(
                            !rects_intersect(a.rc, b.rc),
                            "dpi {dpi} dirs {n}: {} and {} intersect",
                            a.id,
                            b.id
                        );
                    }
                }
            }
            let sl = compute_settings_layout(cw, ch, dpi, false, "balanced", "amoled");
            let srects = settings_text_rects(cw, ch, dpi, &sl);
            for r in &srects {
                // Scrollable body: hidden wiring rects are empty by design.
                if r.rc.right <= r.rc.left || r.rc.bottom <= r.rc.top {
                    continue;
                }
                assert!(
                    r.rc.right - r.rc.left > 0 && r.rc.bottom - r.rc.top > 0,
                    "dpi {dpi}: {} non-positive",
                    r.id
                );
                assert!(
                    r.rc.left >= 0 && r.rc.top >= 0 && r.rc.right <= cw,
                    "dpi {dpi}: {} escapes horizontally",
                    r.id
                );
            }
            for (i, a) in srects.iter().enumerate() {
                for b in srects.iter().skip(i + 1) {
                    // Scrollable body slides under the pinned status bar and
                    // title: body-vs-chrome overlap is the scroll, not a defect.
                    let chrome = |id: &str| {
                        id == "settings.status" || id == "settings.title"
                    };
                    if chrome(&a.id) || chrome(&b.id) {
                        continue;
                    }
                    // Below-the-fold scroll rows share the body; only same-
                    // viewport overlap is a defect.
                    if a.rc.top >= ch && b.rc.top >= ch {
                        continue;
                    }
                    // Hidden wiring rects have no area and never intersect.
                    if a.rc.right <= a.rc.left
                        || a.rc.bottom <= a.rc.top
                        || b.rc.right <= b.rc.left
                        || b.rc.bottom <= b.rc.top
                    {
                        continue;
                    }
                    assert!(
                        !rects_intersect(a.rc, b.rc),
                        "dpi {dpi}: {} and {} intersect",
                        a.id,
                        b.id
                    );
                }
            }
        }
    }

    #[test]
    fn settings_esc_returns_to_main() {
        let mut st = UiState::default();
        st.cur_view = View::Settings;
        assert!(st.on_settings_key_down(NULL, VK_ESCAPE));
        assert_eq!(st.cur_view, View::Main);
        st.cur_view = View::Main;
        assert!(!st.on_settings_key_down(NULL, VK_ESCAPE));
    }

    /// Approved narrow: no scope row and no artwork toggle. The Card is one
    /// query field, one Search, one result list, hero Fix, pinned Apply and
    /// Change filters; fields/presets moved to Settings CARD and never
    /// appear on the Card.
    #[test]
    fn card_list_actions_are_tab_stops_with_advanced_closed() {
        // The pure order: no scope segments, both hero/panel actions Tab stops.
        let closed = card_focus_order_for(false, true, true);
        for id in [
            BTN_FIX_MATCH,
            BTN_PICKER_APPLY,
        ] {
            assert!(closed.contains(&id), "a visible Card action {id} is a Tab stop");
        }
        // The removed scope ids (85/81/82/87/88) are gone: no scope segment
        // is ever a Tab stop again.
        for id in [85, 81, 82, 87, 88] {
            assert!(
                !closed.contains(&id),
                "removed scope id {id} is not a Tab stop"
            );
        }
        for id in [
            BTN_FIELD_DETAILS,
            BTN_FIELD_STATE,
            BTN_FIELD_SYNOPSIS,
            BTN_FIELD_TIMERS,
            card_state_button(0),
            card_state_button(1),
        ] {
            assert!(
                !closed.contains(&id),
                "a Settings-CARD control {id} is a Card Tab stop"
            );
        }
        // The visible order: repair above Apply, matching the layout.
        let fix_at = closed.iter().position(|&id| id == BTN_FIX_MATCH);
        let apply_at = closed.iter().position(|&id| id == BTN_PICKER_APPLY);
        assert!(fix_at < apply_at, "tab order follows the eye");
        // Approved pinning: Apply stays a stop even when disabled; the static
        // order keeps Fix, the live order drops it with nothing playing.
        let no_player = card_focus_order_for(false, true, false);
        assert!(no_player.contains(&BTN_FIX_MATCH));
        assert!(no_player.contains(&BTN_PICKER_APPLY));
        // At rest the order is the static narrow order.
        let at_rest = card_focus_order_for(false, false, false);
        assert!(at_rest.contains(&BTN_FIX_MATCH) && at_rest.contains(&BTN_PICKER_APPLY));
        assert_eq!(at_rest, card_focus_order(), "the at-rest order is unchanged");

        // And through the real window: a full Tab cycle over a live list visits
        // exactly the stops the layout draws, repair action included, with the
        // disclosure closed. A live title is playing, so the hero Fix is in.
        let cand = Candidate {
            kind: Kind::MOVIE,
            title: "Dune".to_string(),
            year: 2021,
            provider: "cinemeta".to_string(),
            provider_id: "tt1160419".to_string(),
            poster: String::new(),
            synopsis: "Paul Atreides on Arrakis.".to_string(),
            genres: "Sci-Fi".to_string(),
            length: "2h 35m".to_string(),
            episode_count: 0,
        };
        app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
            raw_title: "Dune.mkv - PotPlayer".to_string(),
            path: r"C:\Media\Dune.mkv".to_string(),
        }));
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.advanced_open = false;
        st.picker.candidates = vec![cand];
        st.picker.open = true;
        st.picker.selected = 0;
        st.picker.pick_row(picked_row_key(&st.picker.candidates[0]));
        let state = st.card_layout_state();
        assert!(state.has_selection && !state.advanced);
        assert!(state.playing, "the walk needs a live title for the hero Fix");
        let want = card_focus_order_for(state.advanced, state.has_selection, state.can_apply);
        let hwnd = std::ptr::null_mut();
        // Tab direction is stated, never sniffed: the plain `on_settings_key_down`
        // wrapper takes it from `shift_held()`, which reads the live OS key state
        // through `GetKeyState` (this crate's test threads own no message queue),
        // so one stray Shift turns every step into a reverse cycle.
        let mut walk = vec![0];
        for _ in 0..want.len() {
            st.on_settings_key_down_ex(hwnd, VK_TAB, false);
            walk.push(st.focus_id);
        }
        // The reverse cycle that stray Shift produced, deterministically: from
        // the unfocused ring the walk enters at the tail and runs backwards.
        let mut backwards = want.clone();
        backwards.reverse();
        let mut rev_walk = vec![0];
        for _ in 0..want.len() {
            let cur = *rev_walk.last().unwrap();
            rev_walk.push(move_focus(cur, -1, &want));
        }
        assert_eq!(
            rev_walk[1..].to_vec(),
            backwards,
            "Shift+Tab is the same ring walked backwards"
        );
        assert_eq!(
            walk[1..].to_vec(),
            want,
            "the Tab walk must be exactly the visible stop list"
        );
        assert!(walk.contains(&BTN_FIX_MATCH), "the visible repair action is reachable");
        app_support::config::set_live_now_playing(None);
    }

    #[test]
    fn focus_order_covers_rect_table() {
        for (order, want_len) in [
            (main_focus_order(), 4usize),
            (settings_focus_order(), 7),
            (card_focus_order(), 6usize),
        ] {
            assert_eq!(order.len(), want_len);
            let mut seen = std::collections::HashSet::new();
            for id in &order {
                assert!(seen.insert(*id), "duplicate {id}");
            }
        }
        assert_eq!(focus_order_for_view(View::Main).len(), 4);
        assert_eq!(focus_order_for_view(View::Settings).len(), 7);
        assert_eq!(focus_order_for_view(View::Card).len(), 6);
        for id in [BTN_REPORT, BTN_CLOSE, BTN_SETTINGS, BTN_CARD] {
            assert!(main_focus_order().contains(&id), "missing {id}");
        }
        // The Card presets live in the wide Card disclosure, never in
        // Settings: the narrow Card carries no preset row, and neither does
        // the slimmed Settings table.
        assert!(
            !card_focus_order().contains(&card_state_button(0)),
            "narrow Card carries no preset row"
        );
        assert!(
            !settings_focus_order().contains(&card_state_button(0)),
            "Settings carries no preset row"
        );
        assert!(
            !settings_focus_order().contains(&BTN_ADVANCED),
            "Settings has no disclosure control"
        );
        assert!(
            !card_focus_order().contains(&BTN_ADVANCED),
            "narrow Card carries no Change-filters control"
        );
        // Settings lays out exactly the five surviving rows.
        for id in [BTN_DISCORD, BTN_CLOSE, BTN_STARTUP, BTN_CLEAR_CACHE] {
            assert!(
                settings_focus_order().contains(&id),
                "surviving Settings row {id} is a Tab stop"
            );
        }
        // The focus order IS the visible rect table: hidden wiring rects
        // (empty) are keyboard/Esc-only and never Tab stops. The Card pair is
        // the playing shape (hero Fix present); the idle shape omits it.
        for (order, ids) in [
            (
                main_focus_order(),
                compute_layout(&dirs(2), MAIN_CLIENT_W, MAIN_CLIENT_H, 96)
                    .btns
                    .iter()
                    .map(|b| b.id)
                    .collect::<Vec<i32>>(),
            ),
            (
                settings_focus_order(),
                compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, false, "turbo", "amoled")
                    .btns
                    .iter()
                    .filter(|b| b.rc.right > b.rc.left && b.rc.bottom > b.rc.top)
                    .map(|b| b.id)
                    .collect::<Vec<i32>>(),
            ),
            (
                card_focus_order(),
                compute_card_layout_state(
                    MAIN_CLIENT_W,
                    MAIN_CLIENT_H,
                    96,
                    &CardSpecs::default(),
                    CardKind::Series,
                    PreviewSurface::default(),
                    &CardLayoutState {
                        advanced: false,
                        has_selection: false,
                        can_apply: false,
                        playing: true,
                        filter: crate::search::KindFilter::All,
                    },
                )
                .btns
                .iter()
                .filter(|b| b.rc.right > b.rc.left && b.rc.bottom > b.rc.top)
                .map(|b| b.id)
                .collect::<Vec<i32>>(),
            ),
        ] {
            let mut a = order.clone();
            let mut b = ids;
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "focus order and rect table disagree");
        }
        // Idle: no Fix, so the visible table is the static order minus Fix.
        {
            let idle = compute_card_layout(
                MAIN_CLIENT_W,
                MAIN_CLIENT_H,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
            );
            assert!(idle.card_btn(BTN_FIX_MATCH).is_none());
            let mut want = card_focus_order();
            want.retain(|&id| id != BTN_FIX_MATCH);
            let mut got: Vec<i32> = idle
                .btns
                .iter()
                .filter(|b| b.rc.right > b.rc.left && b.rc.bottom > b.rc.top)
                .map(|b| b.id)
                .collect();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(want, got, "idle rect table omits Fix");
        }
    }

    #[test]
    fn move_focus_wraps() {
        let order = vec![BTN_ADD, BTN_REMOVE, BTN_SCAN];
        assert_eq!(move_focus(0, 1, &order), BTN_ADD);
        assert_eq!(move_focus(0, -1, &order), BTN_SCAN);
        assert_eq!(move_focus(BTN_SCAN, 1, &order), BTN_ADD);
        assert_eq!(move_focus(BTN_ADD, -1, &order), BTN_SCAN);
        assert_eq!(move_focus(BTN_ADD, 1, &order), BTN_REMOVE);
        assert_eq!(move_focus(0, 1, &[]), 0);
    }

    /// `Rect` has no `PartialEq` (it is a byte-exact ABI mirror); compare fields.
    fn rect_same(a: Rect, b: Rect) -> bool {
        a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
    }

    /// sRGB channel (0-255) to linear light (WCAG 2.1 relative luminance).
    fn lin(c: u32) -> f64 {
        let s = c as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }

    /// WCAG 2.1 contrast ratio of two `rgb()` COLORREFs, darkest-first.
    fn contrast(a: u32, b: u32) -> f64 {
        let lum = |c: u32| {
            0.2126 * lin(c & 0xFF) + 0.7152 * lin((c >> 8) & 0xFF) + 0.0722 * lin((c >> 16) & 0xFF)
        };
        let (lo, hi) = if lum(a) < lum(b) {
            (lum(a), lum(b))
        } else {
            (lum(b), lum(a))
        };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Every text-on-background pair in the palette, computed from the actual
    /// `rgb()` constants (WCAG 2.1; AA body text needs 4.5:1, large/disabled
    /// 3:1). Prints the ratios so the proof carries the numbers, not just the
    /// verdict. Approved accent `#E5343F` measures ~4.29:1 white-on-red: the
    /// Search/Apply 13px semibold controls clear the 4.0 UI-control floor and
    /// the 3:1 non-text gate, not the old 4.5 body gate.
    #[test]
    fn palette_contrast() {
        let pairs = [
            ("COL_TEXT/COL_BG", COL_TEXT, COL_BG, 7.0),
            ("COL_TEXT/COL_ROW", COL_TEXT, COL_ROW, 7.0),
            ("COL_MUTED/COL_BG", COL_MUTED, COL_BG, 4.5),
            ("COL_MUTED/COL_CARD", COL_MUTED, COL_CARD, 4.5),
            ("COL_MUTED_DIM/COL_BG", COL_MUTED_DIM, COL_BG, 3.0),
            ("COL_ON_ACCENT/COL_ACCENT", COL_ON_ACCENT, COL_ACCENT, 4.0),
            ("COL_TEXT/COL_ACCENT_HOV", COL_TEXT, COL_ACCENT_HOV, 3.0),
            ("COL_DANGER/COL_BG", COL_DANGER, COL_BG, 3.0),
        ];
        for (name, fg, bg, floor) in pairs {
            let r = contrast(fg, bg);
            println!("contrast {name} = {r:.2}:1 (floor {floor})");
            assert!(r >= floor, "{name} = {r:.2}:1, below {floor}:1");
        }
        // The contrast fix the palette comment records: pure white beats
        // `COL_TEXT` on the brand red.
        assert!(contrast(COL_ON_ACCENT, COL_ACCENT) > contrast(COL_TEXT, COL_ACCENT));
    }

    /// The scroll clamp is exact: `list_top` never exceeds the last full
    /// viewport, the card keeps full height while scrolled, and the thumb is
    /// empty exactly when the list fits.
    #[test]
    fn scroll_clamp_and_thumb() {
        for dpi in [96, 144] {
            // Fits: no scrollbar, no offset.
            let l = compute_layout_total(2, MAIN_CLIENT_W, MAIN_CLIENT_H, dpi, 0);
            assert!(!l.scrolled(), "dpi {dpi}: 2 rows must not scroll");
            assert_eq!(l.list_top, 0);
            assert!(rect_same(l.thumb, Rect::default()));
            // Overflow: clamped to the last viewport, thumb present.
            let full = compute_layout_total(40, MAIN_CLIENT_W, MAIN_CLIENT_H, dpi, 0);
            assert!(full.scrolled(), "dpi {dpi}: 40 rows must scroll");
            assert!(full.list_capacity >= 1);
            let max_top = 40 - full.list_capacity;
            let over = compute_layout_total(40, MAIN_CLIENT_W, MAIN_CLIENT_H, dpi, max_top + 9);
            assert_eq!(over.list_top, max_top, "dpi {dpi}: clamp to last viewport");
            assert!(
                rect_same(over.card, full.card),
                "dpi {dpi}: scrolled card keeps full height"
            );
            assert!(
                over.thumb.bottom > over.thumb.top,
                "dpi {dpi}: thumb visible"
            );
            let thumb = scrollbar_thumb(over.scrollbar, 40, over.list_capacity, 0, 24);
            assert!(
                thumb.top <= over.thumb.top,
                "dpi {dpi}: thumb travels down with the offset"
            );
        }
    }

    /// `scroll_list` is a no-op outside the Main list and on a null window;
    /// hover clears when the viewport moves (needs a live window, so only the
    /// no-op halves are pinned here -- movement is proved by the captures).
    #[test]
    fn scroll_list_noops() {
        let mut st = UiState::default();
        st.cur_view = View::Settings;
        assert!(!st.scroll_list(NULL, 3));
        st.cur_view = View::Main;
        assert!(!st.scroll_list(NULL, 3));
        assert_eq!(st.scroll_top, 0);
    }

    /// The Main list's keyboard selection core: clamped at both ends, nothing
    /// selected lands on row 0, an empty list stays unselected, a list shorter
    /// than the viewport never steps past its own last row, and a shrunk list
    /// clamps the stale selection to the surviving rows.
    #[test]
    fn folder_list_selection_core() {
        // Both ends clamp.
        assert_eq!(step_sel_row(0, 3, -1), 0);
        assert_eq!(step_sel_row(2, 3, 1), 2);
        assert_eq!(step_sel_row(1, 3, 1), 2);
        assert_eq!(step_sel_row(1, 3, -1), 0);
        // Nothing selected: the first step lands on row 0.
        assert_eq!(step_sel_row(-1, 3, -1), 0);
        assert_eq!(step_sel_row(-1, 3, 1), 0);
        // Home/End.
        assert_eq!(edge_sel_row(3, false), 0);
        assert_eq!(edge_sel_row(3, true), 2);
        // Empty list: no row to select.
        assert_eq!(step_sel_row(-1, 0, 1), -1);
        assert_eq!(step_sel_row(0, 0, -1), -1);
        assert_eq!(edge_sel_row(0, false), -1);
        assert_eq!(edge_sel_row(0, true), -1);
        // A list shorter than the visible rows: the viewport could show more,
        // but the selection ends are the LIST's ends.
        let l = compute_layout(&dirs(2), MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert!(
            l.list_capacity > 2,
            "this case needs a list shorter than the viewport, got {}",
            l.list_capacity
        );
        assert_eq!(step_sel_row(1, 2, 1), 1);
        assert_eq!(step_sel_row(0, 2, -1), 0);
        assert_eq!(edge_sel_row(2, true), 1);
        // A shrunk list clamps the selection (the refreshDirList guard).
        assert_eq!(clamp_sel_row(2, 1), 0);
        assert_eq!(clamp_sel_row(2, 0), -1);
        assert_eq!(clamp_sel_row(-1, 3), -1);
        assert_eq!(clamp_sel_row(1, 3), 1);
    }

    /// The handler wiring: Up/Down/Home/End drive the Main selection and take
    /// the button focus with them; Settings/Card never touch it; a shrunk list
    /// clamps it through the same guard `refresh_dir_list` runs.
    #[test]
    fn folder_list_keyboard_navigation() {
        let mut st = UiState::default();
        st.ui_dirs = dirs(3);
        st.focus_id = BTN_SCAN;
        assert!(st.on_settings_key_down_ex(NULL, VK_DOWN, false));
        assert_eq!(st.sel_row, 0);
        assert_eq!(st.focus_id, 0, "the list owns the key: no stale ring");
        assert!(st.on_settings_key_down_ex(NULL, VK_DOWN, false));
        assert_eq!(st.sel_row, 1);
        assert!(st.on_settings_key_down_ex(NULL, VK_UP, false));
        assert_eq!(st.sel_row, 0);
        assert!(st.on_settings_key_down_ex(NULL, VK_UP, false));
        assert_eq!(st.sel_row, 0, "clamped at the top");
        assert!(st.on_settings_key_down_ex(NULL, VK_END, false));
        assert_eq!(st.sel_row, 2);
        assert!(st.on_settings_key_down_ex(NULL, VK_HOME, false));
        assert_eq!(st.sel_row, 0);
        // The Settings and Card views ignore the Main selection.
        for view in [View::Settings, View::Card] {
            st.cur_view = view;
            assert!(st.on_settings_key_down_ex(NULL, VK_DOWN, false));
            assert_eq!(st.sel_row, 0, "{view:?} must not move the Main selection");
        }
        // A shrunk list clamps the stale selection.
        st.cur_view = View::Main;
        st.sel_row = 2;
        st.sel_row = clamp_sel_row(st.sel_row, 1);
        assert_eq!(st.sel_row, 0);
    }

    /// Enter on the list opens the selected folder through the same shell path
    /// the pointer uses (`BlockingUi::OpenFolder`, stashed so the wndproc runs
    /// `ShellExecuteW` with `UI` released) and never a Remove; with no
    /// selection, or with a button focused, Enter keeps its old meaning.
    #[test]
    fn enter_opens_selected_folder() {
        let mut st = UiState::default();
        st.ui_dirs = dirs(3);
        st.sel_row = 1;
        assert!(st.on_settings_key_down_ex(NULL, VK_RETURN, false));
        assert_eq!(st.pending_blocking, Some(BlockingUi::OpenFolder));
        assert_eq!(st.sel_row, 1);
        assert_eq!(st.ui_dirs.len(), 3, "open is read-only");
        // No selection: Enter stays the focused-control key, and with nothing
        // focused it is inert.
        st.pending_blocking = None;
        st.sel_row = -1;
        assert!(!st.on_settings_key_down_ex(NULL, VK_RETURN, false));
        assert_eq!(st.pending_blocking, None);
        // A focused button keeps Enter, so Add/Scan/Remove stay reachable.
        st.focus_id = BTN_ADD;
        assert!(st.on_settings_key_down_ex(NULL, VK_RETURN, false));
        assert_eq!(st.pending_blocking, Some(BlockingUi::AddDir));
    }

    #[test]
    fn focus_tab_cycle() {
        let mut st = UiState::default();
        st.cur_view = View::Main;
        st.focus_id = 0;
        assert!(st.on_settings_key_down_ex(NULL, VK_TAB, false));
        assert_eq!(st.focus_id, BTN_CARD);
        assert!(st.on_settings_key_down_ex(NULL, VK_TAB, false));
        assert_eq!(st.focus_id, BTN_SETTINGS);
        assert!(st.on_settings_key_down_ex(NULL, VK_TAB, true));
        assert_eq!(st.focus_id, BTN_CARD);
        assert!(st.on_settings_key_down_ex(NULL, VK_DOWN, false));
        assert_eq!(st.focus_id, BTN_SETTINGS);
        assert!(st.on_settings_key_down_ex(NULL, VK_UP, false));
        assert_eq!(st.focus_id, BTN_CARD);
        assert!(!st.on_settings_key_down_ex(NULL, VK_ESCAPE, false));
    }

    #[test]
    fn focus_activate_honours_disabled() {
        let mut st = UiState::default();
        st.cur_view = View::Main;
        st.scan_active = true;
        st.focus_id = BTN_REFRESH;
        assert!(st.on_settings_key_down_ex(NULL, VK_RETURN, false));
        assert_eq!(st.status_text, SCAN_BUSY_TEXT);
        assert!(st.scan_active);
    }

    /// Recorded from INSIDE the substituted blocking call.
    static BLOCKING_PROBE_RAN: AtomicBool = AtomicBool::new(false);
    static BLOCKING_PROBE_UI_UNLOCKED: AtomicBool = AtomicBool::new(false);
    static BLOCKING_PROBE_ACTION: Mutex<Option<BlockingUi>> = Mutex::new(None);

    /// Stands in for whatever real modal / Win32 blocking call an action would
    /// make (`SHBrowseForFolderW`, `MessageBoxW`, `GetOpen/SaveFileNameW`,
    /// `ShellExecuteW`, `ShowWindow`). Runs at the exact point the real call
    /// would pump the queue and re-enter `main_wnd_proc`: if a `UI` guard is
    /// held there, `try_lock` fails (non-blocking, so no hang).
    fn blocking_probe(action: BlockingUi) {
        BLOCKING_PROBE_UI_UNLOCKED.store(UI.try_lock().is_ok(), Ordering::SeqCst);
        *BLOCKING_PROBE_ACTION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(action);
        BLOCKING_PROBE_RAN.store(true, Ordering::SeqCst);
    }

    /// `show_main_window`'s substituted `center_window`/`ShowWindow` step.
    static SHOW_PROBE_RAN: AtomicBool = AtomicBool::new(false);
    static SHOW_PROBE_UI_UNLOCKED: AtomicBool = AtomicBool::new(false);

    fn show_probe() {
        SHOW_PROBE_UI_UNLOCKED.store(UI.try_lock().is_ok(), Ordering::SeqCst);
        SHOW_PROBE_RAN.store(true, Ordering::SeqCst);
    }

    /// How a route reaches its action through the REAL dispatcher.
    #[derive(Clone, Copy)]
    enum Drive {
        /// A mouse click: `WM_L_BUTTON_DOWN`+`WM_L_BUTTON_UP` on a button.
        Btn { view: View, btn: i32 },
    }

    /// Every fixed blocking action, with the real route that reaches it.
    /// The folder actions (AddDir/OpenFolder) left with the folder feature on
    /// 2026-11-03; their routes are gone with the controls. The unrouted
    /// library actions (Clear index, Export, Import, Open log) left Settings
    /// the same day: their rows are gone, so no click can reach them, and the
    /// guard release they relied on is still proven by the surviving routes.
    const ROUTES: &[(&str, Drive, BlockingUi)] = &[
        (
            "HideWindow ('Close to tray' button)",
            Drive::Btn {
                view: View::Main,
                btn: BTN_CLOSE,
            },
            BlockingUi::HideWindow,
        ),
        (
            "ClearMetadata ('Clear metadata' button)",
            Drive::Btn {
                view: View::Settings,
                btn: BTN_CLEAR_CACHE,
            },
            BlockingUi::ClearMetadata,
        ),
        (
            "FixMatch ('Fix match...' button)",
            Drive::Btn {
                view: View::Card,
                btn: BTN_FIX_MATCH,
            },
            BlockingUi::FixMatch,
        ),
    ];

    /// One route's observation, recorded from inside its blocking call.
    struct Obs {
        ran: bool,
        unlocked: bool,
        action: Option<BlockingUi>,
    }

    /// Pack a client `(x, y)` pair as a mouse `lParam`.
    fn mouse_lparam(x: i32, y: i32) -> isize {
        (((y as u32) & 0xFFFF) << 16 | ((x as u32) & 0xFFFF)) as isize
    }

    #[test]
    fn blocking_actions_run_outside_ui_guard_all_routes() {
        // FALSIFIABLE regression test for the re-entrant `UI` deadlock. It
        // drives the REAL dispatcher end to end for EVERY fixed blocking
        // action: it creates the actual main window so the real `main_wnd_proc`
        // is installed, then routes each action through `SendMessageW` into that
        // wndproc - mouse `WM_L_BUTTON_UP` -> `release_at` ->
        // `do_button_action`/`do_settings_action` -> `run_blocking_ui`, and the
        // keyboard route via `WM_KEYDOWN`/`VK_RETURN`. `show_main_window` (the
        // one action with no dispatcher route) is driven directly. Every modal
        // / Win32 blocking call is substituted by `TEST_BLOCKING_PROBE` (or
        // `TEST_SHOW_PROBE`), whose callback records whether `UI.try_lock()`
        // SUCCEEDS *from inside the blocking call*. Re-introducing the bug
        // (holding the guard across `run_blocking_ui`) makes `try_lock` fail ->
        // this test fails. A per-route watchdog (bounded `recv_timeout`) turns a
        // pre-fix HANG into a failure.
        let _ui_serial = UI_GLOBAL_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tx, rx) = std::sync::mpsc::channel::<Result<Obs, String>>();
        *TEST_SHOW_PROBE.lock().unwrap() = Some(show_probe);
        let tx_panic = tx.clone();
        std::thread::spawn(move || {
            *TEST_BLOCKING_PROBE.lock().unwrap() = Some(blocking_probe);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Err(e) = create_main_window() {
                    let _ = tx.send(Err(e));
                    return;
                }
                let hwnd = ui().main_hwnd as HWND;
                for &(_, drive, _) in ROUTES {
                    BLOCKING_PROBE_RAN.store(false, Ordering::SeqCst);
                    BLOCKING_PROBE_UI_UNLOCKED.store(false, Ordering::SeqCst);
                    *BLOCKING_PROBE_ACTION.lock().unwrap() = None;
                    {
                        let mut st = ui();
                        st.scan_active = false;
                        st.focus_id = 0;
                        st.pressed_btn = 0;
                        st.pressed_x = -1;
                        st.ui_dirs.clear();
                        st.sel_row = -1;
                        // The picker route needs a live item: without it the
                        // Card action refuses with the idle reason and the
                        // blocking path is never exercised.
                        app_support::config::set_live_now_playing(None);
                        match drive {
                            Drive::Btn { view, btn } => {
                                st.cur_view = view;
                                if view == View::Card && btn == BTN_FIX_MATCH {
                                    // The repair row exists only while a result
                                    // is selected, so the route seeds one: the
                                    // affordance is a row in the list, not a peer
                                    // of the query field.
                                    st.picker.candidates = vec![metadata::Candidate {
                                        kind: metadata_core::types::Kind::SERIES,
                                        title: "Show".to_string(),
                                        year: 2008,
                                        provider: "tvmaze".to_string(),
                                        provider_id: "169".to_string(),
                                        poster: String::new(),
                                        synopsis: String::new(),
                                        genres: String::new(),
                                        length: String::new(),
                                        episode_count: 3,
                                    }];
                                    st.picker.open = true;
                                    st.picker.selected = 0;
                                    app_support::config::set_live_now_playing(Some(
                                        app_support::config::NowPlaying {
                                            raw_title: "Show.S01E02.Pilot.1080p.mkv - PotPlayer"
                                                .to_string(),
                                            path: format!(
                                                "{}:{}",
                                                'D', r"\Anime\Show\Season 1\S01E02.mkv"
                                            ),
                                        },
                                    ));
                                }
                                if view == View::Settings {
                                    // Rows below the fold are reachable by
                                    // scrolling: bring the driven row into the
                                    // viewport through the shared offset, or
                                    // the press/release lands on chrome and the
                                    // blocking path is never exercised.
                                    if let Some(l) = st.layout_settings(hwnd)
                                        && let Some(rc) = l.settings_btn(btn)
                                    {
                                        let need = (rc.bottom - l.status.top).max(0);
                                        if need > 0 {
                                            st.scroll_settings(hwnd, need);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let sent = match drive {
                        Drive::Btn { view, btn } => {
                            let btns = {
                                let st = ui();
                                match view {
                                    View::Settings => st.layout_settings(hwnd).map(|l| l.btns),
                                    View::Card => st.layout_card(hwnd).map(|l| l.btns),
                                    View::Main => st.layout(hwnd).map(|l| l.btns),
                                }
                            };
                            match btns.and_then(|b| b.into_iter().find(|b| b.id == btn)) {
                                Some(b) => {
                                    let lp = mouse_lparam(
                                        (b.rc.left + b.rc.right) / 2,
                                        (b.rc.top + b.rc.bottom) / 2,
                                    );
                                    // SAFETY: same thread -> synchronous WNDPROC.
                                    unsafe {
                                        crate::win32::SendMessageW(
                                            hwnd,
                                            crate::win32::WM_L_BUTTON_DOWN,
                                            0,
                                            lp,
                                        );
                                        crate::win32::SendMessageW(
                                            hwnd,
                                            crate::win32::WM_L_BUTTON_UP,
                                            0,
                                            lp,
                                        );
                                    }
                                    Ok(())
                                }
                                None => Err(format!("button {btn} missing from {view:?} layout")),
                            }
                        }
                    };
                    let obs = sent.map(|()| Obs {
                        ran: BLOCKING_PROBE_RAN.load(Ordering::SeqCst),
                        unlocked: BLOCKING_PROBE_UI_UNLOCKED.load(Ordering::SeqCst),
                        action: *BLOCKING_PROBE_ACTION.lock().unwrap(),
                    });
                    let _ = tx.send(obs);
                }
                // show/center: no dispatcher route, driven directly.
                SHOW_PROBE_RAN.store(false, Ordering::SeqCst);
                SHOW_PROBE_UI_UNLOCKED.store(false, Ordering::SeqCst);
                show_main_window();
                let _ = tx.send(Ok(Obs {
                    ran: SHOW_PROBE_RAN.load(Ordering::SeqCst),
                    unlocked: SHOW_PROBE_UI_UNLOCKED.load(Ordering::SeqCst),
                    action: None,
                }));
                // SAFETY: same thread created the window.
                unsafe {
                    crate::win32::DestroyWindow(hwnd);
                }
            }));
            if result.is_err() {
                let _ = tx_panic.send(Err("dispatch panicked".into()));
            }
        });

        for &(label, _, expect) in ROUTES {
            let got = rx.recv_timeout(Duration::from_secs(15)).unwrap_or_else(|_| {
                panic!("deadlock: route `{label}` did not complete within 15s (a UI guard was held across the blocking call)")
            });
            let obs = got.unwrap_or_else(|e| panic!("route `{label}` failed: {e}"));
            assert!(
                obs.ran,
                "route `{label}`: blocking path never exercised - test proves nothing"
            );
            assert_eq!(
                obs.action,
                Some(expect),
                "route `{label}`: dispatcher requested {:?}, expected {:?}",
                obs.action,
                expect
            );
            assert!(
                obs.unlocked,
                "route `{label}`: UI guard was HELD during the blocking call: the re-entrant deadlock is back"
            );
        }
        {
            // Leave no route state on the process-wide UI for the next test.
            let mut st = ui();
            st.ui_dirs.clear();
            st.sel_row = -1;
        }
        let show = rx
            .recv_timeout(Duration::from_secs(15))
            .expect("deadlock: show/center did not complete within 15s")
            .unwrap_or_else(|e| panic!("show/center failed: {e}"));
        assert!(
            show.ran,
            "show/center: blocking path never exercised - test proves nothing"
        );
        assert!(
            show.unlocked,
            "show/center: UI guard was HELD across center_window/ShowWindow: the re-entrant deadlock is back"
        );
        *TEST_BLOCKING_PROBE.lock().unwrap() = None;
        *TEST_SHOW_PROBE.lock().unwrap() = None;
    }

    #[test]
    fn remove_without_selection_feedback() {
        let mut st = UiState::default();
        st.ui_dirs = vec![format!("{}:{}", 'C', r"\Media\Movies")];
        st.sel_row = -1;
        st.hover_row = -1;
        st.remove_dir_at(-1);
        assert_eq!(st.status_text, "Select a folder first.");
        assert_eq!(st.ui_dirs.len(), 1);
    }

    #[test]
    fn scan_blocks_edits() {
        for id in [BTN_ADD, BTN_REMOVE, BTN_REFRESH] {
            assert_eq!(scan_blocked_reason(true, id), SCAN_BUSY_TEXT);
        }
        for id in [BTN_SCAN, BTN_CLOSE, BTN_SETTINGS, BTN_CARD] {
            assert_eq!(scan_blocked_reason(true, id), "");
        }
        for id in [BTN_ADD, BTN_REMOVE, BTN_REFRESH, BTN_SCAN, BTN_CLOSE] {
            assert_eq!(scan_blocked_reason(false, id), "");
        }
    }

    #[test]
    fn remove_dim_needs_target() {
        assert!(!remove_has_target(-1, -1, 1));
        assert!(remove_has_target(0, -1, 1));
        assert!(remove_has_target(-1, 0, 1));
        assert!(!remove_has_target(0, 0, 0));
    }

    #[test]
    fn audit_single_line_verdict_classes() {
        assert_eq!(audit_single_line_verdict(100, 15, 420, 20, true), "");
        assert_eq!(audit_single_line_verdict(100, 15, 420, 20, false), "");
        assert_eq!(audit_single_line_verdict(449, 15, 420, 20, true), "trimmed");
        assert_eq!(
            audit_single_line_verdict(449, 15, 420, 20, false),
            "overflow"
        );
        assert_eq!(audit_single_line_verdict(100, 30, 420, 20, true), "trimmed");
        assert_eq!(
            audit_single_line_verdict(100, 30, 420, 20, false),
            "overflow"
        );
    }

    #[test]
    fn scan_bar_model_walk_resolve() {
        assert_eq!(scan_bar_model(false, 0, 0), (false, false, 0.0));
        let (show, det, _) = scan_bar_model(true, 0, 0);
        assert!(show && !det);
        assert_eq!(scan_bar_model(true, 12, 24), (true, true, 0.5));
        assert_eq!(scan_bar_model(true, 24, 24), (true, true, 1.0));
        assert_eq!(scan_bar_model(true, 30, 24), (true, true, 1.0));
    }

    #[test]
    fn scan_key_fraction_cases() {
        let cases = [
            (0, 10, Some(0.0)),
            (5, 10, Some(0.5)),
            (10, 10, Some(1.0)),
            (15, 10, Some(1.0)),
            (-2, 10, Some(0.0)),
            (5, 0, None),
            (5, -3, None),
        ];
        for (done, total, want) in cases {
            assert_eq!(scan_key_fraction(done, total), want, "({done},{total})");
        }
    }

    #[test]
    fn scan_lifecycle_throttle() {
        let t0 = Instant::now();
        let (st, fx) = scan_lifecycle_next(ScanLifecycle::default(), ScanLifecycleEvent::Start, t0);
        assert!(fx.arm && st.last_paint == t0);
        let mut last = st;
        let mut admitted = 0;
        for i in 1..=100 {
            let now = t0 + Duration::from_millis(i * 10);
            let (next, fx) = scan_lifecycle_next(last, ScanLifecycleEvent::Progress, now);
            if fx.paint {
                admitted += 1;
                last = next;
            }
        }
        assert!(admitted <= 10, "admitted {admitted}/s");
        assert!(admitted > 0);
    }

    #[test]
    fn scan_lifecycle_arm_and_reset() {
        let now = Instant::now();
        let old = now - Duration::from_secs(3600);
        let (st, fx) = scan_lifecycle_next(
            ScanLifecycle {
                active: false,
                last_paint: old,
            },
            ScanLifecycleEvent::Start,
            now,
        );
        assert!(st.active);
        assert!(fx.arm);
        assert_eq!(st.last_paint, now);
        assert!(fx.paint);
        assert!(!fx.kill);
    }

    #[test]
    fn scan_lifecycle_one_scan_at_a_time() {
        let now = Instant::now();
        let running = ScanLifecycle {
            active: true,
            last_paint: now - Duration::from_secs(1),
        };
        let (next, fx) = scan_lifecycle_next(running, ScanLifecycleEvent::Start, now);
        assert_eq!(fx, ScanFx::default());
        assert_eq!(next, running);
    }

    #[test]
    fn scan_lifecycle_tick_gate() {
        let t0 = Instant::now();
        let (st, _) = scan_lifecycle_next(ScanLifecycle::default(), ScanLifecycleEvent::Start, t0);
        let (next, fx) = scan_lifecycle_next(st, ScanLifecycleEvent::Tick, t0);
        assert!(!fx.paint && next.last_paint == t0);
        let later = t0 + SCAN_PAINT_INTERVAL;
        let (next, fx) = scan_lifecycle_next(st, ScanLifecycleEvent::Progress, later);
        assert!(fx.paint && next.last_paint == later);
        let (_, fx) =
            scan_lifecycle_next(ScanLifecycle::default(), ScanLifecycleEvent::Tick, later);
        assert_eq!(fx, ScanFx::default());
        assert!(scan_paint_gate(t0, t0 - Duration::from_secs(1)));
    }

    #[test]
    fn scan_lifecycle_terminal_paint() {
        let now = Instant::now();
        let (st, _) = scan_lifecycle_next(ScanLifecycle::default(), ScanLifecycleEvent::Start, now);
        let (next, fx) = scan_lifecycle_next(st, ScanLifecycleEvent::Done, now);
        assert!(fx.kill && fx.terminal && !fx.paint && !next.active);
        let (back, fx_back) = scan_lifecycle_next(
            st,
            ScanLifecycleEvent::Done,
            now - Duration::from_secs(3600),
        );
        assert!(fx_back.kill && fx_back.terminal && !back.active);
        let (_, fx) = scan_lifecycle_next(ScanLifecycle::default(), ScanLifecycleEvent::Done, now);
        assert!(fx.kill && !fx.terminal);
    }

    #[test]
    fn scan_start_clears_stale_progress() {
        let mut st = UiState::default();
        st.arm_timer = Arc::new(|_| {});
        st.scan_progress = Progress {
            keys_done: 24,
            keys_total: 24,
            keys_fetched: 4,
            keys_cached: 20,
            folders_scanned: 6,
            ..Progress::default()
        };
        st.scan_resolve_start = Some(Instant::now() - Duration::from_secs(60));
        st.scan_resolve_base = 20;
        let now = Instant::now();
        let started = st.begin_scan_run(NULL, false, now);
        assert!(started.is_some());
        assert_eq!(st.scan_progress, Progress::default());
        assert!(st.scan_resolve_start.is_none());
        assert_eq!(st.scan_resolve_base, 0);
        if let Some(c) = st.scan_cancel.take() {
            c.cancel();
        }
    }

    #[test]
    fn scan_start_applies_effects() {
        let arms = Arc::new(AtomicU32::new(0));
        let a = arms.clone();
        let mut st = UiState::default();
        st.arm_timer = Arc::new(move |_| {
            a.fetch_add(1, Ordering::SeqCst);
        });
        st.scan_last_paint = Instant::now() - Duration::from_secs(3600);
        let now = Instant::now();
        assert!(st.begin_scan_run(NULL, false, now).is_some());
        assert!(st.scan_active);
        assert_eq!(arms.load(Ordering::SeqCst), 1);
        assert_eq!(st.scan_last_paint, now);
        assert_eq!(st.status_text, "Scan starting...");
        assert!(!st.scan_force);

        assert!(
            st.begin_scan_run(NULL, true, now + Duration::from_secs(1))
                .is_none()
        );
        assert_eq!(arms.load(Ordering::SeqCst), 1);
        assert_eq!(st.scan_last_paint, now);
        assert_eq!(st.status_text, "Scan starting...");
        if let Some(c) = st.scan_cancel.take() {
            c.cancel();
        }

        let mut st2 = UiState::default();
        let arms2 = Arc::new(AtomicU32::new(0));
        let a2 = arms2.clone();
        st2.arm_timer = Arc::new(move |_| {
            a2.fetch_add(1, Ordering::SeqCst);
        });
        assert!(st2.begin_scan_run(NULL, true, now).is_some());
        assert_eq!(st2.status_text, "Refresh starting...");
        assert!(st2.scan_force);
        if let Some(c) = st2.scan_cancel.take() {
            c.cancel();
        }
    }

    #[test]
    fn refresh_scan_live_text_applies_effects() {
        let mut st = UiState::default();
        let now = Instant::now();
        st.scan_active = true;
        st.scan_cancel = Some(library::Cancel::background());
        st.scan_start = now - Duration::from_secs(30);
        st.scan_last_paint = now - Duration::from_secs(3600);
        st.scan_resolve_start = None;
        st.scan_resolve_base = 0;
        let snap = Progress {
            keys_done: 120,
            keys_total: 1000,
            keys_fetched: 90,
            keys_cached: 20,
            keys_failed: 10,
            folders_scanned: 9,
            folders_skipped: 1,
            ..Progress::default()
        };
        st.scan_progress = snap.clone();
        st.status_text = "SENTINEL".into();

        st.refresh_scan_live_text(NULL, ScanLifecycleEvent::Progress, now);
        assert_eq!(st.scan_last_paint, now);
        assert_eq!(st.scan_resolve_start, Some(now));
        assert_eq!(st.scan_resolve_base, scan_observed_done(&snap));
        let want = scan_live_status_text(
            &snap,
            now.saturating_duration_since(st.scan_start),
            scan_resolve_elapsed(Some(now), now),
            scan_observed_done(&snap),
            window_dpi(NULL),
            live_status_avail_px(NULL),
        );
        assert_eq!(st.status_text, want);
        assert!(st.status_text.starts_with("Scan "));

        st.status_text = "SENTINEL2".into();
        st.scan_progress = Progress {
            keys_done: 121,
            keys_total: 1000,
            keys_fetched: 91,
            keys_cached: 20,
            keys_failed: 10,
            folders_scanned: 9,
            folders_skipped: 1,
            ..Progress::default()
        };
        st.refresh_scan_live_text(NULL, ScanLifecycleEvent::Tick, now);
        assert_eq!(st.status_text, "SENTINEL2");
        assert_eq!(st.scan_last_paint, now);

        st.scan_cancel = None;
        st.status_text = "SENTINEL3".into();
        st.refresh_scan_live_text(
            NULL,
            ScanLifecycleEvent::Progress,
            now + Duration::from_secs(10),
        );
        assert_eq!(st.status_text, "SENTINEL3");
    }

    #[test]
    fn scan_terminal_arg_parity() {
        let kills = Arc::new(AtomicU32::new(0));
        let k = kills.clone();
        let mut st = UiState::default();
        st.kill_timer = Arc::new(move |_| {
            k.fetch_add(1, Ordering::SeqCst);
        });
        let land = |st: &mut UiState, force: bool, stats: Stats, err: Option<UiScanError>| {
            st.scan_stats = stats;
            st.scan_err = err;
            st.scan_force = force;
            st.scan_active = true;
            st.scan_cancel = Some(library::Cancel::background());
            st.on_scan_done(NULL);
        };
        let ok = Stats {
            keys: 24,
            keys_done: 24,
            keys_cached: 5,
            keys_fetched: 2,
            keys_not_found: 17,
            files: 10,
            folders_scanned: 4,
            folders_skipped: 2,
            ..Stats::default()
        };
        land(&mut st, false, ok, None);
        assert_eq!(st.status_text, scan_done_text("Scan", &ok));
        land(&mut st, true, ok, None);
        assert_eq!(st.status_text, scan_done_text("Refresh", &ok));
        assert_eq!(kills.load(Ordering::SeqCst), 2);
        assert!(!st.scan_active);
        assert!(st.scan_cancel.is_none());

        let cancelled = Stats {
            keys: 24,
            keys_done: 12,
            cancelled: true,
            ..Stats::default()
        };
        land(&mut st, false, cancelled, Some(UiScanError::other("boom")));
        assert_eq!(st.status_text, scan_cancelled_text(12, 24));
        let bare = Stats {
            keys: 10,
            keys_done: 4,
            ..Stats::default()
        };
        land(&mut st, true, bare, Some(UiScanError::cancelled()));
        assert_eq!(st.status_text, scan_cancelled_text(4, 10));
        land(&mut st, false, ok, Some(UiScanError::other("disk gone")));
        assert!(st.status_text.contains("Scan failed:"));
        land(&mut st, true, ok, Some(UiScanError::other("disk gone")));
        assert!(st.status_text.contains("Refresh failed:"));

        kills.store(0, Ordering::SeqCst);
        st.status_text = "keep me".into();
        st.scan_active = false;
        st.on_scan_done(NULL);
        assert_eq!(kills.load(Ordering::SeqCst), 1);
        assert_eq!(st.status_text, "keep me");
    }

    #[test]
    fn scan_eta_defensible() {
        let suppressed = [
            (Progress::default(), Duration::from_secs(30)),
            (
                Progress {
                    keys_total: 1000,
                    ..Progress::default()
                },
                Duration::from_secs(30),
            ),
            (
                Progress {
                    keys_done: 3,
                    keys_total: 1000,
                    keys_fetched: 3,
                    ..Progress::default()
                },
                Duration::from_secs(30),
            ),
            (
                Progress {
                    keys_done: 990,
                    keys_total: 1000,
                    keys_cached: 990,
                    ..Progress::default()
                },
                Duration::from_secs(30),
            ),
            (
                Progress {
                    keys_done: 995,
                    keys_total: 1000,
                    keys_cached: 990,
                    keys_fetched: 5,
                    ..Progress::default()
                },
                Duration::from_secs(1),
            ),
            (
                Progress {
                    keys_done: 100,
                    keys_total: 1000,
                    keys_fetched: 100,
                    ..Progress::default()
                },
                Duration::from_secs(1),
            ),
            (
                Progress {
                    keys_done: 1000,
                    keys_total: 1000,
                    keys_fetched: 900,
                    keys_cached: 100,
                    ..Progress::default()
                },
                Duration::from_secs(30),
            ),
        ];
        for (p, resolve) in suppressed {
            assert!(scan_eta(&p, 0, resolve).is_none());
        }
        let pre = Progress {
            keys_done: 100,
            keys_total: 1000,
            keys_fetched: 100,
            ..Progress::default()
        };
        assert!(scan_eta(&pre, 100, Duration::from_secs(20)).is_none());
        let steady = Progress {
            keys_done: 100,
            keys_total: 1000,
            keys_fetched: 100,
            ..Progress::default()
        };
        assert_eq!(
            scan_eta(&steady, 0, Duration::from_secs(20)),
            Some(Duration::from_secs(180))
        );
        let delta = Progress {
            keys_done: 200,
            keys_total: 1000,
            keys_fetched: 200,
            ..Progress::default()
        };
        assert_eq!(
            scan_eta(&delta, 100, Duration::from_secs(20)),
            Some(Duration::from_secs(160))
        );
        let mixed = Progress {
            keys_done: 600,
            keys_total: 1000,
            keys_fetched: 100,
            keys_cached: 500,
            ..Progress::default()
        };
        assert_eq!(
            scan_eta(&mixed, 0, Duration::from_secs(20)),
            Some(Duration::from_secs(80))
        );
        let half = Progress {
            keys_done: 500,
            keys_total: 1000,
            keys_fetched: 500,
            ..Progress::default()
        };
        assert_eq!(
            scan_eta(&half, 0, Duration::from_secs(10)),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn format_scan_duration_cases() {
        let cases = [
            (Duration::ZERO, "0s"),
            (Duration::from_secs(5), "5s"),
            (Duration::from_secs(65), "1m05s"),
            (Duration::from_secs(3720), "1h02m"),
        ];
        for (d, want) in cases {
            assert_eq!(format_scan_duration(d), want);
        }
    }

    #[test]
    fn scan_live_status_text_fit() {
        let p = Progress {
            keys_done: 12,
            keys_total: 24,
            keys_fetched: 7,
            keys_cached: 5,
            folders_scanned: 4,
            folders_skipped: 2,
            ..Progress::default()
        };
        let avail = scan_status_avail_px(96);
        let (_, _, early_eta) = scan_live_status_parts(
            &p,
            Duration::from_millis(500),
            Duration::from_millis(500),
            0,
        );
        assert_eq!(early_eta, "");
        let early = scan_live_status_text(
            &p,
            Duration::from_millis(500),
            Duration::from_millis(500),
            0,
            96,
            avail,
        );
        assert!(early.contains(" | 0s"), "{early}");
        assert!(!early.contains("eta"));
        let (_, _, steady_eta) =
            scan_live_status_parts(&p, Duration::from_secs(20), Duration::from_secs(20), 0);
        assert_ne!(steady_eta, "");
        let base = scan_progress_text(&p);
        let steady = scan_live_status_text(
            &p,
            Duration::from_secs(20),
            Duration::from_secs(20),
            0,
            96,
            avail,
        );
        assert!(steady.contains(&base), "{steady}");
        let walk = scan_live_status_text(
            &Progress::default(),
            Duration::from_secs(30),
            Duration::ZERO,
            0,
            96,
            avail,
        );
        assert!(!walk.contains("eta"));
        let burst = Progress {
            keys_done: 990,
            keys_total: 1000,
            keys_cached: 990,
            ..Progress::default()
        };
        assert!(
            !scan_live_status_text(
                &burst,
                Duration::from_secs(65),
                Duration::from_secs(1),
                0,
                96,
                avail
            )
            .contains("eta")
        );
        assert!(
            !scan_live_status_text(
                &burst,
                Duration::from_secs(90),
                Duration::from_secs(30),
                0,
                96,
                avail
            )
            .contains("eta")
        );
        let big = Progress {
            keys_done: 45000,
            keys_total: 90000,
            keys_fetched: 15000,
            keys_cached: 15000,
            keys_failed: 15000,
            folders_scanned: 450,
            folders_skipped: 32,
            ..Progress::default()
        };
        let got = scan_live_status_text(
            &big,
            Duration::from_secs(3600),
            Duration::from_secs(3600),
            15000,
            96,
            avail,
        );
        assert!(got.len() <= MAX_SCAN_STATUS_LEN, "len {}", got.len());
        assert!(got.contains("folders"), "{got}");
    }

    #[test]
    fn compose_scan_live_status_priority() {
        let p = Progress {
            keys_done: 12000,
            keys_total: 30000,
            keys_fetched: 8000,
            keys_cached: 3000,
            keys_failed: 1000,
            folders_scanned: 200,
            folders_skipped: 15,
            ..Progress::default()
        };
        let (elapsed, resolve) = (Duration::from_secs(20), Duration::from_secs(20));
        let measure = |s: &str| s.chars().count() as i32;
        let cands = scan_status_candidates(&p, elapsed, resolve, 0);
        let full = cands[0].clone();
        assert!(full.contains("eta"));
        assert_eq!(
            compose_scan_live_status(
                &p,
                elapsed,
                resolve,
                0,
                full.chars().count() as i32,
                measure
            ),
            full
        );
        let (counters, elapsed_sfx, _) = scan_live_status_parts(&p, elapsed, resolve, 0);
        let ce = format!("{counters}{elapsed_sfx}");
        assert_eq!(
            compose_scan_live_status(&p, elapsed, resolve, 0, ce.chars().count() as i32, measure),
            ce
        );
        let short = scan_progress_short(&p);
        let se = format!("{short}{elapsed_sfx}");
        assert!(se.chars().count() < ce.chars().count());
        assert_eq!(
            compose_scan_live_status(&p, elapsed, resolve, 0, se.chars().count() as i32, measure),
            se
        );
        let minimal = scan_progress_minimal(&p);
        let floor = scan_progress_floor(&p);
        assert_eq!(minimal, format!("{floor}, 1k failed"));
        assert!(minimal.chars().count() < short.chars().count());
        assert_eq!(
            compose_scan_live_status(
                &p,
                elapsed,
                resolve,
                0,
                minimal.chars().count() as i32,
                measure
            ),
            minimal
        );
        assert_eq!(
            compose_scan_live_status(&p, elapsed, resolve, 0, 1, measure),
            floor
        );
    }

    #[test]
    fn scan_live_status_fits_at_all_dpi() {
        let catching = Progress {
            done: 23402,
            total: 23402,
            resolved: 1118,
            keys_total: 2309,
            keys_done: 1118,
            keys_cached: 418,
            keys_fetched: 700,
            folders_scanned: 3,
            ..Progress::default()
        };
        let cases = [
            (
                Progress {
                    folders_scanned: 3,
                    ..Progress::default()
                },
                Duration::from_secs(30),
                Duration::ZERO,
                0,
            ),
            (
                Progress {
                    keys_done: 12,
                    keys_total: 24,
                    keys_fetched: 7,
                    keys_cached: 5,
                    folders_scanned: 4,
                    folders_skipped: 2,
                    ..Progress::default()
                },
                Duration::from_secs(20),
                Duration::from_secs(20),
                0,
            ),
            (
                Progress {
                    keys_done: 100,
                    keys_total: 1000,
                    keys_fetched: 100,
                    folders_scanned: 9,
                    folders_skipped: 1,
                    ..Progress::default()
                },
                Duration::from_secs(20),
                Duration::from_secs(20),
                0,
            ),
            (
                Progress {
                    keys_done: 12000,
                    keys_total: 30000,
                    keys_fetched: 8000,
                    keys_cached: 3000,
                    keys_failed: 1000,
                    folders_scanned: 200,
                    folders_skipped: 15,
                    ..Progress::default()
                },
                Duration::from_secs(600),
                Duration::from_secs(600),
                0,
            ),
            (
                Progress {
                    keys_done: 45000,
                    keys_total: 90000,
                    keys_fetched: 15000,
                    keys_cached: 15000,
                    keys_failed: 15000,
                    folders_scanned: 450,
                    folders_skipped: 32,
                    ..Progress::default()
                },
                Duration::from_secs(3600),
                Duration::from_secs(3600),
                15000,
            ),
            (
                catching,
                Duration::from_secs(360),
                Duration::from_secs(300),
                418,
            ),
            (
                Progress {
                    keys_done: 999_800,
                    keys_total: 999_900,
                    keys_fetched: 50000,
                    keys_cached: 50000,
                    keys_failed: 899_800,
                    folders_scanned: 999,
                    folders_skipped: 99,
                    ..Progress::default()
                },
                Duration::from_secs(3600),
                Duration::from_secs(3600),
                50000,
            ),
        ];
        for dpi in [96, 120, 144, 192] {
            let cw = scale_px(dpi, MAIN_CLIENT_W);
            let ch = scale_px(dpi, MAIN_CLIENT_H);
            let l = compute_layout(&dirs(0), cw, ch, dpi);
            let avail = l.status.right - l.status.left;
            assert_eq!(avail, scan_status_avail_px(dpi));
            assert_eq!(avail, status_avail_px(cw, dpi));
            let mut widths = vec![avail];
            if dpi > 96 {
                let narrow = status_avail_px(MAIN_CLIENT_W, dpi);
                if narrow < avail {
                    widths.push(narrow);
                }
            }
            for budget in widths {
                for (p, elapsed, resolve, base) in &cases {
                    let text = scan_live_status_text(p, *elapsed, *resolve, *base, dpi, budget);
                    let Some(need) = measure_scan_status_width(dpi, &text) else {
                        return; // no GDI DC: skip (the one native exception)
                    };
                    assert!(
                        need <= budget,
                        "dpi {dpi} avail {budget} need {need}: {text:?}"
                    );
                }
            }
        }
        let big = &cases[6];
        let (counters, elapsed_sfx, eta_sfx) = scan_live_status_parts(&big.0, big.1, big.2, big.3);
        assert_ne!(eta_sfx, "");
        let Some(need) =
            measure_scan_status_width(96, &format!("{counters}{elapsed_sfx}{eta_sfx}"))
        else {
            return;
        };
        assert!(
            need > scan_status_avail_px(96),
            "control is vacuous: {need}"
        );
        let narrow192 = status_avail_px(MAIN_CLIENT_W, 192);
        let Some(min_need) = measure_scan_status_width(192, &scan_progress_minimal(&big.0)) else {
            return;
        };
        assert!(
            min_need > narrow192,
            "minimal must overflow the narrowest budget"
        );
        let floor = scan_progress_floor(&big.0);
        let Some(floor_need) = measure_scan_status_width(192, &floor) else {
            return;
        };
        assert!(floor_need <= narrow192, "floor must fit");
        let fallback = format!("{floor} | {}", format_scan_duration(big.1));
        let Some(fb) = measure_scan_status_width(192, &fallback) else {
            return;
        };
        assert!(fb <= narrow192, "fallback must fit");
        assert_eq!(
            scan_live_status_text(&big.0, big.1, big.2, big.3, 192, narrow192),
            floor
        );
    }

    /// The status line is measured for fitting in exactly the font it paints:
    /// both the measure path and the painter build from `status_font`, counted
    /// per test thread. If either path grows its own face/size again (the old
    /// "Segoe UI" measure drift), its build count for that path drops and this
    /// fails.
    #[test]
    fn status_fit_measures_the_painted_font() {
        let before = STATUS_FONT_BUILDS.with(std::cell::Cell::get);
        let Some(_) = measure_scan_status_width(96, "fit me") else {
            return; // no GDI DC: skip (the one native exception)
        };
        assert!(
            STATUS_FONT_BUILDS.with(std::cell::Cell::get) > before,
            "the measure path stopped building the painted status font"
        );
        let before = STATUS_FONT_BUILDS.with(std::cell::Cell::get);
        let st = UiState::default();
        let rc = Rect {
            left: 0,
            top: 0,
            right: MAIN_CLIENT_W,
            bottom: MAIN_CLIENT_H,
        };
        // SAFETY: a screen-compatible DC for the paint, deleted after.
        let dc = unsafe { Gdi::CreateCompatibleDC(std::ptr::null_mut()) };
        if dc.is_null() {
            return;
        }
        paint_main_card(dc, rc, 96, &st);
        unsafe {
            Gdi::DeleteDC(dc);
        }
        assert!(
            STATUS_FONT_BUILDS.with(std::cell::Cell::get) > before,
            "the paint path stopped building the measured status font"
        );
    }

    #[test]
    fn scan_bar_geometry() {
        for dpi in [96, 120, 144, 192] {
            let cw = scale_px(dpi, MAIN_CLIENT_W);
            let ch = scale_px(dpi, MAIN_CLIENT_H);
            let l = compute_layout(&dirs(0), cw, ch, dpi);
            if dpi == 96 {
                assert_eq!(l.status.right - l.status.left, 420);
            }
            let bar = scan_bar_rect(l.status, dpi);
            assert!(bar.right > bar.left && bar.bottom > bar.top);
            assert!(bar.left >= 0 && bar.top >= 0 && bar.right <= cw && bar.bottom <= ch);
            assert!(
                !rects_intersect(bar, l.status),
                "dpi {dpi}: bar intersects status"
            );
        }
    }

    #[test]
    fn scan_indeterminate_pulse() {
        assert_eq!(scan_indeterminate_pos(Duration::ZERO), 0.0);
        assert_eq!(scan_indeterminate_pos(Duration::from_millis(600)), 0.5);
        assert_eq!(scan_indeterminate_pos(Duration::from_millis(1200)), 0.0);
        for d in [
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(3600),
        ] {
            let v = scan_indeterminate_pos(d);
            assert!((0.0..1.0).contains(&v));
        }
    }

    #[test]
    fn scan_catching_up_text() {
        let p = Progress {
            done: 23402,
            total: 23402,
            resolved: 1118,
            keys_total: 2309,
            keys_done: 1118,
            keys_cached: 418,
            keys_fetched: 700,
            folders_scanned: 3,
            ..Progress::default()
        };
        assert!(scan_catching_up(&p));
        let seg = scan_index_meta_text(&p);
        assert_eq!(
            seg,
            "indexed 23402 of 23402 files | metadata 1118 of 2309 - catching up"
        );
        assert!(scan_progress_text(&p).contains(&seg));
        let measure = |s: &str| s.chars().count() as i32;
        assert_eq!(
            compose_scan_live_status(
                &p,
                Duration::from_secs(20),
                Duration::from_secs(20),
                0,
                seg.chars().count() as i32,
                measure
            ),
            seg
        );
        let (show, det, frac) = scan_bar_progress(&p, true);
        assert!(show && det && frac < 1.0);
    }

    #[test]
    fn scan_finished_text_not_catching_up() {
        let mut p = Progress {
            done: 23402,
            total: 23402,
            resolved: 1118,
            keys_total: 2309,
            keys_done: 1118,
            keys_cached: 418,
            keys_fetched: 700,
            folders_scanned: 3,
            ..Progress::default()
        };
        p.resolved = p.keys_total;
        p.keys_done = p.keys_total;
        assert!(!scan_catching_up(&p));
        let seg = scan_index_meta_text(&p);
        assert_eq!(seg, "indexed 23402 of 23402 files | metadata 2309 of 2309");
        assert!(!seg.contains("catching up"));
        let measure = |s: &str| s.chars().count() as i32;
        assert_eq!(
            compose_scan_live_status(
                &p,
                Duration::from_secs(20),
                Duration::from_secs(20),
                0,
                seg.chars().count() as i32,
                measure
            ),
            seg
        );
        let (_, det, frac) = scan_bar_progress(&p, true);
        assert!(det && frac == 1.0);
        let walk = Progress {
            done: 5,
            total: 10,
            folders_scanned: 1,
            ..Progress::default()
        };
        assert!(!scan_catching_up(&walk));
        assert_eq!(scan_index_meta_text(&walk), "indexed 5 of 10 files");
        let (show, det, _) = scan_bar_progress(&walk, true);
        assert!(show && !det);
        assert_eq!(scan_index_meta_text(&Progress::default()), "");
    }

    #[test]
    fn report_decision_branches() {
        let scratch = Scratch::new("report-decision");
        let missing = scratch.join("missing.json");
        let mut st = UiState::default();
        st.status_text = String::new();
        st.on_report_at(NULL, &missing);
        assert_eq!(st.status_text, REPORT_NOTHING_TEXT);
        assert!(st.status_text.to_lowercase().contains("nothing to report"));

        let dir = scratch.join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        repair::report::write_report(&dir, &repair::report::Report::default()).unwrap();
        st.status_text = String::new();
        st.on_report_at(NULL, &dir.join(repair::report::REPORT_JSON_NAME));
        assert_eq!(st.status_text, REPORT_NOTHING_TEXT);

        let bad = scratch.write("bad.json", "{not json");
        st.status_text = String::new();
        st.on_report_at(NULL, &bad);
        assert_eq!(st.status_text, REPORT_NOTHING_TEXT);

        let one = repair::report::Report {
            entries: vec![repair::report::Entry {
                name: "a.mkv".into(),
                ..repair::report::Entry::default()
            }],
            ..repair::report::Report::default()
        };
        assert_eq!(report_decision(&one, false), (true, ""));
        assert_eq!(
            report_decision(&repair::report::Report::default(), false),
            (false, REPORT_NOTHING_TEXT)
        );
        assert_eq!(report_decision(&one, true), (false, REPORT_NOTHING_TEXT));
    }

    #[test]
    fn report_off_thread() {
        let scratch = Scratch::new("report-thread");
        let dir = scratch.join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        repair::report::write_report(
            &dir,
            &repair::report::Report {
                entries: vec![repair::report::Entry {
                    name: "a.mkv".into(),
                    ..repair::report::Entry::default()
                }],
                ..repair::report::Report::default()
            },
        )
        .unwrap();
        const WANT: &str = "https://example.invalid/potflix/issues/1";
        let mut st = UiState::default();
        st.create_issue = Arc::new(|_| {
            std::thread::sleep(Duration::from_millis(300));
            (WANT.to_string(), None)
        });
        st.open_url = Arc::new(|_| false);
        st.status_text = String::new();
        let start = Instant::now();
        st.on_report_at(NULL, &dir.join(repair::report::REPORT_JSON_NAME));
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "on_report_at blocked"
        );
        assert_eq!(st.status_text, REPORT_PENDING_TEXT);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !st.report_done_text.lock().unwrap().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "worker never published");
            std::thread::sleep(Duration::from_millis(5));
        }
        st.on_report_done();
        assert_eq!(st.status_text, report_launch_text(WANT));
        assert!(st.status_text.contains(WANT));
    }

    /// A failing filing is visible twice: the log line carries the cause (not
    /// just the window-level message), and the status strip names the reason
    /// while the prefilled URL is still offered to the open path.
    #[test]
    fn report_failure_logs_cause_and_names_reason_in_status() {
        const CAUSE: &str = "could not add label: 'needs-repair' not found";
        const PREFILLED: &str = "https://github.com/nathwn12/potflix/issues/new?title=a&body=b";
        let scratch = Scratch::new("report-failure-visible");
        let log_path = scratch.join("potflix-debug.log");
        let log: &'static app_support::debuglog::Logger = Box::leak(Box::new(
            app_support::debuglog::Logger::new(&log_path, 0, 1),
        ));
        assert!(
            set_app_log(log).is_ok(),
            "the process log was already installed"
        );

        let dir = scratch.join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        repair::report::write_report(
            &dir,
            &repair::report::Report {
                entries: vec![repair::report::Entry {
                    name: "a.mkv".into(),
                    ..repair::report::Entry::default()
                }],
                ..repair::report::Report::default()
            },
        )
        .unwrap();

        let mut st = UiState::default();
        st.create_issue = Arc::new(|_| {
            (
                PREFILLED.to_string(),
                Some(repair::Error::caused_by(
                    "repair: gh issue: exit status 1",
                    CAUSE,
                )),
            )
        });
        let opened = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&opened);
        st.open_url = Arc::new(move |_| {
            seen.store(true, Ordering::SeqCst);
            false
        });
        st.on_report_at(NULL, &dir.join(repair::report::REPORT_JSON_NAME));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if !st.report_done_text.lock().unwrap().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "worker never published");
            std::thread::sleep(Duration::from_millis(5));
        }
        st.on_report_done();

        assert!(
            st.status_text.contains("could not add label"),
            "status hides the reason: {:?}",
            st.status_text
        );
        assert!(
            st.status_text.contains(PREFILLED),
            "status hides the prefilled url: {:?}",
            st.status_text
        );
        assert!(
            opened.load(Ordering::SeqCst),
            "the open path was not offered"
        );

        let logged = std::fs::read_to_string(&log_path).expect("read the debug log");
        assert!(
            logged.contains(CAUSE),
            "the log line hides the cause:\n{logged}"
        );
    }

    /// The status strip: a filed issue shows its URL (also when a labelled
    /// retry filed it), and a failed filing names the reason with the prefilled
    /// URL still handed to the open path.
    #[test]
    fn report_status_shows_filed_url_or_failure_reason() {
        const FILED: &str = "https://github.com/nathwn12/potflix/issues/12";
        const PREFILLED: &str = "https://github.com/nathwn12/potflix/issues/new?title=a&body=b";
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let open_ok = Arc::new(AtomicBool::new(true));
        let seen_calls = Arc::clone(&calls);
        let seen_ok = Arc::clone(&open_ok);
        let opened = move |url: &str| {
            seen_calls.lock().unwrap().push(url.to_string());
            seen_ok.load(Ordering::SeqCst)
        };

        // Filed on the first try: the filed URL is shown.
        assert_eq!(
            report_issue_status(FILED, None, &opened),
            report_launch_text(FILED)
        );

        // Filed on the label-less retry: the URL shows; the cause is the log's.
        let err = repair::Error::caused_by(
            "repair: CreateIssue: filed without labels",
            "could not add label: 'needs-repair' not found",
        );
        assert_eq!(
            report_issue_status(FILED, Some(&err), &opened),
            report_launch_text(FILED)
        );

        // Nothing filed: the reason and the prefilled URL both show.
        open_ok.store(false, Ordering::SeqCst);
        let text = report_issue_status(PREFILLED, Some(&err), &opened);
        assert!(text.contains("could not add label"), "status = {text:?}");
        assert!(text.contains(PREFILLED), "status = {text:?}");
        assert!(
            text.to_lowercase().contains("prefilled"),
            "status = {text:?}"
        );

        assert_eq!(
            *calls.lock().unwrap(),
            vec![FILED.to_string(), FILED.to_string(), PREFILLED.to_string()],
            "every non-empty url reaches the open path"
        );
    }

    #[test]
    fn clear_actions_are_distinct() {
        assert_ne!(
            crate::settings::CLEAR_METADATA_STATUS,
            crate::settings::clear_index_status(3)
        );
        let scratch = Scratch::new("clear-distinct");
        let base = scratch.path().to_path_buf();
        let meta = base.join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        let meta_s = meta.to_string_lossy().into_owned();
        let key = "movie|alpha|2001";
        metadata::cache::cache_put_raw(
            &meta_s,
            key,
            format!(
                "{{\"key\":\"movie|alpha|2001\",\"schema\":{},\"media\":{{\"title\":\"Alpha\"}}}}",
                metadata::cache::CACHE_SCHEMA
            )
            .as_bytes(),
        )
        .unwrap();
        let idx = base.join("scanindex.json");
        let idx_s = idx.to_string_lossy().into_owned();
        let seed_index = || {
            let ix = library::scanindex::load_scan_index(&idx_s);
            ix.record(
                r"C:\Media",
                "fp-1",
                metadata::cache::CACHE_SCHEMA,
                &[key.to_string()],
            );
            ix.save(&idx_s).unwrap();
        };
        seed_index();

        let (records, err) = crate::settings::clear_scan_index(&idx_s);
        assert!(err.is_ok());
        assert_eq!(records, 1);
        assert!(!std::path::Path::new(&library::index::index_store_path(&idx_s)).exists());
        assert!(metadata::cache::cache_has(&meta_s, key));

        seed_index();
        crate::settings::clear_metadata_cache(
            &meta_s,
            &base.join("search-cache.json"),
        )
        .unwrap();
        assert!(!metadata::cache::cache_has(&meta_s, key));
        assert_eq!(library::scanindex::load_scan_index(&idx_s).len(), 1);
    }

    #[test]
    fn report_button_layout() {
        let l = compute_layout(&dirs(1), MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        let by_id = |id: i32| l.btns.iter().find(|b| b.id == id).cloned().unwrap();
        let rep = by_id(BTN_REPORT);
        let close = by_id(BTN_CLOSE);
        assert_eq!(rep.label, REPORT_BUTTON_LABEL);
        assert_eq!(rep.style, close.style);
        assert_eq!(rep.rc.top, close.rc.top);
        assert_eq!(rep.rc.bottom, close.rc.bottom);
        assert!(rep.rc.right < close.rc.left);
        assert!(rep.rc.bottom <= l.status.top && rep.rc.right <= MAIN_CLIENT_W);
        assert_eq!(
            button_at_point(
                &l.btns,
                (rep.rc.left + rep.rc.right) / 2,
                (rep.rc.top + rep.rc.bottom) / 2
            ),
            BTN_REPORT
        );
        assert!(main_focus_order().contains(&BTN_REPORT));
        assert_eq!(scan_blocked_reason(true, BTN_REPORT), "");
    }

    #[test]
    fn open_url_texts() {
        assert!(report_opened_text(false).contains("opened"));
        assert!(report_opened_text(true).contains("prefilled"));
        let url = "https://github.com/nathwn12/potflix/issues/new?title=x";
        assert!(report_launch_text(url).contains(url));
        let path = repair_report_path();
        assert!(
            path.to_string_lossy()
                .ends_with(repair::report::REPORT_JSON_NAME)
        );
        assert!(path.to_string_lossy().to_lowercase().contains("potflix"));
    }

    // -- cardview_test.go ---------------------------------------------------

    #[test]
    fn card_view_switch() {
        let mut st = UiState::default();
        st.cur_view = View::Main;
        st.open_card();
        assert_eq!(st.cur_view, View::Card);
        st.close_card();
        assert_eq!(st.cur_view, View::Main);
    }

    #[test]
    fn card_esc() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        assert!(st.on_settings_key_down(NULL, VK_ESCAPE));
        assert_eq!(st.cur_view, View::Main);
        st.cur_view = View::Card;
        assert!(!st.on_settings_key_down(NULL, 0x41));
        assert_eq!(st.cur_view, View::Card);
    }

    #[test]
    fn card_hit_test() {
        let cases: Vec<(CardSpecs, CardKind)> = vec![
            (CardSpecs::default(), CardKind::Series),
            (
                CardSpecs {
                    series: CardSpec {
                        details: "default".into(),
                        state: "synopsis".into(),
                    },
                    movie: CardSpec::default(),
                },
                CardKind::Movie,
            ),
        ];
        for (specs, kind) in &cases {
            let l = compute_card_layout(
                MAIN_CLIENT_W,
                MAIN_CLIENT_H,
                96,
                specs,
                *kind,
                PreviewSurface::default(),
            );
            // Approved narrow at rest (nothing playing): gear, field, Search,
            // pinned Apply — no scope row, no category words, no Change
            // filters; the hero Fix exists only while a title is playing.
            // Card/Back stay wired but hidden (empty rects, never Tab stops).
            let at_rest: Vec<i32> = l.btns.iter().map(|b| b.id).collect();
            assert_eq!(
                at_rest,
                vec![
                    BTN_SETTINGS,
                    BTN_PICKER_FIELD,
                    BTN_PICKER_SEARCH,
                    BTN_PICKER_APPLY,
                    BTN_CARD_BACK,
                    BTN_CARD,
                    BTN_SYNOPSIS,
                ],
                "approved narrow rest state (nothing playing: no hero Fix)"
            );
            // With a title playing the hero ghost joins the rest state.
            let with_player = compute_card_layout_state(
                MAIN_CLIENT_W,
                MAIN_CLIENT_H,
                96,
                specs,
                *kind,
                PreviewSurface::default(),
                &CardLayoutState {
                    advanced: false,
                    has_selection: false,
                    can_apply: false,
                    playing: true,
                    filter: crate::search::KindFilter::All,
                },
            );
            assert!(
                with_player.card_btn(BTN_FIX_MATCH).is_some(),
                "hero Fix present while playing"
            );
            // No disclosure in the redesign: open == rest (payload lives in Settings).
            let open = compute_card_layout_with(
                MAIN_CLIENT_W,
                MAIN_CLIENT_H,
                96,
                specs,
                *kind,
                PreviewSurface::default(),
                true,
                false,
            );
            for id in [
                BTN_PICKER_FIELD,
                BTN_PICKER_SEARCH,
                BTN_PICKER_APPLY,
            ] {
                let rc = open.card_btn(id).unwrap_or_else(|| panic!("id {id} missing"));
                assert!(
                    rc.right <= MAIN_CLIENT_W
                        && rc.bottom <= MAIN_CLIENT_H
                        && rc.left >= 0
                        && rc.top >= 0,
                    "id {id} escapes: {rc:?}"
                );
                assert_eq!(
                    button_at_point(&open.btns, (rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2),
                    id
                );
            }
            // Payload knobs never appear on the Card (Settings CARD owns
            // them); the artwork toggle (70) and the scope ids
            // (85/81/82/87/88) are gone entirely.
            for id in [
                BTN_FIELD_DETAILS,
                BTN_FIELD_STATE,
                BTN_FIELD_SYNOPSIS,
                BTN_FIELD_TIMERS,
                card_state_button(0),
                card_state_button(1),
                card_state_button(2),
                card_state_button(3),
            ] {
                assert!(
                    open.card_btn(id).is_none(),
                    "payload {id} leaked onto the Card"
                );
            }
            // The removed artwork toggle (70) and scope ids (85/81/82/87/88)
            // are gone from every Card composition.
            for id in [70, 85, 81, 82, 87, 88] {
                assert!(
                    open.card_btn(id).is_none() && l.card_btn(id).is_none(),
                    "removed control {id} leaked onto the Card"
                );
            }
            // Visible Card buttons never overlap (hidden wiring rects excluded).
            let visible: Vec<_> = l
                .btns
                .iter()
                .filter(|b| b.rc.right > b.rc.left && b.rc.bottom > b.rc.top)
                .collect();
            for (i, a) in visible.iter().enumerate() {
                for b in visible.iter().skip(i + 1) {
                    assert!(!rects_intersect(a.rc, b.rc), "{} and {} overlap", a.id, b.id);
                }
            }
            // Hero text rows live inside the hero card.
            assert_eq!(l.rows.len(), 2);
            for r in &l.rows {
                assert!(
                    r.left >= l.object.left
                        && r.right <= l.object.right
                        && r.top >= l.object.top
                        && r.bottom <= l.object.bottom,
                    "hero row escapes the hero: {r:?}"
                );
            }
            assert!(
                l.labels.is_empty(),
                "narrow Card carries no section labels"
            );
            // Vertical order: top bar, hero, search, panel, status.
            assert!(
                l.header.bottom <= l.object.top
                    && l.object.bottom <= l.picker_bar.top
                    && l.picker_bar.bottom <= l.stage.top
                    && l.stage.bottom <= l.status.top,
                "topbar/hero/search/panel/status out of order"
            );
            // Hero height cap and geometry tokens.
            assert!(
                l.object.bottom - l.object.top <= scale_px(96, HERO_MAX_H_L),
                "hero exceeds 124px"
            );
            assert_eq!(
                l.obj_cover.right - l.obj_cover.left,
                scale_px(96, HERO_POSTER_W_L)
            );
        }
        let a = compute_card_layout(
            MAIN_CLIENT_W,
            MAIN_CLIENT_H,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
        );
        let b = compute_card_layout(
            MAIN_CLIENT_W,
            MAIN_CLIENT_H,
            96,
            &CardSpecs {
                series: CardSpec {
                    details: "default".into(),
                    state: "none".into(),
                },
                movie: CardSpec::default(),
            },
            CardKind::Series,
            PreviewSurface::default(),
        );
        for (x, y) in a.btns.iter().zip(b.btns.iter()) {
            assert!(r_eq(x.rc, y.rc), "preset {} moved", x.id);
        }
    }

    #[test]
    fn card_text_invariants() {
        for dpi in [96, 120, 144, 192] {
            let cw = scale_px(dpi, MAIN_CLIENT_W);
            let ch = scale_px(dpi, MAIN_CLIENT_H);
            let cases: Vec<(CardSpecs, CardKind)> = vec![
                (CardSpecs::default(), CardKind::Series),
                (
                    CardSpecs {
                        series: CardSpec {
                            details: "default".into(),
                            state: "tag".into(),
                        },
                        movie: CardSpec::default(),
                    },
                    CardKind::Movie,
                ),
            ];
            for (specs, kind) in &cases {
                let l = compute_card_layout(cw, ch, dpi, specs, *kind, PreviewSurface::default());
                let rects = card_text_rects(cw, ch, dpi, &l);
                for r in &rects {
                    // Hidden wiring rects (Card/Back) are empty by design.
                    if r.rc.right <= r.rc.left || r.rc.bottom <= r.rc.top {
                        continue;
                    }
                    assert!(
                        r.rc.right - r.rc.left > 0 && r.rc.bottom - r.rc.top > 0,
                        "dpi {dpi}: {} non-positive",
                        r.id
                    );
                    assert!(
                        r.rc.left >= 0 && r.rc.top >= 0 && r.rc.right <= cw && r.rc.bottom <= ch,
                        "dpi {dpi}: {} escapes",
                        r.id
                    );
                }
                for (i, a) in rects.iter().enumerate() {
                    for b in rects.iter().skip(i + 1) {
                        if a.rc.right <= a.rc.left
                            || a.rc.bottom <= a.rc.top
                            || b.rc.right <= b.rc.left
                            || b.rc.bottom <= b.rc.top
                        {
                            continue;
                        }
                        assert!(
                            !rects_intersect(a.rc, b.rc),
                            "dpi {dpi}: {} and {} intersect",
                            a.id,
                            b.id
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn main_card_affordance() {
        let l = compute_layout(&dirs(2), MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        let s = l.card_link;
        assert!(s.right <= MAIN_CLIENT_W && s.bottom <= MAIN_CLIENT_H && s.left >= 0 && s.top >= 0);
        assert!(s.right > s.left && s.bottom > s.top);
        assert!(s.top >= TAB_TOP_L && s.bottom <= TAB_TOP_L + TAB_H_L);
        assert_eq!(s.right + scale_px(96, SP_2), l.settings.left);
        let rects = main_text_rects(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, &l, 2);
        for (i, a) in rects.iter().enumerate() {
            for b in rects.iter().skip(i + 1) {
                assert!(
                    !rects_intersect(a.rc, b.rc),
                    "{} and {} intersect",
                    a.id,
                    b.id
                );
            }
        }
    }

    #[test]
    fn card_preset_mapping() {
        let (dets, states) = presence::cardspec::card_presets();
        // Line 1 is fixed by the shipped mapping: no Details button exists,
        // so the whole Details id band maps to nothing.
        assert_eq!(dets.len(), 1, "one stored Details value, zero buttons");
        for id in [30, 31, 32, 33] {
            assert!(card_preset_for_button(id).is_none(), "id {id} mapped");
        }
        for (i, p) in states.iter().enumerate() {
            assert_eq!(
                card_preset_for_button(card_state_button(i as i32)),
                Some((1, p.id.clone()))
            );
        }
        // The retired Details ids map to nothing (no Details row is offered),
        // as do the retired kind-toggle ids (50/51).
        for id in [0, 1, 2, 7, 10, 29, 30, 31, 32, 33, 34, 39, 44, 50, 51, 99] {
            assert!(card_preset_for_button(id).is_none(), "id {id} mapped");
        }
        let spec = CardSpec {
            details: "default".into(),
            state: "none".into(),
        };
        // No line-0 branch: the Details row is removed, so even the stored
        // `Default` value never lights a (nonexistent) line-0 control.
        for p in &dets {
            assert_eq!(card_preset_style(0, &p.id, &spec), BTN_TAB);
        }
        for p in &states {
            let want = if p.id == "none" { BTN_TAB_SEL } else { BTN_TAB };
            assert_eq!(card_preset_style(1, &p.id, &spec), want);
        }
    }

    #[test]
    fn preview_rows_quotes_payload() {
        for (i, r) in preview_rows(&Activity::default(), false).iter().enumerate() {
            assert!(!r.is_empty(), "placeholder {i} empty");
        }
        let act = Activity {
            name: "Show Title".into(),
            details: "The Episode".into(),
            state: "S1 E2".into(),
            large_image: "https://example.com/poster.jpg".into(),
            small_image: "play".into(),
            buttons: vec![
                presence::Button {
                    label: "Trailer".into(),
                    url: "https://example.com/trailer".into(),
                },
                presence::Button {
                    label: "IMDb".into(),
                    url: "https://example.com/imdb".into(),
                },
            ],
            ..Activity::default()
        };
        let got = preview_rows(&act, true);
        // The recorded payload is decorated with a caller-compat `small_image`
        // on purpose: the row must still report the cover alone, never a
        // " + play" corner icon, and this equality fails if one is painted.
        assert_eq!(
            got,
            [
                "Header: Show Title",
                "Details: The Episode",
                "State: S1 E2",
                "Images: https://example.com/poster.jpg",
                "Buttons: Trailer -> https://example.com/trailer | IMDb -> https://example.com/imdb",
            ]
        );
        let idle = Activity {
            details: "Browsing".into(),
            state: "Idle".into(),
            large_image: "logo".into(),
            small_image: "stop".into(),
            ..Activity::default()
        };
        let igot = preview_rows(&idle, true);
        assert_eq!(igot[0], "Header: (Discord app name)");
        assert_eq!(igot[2], "State: Idle");
        assert_eq!(igot[3], "Images: logo");
        assert_eq!(igot[4], "Buttons: none");
        let empty = Activity {
            details: "X".into(),
            large_image: "logo".into(),
            small_image: "stop".into(),
            ..Activity::default()
        };
        assert_eq!(preview_rows(&empty, true)[2], "State: ");
    }

    /// The local tooltip keeps the full stored note while the Discord snippet
    /// is capped and explicitly ellipsized; hover, focus and pin each reveal
    /// the tooltip.
    #[test]
    fn synopsis_tooltip_keeps_full_note_and_tracks_focus() {
        let mut st = UiState::default();
        let long = "A retired operative living quietly is pulled back when a ruthless syndicate burns his home. He crosses continents, gathers old allies, and faces a final choice that will decide everything he loves and more.";
        st.record_preview(
            CardKind::Series,
            "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer".to_string(),
            Some(Action {
                running: true,
                has_start: true,
                start: 999,
                ct: 1000,
                ..Action::default()
            }),
            Resolved {
                series: "Breaking Bad".to_string(),
                episode_name: "Problem Dog".to_string(),
                season: 4,
                number: 7,
                synopsis: long.to_string(),
                ..Resolved::default()
            },
        );
        assert_eq!(st.preview_synopsis(), long, "full stored synopsis");
        // The stored note came from an episode pin: it must never ride the
        // Discord tooltip, because it names what happens in the episode being
        // watched (owner-safe rule, 2026-11-03).
        let (card, _) = st.preview_activity();
        assert!(
            card.large_text.is_empty(),
            "episode-sourced prose must never ride: {:?}",
            card.large_text
        );
        // A work-level note is the only synopsis allowed on the card.
        st.record_preview(
            CardKind::Series,
            "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer".to_string(),
            Some(Action {
                running: true,
                has_start: true,
                start: 999,
                ct: 1000,
                ..Action::default()
            }),
            Resolved {
                series: "Breaking Bad".to_string(),
                episode_name: "Problem Dog".to_string(),
                season: 4,
                number: 7,
                synopsis: long.to_string(),
                synopsis_is_work: true,
                ..Resolved::default()
            },
        );
        let (card, _) = st.preview_activity();
        let snippet_body = card.large_text.trim_end_matches('\u{2026}');
        assert!(
            !snippet_body.is_empty() && long.starts_with(snippet_body),
            "snippet must be a prefix of the stored note: {:?}",
            card.large_text
        );
        assert!(
            card.large_text.ends_with('\u{2026}')
                && card.large_text.encode_utf16().count() <= 128,
            "snippet must be capped and explicitly ellipsized: {:?}",
            card.large_text
        );
        assert!(!st.synopsis_visible(), "hidden by default");
        st.hover_btn = BTN_SYNOPSIS;
        assert!(st.synopsis_visible(), "hover reveals");
        st.hover_btn = 0;
        st.focus_id = BTN_SYNOPSIS;
        assert!(st.synopsis_visible(), "keyboard focus reveals");
        st.focus_id = 0;
        st.synopsis_pinned = true;
        assert!(st.synopsis_visible(), "pin keeps it open");
    }

    #[test]
    fn preview_record_round_trip() {
        let mut st = UiState::default();
        // Before the first live push there is no preview: the view falls
        // back to Series with nothing composed.
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        assert!(!st.preview_activity().1);
        let raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer".to_string();
        let action = Some(Action {
            running: true,
            has_start: true,
            start: 999,
            ct: 1000,
            ..Action::default()
        });
        let resolved = Resolved::default();
        st.record_preview(
            CardKind::Series,
            raw.clone(),
            action.clone(),
            resolved.clone(),
        );
        // The live push derives the displayed kind: the recorded series item
        // composes.
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        let (got, have) = st.preview_activity();
        assert!(have);
        let want = presence::compose_card(
            &raw,
            action.as_ref(),
            &resolved,
            false,
            &st.card_specs_state.for_kind(CardKind::Series),
        );
        assert_eq!(got, want);
        // A live movie push re-derives the displayed kind: the movie slot
        // composes now.
        st.record_preview(
            CardKind::Movie,
            raw.clone(),
            action.clone(),
            resolved.clone(),
        );
        let (got_movie, have_movie) = st.preview_activity();
        assert!(have_movie);
        let want_movie = presence::compose_card(
            &raw,
            action.as_ref(),
            &resolved,
            true,
            &st.card_specs_state.for_kind(CardKind::Movie),
        );
        assert_eq!(got_movie, want_movie);
    }

    /// Picking a line under SERIES changes the composed preview immediately
    /// while the MOVIE spec is untouched (and the reverse).
    #[test]
    fn card_per_kind_line_changes_preview() {
        let raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer".to_string();
        let action = Action {
            running: true,
            has_start: true,
            start: 1000,
            ct: 1000,
            ..Action::default()
        };
        let resolved = Resolved::default();
        // The movie leg carries year/genres/length (no synopsis), so the
        // T5-A movie default state is the non-empty `Year - Genres - Runtime`
        // line and the Minimal pick below visibly changes it.
        let movie_resolved = Resolved {
            year: 2024,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            ..Resolved::default()
        };
        let mut st = UiState::default();
        // Record movie first, then series: the displayed kind follows the
        // latest live push — Series.
        st.record_preview(
            CardKind::Movie,
            raw.clone(),
            Some(action.clone()),
            movie_resolved.clone(),
        );
        st.record_preview(
            CardKind::Series,
            raw.clone(),
            Some(action.clone()),
            resolved.clone(),
        );

        let (before_series, _) = st.preview_activity();
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        // Pick a state line under SERIES only: the tag drops the episode
        // title from the default `code / title` pair.
        assert!(
            st.card_specs_state
                .set_line(CardKind::Series, CardLine::State, "tag")
        );
        let (after_series, have) = st.preview_activity();
        assert!(have);
        assert_ne!(
            (before_series.details, before_series.state),
            (after_series.details.clone(), after_series.state.clone()),
            "series preview must re-render after a series line pick"
        );
        // The movie spec is untouched.
        assert!(st.card_specs_state.for_kind(CardKind::Movie).is_default());
        // Re-push the live movie: the displayed kind re-derives to Movie.
        st.record_preview(
            CardKind::Movie,
            raw.clone(),
            Some(action.clone()),
            movie_resolved.clone(),
        );
        let (movie_before, have_movie) = st.preview_activity();
        assert!(have_movie);
        // Pick a state line under MOVIE only: series keeps its pick.
        assert!(
            st.card_specs_state
                .set_line(CardKind::Movie, CardLine::State, "none")
        );
        let (movie_after, _) = st.preview_activity();
        assert_eq!(movie_after.state, "");
        assert_ne!(movie_before.state, movie_after.state);
        assert_eq!(
            st.card_specs_state.for_kind(CardKind::Series).state,
            "tag",
            "series pick must survive a movie pick"
        );
    }

    /// Each live push re-derives the displayed kind: the recorded item for
    /// that kind is previewed, and the layout lits the displayed kind's
    /// preset spec.
    #[test]
    fn card_live_kind_drives_preview_and_lit() {
        let series_raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer".to_string();
        let movie_raw = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer".to_string();
        let action = Action {
            running: true,
            has_start: true,
            start: 1000,
            ct: 1000,
            ..Action::default()
        };
        let mut st = UiState::default();
        assert!(
            st.card_specs_state
                .set_line(CardKind::Series, CardLine::Details, "title")
        );
        assert!(
            st.card_specs_state
                .set_line(CardKind::Movie, CardLine::State, "none")
        );
        st.record_preview(
            CardKind::Series,
            series_raw.clone(),
            Some(action.clone()),
            Resolved::default(),
        );
        st.record_preview(
            CardKind::Movie,
            movie_raw.clone(),
            Some(action.clone()),
            Resolved::default(),
        );

        // The displayed kind follows the latest live push: re-push the
        // series item to preview it again.
        st.record_preview(
            CardKind::Series,
            series_raw.clone(),
            Some(action.clone()),
            Resolved::default(),
        );
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        let (series_act, have_series) = st.preview_activity();
        assert!(have_series);
        let want_series = presence::compose_card(
            &series_raw,
            Some(&action),
            &Resolved::default(),
            false,
            &st.card_specs_state.for_kind(CardKind::Series),
        );
        assert_eq!(series_act, want_series);

        st.record_preview(
            CardKind::Movie,
            movie_raw.clone(),
            Some(action.clone()),
            Resolved::default(),
        );
        assert_eq!(st.displayed_card_kind(), CardKind::Movie);
        let (movie_act, have_movie) = st.preview_activity();
        assert!(have_movie);
        let want_movie = presence::compose_card(
            &movie_raw,
            Some(&action),
            &Resolved::default(),
            true,
            &st.card_specs_state.for_kind(CardKind::Movie),
        );
        assert_eq!(movie_act, want_movie);
        assert_ne!(
            (series_act.details, series_act.state),
            (movie_act.details, movie_act.state),
            "the two kinds must preview different recorded items"
        );

        // The lit preset segments follow the displayed kind's spec (State row
        // only: line 1 is fixed by the shipped mapping).
        for kind in [CardKind::Series, CardKind::Movie] {
            let l = compute_card_layout(
                MAIN_CLIENT_W,
                MAIN_CLIENT_H,
                96,
                &st.card_specs_state,
                kind,
                PreviewSurface::default(),
            );
            let spec = st.card_specs_state.for_kind(kind);
            let (_, states) = presence::cardspec::card_presets();
            for p in &states {
                assert_eq!(
                    card_preset_style(1, &p.id, &spec),
                    if p.id == spec.state {
                        BTN_TAB_SEL
                    } else {
                        BTN_TAB
                    }
                );
            }
            // The layout's own preset buttons carry the same lit rule.
            let ns = states.len() as i32;
            for b in l.btns.iter().filter(|b| {
                (BTN_CARD_STATE_AT..BTN_CARD_STATE_AT + ns).contains(&b.id)
            }) {
                if let Some((line, id)) = card_preset_for_button(b.id) {
                    assert_eq!(b.style, card_preset_style(line, &id, &spec));
                }
            }
        }
    }

    /// Probe-compat for the removed manual toggle: `set_card_kind` drives the
    /// derived kind directly (the app-crate preview probes still sweep both
    /// slots through it) and never touches the stored config.
    ///
    /// NOTE: the no-persist proof below is the weaker one -- it compares the
    /// loaded effective specs before and after, so a save that round-trips
    /// byte-identical would pass unseen. It proves the shim performs no
    /// observable persist through the config paths the view owns, not that it
    /// performs no write at all.
    #[test]
    fn card_kind_probe_shim_is_view_only() {
        // Runs under a scratch root -- never the owner's real config -- with
        // the shared env/config/mirror serial held for both reads, so a
        // mid-flight scratch repoint from another lane cannot land between
        // them.
        crate::test_env::EnvGuard::with_scratch("card-kind-probe", |_dir| {
            let before = app_support::config::load_config().effective_card_specs();
            let mut st = UiState::default();
            let specs_before = st.card_specs_state.clone();
            st.set_card_kind(CardKind::Movie);
            assert_eq!(st.displayed_card_kind(), CardKind::Movie);
            assert_eq!(st.card_specs_state, specs_before);
            st.set_card_kind(CardKind::Series);
            assert_eq!(st.displayed_card_kind(), CardKind::Series);
            assert_eq!(st.card_specs_state, specs_before);
            assert_eq!(
                app_support::config::load_config().effective_card_specs(),
                before,
                "toggling the kind must not persist anything"
            );
        });
    }

    /// Before the first live push there is no preview: `have == false` with
    /// the exact placeholder rows (never a sample card). The first push
    /// derives the displayed kind and the preview appears.
    #[test]
    fn card_missing_kind_has_no_preview() {
        let mut st = UiState::default();
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        let (idle_act, idle_have) = st.preview_activity();
        assert!(!idle_have);
        assert_eq!(idle_act, Activity::default());
        assert_eq!(
            preview_rows(&idle_act, idle_have),
            [
                "Header: -".to_string(),
                "Details: -".to_string(),
                "State: -".to_string(),
                "Images: -".to_string(),
                "Buttons: -".to_string(),
            ]
        );
        let raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer".to_string();
        let resolved = Resolved::default();
        st.record_preview(CardKind::Series, raw.clone(), None, resolved.clone());
        // The preview follows the live kind — Series — so it appears even
        // though the movie slot was never recorded, composing the recorded
        // series item (not placeholders).
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        let (act, have) = st.preview_activity();
        assert!(have);
        assert_eq!(
            act,
            presence::compose_card(
                &raw,
                None,
                &resolved,
                false,
                &st.card_specs_state.for_kind(CardKind::Series),
            )
        );
    }

    /// The retired kind-toggle ids (50/51) dispatch to nothing through the
    /// real card action and map to no preset. The reachable control (a State
    /// preset still maps) proves the ids are inert because they are retired,
    /// not because the whole action is a no-op.
    #[test]
    fn card_freed_kind_ids_are_inert() {
        let mut st = UiState::default();
        let before = st.displayed_card_kind();
        let before_specs = st.card_specs_state.clone();
        let before_status = st.status_text.clone();
        for id in [50, 51] {
            let _ = st.do_card_action(NULL, id);
            assert_eq!(st.displayed_card_kind(), before, "id {id} changed kind");
            assert_eq!(st.card_specs_state, before_specs, "id {id} changed specs");
            assert_eq!(st.status_text, before_status, "id {id} changed status");
        }
        assert!(card_preset_for_button(50).is_none());
        assert!(card_preset_for_button(51).is_none());
        assert!(
            card_preset_for_button(card_state_button(0)).is_some(),
            "the State row must still map: the retired ids are inert, the action is not"
        );
        assert!(!st.preview_activity().1);
    }

    /// The retired Details ids (30..33) dispatch to nothing through the real
    /// card action: no Details button is laid out, focused, or hit-tested, so
    /// no no-op control remains on the Card view. The reachable control (a
    /// State preset still maps and is still laid out) proves the ids are
    /// inert because the Details row is gone, not because the whole action is
    /// a no-op.
    #[test]
    fn card_retired_details_ids_are_inert() {
        let mut st = UiState::default();
        let before_specs = st.card_specs_state.clone();
        let before_status = st.status_text.clone();
        for id in [30, 31, 32, 33] {
            // The mapping itself is pinned by `card_preset_mapping` above; here
            // only the real action's inertness matters.
            let _ = st.do_card_action(NULL, id);
        }
        assert_eq!(st.card_specs_state, before_specs, "specs untouched");
        assert_eq!(st.status_text, before_status, "status untouched");
        // State presets are no longer a Settings row: the line-2 presets live
        // behind the wide Card disclosure, so the reachable control proves the
        // retired Details ids are inert because the Details row is gone, not
        // because the whole action is a no-op.
        assert!(
            card_preset_for_button(card_state_button(0)).is_some(),
            "the State row must still map: the retired ids are inert, the action is not"
        );
        // Details never laid out on Card or Settings; the State preset lives
        // in the wide Card disclosure, never in slimmed Settings.
        let l = compute_card_layout_with(
            MAIN_CLIENT_W,
            MAIN_CLIENT_H,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
            true,
            false,
        );
        assert!(
            l.btns.iter().all(|b| !(30..34).contains(&b.id)),
            "a Details button is still laid out on the Card"
        );
        let s = compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, false, "balanced", "amoled");
        assert!(
            s.btns.iter().all(|b| !(30..34).contains(&b.id)),
            "a Details button is still laid out in Settings"
        );
        assert!(
            s.btns.iter().all(|b| b.id != card_state_button(0)),
            "no State preset button lives in slimmed Settings"
        );
        let wide = compute_card_layout_state(
            1180,
            700,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
            &CardLayoutState {
                advanced: true,
                has_selection: false,
                can_apply: false,
                playing: false,
                filter: crate::search::KindFilter::All,
            },
        );
        assert!(
            wide.card_btn(card_state_button(0)).is_some(),
            "the open wide disclosure still offers the State preset"
        );
        let shut = compute_card_layout(
            1180,
            700,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
        );
        assert!(
            shut.card_btn(card_state_button(0)).is_none(),
            "the State preset hides until the wide disclosure opens"
        );
        assert!(
            !card_focus_order_with(true).iter().any(|id| (30..34).contains(id)),
            "a Details id is still a focus stop"
        );
    }

    /// D4, red-capable: a matched series must never render as a movie.
    /// The pre-fix predicate is reproduced inline -- the shipped v1 layout lit
    /// the segments from `picker.movie` (user state) and the match path never
    /// wrote the matched kind back -- so the first arm below fails against the
    /// old rule and the second arm is the shipped rule: the SELECTED RECORD owns
    /// the preview's kind; there is no scope control left to say otherwise.
    #[test]
    fn card_kind_never_renders_movie_for_a_series_match() {
        let cand = metadata::Candidate {
            kind: metadata_core::types::Kind::SERIES,
            title: "The Office".to_string(),
            year: 2005,
            provider: "tvmaze".to_string(),
            provider_id: "526".to_string(),
            poster: String::new(),
            synopsis: "A mockumentary at a paper company.".to_string(),
            genres: "Comedy".to_string(),
            length: "22 min".to_string(),
            episode_count: 9,
        };
        // Pre-fix predicate: the lit segment was the user's own stale bool.
        let pre_fix_movie_lit = true;
        assert!(
            pre_fix_movie_lit,
            "pre-fix arm: user state lit Movie; the assertion below is the red one"
        );
        // Shipped rule: selecting the series makes the object and the preview a
        // series, whatever the search scope was.
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.filter = crate::search::KindFilter::Movies; // the user's filter
        st.picker.candidates = vec![cand];
        st.picker.open = true;
        assert!(st.select_candidate(0));
        assert!(!st.preview_object().expect("preview object").2, "series");
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        assert!(!st.preview_activity().0.name.to_lowercase().contains("film"));
        // And no scope control exists to follow anything: the removed scope
        // ids (85/81/82/87/88) are absent from every composition, whatever
        // the engine's internal filter is.
        for filter in [
            crate::search::KindFilter::All,
            crate::search::KindFilter::Movies,
            crate::search::KindFilter::Series,
            crate::search::KindFilter::AnimeSeries,
            crate::search::KindFilter::AnimeMovies,
        ] {
            let l = compute_card_layout_state(
                MAIN_CLIENT_W,
                MAIN_CLIENT_H,
                96,
                &CardSpecs::default(),
                st.displayed_card_kind(),
                PreviewSurface::default(),
                &CardLayoutState {
                    advanced: true,
                    has_selection: true,
                    can_apply: false,
                    playing: false,
                    filter,
                },
            );
            for id in [85, 81, 82, 87, 88] {
                assert!(
                    l.btns.iter().all(|b| b.id != id),
                    "removed scope id {id} laid out under {filter:?}"
                );
            }
        }
    }

    /// D5/D2, red-capable: at rest the window carries three payload controls or
    /// fewer, exactly one search-like verb, and no repair affordance outside a
    /// result list. The v1 composition is measured in the same test so the
    /// threshold is shown discriminating, not merely asserted.
    #[test]
    fn advanced_disclosure_keeps_the_rest_state_at_three_controls() {
        for (cw, ch) in [(1180, 700), (463, 703)] {
            let l = compute_card_layout(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
            );
            let knobs = l
                .btns
                .iter()
                .filter(|b| {
                    (BTN_FIELD_DETAILS..=BTN_FIELD_TIMERS).contains(&b.id)
                        || (BTN_CARD_STATE_AT..BTN_CARD_STATE_AT + 8).contains(&b.id)
                })
                .count();
            assert!(knobs <= 3, "{cw}x{ch}: {knobs} payload controls at rest");
            let searches = l.btns.iter().filter(|b| b.id == BTN_PICKER_SEARCH).count();
            assert_eq!(searches, 1, "{cw}x{ch}: exactly one search verb");
            // Approved narrow: hero Fix is the repair affordance and exists
            // only while a title is playing; wide keeps its footer Fix. No
            // list repair outside the hero/panel is the invariant.
            if cw < 1000 {
                assert!(
                    l.btns.iter().all(|b| b.id != BTN_FIX_MATCH),
                    "{cw}x{ch}: hero Fix must be absent with nothing playing"
                );
                let with_player = compute_card_layout_state(
                    cw,
                    ch,
                    96,
                    &CardSpecs::default(),
                    CardKind::Series,
                    PreviewSurface::default(),
                    &CardLayoutState {
                        advanced: false,
                        has_selection: false,
                        can_apply: false,
                        playing: true,
                        filter: crate::search::KindFilter::All,
                    },
                );
                assert!(
                    with_player.btns.iter().any(|b| b.id == BTN_FIX_MATCH),
                    "{cw}x{ch}: hero Fix missing while playing"
                );
            }
            // Approved narrow: no scope row — the removed scope ids
            // (85/81/82/87/88) are absent; payload lives in Settings CARD.
            // Wide keeps the legacy disclosure (minus scope and artwork).
            let open = compute_card_layout_with(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
                true,
                false,
            );
            if cw < 1000 {
                for id in [85, 81, 82, 87, 88] {
                    assert!(
                        open.card_btn(id).is_none(),
                        "{cw}x{ch}: removed scope {id} laid out"
                    );
                }
            } else {
                for id in [
                    BTN_FIELD_DETAILS,
                    BTN_FIELD_STATE,
                    BTN_FIELD_SYNOPSIS,
                    BTN_FIELD_TIMERS,
                    card_state_button(0),
                    card_state_button(1),
                    card_state_button(2),
                    card_state_button(3),
                ] {
                    assert!(
                        open.card_btn(id).is_some(),
                        "{cw}x{ch}: control {id} is not one click away"
                    );
                }
            }
            // Every interactive target is at least 32 px tall, and nothing
            // overlaps with the disclosure open. The synopsis affordance is
            // the full-row text band (hover + keyboard focus for its
            // tooltip): full width, 18 px tall, still reachable — exempt from
            // the touch floor, never from the client/overlap checks.
            for b in &open.btns {
                if b.rc.right <= b.rc.left || b.rc.bottom <= b.rc.top {
                    continue;
                }
                if b.id != BTN_SYNOPSIS {
                    assert!(
                        b.rc.bottom - b.rc.top >= 32,
                        "{cw}x{ch}: target {} is {} px tall",
                        b.id,
                        b.rc.bottom - b.rc.top
                    );
                }
                assert!(
                    b.rc.left >= 0 && b.rc.top >= 0 && b.rc.right <= cw && b.rc.bottom <= ch,
                    "{cw}x{ch}: target {} escapes the client: {:?}",
                    b.id,
                    b.rc
                );
            }
            for (i, a) in open.btns.iter().enumerate() {
                for b in open.btns.iter().skip(i + 1) {
                    assert!(
                        !rects_intersect(a.rc, b.rc),
                        "{cw}x{ch}: buttons {} and {} overlap with the disclosure open",
                        a.id,
                        b.id
                    );
                }
            }
            // The v1 composition, same size: the numbers this pass removed.
            let v1 = compute_card_layout_shipped_v1(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
            );
            // The v1 composition still carries the removed ids as raw numbers
            // (81/82 kind buttons, 70 artwork toggle): the shipped-v1 layout
            // is frozen history, so the count below keeps discriminating.
            let v1_knobs = v1
                .btns
                .iter()
                .filter(|b| {
                    b.id == 81
                        || b.id == 82
                        || (70..=BTN_FIELD_TIMERS).contains(&b.id)
                        || (BTN_CARD_STATE_AT..BTN_CARD_STATE_AT + 8).contains(&b.id)
                })
                .count();
            println!("{cw}x{ch}: payload controls at rest: v1={v1_knobs} now={knobs}");
            assert!(
                v1_knobs > 3 || cw < 500,
                "{cw}x{ch}: the v1 arm must be over the threshold for this test to discriminate"
            );
        }
    }

    /// Owner order: no dead-cache line under Clear metadata. The Settings
    /// layout carries no such rect, no text id, and Clear metadata stays.
    #[test]
    fn no_dead_cache_line_in_settings() {
        let l = compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, false, "turbo", "amoled");
        assert!(
            l.labels.iter().all(|lab| lab.id != "settings.dead"),
            "dead-cache text rect is gone"
        );
        assert!(
            settings_text_rects(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, &l)
                .iter()
                .all(|t| t.id != "settings.dead"),
            "no painted dead-cache line"
        );
        assert!(
            l.settings_btn(BTN_CLEAR_CACHE).is_some(),
            "Clear metadata survives"
        );
    }

    #[test]
    fn count_negatives_empty_dir() {
        let scratch = Scratch::new("count-neg");
        let dir = scratch.path().to_string_lossy().into_owned();
        assert_eq!(metadata::cache::count_negatives(&dir), 0);
        for name in ["notes.txt", ".tmp-abc.json", "partial"] {
            std::fs::write(scratch.join(name), "{}").unwrap();
        }
        std::fs::write(scratch.join("corrupt.json"), "{not json").unwrap();
        assert_eq!(metadata::cache::count_negatives(&dir), 0);
        for i in 0..2 {
            let body = format!(
                r#"{{"key":"k{i}","schema":{},"not_found":true,"checked_at":"2026-01-01T00:00:00Z"}}"#,
                metadata::cache::cache_schema()
            );
            metadata::cache::cache_put_raw(&dir, &format!("k{i}"), body.as_bytes()).unwrap();
        }
        let pos = format!(
            r#"{{"key":"p","schema":{},"media":{{"source":"tvmaze","sourceId":"1","kind":"series","title":"S","fetchedAt":"2026-01-01T00:00:00Z"}}}}"#,
            metadata::cache::cache_schema()
        );
        metadata::cache::cache_put_raw(&dir, "p", pos.as_bytes()).unwrap();
        assert_eq!(metadata::cache::count_negatives(&dir), 2);
        assert_eq!(
            metadata::cache::count_negatives(&scratch.join("missing").to_string_lossy()),
            0
        );
    }

    // -- scan_status_test.go ------------------------------------------------

    #[test]
    fn scan_progress_text_folders() {
        let p = Progress {
            keys_done: 12,
            keys_total: 24,
            keys_fetched: 3,
            keys_cached: 5,
            folders_scanned: 4,
            folders_skipped: 2,
            ..Progress::default()
        };
        assert_eq!(
            scan_progress_text(&p),
            "Scan 12/24 keys | 3 fetched, 5 cached | 4/2 folders"
        );
    }

    #[test]
    fn scan_progress_text_all_cached() {
        let p = Progress {
            keys_done: 24,
            keys_total: 24,
            keys_fetched: 0,
            keys_cached: 24,
            folders_scanned: 6,
            folders_skipped: 1,
            ..Progress::default()
        };
        assert_eq!(
            scan_progress_text(&p),
            "Scan 24/24 keys | 0 fetched, 24 cached | 6/1 folders"
        );
    }

    #[test]
    fn scan_progress_text_four_bucket() {
        let p = Progress {
            keys_done: 10,
            keys_total: 20,
            keys_fetched: 2,
            keys_cached: 7,
            keys_failed: 1,
            folders_scanned: 3,
            folders_skipped: 1,
            ..Progress::default()
        };
        assert_eq!(
            scan_progress_text(&p),
            "Scan 10/20 keys | 2 fetched, 7 cached, 1 failed | 3/1 folders"
        );
    }

    #[test]
    fn scan_progress_text_huge_elides() {
        let p = Progress {
            keys_done: 12_345_678,
            keys_total: 98_765_432,
            keys_fetched: 123_456,
            keys_cached: 12_222_222,
            folders_scanned: 1_234_567,
            folders_skipped: 7_654_321,
            ..Progress::default()
        };
        let got = scan_progress_text(&p);
        assert!(
            !got.contains("12345678") && !got.contains("98765432"),
            "{got}"
        );
        assert!(got.contains("folders") && got.contains('/'));
        assert!(got.len() <= MAX_SCAN_STATUS_LEN, "len {}", got.len());
        // Extreme int64-scale counts stay bounded and keep the suffix.
        let huge = Progress {
            keys_done: 1 << 60,
            keys_total: 1 << 60,
            keys_fetched: 1 << 60,
            keys_cached: 0,
            folders_scanned: 1 << 40,
            folders_skipped: 1 << 40,
            ..Progress::default()
        };
        let got_huge = scan_progress_text(&huge);
        assert!(!got_huge.contains(&(1_i64 << 60).to_string()), "{got_huge}");
        assert!(got_huge.contains("folders"));
        assert!(got_huge.len() <= MAX_SCAN_STATUS_LEN);
    }

    #[test]
    fn scan_done_text_folders() {
        let st = Stats {
            keys: 24,
            keys_cached: 5,
            keys_fetched: 2,
            keys_not_found: 1,
            files: 10,
            folders_scanned: 4,
            folders_skipped: 2,
            ..Stats::default()
        };
        assert_eq!(
            scan_done_text("Scan", &st),
            "Scan done: 24 keys - 5 cached, 2 fetched - 4/2 folders"
        );
        let r = scan_done_text("Refresh", &st);
        assert!(r.starts_with("Refresh done: ") && r.contains("4/2 folders"));
    }

    #[test]
    fn scan_done_text_all_cached() {
        let st = Stats {
            keys: 5,
            keys_cached: 5,
            keys_fetched: 0,
            keys_not_found: 0,
            files: 5,
            folders_scanned: 3,
            folders_skipped: 0,
            ..Stats::default()
        };
        assert_eq!(
            scan_done_text("Scan", &st),
            "Scan done: 5 keys - 5 cached, 0 fetched - 3/0 folders"
        );
    }

    #[test]
    fn scan_done_text_nothing_to_fetch() {
        let st = Stats {
            keys: 7,
            keys_cached: 7,
            keys_fetched: 0,
            keys_not_found: 0,
            files: 9,
            folders_scanned: 4,
            folders_skipped: 3,
            ..Stats::default()
        };
        assert_eq!(
            scan_done_text("Scan", &st),
            "Scan done: nothing to fetch (7 cached) - 3 folders skipped"
        );
        assert_eq!(
            scan_done_text("Refresh", &st),
            "Refresh done: nothing to fetch (7 cached) - 3 folders skipped"
        );
    }

    #[test]
    fn scan_done_text_huge_elides() {
        let st = Stats {
            keys: 12_345_678,
            keys_cached: 12_222_222,
            keys_fetched: 123_456,
            keys_not_found: 999_999,
            files: 2_345_678,
            folders_scanned: 1_234_567,
            folders_skipped: 7_654_321,
            ..Stats::default()
        };
        let got = scan_done_text("Scan", &st);
        assert!(!got.contains("12345678"), "{got}");
        assert!(got.contains("folders"));
        assert!(got.len() <= MAX_SCAN_STATUS_LEN);
    }

    #[test]
    fn scan_status_keeps_folders_suffix() {
        let long = format!("{} 4/2 folders", "k".repeat(MAX_SCAN_STATUS_LEN));
        let got = fit_scan_status(&long);
        assert!(got.len() <= MAX_SCAN_STATUS_LEN, "len {}", got.len());
        assert!(got.contains("4/2 folders"), "{got}");
        let st = Stats {
            keys: 24,
            keys_cached: 5,
            keys_fetched: 2,
            keys_not_found: 1,
            files: 10,
            folders_scanned: 4,
            folders_skipped: 2,
            ..Stats::default()
        };
        let got_done = scan_done_text(&"R".repeat(MAX_SCAN_STATUS_LEN), &st);
        assert!(got_done.len() <= MAX_SCAN_STATUS_LEN);
        assert!(got_done.contains("4/2 folders"));
    }

    #[test]
    fn build_scan_options_contract() {
        let d = vec![
            format!("{}:{}", 'C', r"\Media\Movies"),
            format!("{}:{}", 'D', r"\Shows"),
        ];
        let scan = build_scan_options(d.clone(), false, None);
        assert!(scan.use_index);
        assert_eq!(scan.index_path, crate::settings::scan_index_path());
        assert_eq!(scan.cache_dir, crate::settings::metadata_dir());
        assert!(!scan.force);
        assert_eq!(scan.dirs, d);
        let refresh = build_scan_options(d.clone(), true, None);
        assert!(
            refresh.use_index
                && refresh.index_path == crate::settings::scan_index_path()
                && refresh.cache_dir == crate::settings::metadata_dir()
        );
        assert!(refresh.force);
    }

    #[test]
    fn scan_cancelled_text_cases() {
        assert_eq!(
            scan_cancelled_text(12, 24),
            "Scan cancelled - 12/24 keys done, progress kept"
        );
        let huge = scan_cancelled_text(12_345_678, 98_765_432);
        assert!(!huge.contains("12345678") && !huge.contains("98765432"));
        assert!(huge.starts_with("Scan cancelled - ") && huge.ends_with("progress kept"));
        assert!(huge.len() <= MAX_SCAN_STATUS_LEN);
    }

    #[test]
    fn scan_result_cancelled_never_fails() {
        let cancelled = Stats {
            keys: 24,
            keys_done: 12,
            cancelled: true,
            ..Stats::default()
        };
        let got = scan_result_text("Scan", &cancelled, None);
        assert_eq!(got, scan_cancelled_text(12, 24));
        assert!(!got.contains("failed"));
        assert_eq!(
            scan_result_text("Scan", &cancelled, Some(&UiScanError::other("boom"))),
            scan_cancelled_text(12, 24)
        );
        let bare = Stats {
            keys: 10,
            keys_done: 4,
            ..Stats::default()
        };
        assert_eq!(
            scan_result_text("Scan", &bare, Some(&UiScanError::cancelled())),
            scan_cancelled_text(4, 10)
        );
        let ok = Stats {
            keys: 24,
            keys_cached: 5,
            keys_fetched: 2,
            keys_not_found: 1,
            files: 10,
            folders_scanned: 4,
            folders_skipped: 2,
            ..Stats::default()
        };
        assert_eq!(
            scan_result_text("Scan", &ok, None),
            scan_done_text("Scan", &ok)
        );
        assert!(
            scan_result_text("Scan", &ok, Some(&UiScanError::other("disk gone")))
                .contains("failed")
        );
    }

    #[test]
    fn scan_failed_suffix() {
        let clean = Progress {
            keys_done: 12,
            keys_total: 24,
            keys_fetched: 3,
            keys_cached: 5,
            folders_scanned: 4,
            folders_skipped: 2,
            ..Progress::default()
        };
        assert_eq!(
            scan_progress_text(&clean),
            "Scan 12/24 keys | 3 fetched, 5 cached | 4/2 folders"
        );
        let bad = Progress {
            keys_failed: 2,
            ..clean.clone()
        };
        assert_eq!(
            scan_progress_text(&bad),
            "Scan 12/24 keys | 3 fetched, 5 cached, 2 failed | 4/2 folders"
        );
        let st = Stats {
            keys: 7,
            keys_cached: 7,
            keys_fetched: 0,
            keys_not_found: 0,
            files: 9,
            folders_scanned: 4,
            folders_skipped: 3,
            ..Stats::default()
        };
        assert_eq!(
            scan_done_text("Scan", &st),
            "Scan done: nothing to fetch (7 cached) - 3 folders skipped"
        );
        let st_bad = Stats {
            keys_failed: 1,
            ..st
        };
        let got = scan_done_text("Scan", &st_bad);
        assert!(!got.contains("nothing to fetch"), "{got}");
        assert!(got.contains("1 failed"), "{got}");
    }

    #[test]
    fn scan_control_state() {
        assert_eq!(scan_button_label(false), "Scan");
        assert!(scan_refresh_enabled(false));
        assert_eq!(scan_button_label(true), "Cancel");
        assert!(!scan_refresh_enabled(true));
        assert_eq!(scan_button_label(false), "Scan");
        assert!(scan_refresh_enabled(false));
    }

    // -- settings_scan_test.go ----------------------------------------------

    /// Owner order 2026-11-03: Scan speed has no Settings row. The layout
    /// carries no scan segment and no SCAN caption; the profile still
    /// persists through the settings core (covered in `settings::tests`).
    #[test]
    fn settings_scan_hit_test() {
        let l = compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, false, "balanced", "amoled");
        assert_eq!(l.btns.len(), 9);
        for id in [BTN_SCAN_GENTLE, BTN_SCAN_BALANCED, BTN_SCAN_TURBO] {
            assert!(
                l.settings_btn(id).is_none(),
                "scan segment {id} is gone from Settings"
            );
        }
        assert!(
            l.labels.iter().all(|t| t.id != "settings.scan"),
            "settings still paints the SCAN SPEED caption"
        );
    }

    #[test]
    fn scan_segment_model() {
        assert_eq!(crate::settings::scan_segment_label("gentle"), "Gentle");
        assert_eq!(crate::settings::scan_segment_label("balanced"), "Balanced");
        assert_eq!(crate::settings::scan_segment_label("turbo"), "Turbo");
        assert_eq!(crate::settings::scan_segment_label("bogus"), "Turbo");
        for (profile, id) in [
            ("gentle", BTN_SCAN_GENTLE),
            ("balanced", BTN_SCAN_BALANCED),
            ("turbo", BTN_SCAN_TURBO),
        ] {
            assert_eq!(crate::settings::scan_button_for_profile(profile), id);
            assert_eq!(crate::settings::scan_profile_for_button(id), profile);
        }
        assert_eq!(crate::settings::scan_profile_for_button(BTN_BACK), "");
        for cur in ["gentle", "balanced", "turbo"] {
            for seg in ["gentle", "balanced", "turbo"] {
                let want = if seg == cur { BTN_TAB_SEL } else { BTN_TAB };
                assert_eq!(crate::settings::scan_segment_style(seg, cur), want);
            }
        }
        for cur in ["", "   ", "bogus"] {
            assert_eq!(
                crate::settings::scan_button_for_profile(cur),
                BTN_SCAN_TURBO
            );
            assert_eq!(
                crate::settings::scan_segment_style("turbo", cur),
                BTN_TAB_SEL
            );
            assert_eq!(
                crate::settings::scan_segment_style("balanced", cur),
                BTN_TAB
            );
        }
        assert!(
            crate::settings::SCAN_HINT_TEXT.contains("Turbo")
                && crate::settings::SCAN_HINT_TEXT.contains("(default)")
        );
    }

    #[test]
    fn mouse_xy_splits_lparam() {
        assert_eq!(mouse_xy(0x00C8_0064), (0x64, 0x00C8));
    }

    /// The Scan/Refresh worker is real (`go\window.go:2661-2674`): with a fake
    /// runner (no network) over a temp dir, `start_library_scan` invokes the
    /// runner with options built from the configured dirs, the progress
    /// callback publishes into the shared snapshot, and the UI-thread landing
    /// clears `scan_active`/`scan_cancel` with a terminal line and honest
    /// counters. Reverting the worker body to the stub leaves the runner
    /// uninvoked and the snapshot empty, so this test goes red.
    #[test]
    fn library_scan_worker_runs_runner_and_lands_done() {
        let tmp = Scratch::new("libscan");
        tmp.write("Show.S01E02.Pilot.1080p.mkv", "x");
        let dir = tmp.path().to_string_lossy().into_owned();

        let mut st = UiState::default();
        st.arm_timer = Arc::new(|_| {});
        st.kill_timer = Arc::new(|_| {});
        let d = dir.clone();
        st.scan_dirs = Arc::new(move || vec![d.clone()]);

        let ran = Arc::new(AtomicBool::new(false));
        let seen_dirs = Arc::new(Mutex::new(Vec::new()));
        let seen_force = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        let sd = seen_dirs.clone();
        let sf = seen_force.clone();
        st.scan_run = Arc::new(move |_cancel: &library::Cancel, opts: library::Options| {
            r.store(true, Ordering::SeqCst);
            *sd.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = opts.dirs.clone();
            sf.store(opts.force, Ordering::SeqCst);
            if let Some(progress) = &opts.progress {
                progress(Progress {
                    keys_total: 4,
                    keys_done: 1,
                    keys_cached: 1,
                    folders_scanned: 1,
                    ..Progress::default()
                });
            }
            Ok(Stats {
                keys: 4,
                keys_done: 4,
                keys_cached: 4,
                files: 1,
                unique: 1,
                resolved: 1,
                cached: 1,
                folders_scanned: 1,
                ..Stats::default()
            })
        });

        st.start_library_scan(NULL, false);
        assert!(st.scan_active, "run must latch active until done lands");
        assert!(st.scan_cancel.is_some());

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let landed = st
                .scan_shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .stats
                .keys
                == 4;
            if landed || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ran.load(Ordering::SeqCst), "worker must invoke the runner");
        assert_eq!(
            *seen_dirs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![dir],
            "options must be built from the configured dirs"
        );
        assert!(!seen_force.load(Ordering::SeqCst));

        // The real progress callback published into the shared snapshot.
        assert_eq!(
            st.scan_shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .progress
                .keys_done,
            1
        );

        // The UI-thread landing, exactly as the wndproc arms run it.
        st.publish_scan_progress();
        assert_eq!(st.scan_progress.keys_done, 1);
        assert_eq!(st.scan_progress.keys_total, 4);
        st.publish_scan_done();
        assert_eq!(st.scan_stats.keys, 4);
        st.on_scan_done(NULL);
        assert!(!st.scan_active, "done must unlatch the run");
        assert!(st.scan_cancel.is_none());
        assert!(st.scan_stats.keys > 0, "counters must be honest");
        assert_eq!(st.status_text, scan_done_text("Scan", &st.scan_stats));
    }

    /// The `WM_APP_SCAN_PROGRESS` / `WM_APP_SCAN_DONE` arms publish the shared
    /// snapshot before doing their existing work, driven through the REAL
    /// `main_wnd_proc` against the process-wide `UI` global. This is the
    /// real-path proof the headless `--selftest` cannot carry: a live scan
    /// ends in `start_catch_up`, whose background thread performs live
    /// provider lookups and writes the owner's real index/cache, so neither
    /// `--selftest` nor `cargo test` may run the default runner. Assertions
    /// cover the full terminal state, never the status line alone.
    #[test]
    fn scan_messages_land_through_real_wndproc() {
        let _ui_serial = UI_GLOBAL_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        {
            let mut st = ui();
            st.arm_timer = Arc::new(|_| {});
            st.kill_timer = Arc::new(|_| {});
            st.scan_active = true;
            st.scan_cancel = Some(library::Cancel::background());
            st.scan_force = false;
            st.scan_start = now - Duration::from_secs(30);
            st.scan_last_paint = now - Duration::from_secs(3600);
            st.scan_resolve_start = None;
            st.scan_resolve_base = 0;
            st.scan_progress = Progress::default();
            st.scan_stats = Stats::default();
            st.scan_err = None;
            *st.scan_shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = ScanSnapshot {
                progress: Progress {
                    keys_total: 4,
                    keys_done: 1,
                    keys_cached: 1,
                    folders_scanned: 1,
                    ..Progress::default()
                },
                stats: Stats::default(),
                err: None,
            };
            st.status_text = "Scan starting...".into();
        }
        // SAFETY: scan arms touch no controls; NULL posts are no-ops.
        unsafe {
            main_wnd_proc(NULL, WM_APP_SCAN_PROGRESS, 0, 0);
        }
        {
            let st = ui();
            assert_eq!(st.scan_progress.keys_done, 1);
            assert_eq!(st.scan_progress.keys_total, 4);
            assert!(
                st.status_text.starts_with("Scan "),
                "live line, got {:?}",
                st.status_text
            );
            assert!(st.scan_active);
        }
        let terminal = Stats {
            keys: 4,
            keys_done: 4,
            keys_cached: 4,
            files: 1,
            unique: 1,
            resolved: 1,
            cached: 1,
            folders_scanned: 1,
            ..Stats::default()
        };
        {
            let st = ui();
            *st.scan_shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = ScanSnapshot {
                progress: Progress {
                    keys_total: 4,
                    keys_done: 4,
                    keys_cached: 4,
                    folders_scanned: 1,
                    ..Progress::default()
                },
                stats: terminal,
                err: None,
            };
        }
        // SAFETY: scan arms touch no controls; NULL posts are no-ops.
        unsafe {
            main_wnd_proc(NULL, WM_APP_SCAN_DONE, 0, 0);
        }
        {
            let st = ui();
            assert!(!st.scan_active, "done must unlatch the run");
            assert!(st.scan_cancel.is_none());
            assert_eq!(st.scan_stats.keys, 4);
            assert!(st.scan_stats.keys_done > 0, "counters must be honest");
            assert_eq!(st.status_text, scan_done_text("Scan", &terminal));
        }
        // Leave the global pristine for the next serial holder.
        {
            let mut st = ui();
            st.arm_timer = Arc::new(default_arm_timer);
            st.kill_timer = Arc::new(default_kill_timer);
            st.scan_run = Arc::new(default_scan_run);
            st.scan_dirs = Arc::new(default_scan_dirs);
            st.scan_shared = Arc::new(Mutex::new(ScanSnapshot::default()));
            st.scan_active = false;
            if let Some(c) = st.scan_cancel.take() {
                c.cancel();
            }
            st.scan_force = false;
            st.scan_progress = Progress::default();
            st.scan_stats = Stats::default();
            st.scan_err = None;
            st.scan_resolve_start = None;
            st.scan_resolve_base = 0;
            st.scan_start = Instant::now();
            st.scan_last_paint = Instant::now();
            st.status_text = "Ready".into();
        }
    }

    /// Driving a State preset through the real UI path publishes the new
    /// effective specs to the live mirror: the regression test for the
    /// Card-view-preset-needs-restart defect. It runs under a scratch config
    /// root -- never the owner's real config -- with the shared env/mirror/
    /// config serial held throughout; the guard restores the root and the
    /// mirror on drop, panic-safe.
    #[test]
    fn card_preset_save_publishes_live_specs() {
        crate::test_env::EnvGuard::with_scratch("card-save-publish", |_dir| {
            // Two valid presets in a row: whatever the scratch store held,
            // the second save always changes it, so the publish path is
            // always exercised.
            let mut st = UiState::default();
            let _ = st.do_card_action(NULL, card_state_button(1));
            let _ = st.do_card_action(NULL, card_state_button(2));
            let live = app_support::config::live_card_specs();
            assert_eq!(live.series.state, "synopsis");
            assert_eq!(
                live,
                app_support::config::load_config().effective_card_specs(),
                "the mirror must carry the saved effective specs"
            );
        });
    }

    /// A failed Card-view preset save publishes nothing: the mirror keeps the
    /// last good specs. The save is driven through the real UI path with a
    /// reachable State preset; the retired Details id migrates at the config
    /// seam instead, asserted separately below.
    #[test]
    fn card_preset_failed_save_publishes_nothing() {
        // The guard holds the shared env/mirror/config serial for the whole
        // test: the mirror snapshot, the sentinel publish, the scratch
        // repoint, the driven save, and the mirror read all happen under it,
        // and the guard restores both root and mirror on drop, panic-safe.
        // (Declared before the sentinel guard shape matters: a Drop guard
        // declared after the EnvGuard would drop first, still under the
        // serial. Here the shared guard itself carries the mirror, so one
        // guard covers both.)
        let mut env = crate::test_env::EnvGuard::scratch("card-save-fail");
        let mut sentinel = CardSpecs::default();
        assert!(sentinel.set_line(CardKind::Series, CardLine::State, "tag"));
        app_support::config::set_live_card_specs(sentinel.clone());
        // Break the store without touching the real file: point the cache root
        // at a regular file, so the load yields the zero config and the atomic
        // save's `create_dir_all` fails.
        let blocker = env.dir().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        env.repoint(&blocker);
        let mut st = UiState::default();
        let _ = st.do_card_action(NULL, card_state_button(3));
        let live = app_support::config::live_card_specs();
        let status = st.status_text.clone();
        assert_eq!(live, sentinel, "a failed save must publish nothing");
        assert_eq!(status, "Save failed.");
        // The retired `"title"` Details id never reaches the UI path: stored
        // specs carrying it migrate to `default` on load and on save.
        assert_eq!(
            CardSpec {
                details: "title".into(),
                state: "tag".into(),
            }
            .migrated()
            .details,
            "default",
            "retired Details ids migrate at the config seam"
        );
    }

    // -- Fix match picker (pure half) ----------------------------------------

    fn fix_candidate(
        title: &str,
        year: i32,
        kind: Kind,
        provider: &str,
        episodes: usize,
    ) -> Candidate {
        Candidate {
            kind,
            title: title.to_string(),
            year,
            provider: provider.to_string(),
            provider_id: "p1".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: episodes,
        }
    }

    fn fix_pin(season: i32, number: i32) -> metadata::overrides::EpisodePin {
        metadata::overrides::EpisodePin {
            season,
            number,
            absolute: 0,
            title: format!("episode {number}"),
            synopsis: String::new(),
        }
    }

    #[test]
    fn fix_match_enable_rule_needs_something_playing() {
        assert!(!fix_match_enabled(None));
        let now = app_support::config::NowPlaying {
            raw_title: "Attack.on.Titan.S02E01.mkv - PotPlayer".to_string(),
            path: format!("{}:{}", 'D', r"\Anime\Attack on Titan\Season 2\S02E01.mkv"),
        };
        assert!(fix_match_enabled(Some(&now)));
    }

    #[test]
    fn fix_match_prefill_episode_only_filename_uses_folder_show() {
        let raw = "S01E02-A Warm View Together [ACC73481].mkv - PotPlayer";
        let path = format!(
            "{}:{}",
            'D',
            r"\2ANIMES\Atri S01 1080p BDRip 10 bits DD+ x265-EMBER\S01E02-A Warm View Together [ACC73481].mkv"
        );
        let prefill = fix_match_prefill_name(raw, &path);
        assert!(
            prefill.contains("Atri"),
            "folder show must win: {prefill:?}"
        );
        assert!(
            !prefill.contains("Warm View"),
            "episode title must not leak into the search: {prefill:?}"
        );
    }

    #[test]
    fn fix_match_prefill_bare_movie_carries_the_year() {
        let raw = "Dune.Part.Two.2024.1080p.mkv - PotPlayer";
        for path in ["", "Dune.Part.Two.2024.1080p.mkv"] {
            assert_eq!(
                fix_match_prefill_name(raw, path),
                "Dune Part Two (2024)",
                "no folder context, the parsed movie year rides the field (path {path:?})"
            );
        }
        // The parse inverts the Card's own `Title (Year)` rendering.
        assert_eq!(
            parse_title_year(&fix_match_prefill_name(raw, "")),
            ("Dune Part Two".to_string(), 2024),
            "start_search reads the carried year back"
        );
    }

    /// A dotted release name leaves its year and quality tags in the resolver's
    /// parse: the tail after the release year is not part of the title, and a
    /// name the resolver repeats collapses.
    #[test]
    fn fix_match_prefill_drops_the_release_junk_tail() {
        let junk = "A.Silent.Voice.2016.1080p.BluRay.Opus2.0.H.265-LYS1TH3A.mkv";
        let raw = format!("{junk} - PotPlayer");
        assert_eq!(
            fix_match_prefill_name(&raw, ""),
            "A Silent Voice (2016)",
            "the year ends the title"
        );
        assert_eq!(movie_search_title("2012 2012", 2009), "2012");
        assert_eq!(
            movie_search_title("Blade Runner 2049", 2017),
            "Blade Runner 2049",
            "a title numeral that is not the release year survives"
        );
        assert_eq!(
            movie_search_title("2001 A Space Odyssey", 1968),
            "2001 A Space Odyssey",
            "a leading numeral survives when it is not the release year"
        );
    }

    /// A series docker that adds only release junk to the resolver's own show
    /// yields the show, never the junk; a folder that is the only place the
    /// show is named still supplies it.
    #[test]
    fn fix_match_prefill_series_prefers_the_resolvers_show_over_junk() {
        let path = format!(
            "{}:{}",
            'D',
            r"\1MOVIES\Severance.S01.1080p.WEBRip.DS4K.AV1.DDP.5.1-dAV1nci\Severance.S01E01.1080p.WEBRip.DS4K.AV1.DDP.5.1-dAV1nci.mkv"
        );
        let raw = "Severance.S01E01.1080p.WEBRip.DS4K.AV1.DDP.5.1-dAV1nci.mkv - PotPlayer";
        assert_eq!(fix_match_prefill_name(raw, &path), "Severance");
    }

    #[test]
    fn fix_match_blocked_reason_names_idle_only_for_picker() {
        assert_eq!(
            fix_match_blocked_reason(false, BTN_FIX_MATCH),
            FIX_MATCH_IDLE_REASON
        );
        assert!(!fix_match_blocked_reason(false, BTN_FIX_MATCH).is_empty());
        assert_eq!(fix_match_blocked_reason(true, BTN_FIX_MATCH), "");
        assert_eq!(fix_match_blocked_reason(false, BTN_ADD), "");
        assert_eq!(fix_match_blocked_reason(true, BTN_ADD), "");
    }

    #[test]
    fn fix_match_scope_movie_offers_no_radios() {
        let choices = fix_match_scope_choices(
            &format!("{}:{}", 'D', r"\Movies\Dune (2021)\Dune.2021.mkv"),
            true,
        );
        assert!(choices.is_empty());
    }

    #[test]
    fn fix_match_scope_season_folder_offers_season_and_show() {
        let path = format!("{}:{}", 'D', r"\Anime\Attack on Titan\Season 2\S02E01.mkv");
        let choices = fix_match_scope_choices(&path, false);
        assert_eq!(choices.len(), 2);
        let season = &choices[0];
        assert_eq!(season.scope, Scope::Season);
        assert!(season.default);
        assert_eq!(
            season.key,
            metadata::overrides::season_key_for(&path).unwrap()
        );
        assert!(season.key.ends_with("attack on titan\\season 2"));
        assert_eq!(
            season.label,
            "This season \u{2014} Attack on Titan \\ Season 2"
        );
        assert_eq!(season.note, None);
        let show = &choices[1];
        assert_eq!(show.scope, Scope::Series);
        assert!(!show.default);
        assert_eq!(
            show.key,
            metadata::overrides::series_key_for(&path).unwrap()
        );
        assert!(show.key.ends_with("attack on titan"));
        assert_ne!(season.key, show.key);
        assert_eq!(show.label, "Whole show  \u{2014} Attack on Titan");
        assert_eq!(show.note, None);
    }

    #[test]
    fn fix_match_scope_release_folder_offers_single_radio() {
        let path = format!("{}:{}", 'D', r"\Anime\Atri S01 1080p BDRip\S01E02.mkv");
        let choices = fix_match_scope_choices(&path, false);
        assert_eq!(choices.len(), 1, "one folder, one target, no duplicate");
        let only = &choices[0];
        assert_eq!(only.scope, Scope::Series);
        assert!(only.default);
        assert_eq!(
            only.key,
            metadata::overrides::series_key_for(&path).unwrap()
        );
        assert_eq!(
            only.key,
            metadata::overrides::season_key_for(&path).unwrap()
        );
        assert_eq!(only.label, "Whole show  \u{2014} Atri S01 1080p BDRip");
        assert_eq!(
            only.note.as_deref(),
            Some(
                "This folder is both the show folder and its season, so one match covers every \
                 episode in it."
            )
        );
    }

    #[test]
    fn fix_match_candidate_row_series_carries_episodes() {
        let c = fix_candidate("Attack on Titan", 2013, Kind::SERIES, "anilist", 25);
        assert_eq!(
            candidate_row_text(&c),
            "Attack on Titan (2013) \u{b7} AniList \u{b7} 25 episodes"
        );
        let one = fix_candidate("Show", 2020, Kind::SERIES, "tvmaze", 1);
        assert_eq!(
            candidate_row_text(&one),
            "Show (2020) \u{b7} TVmaze \u{b7} 1 episode"
        );
    }

    #[test]
    fn fix_match_candidate_row_movie_omits_episodes() {
        let c = fix_candidate("Dune", 2021, Kind::MOVIE, "cinemeta", 0);
        assert_eq!(candidate_row_text(&c), "Dune (2021) \u{b7} Cinemeta");
        let counted = fix_candidate("Dune", 2021, Kind::MOVIE, "cinemeta", 12);
        assert_eq!(
            candidate_row_text(&counted),
            "Dune (2021) \u{b7} Cinemeta",
            "a movie never prints an episode count"
        );
    }

    #[test]
    fn fix_match_status_strings_are_frozen() {
        assert_eq!(
            fix_match_matched_status("Attack on Titan", 25),
            "Matched to Attack on Titan \u{2014} 25 episodes now show their own title and cover."
        );
        assert_eq!(fix_match_matched_status("Dune", 0), "Matched to Dune.");
        assert_eq!(
            fix_match_refreshed_status("Attack on Titan"),
            "Refreshed Attack on Titan."
        );
        assert_eq!(
            fix_match_sub_line("Attack.on.Titan.S02E01.mkv - PotPlayer"),
            "Matching \"Attack.on.Titan.S02E01.mkv\""
        );
    }

    // -- Fix match provisional correction (T2-A2) --------------------------

    // Every test below that touches the store or `LOCALAPPDATA` runs under
    // the crate-shared [`crate::test_env::EnvGuard`] serial: one lock for the
    // `window` and `search` lanes, panic-safe restore via `Drop`.

    fn fix_series_candidate() -> Candidate {
        Candidate {
            kind: Kind::SERIES,
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: "https://art/show-picked.jpg".to_string(),
            synopsis: "A show about a show.".to_string(),
            genres: "Drama".to_string(),
            length: "45 min".to_string(),
            episode_count: 3,
        }
    }

    fn fix_movie_candidate() -> Candidate {
        Candidate {
            kind: Kind::MOVIE,
            title: "Dune: Part Two".to_string(),
            year: 2024,
            provider: "cinemeta".to_string(),
            provider_id: "tt15239678".to_string(),
            poster: "https://art/dune-search.jpg".to_string(),
            synopsis: "Paul Atreides unites with the Fremen.".to_string(),
            genres: "Sci-Fi . Adventure".to_string(),
            length: "2h 46m".to_string(),
            episode_count: 0,
        }
    }

    fn fix_movie_path() -> String {
        format!("{}:{}", 'D', r"\Films\Dune Part Two.mkv")
    }

    fn fix_full_series_override(key: &str) -> Override {
        Override {
            scope: Scope::Series,
            key: key.to_string(),
            title: "Show".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "169".to_string(),
            poster: "https://art/show-full.jpg".to_string(),
            media: None,
            episodes: vec![fix_pin(1, 1), fix_pin(1, 2)],
            created_at_ms: 1,
        }
    }

    /// A headless dialog state: null window handles only. Never spawns (no
    /// worker) and never paints; `apply_screen` on null handles is a Win32
    /// no-op, so the outcome arms below run exactly as the wndproc drives
    /// them.
    fn fix_headless_state(path: String, is_movie: bool) -> FixMatchState {
        FixMatchState {
            owner: NULL,
            dlg: NULL,
            dpi: 96,
            raw_title: String::new(),
            path,
            seed: String::new(),
            is_movie,
            matched: None,
            choices: Vec::new(),
            candidates: Vec::new(),
            picked: None,
            ctl: Vec::new(),
            fonts: Vec::new(),
            bg_brush: 0 as Gdi::HBRUSH,
            slot: Arc::new(Mutex::new(None)),
            generation: 0,
            busy: false,
            busy_label: FIX_MATCH_SEARCHING.to_string(),
            progress_phase: 0,
            screen: FixScreen::Search,
            status: None,
            searched: false,
            status_error: false,
            layout: FixMatchLayout::default(),
        }
    }

    #[test]
    fn fix_match_provisional_status_lines_are_frozen() {
        assert_eq!(
            FIX_MATCH_FETCHING_STATUS,
            "Matched; fetching episodes\u{2026}"
        );
        assert_eq!(
            fix_match_episodes_pending_status("Show"),
            "Matched to Show; episode details unavailable \u{2014} the card keeps the corrected title."
        );
    }

    #[test]
    fn fix_match_provisional_saves_before_any_network() {
        crate::test_env::EnvGuard::with_scratch("fix-provisional", |_dir| {
            let path = fix_series_path();
            let key = metadata::overrides::series_key_for(&path).unwrap();
            let cand = fix_series_candidate();
            // No network has run: the helper only builds D's identity record
            // and writes it with C's correction save.
            let over = fix_match_save_provisional(&cand, Scope::Series, &key, &path)
                .expect("provisional save");
            assert_eq!(over.title, "Show");
            assert_eq!(over.provider_id, "169");
            assert!(
                over.media
                    .as_ref()
                    .is_some_and(|media| media.synopsis == "A show about a show."),
                "provisional: the merged synopsis rides the media snapshot"
            );
            assert!(over.episodes.is_empty(), "provisional: no episode table");
            // The card resolves the corrected series (show + provider code)
            // without any episode table.
            let store = metadata::overrides::load().expect("reload");
            let got = store.resolve_for(&path).expect("corrected latch");
            assert_eq!(got.scope, Scope::Series);
            assert_eq!(got.title, "Show");
            assert_eq!(got.provider_id, "169");
            assert_eq!(got.poster, "https://art/show-picked.jpg");
            assert!(got.episodes.is_empty());
        });
    }

    #[test]
    fn fix_match_full_fetch_replaces_provisional_without_duplicates() {
        crate::test_env::EnvGuard::with_scratch("fix-full-replaces", |_dir| {
            let path = fix_series_path();
            let key = metadata::overrides::series_key_for(&path).unwrap();
            fix_match_save_provisional(&fix_series_candidate(), Scope::Series, &key, &path)
                .expect("provisional");
            // The later full pin carries the episode table plus the refreshed
            // poster; it must REPLACE the provisional, not sit beside it.
            fix_match_put(&path, &fix_full_series_override(&key)).expect("full save");
            let store = metadata::overrides::load().expect("reload");
            let got = store.resolve_for(&path).expect("full latch");
            assert_eq!(got.episodes.len(), 2);
            assert_eq!(got.episodes[0].title, "episode 1");
            assert_eq!(got.poster, "https://art/show-full.jpg");
            assert_eq!(
                store.entries().len(),
                1,
                "one latch for the show: no duplicate provisional beside the full record"
            );
        });
    }

    #[test]
    fn fix_match_movie_to_series_clears_the_movie_latch() {
        crate::test_env::EnvGuard::with_scratch("fix-movie-to-series", |_dir| {
            // A legacy movie latch for a file the filename query called a
            // movie but that is really a series episode: the correction scope
            // comes from the user's explicit TV choice through the real
            // `FixMatchState`, never from the filename flag.
            let path = fix_movie_path();
            let movie_key = metadata::overrides::movie_key(&path);
            assert!(!movie_key.is_empty());
            let mut store = metadata::overrides::load()
                .unwrap_or_else(|_| metadata::overrides::Store::empty());
            store
                .put(Override {
                    scope: Scope::Movie,
                    key: movie_key,
                    title: "Wrong Movie".to_string(),
                    year: 2020,
                    provider: "cinemeta".to_string(),
                    provider_id: "tt0000000".to_string(),
                    poster: String::new(),
                    media: None,
                    episodes: Vec::new(),
                    created_at_ms: 0,
                })
                .expect("seed movie latch");
            assert!(store.resolve_for(&path).is_some());
            // Movie-classified file plus the explicit TV choice: the choice
            // offers the series table and a series-kind target (the filename
            // scope would be Movie).
            let mut st = fix_headless_state(path.clone(), true);
            assert!(st.apply_pick_kind(false));
            let choices = st.effective_choices();
            assert!(
                !choices.is_empty(),
                "the TV choice offers a series target even for a movie file"
            );
            assert!(
                choices.iter().all(|c| c.scope != Scope::Movie),
                "no Movie scope survives the TV choice: {choices:?}"
            );
            let (scope, key) = st.selected_target().expect("series target");
            assert_eq!(scope, Scope::Series, "movie file + TV choice saves Series");
            fix_match_save_provisional(&fix_series_candidate(), scope, &key, &path)
                .expect("series correction");
            let store = metadata::overrides::load().expect("reload");
            assert!(
                store.lookup_movie(&path).is_none(),
                "the corrected series must clear the old movie latch"
            );
            let got = store.resolve_for(&path).expect("series latch");
            assert_eq!(got.scope, Scope::Series);
            assert_eq!(got.title, "Show");
            assert!(
                !got.poster.is_empty(),
                "a series provisional carries the candidate cover (series card)"
            );
        });
    }

    #[test]
    fn fix_match_series_to_movie_keeps_the_show_latch() {
        crate::test_env::EnvGuard::with_scratch("fix-series-to-movie", |_dir| {
            // A file mis-latched as a series episode that is really a movie:
            // the same path carries the old season latch and the new movie
            // correction, so C's correction save clears the season key derived
            // from it while the show-wide series latch stands. The scope comes
            // from the SELECTED movie candidate through the real `FixMatchState`.
            let path = fix_series_path();
            let show_key = metadata::overrides::series_key_for(&path).unwrap();
            let season_key = metadata::overrides::season_key_for(&path).unwrap();
            let mut store = metadata::overrides::load()
                .unwrap_or_else(|_| metadata::overrides::Store::empty());
            for (scope, key) in [(Scope::Series, show_key), (Scope::Season, season_key)] {
                store
                    .put(Override {
                        scope,
                        key,
                        title: "Wrong Show".to_string(),
                        year: 2001,
                        provider: "tvmaze".to_string(),
                        provider_id: "1".to_string(),
                        poster: String::new(),
                        media: None,
                        episodes: Vec::new(),
                        created_at_ms: 0,
                    })
                    .expect("seed latch");
            }
            // Series-classified file plus the explicit Movie choice: the
            // choice offers the single movie target (the filename scope would
            // stay Series/Season).
            let mut st = fix_headless_state(path.clone(), false);
            st.choices = fix_match_scope_choices(&path, false);
            assert!(st.apply_pick_kind(true));
            assert!(st.effective_is_movie());
            assert!(
                st.effective_choices().is_empty(),
                "the Movie choice offers no Series/Season radios"
            );
            let (scope, key) = st.selected_target().expect("movie target");
            assert_eq!(scope, Scope::Movie, "series file + Movie choice saves Movie");
            // The movie correction removes the old season latch for the file's
            // own season folder -- and never the show-wide series latch, which
            // keeps serving every other episode -- and the card becomes the
            // movie (no poster: a movie renders from its media snapshot).
            fix_match_save_provisional(&fix_movie_candidate(), scope, &key, &path)
                .expect("movie correction");
            let store = metadata::overrides::load().expect("reload");
            assert!(
                store.lookup_series(&path).is_some(),
                "the corrected movie must keep the show-wide series latch"
            );
            assert!(
                store.lookup_season(&path).is_none(),
                "the corrected movie must clear the old season latch"
            );
            let got = store.resolve_for(&path).expect("movie latch");
            assert_eq!(got.scope, Scope::Movie);
            assert_eq!(got.title, "Dune: Part Two");
            assert!(
                got.poster.is_empty(),
                "a movie provisional carries no poster"
            );
            // A sibling episode still resolves through the surviving show
            // latch. It derives from the same fixture path helper, never a
            // hand-built literal.
            let sibling = std::path::Path::new(&path)
                .parent()
                .expect("the fixture lives in a season folder")
                .join("s01e02.mkv")
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                store
                    .resolve_for(&sibling)
                    .expect("series latch")
                    .scope,
                Scope::Series,
                "the show latch still serves the other episodes"
            );
        });
    }

    #[test]
    fn fix_candidate_is_movie_prefers_kind_then_table_then_provider() {
        // The leg-reported kind wins.
        assert!(fix_candidate_is_movie(&fix_movie_candidate()));
        assert!(!fix_candidate_is_movie(&fix_series_candidate()));
        // A series entry without a table is still a series (no fabricated
        // episodes).
        let mut bare_series = fix_series_candidate();
        bare_series.episode_count = 0;
        assert!(
            !fix_candidate_is_movie(&bare_series),
            "TVmaze stays series even with no episode table"
        );
        let anilist = fix_candidate("Show", 2020, Kind::SERIES, "anilist", 0);
        assert!(!fix_candidate_is_movie(&anilist));
        // An unknown kind with a table is a series; without one the provider
        // decides (series-only legs stay series, otherwise movie).
        let mut unknown_table =
            fix_candidate("Show", 2020, Kind::UNKNOWN, "cinemeta", 3);
        assert!(!fix_candidate_is_movie(&unknown_table));
        unknown_table.episode_count = 0;
        unknown_table.provider = "tvmaze".to_string();
        assert!(!fix_candidate_is_movie(&unknown_table));
        unknown_table.provider = "justwatch".to_string();
        assert!(fix_candidate_is_movie(&unknown_table));
        let movie_no_eps = fix_candidate("Dune", 2024, Kind::MOVIE, "cinemeta", 0);
        assert!(fix_candidate_is_movie(&movie_no_eps));
    }

    #[test]
    fn fix_match_no_candidate_falls_back_to_filename() {
        // Before a candidate is selected the filename query owns the scope.
        let path = fix_series_path();
        let movie_state = fix_headless_state(path.clone(), true);
        assert!(movie_state.effective_is_movie());
        assert!(movie_state.effective_choices().is_empty());
        let series_state = fix_headless_state(path, false);
        assert!(!series_state.effective_is_movie());
        assert_eq!(series_state.effective_choices().len(), 2);
    }

    #[test]
    fn fix_match_empty_search_keeps_the_movie_choice() {
        // The explicit TV choice owns the kind; an empty re-search must not
        // silently flip back to the filename classification (the retired
        // candidate-derived behaviour).
        let path = fix_movie_path();
        let mut st = fix_headless_state(path.clone(), true);
        st.choices = fix_match_scope_choices(&path, true);
        assert!(st.apply_pick_kind(false));
        assert!(!st.effective_is_movie());
        st.candidates.clear();
        st.searched = false;
        assert!(!st.sync_scope_to_selection(), "the TV table is stable");
        assert!(!st.effective_is_movie(), "the TV choice survives an empty search");
        let (scope, _) = st.selected_target().expect("series-kind target");
        assert!(scope != Scope::Movie);
    }

    #[test]
    fn fix_match_empty_search_keeps_the_tv_choice() {
        // The explicit Movie choice owns the kind; an empty re-search keeps it
        // and offers the single movie target again.
        let path = fix_series_path();
        let mut st = fix_headless_state(path.clone(), false);
        st.choices = fix_match_scope_choices(&path, false);
        assert!(st.apply_pick_kind(true));
        assert!(st.effective_is_movie());
        assert!(st.effective_choices().is_empty());
        st.candidates.clear();
        st.searched = false;
        assert!(!st.sync_scope_to_selection(), "the movie target is stable");
        assert!(st.effective_is_movie());
        let (scope, _) = st.selected_target().expect("movie target");
        assert_eq!(scope, Scope::Movie);
    }

    #[test]
    fn fix_match_series_pick_on_series_path_keeps_both_radios() {
        // A series pick for a series file keeps the Series-vs-Season radios
        // usable: the table is unchanged, so sync is a no-op and a same-kind
        // re-pick never resets the user choice.
        let path = fix_series_path();
        let mut st = fix_headless_state(path.clone(), false);
        st.choices = fix_match_scope_choices(&path, false);
        st.candidates = vec![fix_series_candidate()];
        assert!(!st.effective_is_movie());
        let choices = st.effective_choices();
        assert_eq!(choices.len(), 2, "a series pick offers Season + Series");
        assert!(choices.iter().any(|c| c.scope == Scope::Season));
        assert!(choices.iter().any(|c| c.scope == Scope::Series));
        assert!(
            !st.sync_scope_to_selection(),
            "a same-kind pick leaves the table untouched (user pick survives)"
        );
        let (scope, _) = st.selected_target().expect("series-kind target");
        assert!(scope != Scope::Movie);
        // A second series pick is still a no-op.
        st.candidates = vec![fix_candidate("Other Show", 2021, Kind::SERIES, "anilist", 5)];
        assert!(!st.effective_is_movie());
        assert!(!st.sync_scope_to_selection());
    }

    #[test]
    fn fix_match_kind_choice_rebuilds_the_scope_table() {
        // The explicit Movie/TV choice rebuilds the scope table both ways; the
        // selected candidate's kind no longer drives it.
        let series_path = fix_series_path();
        let mut st = fix_headless_state(series_path.clone(), true);
        st.choices = fix_match_scope_choices(&series_path, true);
        assert!(st.choices.is_empty());
        assert!(st.apply_pick_kind(false), "Movie -> TV refreshes the table");
        assert!(!st.is_movie);
        assert_eq!(st.choices.len(), 2);
        let (scope, _) = st.selected_target().expect("series-kind target");
        assert!(scope != Scope::Movie);
        // Back the other way: TV -> Movie resets to the single movie target.
        assert!(st.apply_pick_kind(true));
        assert!(st.is_movie);
        assert!(st.choices.is_empty());
        let (scope, _) = st.selected_target().expect("movie target");
        assert_eq!(scope, Scope::Movie);
    }

    #[test]
    fn fix_match_provisional_failure_saves_nothing() {
        crate::test_env::EnvGuard::with_scratch("fix-provisional-fail", |_dir| {
            // An empty key cannot save: the worker must never spawn on this,
            // and no half-applied correction may reach the store.
            let path = fix_series_path();
            let err = fix_match_save_provisional(&fix_series_candidate(), Scope::Series, "", &path)
                .expect_err("empty key must fail");
            assert!(
                !err.to_string().is_empty(),
                "the dialog reports the failure instead of spawning"
            );
            let store = metadata::overrides::load().expect("reload");
            assert!(
                store.resolve_for(&path).is_none(),
                "a failed provisional must leave no latch behind"
            );
        });
    }

    #[test]
    fn fix_match_latch_failure_keeps_the_provisional_with_clear_status() {
        crate::test_env::EnvGuard::with_scratch("fix-fetch-fail", |_dir| {
            let path = fix_series_path();
            let key = metadata::overrides::series_key_for(&path).unwrap();
            fix_match_save_provisional(&fix_series_candidate(), Scope::Series, &key, &path)
                .expect("provisional");
            // The full pin failed: the outcome keeps the provisional (never
            // rolls it back) and reports the pending copy -- never the
            // `Nothing was saved` error, which would lie.
            match fix_match_latch_failure(true, "Show") {
                FixOutcome::ProvisionalKept(status) => {
                    assert!(status.contains("Matched"), "clear status: {status}");
                    assert_ne!(status, FIX_MATCH_ERROR);
                    assert!(
                        !status.contains("Nothing was saved"),
                        "the latch WAS saved: {status}"
                    );
                }
                other => panic!("a post-provisional failure must keep it: {other:?}"),
            }
            // Without a provisional there is nothing to keep: the old error.
            assert!(matches!(
                fix_match_latch_failure(false, "Show"),
                FixOutcome::Failed
            ));
            // And the store still resolves the usable provisional.
            let store = metadata::overrides::load().expect("reload");
            let got = store.resolve_for(&path).expect("provisional kept");
            assert_eq!(got.title, "Show");
            assert!(got.episodes.is_empty());
        });
    }

    #[test]
    fn fix_match_put_replaces_through_the_correction_path() {
        crate::test_env::EnvGuard::with_scratch("fix-put-correction", |_dir| {
            // A conflicting movie latch: a plain `put` of the series record
            // would leave it in place (movie-first precedence keeps winning).
            let path = fix_series_path();
            let movie_key = metadata::overrides::movie_key(&path);
            let mut store = metadata::overrides::load()
                .unwrap_or_else(|_| metadata::overrides::Store::empty());
            store
                .put(Override {
                    scope: Scope::Movie,
                    key: movie_key,
                    title: "Wrong Movie".to_string(),
                    year: 2020,
                    provider: String::new(),
                    provider_id: String::new(),
                    poster: String::new(),
                    media: None,
                    episodes: Vec::new(),
                    created_at_ms: 0,
                })
                .expect("seed movie latch");
            // The full-record save must go through `put_correction`, so the
            // conflicting latch is gone and the series resolves.
            let key = metadata::overrides::series_key_for(&path).unwrap();
            fix_match_put(&path, &fix_full_series_override(&key)).expect("full save");
            let store = metadata::overrides::load().expect("reload");
            assert!(store.lookup_movie(&path).is_none());
            let got = store.resolve_for(&path).expect("series latch");
            assert_eq!(got.episodes.len(), 2);
        });
    }

    #[test]
    fn fix_match_provisional_kept_outcome_stays_open_with_clear_status() {
        // No store touch, but the shared serial keeps a mid-flight scratch
        // repoint from landing under this lane.
        let _env = crate::test_env::EnvGuard::hold();
        let mut st = fix_headless_state(fix_series_path(), false);
        st.busy = true;
        st.generation = 7;
        let line = fix_match_episodes_pending_status("Show");
        *st.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((7, FixOutcome::ProvisionalKept(line.clone())));
        st.on_job_done();
        assert!(!st.busy, "the fetch is over");
        assert_eq!(st.status.as_deref(), Some(line.as_str()));
        assert!(
            !st.status_error,
            "the pending copy is informational, not the red error"
        );
    }

    #[test]
    fn fix_match_provisional_save_failure_blocks_the_worker() {
        // Break the store without touching the real file: point the cache
        // root at a regular file, so the atomic save's `create_dir_all`
        // fails and the provisional cannot save. The guard restores the root
        // on drop.
        let mut env = crate::test_env::EnvGuard::scratch("fix-latch-blocked");
        let blocker = env.dir().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        env.repoint(&blocker);
        let path = fix_movie_path();
        let mut st = fix_headless_state(path.clone(), true);
        st.candidates = vec![fix_movie_candidate()];
        st.start_latch();
        drop(env);
        assert!(
            st.status_error,
            "the dialog reports the save failure instead of spawning"
        );
        assert!(!st.busy, "no worker was spawned");
        assert_eq!(st.generation, 0, "no job was handed to a worker thread");
    }

    #[test]
    fn fix_match_sub_line_strips_player_suffix() {
        let sub = fix_match_sub_line("Dune.Part.Two.2024.1080p.mkv - PotPlayer");
        assert!(
            !sub.contains(" - PotPlayer"),
            "player suffix must not leak: {sub}"
        );
        assert_eq!(sub, "Matching \"Dune.Part.Two.2024.1080p.mkv\"");
        assert!(
            !sub.contains("Nothing matched"),
            "the sub line names the file, never a verdict: {sub}"
        );
        assert_eq!(
            fix_match_sub_line("Dune.Part.Two.2024.1080p.mkv"),
            "Matching \"Dune.Part.Two.2024.1080p.mkv\"",
            "a title without the suffix passes through quoted"
        );
    }

    #[test]
    fn fix_match_matched_sub_line_carries_provider_and_episodes() {
        let over = Override {
            scope: Scope::Series,
            key: format!("{}:{}", 'd', r"\anime\attack on titan"),
            title: "Attack on Titan".to_string(),
            year: 2013,
            provider: "anilist".to_string(),
            provider_id: "p1".to_string(),
            poster: String::new(),
            media: None,
            episodes: (1..=25).map(|n| fix_pin(2, n)).collect(),
            created_at_ms: 0,
        };
        assert_eq!(
            fix_match_matched_sub_line(&over),
            "Matched by you \u{b7} AniList \u{2014} Attack on Titan, all 25 episodes"
        );
        let text = Override {
            provider: String::new(),
            provider_id: String::new(),
            episodes: Vec::new(),
            scope: Scope::Series,
            key: format!("{}:{}", 'd', r"\anime\attack on titan"),
            title: "My Title".to_string(),
            year: 0,
            poster: String::new(),
            media: None,
            created_at_ms: 0,
        };
        assert_eq!(
            fix_match_matched_sub_line(&text),
            "Matched by you \u{2014} My Title"
        );
        let movie = Override {
            scope: Scope::Movie,
            key: format!("{}:{}", 'd', r"\movies\dune.2021.mkv"),
            title: "Dune".to_string(),
            year: 2021,
            provider: "cinemeta".to_string(),
            provider_id: "p1".to_string(),
            poster: String::new(),
            media: None,
            episodes: (1..=3).map(|n| fix_pin(0, n)).collect(),
            created_at_ms: 0,
        };
        assert_eq!(
            fix_match_matched_sub_line(&movie),
            "Matched by you \u{b7} Cinemeta \u{2014} Dune"
        );
    }

    #[test]
    fn fix_match_text_override_latches_text_without_provider() {
        let path = format!("{}:{}", 'D', r"\Anime\Attack on Titan\Season 2\S02E01.mkv");
        let key = metadata::overrides::season_key_for(&path).unwrap();
        let over = fix_match_text_override("  Attack on Titan  ", 2013, Scope::Season, &key);
        assert_eq!(over.title, "Attack on Titan");
        assert_eq!(over.year, 2013);
        assert_eq!(over.scope, Scope::Season);
        assert_eq!(over.key, key);
        assert!(over.provider.is_empty());
        assert!(over.provider_id.is_empty());
        assert!(over.poster.is_empty());
        assert!(over.media.is_none());
        assert!(over.episodes.is_empty());
        let show_key = metadata::overrides::series_key_for(&path).unwrap();
        let show = fix_match_text_override("Attack on Titan", 0, Scope::Series, &show_key);
        assert_eq!(show.scope, Scope::Series);
        assert_eq!(show.key, show_key);
        assert!(show.provider.is_empty());
        let neg = fix_match_text_override("X", -1, Scope::Series, &show_key);
        assert_eq!(neg.year, 0);
    }

    #[test]
    fn fix_match_close_posts_no_thread_quit() {
        // Every dialog close -- Cancel, the titlebar X (WM_CLOSE), Esc, Match,
        // Refresh metadata, Use-automatic: they all funnel through
        // `DestroyWindow`, which is the `dialog_exists == false` row here --
        // must exit ONLY the modal loop and post NO thread quit for the app's
        // main loop (`tray::message_loop`).
        for dialog_exists in [true, false] {
            let (exit, repost) = fix_match_pump_step(dialog_exists, false, false);
            assert_eq!(exit, !dialog_exists, "dialog_exists={dialog_exists}");
            assert!(!repost, "a dialog close must never repost a quit");
        }
        // A `GetMessageW` error still ends the loop, quietly.
        assert_eq!(fix_match_pump_step(true, false, true), (true, false));
        assert_eq!(fix_match_pump_step(false, false, true), (true, false));
    }

    #[test]
    fn fix_match_foreign_quit_is_reposted_for_app_exit() {
        // A thread quit already in the queue (`GetMessageW` returning 0) is
        // never the dialog's: the loop exits but hands the quit back, so the
        // app's own Exit path still ends the process.
        assert_eq!(fix_match_pump_step(true, true, false), (true, true));
        assert_eq!(fix_match_pump_step(false, true, false), (true, true));
        assert_eq!(fix_match_pump_step(true, true, true), (true, true));
        // A live dialog with a real message keeps pumping (all close routes
        // exit via the dialog-gone row above, never here).
        assert_eq!(fix_match_pump_step(true, false, false), (false, false));
    }

    #[test]
    fn fix_match_row_lines_split_title_and_provider() {
        let series = fix_candidate("Attack on Titan", 2013, Kind::SERIES, "anilist", 25);
        let (title, sub) = fix_match_row_lines(&series);
        assert_eq!(title, "Attack on Titan (2013)");
        assert!(sub.contains("AniList"), "provider: {sub}");
        assert!(sub.contains("Anime TV"), "type: {sub}");
        assert!(sub.contains("25 episodes"), "episodes: {sub}");
        let movie = fix_candidate("Dune", 2021, Kind::MOVIE, "cinemeta", 0);
        let (mtitle, msub) = fix_match_row_lines(&movie);
        assert_eq!(mtitle, "Dune (2021)");
        assert!(
            msub.contains("Cinemeta") && msub.contains("Regular movie"),
            "{msub}"
        );
        assert!(
            !msub.contains("episode"),
            "a movie never prints episodes: {msub}"
        );
        let bare = fix_candidate("Show", 0, Kind::SERIES, "", 0);
        let (btitle, bsub) = fix_match_row_lines(&bare);
        assert_eq!(btitle, "Show");
        assert_eq!(bsub, "Regular TV");
    }

    #[test]
    fn fix_match_button_styles_match_the_brief() {
        assert_eq!(fix_match_button_style(FM_ID_MATCH), BTN_ACCENT);
        assert_eq!(fix_match_button_style(FM_ID_SEARCH), BTN_OUTLINE);
        assert_eq!(fix_match_button_style(FM_ID_REFRESH), BTN_OUTLINE);
        assert_eq!(fix_match_button_style(FM_ID_CHANGE), BTN_OUTLINE);
        assert_eq!(fix_match_button_style(FM_ID_CANCEL), BTN_SUBTLE);
        assert_eq!(fix_match_button_style(FM_ID_USE_TEXT), BTN_TEXT);
        assert_eq!(fix_match_button_style(FM_ID_AUTO), BTN_TEXT);
        // Match is the single accent primary.
        let accents = [
            FM_ID_SEARCH,
            FM_ID_MATCH,
            FM_ID_CANCEL,
            FM_ID_USE_TEXT,
            FM_ID_REFRESH,
            FM_ID_CHANGE,
            FM_ID_AUTO,
        ]
        .iter()
        .filter(|id| fix_match_button_style(**id) == BTN_ACCENT)
        .count();
        assert_eq!(accents, 1);
    }

    #[test]
    fn fix_match_static_colors_follow_the_brief() {
        assert_eq!(fix_match_static_color(FM_ID_TITLE, false), COL_TEXT);
        assert_eq!(fix_match_static_color(FM_ID_APPLIED, false), COL_TEXT);
        for id in [
            FM_ID_SUB,
            FM_ID_NAME_LABEL,
            FM_ID_KIND_LABEL,
            FM_ID_SCOPE_LABEL,
            FM_ID_NOTE,
            FM_ID_STATUS,
            FM_ID_EMPTY,
            FM_ID_SEARCHING,
        ] {
            assert_eq!(
                fix_match_static_color(id, false),
                COL_MUTED,
                "id {id} must be muted"
            );
        }
        assert_eq!(
            fix_match_static_color(FM_ID_STATUS, true),
            COL_DANGER,
            "the error status uses danger"
        );
    }

    #[test]
    fn fix_match_action_rules() {
        assert!(fix_match_search_allowed(false, false));
        assert!(
            !fix_match_search_allowed(true, false),
            "empty name disables Search"
        );
        assert!(
            !fix_match_search_allowed(false, true),
            "busy disables Search"
        );
        assert!(fix_match_match_allowed(true, true, false));
        assert!(
            !fix_match_match_allowed(false, true, false),
            "no selection disables Match"
        );
        assert!(
            !fix_match_match_allowed(true, false, false),
            "no target disables Match"
        );
        assert!(
            !fix_match_match_allowed(true, true, true),
            "busy disables Match"
        );
        assert!(fix_match_use_text_allowed(true, false));
        assert!(
            !fix_match_use_text_allowed(false, false),
            "secondary only when shown"
        );
        assert!(
            !fix_match_use_text_allowed(true, true),
            "busy disables secondary"
        );
    }

    #[test]
    fn fix_match_layout_carries_rule_empty_progress() {
        for dpi in [96, 144] {
            for radios in [0usize, 2usize] {
                let l = fix_match_layout(dpi, FixScreen::Search, radios);
                for (name, rc) in [
                    ("rule", l.rule),
                    ("empty", l.empty),
                    ("progress", l.progress),
                    ("list", l.list),
                    ("status", l.status),
                ] {
                    assert!(
                        rc.right > rc.left && rc.bottom > rc.top,
                        "dpi {dpi} radios {radios}: {name} non-positive {rc:?}"
                    );
                    assert!(
                        rc.left >= 0
                            && rc.top >= 0
                            && rc.right <= l.client_w
                            && rc.bottom <= l.client_h,
                        "dpi {dpi} radios {radios}: {name} escapes {rc:?} client {}x{}",
                        l.client_w,
                        l.client_h
                    );
                }
                // The buttons sit inside the client area, below the status.
                for (id, rc) in &l.buttons {
                    assert!(
                        rc.bottom <= l.client_h && rc.top >= l.status.bottom,
                        "dpi {dpi} radios {radios}: button {id} escapes"
                    );
                }
            }
            // Series (radios): the full 588 stack.
            let series = fix_match_layout(dpi, FixScreen::Search, 2);
            assert_eq!(series.client_h, scale_px(dpi, 588));
            assert_eq!(series.status.top, scale_px(dpi, 502));
            assert_eq!(series.buttons[1].1.top, scale_px(dpi, 532));
            assert_eq!(series.radios.len(), 2);
            // Movie (no radios): the collapsed 496 stack, no dead strip.
            let movie = fix_match_layout(dpi, FixScreen::Search, 0);
            assert_eq!(movie.client_h, scale_px(dpi, 496));
            assert_eq!(movie.status.top, scale_px(dpi, 410));
            assert_eq!(movie.buttons[1].1.top, scale_px(dpi, 440));
            assert!(movie.radios.is_empty());
            // The field frame wraps the one query edit with a 2px border; the
            // kind row occupies the retired Year band.
            let (frame, edit) = (movie.name_frame, movie.name_edit);
            {
                let pad = scale_px(dpi, 2);
                assert_eq!(frame.left, edit.left - pad, "dpi {dpi}: frame left");
                assert_eq!(frame.top, edit.top - pad, "dpi {dpi}: frame top");
                assert_eq!(frame.right, edit.right + pad, "dpi {dpi}: frame right");
                assert_eq!(frame.bottom, edit.bottom + pad, "dpi {dpi}: frame bottom");
                assert!(
                    frame.left >= 0
                        && frame.top >= 0
                        && frame.right <= movie.client_w
                        && frame.bottom <= movie.client_h,
                    "dpi {dpi}: frame escapes"
                );
            }
            assert!(
                movie.status.bottom <= movie.buttons[1].1.top,
                "dpi {dpi}: movie status above buttons"
            );
            let l = series;
            // The rule sits under the header; the strip sits above the list;
            // the empty panel covers the list area so it is never blank.
            assert!(
                l.rule.bottom <= l.name_label.top,
                "dpi {dpi}: rule under header"
            );
            assert_eq!(
                l.progress.bottom - l.progress.top,
                scale_px(dpi, FIX_MATCH_PROGRESS_H_L),
                "dpi {dpi}: 4px strip"
            );
            assert!(
                l.progress.bottom <= l.list.top,
                "dpi {dpi}: strip above list"
            );
            assert_eq!(
                (l.empty.left, l.empty.top, l.empty.right, l.empty.bottom),
                (l.list.left, l.list.top, l.list.right, l.list.bottom),
                "dpi {dpi}: empty covers list"
            );
            // The 56px rows fit the list: at least three full rows visible.
            assert!(
                (l.list.bottom - l.list.top) >= 3 * scale_px(dpi, FIX_MATCH_ROW_H_L),
                "dpi {dpi}: list fits 56px rows"
            );
        }
        let m = fix_match_layout(96, FixScreen::Matched, 0);
        assert!(m.rule.bottom > m.rule.top, "matched screen keeps the rule");
    }

    #[test]
    fn card_preview_fits_all_six_rows() {
        // Approved narrow: no Discord preview surfaces (results panel owns the
        // list); the hero card + search/scope/panel fit every DPI, rows live in
        // the hero. Wide keeps the legacy six-row preview.
        for dpi in [96, 120, 144, 192] {
            let l = compute_card_layout(
                scale_px(dpi, MAIN_CLIENT_W),
                scale_px(dpi, MAIN_CLIENT_H),
                dpi,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
            );
            if l.wide {
                assert!(
                    l.preview.bottom > l.preview.top,
                    "dpi {dpi}: empty preview"
                );
                assert_eq!(l.rows.len(), 3, "dpi {dpi}: media rows");
            } else {
                // Narrow: hero owns the rows, preview empty by design.
                assert_eq!(l.rows.len(), 2, "dpi {dpi}: hero rows");
                for r in &l.rows {
                    assert!(
                        r.left >= l.object.left
                            && r.right <= l.object.right
                            && r.top >= l.object.top
                            && r.bottom <= l.object.bottom,
                        "dpi {dpi}: hero row outside the hero: {r:?}"
                    );
                }
                assert!(
                    l.title.bottom <= l.meta.top,
                    "dpi {dpi}: hero rows overlap"
                );
                assert!(
                    l.art.left >= l.object.left
                        && l.art.right <= l.object.right
                        && l.art.top >= l.object.top
                        && l.art.bottom <= l.object.bottom,
                    "dpi {dpi}: poster outside the hero"
                );
                // Top bar, hero, search, scope, panel, status in order.
                assert!(
                    l.object.bottom <= l.picker_bar.top
                        && l.picker_bar.bottom <= l.stage.top
                        && l.obj_title.bottom <= l.obj_status.top,
                    "dpi {dpi}: topbar/hero/search/panel out of order"
                );
                continue;
            }
            let open = compute_card_layout_with(
                scale_px(dpi, MAIN_CLIENT_W),
                scale_px(dpi, MAIN_CLIENT_H),
                dpi,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
                true,
                false,
            );
            if open.wide {
                let state_top = open
                    .card_btn(card_state_button(0))
                    .expect("dpi {dpi}: state row missing")
                    .top;
                assert!(
                    open.preview.top >= state_top || open.compact,
                    "dpi {dpi}: the preview overlaps the open disclosure"
                );
            } else {
                // Narrow has no disclosure: open == rest, no state row on Card.
                assert!(open.card_btn(card_state_button(0)).is_none());
            }
        }
    }

    /// Approved narrow (460x680) and wide (1180x700+) keep controls inside;
    /// tiny 270px heights keep flags + horizontal fit, vertical may scroll.
    #[test]
    fn card_compact_and_wide_fit_inside_the_client() {
        for (cw, ch) in [(480, 270), (1440, 900), (360, 270)] {
            let l = compute_card_layout(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
            );
            assert_eq!(l.compact, ch < 480, "{cw}x{ch}: compact flag");
            // Approved wide threshold: 1180x700 (the split workspace).
            assert_eq!(l.wide, cw >= 1180 && ch >= 700, "{cw}x{ch}: wide flag");
            for b in &l.btns {
                if b.rc.right <= b.rc.left || b.rc.bottom <= b.rc.top {
                    continue;
                }
                assert!(
                    b.rc.left >= 0 && b.rc.right <= cw,
                    "{cw}x{ch}: button {} escapes horizontally: {:?}",
                    b.id,
                    b.rc
                );
                // Pinned narrow bands must fit at the approved 680px height;
                // tiny 270px viewports scroll the results panel instead.
                if ch >= 680 || l.wide {
                    assert!(
                        b.rc.top >= 0 && b.rc.bottom <= ch,
                        "{cw}x{ch}: button {} outside the client: {:?}",
                        b.id,
                        b.rc
                    );
                }
            }
            for r in [
                l.preview,
                l.art,
                l.title,
                l.meta,
                l.synopsis,
                l.tooltip,
                l.status,
            ] {
                if r.right <= r.left || r.bottom <= r.top {
                    continue;
                }
                assert!(
                    r.left >= 0 && r.right <= cw,
                    "{cw}x{ch}: rect escapes horizontally: {r:?}"
                );
                if ch >= 680 || l.wide {
                    assert!(
                        r.top >= 0 && r.bottom <= ch,
                        "{cw}x{ch}: rect outside the client: {r:?}"
                    );
                }
            }
            for r in &l.rows {
                // Narrow hero rows live inside the hero card, not the preview.
                let inside_hero = r.left >= l.object.left
                    && r.right <= l.object.right
                    && r.top >= l.object.top
                    && r.bottom <= l.object.bottom;
                let inside_preview = (r.left == 0
                    && r.top == 0
                    && r.right == 0
                    && r.bottom == 0)
                    || (r.left >= l.preview.left
                        && r.right <= l.preview.right
                        && r.top >= l.preview.top
                        && r.bottom <= l.preview.bottom);
                assert!(
                    inside_hero || inside_preview,
                    "{cw}x{ch}: media row outside hero/preview: {r:?}"
                );
            }
            for (i, a) in l.btns.iter().enumerate() {
                for b in l.btns.iter().skip(i + 1) {
                    if a.rc.right <= a.rc.left
                        || a.rc.bottom <= a.rc.top
                        || b.rc.right <= b.rc.left
                        || b.rc.bottom <= b.rc.top
                    {
                        continue;
                    }
                    assert!(
                        !rects_intersect(a.rc, b.rc),
                        "{cw}x{ch}: buttons {} {:?} and {} {:?} overlap",
                        a.id,
                        a.rc,
                        b.id,
                        b.rc
                    );
                }
            }
            if ch < 480 {
                // Approved narrow keeps the pinned chrome even in compact:
                // 48px top bar + 24px status bar.
                assert_eq!(l.header.bottom - l.header.top, scale_px(96, TOPBAR_H_L), "{cw}x{ch}: pinned top bar");
                assert_eq!(l.status.bottom - l.status.top, scale_px(96, STATUS_BAR_H_L), "{cw}x{ch}: pinned status");
            }
        }
    }

    #[test]
    fn card_tabs_and_bottom_buttons_share_one_anatomy() {
        // Approved narrow: gear opens Settings (text), hero Fix is an outline
        // ghost (only while playing); hidden Card/Back keep their styles.
        let card = compute_card_layout_state(
            MAIN_CLIENT_W,
            MAIN_CLIENT_H,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
            &CardLayoutState {
                advanced: false,
                has_selection: true,
                can_apply: false,
                playing: true,
                filter: crate::search::KindFilter::All,
            },
        );
        let style_of = |id: i32| {
            card.btns
                .iter()
                .find(|b| b.id == id)
                .unwrap_or_else(|| panic!("card button {id} missing"))
                .style
        };
        assert_eq!(style_of(BTN_CARD_BACK), BTN_OUTLINE);
        assert_eq!(
            style_of(BTN_FIX_MATCH),
            BTN_OUTLINE,
            "hero Fix is an outline ghost, not a text row"
        );
        assert!(
            card.btns.iter().all(|b| b.id != BTN_ADVANCED),
            "narrow Card carries no Change-filters control"
        );
        assert_eq!(style_of(BTN_SETTINGS), BTN_TEXT);
        let settings = compute_settings_layout(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, false, "turbo", "amoled");
        let sstyle_of = |id: i32| {
            settings
                .btns
                .iter()
                .find(|b| b.id == id)
                .unwrap_or_else(|| panic!("settings button {id} missing"))
                .style
        };
        assert_eq!(sstyle_of(BTN_CLEAR_CACHE), BTN_DANGER);
        assert_eq!(sstyle_of(BTN_CARD), BTN_TEXT);
    }

    /// One pixel of a top-down 32bpp BGRA buffer as a Windows `COLORREF`
    /// (`0x00BBGGRR`), the byte order `brand`'s palette constants use.
    fn pixel_at(px: &[u8], w: i32, x: i32, y: i32) -> u32 {
        let i = ((y as usize) * (w as usize) + (x as usize)) * 4;
        u32::from(px[i]) << 16 | u32::from(px[i + 1]) << 8 | u32::from(px[i + 2])
    }

    /// A deterministic 24bpp BMP fixture for the real paint-path test. It is a
    /// solid non-background blue, not a placeholder glyph: the test proves the
    /// decoder drew the supplied bytes, not that the app invented a tile.
    fn solid_bmp(width: i32, height: i32, b: u8, g: u8, r: u8) -> Vec<u8> {
        let stride = (width * 3 + 3) & !3;
        let pixel_bytes = (stride * height) as usize;
        let file_size = 54 + pixel_bytes;
        let mut out = Vec::with_capacity(file_size);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(file_size as u32).to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
        out.extend_from_slice(&2835u32.to_le_bytes());
        out.extend_from_slice(&2835u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for _ in 0..height {
            for _ in 0..width {
                out.extend_from_slice(&[b, g, r]);
            }
            out.extend(std::iter::repeat_n(0, (stride - width * 3) as usize));
        }
        out
    }

    /// Paint the current view through the production paint path into a 32bpp
    /// top-down DIB and return its BGRA pixels. No window, no global `UI` lock:
    /// the caller passes the state, so the render is exactly `paint_view`.
    fn paint_offscreen(w: i32, h: i32, dpi: i32, st: &UiState) -> Vec<u8> {
        use windows_sys::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
            SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        // SAFETY: DC/bitmap created and released within this call; the pixels are
        // copied out before the bitmap is deleted.
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            ReleaseDC(std::ptr::null_mut(), screen);
            assert!(!mem.is_null(), "CreateCompatibleDC failed");
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = w;
            info.bmiHeader.biHeight = -h; // top-down: row 0 is the top.
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(
                mem,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            assert!(!bmp.is_null() && !bits.is_null(), "CreateDIBSection failed");
            let old = SelectObject(mem, bmp);
            with_antialias(mem, || {
                paint_view(
                    mem,
                    Rect {
                        left: 0,
                        top: 0,
                        right: w,
                        bottom: h,
                    },
                    dpi,
                    st,
                )
            });
            Gdi::GdiFlush();
            let px = std::slice::from_raw_parts(
                bits as *const u8,
                (w as usize) * (h as usize) * 4,
            )
            .to_vec();
            SelectObject(mem, old);
            DeleteObject(bmp);
            DeleteDC(mem);
            px
        }
    }

    /// Owner regression (2026-10-28, tightened 2026-10-29): the preview's media
    /// image is clean -- no Potflix `P` mark, no FILM/SERIES badge, no status
    /// glyph over the art -- and absent artwork collapses instead of painting a
    /// blank tile that the wire calls a poster.
    #[test]
    fn preview_media_tile_carries_no_icon_or_badge_ink() {
        let run = Action {
            running: true,
            has_start: true,
            start: 1_700_000_000_000,
            ct: 1000,
            ..Action::default()
        };
        for kind in [CardKind::Movie, CardKind::Series] {
            let (raw, res) = match kind {
                CardKind::Movie => (
                    "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
                    Resolved {
                        series: "Dune: Part Two".to_string(),
                        year: 2024,
                        is_movie: true,
                        genres: "Sci-Fi, Adventure".to_string(),
                        length: "2h 46m".to_string(),
                        ..Resolved::default()
                    },
                ),
                CardKind::Series => (
                    "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer".to_string(),
                    Resolved {
                        series: "Breaking Bad".to_string(),
                        episode_name: "Problem Dog".to_string(),
                        season: 4,
                        number: 7,
                        ..Resolved::default()
                    },
                ),
            };
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.record_preview(kind, raw, Some(run.clone()), res);
            let dpi = 96;
            let (cw, ch) = (MAIN_CLIENT_W, MAIN_CLIENT_H);
            let l = compute_card_layout(
                cw,
                ch,
                dpi,
                &st.card_specs_state,
                st.displayed_card_kind(),
                st.preview_surface,
            );
            let (model, _) = card_preview_model(&st);
            assert!(
                !model.art && model.art_url.is_empty(),
                "{kind:?}: absent artwork must collapse, not claim a poster"
            );
            let px = paint_offscreen(cw, ch, dpi, &st);
            // Approved narrow hero always reserves the 64x96 poster slot (clean,
            // no badge); the model still collapses (no poster URL claimed).
            if l.wide {
                let (mx, my) = (
                    (l.art.left + l.art.right) / 2,
                    (l.art.top + l.art.bottom) / 2,
                );
                assert_ne!(
                    pixel_at(&px, cw, mx, my),
                    COL_SURFACE,
                    "{kind:?}: a blank placeholder tile was painted for absent artwork"
                );
            } else {
                // Narrow: poster slot present by design, no badge ink (checked
                // by the badge-free paint path, not by collapse).
                assert!(
                    l.art.right - l.art.left == scale_px(96, HERO_POSTER_W_L),
                    "hero poster slot"
                );
            }
        }
    }

    /// Owner direction 2026-10-29 (WORKSPACE-PHASE.md): one title query field
    /// carrying an optional year in the same field. The parse is the input
    /// contract the picker's provider search receives; a trailing 19xx/20xx
    /// token is the year, a bare year is a year-only query, everything else is
    /// the title. Unicode titles pass through unchanged.
    #[test]
    fn unified_query_parses_title_and_optional_year() {
        for (input, title, year) in [
            ("Dune", "Dune", 0),
            ("Dune 2021", "Dune", 2021),
            ("Dune: Part Two 2024", "Dune: Part Two", 2024),
            ("  Breaking   Bad   2008  ", "Breaking   Bad", 2008),
            (
                "Blade Runner 2049",
                "Blade Runner 2049",
                0,
            ),
            ("Dune 2028", "Dune", 2028),
            ("Dune 2029", "Dune 2029", 0),
            ("1984", "", 1984),
            ("Dune 20211", "Dune 20211", 0),
            ("Dune 1899", "Dune 1899", 0),
            ("", "", 0),
            ("Am\u{e9}lie 2001", "Am\u{e9}lie", 2001),
        ] {
            assert_eq!(
                parse_title_year(input),
                (title.to_string(), year),
                "parse {input:?}"
            );
        }
        assert!(search_query_allowed("Dune", false), "a title alone searches");
        assert!(
            search_query_allowed("1984", false),
            "a bare year is a valid query"
        );
        assert!(!search_query_allowed("   ", false), "blank never searches");
        assert!(
            !search_query_allowed("Dune", true),
            "a running job freezes Search"
        );
    }

    /// Owner visual-failure repair (2026-10-29): the unified title/year query
    /// and the explicit Movie/TV choice live in the main Card workspace's
    /// first viewport, not only inside the Fix match dialog. The same pure
    /// parse contract stays the input: a title alone, or a title with a
    /// trailing year.
    #[test]
    fn main_card_workspace_exposes_the_picker_in_its_first_viewport() {
        for (cw, ch) in [(1180, 700), (460, 680), (390, 844), (480, 270)] {
            let l = compute_card_layout(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Movie,
                PreviewSurface::default(),
            );
            for id in [BTN_PICKER_FIELD, BTN_PICKER_SEARCH] {
                let rc = l
                    .card_btn(id)
                    .unwrap_or_else(|| panic!("{cw}x{ch}: picker control {id} missing"));
                assert!(
                    rc.left >= 0 && rc.top >= 0 && rc.right <= cw && rc.bottom <= ch,
                    "{cw}x{ch}: picker control {id} escapes the client: {rc:?}"
                );
            }
            // Change filters survives only in the wide disclosure; the narrow
            // layout carries no such control.
            if cw >= 1180 && ch >= 700 {
                assert!(
                    l.card_btn(BTN_ADVANCED).is_some(),
                    "{cw}x{ch}: wide disclosure keeps Change filters"
                );
            } else {
                assert!(
                    l.card_btn(BTN_ADVANCED).is_none(),
                    "{cw}x{ch}: narrow layout carries no Change filters"
                );
            }
            assert!(
                l.picker_bar.right > l.picker_bar.left
                    && l.picker_bar.bottom > l.picker_bar.top,
                "{cw}x{ch}: the picker band is empty"
            );
            assert!(
                l.picker_bar.left >= 0
                    && l.picker_bar.top >= 0
                    && l.picker_bar.right <= cw
                    && l.picker_bar.bottom <= ch,
                "{cw}x{ch}: the picker band escapes the client: {:?}",
                l.picker_bar
            );
            // The field is one control, never a separate year input.
            assert_eq!(
                l.btns
                    .iter()
                    .filter(|b| b.id == BTN_PICKER_FIELD)
                    .count(),
                1,
                "{cw}x{ch}: exactly one query field"
            );
        }
        assert_eq!(
            parse_title_year("Dune 2021"),
            ("Dune".to_string(), 2021),
            "title + year stays one field"
        );
    }

    /// The picker worker publishes asynchronously; the real UI thread applies
    /// it from `WM_APP_PICKER_DONE`. Tests drive that same production
    /// completion entry point until the worker has published (bounded).
    fn drive_picker_job(st: &mut UiState) {
        for _ in 0..400 {
            st.on_picker_job_done();
            if !st.picker.busy {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the picker worker never published");
    }

    fn stub_search(
        calls: Arc<std::sync::Mutex<Vec<(String, i32, crate::search::KindFilter)>>>,
        rows: Result<Vec<metadata::Candidate>, String>,
    ) -> PickerSearch {
        Arc::new(
            move |title: &str,
                  year: i32,
                  filter: crate::search::KindFilter,
                  _only: &[String],
                  sink: &(dyn Fn(Vec<metadata::Candidate>) + Send + Sync)| {
                calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((title.to_string(), year, filter));
                let rows = rows.clone()?;
                // One batch, filtered by the scope's own class rule exactly as
                // the real streaming pass filters it.
                let rows: Vec<metadata::Candidate> = rows
                    .into_iter()
                    .filter(|cand| filter.includes_candidate(cand))
                    .collect();
                if !rows.is_empty() {
                    sink(rows.clone());
                }
                Ok(PickerReport {
                    rows,
                    failed: Vec::new(),
                })
            },
        )
    }

    fn office_series() -> metadata::Candidate {
        metadata::Candidate {
            kind: Kind::SERIES,
            title: "The Office".to_string(),
            year: 2005,
            provider: "tvmaze".to_string(),
            provider_id: "526".to_string(),
            poster: String::new(),
            synopsis: "A mockumentary at a paper company.".to_string(),
            genres: "Comedy".to_string(),
            length: "22 min".to_string(),
            episode_count: 188,
        }
    }

    fn anime_series() -> metadata::Candidate {
        metadata::Candidate {
            kind: Kind::SERIES,
            title: "Cyberpunk: Edgerunners".to_string(),
            year: 2022,
            provider: "anilist".to_string(),
            provider_id: "120377".to_string(),
            poster: String::new(),
            synopsis: "A street kid in Night City.".to_string(),
            genres: "Action . Sci-Fi".to_string(),
            length: "25 min".to_string(),
            episode_count: 10,
        }
    }

    fn dune_movie() -> metadata::Candidate {
        metadata::Candidate {
            kind: Kind::MOVIE,
            title: "Dune".to_string(),
            year: 2021,
            provider: "cinemeta".to_string(),
            provider_id: "tt1160419".to_string(),
            poster: String::new(),
            synopsis: "Paul Atreides on Arrakis.".to_string(),
            genres: "Sci-Fi . Adventure".to_string(),
            length: "2h 35m".to_string(),
            episode_count: 0,
        }
    }

    /// Blocker 1, red-capable: a cold launch with no player opens the SEARCH
    /// surface, not the library. The launch rule is the production one
    /// (`cold_open_view` / `open_cold_view_with`); with a player the shipped
    /// Main landing is unchanged. At rest the search surface carries exactly
    /// one field, one Search and no apply/repair control.
    /// Visual blocker (2026-10-29): at the 1180x700 workspace the Activity
    /// surface's title must be whole. The old paint wrapped
    /// `Watching Dune: Prophecy` at the display face inside a 40px band and cut
    /// the second line, which reads as a corrupt title rather than a name.
    /// Red when the title's ink reaches the band's clip edges.
    #[test]
    fn wide_activity_title_paints_whole_without_clipping() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.candidates = vec![metadata::Candidate {
            kind: Kind::SERIES,
            title: "Dune: Prophecy".to_string(),
            year: 2024,
            provider: "tvmaze".to_string(),
            provider_id: "tvmaze-dune-prophecy".to_string(),
            poster: String::new(),
            synopsis: "The Bene Gesserit origin.".to_string(),
            genres: "Sci-Fi . Drama".to_string(),
            length: "60 min".to_string(),
            episode_count: 6,
        }];
        assert!(st.select_candidate(0), "the row selects");
        let (title, _, is_movie) = st.preview_object().expect("a local preview object");
        assert_eq!((title.as_str(), is_movie), ("Dune: Prophecy", false));
        let px = paint_offscreen(1180, 700, 96, &st);
        let l = compute_card_layout_state(
            1180,
            700,
            96,
            &st.card_specs_state,
            st.displayed_card_kind(),
            st.preview_surface,
            &st.card_layout_state(),
        );
        let activity = l.surfaces[2];
        let band = activity_title_band(activity, 96, true);
        assert!(
            band.right - band.left > 60 && band.bottom - band.top > 10,
            "the Activity surface draws a title band: {band:?}"
        );
        // Ink is a pixel far brighter than the card fill the band sits on.
        let ink = |x: i32, y: i32| -> bool {
            let i = ((y as usize) * 1180 + x as usize) * 4;
            u32::from(px[i]) + u32::from(px[i + 1]) + u32::from(px[i + 2]) > 3 * 0x90
        };
        let mut top_row = band.bottom;
        let mut last_row = band.top - 1;
        let mut last_col = band.left - 1;
        for y in band.top..band.bottom {
            for x in band.left..band.right {
                if ink(x, y) {
                    top_row = top_row.min(y);
                    last_row = last_row.max(y);
                    last_col = last_col.max(x);
                }
            }
        }
        assert!(last_row >= band.top, "the title paints inside its band");
        assert!(
            last_row <= band.bottom - 3,
            "title ink is clipped at the band's bottom edge: last row {last_row}, band ends {}",
            band.bottom - 1
        );
        assert!(
            last_col <= band.right - 2,
            "title ink is clipped at the band's right edge: last col {last_col}, band ends {}",
            band.right - 1
        );
    }

    #[test]
    fn cold_open_without_a_player_shows_the_search_surface() {
        app_support::config::set_live_now_playing(None);
        assert_eq!(
            UiState::cold_open_view(false),
            View::Card,
            "nothing playing: first screen is the search surface"
        );
        assert_eq!(
            UiState::cold_open_view(true),
            View::Main,
            "a playing title keeps the shipped landing"
        );
        let mut st = UiState::default();
        st.open_cold_view_with(false);
        assert_eq!(st.cur_view, View::Card);
        assert!(!st.preview_is_live(), "no push has been recorded");
        assert!(st.preview_object().is_none(), "the empty state is explicit");
        assert!(
            st.object_status_text(false)
                .contains("Preview is local; Apply needs playback.")
        );
        assert_eq!(st.picker.filter, crate::search::KindFilter::All);
        for (cw, ch) in [(463, 703), (1180, 700)] {
            let l = compute_card_layout_state(
                cw,
                ch,
                96,
                &st.card_specs_state,
                st.displayed_card_kind(),
                st.preview_surface,
                &st.card_layout_state(),
            );
            assert!(
                l.card_btn(BTN_PICKER_FIELD).is_some() && l.card_btn(BTN_PICKER_SEARCH).is_some(),
                "{cw}x{ch}: the first screen must carry the one field and one Search"
            );
            assert_eq!(
                l.btns.iter().filter(|b| b.id == BTN_PICKER_SEARCH).count(),
                1,
                "{cw}x{ch}: exactly one search verb"
            );
            if cw < 1000 {
                // Approved narrow: pinned Apply at rest, no hero Fix with
                // nothing playing, and no scope row — the removed scope ids
                // (85/81/82/87/88) are absent.
                assert!(
                    l.card_btn(BTN_PICKER_APPLY).is_some()
                        && l.card_btn(BTN_FIX_MATCH).is_none(),
                    "{cw}x{ch}: pinned Apply at rest, no hero Fix with nothing playing"
                );
                for id in [85, 81, 82, 87, 88] {
                    assert!(
                        l.card_btn(id).is_none(),
                        "{cw}x{ch}: removed scope {id} laid out at rest"
                    );
                }
            } else {
                assert!(
                    l.btns
                        .iter()
                        .all(|b| b.id != BTN_PICKER_APPLY && b.id != BTN_FIX_MATCH),
                    "{cw}x{ch}: an apply/repair control is visible at rest"
                );
                // Universal search: no kind control anywhere at rest — the
                // removed ids are gone from the composition entirely.
                for id in [85, 81, 82, 87, 88] {
                    assert!(
                        l.btns.iter().all(|b| b.id != id),
                        "{cw}x{ch}: removed kind control {id} is visible at rest"
                    );
                }
            }
        }
        // And the same rule drives the real launch: with a player the window
        // opens on Main.
        let mut st = UiState::default();
        st.open_cold_view_with(true);
        assert_eq!(st.cur_view, View::Main);
    }

    /// The empty state's preview surfaces: no title means no synopsis body and
    /// no "No synopsis for this title." boilerplate, and every surface says
    /// `Nothing to preview` instead of painting a blank panel. A selected
    /// record is the opposite state and must not be confused with it.
    #[test]
    fn empty_preview_surfaces_say_nothing_to_preview() {
        let mut st = UiState::default();
        st.open_cold_view_with(false);
        let (model, have) = card_preview_model(&st);
        assert!(!have, "nothing recorded, nothing selected");
        assert!(
            preview_is_empty(have, &model),
            "the surfaces must paint `{PREVIEW_EMPTY_TEXT}` here"
        );
        assert!(model.header.is_empty(), "no title: no `Watching ...` line");
        assert!(
            model.synopsis.is_empty(),
            "no title: the synopsis body stays empty, so the boilerplate is not painted"
        );
        for surface in [PreviewSurface::Member, PreviewSurface::Profile, PreviewSurface::Activity] {
            assert!(
                surface_lines(&model, surface).is_empty(),
                "{surface:?}: the empty model draws no lines at all"
            );
        }

        // The neighbouring state, same window: a picked record is not empty and
        // carries its own header and synopsis.
        let cand = Candidate {
            kind: Kind::SERIES,
            title: "The Office".to_string(),
            year: 2005,
            provider: "tvmaze".to_string(),
            provider_id: "526".to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 9,
        };
        st.picker.candidates = vec![cand];
        st.picker.open = true;
        assert!(st.select_candidate(0));
        let (model, have) = card_preview_model(&st);
        assert!(have && !preview_is_empty(have, &model));
        assert_eq!(model.header, "Watching The Office");
        assert!(model.synopsis.is_empty(), "this fixture carries no synopsis");
    }

    /// The pick arming rule, end to end through the production transitions: an
    /// auto-highlight is not a choice, an explicit pick is, and the same record
    /// arriving in a NEW result set is a highlight again -- it cannot arm Apply
    /// without a fresh pick.
    #[test]
    fn a_pick_does_not_survive_a_query_edit_scope_change_or_new_search() {
        let cand = Candidate {
            kind: Kind::MOVIE,
            title: "Dune".to_string(),
            year: 2021,
            provider: "cinemeta".to_string(),
            provider_id: "tt1160419".to_string(),
            poster: String::new(),
            synopsis: "Paul Atreides on Arrakis.".to_string(),
            genres: "Sci-Fi".to_string(),
            length: "2h 35m".to_string(),
            episode_count: 0,
        };
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.candidates = vec![cand.clone()];
        st.picker.open = true;
        st.picker.selected = 0;
        assert!(!st.has_picked_candidate(), "the seeded highlight is not a pick");
        assert!(st.select_candidate(0), "the row selects");
        assert!(st.has_picked_candidate(), "an explicit pick arms Apply");

        // New query, same record back at index 0: still not a choice.
        st.picker.set_query("Dune 2021".to_string());
        assert!(!st.has_picked_candidate(), "the query edit dropped the pick");
        st.picker.candidates = vec![cand.clone()];
        st.picker.selected = 0;
        st.picker.open = true;
        assert!(
            !st.has_picked_candidate(),
            "an identical row in a fresh list is a highlight, not a pick"
        );
        // Same for a scope change and for a brand-new search.
        assert!(st.select_candidate(0));
        st.picker.set_filter(crate::search::KindFilter::Movies);
        st.picker.candidates = vec![cand.clone()];
        st.picker.selected = 0;
        st.picker.open = true;
        assert!(!st.has_picked_candidate(), "the scope change dropped the pick");
        assert!(st.select_candidate(0));
        let (generation, ..) = st.picker.begin_search().expect("search starts");
        assert!(st.picker.finish_search(generation, vec![cand]));
        st.picker.selected = 0;
        st.picker.open = true;
        assert!(!st.has_picked_candidate(), "the new search dropped the pick");
        // A closed list can never be armed, however it got there.
        assert!(st.select_candidate(0));
        st.picker.close_results();
        assert!(!st.has_picked_candidate(), "a closed list holds no pick on screen");
    }

    /// Blocker 1, red-capable, end to end through the production transitions:
    /// cold open with NO player -> type -> Search -> select a row -> the object
    /// card and the preview are composed from the SELECTED RESOLVED RECORD, and
    /// the surface says in words that nothing was sent.
    #[test]
    fn no_player_search_select_previews_locally_and_never_claims_live() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut st = UiState::default();
        st.open_cold_view_with(false);
        assert_eq!(st.cur_view, View::Card);
        st.picker_search = stub_search(calls.clone(), Ok(vec![dune_movie(), office_series()]));

        // Type into the one field. (The process-wide now-playing mirror is
        // shared with parallel tests, so the field's starting content is set
        // here explicitly; the cold-open rule itself is asserted above.)
        st.picker.set_query(String::new());
        st.focus_id = BTN_PICKER_FIELD;
        for ch in "The Office".chars() {
            assert!(st.on_card_char(ch as u32));
        }
        assert_eq!(st.picker.query, "The Office");
        // One Search, through the real card dispatch.
        assert_eq!(st.do_card_action(NULL, BTN_PICKER_SEARCH), None);
        drive_picker_job(&mut st);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("The Office".to_string(), 0, crate::search::KindFilter::All)],
            "one query, one search, All"
        );
        assert!(st.picker.open, "results are open");
        assert_eq!(st.picker.candidates.len(), 2);
        assert_eq!(st.picker.status, "2 matches for \"The Office\"");
        assert!(!st.preview_is_live());

        // Select the series row: the preview is that record, and it is a series.
        let series_idx = st
            .picker
            .candidates
            .iter()
            .position(|c| c.kind == Kind::SERIES)
            .expect("a series row");
        assert!(st.select_candidate(series_idx));
        let (title, year, is_movie) = st.preview_object().expect("a local preview object");
        assert_eq!((title.as_str(), year, is_movie), ("The Office", 2005, false));
        assert_eq!(st.displayed_card_kind(), CardKind::Series);
        let (act, have) = st.preview_activity();
        assert!(have, "the preview composes from the selected record");
        assert_eq!(act.name, "The Office");
        assert!(!st.preview_is_live(), "a local preview is never 'live'");
        let status = st.object_status_text(false);
        assert_eq!(
            status, LOCAL_PREVIEW_STATUS,
            "one true sentence, and it never claims playback is idle"
        );
        assert!(
            !status.contains("nothing playing"),
            "the copy must not claim nothing is playing beside a preview: {status:?}"
        );
        assert!(status.contains("not sent to Discord"), "{status:?}");

        // A movie row composes a movie: the record's kind owns the preview, so
        // a series can never inherit a stale Movie and vice versa.
        let movie_idx = st
            .picker
            .candidates
            .iter()
            .position(|c| c.kind == Kind::MOVIE)
            .expect("a movie row");
        assert!(st.select_candidate(movie_idx));
        assert!(st.preview_object().expect("object").2);
        assert_eq!(st.displayed_card_kind(), CardKind::Movie);
        // Selecting is local: the query is untouched and no latch is running.
        assert_eq!(st.picker.query, "The Office");
        assert!(!st.picker.busy);
    }

    /// Approved pinning: Apply is always laid out at the panel bottom (disabled
    /// until a row is picked); hero Fix is in the hero only while a title is
    /// playing. Honesty is the enabled style, not presence.
    #[test]
    fn apply_to_playing_title_exists_only_with_a_player_and_a_selection() {
        let base = CardLayoutState {
            advanced: false,
            has_selection: true,
            can_apply: false,
            playing: false,
            filter: crate::search::KindFilter::All,
        };
        for (cw, ch) in [(463, 703), (1180, 700)] {
            let disabled = compute_card_layout_state(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
                &base,
            );
            if cw < 1000 {
                // Narrow pinning: always laid out, disabled look.
                let _apply = disabled
                    .card_btn(BTN_PICKER_APPLY)
                    .unwrap_or_else(|| panic!("{cw}x{ch}: pinned Apply missing"));
                let style = disabled
                    .btns
                    .iter()
                    .find(|b| b.id == BTN_PICKER_APPLY)
                    .unwrap()
                    .style;
                assert_eq!(style, BTN_OUTLINE, "{cw}x{ch}: Apply must look disabled");
            } else {
                // Wide keeps the legacy guard: no Apply without a player.
                assert!(
                    disabled.card_btn(BTN_PICKER_APPLY).is_none(),
                    "{cw}x{ch}: wide Apply without a player would be a lie"
                );
            }
            let playing = compute_card_layout_state(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
                &CardLayoutState {
                    can_apply: true,
                    playing: true,
                    ..base
                },
            );
            let apply = playing
                .card_btn(BTN_PICKER_APPLY)
                .unwrap_or_else(|| panic!("{cw}x{ch}: apply missing with a player"));
            if cw < 1000 {
                let style = playing
                    .btns
                    .iter()
                    .find(|b| b.id == BTN_PICKER_APPLY)
                    .unwrap()
                    .style;
                assert_eq!(style, BTN_ACCENT, "{cw}x{ch}: Apply lights when it can apply");
                // Hero Fix never overlaps pinned Apply.
                let repair = playing
                    .card_btn(BTN_FIX_MATCH)
                    .unwrap_or_else(|| panic!("{cw}x{ch}: hero Fix missing"));
                assert!(!rects_intersect(apply, repair));
            } else {
                // Wide keeps the legacy list actions.
                let results = playing.picker_results;
                assert!(
                    apply.top >= results.top && apply.bottom <= results.bottom,
                    "{cw}x{ch}: apply must live inside the results list"
                );
            }
        }
        // At rest Apply stays pinned (disabled look); hero Fix needs a player.
        let rest = compute_card_layout_state(
            463,
            703,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
            &CardLayoutState {
                can_apply: true,
                has_selection: false,
                playing: true,
                ..base
            },
        );
        assert!(
            rest.card_btn(BTN_PICKER_APPLY).is_some() && rest.card_btn(BTN_FIX_MATCH).is_some()
        );
        // With nothing playing the hero ghost is absent, not disabled.
        let idle = compute_card_layout_state(
            463,
            703,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
            &base,
        );
        assert!(idle.card_btn(BTN_FIX_MATCH).is_none());
    }

    /// The field cap: exactly 128 scalar values, a visible counter, a refused
    /// 129th character that says why, and a paste that clips at the cap instead
    /// of silently producing a different title.
    #[test]
    fn picker_query_is_capped_with_a_visible_counter() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.focus_id = BTN_PICKER_FIELD;
        for _ in 0..crate::search::MAX_QUERY_CHARS {
            assert!(st.on_card_char('a' as u32));
        }
        assert_eq!(st.picker.query_len(), crate::search::MAX_QUERY_CHARS);
        assert_eq!(st.picker.counter_text(), "128/128");
        assert!(!st.on_card_char('b' as u32), "the 129th char is refused");
        assert_eq!(st.picker.query_len(), crate::search::MAX_QUERY_CHARS);
        assert_eq!(
            st.picker.status,
            crate::search::QueryIssue::TooLong.message(),
            "a refused character explains itself"
        );
        // A multi-byte scalar counts once, not per byte.
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.focus_id = BTN_PICKER_FIELD;
        st.picker.set_query("\u{1f600}".repeat(10));
        assert_eq!(st.picker.query_len(), 10);
        assert!(!st.picker.query.is_empty());
        // Paste clips at the cap and says so.
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.focus_id = BTN_PICKER_FIELD;
        assert!(st.paste_into_picker(&"x".repeat(300)));
        assert_eq!(st.picker.query_len(), crate::search::MAX_QUERY_CHARS);
        assert_eq!(st.picker.status, crate::search::QueryIssue::TooLong.message());
        // Paste also refuses control characters, and is inert when the field
        // does not hold focus.
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.focus_id = BTN_CARD_BACK;
        assert!(!st.paste_into_picker("Dune"));
        assert!(st.picker.query.is_empty());
    }

    /// Empty, year-only and provider-failure are three distinct, explicit
    /// states: none of them runs a search, none of them panics, and the failure
    /// keeps the query so a retry needs no retyping.
    #[test]
    fn empty_year_only_and_provider_failure_states_are_explicit() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker_search = Arc::new(
            |_: &str,
             _: i32,
             _: crate::search::KindFilter,
             _: &[String],
             _: &(dyn Fn(Vec<metadata::Candidate>) + Send + Sync)|
             -> Result<PickerReport, String> {
                panic!("a refused query must never reach the providers")
            },
        );
        let _ = st.do_card_action(NULL, BTN_PICKER_SEARCH);
        assert_eq!(
            st.picker.status,
            crate::search::QueryIssue::Empty.message(),
            "an empty submit explains itself"
        );
        st.picker.set_query("2024".to_string());
        let _ = st.do_card_action(NULL, BTN_PICKER_SEARCH);
        assert_eq!(st.picker.status, crate::search::QueryIssue::YearOnly.message());
        assert!(!st.picker.busy && !st.picker.open);

        // Provider failure: its own status, an empty (not open) list, and the
        // query preserved.
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker_search = stub_search(calls.clone(), Err("providers unreachable".to_string()));
        st.picker.set_query("The Office".to_string());
        assert!(st.card_picker_search());
        drive_picker_job(&mut st);
        assert_eq!(st.picker.status, FIX_MATCH_ERROR);
        assert!(!st.picker.busy && !st.picker.open && st.picker.candidates.is_empty());
        assert_eq!(st.picker.query, "The Office", "a failure keeps the query");
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "the failure came from a real (stubbed) provider call"
        );
        assert!(!st.preview_is_live() && st.preview_object().is_none());
    }

    /// The stream is visible: a batch that lands while the job is still running
    /// opens the list and reports the running count. The failure this pins: the
    /// old one-shot finish left the overlay blank until the last provider
    /// answered.
    #[test]
    fn streamed_search_shows_rows_before_the_job_finishes() {
        let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = go.clone();
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker_search = Arc::new(
            move |_title: &str,
                  _year: i32,
                  _filter: crate::search::KindFilter,
                  _only: &[String],
                  sink: &(dyn Fn(Vec<metadata::Candidate>) + Send + Sync)| {
                sink(vec![dune_movie()]);
                // Hold the rest of the stream until the test has seen batch one.
                for _ in 0..1000 {
                    if gate.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                sink(vec![office_series()]);
                Ok(PickerReport::default())
            },
        );
        st.picker.set_query("Dune".to_string());
        assert!(st.card_picker_search());
        let mut partial = None;
        for _ in 0..400 {
            st.on_picker_job_done();
            if st.picker.busy && st.picker.open && st.picker.candidates.len() == 1 {
                partial = Some(st.picker.status.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        go.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            partial.as_deref(),
            Some("1 match so far\u{2026}"),
            "the first batch is on screen while the job still runs"
        );
        drive_picker_job(&mut st);
        assert_eq!(st.picker.candidates.len(), 2);
        assert_eq!(st.picker.status, "2 matches for \"Dune\"");
        assert!(!st.picker.busy && st.picker.open);
    }

    /// A pick is a target while the list still streams: `card_picker_match`
    /// must pass the busy flag and stop only at the player check. The latch
    /// then advances the generation, so every later batch of that search is
    /// stale and cannot repopulate the list over it.
    #[test]
    fn a_pick_latches_mid_stream_and_late_batches_are_discarded() {
        let _env = crate::test_env::EnvGuard::scratch("picker-latch-window");
        app_support::config::set_live_now_playing(None);
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.set_query("Dune".to_string());
        let (generation, ..) = st.picker.begin_search().expect("search starts");
        st.picker_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((generation, FixOutcome::Search(vec![dune_movie()])));
        st.on_picker_job_done();
        assert!(st.picker.busy && st.picker.open, "a batch landed mid-stream");
        assert!(st.select_candidate(0));
        assert!(st.has_picked_candidate());
        // The busy flag alone must not refuse the pick.
        assert!(!st.card_picker_match());
        assert_eq!(
            st.picker.status, FIX_MATCH_IDLE_REASON,
            "the pick passed the busy guard and stopped at the player check"
        );
        // What a real latch does before saving: advance the generation and drop
        // the queue, so batch two and the stream end are both stale.
        st.picker.generation += 1;
        st.picker.busy = true;
        st.picker.open = false;
        st.picker_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        st.picker_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((generation, FixOutcome::Search(vec![office_series()])));
        st.picker_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((generation, FixOutcome::SearchDone));
        st.on_picker_job_done();
        assert_eq!(
            st.picker.candidates.len(),
            1,
            "no late batch repopulates the list over the latch"
        );
        assert!(st.picker.busy, "the latch still owns the busy flag");
        assert!(!st.picker.open, "the latch closed the list");
    }

    /// Universal search: no segment maps anywhere. The removed scope ids
    /// (85/81/82/87/88) are not actionable and change no filter, and every
    /// card search runs both chains (filter `All`) with no row filtered out
    /// by scope — anime and regular rows share one list.
    #[test]
    fn no_scope_segment_selects_a_filter_and_every_search_runs_both_chains() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        for id in [85, 81, 82, 87, 88] {
            let _ = st.do_card_action(NULL, id);
            assert_eq!(
                st.picker.filter,
                crate::search::KindFilter::All,
                "removed scope id {id} changed the filter"
            );
        }
        let anime_movie = metadata::Candidate {
            kind: Kind::MOVIE,
            title: "Akira".to_string(),
            year: 1988,
            provider: "kitsu".to_string(),
            provider_id: "1".to_string(),
            poster: String::new(),
            synopsis: "Neo-Tokyo, 2019.".to_string(),
            genres: "Sci-Fi".to_string(),
            length: "2h 4m".to_string(),
            episode_count: 0,
        };
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker_search = stub_search(
            calls.clone(),
            Ok(vec![
                anime_series(),
                anime_movie.clone(),
                dune_movie(),
                office_series(),
            ]),
        );
        st.picker.set_query("Mixed".to_string());
        assert!(st.card_picker_search());
        drive_picker_job(&mut st);
        assert_eq!(
            calls.lock().unwrap()[0].2,
            crate::search::KindFilter::All,
            "the worker always runs the universal scope"
        );
        assert_eq!(
            st.picker.candidates.len(),
            4,
            "both chains share one list: no row is filtered by scope"
        );
    }

    /// A season folder keeps the SEASON default even when the folder really
    /// holds episode siblings: a season latch is still a series latch, and
    /// latching the whole show would apply one season's record to every other
    /// season folder.
    #[test]
    fn a_season_folder_keeps_the_season_default_even_with_episode_siblings() {
        crate::test_env::EnvGuard::with_scratch("scope-default", |dir| {
            let season = dir.join("Show").join("Season 1");
            std::fs::create_dir_all(&season).unwrap();
            for n in 1..=3 {
                std::fs::write(season.join(format!("Show S01E0{n}.mkv")), "x").unwrap();
            }
            let path = season
                .join("Show S01E01.mkv")
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                crate::search::detect_shape(&path),
                crate::search::MediaShape::Series,
                "the folder really is a series"
            );
            let choices = fix_match_scope_choices(&path, false);
            assert_eq!(choices.len(), 2);
            assert_eq!(choices[0].scope, Scope::Season);
            assert!(
                choices[0].default,
                "the season stays the default target of a season folder"
            );
            assert!(!choices[1].default);
        });
    }

    /// The chain: the painted result row carries title, year and kind, and the
    /// mixed search's rows come back in the merge order the row index selects.
    #[test]
    fn mixed_result_rows_carry_title_year_and_kind() {
        let (title, sub) = fix_match_row_lines(&office_series());
        assert_eq!(title, "The Office (2005)");
        assert!(
            sub.contains("Regular TV") && sub.contains("188 episodes"),
            "{sub:?}"
        );
        let (title, sub) = fix_match_row_lines(&dune_movie());
        assert_eq!(title, "Dune (2021)");
        assert!(sub.contains("Regular movie"), "{sub:?}");
        assert!(!sub.contains("episodes"), "a movie has no episode table");
        // A year-only record renders without a fake year.
        let mut c = dune_movie();
        c.year = 0;
        assert_eq!(fix_match_row_lines(&c).0, "Dune");
    }

    /// The main-workspace query is a real editable field, not a painted label:
    /// typed characters and backspace drive the one canonical picker model.
    #[test]
    fn picker_field_types_and_backspaces_in_the_main_workspace() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.focus_id = BTN_PICKER_FIELD;
        for ch in "Dune 2021".chars() {
            assert!(st.on_card_char(ch as u32));
        }
        assert_eq!(st.picker.query, "Dune 2021");
        assert!(st.on_card_char(8));
        assert_eq!(st.picker.query, "Dune 202");
        st.focus_id = BTN_CARD_BACK;
        assert!(!st.on_card_char('x' as u32), "only the field takes text");
    }

    /// Owner visual-failure repair (2026-10-29): an absent poster URL must not
    /// reserve and paint a blank `poster` tile. The cover slot is present only
    /// when a real image can be drawn.
    #[test]
    fn absent_artwork_collapses_instead_of_a_blank_poster_tile() {
        let act = Activity {
            name: "Watching Dune: Part Two".to_string(),
            details: "Dune: Part Two (2024)".to_string(),
            state: "Sci-Fi, Adventure".to_string(),
            ..Activity::default()
        };
        let p = surface_preview(&act, true, "", 0, 0, true);
        assert!(
            !p.art,
            "an absent cover must collapse the art slot, not claim a poster"
        );
    }

    /// The search result overlay is a real paint path, not just a model: a
    /// candidate row must appear over the preview stage and its painted chip
    /// must differ from the canvas behind it.
    #[test]
    fn picker_overlay_paints_a_candidate_row_in_the_main_workspace() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.open = true;
        st.picker.searched = true;
        st.picker.candidates = vec![Candidate {
            kind: Kind::MOVIE,
            title: "Dune: Part Two".to_string(),
            year: 2024,
            provider: "cinemeta".to_string(),
            provider_id: "tt15239678".to_string(),
            poster: "https://example.com/dune.jpg".to_string(),
            synopsis: "Paul Atreides unites with the Fremen.".to_string(),
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            episode_count: 0,
        }];
        st.picker.selected = 0;
        let (cw, ch) = (1180, 700);
        let l = compute_card_layout(
            cw,
            ch,
            96,
            &st.card_specs_state,
            st.displayed_card_kind(),
            st.preview_surface,
        );
        let rows = picker_row_rects(&l, 1, 0, 96);
        assert_eq!(rows.len(), 1, "one candidate gets one visible row");
        let px = paint_offscreen(cw, ch, 96, &st);
        let r = rows[0];
        let (mx, my) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
        assert_ne!(
            pixel_at(&px, cw, mx, my),
            COL_BG,
            "the result overlay was not painted over the stage"
        );
    }

    /// With Advanced open the picker viewport holds one row at the default
    /// 462x712 client, while the status says "3 matches": every candidate must
    /// still resolve through the production hit test, scrolled into view.
    #[test]
    fn advanced_open_picker_list_keeps_three_rows_reachable_at_default_client() {
        let l = compute_card_layout_state(
            462,
            712,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
            &CardLayoutState {
                advanced: true,
                has_selection: false,
                can_apply: false,
                playing: false,
                filter: crate::search::KindFilter::All,
            },
        );
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.candidates = vec![office_series(), dune_movie(), anime_series()];
        st.picker.open = true;
        st.picker.searched = true;
        st.picker.selected = 0;
        assert_eq!(st.picker.candidates.len(), 3);
        // Every candidate resolves through the production hit test after it is
        // selected (selection scrolls it into the one-row viewport).
        for idx in 0..st.picker.candidates.len() {
            assert!(st.select_candidate(idx), "row {idx} selects");
            let x = (l.picker_results.left + l.picker_results.right) / 2;
            let mut found = false;
            let mut y = l.picker_results.top + 1;
            while y < l.picker_results.bottom {
                if picker_result_at(&l, &st.picker, x, y, 96) == Some(idx) {
                    found = true;
                    break;
                }
                y += 2;
            }
            assert!(found, "row {idx} is not reachable with Advanced open at 462x712");
        }
    }

    /// A finished live stream and its cache replay paint identical rows: the
    /// stored answer uses the same ranking the streamed batches merge through,
    /// so provider and order agree whatever leg answered first -- and the
    /// finish invalidates a late batch instead of reopening the list over the
    /// final count.
    #[test]
    fn live_stream_and_cache_replay_agree_and_finish_invalidates_late_batches() {
        fn frieren(provider: &str, id: &str, title: &str) -> metadata::Candidate {
            metadata::Candidate {
                kind: Kind::SERIES,
                title: title.to_string(),
                year: 2023,
                provider: provider.to_string(),
                provider_id: id.to_string(),
                poster: String::new(),
                synopsis: String::new(),
                genres: String::new(),
                length: String::new(),
                episode_count: 0,
            }
        }
        let tenrai = frieren("tenrai", "t-1", "Frieren");
        // Same work, different punctuation: normalized tokens agree
        // (`frieren` == `frieren`), but only the exact title ranks first, so
        // the merge keeps the tenrai identity. A genuinely longer title
        // (e.g. "Frieren Beyond Journey's End") is a different work under the
        // no-subset-merge rule and must NOT collapse here.
        let anilist = frieren("anilist", "a-1", "Frieren!");
        // Arrival order never changes the painted rows: the exact title stays
        // first and the same work collapses onto its identity.
        let mut painted: Vec<Vec<metadata::Candidate>> = Vec::new();
        for order in [vec![tenrai.clone(), anilist.clone()], vec![anilist.clone(), tenrai.clone()]] {
            let mut p = crate::search::MediaPicker::default();
            p.set_query("frieren 2023".to_string());
            p.set_filter(crate::search::KindFilter::AnimeSeries);
            let (generation, ..) = p.begin_search().expect("search starts");
            for batch in order {
                assert!(p.push_search(generation, vec![batch]), "batch applies mid-stream");
            }
            assert!(p.finish_search_now(generation), "the stream finishes");
            assert_eq!(p.candidates.len(), 1, "one work is one row");
            painted.push(p.candidates.clone());
        }
        assert_eq!(painted[0], painted[1], "live batches agree whatever leg answered first");
        // Both spellings normalize to the same exact title, so the closeness
        // rank is tied and the provider leg order decides: AniList is the
        // higher-precedence anime leg, so its identity is kept.
        assert_eq!(painted[0][0].provider, "anilist", "the tied rank keeps the better leg");
        // The cache round-trips the painted answer, and replaying it paints the
        // same provider in the same order.
        let _guard = arm_cache();
        let key = crate::search::search_cache_key(
            "frieren",
            2023,
            crate::search::KindFilter::AnimeSeries,
        );
        assert_eq!(key, "frieren|2023|anime-tv");
        store_search(&key, &painted[0], &[]);
        let cached = cached_search(&key).expect("the finished stream is cached");
        assert_eq!(cached.rows, painted[0], "the cache holds the painted rows");
        let mut replay = crate::search::MediaPicker::default();
        replay.set_query("frieren 2023".to_string());
        replay.set_filter(crate::search::KindFilter::AnimeSeries);
        let (generation, ..) = replay.begin_search().expect("replay starts");
        assert!(replay.push_search(generation, cached.rows.clone()));
        assert!(replay.finish_search_now(generation));
        assert_eq!(replay.candidates, painted[0], "replay paints the live rows");
        assert_eq!(replay.candidates[0].provider, "anilist");
        // Finishing invalidates a late batch: it is refused and the final
        // status stands instead of flipping back to a running count.
        let mut p = crate::search::MediaPicker::default();
        p.set_query("frieren 2023".to_string());
        let (generation, ..) = p.begin_search().expect("search starts");
        assert!(p.push_search(generation, vec![tenrai.clone()]));
        assert!(p.finish_search_now(generation));
        let final_status = p.candidates.clone();
        assert!(
            !p.push_search(generation, vec![anilist.clone()]),
            "a batch after the finish is stale"
        );
        assert_eq!(p.candidates, final_status, "the late batch paints nothing");
        assert!(!p.busy && p.open, "the finished list stays open on the final rows");
        assert!(
            !p.status.contains("so far"),
            "the final count stands: {}",
            p.status
        );
        cache_off();
    }

    /// The Card paints a match as `Title (Year)`, so the one-field parse
    /// inverts exactly that: parenthesized, comma and colon suffixes (with or
    /// without the space) and full-width digits all yield the typed year.
    #[test]
    fn title_year_parses_card_rendered_forms() {
        for (input, title, year) in [
            ("Dune (2021)", "Dune", 2021),
            ("Dune, 2021", "Dune", 2021),
            ("Dune: 2021", "Dune", 2021),
            ("Dune,2021", "Dune", 2021),
            ("Dune:2021", "Dune", 2021),
            ("Dune 2021", "Dune", 2021),
            ("Dune: Part Two 2024", "Dune: Part Two", 2024),
            ("Dune: Part Two (2024)", "Dune: Part Two", 2024),
            ("The Office (2005)", "The Office", 2005),
            ("Dune ２０２１", "Dune", 2021),
            ("\u{FF12}\u{FF10}\u{FF12}\u{FF11}", "", 2021),
            ("Blade Runner 2049", "Blade Runner 2049", 0),
            ("Dune", "Dune", 0),
            ("1984", "", 1984),
        ] {
            assert_eq!(
                parse_title_year(input),
                (title.to_string(), year),
                "parse {input:?}"
            );
        }
    }

    /// Settings scrolls: the below-fold tail is unreachable at offset 0 and
    /// reachable after scrolling, and paint, hit test and focus share the one
    /// `settings_scroll` offset (wheel/PgDn/End move it; focus reveal keeps it).
    /// The slimmed Settings fits its five rows above the status bar at rest:
    /// every surviving control hits inside the client with no scroll.
    #[test]
    fn settings_scroll_reaches_below_fold_controls() {
        let (cw, ch, dpi) = (MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        let raw = compute_settings_layout(cw, ch, dpi, false, "balanced", "amoled");
        let status_top = raw.status.top;
        let mut st = UiState::default();
        st.cur_view = View::Settings;
        let l0 = st.layout_settings_for(cw, ch, dpi);
        for id in settings_focus_order() {
            if id == BTN_CARD {
                continue;
            }
            let rc = l0
                .settings_btn(id)
                .unwrap_or_else(|| panic!("surviving row {id} missing"));
            assert!(
                rc.bottom <= status_top,
                "surviving row {id} {rc:?} needs a fold that no longer exists"
            );
            assert_eq!(
                button_at_point_above_status(
                    &l0.btns,
                    l0.status.top,
                    (rc.left + rc.right) / 2,
                    (rc.top + rc.bottom) / 2
                ),
                id,
                "surviving row {id} must hit at rest"
            );
        }
        let _ = raw;
    }

    /// A row straddling `status_top` paints and hits only above it: the
    /// clipped band is returned, the status bar itself never hits.
    #[test]
    fn straddling_row_neither_paints_nor_hits_in_status() {
        let status_top = 600;
        let rc = Rect {
            left: 10,
            top: 590,
            right: 100,
            bottom: 620,
        };
        let vis = clip_rect_above_status(rc, status_top).expect("the band above");
        assert_eq!((vis.top, vis.bottom), (590, 600));
        let btn = UiButton {
            id: 7,
            label: String::new(),
            style: BTN_OUTLINE,
            rc,
        };
        assert_eq!(button_at_point_above_status(std::slice::from_ref(&btn), status_top, 50, 610), 0);
        assert_eq!(button_at_point_above_status(&[btn], status_top, 50, 595), 7);
        assert_eq!(row_at_point_above_status(&[rc], status_top, 50, 610), -1);
        assert_eq!(row_at_point_above_status(&[rc], status_top, 50, 595), 0);
        let inside = Rect {
            top: 600,
            bottom: 620,
            ..rc
        };
        assert!(clip_rect_above_status(inside, status_top).is_none());
        let above = Rect {
            top: 570,
            bottom: 590,
            ..rc
        };
        assert_eq!(
            clip_rect_above_status(above, status_top).unwrap().bottom,
            590
        );
    }

    /// At the supported-minimum clients Apply stays inside the client above
    /// the status bar instead of sliding below the fold.
    #[test]
    fn narrow_footer_stays_inside_small_clients() {
        for (cw, ch) in [(360, 270), (460, 360)] {
            for playing in [false, true] {
                let l = compute_card_narrow(
                    cw,
                    ch,
                    96,
                    &CardSpecs::default(),
                    CardKind::Series,
                    PreviewSurface::default(),
                    &CardLayoutState {
                        advanced: false,
                        has_selection: true,
                        can_apply: true,
                        playing,
                        filter: crate::search::KindFilter::All,
                    },
                );
                let apply = l.card_btn(BTN_PICKER_APPLY).unwrap_or_else(|| {
                    panic!("{cw}x{ch} Apply button missing")
                });
                assert!(
                    apply.left >= 0 && apply.top >= 0 && apply.right <= cw && apply.bottom <= ch,
                    "{cw}x{ch} Apply escapes the client: {apply:?}"
                );
                assert!(
                    apply.bottom <= l.status.top,
                    "{cw}x{ch} Apply slides under the status bar: {apply:?}"
                );
                assert!(
                    l.card_btn(BTN_ADVANCED).is_none(),
                    "{cw}x{ch} still lays out Change filters"
                );
            }
        }
    }

    /// A button-up with no matching press activates nothing: the picker keeps
    /// its selection, and an unrelated press never fires the release point.
    #[test]
    fn release_without_press_activates_nothing() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.candidates = vec![office_series(), dune_movie(), anime_series()];
        st.picker.open = true;
        st.picker.searched = true;
        st.picker.selected = 0;
        // Wide client: every candidate is visible at once.
        let l = st.layout_card_for(1180, 700, 96);
        let row_btn = |l: &CardLayout, idx: usize| {
            l.btns
                .iter()
                .find(|b| b.id == BTN_PICKER_RESULT_AT + idx as i32)
                .unwrap_or_else(|| panic!("row {idx} must be visible"))
                .rc
        };
        let (r0, r1) = (row_btn(&l, 0), row_btn(&l, 1));
        let (x0, y0) = ((r0.left + r0.right) / 2, (r0.top + r0.bottom) / 2);
        let (x1, y1) = ((r1.left + r1.right) / 2, (r1.top + r1.bottom) / 2);
        // A release with no press selects nothing.
        st.release_card_layout_at(&l, x1, y1, 96);
        assert_eq!(st.picker.selected, 0, "press-less release must not select");
        // A press on one row and a release on another selects nothing.
        st.press_card_layout_at(&l, x0, y0, 96);
        st.release_card_layout_at(&l, x1, y1, 96);
        assert_eq!(st.picker.selected, 0, "a mismatched press must not select");
        // The paired press selects.
        st.click_card_layout_at(&l, x1, y1, 96);
        assert_eq!(st.picker.selected, 1, "the paired click selects");
        // Same rule off the list: a stray press never fires Apply.
        let adv = l.card_btn(BTN_ADVANCED).expect("Change filters laid out");
        let (ax, ay) = ((adv.left + adv.right) / 2, (adv.top + adv.bottom) / 2);
        let was = st.advanced_open;
        st.press_card_layout_at(&l, x0, y0, 96);
        st.release_card_layout_at(&l, ax, ay, 96);
        assert_eq!(st.advanced_open, was, "a mismatched press must not fire");
        st.click_card_layout_at(&l, ax, ay, 96);
        assert_eq!(st.advanced_open, !was, "the paired click fires");
    }

    /// The Fix-match dialog keeps the row the user clicked when a new batch
    /// lands: the picked key survives a batch that still carries it (instead
    /// of reselecting index 0) and drops only when the row is gone.
    #[test]
    fn fix_match_keeps_picked_row_across_batches() {
        fn cand(provider: &str, id: &str, title: &str) -> Candidate {
            Candidate {
                kind: Kind::SERIES,
                title: title.to_string(),
                year: 2023,
                provider: provider.to_string(),
                provider_id: id.to_string(),
                poster: String::new(),
                synopsis: String::new(),
                genres: String::new(),
                length: String::new(),
                episode_count: 0,
            }
        }
        let mut st = fix_headless_state(fix_series_path(), false);
        let (a, b, c) = (
            cand("tvmaze", "a", "Alpha"),
            cand("anilist", "b", "Beta"),
            cand("tenrai", "c", "Gamma"),
        );
        st.candidates = vec![a.clone(), b.clone(), c.clone()];
        // The user clicked the middle row.
        st.picked = Some(fix_pick_key(&b));
        assert_eq!(
            st.selected_candidate().map(|c| c.provider_id),
            Some("b".to_string()),
            "the headless selection follows the pick"
        );
        // A new batch reorders and grows but still carries the row.
        let d = cand("kitsu", "d", "Delta");
        *st.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((st.generation, FixOutcome::Search(vec![c.clone(), a.clone(), d.clone(), b.clone()])));
        st.on_job_done();
        assert_eq!(
            st.selected_candidate().map(|c| c.provider_id),
            Some("b".to_string()),
            "the batch must keep the clicked row, not index 0"
        );
        // A batch that drops the row drops the pick and falls back to first.
        *st.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((st.generation, FixOutcome::Search(vec![a.clone(), c.clone()])));
        st.on_job_done();
        assert_eq!(st.picked, None, "the lost row clears the pick");
        assert_eq!(
            st.selected_candidate().map(|c| c.provider_id),
            Some("a".to_string())
        );
    }

    /// The Card's synopsis affordance is a real control (hoverable, clickable,
    /// focusable) and a Settings body row is reachable and visibly hovered.
    #[test]
    fn synopsis_affordance_and_settings_hover_are_reachable() {
        let l = compute_card_narrow(
            460,
            680,
            96,
            &CardSpecs::default(),
            CardKind::Series,
            PreviewSurface::default(),
            &CardLayoutState {
                advanced: false,
                has_selection: true,
                can_apply: true,
                playing: true,
                filter: crate::search::KindFilter::All,
            },
        );
        let syn = l.card_btn(BTN_SYNOPSIS).expect("synopsis affordance laid out");
        assert_eq!(
            (syn.left, syn.top, syn.right, syn.bottom),
            (l.synopsis.left, l.synopsis.top, l.synopsis.right, l.synopsis.bottom),
            "the affordance is the synopsis rect"
        );
        assert!(
            card_focus_order().contains(&BTN_SYNOPSIS),
            "the affordance is a Tab stop"
        );
        assert_eq!(
            button_at_point_above_status(
                &l.btns,
                l.status.top,
                (syn.left + syn.right) / 2,
                (syn.top + syn.bottom) / 2
            ),
            BTN_SYNOPSIS,
            "the affordance is hoverable"
        );
        let mut st = UiState::default();
        st.hover_btn = BTN_SYNOPSIS;
        assert!(st.synopsis_visible(), "hover reveals the synopsis");
        // A Settings body row hits through the shared scrolled table and stays
        // visible above the status bar.
        let mut settings = UiState::default();
        settings.cur_view = View::Settings;
        let sl = settings.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        let rc = sl.settings_btn(BTN_DISCORD).expect("Discord row laid out");
        assert_eq!(
            button_at_point_above_status(
                &sl.btns,
                sl.status.top,
                (rc.left + rc.right) / 2,
                (rc.top + rc.bottom) / 2
            ),
            BTN_DISCORD,
            "the Settings row is hoverable"
        );
        assert!(
            clip_rect_above_status(rc, sl.status.top).is_some(),
            "the Settings row paints above the status bar"
        );
    }

    /// Owner visual-failure repair (2026-10-29): with real image bytes in the
    /// cover cache, the production paint path must show those pixels in the
    /// art rect. The supplied fixture is a solid blue BMP, so the centre pixel
    /// is an independent oracle: a blank tile or an overlay glyph cannot match.
    /// No network is touched; the bytes are seeded through the cache seam.
    #[test]
    fn preview_paints_supplied_cover_bytes_without_a_blank_tile() {
        crate::test_env::EnvGuard::with_scratch("cover-paint", |_dir| {
            const URL: &str = "https://example.com/dune-cover.bmp";
            crate::artwork::put_cached(URL, &solid_bmp(64, 96, 0x32, 0x64, 0xC8)).unwrap();
            assert!(
                crate::artwork::Cover::load(URL).is_some(),
                "the seeded BMP must decode through GDI+"
            );
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.record_preview(
                CardKind::Movie,
                "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
                Some(Action {
                    running: true,
                    has_start: true,
                    start: 1_700_000_000_000,
                    ct: 1000,
                    ..Action::default()
                }),
                Resolved {
                    series: "Dune: Part Two".to_string(),
                    year: 2024,
                    is_movie: true,
                    genres: "Sci-Fi, Adventure".to_string(),
                    length: "2h 46m".to_string(),
                    artwork: URL.to_string(),
                    ..Resolved::default()
                },
            );
            let (model, _) = card_preview_model(&st);
            assert!(model.art, "cached cover must turn the cover slot on");
            let (cw, ch) = (1440, 900);
            let l = compute_card_layout(
                cw,
                ch,
                96,
                &st.card_specs_state,
                st.displayed_card_kind(),
                st.preview_surface,
            );
            let px = paint_offscreen(cw, ch, 96, &st);
            let (mx, my) = (
                (l.art.left + l.art.right) / 2,
                (l.art.top + l.art.bottom) / 2,
            );
            let got = pixel_at(&px, cw, mx, my);
            assert!(
                got != COL_SURFACE && got != COL_CARD && got != COL_BG,
                "the decoded cover pixels did not reach the art rect: {got:#08x}"
            );
        });
    }

    /// The picker's explicit Movie/TV choice (owner direction 2026-10-29)
    /// replaces the filename/candidate derivation: it drives both the provider
    /// search kind and the latch scope, and the layout carries the kind row
    /// where the retired separate Year control used to sit.
    #[test]
    fn kind_choice_drives_search_and_latch_without_a_year_control() {
        let mut st = fix_headless_state(fix_series_path(), false);
        st.choices = fix_match_scope_choices(&st.path, false);
        assert_eq!(st.search_want(), Kind::SERIES, "TV is the default kind");
        assert!(
            matches!(
                st.selected_target(),
                Some((Scope::Series | Scope::Season, _))
            ),
            "a TV choice latches a series/season target"
        );
        assert!(st.apply_pick_kind(true), "Movie changes the kind");
        assert_eq!(st.search_want(), Kind::MOVIE);
        assert!(st.choices.is_empty(), "a movie offers no scope radios");
        assert!(
            matches!(st.selected_target(), Some((Scope::Movie, _))),
            "a movie choice latches the movie key"
        );
        assert!(st.apply_pick_kind(false), "TV changes the kind back");
        assert_eq!(st.search_want(), Kind::SERIES);
        assert!(!st.choices.is_empty(), "a TV choice offers the scope radios");
        assert!(
            !st.apply_pick_kind(false),
            "picking the current kind is a no-op"
        );

        let l = fix_match_layout(96, FixScreen::Search, 2);
        assert_eq!(l.kind_radios.len(), 2, "Movie and TV radios");
        for rc in std::iter::once(l.kind_label).chain(l.kind_radios.iter().copied()) {
            assert!(
                rc.left >= 0 && rc.top >= 0 && rc.right <= l.client_w && rc.bottom <= l.client_h,
                "kind rect outside the dialog: {rc:?}"
            );
        }
        assert!(
            l.kind_label.top >= l.name_edit.bottom,
            "kind row sits under the one query field"
        );
        assert!(
            l.kind_radios[1].bottom <= l.progress.top,
            "kind row sits above the results strip"
        );
        assert!(
            l.name_edit.right <= l.search.left && l.name_edit.bottom <= l.kind_label.top,
            "the query field and Search share the first row"
        );
    }

    /// Approved narrow + untouched wide: narrow keeps the hero/search/scope/
    /// panel inside the client (tiny heights scroll the panel); wide keeps the
    /// three disjoint surfaces.
    #[test]
    fn designer_layout_never_clips_at_workspace_and_phone_sizes() {
        for (cw, ch, want_wide) in [
            (1440, 900, true),
            (1180, 700, true),
            (1000, 700, false),
            (460, 680, false),
            (390, 844, false),
            (360, 700, false),
            (480, 270, false),
        ] {
            let l = compute_card_layout(
                cw,
                ch,
                96,
                &CardSpecs::default(),
                CardKind::Series,
                PreviewSurface::default(),
            );
            // Approved wide threshold: 1180x700.
            assert_eq!(l.wide, cw >= 1180 && ch >= 700, "{cw}x{ch}: wide flag");
            let _ = want_wide;
            for b in &l.btns {
                if b.rc.right <= b.rc.left || b.rc.bottom <= b.rc.top {
                    continue;
                }
                assert!(
                    b.rc.left >= 0 && b.rc.right <= cw,
                    "{cw}x{ch}: button {} escapes horizontally: {:?}",
                    b.id,
                    b.rc
                );
                if ch >= 680 || l.wide {
                    assert!(
                        b.rc.top >= 0 && b.rc.bottom <= ch,
                        "{cw}x{ch}: button {} outside the client: {:?}",
                        b.id,
                        b.rc
                    );
                }
            }
            for (i, a) in l.btns.iter().enumerate() {
                for b in l.btns.iter().skip(i + 1) {
                    if a.rc.right <= a.rc.left
                        || a.rc.bottom <= a.rc.top
                        || b.rc.right <= b.rc.left
                        || b.rc.bottom <= b.rc.top
                    {
                        continue;
                    }
                    assert!(
                        !rects_intersect(a.rc, b.rc),
                        "{cw}x{ch}: buttons {} {:?} and {} {:?} overlap",
                        a.id,
                        a.rc,
                        b.id,
                        b.rc
                    );
                }
            }
            for (name, r) in [
                ("preview", l.preview),
                ("art", l.art),
                ("title", l.title),
                ("meta", l.meta),
                ("synopsis", l.synopsis),
                ("rail", l.rail),
                ("stage", l.stage),
            ] {
                if r.right <= r.left || r.bottom <= r.top {
                    // Narrow redesign: preview/synopsis empty (results panel owns
                    // the list); tiny heights scroll.
                    continue;
                }
                assert!(
                    r.left >= 0 && r.right <= cw,
                    "{cw}x{ch}: {name} escapes horizontally: {r:?}"
                );
                if ch >= 680 || l.wide {
                    assert!(
                        r.top >= 0 && r.bottom <= ch,
                        "{cw}x{ch}: {name} outside the client: {r:?}"
                    );
                }
                let _ = name;
            }
            if !l.compact {
                assert!(l.status.bottom > l.status.top, "{cw}x{ch}: empty status");
            }
            for r in &l.rows {
                // Narrow hero rows live in the hero; wide rows in the preview.
                let inside_hero = r.left >= l.object.left
                    && r.right <= l.object.right
                    && r.top >= l.object.top
                    && r.bottom <= l.object.bottom;
                let inside_preview = (r.left == 0
                    && r.top == 0
                    && r.right == 0
                    && r.bottom == 0)
                    || (r.left >= l.preview.left
                        && r.right <= l.preview.right
                        && r.top >= l.preview.top
                        && r.bottom <= l.preview.bottom);
                assert!(
                    inside_hero || inside_preview,
                    "{cw}x{ch}: media row outside hero/preview: {r:?}"
                );
            }
            if !l.compact {
                let rects = card_text_rects(cw, ch, 96, &l);
                for (i, a) in rects.iter().enumerate() {
                    for b in rects.iter().skip(i + 1) {
                        if a.rc.right <= a.rc.left
                            || a.rc.bottom <= a.rc.top
                            || b.rc.right <= b.rc.left
                            || b.rc.bottom <= b.rc.top
                        {
                            continue;
                        }
                        assert!(
                            !rects_intersect(a.rc, b.rc),
                            "{cw}x{ch}: {} {:?} and {} {:?} intersect",
                            a.id,
                            a.rc,
                            b.id,
                            b.rc
                        );
                    }
                }
            }
            if l.wide {
                for (i, a) in l.surfaces.iter().enumerate() {
                    for b in l.surfaces.iter().skip(i + 1) {
                        assert!(
                            !rects_intersect(*a, *b),
                            "{cw}x{ch}: surfaces overlap: {a:?} / {b:?}"
                        );
                    }
                }
                assert!(
                    l.rail.right <= l.stage.left,
                    "{cw}x{ch}: rail overlaps the stage"
                );
                assert!(
                    l.surfaces.iter().all(|s| s.right > s.left && s.bottom > s.top),
                    "{cw}x{ch}: an empty wide surface"
                );
            } else {
                // Approved narrow: results panel owns the list; hero owns
                // rows. The one field and one Search are at rest; the removed
                // scope ids (85/88 here, all five in the layout test) and
                // Change filters are absent.
                if !l.compact {
                    for id in [BTN_PICKER_FIELD, BTN_PICKER_SEARCH] {
                        assert!(
                            l.card_btn(id).is_some(),
                            "{cw}x{ch}: control {id} missing at rest"
                        );
                    }
                    for id in [85, 88] {
                        assert!(
                            l.card_btn(id).is_none(),
                            "{cw}x{ch}: removed scope {id} laid out at rest"
                        );
                    }
                }
            }
        }
    }

    /// QA wide item: the wide workspace at **1180x760** -- 60 px taller than the
    /// 1180x700 floor every existing gate pins. No size above the floor was
    /// asserted anywhere, so a regression that only shows above it (a band
    /// pinned to a 700-derived coordinate, a surface that stops growing, a
    /// control that leaves the client) would have shipped unseen.
    ///
    /// Red when the wide branch is not taken at this size, when a control
    /// leaves the client or two controls/surfaces intersect, when the extra
    /// height does not reach the stage and the lower band, or when a presence
    /// surface paints no panel at all.
    #[test]
    fn wide_workspace_at_1180x760_holds_every_control_and_paints_three_surfaces() {
        let (cw, ch) = (1180, 760);
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.candidates = vec![office_series(), dune_movie(), anime_series()];
        st.picker.searched = true;
        assert!(st.select_candidate(0), "the row selects");
        let layout = |cw: i32, ch: i32| {
            compute_card_layout_state(
                cw,
                ch,
                96,
                &st.card_specs_state,
                st.displayed_card_kind(),
                st.preview_surface,
                &st.card_layout_state(),
            )
        };
        let l = layout(cw, ch);
        let floor = layout(cw, 700);
        assert!(l.wide, "1180x760 is the wide workspace");
        assert!(floor.wide, "fixture: 1180x700 is the approved wide floor");

        // Every laid-out control stays inside the client, and no two intersect.
        for b in &l.btns {
            if b.rc.right <= b.rc.left || b.rc.bottom <= b.rc.top {
                continue;
            }
            assert!(
                b.rc.left >= 0 && b.rc.right <= cw && b.rc.top >= 0 && b.rc.bottom <= ch,
                "button {} outside the client: {:?}",
                b.id,
                b.rc
            );
        }
        for (i, a) in l.btns.iter().enumerate() {
            for b in l.btns.iter().skip(i + 1) {
                if a.rc.right <= a.rc.left
                    || a.rc.bottom <= a.rc.top
                    || b.rc.right <= b.rc.left
                    || b.rc.bottom <= b.rc.top
                {
                    continue;
                }
                assert!(
                    !rects_intersect(a.rc, b.rc),
                    "buttons {} {:?} and {} {:?} overlap",
                    a.id,
                    a.rc,
                    b.id,
                    b.rc
                );
            }
        }

        // Three disjoint, non-empty surfaces.
        for (i, a) in l.surfaces.iter().enumerate() {
            assert!(
                a.right > a.left && a.bottom > a.top,
                "surface {i} is empty: {a:?}"
            );
            assert!(
                a.left >= 0 && a.right <= cw && a.top >= 0 && a.bottom <= ch,
                "surface {i} outside the client: {a:?}"
            );
            for b in l.surfaces.iter().skip(i + 1) {
                assert!(!rects_intersect(*a, *b), "surfaces overlap: {a:?} / {b:?}");
            }
        }

        // The 60 px the client gained is absorbed by the stage and the lower
        // band, and by nothing else: the rail, the object card and the tab strip
        // are identical at both heights.
        assert_eq!(l.stage.top, floor.stage.top, "the stage moved");
        assert_eq!(
            l.stage.bottom - floor.stage.bottom,
            ch - 700,
            "the stage did not absorb the extra height"
        );
        assert!(
            r_eq(l.surfaces[0], floor.surfaces[0]),
            "the member strip grew: {:?} vs {:?}",
            l.surfaces[0],
            floor.surfaces[0]
        );
        for (i, (now, then)) in l.surfaces.iter().zip(floor.surfaces.iter()).enumerate() {
            if i == 0 {
                continue;
            }
            assert_eq!(now.top, then.top, "surface {i} moved");
            assert_eq!(
                now.bottom - then.bottom,
                ch - 700,
                "surface {i} did not absorb the extra height"
            );
        }
        assert!(
            r_eq(l.object, floor.object),
            "the object card grew: {:?} vs {:?}",
            l.object,
            floor.object
        );

        // The surfaces actually paint a panel at this size: a sample inside each
        // one is not the bare card fill.
        let px = paint_offscreen(cw, ch, 96, &st);
        for (i, s) in l.surfaces.iter().enumerate() {
            let (x, y) = ((s.left + s.right) / 2, (s.top + s.bottom) / 2);
            assert_ne!(
                pixel_at(&px, cw, x, y),
                COL_BG,
                "surface {i} painted nothing at {x},{y}"
            );
        }
    }

    /// Defect (2026-11-13): the wide object card dropped the runtime. The
    /// narrow card renders `2021 · Film · 2h 35m`; the wide card formatted
    /// `"{year} · {kind}"` on its own and rendered `2021 · Film`. Both cards now
    /// read the one [`hero_meta_for`] formatter, so the runtime reaches the wide
    /// surface at the same size as the narrow one.
    ///
    /// Red when the wide meta ink stops at the bare year and kind: the same
    /// record painted with its runtime and painted without it then draws the
    /// same pixels, and the meta's rightmost ink column does not move.
    #[test]
    fn wide_object_card_paints_the_runtime_beside_the_year_and_kind() {
        let (cw, ch) = (1180, 760);
        let paint = |length: &str| {
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.picker.candidates = vec![metadata::Candidate {
                length: length.to_string(),
                ..dune_movie()
            }];
            st.picker.searched = true;
            assert!(st.select_candidate(0), "the row selects");
            let l = compute_card_layout_state(
                cw,
                ch,
                96,
                &st.card_specs_state,
                st.displayed_card_kind(),
                st.preview_surface,
                &st.card_layout_state(),
            );
            assert!(l.wide, "1180x760 is the wide workspace");
            assert!(
                l.obj_meta.right > l.obj_meta.left && l.obj_meta.bottom > l.obj_meta.top,
                "the object card lays out its meta line: {:?}",
                l.obj_meta
            );
            (paint_offscreen(cw, ch, 96, &st), l.obj_meta)
        };
        // Muted meta text on the card fill: ink is far brighter than either.
        let rightmost_ink = |px: &[u8], rc: Rect| -> i32 {
            let mut right = rc.left - 1;
            for y in rc.top..rc.bottom {
                for x in rc.left..rc.right {
                    let i = ((y as usize) * cw as usize + x as usize) * 4;
                    let sum = u32::from(px[i]) + u32::from(px[i + 1]) + u32::from(px[i + 2]);
                    if sum > 3 * 0x90 {
                        right = right.max(x);
                    }
                }
            }
            right
        };

        let (with_runtime, meta) = paint("2h 35m");
        let (without_runtime, bare_meta) = paint("");
        assert!(
            r_eq(meta, bare_meta),
            "fixture: the runtime changes no layout ({meta:?} vs {bare_meta:?})"
        );
        let right_with = rightmost_ink(&with_runtime, meta);
        let right_without = rightmost_ink(&without_runtime, bare_meta);
        assert!(
            right_without >= meta.left,
            "fixture: `2021 · Film` paints inside the meta rect"
        );
        assert!(
            right_with > right_without,
            "the wide card paints no runtime: meta ink ends at {right_with} with `2h 35m` \
             and at {right_without} without it"
        );
        assert!(
            right_with - right_without >= 20,
            "the runtime is missing from the wide meta line: only {}px wider than the bare \
             year and kind, which is not ` · 2h 35m`",
            right_with - right_without
        );
    }

    /// One payload, three surfaces: the header, details and state are the same
    /// strings on every surface; the synopsis body is local to the Activity
    /// tab; the designer's off-fields disappear everywhere at once.
    #[test]
    fn preview_model_keeps_all_three_surfaces_in_lockstep() {
        let run = Action {
            running: true,
            has_start: true,
            start: 1_700_000_000_000,
            ct: 1000,
            ..Action::default()
        };
        let res = Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul unites with the Fremen.".to_string(),
            artwork: "https://example.com/dune.jpg".to_string(),
            ..Resolved::default()
        };
        let act = presence::compose_card(
            "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
            Some(&run),
            &res,
            true,
            &presence::cardspec::default_card_spec(),
        );
        let p = surface_preview(&act, true, &res.synopsis, 1_700_000_060_000, 166 * 60_000, true);
        assert_eq!(p.header, "Watching Dune: Part Two");
        assert!(p.art);
        assert!(p.clock.timer && p.clock.fraction > 0.0 && p.clock.fraction < 1.0);
        for surface in [
            PreviewSurface::Member,
            PreviewSurface::Profile,
            PreviewSurface::Activity,
        ] {
            let lines = surface_lines(&p, surface);
            assert_eq!(lines.first().map(String::as_str), Some(p.header.as_str()));
            assert!(lines.contains(&p.details), "{surface:?}: details missing");
            assert!(lines.contains(&p.state), "{surface:?}: state missing");
            if surface == PreviewSurface::Activity {
                assert!(
                    lines.contains(&p.synopsis),
                    "the Activity tab carries the local synopsis"
                );
            } else {
                assert!(
                    !lines.iter().any(|l| l == &p.synopsis),
                    "{surface:?}: the synopsis body is Activity-only"
                );
            }
        }

        // Episode identity: the header is the series title and the state keeps
        // the episode facts.
        let ep = Resolved {
            series: "Breaking Bad".to_string(),
            episode_name: "Problem Dog".to_string(),
            season: 4,
            number: 7,
            synopsis: "Walt faces the consequences.".to_string(),
            ..Resolved::default()
        };
        let act = presence::compose_card(
            "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer",
            Some(&run),
            &ep,
            false,
            &presence::cardspec::default_card_spec(),
        );
        let p = surface_preview(&act, true, &ep.synopsis, 1_700_000_060_000, 45 * 60_000, true);
        assert_eq!(p.header, "Watching Breaking Bad");
        assert!(
            p.state.contains("S04E07") && p.state.contains("Problem Dog"),
            "episode state: {:?}",
            p.state
        );

        // The designer's off-fields vanish from the payload AND the model.
        let bare = presence::designer::DesignerSpec {
            artwork: false,
            details: false,
            state: false,
            synopsis: false,
            timers: false,
        }
        .apply(act);
        let p = surface_preview(&bare, true, "", 1_700_000_060_000, 45 * 60_000, false);
        assert!(p.details.is_empty() && p.state.is_empty());
        assert!(!p.art && p.synopsis.is_empty() && !p.clock.timer);
        for surface in [
            PreviewSurface::Member,
            PreviewSurface::Profile,
            PreviewSurface::Activity,
        ] {
            assert_eq!(
                surface_lines(&p, surface),
                vec![p.header.clone()],
                "{surface:?}: only the header survives"
            );
        }
    }

    /// Runtime parsing and the preview clock: `\d+ min`, `Hh Mm`, `Hh`, and
    /// `H:MM[:SS]`; paused counts up and holds the bar; no anchor hides it.
    #[test]
    fn hero_meta_and_version_derive_from_the_record() {
        let mut version_parts = crate::settings::APP_VERSION.split('.');
        let major = version_parts.next().unwrap_or("").trim();
        let minor = version_parts.next().unwrap_or("").trim();
        assert_eq!(
            app_version_short(),
            format!("v{major}.{minor}"),
            "from {}",
            crate::settings::APP_VERSION
        );
        // Movies show the runtime, never an episode marker.
        assert_eq!(
            hero_meta_for(2024, true, false, "2h 46m", 0, 0, 0),
            "2024 · Film · 2h 46m"
        );
        assert_eq!(hero_meta_for(2024, true, false, "", 0, 0, 0), "2024 · Film");
        assert_eq!(
            hero_meta_for(2024, true, false, "2h 46m", 2, 1, 0),
            "2024 · Film · 2h 46m",
            "a Film never carries S02E01 even with episode numbers present"
        );
        // Series show the episode code, never a bare runtime.
        assert_eq!(
            hero_meta_for(2024, false, true, "60 min", 2, 1, 0),
            "2024 · Anime series · S02E01"
        );
        assert_eq!(
            hero_meta_for(2024, false, false, "60 min", 2, 1, 0),
            "2024 · Series · S02E01"
        );
        assert_eq!(hero_episode_code(2, 1, 0), "S02E01");
        assert_eq!(hero_episode_code(0, 0, 0), "");
        // The match-count header exists only after a search ran.
        let mut st = UiState::default();
        assert!(!st.picker.searched, "a fresh picker never searched");
        st.picker.searched = true;
        assert!(st.picker.searched);
    }

    /// The status version keeps its 16px right inset and its `GetTextExtent`
    /// width fits inside the inset rect at every DPI (the shipped clip was a
    /// version touching the window's right edge).
    #[test]
    fn status_version_keeps_16px_inset_and_fits_at_every_dpi() {
        for dpi in [96, 120, 144, 168, 192, 224, 240, 288] {
            let cw = scale_px(dpi, MAIN_CLIENT_W);
            let (_, content_w) = content_span(cw, dpi);
            let margin = (cw - content_w) / 2;
            let status = Rect {
                left: margin,
                top: 0,
                right: margin + content_w,
                bottom: scale_px(dpi, STATUS_BAR_H_L),
            };
            let rc = status_version_rect(status, dpi);
            assert_eq!(
                status.right - rc.right,
                scale_px(dpi, STATUS_VERSION_INSET_L),
                "dpi {dpi}: 16px right inset"
            );
            assert!(rc.right > rc.left, "dpi {dpi}: the inset rect is non-empty");
            let Some(need) = measure_version_width(dpi) else {
                return; // no GDI DC: skip (the one native exception)
            };
            assert!(
                need <= rc.right - rc.left,
                "dpi {dpi}: version {need}px exceeds inset rect {}px",
                rc.right - rc.left
            );
        }
    }

    /// The dark caption covers both DWM attributes: 20
    /// (`DWMWA_USE_IMMERSIVE_DARK_MODE`) via `apply_dark_caption`, and the
    /// pre-22000 fallback 19. The fallback FFI round-trips (a null HWND must
    /// return an HRESULT, never crash); the HRESULT is quoted, not asserted.
    #[test]
    fn dark_caption_covers_20_with_19_fallback() {
        assert_eq!(crate::win32::DWMWA_USE_IMMERSIVE_DARK_MODE, 20);
        assert_eq!(DWMWA_DARK_MODE_FALLBACK_19, 19);
        let hr = apply_dark_mode_fallback_19(std::ptr::null_mut());
        eprintln!("dark-19 fallback HRESULT on null HWND: {hr:#X}");
    }

    #[test]
    fn preview_runtime_and_clock_are_honest() {
        assert_eq!(parse_length_ms("166 min"), 166 * 60_000);
        assert_eq!(parse_length_ms("2h 19m"), 139 * 60_000);
        assert_eq!(parse_length_ms("2h"), 120 * 60_000);
        assert_eq!(parse_length_ms("45 min"), 45 * 60_000);
        assert_eq!(parse_length_ms("1:32:00"), 92 * 60_000);
        assert_eq!(parse_length_ms("1:32"), 92 * 60_000);
        assert_eq!(parse_length_ms(""), 0);
        assert_eq!(parse_length_ms("unknown"), 0);

        let playing = Activity {
            has_start: true,
            start: 1_000_000,
            ..Activity::default()
        };
        let c = preview_clock(&playing, 1_060_000, 600_000);
        assert!(c.timer && !c.paused);
        assert!((c.fraction - 0.1).abs() < 1e-9, "fraction {}", c.fraction);
        assert_eq!(c.left, "01:00");
        assert_eq!(c.right, "-09:00");

        let paused = Activity {
            has_start: true,
            start: 1_000_000,
            paused: true,
            ..Activity::default()
        };
        let c = preview_clock(&paused, 1_065_000, 600_000);
        assert!(c.timer && c.paused);
        assert_eq!(c.fraction, 0.0);
        assert_eq!(c.left, "Paused");
        assert_eq!(c.right, "01:05");

        assert!(!preview_clock(&Activity::default(), 1_000_000, 600_000).timer);
    }

    /// Approved narrow drops the Discord timer strip from the hero (status is
    /// "Playing in PotPlayer"); the countdown lives in the model and the wide
    /// surfaces. Pinned both ways at the phone size through the model, and
    /// through the wide paint path for ink.
    #[test]
    fn narrow_preview_keeps_the_countdown_when_a_runtime_exists() {
        let now = preview_now_ms();
        let seeded = |length: &str| {
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.preview_surface = PreviewSurface::Profile;
            st.record_preview(
                CardKind::Series,
                "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer".to_string(),
                Some(Action {
                    running: true,
                    has_start: true,
                    start: now - 12 * 60_000,
                    ct: 1000,
                    ..Action::default()
                }),
                Resolved {
                    series: "Breaking Bad".to_string(),
                    episode_name: "Problem Dog".to_string(),
                    season: 4,
                    number: 7,
                    length: length.to_string(),
                    ..Resolved::default()
                },
            );
            st
        };
        let (cw, ch, dpi) = (390, 844, 96);
        // The label band under the 4px track: `state.bottom + SP_2` for the
        // track, `+4` for its height, `+SP_2` for the labels. The measured
        // band starts one row under the track and stops one row short of the
        // panel foot, so neither the track nor the border can count as ink.
        let band = |st: &UiState| {
            let l = compute_card_layout(
                cw,
                ch,
                dpi,
                &st.card_specs_state,
                st.displayed_card_kind(),
                st.preview_surface,
            );
            let pad = scale_px(dpi, SP_3);
            let foot = (l.synopsis.bottom + scale_px(dpi, SP_2) + scale_px(dpi, 30))
                .min(l.preview.bottom - scale_px(dpi, SP_3));
            (
                l.preview.left + pad,
                l.preview.right - pad,
                l.synopsis.bottom + scale_px(dpi, SP_2) + scale_px(dpi, 5),
                foot - scale_px(dpi, 1),
            )
        };
        let ink = |px: &[u8], x0: i32, x1: i32, y0: i32, y1: i32| {
            let mut n = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    if pixel_at(px, cw, x, y) != COL_CARD {
                        n += 1;
                    }
                }
            }
            n
        };

        let with = seeded("45 min");
        let without = seeded("");
        let (armed, have) = with.preview_activity();
        assert!(have && armed.has_start, "the seeded preview is not running");
        let (model, _) = card_preview_model(&with);
        assert_eq!(model.clock.left, "12:00", "elapsed label");
        assert!(
            matches!(model.clock.right.as_str(), "-33:00" | "-32:59"),
            "countdown label from 45 min: {:?}",
            model.clock.right
        );
        assert!((model.clock.fraction - 12.0 / 45.0).abs() < 1e-3);
        let (bare, _) = card_preview_model(&without);
        assert_eq!(bare.clock.left, "12:00", "elapsed survives no runtime");
        assert_eq!(bare.clock.right, "", "no runtime, no countdown");
        assert_eq!(bare.clock.fraction, 0.0, "no runtime, no fill");
        assert_eq!(
            surface_lines(&bare, PreviewSurface::Profile),
            vec![
                "Watching Breaking Bad".to_string(),
                "S04E07 \u{b7} Problem Dog".to_string(),
                "12:00".to_string()
            ],
            "the zero-runtime surface lists elapsed only"
        );
        assert!(
            surface_lines(&model, PreviewSurface::Profile)
                .iter()
                .any(|l| l.starts_with("-3")),
            "the runtime surface lists the countdown"
        );

        let (left0, right1, top, bottom) = band(&with);
        assert_eq!(band(&without), (left0, right1, top, bottom), "payload-independent strip");
        // Narrow redesign: hero carries no timer strip, so the narrow band is
        // empty by design. Prove the countdown through the wide paint path.
        if bottom <= top {
            let wl = compute_card_layout(1180, 700, 96, &with.card_specs_state, with.displayed_card_kind(), with.preview_surface);
            assert!(wl.preview.bottom > wl.preview.top, "wide preview paints the timer");
            return;
        }
        assert!(
            bottom > top + scale_px(dpi, 4),
            "{cw}x{ch}: the label band has no height: {top}..{bottom}"
        );
        let mid = (left0 + right1) / 2;
        let px_with = paint_offscreen(cw, ch, dpi, &with);
        let px_without = paint_offscreen(cw, ch, dpi, &without);
        let elapsed_with = ink(&px_with, left0, mid, top, bottom);
        let countdown_with = ink(&px_with, mid, right1, top, bottom);
        let elapsed_without = ink(&px_without, left0, mid, top, bottom);
        let countdown_without = ink(&px_without, mid, right1, top, bottom);
        assert!(
            elapsed_with > 40 && elapsed_without > 40,
            "{cw}x{ch}: the elapsed label is missing at the phone size \
             ({elapsed_with}/{elapsed_without} ink px)"
        );
        assert!(
            countdown_with > 40,
            "{cw}x{ch}: a runtime is present but the countdown label is absent from the \
             right half of the strip ({countdown_with} ink px) -- a width guard crept in"
        );
        assert_eq!(
            countdown_without, 0,
            "{cw}x{ch}: no runtime, yet the right half of the strip carries \
             {countdown_without} ink px -- the zero-runtime fallback invented a countdown"
        );
    }

    /// A designer toggle publishes to the live mirror and the Card preview
    /// recomposes immediately: details/synopsis/timers disappear from the
    /// composed activity, not just from a paint flag. Covers are always on:
    /// there is no artwork toggle, and the preview keeps the art while the
    /// other fields flip.
    #[test]
    fn designer_toggles_publish_and_recompose_the_preview() {
        let _serial = UI_GLOBAL_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        presence::set_live_designer(presence::designer::DesignerSpec::default());
        let mut st = UiState::default();
        st.record_preview(
            CardKind::Movie,
            "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
            Some(Action {
                running: true,
                has_start: true,
                start: 1_000,
                ct: 1000,
                ..Action::default()
            }),
            Resolved {
                series: "Dune: Part Two".to_string(),
                year: 2024,
                is_movie: true,
                genres: "Sci-Fi".to_string(),
                length: "2h 46m".to_string(),
                synopsis: "Paul unites with the Fremen.".to_string(),
                artwork: "https://example.com/dune.jpg".to_string(),
                ..Resolved::default()
            },
        );
        let (before, have) = st.preview_activity();
        assert!(have && !before.details.is_empty() && !before.large_image.is_empty());
        st.toggle_designer_field(DesignerField::Details);
        assert!(!presence::live_designer().details);
        assert!(st.preview_activity().0.details.is_empty());
        // Covers are always on: no artwork toggle exists, so the art survives
        // every remaining toggle. The tooltip stays empty here because this
        // note is episode-sourced, not work-sourced.
        let (off, _) = st.preview_activity();
        assert!(!off.large_image.is_empty(), "covers are always on");
        assert!(
            off.large_text.is_empty(),
            "episode-sourced prose never rides the tooltip: {:?}",
            off.large_text
        );
        st.toggle_designer_field(DesignerField::Timers);
        let (off, _) = st.preview_activity();
        assert!(!off.has_start && off.start == 0);
        assert!(!presence::live_designer().timers);
        presence::set_live_designer(presence::designer::DesignerSpec::default());
    }

    // ------------------------------------------------------------------
    // Persistent search cache + background preload
    // ------------------------------------------------------------------

    /// Arm the in-memory search cache for one test and keep the serial guard
    /// for its lifetime, so no unit test reads or writes the owner's file.
    fn arm_cache() -> std::sync::MutexGuard<'static, ()> {
        let guard = TEST_CACHE_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *TEST_SEARCH_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(std::collections::HashMap::new());
        guard
    }

    fn cache_arm_entry(key: &str, rows: Vec<Candidate>, failed: Vec<String>) {
        TEST_SEARCH_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
            .expect("the test cache is armed")
            .insert(
                key.to_string(),
                metadata::CachedSearch { rows, failed },
            );
    }

    fn cache_off() {
        *TEST_SEARCH_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Wait (bounded) for the preload thread to publish through the production
    /// `on_preload_done` entry point.
    fn drive_preload(st: &mut UiState) {
        for _ in 0..400 {
            st.on_preload_done();
            if st.preload.ready || st.preload_cancel.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the preload worker never published");
    }

    /// A complete cache hit is the whole answer: no provider call, the cached
    /// rows on screen, and the status says so.
    #[test]
    fn a_complete_cache_hit_answers_search_with_no_provider_call() {
        let _guard = arm_cache();
        let _env = crate::test_env::EnvGuard::scratch("picker-cache-hit");
        cache_arm_entry(
            &crate::search::search_cache_key("The Office", 0, crate::search::KindFilter::Series),
            vec![office_series()],
            Vec::new(),
        );
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.cache_enabled = true;
        st.picker_search = stub_search(calls.clone(), Ok(Vec::new()));
        st.picker.set_query("The Office".to_string());
        st.picker.set_filter(crate::search::KindFilter::Series);
        assert!(st.card_picker_search());
        assert!(
            calls.lock().unwrap().is_empty(),
            "a complete cache hit never calls a provider"
        );
        assert_eq!(st.picker.candidates.len(), 1);
        assert!(!st.picker.busy);
        // Approved copy deletes "(cached)" from the Card: the status is the
        // bare match count.
        assert_eq!(st.picker.status, "1 match for \"The Office\"");
        cache_off();
    }

    /// A partial entry shows its rows at once and retries ONLY the failed
    /// providers, never one that already answered.
    #[test]
    fn a_partial_cache_hit_shows_rows_and_retries_only_the_failed_providers() {
        let _guard = arm_cache();
        let _env = crate::test_env::EnvGuard::scratch("picker-cache-partial");
        cache_arm_entry(
            &crate::search::search_cache_key("The Office", 0, crate::search::KindFilter::Series),
            vec![office_series()],
            vec!["cinemeta".to_string()],
        );
        let onlys = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = onlys.clone();
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.cache_enabled = true;
        st.picker_search = Arc::new(
            move |_title: &str,
                  _year: i32,
                  _filter: crate::search::KindFilter,
                  only: &[String],
                  _sink: &(dyn Fn(Vec<metadata::Candidate>) + Send + Sync)| {
                seen.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(only.to_vec());
                Ok(PickerReport {
                    rows: vec![office_series()],
                    failed: Vec::new(),
                })
            },
        );
        st.picker.set_query("The Office".to_string());
        st.picker.set_filter(crate::search::KindFilter::Series);
        assert!(st.card_picker_search());
        assert_eq!(
            st.picker.candidates.len(),
            1,
            "the cached rows are on screen before the retry answers"
        );
        assert!(st.picker.busy, "the retry still runs");
        drive_picker_job(&mut st);
        assert_eq!(
            onlys.lock().unwrap()[0],
            vec!["cinemeta".to_string()],
            "only the failed provider is retried"
        );
        assert_eq!(st.picker.status, "1 match for \"The Office\"");
        cache_off();
    }

    /// The poll's prefetch fills the preload, and opening the card paints it
    /// with no network wait.
    #[test]
    fn prefetch_preloads_the_playing_title_and_the_card_opens_populated() {
        let _guard = arm_cache();
        crate::test_env::EnvGuard::with_scratch("picker-prefetch", |dir| {
            let season = dir.join("TV").join("The Office").join("Season 1");
            std::fs::create_dir_all(&season).unwrap();
            std::fs::write(season.join("The Office S01E01.mkv"), "x").unwrap();
            let path = season
                .join("The Office S01E01.mkv")
                .to_string_lossy()
                .into_owned();
            app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
                raw_title: "The Office S01E01.mkv - PotPlayer".to_string(),
                path,
            }));
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.cache_enabled = true;
            st.picker_search = stub_search(calls.clone(), Ok(vec![office_series()]));
            st.prefetch_now_playing();
            drive_preload(&mut st);
            assert!(st.preload.ready, "the playing title is preloaded");
            assert!(
                st.preload.key.starts_with("the office|"),
                "the key is title|year|scope: {}",
                st.preload.key
            );
            assert_eq!(calls.lock().unwrap().len(), 1);
            st.open_card();
            assert!(st.picker.open, "the list opens on the preload");
            assert_eq!(
                st.picker.filter,
                crate::search::KindFilter::All,
                "universal search: the card always runs both chains"
            );
            // Approved copy deletes "(cached)" from the Card.
            assert_eq!(st.picker.status, "1 match for \"The Office\"");
            assert_eq!(
                calls.lock().unwrap().len(),
                1,
                "opening the card runs no network search"
            );
        });
        app_support::config::set_live_now_playing(None);
        cache_off();
    }

    /// The same title|year|scope is a cache hit: the next episode of the same
    /// show never searches again.
    #[test]
    fn prefetch_of_the_next_episode_is_a_cache_hit() {
        let _guard = arm_cache();
        crate::test_env::EnvGuard::with_scratch("picker-prefetch-hit", |dir| {
            let season = dir.join("TV").join("The Office").join("Season 1");
            std::fs::create_dir_all(&season).unwrap();
            for n in 1..=2 {
                std::fs::write(season.join(format!("The Office S01E0{n}.mkv")), "x").unwrap();
            }
            let first = season.join("The Office S01E01.mkv");
            app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
                raw_title: "The Office S01E01.mkv - PotPlayer".to_string(),
                path: first.to_string_lossy().into_owned(),
            }));
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.cache_enabled = true;
            st.picker_search = stub_search(calls.clone(), Ok(vec![office_series()]));
            st.prefetch_now_playing();
            drive_preload(&mut st);
            assert!(st.preload.ready);
            assert_eq!(calls.lock().unwrap().len(), 1);
            // The next episode: a different path, the same work.
            let next = season.join("The Office S01E02.mkv");
            app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
                raw_title: "The Office S01E02.mkv - PotPlayer".to_string(),
                path: next.to_string_lossy().into_owned(),
            }));
            st.prefetch_now_playing();
            assert_eq!(
                calls.lock().unwrap().len(),
                1,
                "the next episode is a preload hit, not a search"
            );
            assert!(st.preload.ready);
        });
        app_support::config::set_live_now_playing(None);
        cache_off();
    }

    /// A disabled cache returns before marking the key: the first poll of a
    /// launch must not stop the later enabled poll from prefetching.
    #[test]
    fn a_disabled_cache_never_marks_the_key_done() {
        let _guard = arm_cache();
        app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
            raw_title: "The Office S01E01.mkv - PotPlayer".to_string(),
            path: "M:\\TV\\The Office\\The Office S01E01.mkv".to_string(),
        }));
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut st = UiState::default();
        st.cache_enabled = false;
        st.picker_search = stub_search(calls.clone(), Ok(vec![office_series()]));
        st.prefetch_now_playing();
        assert!(st.preload.key.is_empty(), "no key is claimed while disabled");
        assert!(calls.lock().unwrap().is_empty());
        // Enabled, the same call now prefetches.
        st.cache_enabled = true;
        st.prefetch_now_playing();
        drive_preload(&mut st);
        assert!(st.preload.ready);
        assert_eq!(calls.lock().unwrap().len(), 1);
        app_support::config::set_live_now_playing(None);
        cache_off();
    }

    /// An empty preload leaves the list closed and clears the cancel, so the
    /// next poll retries instead of hanging on a dead key.
    #[test]
    fn an_empty_preload_leaves_the_list_closed_and_can_retry() {
        let _guard = arm_cache();
        crate::test_env::EnvGuard::with_scratch("picker-prefetch-empty", |dir| {
            let season = dir.join("TV").join("Show").join("Season 1");
            std::fs::create_dir_all(&season).unwrap();
            std::fs::write(season.join("Show S01E01.mkv"), "x").unwrap();
            app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
                raw_title: "Show S01E01.mkv - PotPlayer".to_string(),
                path: season.join("Show S01E01.mkv").to_string_lossy().into_owned(),
            }));
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.cache_enabled = true;
            st.picker_search = stub_search(calls.clone(), Ok(Vec::new()));
            st.prefetch_now_playing();
            drive_preload(&mut st);
            assert!(!st.preload.ready, "an empty prefetch is not ready");
            assert!(st.preload_cancel.is_none(), "the cancel is cleared");
            st.open_card();
            assert!(
                !st.picker.open,
                "an empty prefetch never opens the list for a manual-only search"
            );
            // The next poll retries the same key instead of early-returning.
            st.prefetch_now_playing();
            drive_preload(&mut st);
            assert_eq!(calls.lock().unwrap().len(), 2, "a dead key retries");
        });
        app_support::config::set_live_now_playing(None);
        cache_off();
    }

    /// A changed work cancels the old preload and drops the stale query, so
    /// priming seeds the new title.
    #[test]
    fn a_changed_work_clears_the_stale_query_and_preloads_the_new_title() {
        let _guard = arm_cache();
        crate::test_env::EnvGuard::with_scratch("picker-prefetch-change", |dir| {
            let season = dir.join("TV").join("The Office").join("Season 1");
            std::fs::create_dir_all(&season).unwrap();
            let office = season.join("The Office S01E01.mkv");
            std::fs::write(&office, "x").unwrap();
            app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
                raw_title: "The Office S01E01.mkv - PotPlayer".to_string(),
                path: office.to_string_lossy().into_owned(),
            }));
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.cache_enabled = true;
            st.picker_search = stub_search(
                calls.clone(),
                Ok(vec![office_series(), dune_movie()]),
            );
            st.prefetch_now_playing();
            drive_preload(&mut st);
            assert!(st.preload.ready);
            st.picker.set_query("The Office".to_string());
            // A different work now plays: a movie in a Films folder.
            let films = dir.join("Films");
            std::fs::create_dir_all(&films).unwrap();
            let dune = films.join("Dune Part Two 2024.mkv");
            std::fs::write(&dune, "x").unwrap();
            app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
                raw_title: "Dune Part Two 2024.mkv - PotPlayer".to_string(),
                path: dune.to_string_lossy().into_owned(),
            }));
            st.prefetch_now_playing();
            assert!(
                st.picker.query.is_empty(),
                "the previous work's query is dropped"
            );
            drive_preload(&mut st);
            st.open_card();
            assert_eq!(
                st.picker.filter,
                crate::search::KindFilter::All,
                "universal search: the card always runs both chains"
            );
            assert!(st.picker.open);
            assert_eq!(st.picker.candidates[0].kind, Kind::MOVIE);
        });
        app_support::config::set_live_now_playing(None);
        cache_off();
    }

    /// A path change is a re-latch even when the work is unchanged: the next
    /// episode keeps the show identity (one preload key, one search) and
    /// re-seeds the card from its own file instead of staying consumed.
    #[test]
    fn the_next_episode_keeps_the_show_and_re_latches_its_own_file() {
        let _guard = arm_cache();
        crate::test_env::EnvGuard::with_scratch("picker-next-episode", |dir| {
            let season = dir.join("TV").join("The Office").join("Season 1");
            std::fs::create_dir_all(&season).unwrap();
            let e1 = season.join("The Office S01E01.mkv");
            let e2 = season.join("The Office S01E02.mkv");
            std::fs::write(&e1, "x").unwrap();
            std::fs::write(&e2, "x").unwrap();
            let e1 = e1.to_string_lossy().into_owned();
            let e2 = e2.to_string_lossy().into_owned();
            let playing = |path: &str, name: &str| {
                app_support::config::set_live_now_playing(Some(app_support::config::NowPlaying {
                    raw_title: format!("{name} - PotPlayer"),
                    path: path.to_string(),
                }));
            };
            playing(&e1, "The Office S01E01.mkv");
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut st = UiState::default();
            st.cur_view = View::Card;
            st.cache_enabled = true;
            st.picker_search = stub_search(calls.clone(), Ok(vec![office_series()]));
            st.prefetch_now_playing();
            drive_preload(&mut st);
            assert!(st.preload.ready);
            let key = st.preload.key.clone();
            st.open_card();
            assert_eq!(st.picker.query, "The Office");
            assert_eq!(st.picker_path, e1);

            // A query typed for episode one belongs to episode one: the switch
            // re-seeds from the new file even though the user touched the field.
            st.picker.set_query("The Office typed".to_string());
            // The next episode: the same show, a different file.
            playing(&e2, "The Office S01E02.mkv");
            st.prefetch_now_playing();
            assert_eq!(st.preload.key, key, "the show identity is kept");
            assert_eq!(st.preload.path, e2, "the preload follows the new file");
            assert_eq!(st.picker_path, e2, "the card re-latches the new file");
            assert_eq!(
                st.picker.query, "The Office",
                "a switch re-seeds even a typed query"
            );
            assert!(
                calls.lock().unwrap().len() <= 1,
                "the same work is never searched twice for one episode step"
            );
        });
        app_support::config::set_live_now_playing(None);
        cache_off();
    }

    /// The Fix match dialog seeds from the playing file when the card's query
    /// belongs to a different one.
    #[test]
    fn fix_match_seeds_from_the_playing_file_on_a_switch() {
        let raw = "Severance.S01E02.1080p.WEBRip.DS4K.AV1.DDP.5.1-dAV1nci.mkv - PotPlayer";
        let path = format!(
            "{}:{}",
            'D',
            r"\1MOVIES\Severance S01\Severance.S01E02.1080p.WEBRip.DS4K.AV1.DDP.5.1-dAV1nci.mkv"
        );
        assert_eq!(
            fix_match_seed_query("Severance 2022", true, raw, &path),
            "Severance 2022",
            "the card's own query for THIS file is kept"
        );
        assert_eq!(
            fix_match_seed_query("The Office", false, raw, &path),
            "Severance",
            "a query for another file never survives the switch"
        );
        assert_eq!(
            fix_match_seed_query(
                "The Office",
                false,
                "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
                &format!("{}:{}", 'D', r"\Films\Dune.Part.Two.2024.1080p.mkv")
            ),
            "Dune Part Two (2024)",
            "the new file seeds itself, year included"
        );
    }

    // -- One-table click proof (owner-reported dead Settings/Card rows). -----
    //
    // Every test below takes its rect from the same layout the wndproc
    // hit-tests, dispatches DOWN+UP at that rect's centre through the window's
    // own click entry point (`click_layout`/`click_card_layout` ->
    // `activate_focused` -> `do_settings_action`/`do_card_action`), and
    // asserts the effect. A control that paints but has no effect fails here.

    fn settings_click_state() -> UiState {
        let mut st = UiState::default();
        st.cur_view = View::Settings;
        st.scan_dirs = Arc::new(Vec::new);
        st.arm_timer = Arc::new(|_| {});
        st.kill_timer = Arc::new(|_| {});
        st.scan_run = Arc::new(|_, _| Ok(Stats::default()));
        st
    }

    fn click_settings_id(st: &mut UiState, l: &SettingsLayout, id: i32) -> Option<BlockingUi> {
        let rc = l
            .settings_btn(id)
            .unwrap_or_else(|| panic!("settings id {id} missing from layout"));
        assert!(
            rc.right > rc.left && rc.bottom > rc.top,
            "settings id {id} has no hit rect"
        );
        // Scroll the target into view through the shared offset first: a row
        // below the fold is reachable by scrolling, never dead on arrival.
        let need = (rc.bottom - l.status.top).max(0);
        if need > 0 {
            assert!(
                st.scroll_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96, need),
                "settings id {id} must scroll into view"
            );
        }
        let l2 = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        let rc2 = l2
            .settings_btn(id)
            .unwrap_or_else(|| panic!("settings id {id} missing after scroll"));
        let (x, y) = ((rc2.left + rc2.right) / 2, (rc2.top + rc2.bottom) / 2);
        assert_eq!(
            button_at_point_above_status(&l2.btns, l2.status.top, x, y),
            id,
            "settings id {id} centre misses its own table"
        );
        st.click_layout(&l2, x, y)
    }

    fn card_click_id(st: &mut UiState, l: &CardLayout, id: i32) -> Option<BlockingUi> {
        let rc = l
            .card_btn(id)
            .unwrap_or_else(|| panic!("card id {id} missing from layout"));
        assert!(
            rc.right > rc.left && rc.bottom > rc.top,
            "card id {id} has no hit rect"
        );
        let (x, y) = ((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2);
        assert_eq!(
            button_at_point(&l.btns, x, y),
            id,
            "card id {id} centre misses its own table"
        );
        st.click_card_layout(l, x, y)
    }

    #[test]
    fn media_folder_controls_are_gone_from_settings() {
        // Owner order 2026-11-03: Potflix never scans a library, so the whole
        // MEDIA FOLDERS group (add/scan/refresh/remove/report) is absent from
        // the layout table, not merely hidden.
        let st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        for id in [BTN_ADD, BTN_SCAN, BTN_REFRESH, BTN_REMOVE, BTN_REPORT] {
            assert!(
                l.btns.iter().all(|b| b.id != id),
                "settings still lays out folder control {id}"
            );
        }
        assert!(
            l.labels.iter().all(|t| t.id != "settings.media"),
            "settings still paints the MEDIA FOLDERS caption"
        );
    }

    #[test]
    fn click_settings_discord_flips() {
        let mut st = settings_click_state();
        let before = st.discord_enabled;
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(click_settings_id(&mut st, &l, BTN_DISCORD), None);
        assert_eq!(st.discord_enabled, !before);
    }

    #[test]
    fn click_settings_close_to_tray_flips() {
        let mut st = settings_click_state();
        let before = st.close_to_tray;
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(click_settings_id(&mut st, &l, BTN_CLOSE), None);
        assert_eq!(st.close_to_tray, !before);
    }

    /// Owner order 2026-11-03: the defaults are the product. Metadata cache,
    /// Card layout, the Discord field toggles, the line-2 presets, Clear
    /// index, Export, Import, Open log and Scan speed are gone from Settings
    /// outright -- not hidden. The layout table, the click route and the
    /// focus ring carry none of them.
    #[test]
    fn removed_settings_controls_are_gone_from_settings() {
        let st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        for id in [
            BTN_CLEAR_INDEX,
            BTN_EXPORT,
            BTN_IMPORT,
            BTN_OPEN_LOG,
            BTN_SCAN_GENTLE,
            BTN_SCAN_BALANCED,
            BTN_SCAN_TURBO,
        ] {
            assert!(
                l.btns.iter().all(|b| b.id != id),
                "settings still lays out removed control {id}"
            );
            assert!(
                l.settings_btn(id).is_none(),
                "settings still hit-tests removed control {id}"
            );
        }
        for id in [BTN_FIELD_DETAILS, BTN_FIELD_STATE, BTN_FIELD_SYNOPSIS, BTN_FIELD_TIMERS] {
            assert!(
                l.btns.iter().all(|b| b.id != id),
                "settings still lays out removed field toggle {id}"
            );
        }
        for i in 0..4 {
            assert!(
                l.btns.iter().all(|b| b.id != card_state_button(i)),
                "settings still lays out removed preset segment {i}"
            );
        }
        assert!(
            l.labels.iter().all(|t| t.id != "settings.card"),
            "settings still paints the CARD caption"
        );
        assert!(
            l.labels.iter().all(|t| t.id != "settings.scan"),
            "settings still paints the SCAN SPEED caption"
        );
        // And the focus ring reaches exactly the surviving rows.
        assert_eq!(
            settings_focus_order(),
            vec![
                BTN_CARD,
                BTN_DISCORD,
                BTN_CLOSE,
                BTN_STARTUP,
                BTN_THEME_AMOLED,
                BTN_THEME_DARK,
                BTN_CLEAR_CACHE,
            ]
        );
    }

    #[test]
    fn click_settings_startup_flips() {
        let mut st = settings_click_state();
        let cell = Arc::new(Mutex::new(false));
        let get_cell = cell.clone();
        st.startup_set = Arc::new(move |v| {
            *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = v;
            Ok(())
        });
        st.startup_get = Arc::new(move || {
            *get_cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        st.test_config_path = Some(std::env::temp_dir().join(format!(
            "potflix-startup-{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        )));
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(click_settings_id(&mut st, &l, BTN_STARTUP), None);
        assert!(st.startup_state, "startup flips on");
        let _ = std::fs::remove_file(st.test_config_path.clone().unwrap_or_default());
    }

    #[test]
    fn click_settings_theme_dark_persists_and_back() {
        let scratch = Scratch::new("click-theme");
        let cfg = scratch.join("config.json");
        let mut st = settings_click_state();
        st.test_config_path = Some(cfg.clone());
        st.theme_state = app_support::config::THEME_AMOLED.to_string();
        set_live_theme(app_support::config::THEME_AMOLED);
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(click_settings_id(&mut st, &l, BTN_THEME_DARK), None);
        assert_eq!(st.theme_state, app_support::config::THEME_DARK);
        assert_eq!(live_theme(), app_support::config::THEME_DARK);
        let saved = app_support::config::load_config_from(&cfg).unwrap_or_default();
        assert_eq!(
            saved.effective_theme(),
            app_support::config::THEME_DARK,
            "theme persists"
        );
        let l2 = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(click_settings_id(&mut st, &l2, BTN_THEME_AMOLED), None);
        assert_eq!(st.theme_state, app_support::config::THEME_AMOLED);
        set_live_theme(app_support::config::THEME_AMOLED);
    }

    /// Scan speed has no Settings control any more: the profile still persists
    /// and applies live through the settings core, but Settings offers no row
    /// that could rebuild the default.
    #[test]
    fn click_settings_scan_segments_are_gone() {
        let st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        for id in [BTN_SCAN_GENTLE, BTN_SCAN_BALANCED, BTN_SCAN_TURBO] {
            assert!(
                l.btns.iter().all(|b| b.id != id),
                "settings still lays out scan segment {id}"
            );
            assert!(
                !settings_focus_order().contains(&id),
                "scan segment {id} is still a Tab stop"
            );
        }
    }

    #[test]
    fn corrupt_overrides_are_quarantined_and_latch_refuses() {
        let scratch = Scratch::new("overrides-quarantine");
        let path = scratch.join("overrides.json");
        let junk = "{not json";
        std::fs::write(&path, junk).unwrap();
        let err =
            metadata::overrides::load_from(&path).expect_err("corrupt store must error");
        assert!(err.to_string().contains("overrides"), "names the store: {err}");
        let backup =
            metadata::overrides::corrupt_backup_for(&path).expect("quarantine backup");
        assert_eq!(std::fs::read(&backup).unwrap(), junk.as_bytes());
        assert!(!path.exists(), "the bad file is moved aside");

        // Repeat loads never stack a second backup ("once").
        std::fs::write(&path, junk).unwrap();
        assert!(metadata::overrides::load_from(&path).is_err());
        let backups: Vec<String> = std::fs::read_dir(scratch.path())
            .unwrap()
            .filter_map(|entry| {
                entry
                    .ok()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
            })
            .filter(|name| name.starts_with("overrides.json.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1, "one quarantine: {backups:?}");

        // The latch policy: a load error is refused, never an empty store
        // whose next write would be a one-entry file over every manual match.
        let over = metadata::overrides::Override {
            scope: metadata::overrides::Scope::Movie,
            key: "d:\\films\\dune part two.mkv".to_string(),
            title: "Dune: Part Two".to_string(),
            year: 2024,
            provider: "cinemeta".to_string(),
            provider_id: "tt15239678".to_string(),
            poster: String::new(),
            media: None,
            episodes: Vec::new(),
            created_at_ms: 1_700_000_000_000,
        };
        let mut store = metadata::overrides::Store::empty_at(path.clone());
        let err = store
            .put_correction("d:\\films\\dune part two.mkv", over)
            .expect_err("a latch over corrupt bytes must refuse");
        assert!(
            err.to_string().contains("corrupt"),
            "the refusal names the cause: {err}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            junk.as_bytes(),
            "refusal leaves the corrupt bytes untouched"
        );
    }

    #[test]
    fn search_cache_puts_serialize_and_merge_under_lock() {
        use metadata::searchcache::SearchCache;
        use metadata_core::types::Kind;
        let scratch = Scratch::new("searchcache-merge");
        let path = scratch.join("search-cache.json");
        let mk = |id: &str| metadata::search::Candidate {
            kind: Kind::SERIES,
            title: format!("Show {id}"),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: id.to_string(),
            poster: String::new(),
            synopsis: String::new(),
            genres: String::new(),
            length: String::new(),
            episode_count: 0,
        };
        // Eight concurrent puts from eight separately loaded instances: every
        // key survives, none is lost to a racing load->insert->save.
        std::thread::scope(|scope| {
            for i in 0..8 {
                let path = &path;
                let mk = &mk;
                scope.spawn(move || {
                    let key = format!("show-{i}|2008|series");
                    let mut cache = SearchCache::load_from(path);
                    cache.put(&key, &[mk(&key)], &[]).expect("concurrent put");
                });
            }
        });
        let reloaded = SearchCache::load_from(&path);
        for i in 0..8 {
            let key = format!("show-{i}|2008|series");
            assert!(
                reloaded.get(&key).is_some(),
                "concurrent put of {key} survived"
            );
        }
        // Sequential saves over an existing file keep working (rename path).
        let mut cache = SearchCache::load_from(&path);
        cache
            .put("show-9|2008|series", &[mk("show-9")], &["kitsu".to_string()])
            .expect("overwrite save");
        assert!(SearchCache::load_from(&path).get("show-9|2008|series").is_some());
    }

    /// Covers are always on: the artwork toggle (70) is gone from Settings
    /// and from the click table, while the rest of the field toggles stay.
    #[test]
    fn artwork_toggle_is_gone_from_settings() {
        let st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert!(
            l.btns.iter().all(|b| b.id != 70),
            "no artwork row is laid out"
        );
        assert!(
            l.settings_btn(70).is_none(),
            "no artwork row is hit-testable"
        );
        assert!(
            presence::DesignerSpec::default().artwork,
            "covers stay on: nothing can flip the artwork field off"
        );
        presence::set_live_designer(presence::DesignerSpec::default());
    }

    /// Owner order 2026-11-03: the four Discord field toggles are gone from
    /// Settings outright. The designer's shipped default (every field on) is
    /// the product, so no control can rebuild it.
    #[test]
    fn click_settings_field_toggles_are_gone() {
        let st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        for id in [BTN_FIELD_DETAILS, BTN_FIELD_STATE, BTN_FIELD_SYNOPSIS, BTN_FIELD_TIMERS] {
            assert!(
                l.btns.iter().all(|b| b.id != id),
                "settings still lays out field toggle {id}"
            );
            assert!(
                l.settings_btn(id).is_none(),
                "settings still hit-tests field toggle {id}"
            );
            assert!(
                !settings_focus_order().contains(&id),
                "field toggle {id} is still a Tab stop"
            );
        }
        // The designer's shipped default stays identity: with no toggle able
        // to flip a field, the composed card is the card.
        presence::set_live_designer(presence::DesignerSpec::default());
    }

    #[test]
    fn click_settings_clear_metadata_routes() {
        let mut st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(
            click_settings_id(&mut st, &l, BTN_CLEAR_CACHE),
            Some(BlockingUi::ClearMetadata)
        );
    }

    /// Owner order 2026-11-03: Clear index, Export, Import and Open log have
    /// no Settings row any more. Clear metadata is the one surviving library
    /// action.
    #[test]
    fn click_settings_library_extras_are_gone() {
        let st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        for id in [BTN_CLEAR_INDEX, BTN_EXPORT, BTN_IMPORT, BTN_OPEN_LOG] {
            assert!(
                l.btns.iter().all(|b| b.id != id),
                "settings still lays out library control {id}"
            );
            assert!(
                l.settings_btn(id).is_none(),
                "settings still hit-tests library control {id}"
            );
            assert!(
                !settings_focus_order().contains(&id),
                "library control {id} is still a Tab stop"
            );
        }
        assert!(
            l.settings_btn(BTN_CLEAR_CACHE).is_some(),
            "Clear metadata survives"
        );
    }

    #[test]
    fn click_settings_gear_returns_to_card() {
        let mut st = settings_click_state();
        let l = st.layout_settings_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(click_settings_id(&mut st, &l, BTN_CARD), None);
        assert_eq!(st.cur_view, View::Card);
    }

    /// Universal search: the removed scope ids (85/82/81/87/88) are absent
    /// from the Card layout and change no filter — every query runs both
    /// chains under `All`.
    #[test]
    fn click_card_scope_ids_are_gone_and_change_no_filter() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.set_filter(crate::search::KindFilter::Movies);
        // The engine still accepts the scope internally, but no control can
        // select it: the layout carries no such button.
        assert_eq!(st.picker.filter, crate::search::KindFilter::Movies);
        st.picker.set_filter(crate::search::KindFilter::All);
        let l = st.layout_card_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        for id in [85, 82, 81, 87, 88] {
            assert!(
                l.card_btn(id).is_none(),
                "removed scope id {id} is laid out"
            );
        }
        // Clicking where a scope segment used to be is a no-op: the filter
        // stays universal.
        assert_eq!(st.picker.filter, crate::search::KindFilter::All);
    }

    #[test]
    fn click_card_result_selects_without_applying() {
        app_support::config::set_live_now_playing(None);
        let mut st = UiState::default();
        st.cur_view = View::Card;
        st.picker.candidates = vec![office_series(), dune_movie()];
        st.picker.open = true;
        st.picker.searched = true;
        st.picker.selected = 0;
        let l = st.layout_card_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        let id = BTN_PICKER_RESULT_AT + 1;
        let rc = l.card_btn(id).expect("second result missing");
        let (x, y) = ((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2);
        assert_eq!(button_at_point(&l.btns, x, y), id);
        assert_eq!(st.click_card_layout(&l, x, y), None);
        assert_eq!(st.picker.selected, 1, "row selects");
        assert!(st.has_picked_candidate(), "the click is a pick");
        assert!(st.picker.open, "selecting never applies without playback");
    }

    #[test]
    fn click_card_apply_without_pick_refuses() {
        app_support::config::set_live_now_playing(None);
        let mut st = UiState::default();
        st.cur_view = View::Card;
        // Open with a highlighted but never-picked row: Apply must refuse.
        st.picker.candidates = vec![office_series()];
        st.picker.open = true;
        st.picker.searched = true;
        st.picker.selected = 0;
        assert!(!st.has_picked_candidate());
        // Apply is pinned even without a pick (disabled look, live control).
        st.picker_path = String::new();
        let l = st.layout_card_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(card_click_id(&mut st, &l, BTN_PICKER_APPLY), None);
        assert_eq!(st.picker.status, "Pick a result row first.");
        assert!(st.picker.open, "refused Apply never latches");
    }

    /// Owner order 2026-11-03: the narrow Card carries no Change-filters
    /// control. `BTN_ADVANCED` toggles state the narrow layout never exposes,
    /// so the button is gone from the narrow table (the wide workspace keeps
    /// its own Advanced disclosure).
    #[test]
    fn click_card_change_filters_is_gone_from_narrow() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        let l = st.layout_card_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert!(
            l.card_btn(BTN_ADVANCED).is_none(),
            "narrow Card still lays out Change filters"
        );
        assert!(
            !card_focus_order().contains(&BTN_ADVANCED),
            "Change filters is still a narrow Tab stop"
        );
    }

    #[test]
    fn click_card_gear_opens_settings() {
        let mut st = UiState::default();
        st.cur_view = View::Card;
        let l = st.layout_card_for(MAIN_CLIENT_W, MAIN_CLIENT_H, 96);
        assert_eq!(card_click_id(&mut st, &l, BTN_SETTINGS), None);
        assert_eq!(st.cur_view, View::Settings);
    }

    /// The bar tells the truth: a disconnected link never renders the old
    /// "Discord connected" literal; a down link names the reason in one short
    /// muted label; a healthy link shows no chrome at all. The link half reads
    /// the same live mirror the publisher writes, never a constant.
    #[test]
    fn pill_names_the_down_link_never_a_healthy_one() {
        app_support::config::set_live_discord_up(false);
        let mut st = UiState::default();
        assert!(st.discord_enabled, "the shipped default leaves Discord on");
        let down = discord_bar_text(&st);
        assert_eq!(down, "Discord not running", "the down link names itself: {down:?}");
        assert!(
            !down.contains("connected"),
            "a down link must never claim a connection: {down:?}"
        );
        st.discord_enabled = false;
        assert_eq!(discord_bar_text(&st), "Discord off", "an off switch names itself");
        st.discord_enabled = true;
        app_support::config::set_live_discord_up(true);
        assert!(
            discord_bar_text(&st).is_empty(),
            "a healthy link shows no chrome"
        );
        app_support::config::set_live_discord_up(false);
    }

    #[test]
    fn focus_ring_is_muted_never_accent() {
        assert_eq!(STATE_FOCUS, rgb(0xA7, 0xAB, 0xB4), "focus is #A7ABB4");
        assert_eq!(STATE_FOCUS, COL_MUTED);
        assert_ne!(STATE_FOCUS, COL_ACCENT, "focus is never accent red");
        assert_ne!(STATE_FOCUS, COL_ACCENT_HOV);
    }
}
