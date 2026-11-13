//! Narrow chrome primitives: four explicit paint functions, each with a
//! measured rect owned by the layout and exactly one caller in `window.rs`.
//!
//! - `bar` (48px): 20px mark left (no wordmark: the OS caption already reads
//!   "Potflix"); right hairline chip plus 32px gear glyph (`#A7ABB4` rest,
//!   `#F2F3F5` hover, accent never).
//! - `hero` (<=124px): 64x96 poster, 20 semibold title single line, 12 muted
//!   meta, 12 status, 32px ghost "Fix match" right-aligned on the status row.
//! - `result_row` (56px): 32x48 cover thumb, 13 semibold title, 12 muted meta
//!   built from the record only (provider names forbidden).
//! - `settings_row` (48px): 13 label left at 16, 12 muted desc under it,
//!   right-aligned 36px control; 11px caps labels 32px above each group
//!   (layout owns the caps; this paints one row).

use windows_sys::Win32::Graphics::Gdi;

use super::{
    fill_rect_solid, fill_round_rect, scale_px, stroke_round_rect_w, COL_ACCENT,
    COL_BORDER, COL_CARD, COL_MUTED, COL_ROW_HOVER, COL_SURFACE, COL_TEXT, FS_CHIP, FS_HERO_META,
    FS_HERO_TITLE, FS_ROW_TITLE, R_CTRL, R_PANEL, SP_1, SP_2, SP_3, STROKE_HAIR, Rect,
};
use crate::win32;

// ---------------------------------------------------------------------------
// Providers forbidden: record-only meta.
// ---------------------------------------------------------------------------

/// The narrow result meta, built from the record only: no provider names.
///
/// Series: `Anime series · 2022 · 10 episodes` (or `TV series · …`);
/// movie: `Anime movie · 2022` / `Film · 2022`. Zero year dropped, zero
/// episodes dropped, singular handled.
#[must_use]
pub fn result_meta(kind_is_movie: bool, anime: bool, year: i32, episodes: i32) -> String {
    let kind_word = if kind_is_movie {
        if anime { "Anime movie" } else { "Film" }
    } else if anime {
        "Anime series"
    } else {
        "TV series"
    };
    let mut s = kind_word.to_string();
    if year > 0 {
        s.push_str(&format!(" · {year}"));
    }
    if !kind_is_movie && episodes > 0 {
        let plural = if episodes == 1 { "" } else { "s" };
        s.push_str(&format!(" · {episodes} episode{plural}"));
    }
    s
}

/// The narrow hero meta, built from the record only: the episode marker never
/// appears for a movie. Movies show the runtime (`2024 · Film · 2h 46m`);
/// series show the episode code (`2024 · Anime series · S02E01`,
/// `2024 · Series · S02E01`). Zero year dropped, empty detail dropped.
#[must_use]
pub fn hero_meta(
    year: i32,
    kind_is_movie: bool,
    anime: bool,
    length: &str,
    episode_code: &str,
) -> String {
    let kind_word = if kind_is_movie {
        "Film"
    } else if anime {
        "Anime series"
    } else {
        "Series"
    };
    let mut parts: Vec<String> = Vec::new();
    if year > 0 {
        parts.push(format!("{year}"));
    }
    parts.push(kind_word.to_string());
    let detail = if kind_is_movie {
        length.trim().to_string()
    } else {
        episode_code.trim().to_string()
    };
    // A series with no episode code falls back to its runtime rather than a
    // bare year + kind; a movie with no runtime stays bare.
    let detail = if detail.is_empty() && !kind_is_movie {
        length.trim().to_string()
    } else {
        detail
    };
    if !detail.is_empty() {
        parts.push(detail);
    }
    parts.join(" · ")
}

/// Measure `s` in `font` on `hdc`: the `GetTextExtent` proof the brief demands.
#[must_use]
pub fn text_width_px(hdc: Gdi::HDC, font: Gdi::HFONT, s: &str) -> i32 {
    use windows_sys::Win32::Foundation::SIZE;
    if s.is_empty() {
        return 0;
    }
    // SAFETY: extents read only; previous font restored.
    unsafe {
        let old = Gdi::SelectObject(hdc, font);
        let u: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE { cx: 0, cy: 0 };
        let r = Gdi::GetTextExtentPoint32W(hdc, u.as_ptr(), u.len() as i32, &mut sz);
        Gdi::SelectObject(hdc, old);
        if r == 0 {
            return 0;
        }
        sz.cx
    }
}

