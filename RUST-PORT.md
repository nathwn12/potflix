# Potflix — Rust port notes

Deep technical material for the Rust implementation: crate layout, the parity
contract, verification, Windows-specific notes, dependencies, intent and
limitations, and the deliberate deviations from the original Java flow carried
over from the retired Go era.
The user-facing manual is [README.md](./README.md).

The behaviour in [README.md](./README.md) `## Features` is the app's
**intended contract**, carried over from the retired Go reference into the
Rust implementation. The Rust app builds and its headless self-test passes (see
**Verification**, below), but those gates prove wiring and the title cleaner
only — they do not prove feature-level behaviour. The features were implemented
for the Go reference and are carried in the Rust port unchanged, so
they are expected to behave as described, but they are not re-verified live
against PotPlayer or Discord on the Rust app, and no CI runs them.

## Layout

- `rust\` — the implementation: a Cargo workspace (`rust\Cargo.toml`) with one
  member crate per package of the retired Go module under `rust\crates\*`,
  plus `app`, which owns the workspace's `potflix` tray binary. Cargo
  workspace, edition 2024.
- `tools\check.ps1` — the single exit command (gate). `tools\rust-build.ps1` —
  the Rust build.

## Crates

One member crate per Go package; each line is the crate's own ownership.

| Crate | Owns |
|---|---|
| `titleclean` | Dependency-free port of the Java media-title cleaner (`me.potflix.rpc.Main.clean`); JDK-11 Unicode tables ported as data; also carries the `titleclean` parity CLI. |
| `store` | The crash-safe single-file append-only key/value store behind the library index and the metadata cache; hand-rolled CRC32, `CreateFileW` for the share flags. |
| `metadata-core` | The network-free half of metadata: record types, filename parsing, candidate matching, alternate titles, request context. |
| `metadata-http` | The transport half: the shared HTTP client / fetch helpers (ureq over `native-tls`) and the per-host rate limiter. |
| `metadata` | The provider chain, the four keyless providers (TVmaze, AniList, Cinemeta, Wikipedia) and the permanent on-disk cache; owns the `metacheck` CLI. |
| `library` | The shared walk + diff loop behind Scan / Refresh, the background metadata catch-up, and `metawarm`; the store-backed `library.idx`; owns the `metawarm` CLI. |
| `discord` | Local Discord IPC pipe transport (`discord-ipc-0..9`): handshake, `SET_ACTIVITY`, `READY` gating, reply matching. |
| `player` | PotPlayer window discovery (`EnumWindows`), the `WM_USER` time/status queries, and the `WM_COPYDATA` play-file path query. |
| `presence` | The presence state machine and the activity payload builder; owns `CardSpec`, the stored card shape. |
| `repair` | The needs-repair report and the sanitized GitHub issue path. |
| `bundle` | Metadata-only cache bundles: export, fill-missing import, count, clear. |
| `app-support` | The non-Win32 support surface split out of the Go root package: client-id resolution, the settings/config store, the debug log, and the single-instance lock. |
| `potflix-win` | The Win32 surface split out of the Go root package: the raw binding table, the tray window, and the settings window. |
| `app` | The tray application — the Rust `app` crate's `main` and `selftest` binaries — and the icon resource (`build.rs`); owns the workspace's `potflix` binary. |

## Verification

All of the following were run on 2026-10-13 on this machine and passed:

- `pwsh -ExecutionPolicy Bypass -File tools\check.ps1` → PASS (clippy `-D warnings`, `cargo test --workspace --release --locked`, titleclean parity).
- `pwsh -ExecutionPolicy Bypass -File tools\check.ps1 -Build -Selftest` → PASS; `dist\potflix.exe` produced, `--selftest` emitted `M1 SELFTEST PASS` (exit 0).
- Build artifact: `dist\potflix.exe`, 3,593,216 bytes, SHA-256 `C73257FD92A7E9E7D1AFA347C9C25CAE221E7F6B7B6FEBE57A58159A5616C2F6` (rebuild 2026-10-13).
- Hygiene: `gitleaks detect --no-git` reports 2 findings, both false positives in ignored build artifacts under `rust\target\`; tracked source is clean. The two content hits (a client id in `tools\epname-fix-report.md`, and a real first name in a test doc-example path in `rust\crates\store\src\sys\windows.rs`) were redacted before publication.
- Rust `target\` and `dist\` remain untracked (gitignored).

The self-test is headless: it proves the hidden window + tray add, log
rotation, second-instance lock, and the full IPC round-trip against an
in-process fake Discord pipe, with no live PotPlayer or Discord.

`tools\live-check.ps1` is the repeatable live proof that closing PotPlayer
clears the card: it launches the built app, plays a real file, waits for the
card push and its Discord ack, closes PotPlayer gently (a `WM_CLOSE` request,
never a force-kill), and requires the clear to be ACKNOWLEDGED with no
`push ack timeout` in between — an unacked clear is the stuck-card bug.
Usage: `powershell -ExecutionPolicy Bypass -File tools\live-check.ps1`
(`-File` overrides the media file, `-CardTimeoutSeconds` /
`-ClearTimeoutSeconds` bound the waits, `-KeepLog` keeps this run's log
lines). Exit codes: 0 = pass, 1 = check failure, 2 =
precondition/environment (Discord reachable, no PotPlayer or Potflix instance
already running, built exe present), under which nothing was changed.

## The parity contract

This implementation's core claim: the Rust title cleaner reproduces the retired Go
implementation's output **byte-for-byte**. On each corpus the Rust binary is
run from the repo root and its TSV output must hash identically to the
committed contract, [`rust\parity-golden.sha256`](./rust/parity-golden.sha256)
— a 64-hex SHA-256 per corpus, nothing else.

The oracle is now FROZEN: its Go reproducer is gone, so the hashes can never
be regenerated — only matched. No 1.1.0 change touches
`rust\crates\titleclean\` — the cleaner is byte-parity frozen — so the
frozen hashes still match without re-proving.

The corpora themselves (`tools\titles-sample4.txt`, `tools\titles-sample.txt`)
are deliberately **not committed**: they carry real media filenames from a real
library, which must not be published. The oracle therefore holds hashes only,
so a missing corpus fails the gate rather than being treated as "nothing to
compare" (`tools\check.ps1` § `Invoke-TitlecleanParity`). The oracle header's
regeneration commands named the retired Go reproducer; they are history now —
the oracle holds hashes only, and a hash mismatch fails the gate.

## Windows-specific notes

These are load-bearing for the port and easy to break:

- The `PAINTSTRUCT` mirror is **72 bytes on amd64**. 68 is the sum of the field
  widths, not the ABI size; Go rounds to the 8-byte alignment and reports 72,
  exactly like the Windows header. A 68-byte buffer is a stack overwrite by
  `BeginPaint`, so 72 is what the port asserts.
- All UI geometry is integer `scalePx = (v*dpi + 48) / 96` from a 460x680
  logical design, and font heights are negative. An `f32`/`f64` scale would
  change hit-tests versus paint, so the port keeps `i32` everywhere.
- One OS thread owns the tray and main window procedures and the `GetMessageW`
  message loop; workers (poll, scan, report) never touch the UI — their payloads
  ride mutex-guarded snapshots and repaint only via `InvalidateRect` plus an
  async `PostMessageW`.

## Dependencies

Every direct dependency is pinned to an exact version and `rust\Cargo.lock` is
committed (never gitignored); `--locked` is the build norm. Deliberate
exclusions, recorded in `rust\Cargo.toml`:

- **No third-party Unicode / word-segmentation crate.** `titleclean` must
  reproduce the JDK-11 tables exactly, so it carries that data ported verbatim
  from the retired Go tree (`go\internal\titleclean`); a third-party Unicode
  table may not enter the graph. The regex matcher is part of the port for the
  same reason.
- `ureq` with `native-tls` (SChannel on Windows), so HTTPS needs no C toolchain
  and no OpenSSL; rustls/ring is deliberately not in the graph.
- CRC32 for the store is hand-rolled (table-driven IEEE), not a crate.

## Intent and limitations

Potflix is a local Windows tray app: whatever is currently playing in
PotPlayer shows as a Discord Rich Presence card. That is the whole job, and
the stance is deliberate:

- **Keyless.** No API keys, no accounts, no tokens, no telemetry, no server
  component — nothing that can leak and nothing to rotate.
- **Personal scale.** One title at a time, cached under
  `%LOCALAPPDATA%\Potflix\meta`, tolerant of being offline (a title with no
  cached answer shows its clean filename).

The limits below follow from that stance. Each carries its reason inline: a
limit with no reason reads like an unfixed bug; with its reason it reads as
the choice it is.

| Limitation | Why |
|---|---|
| Metadata comes from free public sources; some titles are incomplete or missing | Keyless is the constraint. Keyed providers (TMDB, TheTVDB, Trakt) would answer better, but each needs a credential to ship — and a hardcoded credential in a public repo is a leak. |
| Requests are spaced conservatively per host, so a first lookup can be slower than a keyed client | Each service publishes a rate limit; the app stays under it and accepts the latency rather than getting throttled or timed out. Correctness over tightness. Intervals below. |
| A genuine miss is bounded in time rather than walking every source at full timeout | An unknown file — a personal video — should settle quickly instead of hanging the card. |
| Metadata is cached for 30 days — misses too (`rust\crates\metadata\src\cache.rs:47,53`) | It is a cache, not a store: a stale answer is re-fetched rather than kept forever. |
| Anime per-episode season is not available | No keyless source carries it: AniList episodes are numbered continuously and the port fixes their season to 0 (`rust\crates\metadata\src\anilist.rs:289-290`). |
| The card shows series artwork, never an episode still | One card, one recognisable poster — stills never override (`rust\crates\metadata\src\resolver.rs:817-819`). |
| A file with no episode or season tag is treated as a movie | Title-only classification is all a filename gives; an anime movie or a TV special therefore takes the movie chain, which still resolves it. |
| Artwork falls back to the app logo when no source has a poster | A deliberate mark beats a broken image. |
| Bangumi answers only what a romaji query can title-validate: entries whose validated pick is the native-script `name` still match, but a native-only name no romaji query resembles is a miss, not a wrong card (`rust\crates\metadata\src\bangumi.rs:116-122,174-177`) | A wrong title is worse than no title — the pick returns a definitive miss rather than a wrong show. |
| Year is a preference, never a veto: a query whose year matches nothing still returns the best title match (`rust\crates\metadata\src\bangumi.rs:188-191`, `rust\crates\metadata\src\yearpref.rs`) | A year mismatch is a filename quirk, not a wrong title — rejecting on it would drop a complete answer for a nice-to-have. Narrow exception: the brick fill vetoes a known year mismatch only from a floor-less provider (movie/no-floor match — today Wikipedia alone, `rust\crates\metadata\src\brick.rs:has_match_floor`), because a cached Found brick never expires; floored providers are never year-vetoed, so a premiere-year answer for a later season still files. |
| Some providers are unofficial or undocumented (JustWatch's GraphQL, Episodate, LiveChart.me, ANN) and may change without notice (`rust\crates\metadata-http\src\ratelimit.rs:52-71`) | Keyless is the constraint: no keys, no registration — so the chain leans on whatever free endpoint answers, and an endpoint that moves breaks its leg until the code follows. |

The source chain, per media kind (`rust\crates\metadata\src\resolver.rs:287-302`):

- **Movie (3 legs):** Cinemeta (general catalog), then Wikipedia (encyclopedic fallback), then JustWatch (title search, `MOVIE` filter only — `rust\crates\metadata\src\justwatch.rs:151-154`).
- **Series (9 legs):** TVmaze (TV), AniList (anime), Tenrai (anime), Kitsu (anime), Cinemeta (general fallback), Episodate (TV), Bangumi (anime/live action), LiveChart.me (anime), ANN (anime).

Episodate, Bangumi, LiveChart.me and ANN answer series only and self-gate movies before touching the wire (`episodate.rs:202`, `bangumi.rs:205`, `livechart.rs:187`, `ann.rs:340`); JustWatch answers both kinds but only the movie leg is walked for movies.

Every one is keyless and none requires an account.

What wins when sources disagree — completeness first, year last:

| Preference | Why |
|---|---|
| A complete answer beats a title-only one: the card renders cover, title, episode, synopsis (and length) (`rust\crates\presence\src\lib.rs:558-572`), and a movie answer only settles the lookup once it carries a synopsis (`rust\crates\metadata\src\resolver.rs:773-787`) | A Breaking Bad with a legit title + episode + length + synopsis is the whole card; a bare title leaves its lines empty. |
| Year only breaks ties, never rejects: a known year disambiguates duplicate exact titles and otherwise rides along as display (`rust\crates\metadata\src\cinemeta.rs:15-19`); year never gates the chain (`rust\crates\metadata\src\resolver.rs:20-26`) | A year mismatch is a filename quirk, not a wrong title — rejecting on it would drop a complete answer for a nice-to-have. |
| Completeness beats order: an episode match with no synopsis is kept as the fallback while the walk continues, so a later source carrying the episode *and* the synopsis wins (`rust\crates\metadata\src\resolver.rs:744-753,789-805`) | First answer is not best answer; the card is only as good as the synopsis line it prints. |

Per-host request spacing at the default Turbo profile
(`rust\crates\metadata-http\src\ratelimit.rs:30-51`; Balanced doubles and
Gentle quadruples these, `ratelimit.rs:110-124`):

| Host | Spacing |
|---|---|
| `api.tvmaze.com` | 500 ms (`ratelimit.rs:31`) |
| `graphql.anilist.co` | 2000 ms (`ratelimit.rs:32`) — the documented degraded 30/min figure, held until the docs say otherwise; if this number looks mid-flight, the posture at `ratelimit.rs:23-29` is the authority, not the figure |
| `api.tenrai.org` | 500 ms (`ratelimit.rs:46`) |
| `kitsu.io` | 1000 ms (`ratelimit.rs:50`) |
| `v3-cinemeta.strem.io` | 1000 ms (`ratelimit.rs:44`) |
| `en.wikipedia.org` | 1000 ms (`ratelimit.rs:41`) |
| `apis.justwatch.com` | 2000 ms (`ratelimit.rs:55`) |
| `www.episodate.com` | 2000 ms (`ratelimit.rs:59`) |
| `api.bgm.tv` | 2000 ms (`ratelimit.rs:63`) |
| `www.livechart.me` | 2000 ms (`ratelimit.rs:67`) |
| `cdn.animenewsnetwork.com` | 2000 ms (`ratelimit.rs:71`) |
| `api.jikan.moe` | 350 ms (`ratelimit.rs:34`) |
| `itunes.apple.com` | 3000 ms (`ratelimit.rs:35`) |

The five new hosts sit at one request per 2 s deliberately: these are small
or unofficial endpoints that publish no limit, so the app reads
conservatively (`ratelimit.rs:52-71` — "conservative chosen reading, not a
measured or published figure") and accepts the latency. A slow-but-working
card beats a fast one that gets throttled: spacing is the price of staying
unblocked on someone else's free service. The `jikan` and `itunes` rows are
port fidelity to `ratelimit.go:17-30`, retained deliberately even though both
providers are dead (Jikan discontinued 2026-10-01, iTunes leg replaced by
Cinemeta) — an unknown host is ungated (`ratelimit.rs:14-16`), so dead rows
cost nothing and the table stays a faithful port.

For each source's licence, see [THIRD-PARTY-NOTICES.md](./THIRD-PARTY-NOTICES.md) —
one authority, one place; the terms are not restated here.

## Deviations from the original Java flow

These are the retired Go reference's deliberate deviations from the original
Java flow; the Rust implementation targets the Go behaviour, so they are the
behaviour it carries. Mostly proven live for the Go reference (whose tree is
retired; its history remains in git):

1. Empty asset text is omitted, not sent empty. `go\internal\discord\activity.go:86-109`
   tags every text field `omitempty` (rationale at `activity.go:42-46`): Java
   set `""` for `large_text` / `small_text` (`Main.java:146,150,183`) and the
   GameSDK dropped it on the wire, so Discord never sees `"large_text":""`.
2. Exit clears the card. `go\main.go:196-199` nulls the activity before
   quitting; Java exited straight (`Main.java:544`) and left the last card on
   the profile.
3. Capped rotating debug log. `go\debuglog.go:20-23,74-97` caps
   `potflix-debug.log` at 5 MB x 3 files beside the exe; Java appended to one
   unbounded file (`Main.java:37-41`).
4. Commands wait for `READY`. `go\internal\discord\pipe.go:70-73,341-343`
   refuses a send with `ErrNotReady` until Discord's `READY` dispatch arrives
   (Discord discards anything sent earlier); Java's GameSDK hid that handshake.
5. `timestamps.end` rides while playing. `go\internal\presence\gate.go:151-160`
   projects the end from the live position and
   `go\internal\discord\activity.go:143-153` serializes start + end, so the
   Discord time bar tracks the real position; Java sent `timestamps.start`
   only (`Main.java:176`).
6. A `?`-family mark between title text folds to `:`. `go\internal\titleclean\clean.go:373,381,385-419`
   rewrites a mark (ASCII `?`, fullwidth `？`, small `﹖`) to `:` when a
   non-space character precedes it and it is not the last character of the
   extracted title, so `Spider-Man? Brand New Day (2026) ...` cleans to
   `Spider-Man: Brand New Day`; a mark that ends the title is a real question
   mark and is kept. The parity CLI therefore diverges from Java on titles
   carrying such a mark, by design.
7. The client-id lookup also searches beside the exe. `go\clientid.go:57-66,80-100`
   looks in the executable's directory before the process working directory;
   Java opened `client-id.txt` / `config.properties` against the CWD only
   (`Main.java:524-526`).
8. The settings window opens on a manual launch. `go\main.go:202-211` creates it
   and calls `showMainWindow` unless the launch carries `--minimized` (the
   tray-only autostart path); Java was headless with a tray only
   (`Main.java:540-553`) and `setUi` is a no-op sink (`Main.java:556`).
9. The activity type is `Watching`, not `Playing`. `go\internal\discord\activity.go:80`
   sets `activityTypeWatching = 3` (rationale at `activity.go:76-79`: Discord's
   time bar requires Listening or Watching); Java set `ActivityType.PLAYING`
   (`Main.java:144,159`).
10. A headless OP/ED with no name renders the tag alone. `go\internal\titleclean\clean.go:196-206`
    sets both the title and the display to the episode tag, so `ED 03.mkv`
    renders `ED 3`; Java fell through to `buildDisplay` (`Main.java:372-377`,
    `:479-483`) and duplicated it as `ED 3 — ED 3`.

## Departures from the retired Go reference (Rust era)

The Go module above is retired (tree removed; history in git); the Rust
implementation under `rust\` deliberately goes
further where the owner's live card proved the Go behaviour wrong:

11. Wikipedia disambiguation suffixes are stripped. `rust\crates\metadata\src\wikipedia.rs`
    (`strip_disambiguation`) drops a trailing `(film)` / `(2021 film)` /
    `(TV series)`-style parenthetical from the provider title, so the card
    shows `Ready Player One` with details `Ready Player One (2018)` instead
    of `Ready Player One (film)` / `Ready Player One (film) (2018)`. Cached
    records heal on decode (`rust\crates\metadata\src\cache.rs`,
    `decode_cache_entry`, no schema bump). Go stored the raw page title
    (`go\internal\metadata\wikipedia.go:504`).
12. Truncated card lines are always signalled, never silent. `rust\crates\presence\src\lib.rs`
    (`trim_word`, `smart_trim_synopsis`, the `large_text` build): a cut landing on a sentence end returns the complete sentence with its own `.`/`!`/`?` and no ellipsis, and every other cut ends with a single `…` — a whole-word end wherever the text has word boundaries, always a terminator otherwise, so a note with no spaces at all (a long token, CJK text) is cut on a character boundary and still ends with that `…`.
    The tooltip now always carries its terminator, and a
    spaceless note is cut on a character boundary with that terminator attached. Guarantee: never silently truncated. Go kept the
    cut punctuation and hard-cut the tooltip
    (`go\internal\presence\presence.go:573-644,673-675`).
13. Wikipedia artwork accepts the article's own file. `rust\crates\metadata\src\wikipedia.rs`
    (`wiki_pick_poster`) falls back to a file named exactly like the page
    (`File:Ready Player One (film).png`) when no PageImages image and no
    `*poster*` file exist; Go only accepted a literal `poster` word and left
    the card on `logo`.
14. Movies come from Cinemeta, not iTunes; Jikan is gone.
    `rust\crates\metadata\src\cinemeta.rs` (Stremio's keyless catalog addon)
    is first for movies and third for series, replacing the iTunes leg —
    which returned the first `feature-movie` row with no title or year check
    (`Inception` answered as `Ring of Fire`) — and the Jikan leg (dead since
    2026-10-01). A Cinemeta pick must match the query title (house-ranked)
    and any known query year, or it is a miss rather than a wrong card. The
    retired Go kept the old TVmaze/AniList/Jikan/iTunes chain.
15. The tray icon survives an explorer restart (additive, not a departure).
    `rust\crates\potflix-win\src\tray.rs` registers `TaskbarCreated` with
    `RegisterWindowMessageW` and its wndproc re-adds the icon through the
    existing add path when that broadcast arrives. Neither Java nor Go
    handled it: after a shell crash, update, or logon cycle the app kept
    running with no icon and no way back in (the tray menu is the only UI
    route and the only Exit path).
16. A failed tray add keeps the app alive with the window visible (a real
    departure). `rust\crates\app\src\main.rs` (`run_ui`) logs
    `notify-icon add result=false` and continues — Discord transport, poll
    loop, and settings window all start — and leaves the main window shown
    instead of hidden, so the app is reachable and quittable (X). Java
    (`Main.java:550-551`) and the retired Go exited the process; silently
    dying serves no card, and the card does not depend on the tray.
17. The scan completion log names the instant, not the verdict. `rust\crates\potflix-win\src\window.rs`
    (`on_scan_done`) logs `unresolved-at-scan-time=` where the retired Go logged
    `not-found=` (`go\window.go:2749`): the per-file counter is honest but sampled
    before the async catch-up pass, so a nonzero value can coexist with a later
    successful fetch — Go's name reads as final at a moment that is not final.
18. The card is per kind and never repeats itself. `rust\crates\presence\src\lib.rs`
    (`compose_card`, `compose_details_for`, `compose_state_for`,
    `synopsis_state`, `compose_large_text_inner`) builds each body line as a
    fill ladder whose last rung is identity from the file itself, except the
    deliberate empties below; `rust\crates\presence\src\cardspec.rs`
    (`CardSpec`, `render_kind`, `card_presets`, `CARD_STATE_NONE`) keeps the
    presets as re-heads that never remove the fill, except the deliberate
    one-line paths: the `Minimal` state preset (stored `none`), which empties
    the state line, an empty default series details line (no episode
    title and no filename episode name), and any state rung whose final text
    would repeat the header (the clean-title fallback equal to the header
    blanks, `Paused` when paused) -- Minimal is not the only one-line
    path. SERIES covers TV + anime, MOVIE is movies. Shipped
    default: SERIES line 1 the episode title (else the filename's own episode
    name, else empty -- never the show title, which is the activity header,
    and never the episode code, which lives on line 2), line 2 the `SxxExx`
    code plus the synopsis (then the filename tag, then the clean title unless
    it repeats the header -- then empty for a deliberate one-line card,
    `Paused` when paused); MOVIE line 1 the synopsis (then the year,
    then genres/length, then empty -- never the title, which is the
    header/pill), line 2 the year/genres/runtime join with missing parts
    skipped (then genres/length beside a year, then empty beside
    genres/length -- the state never repeats what line 1 already shows).
    A season-0/absent season never fabricates `S00`: an absolute episode
    renders `E02` from its absolute number, and a season-0 shape whose numbers
    disagree renders the absolute one. The movie tooltip is omitted, so
    the synopsis is not repeated; the series hover text of the LARGE image
    slot — the cover when artwork resolved, the PotFlix `logo` asset
    otherwise — carries the whole stored note. Slot budgets in UTF-16 units
    held in one place (`NAME_BUDGET`,
    `DETAILS_BUDGET`, `STATE_BUDGET`, `STATE_SYNOPSIS_BUDGET`,
    `LARGE_TEXT_BUDGET`): header 128, line 1 128, line 2 128 plain / 72 with
    a synopsis, hover 128; body lines join with `JOINER` (U+00B7 with single
    spaces); cuts (`trim_word`, `smart_trim_synopsis`) are always signalled: a cut landing on a sentence end returns the complete sentence with its own `.`/`!`/`?` and no ellipsis, and every other cut ends with a single `…` —
    never silently truncated; a whole-word end wherever the text has word boundaries; always a terminator otherwise (a spaceless note — long token, CJK text — cut on a character boundary, still ending with that `…`). Every provider mapper caps the note at fetch time
    (`cap_synopsis`, `MAX_SYNOPSIS_UNITS` = 128 UTF-16 code units), so the
    stored note always fits the 128-unit hover whole. Per-kind choices persist in
    `rust\crates\app-support\src\config.rs` (`CardSpecs`, `CardKind`,
    `CardLine`, `for_kind`) as
    `"card":{"series":{"details":"…","state":"…"},"movie":{…}}`; the legacy
    flat `{"details":"…","state":"…"}` still loads as the series pair with the
    movie pair default and re-saves nested — a backward-compatible read, not a
    byte-identical re-write; an all-default config still writes `"card":{}`.
    Retired Details ids (`title`/`series`, which repeat the header, and
    `episode`, byte-identical to the default) migrate to `default` on load and
    on save. The non-repetition rules hold by construction: no body line
    repeats the header title, and the movie state never repeats what the movie
    details line already shows; the full note stays on hover (series) or on
    line 1 (movies, whose tooltip is omitted).
19. The Card-view preview is what Discord gets, row for row. The preview renders through the
    same composer the send path uses (`compose_card` via
    `CardSpec::render_kind`), so picking a line changes the preview
    immediately; it is exact for the rows it shows (Header / Details / State
    / Images / Buttons / Tooltip). The progress-bar end timestamp is
    projected by the send gate from the player clock, not by the composer,
    so the preview shows no timestamps. The preview follows the live card's
    kind on its own — there is no Series/Movie toggle and no `Card lines for`
    row: there is no Details row (line 1 is fixed by the shipped mapping; the
    stored `Default` value stays for compatibility) and the State row offers
    its four, and a preset save writes the DISPLAYED kind. A preset save publishes the new specs to the running sender, in-process, through the
    store's live mirror (`app_support::config::set_live_card_specs()` / `live_card_specs()`, backed by a process-wide lock) — the same live-apply shape as the
    scan-speed path (`metadata_http::set_rate_profile`); the poll loop (`push_activity` / `poll_loop` in `rust\crates\app\src\main.rs`, preset choice via
    `rust\crates\potflix-win\src\window.rs` `on_select_card_preset`) reads the published specs on every push, so the change reaches the live Discord card
    at the next poll tick: no restart, and no push from the click itself. The send gate compares name/details/state/paused/images/buttons/has_start against the last-sent card and sends when they differ (`rust\crates\presence\src\gate.rs:209-221`), so the changed lines go out by themselves — while paused too, where the card is re-parked when the compared content is unchanged — the header, the two body lines, the large image, the button list and the paused flag — and a change to any of those still goes out with a fresh pause anchor; a change confined to the hover text does not re-park on its own. That set mirrors the ported gate's paused comparison (`rust\crates\presence\src\gate.rs:170-193`), and no card preset changes the hover text alone.
20. A wrong card is fixed by hand, not by re-scanning. While something plays,
    `Fix match…` (`rust\crates\potflix-win\src\window.rs`, `FIX_MATCH_*` copy
    through `fix_match_scope_choices`) pre-fills the name from the resolver's
    own parse, searches the provider chain on demand, and pins the pick as a
    latch in `%LOCALAPPDATA%\Potflix\overrides.json`
    (`rust\crates\metadata\src\overrides.rs`, keyed by movie path, season
    folder, or show folder; `rust\crates\metadata\src\search.rs`,
    `pin_from_candidate`, fetches the episode table once so every episode in
    a season/show scope keeps its own title and synopsis under the scope's
    shared cover). The latch wins for lookup (`rust\crates\library\src\catchup.rs`
    resolves latched records with no fetch) and for display
    (`rust\crates\app\src\main.rs`, `latch_resolved`), a latched record is
    never searched again, and an already-matched item offers
    `Refresh metadata`, `Change match…` and `Use automatic matching` from the
    same door.
21. The clear survives a stuck pipe. An ack timeout drops the connection so
    the supervisor reconnects (`rust\crates\discord\src\lib.rs`,
    `on_ack_timeout`, `ACK_TIMEOUT` 2 s; backoff `BACKOFF_MAX` 60 s,
    `READY_GRACE` 10 s), the poll loop re-asserts the clear on a new
    connection generation (`rust\crates\app\src\main.rs`, `clear_due`), a
    window stopped for `STOP_GRACE_MS` (15 s) reads as no-player, and the
    player reads are bounded by `SendMessageTimeoutW`
    (`rust\crates\player\src\control.rs`), so a hung player window cannot
    freeze the loop before it reaches the clear.
