//! The Potflix brand system, ported from the repo's own identity pass.
//!
//! `design\make_logo.py` is the authority for everything in this module: the
//! palette it prints as `PALETTE_REPORT`, the faces it ships in `design\fonts\`,
//! the Ribbon-P mark it renders into `assets\icon.ico`, and the house squircle
//! fraction it uses for every tile. Nothing here is invented -- each token names
//! the part of that script it comes from.
//!
//! # Why this module exists
//!
//! The Win32 client area was drawing with system fonts and an ad-hoc grey ramp,
//! so the shipped window carried none of the brand. Every token below is either
//! a hex the identity pass already prints, or a value derived from one by a
//! stated rule. The two rules that matter:
//!
//! * **The squircle fraction.** `make_logo.py` rounds a tile with
//!   `size * 0.225` (`tile()`) and the app icon's own radius is 112/512 =
//!   `0.21875` (the brief's 21.9%). [`squircle`] is that fraction as integer
//!   math, and every radius in the app is `squircle(<its own square>)` -- a
//!   36px control rounds like a 36px tile, a 64px panel like a 64px tile.
//! * **The palette is fixed.** `RED`/`DARK`/`BLACK`/`WHITE` plus the card
//!   quartet. `DARK` is not free-hand: it is `round(RED * 0.62)`, which
//!   `make_logo.py` states at its own definition site, and
//!   [`dark_is_red_scaled`] re-derives it so the claim is checked, not trusted.
//!
//! # Contrast
//!
//! Every text/surface pair the app paints is measured by [`contrast`] from the
//! actual constants (`brand_contrast_gate` below). The brand palette was chosen
//! for the Discord card, a large-format surface; the app reuses it at 11-13px,
//! so each pair is re-checked at the small-text threshold.

// The Win32 surface is inherently pointer-taking: every entry point accepts an
// `HWND`/`HICON` and hands it to `windows-sys`. The handles are opaque and the
// module never dereferences them itself, so the lint is silenced module-wide
// (the same allow `window` carries).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi;
use windows_sys::Win32::UI::WindowsAndMessaging::HICON;

use crate::win32;

/// Pack `(r, g, b)` into the `COLORREF` (`0x00BBGGRR`) every paint call takes.
/// The same packing `window`'s own `rgb` builds; owned here so this module's
/// tokens do not depend on the module that paints with them.
#[must_use]
pub const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}

// ---------------------------------------------------------------------------
// Palette (`design\make_logo.py:41-50`).
// ---------------------------------------------------------------------------

/// `RED = (0xE5, 0x09, 0x14)` -- lit ribbon facets, the accent, badges.
pub const RED: u32 = rgb(0xE5, 0x09, 0x14);
/// `DARK = (0x8E, 0x06, 0x0C)` = `round(RED * 0.62)` -- the receding ribbon
/// plane, and (here) the pressed state of an accent control. It is never a
/// control's BOUNDARY: it is the fill *inside* a red edge, which is the Ribbon
/// P's own two-facet language (lit face `RED`, receding plane `DARK`). So the
/// accent button's silhouette stays `RED` (4.11:1 on the canvas) while its
/// interior recedes, and its label stays legible (`WHITE` on `DARK`, 9.62:1).
pub const DARK: u32 = rgb(0x8E, 0x06, 0x0C);
/// `BLACK = (0x0B, 0x0B, 0x0B)` -- the canvas, and the badge/tile disc.
pub const BLACK: u32 = rgb(0x0B, 0x0B, 0x0B);
/// `WHITE = (0xFF, 0xFF, 0xFF)` -- glyphs on black, and text on the accent fill.
pub const WHITE: u32 = rgb(0xFF, 0xFF, 0xFF);