fn draw_left(
    hdc: Gdi::HDC,
    s: &str,
    rc: Rect,
    font: Gdi::HFONT,
    color: u32,
    ellipsis: bool,
) {
    let mut fmt = crate::win32::DT_LEFT
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER;
    if ellipsis {
        fmt |= crate::win32::DT_END_ELLIPSIS;
    }
    super::draw_ui_text(hdc, s, rc, fmt, font, color);
}

fn draw_center(
    hdc: Gdi::HDC,
    s: &str,
    rc: Rect,
    font: Gdi::HFONT,
    color: u32,
) {
    // No ellipsis: scope labels must never ellipsise at 460px; the layout
    // guarantees the fit and the test pins it with GetTextExtent.
    let fmt = crate::win32::DT_CENTER
        | crate::win32::DT_SINGLE_LINE
        | crate::win32::DT_NO_PREFIX
        | crate::win32::DT_V_CENTER;
    super::draw_ui_text(hdc, s, rc, fmt, font, color);
}

// ---------------------------------------------------------------------------
// bar: 48px chrome. Measured rect = the top bar; one caller =
// `window::paint_narrow_bar`.
// ---------------------------------------------------------------------------

/// Paint the narrow top bar: 20px mark left (the OS caption already carries
/// the wordmark, so no in-app text label), 12px hairline chip plus 32px gear
/// glyph right. The gear is never the primary action: its glyph is muted
/// (`#A7ABB4`) at rest, body text (`#F2F3F5`) on hover/press, accent never --
/// see [`gear_color`].
pub fn bar(
    hdc: Gdi::HDC,
    bar_rc: Rect,
    dpi: i32,
    chip_text: &str,
    gear_rc: Rect,
    gear_hovered: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    if bar_rc.right <= bar_rc.left || bar_rc.bottom <= bar_rc.top {
        return;
    }
    fill_rect_solid(hdc, bar_rc, COL_SURFACE);
    fill_rect_solid(
        hdc,
        Rect {
            left: bar_rc.left,
            top: bar_rc.bottom - sc(STROKE_HAIR),
            right: bar_rc.right,
            bottom: bar_rc.bottom,
        },
        COL_BORDER,
    );
    // 20px mark.
    let mark = sc(20);
    let mark_rc = Rect {
        left: bar_rc.left + sc(16),
        top: bar_rc.top + (bar_rc.bottom - bar_rc.top - mark) / 2,
        right: bar_rc.left + sc(16) + mark,
        bottom: bar_rc.top + (bar_rc.bottom - bar_rc.top - mark) / 2 + mark,
    };
    let icon = crate::brand::mark_icon();
    if !icon.is_null() {
        // SAFETY: process-owned mark handle; DrawIconEx reads it.
        unsafe {
            crate::win32::DrawIconEx(
                hdc,
                mark_rc.left,
                mark_rc.top,
                icon,
                mark,
                mark,
                0,
                std::ptr::null_mut(),
                crate::win32::DI_NORMAL,
            );
        }
    }
    let chip_font = crate::brand::ui(sc(FS_CHIP), crate::brand::UI_WEIGHT);
    let gear_font = crate::brand::ui(sc(16), crate::brand::UI_WEIGHT);
    // No in-app wordmark: the OS caption already reads "Potflix", so a second
    // text label in this bar doubles the name. The mark stays at the left.
    // Right side: hairline chip + 32px gear glyph. Gear rect is the layout's
    // measured rect; the chip sits left of it with an 8px gap. A healthy link
    // shows NO chrome: an empty chip text paints nothing, so the bar stays
    // quiet unless there is a reason to name.
    if !chip_text.is_empty() {
        let chip_pad_x = sc(10);
        let chip_w = text_width_px(hdc, chip_font, chip_text) + chip_pad_x * 2;
        let chip_h = sc(24);
        let chip_rc = Rect {
            left: gear_rc.left - sc(SP_2) - chip_w,
            top: bar_rc.top + (bar_rc.bottom - bar_rc.top - chip_h) / 2,
            right: gear_rc.left - sc(SP_2),
            bottom: bar_rc.top + (bar_rc.bottom - bar_rc.top - chip_h) / 2 + chip_h,
        };
        if chip_rc.right > chip_rc.left {
            fill_round_rect(hdc, chip_rc, COL_SURFACE, sc(R_CTRL));
            stroke_round_rect_w(hdc, chip_rc, COL_BORDER, sc(R_CTRL), sc(STROKE_HAIR));
            draw_center(hdc, chip_text, chip_rc, chip_font, COL_MUTED);
        }
    }
    // Gear glyph centred in its 32px rect; never the "Settings" text (that is
    // what clipped to "Set…"). Muted at rest, body text on hover/press --
    // accent never (the gear is not the primary action).
    draw_center(hdc, "⚙", gear_rc, gear_font, gear_color(gear_hovered));
    // SAFETY: fonts created above.
    unsafe {
        Gdi::DeleteObject(chip_font);
        Gdi::DeleteObject(gear_font);
    }
}

