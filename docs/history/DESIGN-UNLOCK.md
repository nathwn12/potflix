# Design unlock: the first-time flow (approved scope, not built)

**Status:** unlock / design decision. This file suspends named UX locks for ONE
pass and predeclares what that pass must prove. It writes no product code and
claims nothing as shipped. Every claim below is a read of the working tree at
`f3e6b52` plus its uncommitted workspace pass; **no build was run, no binary was
launched, and no screenshot was taken for this document.** Anything that can
only be seen by running the app is marked `UNVERIFIED`.

Owner trigger (2026-10-29, installed 1.1.0 opened at 463x703): *"we have search,
but we have `fix match`"*, *"why is [the local Windows account name] the sample
user"*, *"the knobs are all over the place"*, *"genuinely confusing"*. The owner
asked for the break-it protocol (first-time user, user intentionally breaking
it) at the same time.

---

## 1. Objective

One falsifiable sentence:

> A first-time user, on a cold launch at 463x703 with PotPlayer playing
> `Better Call Saul`, reaches a correct, live-looking presence preview of that
> series in one visible flow - exactly one search entry point, no unexplained
> second match action, no local account name anywhere on screen, and no clipped
> or overlapping control at 463x703 or 1180x700 with disclosures open.

It is falsified by any of: a second visible search/match affordance at rest, the
local Windows account name or the sample handle that stood in for it in any
surface, a matched series with `Movie`
selected, a control outside the client rect at either size, or a tap target under
32 px.

**Implemented flow (2026-10-29 repair pass), stated honestly.** With nothing
playing the cold launch opens the search surface; the shortest path to a preview
is **three actions** - type the title, `Search`, pick a row - and that preview is
a local composition whose status line reads `Local preview · not sent to
Discord` (`LOCAL_PREVIEW_STATUS` in `window.rs`). Changing what Discord
actually shows for the playing file is **four** actions: the three above plus
`Apply to playing title`, which exists only while something is playing and only
after a row is picked. The earlier `<= 1 text entry
+ 1 confirm` threshold is withdrawn below (D1): it counted the player's own
title as the user's input and left the "which record is this?" decision
unpriced, and it is not what the implemented flow costs.

---

## 2. Suspension ledger

Each entry: what the doc says, what it mandates, why it stops binding for this
pass. Line numbers are the quoted text's own line.

| # | Doc:line | Quoted requirement | What it mandates | Why it no longer binds |
|---|---|---|---|---|
| L1 | `WORKSPACE-PHASE.md:3-6` | "This file is the design lock for the split-workspace A direction." | The A split-workspace is THE locked design; the pass may only finish it. | The owner rejected the resulting flow at 463x703. The lock was written with no first-time-user gate; it cannot outrank the user it was written for. |
| L2 | `WORKSPACE-PHASE.md:160-177` | "The source-side shape is in the working tree; none of it counts until a build of that tree passes this gate." | Acceptance = one field, picker kind choice, three simultaneous previews, no clipping, header = media title. | The *no-clipping* and *header* lines stay (floor). The one-field / kind-choice / three-preview lines are re-cut into deltas D1-D7, which are stricter. |
| L3 | `WORKSPACE-PHASE.md:143-149` | "The approved split workspace supersedes the 460x680 single-column shape as a fixed window lock" | The designer lives at >=1180x700 and the narrow window is a reduced stack. | At 463x703 the wide branch is unreachable (`window.rs:1423`), so the "larger interactive RPC designer" is invisible in the only size the owner actually opened. The designer must also exist at the narrow size. |
| L4 | `UI-PHASE.md:33` | "**Beautiful:** Netflix-styled layout, clear progress while scanning/refreshing." | Netflix visual language for the window. | Already superseded by `WORKSPACE-PHASE.md:78`; carried here so no lane re-derives it. Minimal dark + white controls replaces it. |
| L5 | `README.md:18` | "First run opens the settings window: add a media folder, then Scan." | Onboarding is settings-first, scan-first. | The owner's first complaint is the flow, not the scan. Settings-first makes the first screen a folder list with no title field. Onboarding becomes search-first; settings move behind one disclosure. |
| L6 | `README.md:26` | "use Fix match.: type the name (plus the year when you know it), Search, pick the candidate, choose this season or the whole show, and Match." | Five-step documented match path, named "Fix match". | This is the confusion the owner named. One search entry + one confirm replaces it; the doc's step list is the thing being deleted. |
| L7 | `README.md:43` | "there is no Series/Movie toggle" | The Card view must not offer a kind toggle. | False in the working tree already: the picker paints `Movie`/`TV` (`window.rs:2012-2022`, `:10551-10563`). The lock is suspended and replaced by D4 (kind is derived and displayed, not chosen). |
| L8 | `RUST-PORT.md:103-106` | "All UI geometry is integer `scalePx = (v*dpi + 48) / 96` from a 460x680 logical design" | 460x680 is the design size; every rect is integer-scaled from it. | The *integer scaling rule* stays. The 460x680 design size stops being the acceptance size; 463x703 is the test floor and 1180x700 the ceiling (`D6`). |
| L9 | `RUST-PORT.md:393-398` | "`Fix match.` (`rust\crates\potflix-win\src\window.rs`, `FIX_MATCH_*` copy ...)" | `Fix match` is *the* picker and its copy is parity-frozen. | The copy is exactly the second entry point the owner rejected. The parity lock covers wire shape, which does not move; the picker's UI name and step count do. |
| L10 | `AGENTS.md:3` | "Netflix-styled fork of `ayunami2000/PotPlayer-DiscordRPC`" | Repo self-description carries the Netflix styling. | Stale after L4; a lane reading AGENTS.md would re-apply the rejected bar. |

