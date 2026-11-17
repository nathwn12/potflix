# Workspace phase (approved; implemented in source; not installed)

**Status:** approved by the owner 2026-10-29. This file described the
split-workspace A direction; **as of 2026-10-29 it is superseded as a design
lock by `DESIGN-UNLOCK.md`**, which re-cuts the flow around one object (the
title PotPlayer is playing) and carries the current thresholds. The history
below is kept as written; the "design lock" claim no longer binds. As of 2026-10-29 the working tree implements
the direction in source (items 1, 2, 3, and 5 below); nothing in it is
installed or released. The installed build is **1.1.0** (binary dated
2026-10-28), built before these changes, so *shipped* in this file means that
installed build, never this tree. Source implementation is not installation
and not proof: this document did not build, install, or run the app, and the
acceptance gate at the end is unchanged.

## Goal

One window where the library work and a live RPC designer share the surface:
the working controls stay in view while the designer shows what Discord will
draw. The installed 1.1.0 build is the 460x680 single-column shape with Card
and Settings tabs (`rust\crates\potflix-win\src\window.rs`, `MAIN_CLIENT_W` /
`MAIN_CLIENT_H` at `:508-509`, `tab_strip_rects` at `:664`; the 1.1.0 source
at HEAD opens at that size, `git show HEAD:rust\crates\potflix-win\src\window.rs`,
lines 8813-8815).
The working tree keeps that opening size and adds the split workspace at
1180x700 and above: a larger surface with the working controls and the
designer visible together (`compute_card_layout` at `:1410-1417`, `wide` at
`:1421-1423`; `main_window_style` at `:10945-10952` makes the window
resizable).

## Approved direction (in the working tree; not installed)

1. **One title query field, with the year in the same field — implemented in
   the working tree, not installed.** The title search is a single input that
   carries an optional year in the same field; there is no second Year
   control. `parse_title_year` splits a trailing `19xx`/`20xx` token into the
   year, leaves the rest as the title, and treats a bare year as a year-only
   query (`rust\crates\potflix-win\src\window.rs:3936-3954`). The dialog
   creates one borderless `FM_ID_NAME` edit inside an owner-drawn frame and
   creates no year control (`window.rs:5186-5190`); the working tree has no
   `FM_ID_YEAR` symbol. Both the provider search and the provider-free latch
   read the pair from `parsed_query()` over that one field
   (`window.rs:6028-6032`, `:6380`, `:6456`). The installed 1.1.0 build still
   paints the old two-control shape, a `Name` field and a separate
   `Year  (optional)` field (the 1.1.0 source at HEAD still defines
   `FM_ID_YEAR`; `git show HEAD:rust\crates\potflix-win\src\window.rs`, lines
   3290 and 4226), so the unified field is in the tree but not installed. The
   lookup itself is **not new**: the provider search
   already takes a name, a year, and a kind preference
   (`rust\crates\metadata\src\search.rs:121`,
   `search(cancel, name, year, want: Kind)`), and `Fix match…` already calls
   it. This direction changes the input shape, not the lookup capability.
2. **Manual movie/TV selection — implemented in the working tree, not
   installed.** The search picker offers an explicit Movie/TV choice, so the
   user can override the inferred kind. The tree creates the two owner-drawn
   radios (`window.rs:4256-4257`, `:5193-5202`); the choice is seeded from the
   filename query when the dialog opens and from then on owns the search kind
   and the latch scope (`effective_is_movie` at `:4821-4827`). `search_want`
   feeds the provider search (`:6034-6041`, `:6384`), and switching kinds
   drops the old candidates and rebuilds the scope table (`apply_pick_kind`
   at `:6043-6064`). The installed 1.1.0 picker derives the kind and has no
   Movie/TV radios (the 1.1.0 source at HEAD has no `FM_ID_KIND_MOVIE`
   symbol), so the manual choice is in the tree but not installed. This is the
   search/latch picker; the Card view keeps its shipped rule that it follows
   the live card's kind and paints no Series/Movie toggle
   (`RUST-PORT.md:383-386`; `compute_card_layout` takes the displayed live
   kind, never a Card-view toggle, `window.rs:1415-1416`).
3. **A larger interactive RPC designer — implemented in the working tree, not
   installed.** The Card view grows into the designer: at 1180x700 and above
   the editor rail sits beside the live previews, and a field toggle
   recomposes the preview and the next wire frame together
   (`compute_card_layout` at `window.rs:1410-1417`, `wide` at `:1421-1423`,
   the wide branch and its rail at `:1573-1586`; `toggle_designer_field` at
   `:7894-7926` publishes through `presence::set_live_designer`). The preview
   engine and the live-apply path are **existing**: the preview renders
   through the same composer the send path uses and a preset save reaches the
   live card at the next poll tick (`RUST-PORT.md:374-391`); the designer is a
   field filter over that one composer, not a second preview engine
   (`rust\crates\presence\src\designer.rs:1-14`). The installed 1.1.0 build
   has neither the wide composition nor the toggles.
4. **Minimal dark UI with white controls.** Dark surfaces, white controls,
   minimal decoration. This supersedes the "Netflix-styled" bar in
   `UI-PHASE.md:33`. The 1.1.0 palette is already the dark `BLACK`/`CARD`
   surfaces with `WHITE` text and a brand `RED` accent
   (`rust\crates\potflix-win\src\brand.rs`), and the working tree keeps that
   palette (`window.rs:350-351` keeps the designer clear of the red selection
   indicator). This document does not claim the red accent is removed; the
   minimal frame is the part of this direction that is a judgement call, not
   a source fact.
