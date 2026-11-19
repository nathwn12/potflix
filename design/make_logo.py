"""Potflix identity pass 2 - Ribbon P + heavy wordmark (stdlib + Pillow only).

Run:  python design\\make_logo.py
Renders at 8x supersample, downsamples with Image.LANCZOS. Flat fills only.

Palette (exact):
  RED   #E50914  lit ribbon facets, badges
  DARK  #8E060C  receding ribbon facet (brand red scaled by 0.62, rounded)
  BLACK #0B0B0B  tiles, wordmark-on-light
  WHITE #FFFFFF  wordmark-on-black, badge glyphs
  LIGHT #F5F5F5  candidate-sheet light panel only (not brand)

Fonts live in design\\fonts\\ (OFL, alongside <family>-OFL.txt).
Chosen wordmark: Archivo Black (single-master display black, even color,
tight grotesque caps; condensed faces lose legibility across 7 glyphs on a
square tile, geometric faces read soft).

Outputs:
  design\\preview\\logo-candidates.png  (6-typeface labelled sheet)
  assets\\discord\\logo.png   1024x1024 chosen lockup on black rounded tile
  assets\\appicon.png          1024x1024 Ribbon P on black rounded tile
  assets\\appicon-square.png   1024x1024 Ribbon P on full-bleed black square
  assets\\discord\\play|pause|stop.png  1024x1024 V1 dark badge set (black disc,
    red ring + red glyph; PROTECTED writers below leave logo/appicons/ico
    byte-identical - this script reads logo.png, it does not rewrite it)
  assets\\discord\\cover.png  1024x576 C1 PRIMARY invite cover (no wordmark)
  design\\preview\\cover-variants.png  labelled C1/C2/C3 plain + mock-overlay sheet
  design\\preview\\badges-variants.png  labelled V1/V2/V3 sheet (large + 24px
    actual on #232428 and #0B0B0B)
  assets\\icon.ico             ONE 256x256 BMP/DIB frame, transparent corners
  design\\preview\\card-*.png  refreshed communication-aid cards (V1 small art)
"""

import os
import struct
from PIL import Image, ImageDraw, ImageFont, ImageFilter

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SS = 8  # supersample factor: the crispness source

RED = (0xE5, 0x09, 0x14, 255)  # #E50914 brand red (lit facets)
DARK = (0x8E, 0x06, 0x0C, 255)  # #8E060C = round(#E50914 * 0.62)
BLACK = (0x0B, 0x0B, 0x0B, 255)  # #0B0B0B tiles
WHITE = (0xFF, 0xFF, 0xFF, 255)  # #FFFFFF wordmark / glyphs
LIGHT = (0xF5, 0xF5, 0xF5, 255)  # sheet light panel only
CARD = (0x23, 0x24, 0x28, 255)
CARD_HDR = (0xF2, 0xF3, 0xF5, 255)
CARD_DET = (0xDB, 0xDE, 0xE1, 255)
CARD_MUT = (0xB5, 0xBA, 0xC1, 255)

PALETTE_REPORT = (
    "RED=#E50914 DARK=#8E060C(round(#E50914*0.62)) BLACK=#0B0B0B WHITE=#FFFFFF"
)

FDIR = os.path.join(ROOT, "design", "fonts")
FONTS = {  # key: file, expected family, variable->named instance, tracking em
    "archivo": dict(
        file="ArchivoBlack-Regular.ttf", family="Archivo Black", named=None, track=0.08
    ),
    "montserrat-black": dict(
        file="Montserrat-Variable.ttf", family="Montserrat", named=b"Black", track=0.10
    ),
    "poppins-black": dict(
        file="Poppins-Black.ttf", family="Poppins", named=None, track=0.08
    ),
    "poppins-xbold": dict(
        file="Poppins-ExtraBold.ttf", family="Poppins", named=None, track=0.08
    ),
    "anton": dict(file="Anton-Regular.ttf", family="Anton", named=None, track=0.06),
    "bebas": dict(
        file="BebasNeue-Regular.ttf", family="Bebas Neue", named=None, track=0.10
    ),
}
CHOSEN = "archivo"
INTER = dict(file="Inter-Variable.ttf", family="Inter")

# tight-but-even manual kern pairs (em units, applied on top of tracking)
KERN = {
    ("T", "F"): -0.020,
    ("F", "L"): -0.030,
    ("L", "I"): -0.010,
    ("P", "O"): -0.012,
    ("O", "T"): -0.012,
    ("I", "X"): +0.008,
}


def load_font(spec, size):
    """Truetype load + named-instance select + family assert (no silent fallback)."""
    path = os.path.join(FDIR, spec["file"])
    f = ImageFont.truetype(path, size)
    if spec.get("named"):
        f.set_variation_by_name(spec["named"])
    got = f.getname()[0]
    assert got == spec["family"], "font fallback: %s -> %s" % (path, got)
    return f


def load_inter(size, named):
    f = ImageFont.truetype(os.path.join(FDIR, INTER["file"]), size)
    f.set_variation_by_name(named)
    assert f.getname()[0] == "Inter"
    return f


def new(size, color=(0, 0, 0, 0)):
    return Image.new("RGBA", (int(size[0] * SS), int(size[1] * SS)), color)