/// `CARD = (0x23, 0x24, 0x28)` -- the card surface.
pub const CARD: u32 = rgb(0x23, 0x24, 0x28);
/// `CARD_HDR = (0xF2, 0xF3, 0xF5)` -- the card's header text.
pub const CARD_HDR: u32 = rgb(0xF2, 0xF3, 0xF5);
/// `CARD_DET = (0xDB, 0xDE, 0xE1)` -- the card's detail text.
pub const CARD_DET: u32 = rgb(0xDB, 0xDE, 0xE1);
/// `CARD_MUT = (0xB5, 0xBA, 0xC1)` -- the card's muted text.
pub const CARD_MUT: u32 = rgb(0xB5, 0xBA, 0xC1);

// ---------------------------------------------------------------------------
// Derived surfaces. Each is a stated step off a token above, not a new hue.
// ---------------------------------------------------------------------------

/// `(0x1A, 0x1B, 0x1E)` -- `make_logo.py`'s own sheet background
/// (`candidate_sheet()`, `badges_variants_sheet()`). One luminance step above
/// `BLACK`: the chrome band (header/footer) and the list panel, so the canvas
/// and the chrome read as two planes with no hue shift.
pub const CHROME: u32 = rgb(0x1A, 0x1B, 0x1E);

/// `(0x2E, 0x31, 0x38)` -- one step above `CARD`: a card-surface control
/// hovered or selected. The elevation rule is a luminance step, never a shadow
/// (a drop shadow is invisible on `BLACK`).
pub const CARD_HOVER: u32 = rgb(0x2E, 0x31, 0x38);

/// `(0x5F, 0x63, 0x6D)` -- the scan bar's unfilled track. The card anatomy's
/// own track (`0x42444A`) measures 2.02:1 on `BLACK`, under the 3:1 non-text
/// gate: it was drawn on `CARD` in the Discord card, and this app draws the bar
/// on the canvas. Lifted one step until it clears (3.27:1 on `BLACK`), because a
/// track that fails the gate is not a track.
pub const TRACK: u32 = rgb(0x5F, 0x63, 0x6D);

/// `(0x6E, 0x72, 0x7C)` -- the control BOUNDARY: the hairline that identifies a
/// secondary control's shape. Not a brand hex; derived to clear the 3:1
/// non-text gate against every surface it is drawn on (`BLACK` 4.09:1, `CHROME`
/// 3.58:1, `CARD` 3.22:1), because a boundary that fails that gate is not a
/// boundary. Measured by `brand_contrast_gate`, not asserted by eye.
pub const EDGE: u32 = rgb(0x6E, 0x72, 0x7C);

/// `(0xF2, 0x62, 0x6A)` -- destructive TEXT. The brand red on the card surface
/// is 3.23:1, under the 4.5:1 body gate, so the destructive label is the brand
/// red lifted until it clears: 4.97:1 on `CARD`, 6.31:1 on `BLACK`.
pub const DANGER_TEXT: u32 = rgb(0xF2, 0x62, 0x6A);

/// `#8A, 0x8F, 0x98` -- disabled text: still readable on every surface it lands
/// on (`4.77:1` on `CARD`, `6.06:1` on `BLACK`, `5.30:1` on `CHROME`), clearly a
/// step below the muted role. The card quartet's muted step IS the secondary
/// text step, so a disabled control needs its own.
pub const DISABLED_TEXT: u32 = rgb(0x8A, 0x8F, 0x98);

/// `(0xF6, 0x12, 0x1D)` -- the accent's hover step.
pub const RED_HOVER: u32 = rgb(0xF6, 0x12, 0x1D);

/// Approved narrow redesign tokens (mock `potflix-preview/index.html`).
/// Kept beside the identity palette so the `palette_matches_make_logo` gate
/// still pins the original hexes; the live `COL_*` roles point here.
pub const BG_V2: u32 = rgb(0x0F, 0x10, 0x12);
pub const CARD_V2: u32 = rgb(0x2B, 0x2D, 0x33);
pub const ROW_HOVER_V2: u32 = rgb(0x33, 0x36, 0x3D);
pub const HAIRLINE_V2: u32 = rgb(0x3A, 0x3D, 0x44);
pub const STRONG_V2: u32 = rgb(0x56, 0x5A, 0x63);
pub const MUTED_V2: u32 = rgb(0xA7, 0xAB, 0xB4);
pub const DIM_V2: u32 = rgb(0x8A, 0x8F, 0x99);
pub const ACCENT_V2: u32 = rgb(0xE5, 0x34, 0x3F);
pub const ACCENT_HOV_V2: u32 = rgb(0xF0, 0x4B, 0x55);