`docs\` does not exist in this repo (`Get-ChildItem -Recurse -Filter *.md -Path docs` returned nothing) — there is no fourth lock file to find.

`DECISION.md` (1-36) and `ARTWORK-PHASE.md` (12-26) contain **no** suspended UX
lock: the first is the Rust-vs-Go decision and parity oracle, the second is the
icon/artwork rule, which is floor (F5 below), not a suspension.

---

## 3. Immutable floor (not suspended; restated for this pass)

- **Accessibility:** every control reachable and operable by keyboard, a visible
  focus ring at all times, semantics carried by text (not colour alone), body
  text at AA contrast on its own surface, and no motion that ignores the
  reduced-motion setting.
- **Performance:** nothing in this pass may break the build or the deploy path;
  a change that cannot build `--locked` with `-D warnings` is not a design
  option.
- **Verification independence:** the verdict on this pass comes from a seat that
  wrote none of it, on a fresh context; a rendered artifact is required, and
  static review is never visual proof.
- **Facts and claims:** every claim carries a citable command, output or
  `file:line`; unavailable evidence is `UNVERIFIED`, never inferred.
- **Custody and secrets:** `client-id.txt`, `*.env*`, `*secrets*` stay unopened
  and unquoted; no secret value enters a brief, a doc, a log or a commit.
- **Destructive acts:** no deletion of a user's stored match, config, cache or
  library index in this pass; a destructive control stays behind its existing
  confirmation.
- **Outward-text gates:** PR body, commit message, tag message, release note and
  issue carry no absolute path, drive letter, temp path, session id, local user
  name or host name.

Additional shipped-product invariants that this pass does **not** touch (they
are floor by prior owner decision, restated only because the header and cover
are in scope):

- F5 — header is `Watching <media title>`, never `Watching Potflix`
  (`presence\src\lib.rs:975-985`).
- F6 — no icon, logo, type badge or play/pause glyph on or instead of the cover;
  the small slot is never populated (`presence\src\designer.rs:60-65`,
  `:85-87`).
- F7 — apart from the one exception recorded below, the wire shape and the
  composer are unchanged; a UI pass may not alter `SET_ACTIVITY` payload
  semantics (`presence\src\designer.rs:1-14`).
  - **F7 exception, recorded 2026-10-30 — this pass did change one wire value.**
    The branded large-image fallback was removed. At `f3e6b52` a card whose
    artwork was missing, not `http(s)`, or over the 300-char budget sent
    `large_image` equal to the app's registered `logo` asset
    (`presence\src\lib.rs:957-968`, the fallback at `:966`); the pass sends an
    empty string instead (`presence\src\lib.rs:997-1011`), asserted at
    `presence\src\presence_test.rs:944-965`, `:1595-1605`, `:2314`. Reason: the
    branded mark contradicted the F6 boundary above — it put the Potflix logo
    on the cover of exactly the files that have no poster — so keeping it while
    claiming no branded artwork would have been untrue. Scope is that one value
    and nothing else: header, body lines, budgets, timestamps and every other
    composer field are untouched, and `README.md` already describes the empty
    slot. The invariant stands for the rest of the pass; this is its only
    exception.

---

## 4. Named deltas and predeclared thresholds

"Today" is measured from source, not from a run. Each number cites where the
behaviour is decided.

| # | Delta | Today (measured in source) | Threshold after the pass |
|---|---|---|---|
| D1 | Primary flow: actions from launch to a matched preview | **>= 4 actions** and only with a player: launch landed on Main (`window.rs:3451`), Main has no search at all (action grid `window.rs:867-921` = Scan/Refresh/Add folder/Remove/Report/Close to tray), so click `Card` (`window.rs:1987`), click the field (`window.rs:2006-2011`, handler `:7645-7648`), type, `Search` (`:2024-2029`), then `Match` (`:10565-10570` relabelled the same button; `card_picker_match` `:7778`). | **Superseded 2026-10-29 (implemented): 3 actions to a local preview, 4 to change Discord.** Nothing playing: the cold launch opens the search surface, then type + `Search` + pick a row = 3, and the result's status line reads `Local preview · not sent to Discord`. Something playing: the same 3 plus `Apply to playing title` = 4, and only that writes the match. The earlier `<= 1 text entry + 1 confirm` bar is WITHDRAWN: it priced the pick as free. Reader justification: the pick is the decision this screen exists for, so it is counted, and no label hides a second verb behind the first button. |
| D2 | Duplicate concepts: search inputs / match affordances | **2 search-like affordances at rest** in the Card view: the picker `Search` (`window.rs:2024-2029`) and `Fix match.` (`:2077-2087`); a third `Search` existed inside the dialog (`:3650`). | **Exactly 1 search input and 1 search verb** at rest, fixed at `Search`: the same button never relabels itself `Match`. The only match/repair affordances live inside the results list: `Apply to playing title` (only with a player and a selected row) and `Not this one? Open advanced match…`. The tray `Fix match` entry is the same criteria dialog reached from outside the window and is documented as such in `README.md`. Justification: two verbs with one job is the literal complaint. |
| D3 | Identity string | **2 occurrences of the local account name** at `f3e6b52` (`window.rs:10294`, `:10380`) plus the sample handle (`:10386`) and two `N` avatar glyphs (`:10286`, `:10372`); the 2026-10-29 repair removed all of them (0 hits for the account name in the working tree). No live identity exists anywhere: `username`, `display_name` and `global_name` have **0** hits across `discord\src\` and `presence\src\`. | **0** occurrences of the local account name in any surface. The preview shows the connected Discord identity, or the literal **`You`** when no identity is available. Justification: the sample user is a local machine name that will ship to strangers and reads as demo data in a live preview. |
| D4 | Kind truthfulness | The picker's kind was **user state, never resolved state**: a `movie: bool` on `MediaPicker` (`search.rs:23`) set only by `set_kind`, seeded from the filename and from the dialog's own seed, and painted straight through; the match path never wrote the matched kind back. | **0** states where a series shows as a film. `movie: bool` is gone: the search carries an explicit `KindFilter` (`All types` default, `Movies`, `Series` behind the `Advanced` disclosure) that shapes the SEARCH only, and the record a row resolves owns the preview's kind and year (the selection composes from the candidate). Justification: a state indicator that lies about the thing it is indicating is worse than no indicator. |
| D5 | Knob density at rest, 463x703 | **20 interactive controls** at rest in the shipped v1 Card/narrow composition; **10** in Settings plus 2 tabs. | **<= 3** settings controls visible at rest at 463x703; every field toggle, the line-2 state row and the search scope still reachable **within one disclosure**. At rest the Card view lays out exactly 6 controls: two tabs, the field, `Search`, `Advanced`, `Back`. Justification: "knobs all over the place" is a count complaint; one disclosure keeps every knob reachable without deleting a capability. |
| D6 | No clipping / overlap, and target size | Pinned only at **460x680**: `window.rs:11813-11829`, `:11997-12015`, `:13910-13911`. There is **no** 463x703 or 1180x700 assertion anywhere (`463`/`703` have 0 hits in `window.rs` and `examples\uicapture.rs`). | **No** clipped or overlapping control at **1180x700 and 463x703** with disclosures open, and **every interactive target >= 32 px tall**. Justification: two fixed sizes are the only evidence a renderer can actually produce. |
| D7 | Header | `Watching <media title>` (`presence\src\lib.rs:975-985`), asserted never to contain the app name (`presence\src\presence_test.rs:3591-3593`). | **Unchanged.** The header remains `Watching <media title>`. Justification: this is the one identity the owner never questioned; it is floor (F5) as well as a delta so no lane "improves" it. |

---

## 5. Break-it matrix

Current behaviour is read from source; "after" is the contract the pass must
meet. `UNVERIFIED` means the behaviour is decided inside a live window, a real
provider response, or a Discord client and cannot be read from the tree.

| # | Scenario | Current behaviour (decided at) | After the pass |
|---|---|---|---|
| B1 | First-time cold open, nothing playing | Main view, whose only actions are library actions (`window.rs:867-921`); Card view preview is empty because `preview_kind` is `None` (`:8117-8127`); the query field is seeded only while something plays (`:7018-7024`). **No path to a preview at all.** | One field on the first screen, usable with nothing playing; a preview appears from the resolved match, not from a player. |
| B2 | Wrong-kind: series title, `Movie` preselected | `prime_picker` seeds the flag once (`window.rs:7030`); afterwards the flag persists across Card visits (`:7019` returns early on a non-empty query) and the dialog inherits it verbatim (`:4996-5004`). The painted toggle follows the flag, not the card (`:10551-10563`). | Kind derives from the resolved candidate; the control displays that kind; a series can never show `Movie` lit. |
| B3 | Ambiguous title (`Dune` 1984 vs 2021; `The Office` US vs UK; film vs series of one name) | One result set filtered by the requested kind (`metadata\src\search.rs:88-96`, `:121-149`); rows are rendered with `Title (Year) · Provider` (`window.rs:3804`, asserted at `:15505`) and selected by index (`:7968-7978`). Region is not in the row. | Every candidate row carries year and provider; kind is part of the row, not a pre-filter the user must guess. Region stays `UNVERIFIED` (provider payload, not local code). |
| B4 | Empty submit | `search_allowed` false (`search.rs:55-57` via `search_query_allowed`); the button reports `Type a title or year first.` (`window.rs:7675`); the dialog shows `Find the right match` / hint (`:3621-3622`). | The confirm stays disabled and explains itself inline; no modal dead end. |
| B5 | 300-character title | Accepted with **no cap**: `on_card_char` appends any non-control char (`window.rs:7717-7741`), `set_query` stores it whole (`search.rs:68-78`). Display truncates later at `NAME_BUDGET = 128` (`presence\src\lib.rs:390`, `:959-973`) and ellipsizes in the rect (`window.rs:10301`, `:10403`). | Input is bounded at entry with a visible counter/limit; a 300-char paste cannot produce a silently different title than the user typed. |
| B6 | Emoji and `%%%` | Emoji pass the control-char filter (`window.rs:7734`) and every char is pushed (`:7737-7739`); no local validation of the query. Result depends on providers: 0 rows gives `No matches. Try a shorter title or add the year.` (`search.rs:124-126`). | Same acceptance, but the empty-result state names the query it failed on; rendering of astral chars is `UNVERIFIED`. |
| B7 | Year-only input | `parse_title_year` splits a trailing `19xx`/`20xx` and treats a bare year as year-only (`window.rs:3936-3954`; `1899` stays a title per the test at `:16761-16768`); `begin_search` then calls the provider with an empty name and that year (`search.rs:96-111`). Provider answer is `UNVERIFIED`. | Year-only is either a first-class search (with a stated result) or rejected in the field's own copy; never a silent empty result. |
| B8 | Provider offline / failure | Search failure posts `FixOutcome::Failed` → `FIX_MATCH_ERROR` = "Couldn't reach the metadata providers. Nothing was saved." (`window.rs:7882-7885`, `:3633`). | Same promise, shown inline next to the field, with the query preserved and a retry that does not require retyping. |
| B9 | Discord not running | **No preview is recorded at all.** `record_preview` is called only inside the `Ok(())` arm of `dc.set_activity(wire)` (`app\src\main.rs:433-443`), so the Card view keeps its empty default (`window.rs:8117-8127`). | The preview is a local composition and must render without a live socket; the connection state is reported separately. |
| B10 | Every RPC field toggled off | Header survives by construction: `DesignerSpec::apply` clears details/state/art/synopsis/timers and never `name` (`presence\src\designer.rs:60-89`). | Unchanged: header still `Watching <title>`; the empty-body card is a legitimate state with an explanatory line, not a blank panel. |
| B11 | Rapid repeated Enter | `begin_search` no-ops while busy (`search.rs:96-99`); `card_picker_match` returns early while busy (`window.rs:7779-7781`); a stale worker post is rejected by generation (`search.rs:115-118`, `:7862-7873`). | Unchanged; one search in flight, no duplicate latch, no flicker. |
| B12 | 120-character title overflow | Survives the 128-unit header budget (`presence\src\lib.rs:390`); the preview title rect ellipsizes (`window.rs:10403`) and the narrow title rect is fixed-height (`:1928-1933`). Whether 120 chars fit at 463x703 is `UNVERIFIED` (no render). | Measured in a render at both sizes; the title truncates at a word boundary with a visible ellipsis and never pushes the meta row. |
| B13 | Resize mid-flow 1180x700 ↔ 463x703 | Layout is pure in `(cw, ch)` (`window.rs:1410-1423`); the wide branch needs both >=1180 and >=700 (`:1423`), so crossing it collapses three surfaces to one (`:1867-1912`). Picker state (`open`, `candidates`, `selected`) is model state and survives a resize (`search.rs:19-34`). Visual continuity is `UNVERIFIED`. | Crossing the breakpoint keeps the query, the result set and the selection; nothing the user typed is lost and no control lands outside the client rect. |

Grounded in code: B1-B13 except the explicitly marked provider/visual/Discord
halves of B3, B6, B7, B12, B13, and B9's real-socket behaviour.

---

## 6. Exploration budget and stop

Two divergent directions, time-boxed, then converge and ship a render.

- **Direction A — the single command bar (20 min).** The first screen is one
  field and one confirm. Results appear inline and each row states its kind and
  year, so kind is never a control (`D4`). `Fix match…` ceases to exist as a
  verb; repair is a row action on an existing selection (`D2`). Settings become
  one disclosure.
- **Direction B — two modes (20 min).** `Watch` (search + preview) and
  `Designer` (the larger interactive RPC designer, available at 463x703 as well
  as 1180x700), with Settings as a drawer behind one control.

**Converge by one named rule:** whichever direction reaches the target with the
fewer *new* state machines wins, and the loser's best element is folded in.
Today the only existing model is `MediaPicker` (`search.rs:13-34`), so a
direction that reuses it unmodified wins on that rule; a direction that needs a
second search model is rejected regardless of how it looks.

**Stop:** at 40 minutes of exploration, converge on the rule above, implement
nothing beyond the chosen direction, and produce a rendered artifact from
`rust\crates\potflix-win\examples\uicapture.rs` at **463x703** and **1180x700**
with disclosures open. No verdict without those two images; a static diff is not
a result.

---

## 7. Retirement plan

Edited, deleted, or marked superseded **in the same pass** as the implementation:

| Target | Action |
|---|---|
| `WORKSPACE-PHASE.md:3-6`, `:143-149` | Add a dated supersession paragraph naming this file; keep the history, kill the "design lock" claim. |
| `WORKSPACE-PHASE.md:160-177` | Re-cut the acceptance list to D1-D7; keep the no-clipping and header lines verbatim. |
| `README.md:18` | Rewrite the first-run paragraph: first screen is the search field, settings behind one disclosure. |
| `README.md:26` | Delete the five-step `Fix match` walkthrough; replace with the one-entry/one-confirm flow. |
| `README.md:43` | Replace "there is no Series/Movie toggle" with the derived-kind rule. |
| `RUST-PORT.md:393-398` | Mark the `Fix match` picker paragraph superseded; the wire/parity paragraphs around it stay. |
| `RUST-PORT.md:103-106` | Keep the integer-`scalePx` rule; change the design size to "463x703 floor, 1180x700 ceiling". |
| `AGENTS.md:3` | Drop "Netflix-styled"; keep the fork attribution and the licence pointer. |
| `UI-PHASE.md:33` | Append a one-line dated note that the unit of supersession is now this file, not just `WORKSPACE-PHASE.md`. |
| `CHANGELOG.md` `[Unreleased]` | Add the flow change in the same pass; the `1.1.0` section stays untouched as released history. |

Outside this lane's reach, named follow-ups:

- **Released history** — `CHANGELOG.md` `[1.1.0]` (the quoted pre-fill text at
  `WORKSPACE-PHASE.md:135-141`) is a shipped record; it is not edited, and the
  new pass does not claim it never happened.
- **The installed 1.1.0 binary** — still the 460x680 composition until a build
  of the new tree is installed; that install is a release-lane act, not this
  pass's.
- **Source removal of the old dialog** — deleting `Fix match` copy and the
  `FM_ID_*` dialog is a source change in `window.rs`, owned by the
  implementation lane, not by this doc.
- **Commit** — landing this file and the pass is a git act for `git-agent`;
  this lane writes the doc only.

---

## Provenance

Written 2026-10-29 on a fresh context from the working tree at `f3e6b52` plus
its uncommitted workspace pass. Read-only: no build, no launch, no install, no
commit, no source edit. The installed-build observations quoted from the owner
are his, not reproduced here as measurements.