def down(img, mode="RGBA"):
    w, h = img.size
    return img.resize((w // SS, h // SS), Image.LANCZOS).convert(mode)


def rr(d, box, r, fill):
    d.rounded_rectangle([b * SS for b in box], radius=int(r * SS), fill=fill)


def tile(size, bg):
    """Rounded-square tile on transparency (corners stay transparent)."""
    img = new((size, size))
    rr(ImageDraw.Draw(img), (0, 0, size, size), size * 0.225, bg)
    return img


# Grid: box is 24u x 24u. All strokes 5u. Counter punched in tile bg.
# Facets: (1) front face RED, (2) receding ribbon plane DARK offset (+ex,+ex),
# (3) diagonal fold crease DARK across the bowl upright.
def ribbon_polys(x0, y0, W):
    u = W / 24.0
    X = lambda v: x0 + v * u
    Y = lambda v: y0 + v * u
    stem = [(X(4), Y(2)), (X(9), Y(2)), (X(9), Y(22)), (X(4), Y(22))]
    top = [(X(4), Y(2)), (X(20), Y(2)), (X(20), Y(7)), (X(4), Y(7))]
    upright = [(X(15), Y(7)), (X(20), Y(7)), (X(20), Y(11)), (X(15), Y(11))]
    mid = [(X(4), Y(11)), (X(20), Y(11)), (X(20), Y(16)), (X(4), Y(16))]
    counter = [(X(9), Y(7)), (X(15), Y(7)), (X(15), Y(11)), (X(9), Y(11))]
    crease = [(X(15), Y(9.0)), (X(20), Y(7.6)), (X(20), Y(9.1)), (X(15), Y(10.5))]
    return [stem, top, upright, mid], counter, crease, 1.2 * u


def ribbon_p(img, box):
    """Ribbon P mark. box=(x0,y0,x1,y1) in 1x units; img is SS-scale RGBA."""
    x0, y0, x1, y1 = box
    W = x1 - x0
    polys, counter, crease, ex = ribbon_polys(x0, y0, W)
    d = ImageDraw.Draw(img)

    def poly(pts, dx=0.0, dy=0.0, fill=None):
        d.polygon([((x + dx) * SS, (y + dy) * SS) for x, y in pts], fill=fill)

    for p in polys:  # receding ribbon plane
        poly(p, ex, ex, DARK)
    for p in polys:  # lit front face
        poly(p, 0, 0, RED)
    poly(counter, 0, 0, BLACK)  # bowl counter
    poly(crease, 0, 0, DARK)  # fold crease


def tracked_width(font, text, track_em):
    w, size = 0.0, font.size
    for i, ch in enumerate(text):
        w += font.getlength(ch) + track_em * size
        if i + 1 < len(text):
            w += KERN.get((ch, text[i + 1]), 0.0) * size
    return w


def draw_wordmark(img, box, key, fill):
    """POTFLIX in heavy caps, glyph-by-glyph manual tracking. Returns family."""
    spec = FONTS[key]
    x0, y0, x1, y1 = box
    target = (x1 - x0) * 0.98
    probe = load_font(spec, 100)
    size = max(8, int(100 * target / tracked_width(probe, "POTFLIX", spec["track"])))
    font = load_font(spec, size * SS)
    fam = font.getname()[0]
    d = ImageDraw.Draw(img)
    total = tracked_width(font, "POTFLIX", spec["track"])
    cx = (x0 + x1) / 2 * SS
    ymid = (y0 + y1) / 2 * SS
    x = cx - total / 2
    for i, ch in enumerate("POTFLIX"):
        d.text((x, ymid), ch, font=font, fill=fill, anchor="lm")
        x += font.getlength(ch) + spec["track"] * font.size
        if i + 1 < 7:
            x += KERN.get((ch, "POTFLIX"[i + 1]), 0.0) * font.size
    return fam


def render_mark_tile(size, margin_frac=0.21):
    t = tile(size, BLACK)
    m = size * margin_frac
    ribbon_p(t, (m, m, size - m, size - m))
    return down(t)


def render_mark_square(size, margin_frac=0.12):
    """Mark on a full-bleed opaque black square (for circular masking)."""
    img = new((size, size), BLACK)
    m = size * margin_frac
    ribbon_p(img, (m, m, size - m, size - m))
    return down(img)


def render_lockup(size, key):
    """Chosen lockup: Ribbon P over POTFLIX on black rounded tile."""
    t = tile(size, BLACK)
    mw = size * 0.52
    ribbon_p(t, ((size - mw) / 2, size * 0.105, (size + mw) / 2, size * 0.105 + mw))
    fam = draw_wordmark(
        t, (size * 0.088, size * 0.70, size * 0.912, size * 0.87), key, WHITE
    )
    return down(t), fam


# Dark-mode Discord badges (1024 fractions; S = 1024):
#   DISC_D  = 0.90 S  disc diameter (V1 + V3); margin 0.05 S per side (51.2 px)
#   RING_W  = 0.055 S ring stroke (56.3 px); ring outer flush with disc edge,
#             inner diameter 0.79 S. At 24 px: 24/1024 scale -> stroke 1.32 px,
#             disc ~21.6 px. Clearly visible, fully inside canvas.
#   V2_DISC = 0.86 S  disc diameter; base margin 0.07 S per side (71.7 px)
#   FACET   = dx=dy=0.032 S (32.8 px each, magnitude ~0.045 S / 46.4 px):
#             dark disc offset behind black disc -> bottom-right crescent up to
#             ~1.1 px at 24 px. Max extent 0.962 S, min 0.07 S: nothing clipped.
# Glyph geometry (play triangle / pause bars / stop square, optical shift)
# is unchanged from the original badge; glyph fill is now RED.
BADGE_DISC_D = 0.90
BADGE_RING_W = 0.055
BADGE_V2_D = 0.86
BADGE_FACET_DX = 0.032
BADGE_FACET_DY = 0.032


def draw_badge_glyph(d, c, r, kind):
    if kind == "play":
        sh = r * 0.10  # optical shift right
        d.polygon(
            [
                (c - r * 0.55 + sh, c - r * 0.75),
                (c - r * 0.55 + sh, c + r * 0.75),
                (c + r * 0.75 + sh, c),
            ],
            fill=RED,
        )
    elif kind == "pause":
        w = r * 0.52
        d.rectangle(
            [c - r * 0.72, c - r * 0.72, c - r * 0.72 + w, c + r * 0.72], fill=RED
        )
        d.rectangle(
            [c + r * 0.72 - w, c - r * 0.72, c + r * 0.72, c + r * 0.72], fill=RED
        )
    else:
        d.rounded_rectangle(
            [c - r * 0.62, c - r * 0.62, c + r * 0.62, c + r * 0.62],
            radius=int(r * 0.12),
            fill=RED,
        )


def render_badge_v1(size, kind):
    """V1 PRIMARY: black disc + red ring + red glyph."""
    img = new((size, size))
    d = ImageDraw.Draw(img)
    disc = size * BADGE_DISC_D * SS
    ringw = size * BADGE_RING_W * SS
    x0 = (size * SS - disc) / 2
    box = [x0, x0, x0 + disc, x0 + disc]
    d.ellipse(box, fill=RED)  # ring outer
    inner = ringw * 2
    d.ellipse(
        [box[0] + ringw, box[1] + ringw, box[2] - ringw, box[3] - ringw], fill=BLACK
    )
    c, r = size / 2 * SS, size * 0.30 * SS
    draw_badge_glyph(d, c, r, kind)
    return down(img)


def render_badge_v2(size, kind):
    """V2: black disc + red glyph + dark-red offset facet (no ring)."""
    img = new((size, size))
    d = ImageDraw.Draw(img)
    disc = size * BADGE_V2_D * SS
    dx, dy = size * BADGE_FACET_DX * SS, size * BADGE_FACET_DY * SS
    x0 = (size * SS - disc) / 2
    d.ellipse([x0 + dx, x0 + dy, x0 + disc + dx, x0 + disc + dy], fill=DARK)
    d.ellipse([x0, x0, x0 + disc, x0 + disc], fill=BLACK)
    c, r = size / 2 * SS, size * 0.30 * SS
    draw_badge_glyph(d, c, r, kind)
    return down(img)


def render_badge_v3(size, kind):
    """V3: dark-red disc + red glyph (no black)."""
    img = new((size, size))
    d = ImageDraw.Draw(img)
    disc = size * BADGE_DISC_D * SS
    x0 = (size * SS - disc) / 2
    d.ellipse([x0, x0, x0 + disc, x0 + disc], fill=DARK)
    c, r = size / 2 * SS, size * 0.30 * SS
    draw_badge_glyph(d, c, r, kind)
    return down(img)


def render_badge(size, kind):
    """V1 is the shipped asset; cards + assets/discord use this."""
    return render_badge_v1(size, kind)


def candidate_sheet():
    """Labelled sheet: per typeface, mark tile + wordmark on black and light."""
    keys = [
        "archivo",
        "montserrat-black",
        "poppins-black",
        "poppins-xbold",
        "anton",
        "bebas",
    ]
    labels = {
        "archivo": "Archivo Black",
        "montserrat-black": "Montserrat Black (variable, Black instance)",
        "poppins-black": "Poppins Black",
        "poppins-xbold": "Poppins ExtraBold",
        "anton": "Anton Regular",
        "bebas": "Bebas Neue Regular",
    }
    RW, RH, HEAD = 1800, 600, 150  # 1x units
    rows = []
    fams = {}
    for key in keys:
        row = new((RW, RH), (0x1A, 0x1B, 0x1E, 255))
        d = ImageDraw.Draw(row)
        d.rectangle([0, 0, RW * SS, RH * SS], fill=(0x1A, 0x1B, 0x1E, 255))
        fl = load_inter(30 * SS, b"Bold")
        fs = load_inter(22 * SS, b"Regular")
        d.text(
            (40 * SS, 26 * SS),
            "%s  .  track %.2fem" % (labels[key], FONTS[key]["track"]),
            font=fl,
            fill=WHITE,
            anchor="lt",
        )
        spec = FONTS[key]
        d.text(
            (40 * SS, 66 * SS),
            "%s / %s" % (spec["file"], "named Black" if spec["named"] else "static"),
            font=fs,
            fill=CARD_MUT,
            anchor="lt",
        )
        for col, bg, wmfill in ((0, BLACK, WHITE), (1, LIGHT, BLACK)):
            px = RW / 2 * col
            d.rectangle([px * SS, 110 * SS, (px + RW / 2) * SS, RH * SS], fill=bg)
            t = tile(300, BLACK)
            ribbon_p(t, (300 * 0.21,) * 2 + (300 * 0.79,) * 2)
            row.alpha_composite(t, (int((px + 70) * SS), int(170 * SS)))
            fams[key] = draw_wordmark(
                row, (px + 400, 250, px + RW / 2 - 60, 420), key, wmfill
            )
        rows.append(down(row.convert("RGB")))
    head = new((RW, HEAD), (0x1A, 0x1B, 0x1E, 255))
    dh = ImageDraw.Draw(head)
    ft = load_inter(46 * SS, b"Bold")
    dh.text(
        (RW / 2 * SS, HEAD / 2 * SS),
        "POTFLIX LOGO CANDIDATES",
        font=ft,
        fill=WHITE,
        anchor="mm",
    )
    sheet = Image.new("RGB", (RW, HEAD + RH * len(rows)), (26, 27, 30))
    sheet.paste(down(head.convert("RGB")), (0, 0))
    for i, r in enumerate(rows):
        sheet.paste(r, (0, HEAD + RH * i))
    return sheet, fams


VARIANT_BLURB = {
    "V1": "V1 PRIMARY: black disc + red ring + red glyph",
    "V2": "V2: black disc + red glyph + dark-red offset facet (no ring)",
    "V3": "V3: dark-red disc + red glyph (no black)",
}


def badges_variants_sheet(badges):
    """Labelled sheet: V1/V2/V3 x (large + 24px actual, 6x nearest) x CARD/BLACK.

    badges: {(variant, kind): 1024 RGBA}. Sheet is 1x RGB (no supersample).
    """
    W, HEAD, ROWH = 1800, 170, 560
    sheet = Image.new("RGB", (W, HEAD + ROWH * 3), (26, 27, 30))
    dh = ImageDraw.Draw(sheet)
    ft = load_inter(44, "Bold")
    fs = load_inter(24, "Regular")
    dh.text(
        (W // 2, HEAD // 2),
        "POTFLIX DISCORD BADGES - DARK VARIANTS",
        font=ft,
        fill=WHITE,
        anchor="mm",
    )
    y = HEAD
    for vi, v in enumerate(("V1", "V2", "V3")):
        dh.text(
            (40, y + 14),
            "%s  .  %s" % (v, VARIANT_BLURB[v]),
            font=load_inter(28, "Bold"),
            fill=WHITE,
        )
        dh.text(
            (40, y + 52),
            "disc/ring numbers state fractions of 1024 in make_logo.py header",
            font=fs,
            fill=CARD_MUT[:3],
        )
        panels = (("CARD #232428", (0x23, 0x24, 0x28)), ("BLACK #0B0B0B", (11, 11, 11)))
        pw = (W - 80) // 2
        for pi, (plabel, bg) in enumerate(panels):
            x = 40 + pi * pw
            panel = Image.new("RGB", (pw, ROWH - 110), bg)
            pd = ImageDraw.Draw(panel)
            fl = load_inter(24, "Bold")
            pd.text(
                (20, 8),
                "%s  .  large + 24px actual (6x nearest)" % plabel,
                font=fl,
                fill=(255, 255, 255) if pi == 0 else (245, 245, 245),
            )
            bx = 20
            for kind in ("play", "pause", "stop"):
                b = badges[(v, kind)]
                lg = b.resize((150, 150), Image.LANCZOS)
                panel.paste(lg, (bx, 50), lg)
                pd.text(
                    (bx + 75, 210),
                    kind,
                    font=load_inter(22, "Regular"),
                    fill=(181, 186, 193),
                    anchor="mm",
                )
                sm = b.resize((24, 24), Image.LANCZOS)
                big = sm.resize((144, 144), Image.NEAREST)
                panel.paste(big, (bx + 3, 240), big)
                pd.text(
                    (bx + 75, 394),
                    "24px",
                    font=load_inter(22, "Regular"),
                    fill=(181, 186, 193),
                    anchor="mm",
                )
                bx += 270
            sheet.paste(panel, (x, y + 90))
        y += ROWH
    return sheet


def _is_redish(px, alpha_floor=128):
    r, g, b, a = px if len(px) == 4 else (px[0], px[1], px[2], 255)
    return a >= alpha_floor and r >= 150 and g <= 80 and b <= 80


def probe_badges(v1, v2, v3):
    """Pixel probes: disc interior / ring-facet / glyph. Prints + asserts."""
    for kind, im in v1.items():
        disc = im.getpixel((512, 180))  # above glyph, inside ring
        ring = im.getpixel((512, 80))  # ring mid-top (radius ~432)
        if kind == "play":
            gly = im.getpixel((512, 512))
        elif kind == "pause":
            gly = im.getpixel((371, 512))  # left bar middle
        else:
            gly = im.getpixel((512, 512))
        print(
            "V1 %s raw disc=%s ring=%s glyph=%s"
            % (kind, tuple(disc), tuple(ring), tuple(gly))
        )
        assert tuple(disc)[:3] == (0x0B, 0x0B, 0x0B), "V1 %s disc not black" % kind
        assert tuple(ring)[:3] == (0xE5, 0x09, 0x14), "V1 %s ring not red" % kind
        assert tuple(gly)[:3] == (0xE5, 0x09, 0x14), "V1 %s glyph not red" % kind
    for kind, im in v3.items():
        disc = im.getpixel((512, 180))
        print("V3 %s raw disc=%s" % (kind, tuple(disc)))
        assert tuple(disc)[:3] == (0x8E, 0x06, 0x0C), "V3 %s disc not dark" % kind
    for kind, im in v2.items():
        disc = im.getpixel((512, 180))  # black interior above glyph
        print("V2 %s raw interior=%s" % (kind, tuple(disc)))
        assert tuple(disc)[:3] == (0x0B, 0x0B, 0x0B), "V2 %s interior not black" % kind
        # facet: walk the SE diagonal for a DARK pixel outside the black disc
        found = None
        for t in range(430, 520):
            off = int(t * 0.7071)
            p = (512 + off, 512 + off)
            if p[0] >= 1024 or p[1] >= 1024:
                continue
            px = im.getpixel(p)
            if tuple(px)[:3] == (0x8E, 0x06, 0x0C):
                found = (p, tuple(px))
                break
        print("V2 %s facet probe=%s" % (kind, found))
        assert found is not None, "V2 %s facet not found on SE diagonal" % kind


def legibility_24(v1):
    """Downscale V1 to 24x24; count red-ish ring + glyph pixels. Prints+asserts."""
    for kind, im in v1.items():
        sm = im.resize((24, 24), Image.LANCZOS)
        reds = [
            (x, y)
            for y in range(24)
            for x in range(24)
            if _is_redish(sm.getpixel((x, y)))
        ]
        core = [
            (x, y)
            for y in range(6, 18)
            for x in range(6, 18)
            if _is_redish(sm.getpixel((x, y)))
        ]
        print(
            "legibility24 V1 %s red_total=%d/576 red_core12x12=%d/144"
            % (kind, len(reds), len(core))
        )
        assert len(reds) >= 30, "V1 %s ring washed out at 24px" % kind
        assert len(core) >= 8, "V1 %s glyph lost at 24px" % kind


def border_check(named):
    """Outermost 2px border of each badge must be fully transparent."""
    for name, im in named.items():
        w, h = im.size
        bad, sample = 0, None
        for y in list(range(2)) + list(range(h - 2, h)):
            for x in range(w):
                if im.getpixel((x, y))[3] != 0:
                    bad += 1
                    sample = ((x, y), tuple(im.getpixel((x, y))))
        for y in range(2, h - 2):
            for x in list(range(2)) + list(range(w - 2, w)):
                if im.getpixel((x, y))[3] != 0:
                    bad += 1
                    sample = ((x, y), tuple(im.getpixel((x, y))))
        tl = tuple(im.getpixel((0, 0)))
        tr = tuple(im.getpixel((w - 1, 0)))
        print(
            "border %s nonzero=%d/8192 corners TL=%s TR=%s sample=%s"
            % (name, bad, tl, tr, sample)
        )
        assert bad == 0, "%s clipped: %d border pixels opaque" % (name, bad)


# Discord Rich Presence Invite "Cover Image" slot: 1024x576 RGBA.
# Discord draws over it: the app icon top-left, name/status lines beside it,
# and a full-width Join button across the bottom band. So: NO wordmark in the
# art (Discord draws the app name), atmospheric right-side interest only, and
# the top-left quadrant + bottom ~30% kept quiet and dark.
COVER_W, COVER_H = 1024, 576
COVER_TEXT_BOX = (140, 24, 512, 140)  # name/status lines beside the icon
COVER_JOIN_BOX = (24, 462, 1000, 540)  # Join button footprint
COVER_BAND_Y = 403  # bottom band starts (~70% of height)

COVER_BLURBS = {
    "C1": "C1 PRIMARY: black field + dark-red facet sweep, watermark P right third",
    "C2": "C2: black field, full-red mark center-right, soft dark-red vignette",
    "C3": "C3: black field, film-strip sprocket motif right edge, small mark lower right",
}


def _paste_mark(base, box, opacity=1.0):
    """Full Ribbon-P (RED face / DARK plane / BLACK counter) over a 1x base."""
    x0, y0, x1, y1 = box
    mw = x1 - x0
    pad = int(mw * 0.08) + 1
    layer = new((mw + 2 * pad, mw + 2 * pad))
    ribbon_p(layer, (pad, pad, pad + mw, pad + mw))
    mark = down(layer)
    if opacity < 1.0:
        mark.putalpha(mark.split()[3].point(lambda v: int(v * opacity + 0.5)))
    base.alpha_composite(mark, (int(x0 - pad), int(y0 - pad)))
    return base


def render_cover_c1():
    """C1 PRIMARY: black field, DARK diagonal facet sweep, watermark P right."""
    img = new((COVER_W, COVER_H), BLACK)
    d = ImageDraw.Draw(img)
    outer = [(560, 0), (1024, 0), (1024, 240), (780, 400), (600, 400)]
    d.polygon([(x * SS, y * SS) for x, y in outer], fill=DARK)
    crease = [(772, 0), (812, 0), (602, 400), (567, 400)]
    d.polygon([(x * SS, y * SS) for x, y in crease], fill=BLACK)
    base = down(img)
    _paste_mark(base, (700, 50, 996, 346), opacity=0.16)
    return base


def render_cover_c2():
    """C2: black field, full-RED mark center-right, soft DARK vignette right."""
    base = down(new((COVER_W, COVER_H), BLACK))
    vig = Image.new("RGBA", (COVER_W, COVER_H), (0, 0, 0, 0))
    ImageDraw.Draw(vig).ellipse([560, -60, 1100, 460], fill=DARK)
    vig = vig.filter(ImageFilter.GaussianBlur(70))
    vig.putalpha(vig.split()[3].point(lambda v: int(v * 0.45 + 0.5)))
    base.alpha_composite(vig)
    _paste_mark(base, (670, 120, 880, 330))
    return base


def render_cover_c3():
    """C3: black field, DARK film strip + sprocket holes, right edge only."""
    img = new((COVER_W, COVER_H), BLACK)
    d = ImageDraw.Draw(img)
    d.rectangle([946 * SS, 0, 1024 * SS, 392 * SS], fill=DARK)
    for y in range(28, 360, 56):
        rr(d, (968, y, 1002, y + 26), 6, BLACK)
    base = down(img)
    _paste_mark(base, (800, 240, 930, 370))
    return base


def mock_invite_overlay(cover):
    """Mock Discord invite card over a cover copy (preview sheet only)."""
    im = cover.copy()
    d = ImageDraw.Draw(im)
    im.alpha_composite(render_mark_tile(88), (24, 24))
    d.text((132, 30), "Potflix", font=load_inter(36, "Bold"), fill=WHITE)
    d.text(
        (132, 72),
        "Watching The Rising of the Shield Hero",
        font=load_inter(24, "Regular"),
        fill=(0xB5, 0xBA, 0xC1, 255),
    )
    d.rounded_rectangle([24, 462, 1000, 540], radius=14, fill=WHITE)
    d.text(
        (512, 501),
        "Join",
        font=load_inter(30, "Bold"),
        fill=(0x0B, 0x0B, 0x0B, 255),
        anchor="mm",
    )
    return im


def cover_variants_sheet(covers):
    """Labelled sheet: C1/C2/C3 plain + with mock invite overlay, full size."""
    W, HEAD, LABEL = 1024, 150, 64
    order = ("C1", "C2", "C3")
    sheet = Image.new(
        "RGB", (W, HEAD + len(order) * 2 * (LABEL + COVER_H)), (26, 27, 30)
    )
    d = ImageDraw.Draw(sheet)
    d.text(
        (W // 2, 52),
        "POTFLIX DISCORD COVERS - INVITE IMAGE VARIANTS",
        font=load_inter(44, "Bold"),
        fill=WHITE,
        anchor="mm",
    )
    d.text(
        (W // 2, 108),
        "1024x576 RGBA . no wordmark in art . mock invite overlay below each",
        font=load_inter(24, "Regular"),
        fill=CARD_MUT[:3],
        anchor="mm",
    )
    y = HEAD
    for key in order:
        for tag, im in (
            ("plain", covers[key]),
            ("+ mock invite overlay", mock_invite_overlay(covers[key])),
        ):
            label = (
                "%s . plain" % COVER_BLURBS[key]
                if tag == "plain"
                else "%s . %s" % (COVER_BLURBS[key], tag)
            )
            d.text((24, y + 14), label, font=load_inter(26, "Bold"), fill=WHITE)
            y += LABEL
            sheet.paste(im.convert("RGB"), (0, y))
            y += COVER_H
    return sheet


def cover_region_stats(im, box):
    """Mean luminance + population std over a 1x box (Rec.709 luma)."""
    import statistics

    x0, y0, x1, y1 = box
    vals = [
        0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]
        for y in range(y0, y1)
        for x in range(x0, x1)
        for p in [im.getpixel((x, y))]
    ]
    return sum(vals) / len(vals), statistics.pstdev(vals)


def probe_cover(c1):
    """Overlay-legibility + wordmark-absence + palette report for C1."""
    for name, box in (("text", COVER_TEXT_BOX), ("join", COVER_JOIN_BOX)):
        mean, sd = cover_region_stats(c1, box)
        print(
            "cover legibility %s box=%s mean_lum=%.2f std=%.2f"
            " (gate: mean<=40.0 std<=20.0 for white overlay text)"
            % (name, box, mean, sd)
        )
        assert mean <= 40.0 and sd <= 20.0, "cover %s region too bright/busy" % name
    raw = c1.tobytes()
    n = COVER_W * COVER_H
    whites = sum(1 for i in range(0, len(raw), 4) if min(raw[i : i + 3]) >= 240)
    print("cover wordmark check near-white pixels=%d/%d (gate: 0)" % (whites, n))
    assert whites == 0, "cover contains wordmark-like white pixels"
    from collections import Counter

    cnt = Counter(tuple(raw[i : i + 3]) for i in range(0, len(raw), 4))
    top = cnt.most_common(8)
    print(
        "cover distinct colours=%d top=%s"
        % (len(cnt), [("#%02X%02X%02X" % c, k) for c, k in top])
    )
    brand = [(0xE5, 0x09, 0x14), (0x8E, 0x06, 0x0C), (0x0B, 0x0B, 0x0B)]
    near = sum(
        k
        for c, k in cnt.items()
        if min(max(abs(c[i] - b[i]) for i in range(3)) for b in brand) <= 8
    )
    print("cover palette near-brand(<=8 per channel)=%.4f" % (near / n))


def sha256_file(path):
    import hashlib

    h = hashlib.sha256()
    with open(path, "rb") as f:
        h.update(f.read())
    return h.hexdigest()


PROTECTED = [
    "assets/discord/logo.png",
    "assets/discord/play.png",
    "assets/discord/pause.png",
    "assets/discord/stop.png",
    "assets/appicon.png",
    "assets/appicon-square.png",
    "assets/icon.ico",
]


def fmt(s):
    return "%d:%02d" % (s // 60, s % 60)


def card(
    w,
    h,
    header,
    details,
    state,
    bar=None,
    countup=None,
    buttons=False,
    large_img=None,
    small_img=None,
):
    img = new((w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    rr(d, (0, 0, w, h), 10, CARD)
    fH = load_inter(30, b"Bold")
    fD = load_inter(26, b"Medium")
    fS = load_inter(24, b"Regular")
    fT = load_inter(22, b"Regular")
    th = h - 72
    lg = large_img.resize((th * SS, th * SS), Image.LANCZOS)
    img.alpha_composite(lg, (int(36 * SS), int(36 * SS)))
    sm = small_img.resize((int(th * 0.36 * SS), int(th * 0.36 * SS)), Image.LANCZOS)
    img.alpha_composite(sm, (int((36 + th * 0.64) * SS), int((36 + th * 0.64) * SS)))
    x, y = 36 + th + 32, 52
    for txt, fnt, col in (
        (header, fH, CARD_HDR),
        (details, fD, CARD_DET),
        (state, fS, CARD_MUT),
    ):
        if txt:
            d.text((x * SS, y * SS), txt, font=fnt, fill=col)
            y += 46
    if bar is not None:
        el, tot = bar
        y += 12
        bw = w - x - 120
        frac = max(0.0, min(1.0, el / float(tot)))
        d.rounded_rectangle(
            [x * SS, y * SS, (x + bw) * SS, (y + 12) * SS],
            radius=6 * SS,
            fill=(0x42, 0x44, 0x4A, 255),
        )
        if frac > 0:
            d.rounded_rectangle(
                [x * SS, y * SS, (x + bw * frac) * SS, (y + 12) * SS],
                radius=6 * SS,
                fill=RED,
            )
        y += 26
        d.text((x * SS, y * SS), fmt(el), font=fT, fill=CARD_MUT)
        d.text(((x + bw) * SS, y * SS), fmt(tot), font=fT, fill=CARD_MUT, anchor="ra")
    elif countup is not None:
        y += 12
        d.text((x * SS, y * SS), "%s elapsed" % fmt(countup), font=fT, fill=CARD_MUT)
    if buttons:
        y += 64
        fB = load_inter(24, b"Bold")
        bx = x
        for lab in ("Watch Episode", "View Series"):
            bw2 = d.textlength(lab, font=fB) + 56
            d.rounded_rectangle(
                [bx * SS, y * SS, (bx + bw2) * SS, (y + 48) * SS],
                radius=8 * SS,
                fill=(0x4E, 0x51, 0x5A, 255),
            )
            d.text(
                ((bx + bw2 / 2) * SS, (y + 24) * SS),
                lab,
                font=fB,
                fill=(0xC4, 0xC9, 0xCE, 255),
                anchor="mm",
            )
            d.line(
                [bx * SS, (y + 24) * SS, (bx + bw2) * SS, (y + 24) * SS],
                fill=RED,
                width=4 * SS,
            )
            bx += bw2 + 24
        d.text(
            (bx * SS + 16 * SS, (y + 24) * SS),
            "OWNER-EXCLUDED",
            font=fB,
            fill=RED,
            anchor="lm",
        )
    return down(img, "RGB")


def write_ico(im256, path):
    """ONE 256x256 BMP/DIB ICO frame; hand-rolled DIB fallback if Pillow emits PNG."""
    rgba = im256.convert("RGBA")
    rgba.save(path, format="ICO", sizes=[(256, 256)], bitmap_format="bmp")
    with open(path, "rb") as f:
        data = f.read()
    if data[22:26] == b"\x89PNG":
        rgba.save(path, format="ICO", sizes=[(256, 256)])  # placeholder replaced below
        data = manual_bmp_ico(rgba)
        with open(path, "wb") as f:
            f.write(data)
    return verify_ico(path)


def manual_bmp_ico(px):
    w, h = px.size
    xor = bytearray()
    for y in range(h - 1, -1, -1):
        for x in range(w):
            r, g, b, a = px.getpixel((x, y))
            xor += bytes((b, g, r, a))
    andmask = bytearray()
    for y in range(h - 1, -1, -1):
        row = bytearray()
        bits = 0
        for x in range(w):
            bits = (bits << 1) | (1 if px.getpixel((x, y))[3] < 128 else 0)
            if x % 8 == 7:
                row.append(bits)
                bits = 0
        andmask += row
    dib = (
        struct.pack("<IIIHHIIIIII", 40, w, h * 2, 1, 32, 0, len(xor), 0, 0, 0, 0)
        + bytes(xor)
        + bytes(andmask)
    )
    return (
        struct.pack("<HHH", 0, 1, 1)
        + struct.pack("<BBBBHHII", 0, 0, 0, 0, 1, 32, len(dib), 22)
        + dib
    )


def verify_ico(path):
    with open(path, "rb") as f:
        data = f.read()
    assert data[0:4] == b"\x00\x00\x01\x00", "not an ICO"
    count = struct.unpack("<H", data[4:6])[0]
    assert count == 1, "want 1 frame, got %d" % count
    _w, _h = data[6], data[7]
    _pl, _bpp = struct.unpack("<HH", data[10:14])
    _sz, off = struct.unpack("<II", data[14:22])
    magic = data[off : off + 16]
    print(
        "ico frames=%d size=%dx%d bpp=%d frame_off=%d magic=%s"
        % (count, _w or 256, _h or 256, _bpp, off, magic.hex(" "))
    )
    assert magic[:4] == b"\x28\x00\x00\x00", (
        "frame is NOT BITMAPINFOHEADER: %s" % magic.hex(" ")
    )
    return magic


def check_mark_32():
    """Ribbon P glyph alone at 32px: nontrivial ink in every quadrant."""
    img = new((32, 32))
    ribbon_p(img, (0, 0, 32, 32))
    m = down(img)
    assert m.size == (32, 32)
    cov = []
    for qx, qy in ((0, 0), (16, 0), (0, 16), (16, 16)):
        n = sum(
            1
            for y in range(16)
            for x in range(16)
            if m.getpixel((qx + x, qy + y))[3] > 128
        )
        cov.append(n / 256.0)
    print("mark32 quadrant alpha coverage TL=%.3f TR=%.3f BL=%.3f BR=%.3f" % tuple(cov))
    assert all(c > 0.02 for c in cov), "mark fails 32px quadrant check"
    return cov


def main():
    P = lambda *a: os.path.join(ROOT, *a)
    os.makedirs(P("design", "preview"), exist_ok=True)
    os.makedirs(P("assets", "discord"), exist_ok=True)
    sha_before = {}
    for rel in PROTECTED:
        fp = P(*rel.split("/"))
        sha_before[rel] = sha256_file(fp) if os.path.exists(fp) else "MISSING"

    stale = P("design", "preview", "logo-concepts.png")
    if os.path.exists(stale):
        os.remove(stale)
        print("removed superseded %s" % stale)

    sheet, fams = candidate_sheet()
    sheet.save(P("design", "preview", "logo-candidates.png"))

    # PROTECTED (brief boundary): logo/appicons/ico are NOT rewritten here.
    # Render the lockup in memory only (wordmark font proof), read the large
    # card art from disk.
    _, lockfam = render_lockup(1024, CHOSEN)
    v1, v2, v3 = {}, {}, {}
    for k in ("play", "pause", "stop"):
        v1[k] = render_badge_v1(1024, k)
        v2[k] = render_badge_v2(1024, k)
        v3[k] = render_badge_v3(1024, k)
    kinds = {}
    for k in ("play", "pause", "stop"):
        b = v1[k]
        b.save(P("assets", "discord", "%s.png" % k))
        kinds[k] = b
    badges = {}
    badges.update({("V1", k): v1[k] for k in v1})
    badges.update({("V2", k): v2[k] for k in v2})
    badges.update({("V3", k): v3[k] for k in v3})
    badges_variants_sheet(badges).save(P("design", "preview", "badges-variants.png"))
    print(
        "protected-skip logo.png appicon.png appicon-square.png icon.ico"
    )

    W, H = 1100, 340
    lg_btn = Image.open(P("assets", "discord", "logo.png")).convert("RGBA")
    cards = [
        (
            "card-before.png",
            "Watching PotFlix",
            "The Rising of the Shield Hero",
            'S1 E13 "The Shield Demon"',
            dict(small_kind="play", bar=(400, 1420)),
        ),
        (
            "card-after.png",
            "Watching The Rising of the Shield Hero",
            "The Shield Demon",
            "S1 E13",
            dict(small_kind="play", bar=(400, 1420)),
        ),
        (
            "card-after-movie.png",
            "Watching Dune Part Two (2024)",
            "Dune Part Two (2024)",
            "",
            dict(small_kind="play", bar=(400, 1420)),
        ),
        (
            "card-after-paused.png",
            "Watching The Rising of the Shield Hero",
            "The Shield Demon",
            "S1 E13 - Paused",
            dict(small_kind="pause", countup=400),
        ),
        (
            "card-netflix-reference.png",
            "Watching The Rising of the Shield Hero",
            "The Shield Demon",
            "S1 E13",
            dict(small_kind="play", bar=(400, 1420), buttons=True),
        ),
    ]
    for name, hdr, det, st, kw in cards:
        small = kinds[kw.pop("small_kind")]
        card(W, H, hdr, det, st, large_img=lg_btn, small_img=small, **kw).save(
            P("design", "preview", name)
        )

    covers = {
        "C1": render_cover_c1(),
        "C2": render_cover_c2(),
        "C3": render_cover_c3(),
    }
    for key, im in covers.items():
        assert im.size == (COVER_W, COVER_H) and im.mode == "RGBA", key
    covers["C1"].save(P("assets", "discord", "cover.png"))
    cover_variants_sheet(covers).save(P("design", "preview", "cover-variants.png"))

    from PIL import Image as I

    print("palette " + PALETTE_REPORT)
    for rel in [
        "design/preview/logo-candidates.png",
        "assets/discord/play.png",
        "assets/discord/pause.png",
        "assets/discord/stop.png",
        "assets/discord/cover.png",
        "design/preview/badges-variants.png",
        "design/preview/cover-variants.png",
        "design/preview/card-before.png",
        "design/preview/card-after.png",
        "design/preview/card-after-movie.png",
        "design/preview/card-after-paused.png",
        "design/preview/card-netflix-reference.png",
    ]:
        im = I.open(P(*rel.split("/")))
        print("%s %dx%d %s" % (rel, im.size[0], im.size[1], im.mode))
    for k, fam in list(fams.items()) + [("lockup:" + CHOSEN, lockfam)]:
        print("wordmark font %s -> getname family=%r" % (k, fam))
    check_mark_32()
    for k in ("play", "pause", "stop"):
        for vn, d in (("V1", v1), ("V2", v2), ("V3", v3)):
            im = d[k]
            assert im.size == (1024, 1024) and im.mode == "RGBA"
    probe_badges(v1, v2, v3)
    legibility_24(v1)
    border_check(
        {
            "V1 play": v1["play"],
            "V1 pause": v1["pause"],
            "V1 stop": v1["stop"],
            "V2 play": v2["play"],
            "V2 pause": v2["pause"],
            "V2 stop": v2["stop"],
            "V3 play": v3["play"],
            "V3 pause": v3["pause"],
            "V3 stop": v3["stop"],
        }
    )
    print(
        "badge geometry DISC_D=%.3f RING_W=%.3f V2_DISC=%.3f FACET_DX=%.3f FACET_DY=%.3f (fractions of 1024)"
        % (BADGE_DISC_D, BADGE_RING_W, BADGE_V2_D, BADGE_FACET_DX, BADGE_FACET_DY)
    )
    print("cover.png bytes=%d" % os.path.getsize(P("assets", "discord", "cover.png")))
    probe_cover(covers["C1"])
    for rel, want in (
        ("design/preview/badges-variants.png", (1800, 1850)),
        ("design/preview/card-before.png", (1100, 340)),
        ("design/preview/card-after.png", (1100, 340)),
        ("design/preview/card-after-movie.png", (1100, 340)),
        ("design/preview/card-after-paused.png", (1100, 340)),
        ("design/preview/card-netflix-reference.png", (1100, 340)),
    ):
        got = I.open(P(*rel.split("/"))).size
        print("carddim %s %dx%d" % (rel, got[0], got[1]))
        assert tuple(got) == want, "%s dim %s != %s" % (rel, got, want)
    for rel in PROTECTED:
        after = sha256_file(P(*rel.split("/")))
        same = after == sha_before[rel]
        print(
            "sha %s before=%s after=%s unchanged=%s"
            % (rel, sha_before[rel][:12], after[:12], same)
        )
        assert same, "%s changed by run" % rel


if __name__ == "__main__":
    main()