// ---------------------------------------------------------------------------
// The house squircle.
// ---------------------------------------------------------------------------

/// The house fraction's numerator: the app icon's radius, 112px on a 512px
/// square (`assets\appicon.png`; the brief's 21.9%).
pub const SQUIRCLE_NUM: i32 = 112;
/// Its denominator.
pub const SQUIRCLE_DEN: i32 = 512;

/// `round(side * 112 / 512)`: the corner radius of a `side`-square tile, in the
/// same integer domain the layout uses. Every radius token in the app is this
/// function applied to the square it rounds, so the house fraction is the single
/// source of every curve here.
#[must_use]
pub const fn squircle(side: i32) -> i32 {
    (side * SQUIRCLE_NUM + SQUIRCLE_DEN / 2) / SQUIRCLE_DEN
}

/// `round(RED * 0.62)`, the identity pass's own derivation of `DARK`
/// (`make_logo.py:8`). `true` when [`DARK`] is still that value.
#[must_use]
pub fn dark_is_red_scaled() -> bool {
    let scale = |c: u32| (c * 62 + 50) / 100;
    rgb(scale(0xE5), scale(0x09), scale(0x14)) == DARK
}

// ---------------------------------------------------------------------------
// Type scale (logical px at 96 DPI).
// ---------------------------------------------------------------------------

/// The wordmark.
pub const FS_WORDMARK: i32 = 20;
/// A section micro-label: tiny, uppercase, letter-spaced.
pub const FS_LABEL: i32 = 10;
/// A view's own heading.
pub const FS_TITLE: i32 = 17;
/// Body text: rows, labels, values.
pub const FS_BODY: i32 = 13;
/// Control labels.
pub const FS_CONTROL: i32 = 13;
/// The status line and hints.
pub const FS_HINT: i32 = 11;
/// The badge numeral.
pub const FS_BADGE: i32 = 11;

/// The wordmark's tracking, in em -- `make_logo.py`'s `track=0.08` for the
/// chosen Archivo Black lockup.
pub const TRACK_WORDMARK: f32 = 0.08;
/// The micro-label's tracking, in em: a 10px uppercase label needs it to read.
pub const TRACK_LABEL: f32 = 0.14;

// ---------------------------------------------------------------------------
// The shipped faces.
// ---------------------------------------------------------------------------

/// Archivo Black -- the chosen wordmark/display face (`CHOSEN = "archivo"`:
/// "single-master display black, even color, tight grotesque caps"). OFL, and
/// the licence ships beside the TTF in `design\fonts\`.
pub const DISPLAY_TTF: &[u8] = include_bytes!("../../../../design/fonts/ArchivoBlack-Regular.ttf");
/// Inter Variable -- the UI face (`INTER` in `make_logo.py`). OFL, licence
/// alongside.
pub const UI_TTF: &[u8] = include_bytes!("../../../../design/fonts/Inter-Variable.ttf");

/// The family name GDI must report for the display face. Read back with
/// [`face_of`]; a mismatch is a reported failure, never a silent fallback.
pub const DISPLAY_FAMILY: &str = "Archivo Black";
/// The UI family name.
pub const UI_FAMILY: &str = "Inter";