/// The gear glyph's colour: `#A7ABB4` at rest, `#F2F3F5` on hover/press,
/// accent never. Pure so the paint and its test agree on the one rule.
#[must_use]
pub fn gear_color(hovered: bool) -> u32 {
    if hovered { COL_TEXT } else { COL_MUTED }
}

// ---------------------------------------------------------------------------
// hero: <=124px object card. Measured rects = object/cover/title/meta/status +
// ghost; one caller = `window::paint_narrow_card`.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn hero(
    hdc: Gdi::HDC,
    hero_rc: Rect,
    cover_rc: Rect,
    title_rc: Rect,
    meta_rc: Rect,
    status_rc: Rect,
    fix_rc: Rect,
    fix_label: &str,
    dpi: i32,
    title: &str,
    meta: &str,
    status: &str,
    cover: Option<&crate::artwork::Cover>,
    hovered: bool,
    pressed: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    if hero_rc.right <= hero_rc.left || hero_rc.bottom <= hero_rc.top {
        return;
    }
    fill_round_rect(hdc, hero_rc, COL_CARD, sc(R_PANEL));
    stroke_round_rect_w(hdc, hero_rc, COL_BORDER, sc(R_PANEL), sc(STROKE_HAIR));
    // 64x96 poster.
    super::paint_cover(hdc, cover_rc, dpi, cover);
    let title_font = crate::brand::ui(sc(FS_HERO_TITLE), crate::brand::UI_WEIGHT);
    let meta_font = crate::brand::ui(sc(FS_HERO_META), crate::brand::UI_WEIGHT);
    // Title 20 semibold, single line, ellipsised (one line, never wraps).
    draw_left(hdc, title, title_rc, title_font, COL_TEXT, true);
    draw_left(hdc, meta, meta_rc, meta_font, COL_MUTED, true);
    draw_left(hdc, status, status_rc, meta_font, COL_MUTED, true);
    // 32px ghost "Fix match" right-aligned on the status row: outline only at
    // rest, hover fill; never accent. Absent when there is no live title: an
    // empty rect paints nothing but keeps the object card.
    let ghost_font = crate::brand::ui(sc(12), crate::brand::UI_WEIGHT);
    if fix_rc.right > fix_rc.left && fix_rc.bottom > fix_rc.top && !fix_label.is_empty() {
        let fill = if pressed {
            COL_SURFACE
        } else if hovered {
            COL_ROW_HOVER
        } else {
            COL_CARD
        };
        fill_round_rect(hdc, fix_rc, fill, sc(R_CTRL));
        stroke_round_rect_w(hdc, fix_rc, COL_BORDER, sc(R_CTRL), sc(STROKE_HAIR));
        draw_center(hdc, fix_label, fix_rc, ghost_font, COL_TEXT);
    }
    // SAFETY: fonts created above.
    unsafe {
        Gdi::DeleteObject(title_font);
        Gdi::DeleteObject(meta_font);
        Gdi::DeleteObject(ghost_font);
    }
}