5. **Simultaneous member-list / profile / Activity previews — implemented in
   the working tree, not installed.** In the wide composition all three
   surface rects are distinct and all three paint (`window.rs:1410-1417`,
   `:1573-1586`); the narrow composition keeps one selected surface at full
   width so no panel is clipped (`PreviewSurface` at `:2190-2196`). The
   installed 1.1.0 window has one preview panel and no surface switcher (the
   1.1.0 source at HEAD has no `PreviewSurface` symbol), so the three
   simultaneous previews are in the tree but not installed.

## Discord layout is inference

Discord's desktop client layout is not documented. The member-list, profile
popout, and Activity placement the designer models is an **inference** about
the client's layout, not a published contract and not a pixel-parity claim. Do not cite this document as evidence of Discord's actual geometry.
The previews are a local model; the only authoritative surface is the card
the client draws from `SET_ACTIVITY` (`rust\crates\discord\src\activity.rs`).
Prototype-only local fixtures are not documented here and are not shipped
fact.

## Invariants that do not move

- **No app-name header.** The card header is the media title; the client
  draws `Watching <media title>`, never `Watching Potflix`. `header_line` is
  the one definition the wire and the Card-view caption share
  (`rust\crates\presence\src\lib.rs:980`; `README.md` `## What you get`).
- **No small icon overlay.** The small slot is never populated, and the local
  preview paints no `P` mark, FILM/SERIES badge, or play/pause glyph
  (`rust\crates\presence\src\lib.rs:565`, `:1500`; `ARTWORK-PHASE.md`
  supersession note; `CHANGELOG.md` `[Unreleased]`).
- **Live metadata lookup is existing.** The keyless provider chain and its
  on-demand search are already shipped (`rust\crates\metadata\src\search.rs`,
  `Fix match…` in `window.rs`); the workspace direction introduces no new
  lookup capability.

## Superseded UI locks

The old window-shape lock, quoted exactly:

> All UI geometry is integer `scalePx = (v*dpi + 48) / 96` from a 460x680
> logical design, and font heights are negative.

(`RUST-PORT.md`, `## Windows-specific notes`.)

The old visual lock, quoted exactly:

> **Beautiful:** Netflix-styled layout, clear progress while scanning/refreshing.

(`UI-PHASE.md` `## Required controls`.)

The old search control shape, quoted exactly:

> the name field is pre-filled with the resolver's own parse of the playing
> title (so it reads `Attack on Titan`, never the raw filename), plus an
> optional year;

(`CHANGELOG.md`, `## [1.1.0]`.)

The approved split workspace supersedes the 460x680 single-column shape as a
fixed window lock and the "Netflix-styled" bar as the visual target. The
one-field query supersedes the separate name-plus-year control shape. The
integer-scaling rule itself (one scale function, no float geometry) is not
superseded. The installed 1.1.0 remains the 460x680 composition until a build
carrying the workspace is installed; the released 1.1.0 history stays as
written.

**Superseded 2026-10-29 by `DESIGN-UNLOCK.md`.** The wide-only designer
(>=1180x700) and this paragraph's gate no longer bind: the designer must also
exist at the narrow size the owner actually opened (463x703), and the flow is
re-cut by deltas D1-D7 in that file. The integer-scaling rule and the
`Watching <media title>` header survive unchanged.

## Acceptance (install/release gate)

The source-side shape is in the working tree; none of it counts until a build
of that tree passes this gate. Nothing below is claimed as met here.

Re-cut 2026-10-29 to `DESIGN-UNLOCK.md` D1-D7. The no-clipping line and the
header line are unchanged.

- **D1 (re-cut 2026-10-29):** from a cold launch with nothing playing, a
  matched **local preview** costs **three actions**: type the title, `Search`,
  pick a row; that preview is a local composition and nothing is sent.
  Changing what Discord actually shows for the playing file costs **four**:
  those three plus `Apply to playing title`, which appears only while something
  is playing and only after a row is picked. **Superseded 2026-10-29:** this
  row formerly required the whole path to fit in a single text entry plus a
  single confirm; that cap counted the player's own title as the user's input
  and left the "which record is this?" decision unpriced, so it is withdrawn.
  `DESIGN-UNLOCK.md` `## 1` carries the same numbers.
- **D2:** exactly one search-like control at rest; zero repair affordances
  outside a result list.
- **D3:** zero occurrences of the local account name in any surface.
- **D4:** zero states where a matched series renders `Movie` selected.
- **D5:** at most three settings controls visible at rest at 463x703; every
  payload field reachable within one disclosure.
- **D6:** no clipped or overlapping control at 1180x700 and 463x703 with
  disclosures open; every interactive target at least 32 px tall.
- **D7 (floor, unchanged):** the header is the media title, never the app
  name; the small slot stays empty; the preview paints no icon over the art.
- The existing gates still pass, and the live-render proof is captured after
  the last edit at both sizes. Static review is not visual proof.
- The installed binary is the build that carries this work, not 1.1.0; until
  that install is proved, this direction stays unshipped.

## Provenance

Requested by the owner after the split-workspace A design was approved
(2026-10-29). Repaired 2026-10-29 on a fresh context after a verifier found
this file still describing the pre-change tree's separate Year control; the
repair is documentation only and changed no source. The workspace source lane
runs in parallel; this lane changed documentation only.