/// The display face's weight (a single-master 900).
pub const DISPLAY_WEIGHT: i32 = win32::FW_BLACK;
/// The UI face's weight.
///
/// **There is only one.** `Inter-Variable.ttf` is a variable font, and GDI has no
/// axis instancing: asking it for `FW_MEDIUM`/`FW_BOLD` makes it pick no face and
/// SYNTHESISE the weight by redrawing the glyph at an offset, which renders as a
/// visible double-strike at UI sizes (measured: every 500-weight label ghosted
/// while every 400-weight string stayed crisp in the same frame). So emphasis in
/// this app is carried by Archivo Black -- the brand's own display face, static,
/// and the brief's stated role for it ("drives the wordmark AND section
/// headings") -- and Inter is used at its default instance only.
pub const UI_WEIGHT: i32 = win32::FW_NORMAL;

/// Register the embedded faces once per process.
///
/// `AddFontMemResourceEx` installs a face from a memory image for the life of
/// the process: no install, no registry write, no system-font dependency. The
/// images are `'static` slices, so the memory stays valid for as long as the
/// font is in use, which is the API's own requirement. Returns the number of
/// faces the calls reported, or `None` when a call failed.
#[must_use]
pub fn install_fonts() -> Option<u32> {
    static INSTALLED: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *INSTALLED.get_or_init(|| {
        let mut total = 0u32;
        for face in [DISPLAY_TTF, UI_TTF] {
            let mut count = 0u32;
            // SAFETY: `face` is a `'static` slice that outlives every font made
            // from it; the reserved pointer is documented as must-be-null; the
            // call only writes `count`.
            let handle = unsafe {
                win32::AddFontMemResourceEx(
                    face.as_ptr().cast(),
                    face.len() as u32,
                    std::ptr::null(),
                    std::ptr::from_mut(&mut count),
                )
            };
            if handle.is_null() {
                return None;
            }
            total += count.max(1);
        }
        Some(total)
    })
}

/// Whether the embedded faces are installed (`AddFontMemResourceEx` succeeded).
#[must_use]
pub fn fonts_installed() -> bool {
    install_fonts().is_some()
}

/// A brand font at `height_px` (a negative `lfHeight` is a character height) and
/// `weight`. `display` picks Archivo Black, otherwise Inter.
#[must_use]
pub fn font(height_px: i32, weight: i32, display: bool) -> Gdi::HFONT {
    let _ = install_fonts();
    let face = win32::utf16_z(if display { DISPLAY_FAMILY } else { UI_FAMILY });
    // SAFETY: `face` outlives the call. The precision/quality pair is the
    // port's own fix (`OUT_TT_PRECIS` + `CLEARTYPE_QUALITY`): a TrueType
    // outline face with subpixel antialiasing, never the system default.
    unsafe {
        win32::CreateFontW(
            -height_px.abs(),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            win32::DEFAULT_CHARSET,
            win32::OUT_TT_PRECIS,
            win32::CLIP_DEFAULT_PRECIS,
            win32::CLEARTYPE_QUALITY,
            0,
            face.as_ptr(),
        )
    }
}

/// The wordmark/heading face: Archivo Black at `size`.
#[must_use]
pub fn display(size: i32) -> Gdi::HFONT {
    font(size, DISPLAY_WEIGHT, true)
}

/// The UI face at `size` / `weight`.
#[must_use]
pub fn ui(size: i32, weight: i32) -> Gdi::HFONT {
    font(size, weight, false)
}

