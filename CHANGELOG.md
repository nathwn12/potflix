# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- One universal search: the scope row (`All / TV / Movies / Anime TV / Anime Movies`) is gone; every query runs both chains and merges, so no category reaches the user.
- Result rows carry poster thumbnails, and artwork is always on: the Artwork toggle is deleted, and an undecodable cached cover is dropped instead of poisoning the slot.
- The settled card: a movie reads `Year · Genre, Genre` over the runtime; a series leaves line 1 empty and reads `S05E14 · Episode` on line 2; a paused card carries no timestamps, so no count-up timer appears; episode prose never rides the tooltip; the owner's latch poster outranks provider art.
- Settings is exactly five rows: Discord connection, Close to tray, Run at startup, Theme, Clear metadata. The presence chip tells the truth: silent while the link is healthy, naming the reason only when it is not.

### Fixed

- Every settings write honours the injected config path, so a test run can no longer rewrite the owner's real `config.json`.
- A single-file movie correction inside a latched show no longer deletes the
  show-wide series latch: only the season-folder latch for the file's own
  season is cleared, so the show cover, episode pins, and every other episode
  keep resolving while the corrected file resolves movie
  (`rust\crates\metadata\src\overrides.rs`, `Store::put_correction`).
- `edgerunners 2026` can no longer show a 1990s entry: a trailing year is now
  an identity gate, while a future number such as `Blade Runner 2049` stays
  part of the title.

## [1.1.0] - 2026-10-19

### Added

- Manual match for a wrong card: while something is playing, `Fix match…`
  opens a picker (`rust\crates\potflix-win\src\window.rs`, the `FIX_MATCH_*`
  copy, `fix_match_prefill_name`, `fix_match_scope_choices`) — the name field
  is pre-filled with the resolver's own parse of the playing title (so it
  reads `Attack on Titan`, never the raw filename), plus an optional year;
  Search walks the provider chain and each candidate row reads
  `Attack on Titan (2013) · AniList · 25 episodes` (a movie omits the episode
  count). The scope radios choose the latch target — `This season` (default)
  or `Whole show` for a series file (one radio when the folder is both at
  once), while a movie pins its own file directly with no radios. The latch is
  stored locally in `%LOCALAPPDATA%\Potflix\overrides.json`
  (`rust\crates\metadata\src\overrides.rs`, `STORE_FILE_NAME`), keyed by path
  (the movie file, the season folder, the show folder), and wins over the
  automatic ladder for both lookup and display: the catch-up pass resolves a
  latched record from the store itself with no provider fetch
  (`rust\crates\library\src\catchup.rs`), and the poll loop renders the card
  from the latch (`rust\crates\app\src\main.rs`, `latch_resolved`). Pinning a
  season or a whole show fetches the episode table once
  (`rust\crates\metadata\src\search.rs`, `pin_from_candidate`), so every
  episode in scope renders its own title and synopsis under the scope's shared
  cover, and a latched record is never searched again. On an already-matched
  item the same door offers `Refresh metadata`, `Change match…` and
  `Use automatic matching`.
- Folder-list keyboard navigation: Up/Down step the selection, Home/End jump
  to the first/last row, Enter opens the selected folder
  (`rust\crates\potflix-win\src\window.rs`, `on_main_list_key`).
- A repeatable end-to-end clear check: `tools\live-check.ps1` plays a real file
  through the real player, waits for the card push and its Discord ack, closes
  PotPlayer gently and requires the clear to be ACKNOWLEDGED with no
  `push ack timeout` in between. Exit codes: 0 = pass, 1 = check failure,
  2 = precondition/environment (nothing was changed).

### Changed

- The card State line no longer carries the synopsis by default: the series
  default is the episode tag alone (then the clean title, the synopsis only as
  the last resort before a blank line), the movie default is
  `genres · length` (then year, clean title, synopsis last)
  (`rust\crates\presence\src\lib.rs`, `compose_state_for`). The synopsis stays
  available two ways: the explicit `Synopsis` preset (the 72-unit
  `STATE_SYNOPSIS_BUDGET`) and the whole-note hover text of the cover slot
  (`large_text`). The shipped per-kind defaults below are updated to match.