// ---------------------------------------------------------------------------
// result_row: 56px record row. Measured rect = the row; one caller =
// `window::paint_narrow_card` (via the results panel loop).
// ---------------------------------------------------------------------------

/// The 32x48 cover thumb of one result row. Pure, so the paint and the test
/// that proves the thumb fits inside the row read the same rect.
pub fn result_row_thumb(rc: Rect, dpi: i32) -> Rect {
    let sc = |v: i32| scale_px(dpi, v);
    Rect {
        left: rc.left + sc(SP_3),
        top: rc.top + sc(SP_1),
        right: rc.left + sc(SP_3) + sc(32),
        bottom: rc.top + sc(SP_1) + sc(48),
    }
}

/// Paint one 56px result row: 32x48 cover thumb (the decoded poster when its
/// bytes are cached, else the empty tile), 13 semibold title, 12 muted
/// record-only meta. Selected gets the hover fill plus the accent left bar.
/// The click/hit geometry still reads from the layout table: this paints
/// inside the measured row rect only.
pub fn result_row(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    title: &str,
    meta: &str,
    cover: Option<&crate::artwork::Cover>,
    selected: bool,
) {
    let sc = |v: i32| scale_px(dpi, v);
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return;
    }
    fill_round_rect(
        hdc,
        rc,
        if selected { COL_ROW_HOVER } else { COL_CARD },
        sc(super::R_ROW),
    );
    if selected {
        fill_rect_solid(
            hdc,
            Rect {
                left: rc.left,
                top: rc.top,
                right: rc.left + sc(super::SELECT_BAR_W),
                bottom: rc.bottom,
            },
            COL_ACCENT,
        );
    }
    // 32x48 cover thumb: the decoded poster when cached, else the empty
    // tile. Same tile painter as the hero cover, so rows and hero agree.
    let thumb = result_row_thumb(rc, dpi);
    super::paint_cover(hdc, thumb, dpi, cover);
    let title_font = crate::brand::ui(sc(FS_ROW_TITLE), crate::brand::UI_WEIGHT);
    let meta_font = crate::brand::ui(sc(FS_HERO_META), crate::brand::UI_WEIGHT);
    let text_l = thumb.right + sc(SP_3);
    let text_r = rc.right - sc(SP_3);
    let mid = (rc.top + rc.bottom) / 2;
    draw_left(
        hdc,
        title,
        Rect {
            left: text_l,
            top: rc.top + sc(4),
            right: text_r,
            bottom: mid,
        },
        title_font,
        COL_TEXT,
        true,
    );
    draw_left(
        hdc,
        meta,
        Rect {
            left: text_l,
            top: mid,
            right: text_r,
            bottom: rc.bottom - sc(4),
        },
        meta_font,
        COL_MUTED,
        true,
    );
    // SAFETY: fonts created above.
    unsafe {
        Gdi::DeleteObject(title_font);
        Gdi::DeleteObject(meta_font);
    }
    let _ = win32::SRCCOPY;
}

// ---------------------------------------------------------------------------
// settings_row: 48px settings row. Measured rect = the row; one caller =
// `window::paint_narrow_settings`.
// ---------------------------------------------------------------------------

/// What the right-aligned 36px control of a settings row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NarrowCtl {
    /// 36x20 pill switch, on/off.
    Toggle(bool),
    /// Chevron for action rows.
    Chevron,
    /// No control (Theme row paints its two segments separately).
    None,
}