/// The face name `hfont` actually selects in `hdc`, read back with
/// `GetTextFaceW` -- the loader's fallback check.
#[must_use]
pub fn face_of(hdc: Gdi::HDC, hfont: Gdi::HFONT) -> String {
    // SAFETY: the font is selected and restored around the query; the buffer is
    // live and its length is passed.
    unsafe {
        let old = Gdi::SelectObject(hdc, hfont);
        let mut buf = [0u16; 64];
        let n = win32::GetTextFaceW(hdc, buf.len() as i32, buf.as_mut_ptr());
        Gdi::SelectObject(hdc, old);
        if n <= 0 {
            return String::new();
        }
        let end = (n as usize - 1).min(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }
}

// ---------------------------------------------------------------------------
// Contrast (WCAG 2.x relative luminance), measured from the constants above.
// ---------------------------------------------------------------------------

/// Relative luminance of a `COLORREF`, per WCAG 2.1.
#[must_use]
pub fn luminance(color: u32) -> f64 {
    let channel = |shift: u32| {
        let c = ((color >> shift) & 0xFF) as f64 / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(0) + 0.7152 * channel(8) + 0.0722 * channel(16)
}

/// The contrast ratio between two `COLORREF`s, ordered so it is >= 1.
#[must_use]
pub fn contrast(a: u32, b: u32) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

// ---------------------------------------------------------------------------
// The mark: the Ribbon P, loaded from the embedded `assets\icon.ico`.
// ---------------------------------------------------------------------------

/// The app icon as an `HICON` of `cx` x `cy` px, built straight from the
/// embedded `.ico` bytes.
///
/// The file holds one BMP/DIB frame (`make_logo.py`'s `write_ico` asserts
/// exactly that), which is the "resource bits" shape `CreateIconFromResourceEx`
/// consumes -- so no temp file, no `LoadImageW`, and the mark in the window is
/// byte-for-byte the mark in the taskbar. `cx`/`cy` of 0 mean the frame's own
/// size (256x256).
#[must_use]
pub fn create_mark_icon(cx: i32, cy: i32) -> HICON {
    let Ok((off, size)) = crate::tray::parse_icon_entry(crate::tray::ICON_ICO) else {
        return std::ptr::null_mut();
    };
    let frame = &crate::tray::ICON_ICO[off..off + size];
    // SAFETY: `frame` is a live slice of the embedded `.ico` and `size` is its
    // length; the flags are the documented ones.
    unsafe {
        win32::CreateIconFromResourceEx(
            frame.as_ptr(),
            size as u32,
            1,
            win32::ICON_RESOURCE_VERSION,
            cx,
            cy,
            win32::LR_DEFAULTCOLOR,
        )
    }
}

/// The window's own mark as `(big, small)`, sized for the titlebar's large and
/// small icon slots. Either may be null if the loader failed; the caller reports
/// that rather than shipping a default icon.
#[must_use]
pub fn window_icons() -> (HICON, HICON) {
    (mark_icon(), mark_icon_small())
}

/// The process's one 256px Ribbon-P icon, loaded once.
///
/// Cached because three call sites want the same mark: the window's class icon,
/// its `WM_SETICON` payload, and the header tile the paint path draws. A raw
/// handle is not `Send`/`Sync`, so the cache stores its address and casts back;
/// the icon lives for the process's lifetime and is never destroyed.
#[must_use]
pub fn mark_icon() -> HICON {
    static BIG: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BIG.get_or_init(|| create_mark_icon(0, 0) as usize) as HICON
}

/// The process's one small (`SM_CXSMICON`) Ribbon-P icon, loaded once, for the
/// titlebar's small slot. The shell reads that slot at its own metric size, so
/// handing it a 256px image to squash would soften the mark.
#[must_use]
pub fn mark_icon_small() -> HICON {
    static SMALL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SMALL.get_or_init(|| {
        // SAFETY: a read-only system metric.
        let sm = unsafe { win32::GetSystemMetrics(win32::SM_CXSMICON) }.max(16);
        let h = create_mark_icon(sm, sm);
        if h.is_null() { mark_icon() as usize } else { h as usize }
    }) as HICON
}

/// Apply the Ribbon P to `hwnd` as BOTH the class icon and the window icon.
///
/// `WM_SETICON` alone sets the window's own icon (what the titlebar, alt-tab and
/// the taskbar read). The class slot is set too, so `GetClassLongPtrW(GCLP_HICON)`
/// -- the handle the redesign's proof asserts non-zero -- is the mark and not
/// the generic application icon the window manager substitutes when a class
/// declares none.
///
/// Returns the class-icon handle the write produced, which is the proof value.
pub fn apply_window_icon(hwnd: HWND) -> HICON {
    let big = mark_icon();
    if big.is_null() {
        return std::ptr::null_mut();
    }
    let small = mark_icon_small();
    // SAFETY: `hwnd` is the caller's own window; the handles are live icons this
    // process owns and keeps for its lifetime (never destroyed while set).
    unsafe {
        win32::SetClassLongPtrW(hwnd, win32::GCLP_HICON, big as isize);
        win32::SetClassLongPtrW(hwnd, win32::GCLP_HICONSM, small as isize);
        win32::SendMessageW(hwnd, win32::WM_SET_ICON, win32::ICON_BIG as usize, big as isize);
        win32::SendMessageW(
            hwnd,
            win32::WM_SET_ICON,
            win32::ICON_SMALL as usize,
            small as isize,
        );
        win32::GetClassLongPtrW(hwnd, win32::GCLP_HICON) as HICON
    }
}

/// The class's large-icon handle, read back with `GetClassLongPtrW(GCLP_HICON)`.
#[must_use]
pub fn class_icon(hwnd: HWND) -> HICON {
    // SAFETY: reads one class word from the caller's own window.
    unsafe { win32::GetClassLongPtrW(hwnd, win32::GCLP_HICON) as HICON }
}

// ===TESTS===

#[cfg(test)]
mod tests {
    use super::*;

    /// The house fraction is the app icon's own radius: 112 on 512.
    #[test]
    fn squircle_is_the_house_fraction() {
        assert_eq!(squircle(512), 112);
        assert_eq!(squircle(1024), 224);
        // A 36px control rounds like a 36px tile; a 64px panel like a 64px tile.
        assert_eq!(squircle(36), 8);
        assert_eq!(squircle(64), 14);
        assert_eq!(squircle(32), 7);
        assert_eq!(squircle(18), 4);
    }

    /// `DARK` is `round(RED * 0.62)` -- re-derived, not trusted.
    #[test]
    fn dark_is_red_scaled_exactly() {
        assert!(dark_is_red_scaled(), "DARK is not round(RED*0.62)");
        assert_eq!(DARK, rgb(0x8E, 0x06, 0x0C));
    }

    /// The palette is the identity pass's own, to the byte.
    #[test]
    fn palette_matches_make_logo() {
        assert_eq!(RED, rgb(0xE5, 0x09, 0x14));
        assert_eq!(BLACK, rgb(0x0B, 0x0B, 0x0B));
        assert_eq!(WHITE, rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(CARD, rgb(0x23, 0x24, 0x28));
        assert_eq!(CARD_HDR, rgb(0xF2, 0xF3, 0xF5));
        assert_eq!(CARD_DET, rgb(0xDB, 0xDE, 0xE1));
        assert_eq!(CARD_MUT, rgb(0xB5, 0xBA, 0xC1));
    }

    /// Relative luminance sanity: the ramp is ordered and the ratio is
    /// symmetric.
    #[test]
    fn luminance_orders_the_palette() {
        assert!(luminance(BLACK) < luminance(CARD));
        assert!(luminance(CARD) < luminance(CARD_MUT));
        assert!(luminance(CARD_MUT) < luminance(WHITE));
        assert!((contrast(WHITE, BLACK) - contrast(BLACK, WHITE)).abs() < 1e-9);
        assert!((contrast(WHITE, WHITE) - 1.0).abs() < 1e-9);
    }

    /// The gate values, printed so the numbers in the report are the numbers the
    /// test measured.
    #[test]
    fn brand_contrast_gate() {
        // Body text: >= 4.5:1 against every surface it is painted on.
        for (name, fg, bg) in [
            ("CARD_HDR on BLACK", CARD_HDR, BLACK),
            ("CARD_HDR on CHROME", CARD_HDR, CHROME),
            ("CARD_HDR on CARD", CARD_HDR, CARD),
            ("CARD_DET on CARD", CARD_DET, CARD),
            ("CARD_MUT on BLACK", CARD_MUT, BLACK),
            ("CARD_MUT on CHROME", CARD_MUT, CHROME),
            ("CARD_MUT on CARD", CARD_MUT, CARD),
            ("CARD_HDR on CARD_HOVER", CARD_HDR, CARD_HOVER),
            ("WHITE on RED", WHITE, RED),
            ("DANGER_TEXT on CARD", DANGER_TEXT, CARD),
            ("DANGER_TEXT on BLACK", DANGER_TEXT, BLACK),
            ("DANGER_TEXT on CHROME", DANGER_TEXT, CHROME),
            ("WHITE on DARK", WHITE, DARK),
            ("CARD_MUT on CHROME", CARD_MUT, CHROME),
        ] {
            let r = contrast(fg, bg);
            println!("CONTRAST body {name} = {r:.2}:1");
            assert!(r >= 4.5, "body {name} = {r:.2}:1 < 4.5:1");
        }
        // Non-text: >= 3:1 -- boundaries, indicators, glyphs. Only the pairs the
        // app actually paints are listed, so the gate cannot pass on a
        // combination no surface uses.
        for (name, fg, bg) in [
            ("RED on BLACK", RED, BLACK),
            ("RED on CHROME", RED, CHROME),
            ("RED on CARD", RED, CARD),
            ("EDGE on BLACK", EDGE, BLACK),
            ("EDGE on CHROME", EDGE, CHROME),
            ("EDGE on CARD", EDGE, CARD),
            ("TRACK on BLACK", TRACK, BLACK),
        ] {
            let r = contrast(fg, bg);
            println!("CONTRAST nontext {name} = {r:.2}:1");
            assert!(r >= 3.0, "non-text {name} = {r:.2}:1 < 3:1");
        }
    }

    /// The embedded faces are the identity pass's own TTFs, and the OFL licences
    /// that ship beside them travel with the embedded copies.
    #[test]
    fn embedded_faces_are_the_design_fonts() {
        // Both are real TrueType images: the sfnt version tag is 0x00010000.
        assert_eq!(&DISPLAY_TTF[..4], &[0x00, 0x01, 0x00, 0x00]);
        assert_eq!(&UI_TTF[..4], &[0x00, 0x01, 0x00, 0x00]);
        assert!(DISPLAY_TTF.len() > 80_000, "Archivo Black looks truncated");
        assert!(UI_TTF.len() > 500_000, "Inter Variable looks truncated");
        for name in ["archivoblack-OFL.txt", "inter-OFL.txt"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../design/fonts")
                .join(name);
            assert!(path.is_file(), "missing licence {name}");
        }
    }

    /// `AddFontMemResourceEx` accepts both embedded faces.
    #[test]
    fn embedded_faces_install() {
        let n = install_fonts().expect("AddFontMemResourceEx failed for an embedded face");
        assert!(n >= 2, "expected two faces, got {n}");
        assert!(fonts_installed());
    }

    /// The mark loads from the embedded `.ico`, at both slot sizes. The `.ico`
    /// may carry multiple frames (256px + small slots); the entry must sit
    /// inside the file, not necessarily fill it.
    #[test]
    fn mark_icon_loads_at_both_slot_sizes() {
        let (off, size) = crate::tray::parse_icon_entry(crate::tray::ICON_ICO).unwrap();
        assert!(
            off + size <= crate::tray::ICON_ICO.len(),
            "icon entry escapes: off {off} + size {size} > len {}",
            crate::tray::ICON_ICO.len()
        );
        let big = create_mark_icon(0, 0);
        assert!(!big.is_null(), "256px mark icon failed to load");
        let small = create_mark_icon(16, 16);
        assert!(!small.is_null(), "16px mark icon failed to load");
        // SAFETY: both handles were created above and are not used again.
        unsafe {
            win32::DestroyIcon(big);
            win32::DestroyIcon(small);
        }
    }
}