- The needs-repair report now carries the running app version (`APP_VERSION`,
  `rust\crates\potflix-win\src\settings.rs`); extras and featurettes are
  excluded from the repair list and counted separately, and a latched record
  is counted once as `pinned` (`rust\crates\library\src\catchup.rs`,
  `rust\crates\repair\src\report.rs`).
- `titleclean` is untouched: no file under `rust\crates\titleclean\` changed,
  so the byte-parity contract still matches without re-proving.

### Fixed

- The stuck card after the player closes: an ack timeout now drops the
  connection so the supervisor reconnects (`rust\crates\discord\src\lib.rs`,
  `on_ack_timeout`, `ACK_TIMEOUT`), the poll loop re-asserts the clear on a
  new connection generation (`rust\crates\app\src\main.rs`, `clear_due`), a
  window stopped for `STOP_GRACE_MS` (15 s) is treated as no-player, and the
  player reads are bounded by `SendMessageTimeoutW` so a hung player window
  cannot freeze the loop (`rust\crates\player\src\control.rs`). Reconnect
  backoff (`BACKOFF_MAX` 60 s, `READY_GRACE` 10 s) keeps a dead-but-accepting
  Discord pipe from causing a reconnect storm.
- Filing the repair report as a GitHub issue no longer dies when the label is
  missing: the create is retried once without the label flags, the error keeps
  the first attempt's cause text, and the prefilled new-issue URL is still
  shown (`rust\crates\repair\src\issue.rs`).
- Episode-only filenames resolve using the folder's series context: a bare
  number like `01.mkv` under a usable show folder becomes a series query
  carrying the folder's show and season
  (`rust\crates\metadata-core\src\context.rs`, `resolve_query`).
- Three resolver misses now resolve: `Now You See Me 2 (2016)` classifies as
  the MOVIE `Now You See Me 2` (the parenthesised sequel year is salvaged,
  never an episode), `The.Blair.Witch.Project.1999…-LEGi0N.mkv` queries with
  the raw's plausible year and answers on the year-confirmed catalog row, and
  `Shingeki no Kyojin` S02E01/S03E05 resolve — the S02 case returns Attack on
  Titan S2E1 `Beast Titan`. Measured live 2026-10-19. The metadata cache
  schema bumped 8 → 9 so stale negative entries cannot suppress the fixes;
  older records are a whole-record miss and re-resolve on their own, as before.

### Rust port (rewrite)

#### Added

- The implementation is now a Rust Cargo workspace under `rust\` (edition 2024,
  one member crate per Go package, plus `app` which owns the workspace's
  `potflix` tray binary): `titleclean`, `store`, `metadata-core`,
  `metadata-http`, `metadata`, `library`, `discord`, `player`, `presence`,
  `repair`, `bundle`, `app-support`, `potflix-win`, `app`. The diagnostic CLIs
  `titleclean`, `metacheck` and `metawarm` are carried by their owning crates.
- Title-cleaner **parity contract**: the Rust cleaner is proven byte-identical
  to the frozen Go reference's TSV output on two corpora. The committed contract
  is `rust\parity-golden.sha256` — a SHA-256 per corpus; the corpora are
  deliberately not committed because they carry real media filenames.
- Pinned, minimal dependency set with `rust\Cargo.lock` committed and `--locked`
  as the build norm: `serde`, `serde_json`, `ureq` (native-tls, SChannel),
  `windows-sys`. No third-party Unicode/word-segmentation crate — `titleclean`
  carries the JDK-11 tables ported verbatim.
- Rust build and gate: `tools\rust-build.ps1` builds `dist\potflix.exe`; the
  single exit command `tools\check.ps1` runs clippy, the workspace tests and the
  parity step, with `-Build` / `-Selftest` adding the app build and the headless
  self-test.
- The Rust tray app's wiring is complete and its headless `--selftest`
  passes: the full gate (`tools\check.ps1 -Build -Selftest`) succeeds,
  proving the hidden window + tray add, log rotation, second-instance
  lock, and the full IPC round-trip against an in-process fake Discord
  pipe with no live PotPlayer or Discord.

#### Changed

- The Go module in `go\` and `src\main\java\me\potflix\rpc\Main.java` are now
  retired: both reference trees are removed and the Rust workspace under
  `rust\` is the sole implementation. Their history remains in git, and the
  title-cleaner parity oracle (`rust\parity-golden.sha256`) is FROZEN — its
  Go reproducer is gone, so the hashes can never be regenerated, only matched.
- Presence card is always filled and per-kind: each body line is a fill ladder
  ending on file identity, so a card is never blank; SERIES (TV + anime) and
  MOVIE store their own Details/State choices as
  `"card":{"series":{"details":"…","state":"…"},"movie":{…}}` (a legacy flat
  pair still loads as the series pair and re-saves nested, and an
  all-default config still writes `"card":{}`). Shipped SERIES
  default is the episode name plus the episode tag, MOVIE default is
  `Title (Year)` plus `genres · length` (then year, clean title, with the
  synopsis only as the last resort before a blank line);
  the hover text of the LARGE image slot — the cover when artwork resolved,
  the PotFlix `logo` asset otherwise — carries the whole stored note.
  Budgets in UTF-16 units (`NAME_BUDGET`, `DETAILS_BUDGET`, `STATE_BUDGET` /
  `STATE_SYNOPSIS_BUDGET`, `LARGE_TEXT_BUDGET`): 128 / 128 / 128 plain (72
  with a synopsis) / 128, body lines joined with `JOINER` (U+00B7 with
  single spaces). Truncation is always signalled, never silent: a cut landing on a sentence end returns the complete sentence with its own `.`/`!`/`?` and no ellipsis, and every other cut ends with a single `…`. Guarantee: never silently truncated; a whole-word end wherever the text has word boundaries; always a terminator otherwise — a note with no spaces at all (a long token, CJK text) is cut on a character boundary and still ends with that `…`. Every provider mapper caps the note
  at fetch time (`cap_synopsis`, `MAX_SYNOPSIS_UNITS` = 128 UTF-16 code
  units), so the stored note always fits the 128-unit hover whole; `Minimal`
  state (`none`) is the only one-line card. The Card view's `Card lines for`
  row (a Series/Movie toggle, `BTN_CARD_KIND_AT`) picks which kind the
  Details and State preset rows edit — writes go to the SELECTED kind only.
  The Card-view preview renders through the send-path composer, exact for
  the rows it shows (Header / Details / State / Images / Buttons / Tooltip);
  the progress-bar end timestamp is projected by the send gate from the
  player clock, not by the composer, so the preview shows no timestamps.
  A Card-view preset save publishes the new specs to the running sender, in-process, through the store's live mirror (the same live-apply shape as the scan-speed path); the poll loop reads the published specs on every push, so the change reaches the live Discord card at the next poll tick: no restart, and no push from the click itself.
  The send gate compares name/details/state/paused/images/buttons/has_start against the last-sent card and sends when they differ, so the changed lines go out by themselves — while paused too, where the card is re-parked when the compared content is unchanged — the header, the two body lines, the large image, the button list and the paused flag — and a change to any of those still goes out with a fresh pause anchor; a change confined to the hover text does not re-park on its own.

#### Removed

- The frozen Go reference (`go\`, 124 files, including the generated
  `go\internal\titleclean\jdk11tables.go` — the Rust port carries its own
  tables), the frozen Java reference (`src\main\java\me\potflix\rpc\Main.java`),
  the Go build path (`tools\build.ps1`), and the Java-era one-off tools
  (`tools\BatchClean.java`, `tools\EpisodeNameDump.java`,
  `tools\EpisodeVerifier2.java`, `tools\TestClean.java`,
  `tools\TestPresence.java`, `tools\epname-fix-report.md`). `tools\build.ps1`
  is no longer retained for reproducing the reference — the reference is gone.
  `src\main\resources\icon.png` stays: it is regenerated by the live
  `tools\make-icons.ps1`.

### Added

- Stdlib-only Go tray app (module `potflix`): single-instance lock, hidden
  tray window, capped rotating debug log, and headless `--selftest` wiring
  proof; the Java launcher flow is retired to a frozen reference.
- Media-title cleaner ported from the Java reference and proven byte-identical
  on the full corpus and holdout, including the P1-P12 episode naming patterns.
- Keyless canonical series/episode titles from free sources needing no
  account (TVmaze for TV, AniList + Jikan for anime, iTunes for movies),
  cached permanently on disk with 30-day negative entries; offline-safe
  fallback to the clean filename title.
- Library window: media folders with incremental `Scan` (per-directory skip
  index), full `Refresh`, and metadata-only JSON export/import bundles.
- Scan-speed profiles (`Turbo` / `Balanced` / `Gentle`) applied live with no
  restart, plus a Start-with-Windows toggle and card line presets.
- Netflix-style presence card: clean title header, episode name and tag,
  ` - Paused` count-up card that re-parks unchanged content while paused — the header, the two body lines, the large image, the button list and the paused flag (a change to any of those still goes out with a fresh pause anchor; a hover-text-only change does not re-park on its own), `Browsing` / `Idle`
  when idle, `READY`-gated sends, and clear-activity-on-exit.
- Diagnostic CLIs: `metacheck` (resolve one title), `metawarm` (pre-warm the
  cache over a library walk), `titleclean` (corpus parity TSV).
- Inno Setup installer (`tools/potflix.iss`) and embedded exe/tray icon
  resources.
- `internal/store`: crash-safe, append-only single-file key/value store behind
  both the file index and the metadata cache - a 24-byte own-CRC header,
  `tag | len | crc32 | payload` frames, a torn tail truncated on replay instead
  of erroring, temp + `Sync` + rename compaction, `Commit` as the fsync
  barrier, and (Windows) a `syscall.CreateFile` open with
  `FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE` so a live handle never
  blocks removal, with an explicit `ErrRemoved` contract for the durability
  claims.
- `internal/repair`: the needs-repair inventory the user can file -
  `needs-repair.json` + `needs-repair.txt` written atomically into
  `<index dir>\inventory`, plus a GitHub issue path (`gh issue create` when the
  GitHub CLI is available, otherwise a prefilled new-issue URL) whose published
  body carries only the reason, the file's basename, the attempt count and a
  sanitized error - never a local path
  (`TestIssueBodyNeverCarriesALocalPath`); the local report keeps the full path.
- Library window: a `Report` control that reads the local needs-repair report
  and files it; the live `indexed N of M files | metadata K of J` line with its
  `- catching up` state and a bar driven by metadata progress, so it cannot read
  full while metadata remains.
- Metadata catch-up: `StartCatchUp` resolves whatever the index still owes on a
  background worker (3 lookups at a time), started once at launch from
  `main.go:189`, idempotent and resumable because the pending set is the index's
  own state; entries unresolved after 3 attempts are parked `needs_repair` and
  retried only after a 6-hour cooldown.

### Changed

- Library index moved off the JSON file: `library.idx` (one `internal/store`
  file beside the configured index path) holds a record per video file - path,
  name, type, size, mtime, parsed title/kind/season/episode, metadata key,
  state, attempts, last error, next try - plus a rollup per folder. A scan is
  now enumerate + a `(size, mtime)` diff: unchanged files keep their record and
  are never re-parsed, vanished files are pruned, and an indexed `Scan`
  performs ZERO network fetches (metadata is the background catch-up above).
  First open on a machine with the old index imports it and RENAMES it to
  `scanindex.json.bak` - never deletes it - and the imported rollups carry no
  per-file data, so their folders re-walk once. Measured on this box (see
  `go/internal/library/BENCH.md`): the record codec is ~218 B binary against
  ~429 B of JSON for a 92-character path, the migrated real index (928
  folder rollups) loads in 9.04 ms, a 23,402-record store (7,556,531 B)
  reopens and decodes in 28.6 ms, and the store micro-bench writes 23,402
  records in 38.43 ms total and loads 24,520 records in 7.386 ms; a warm
  re-walk of the reduced real-shape corpus (2,300 files, all folders
  re-walked) is 141.9 / 143.6 / 149.7 ms with `Parsed==0` and zero fetches,
  and the folder-skip steady state is 6.4 / 7.6 / 7.7 ms.
- Metadata cache moved off one JSON file per key: 1,118 loose
  `meta\*.json` files become one `meta.idx` store behind one handle held for
  the process, so a read is a map lookup rather than a whole-file replay and
  the old 64 KiB per-entry read cap is gone (four entries on this machine were
  64 KB-232 KB and were therefore permanently treated as "not cached"). When
  the store is empty the legacy directory is imported once and KEPT as the
  recovery copy - only the new `ClearCache(dir)` removes it.
- Settings: the single Clear action is split into **Clear metadata** (cache
  only - the file index survives, so no re-walk) and **Clear index** (drops the
  file index for a full re-index; the metadata cache is untouched).
- Library scan does each piece of disk work once: one directory enumeration
  per folder (reused for the folder hash and the recorded record), no second
  traversal or repeated hashing at record time, the index saves only when
  records changed, and every cached key is probed once for its live verdict;
  a hit is then served from memory without a second read, and pure cache
  hits no longer queue behind the network limiter. Pruning now
  happens on the scan write path, so loading the index never mutates it.
  Measured on this box (see `go/internal/library/BENCH.md`): `ScanWarm`
  26.3 ms -> 20.3 ms, indexed second pass 25.0 ms -> 20.0 ms without the
  index and 19.1 ms -> 8.8 ms with it (-54%), the real-shape benchmark
  0.59 s/op on the reduced corpus while the full-scale baseline could not
  finish a single op in over 30 minutes, and a real-corpus read-only warm
  walk (`metawarm -dry -offline` over the three configured media roots: two
  movie libraries and one anime library) covering 4,255 files / 3,776 unique
  in about 1.2 s.
- Library window scan progress is phase-aware: a sweeping strip while
  walking, a determinate keys bar while resolving, with live counters,
  elapsed time, and an ETA only when a measured resolve-phase rate supports
  one; repaints are coalesced to about ten per second with the terminal
  state always painted; cancelling keeps the progress made so far.

### Fixed

- Stale cached resolutions no longer survive this matching fix either: the
  metadata cache schema bumped to 8 (the year-override corroboration is now
  judged by content token, not token length -- a token corroborates when it
  carries a letter and is neither a generic function word, a romanised
  particle, a release-form word, nor a numeral form. Pre-fix misses like the
  2009 `Dragon Ball Kai` (`6033`) for a `DBZ Kai`/2014 query must re-resolve
  to `20635`, whose canonical titles overlap the query on the content token
  `kai`; the Yamato corroboration and the `Monster`/2019 refusal are
  unchanged). Records written under an older schema are a whole-record miss
  and re-resolve on their own, as before.
- Stale cached resolutions no longer survive this matching fix either: the
  metadata cache schema bumped to 7 (an AniList alias may now extend the
  year-override identity only with canonical corroboration -- the query
  shares a distinctive token with the entry's canonical titles. Pre-fix
  misses like the 1977 Yamato movie for a `Space Battleship Yamato`/1974
  query must re-resolve to the 1974 series (`1650`), while the
  corroboration-free `Monster`/2019 refusal (the 2004 series `19` over the
  `Golden Kamuy` OVA `124756`) is unchanged). Records written under an
  older schema are a whole-record miss and re-resolve on their own, as
  before.
- Stale cached resolutions no longer survive this matching fix either: the
  metadata cache schema bumped to 6 (the year-override coverage is now judged
  across canonical titles only -- an AniList synonym like `"Monster"` on the
  `Golden Kamuy` OVA (`124756`, 2019) no longer authorizes a year override
  that steals a `Monster`/2019 query from the 2004 series (`19`); pre-fix
  positives of that shape must re-resolve rather than keep serving). Records
  written under an older schema are a whole-record miss and re-resolve on
  their own, as before.
- Stale cached resolutions no longer survive these matching fixes either: the
  metadata cache schema bumped to 5 (the year-preference coverage is now
  judged across entry aliases instead of the display name alone, and synonyms
  are never displayed as the English title -- pre-fix positives like `Sailor
  Moon` for a 2014 Crystal query, or a synonym arc-name card for `Gintama`,
  must re-resolve rather than keep serving). Records written under an older
  schema are a whole-record miss and re-resolve on their own, as before.
- Year disambiguation no longer goes blind on English display titles: the
  year-preference coverage test is judged across the entry's aliases, not the
  one display string. `Bishoujo Senshi Sailor Moon` with year 2014 picked the
  1992 series (`Sailor Moon`) because Crystal's English display name (`Pretty
  Guardian Sailor Moon Crystal`) fails coverage its romaji alias covers; it
  now selects Crystal (`7163`). Measured live 2026-10-15.
- Synonyms are matching material only, never display text: an AniList entry
  with null `english` now displays romaji instead of the first synonym. Live
  `Media(id:20760)` (`Gintama: Yorinuki Gintama-san on Theater 2D`) showed
  the romaji arc-name first synonym (`Shinsengumi Douran-hen, Kabuki Chou
  Shitennou-hen`) and dropped `Gintama` from the card; synonym queries still
  match the entry. Measured live 2026-10-15.
- Stale cached resolutions no longer survive a matching change: the metadata
  cache schema bumped to 4 (the alias-aware anime matching / English-first
  display fix changed what keys should resolve to but kept serving pre-fix
  positives, e.g. `Shingeki no Kyojin: Picture Drama` for the romaji AoT
  query). Records written under an older schema are now a whole-record miss
  and re-resolve on their own -- no cache deletion -- and the library scan
  index (already gated on the same version) re-scans instead of skipping.
- Wikipedia disambiguation suffixes no longer reach the card: a page title
  like `Ready Player One (film)` / `Dune (2021 film)` / `Backrooms (film)` is
  stored and shown as the bare title (`Ready Player One`), so the movie line
  reads `Ready Player One (2018)` instead of `Ready Player One (film) (2018)`.
  Only a trailing parenthetical holding an optional year plus a media kind word
  (`film`, `movie`, `TV series`, …) is stripped -- `Nymphomaniac (Director's
  Cut)` survives. Cached records written before the strip heal on decode (no
  schema bump, no refetch). Deliberate departure from the frozen Go reference
  (`go\internal\metadata\wikipedia.go:504`), which stored the raw page title.
- Truncated card lines are now always signalled, never silent: a cut landing on a sentence end returns the complete sentence with its own `.`/`!`/`?` and no ellipsis, and every other cut ends with a single `…`; a whole-word end wherever the text has word boundaries, always a terminator otherwise — even a note with no spaces at all (a long token, CJK text), cut on a character boundary and still ending with that `…`:
  strips trailing `, ; : - – — ·` before the terminator, and ends with U+2026
  when cut (nothing extra when whole). The live card carried `Majin Buu,…` /
  `meet Elliot;…` in `state` and a mid-word `…Zak Penn and Ernest` in
  `large_text`. Deliberate departure from the frozen Go reference
  (`go\internal\presence\presence.go:573-644,673-675`), which kept the cut
  punctuation and hard-cut the tooltip.
- Wikipedia artwork fallback now also accepts the article's own file: when no
  PageImages image and no `*poster*` file exist, a file named exactly like the
  page (`File:Ready Player One (film).png`) resolves via `imageinfo` -- the
  live article carries that poster while PageImages serves none (fair-use), so
  the card showed `logo`. Measured live 2026-10-15.

- Arc-suffixed series filenames (`Show - Arc - NN`, e.g. `Dragon Ball
  Super - Beerus - 01`) now resolve: when the full title matches nothing,
  the lookup normalizes the separator (` - ` -> `: `) instead of truncating
  the name, so the card shows the 2026 series, `The God Who Destroys`, and
  artwork instead of the 2015 series or a bare absolute tag.
- `[Anime Time] Dragon Ball Super Movie 01 - Broly.mkv` now parses as the 2018
  MOVIE `Dragon Ball Super: Broly` (no season, no episode): a trailing
  `Movie <n>` / `the Movie` marker outranks the weak absolute-episode tag the
  cleaner's `ALT_EP` split leaves behind, and the marker's own number is never
  an episode, so it no longer resolves as the series `Dragon Ball Super Movie`
  episode 1. A strong `SxxEyy` tag still wins, and a title that merely ends in
  the word (e.g. `The Lego Movie`) is left exactly as parsed.
- Romaji anime filenames no longer resolve to a romaji-named spin-off.
  `Shingeki no Kyojin S01E03` came back as the `Shingeki no Kyojin: Picture
  Drama` entry (`Day 5/Day 6/Day 7`) with a romanized card title instead of
  the main TV series. Every anime candidate now scores all of its known
  titles (AniList romaji + English + synonyms, Tenrai `title` +
  `title_english`, Kitsu canonical + `en` + `en_jp`), so the main series'
  exact romaji alias outranks the special's near match, and the display
  title prefers English in every leg (AniList falls back to the first
  synonym, measured live English for entries whose `english` is null,
  before romaji). The live chain now returns `Attack on Titan` S1E3 from
  Kitsu. Measured live 2026-10-15.