/// Paint one 48px settings row: 13 label left at 16, 12 muted description
/// under it, right-aligned 36px control. Returns the control rect for
/// hit-testing parity.
pub fn settings_row(
    hdc: Gdi::HDC,
    rc: Rect,
    dpi: i32,
    label: &str,
    desc: &str,
    ctl: NarrowCtl,
) -> Rect {
    let sc = |v: i32| scale_px(dpi, v);
    if rc.right <= rc.left || rc.bottom <= rc.top {
        return Rect::default();
    }
    fill_round_rect(hdc, rc, COL_CARD, sc(super::R_ROW));
    stroke_round_rect_w(hdc, rc, COL_BORDER, sc(super::R_ROW), sc(STROKE_HAIR));
    let label_font = crate::brand::ui(sc(13), crate::brand::UI_WEIGHT);
    let desc_font = crate::brand::ui(sc(12), crate::brand::UI_WEIGHT);
    let ctl_font = crate::brand::ui(sc(13), crate::brand::UI_WEIGHT);
    let text_l = rc.left + sc(16);
    let ctl_w = sc(36);
    let ctl_rc = Rect {
        left: rc.right - sc(SP_3) - ctl_w,
        top: rc.top + (rc.bottom - rc.top - sc(20)) / 2,
        right: rc.right - sc(SP_3),
        bottom: rc.top + (rc.bottom - rc.top - sc(20)) / 2 + sc(20),
    };
    let text_r = (ctl_rc.left - sc(SP_3)).max(text_l + sc(40));
    let mid = (rc.top + rc.bottom) / 2;
    draw_left(
        hdc,
        label,
        Rect {
            left: text_l,
            top: rc.top + sc(4),
            right: text_r,
            bottom: mid,
        },
        label_font,
        COL_TEXT,
        true,
    );
    draw_left(
        hdc,
        desc,
        Rect {
            left: text_l,
            top: mid,
            right: text_r,
            bottom: rc.bottom - sc(4),
        },
        desc_font,
        COL_MUTED,
        true,
    );
    match ctl {
        NarrowCtl::Toggle(on) => {
            // 36x20 pill switch right-aligned.
            let pill = Rect {
                left: ctl_rc.right - sc(36),
                top: ctl_rc.top,
                right: ctl_rc.right,
                bottom: ctl_rc.top + sc(20),
            };
            fill_round_rect(
                hdc,
                pill,
                if on { COL_ACCENT } else { COL_SURFACE },
                sc(10),
            );
            if !on {
                stroke_round_rect_w(hdc, pill, COL_BORDER, sc(10), sc(STROKE_HAIR));
            }
            let knob = sc(14);
            let kx = if on { pill.right - sc(3) - knob } else { pill.left + sc(3) };
            fill_round_rect(
                hdc,
                Rect {
                    left: kx,
                    top: pill.top + sc(3),
                    right: kx + knob,
                    bottom: pill.bottom - sc(3),
                },
                super::COL_ON_ACCENT,
                sc(7),
            );
        }
        NarrowCtl::Chevron => {
            draw_center(hdc, "›", ctl_rc, ctl_font, COL_MUTED);
        }
        NarrowCtl::None => {}
    }
    // SAFETY: fonts created above.
    unsafe {
        Gdi::DeleteObject(label_font);
        Gdi::DeleteObject(desc_font);
        Gdi::DeleteObject(ctl_font);
    }
    ctl_rc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gear_glyph_is_muted_rest_text_hover_accent_never() {
        assert_eq!(gear_color(false), crate::brand::rgb(0xA7, 0xAB, 0xB4), "rest is #A7ABB4");
        assert_eq!(gear_color(true), crate::brand::rgb(0xF2, 0xF3, 0xF5), "hover is #F2F3F5");
        assert_ne!(gear_color(false), COL_ACCENT, "rest is never accent");
        assert_ne!(gear_color(true), COL_ACCENT, "hover is never accent");
        assert_ne!(gear_color(true), crate::brand::rgb(0xF0, 0x4B, 0x55), "hover is never accent-hover");
    }

    #[test]
    fn hero_meta_never_marks_a_film_with_an_episode() {
        assert_eq!(
            hero_meta(2024, true, false, "2h 46m", "S02E01"),
            "2024 · Film · 2h 46m",
            "a Film keeps its runtime even when an episode code is passed"
        );
        assert_eq!(
            hero_meta(2024, false, true, "60 min", "S02E01"),
            "2024 · Anime series · S02E01"
        );
        assert_eq!(
            hero_meta(2024, false, false, "60 min", "S02E01"),
            "2024 · Series · S02E01"
        );
        assert_eq!(hero_meta(0, true, false, "", ""), "Film");
        assert_eq!(hero_meta(0, false, false, "", ""), "Series");
    }
}
