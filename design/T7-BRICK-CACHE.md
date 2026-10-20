# T7 Brick Cache — frozen SPEC (T7-2-B)

Status: frozen. This is the repo-resident SPEC for the per-field brick cache.
It closes the "SPEC only in chat" gap: chat sketches no longer govern; this
file does. T7-2-B implements exactly this file. Anything not named here is
not in scope.

## 1. Objective

Cache one fact per (entity, episode, field) slot on disk so repeat card
serves never re-fetch, with bounded negative caching (a definitive miss stays
quiet for one day, never permanent) and hand corrections that no provider
answer may overwrite. The brick layer stores, serves, and fills facts; it
performs no resolver/app/card/UI wiring (T7-3 and later own the merge).

Non-goals (explicitly out): resolver/app/card/UI wiring, a TMDb client,
force / Fix-match flows (T7-4 owns them), pushing, app launches.

## 2. Fields

`BrickField` (entity-level unless noted):

- `Artwork` — series/movie poster URL (entity-level; episode stills never
  override).
- `EpisodeTitle` — one episode's title (episode-level).
- `EpisodeSynopsis` — one episode's plain-text summary (episode-level).
- `ShowSynopsis` — series-level summary (entity-level).
- `MovieSynopsis` — movie-level summary (entity-level).
- `Genres` — joined display names, `Vec<String>` payload (entity-level).
- `Length` — `"45 min"`, `"2h 19m"` (entity-level).
- `Year` — `i32` payload (entity-level).
- `Title` — canonical title (entity-level). Manual-only until T7-4:
  extraction never emits it, priority answers `[]`, fill never attempts it.

Value shapes: `Year` takes `BrickValue::Year`, `Genres` takes
`BrickValue::Genres`, every other field takes `BrickValue::Text`. A Found
brick with a shape mismatch or an empty value (blank text, zero year, no
usable genre name) is rejected at insert with no write. Miss bricks carry the
empty-text placeholder, which is meaningless by design.

Applicable field sets per pass:

- movie: Artwork, MovieSynopsis, Genres, Length, Year.
- series with episode (`EpisodeKey::Episode` / `Absolute`): Artwork,
  EpisodeTitle, EpisodeSynopsis, ShowSynopsis, Genres, Length, Year.
- series without episode: the show-level set only (no episode slots).

## 3. Identity

`EntityKey { id, tmdb?, tvdb?, tvmaze?, anilist?, imdb? }`:

- `id` is show-level, derived by `entity_key_for_query`:
  `series|<title>|<year>` for series (no season — S01 and S02 of one show
  share one bucket), `movie|<title>|<year>` for movies. `UNKNOWN` buckets
  like series. Title folding is `cache::cache_key_title` (lowercase,
  whitespace-collapsed). Examples: `series|breaking bad|2008`,
  `movie|dune|2021`.
- The map bucket is `EntityKey::key()` (lowercase, whitespace-collapsed id).
  `insert` rejects a brick whose entity id does not match the bucket, so one
  show's facts can never file under another show.
- External IDs are corroborating metadata, learned over time: `insert` /
  `insert_many` absorb IDs without clobbering held ones; a refused brick
  (Miss over Found, provider Found over Manual Found) still teaches its IDs.
  IDs never fork a bucket.

`EpisodeKey`: `None` for entity-level fields, `Episode{season,number}` for
`S02E03`, `Absolute{number}` for anime absolute numbering. Derived by
`episode_key_for_query`, which keeps the query's own addressing: an absolute
number never becomes `Episode{season:0}` (no `S00`), movies always `None`.

The resolver's media cache (`cache::cache_key`, per show-season) is
untouched: brick identity is deliberately coarser so show-level Found bricks
are reused across episodes and T7-4 force invalidation is show-scoped.

## 4. Capability matrix (truth)

`capacity(source, kind) -> &'static [BrickField]`. A wrong-kind source
answers `[]` (its `find` gates with `Ok(None)` before any fetch). `Tmdb`
(reserved, no client) and `Manual` (never fetched) answer `[]` for every
kind. `UNKNOWN` reads as series.

Series legs (`Kind::SERIES`, and `UNKNOWN`):

- tvmaze: Artwork, EpisodeTitle, EpisodeSynopsis, ShowSynopsis, Genres,
  Length, Year.
- anilist: Artwork, EpisodeTitle, ShowSynopsis, Genres, Length, Year
  (no EpisodeSynopsis — rows carry titles only).
- tenrai: all seven series fields.
- kitsu: Artwork, EpisodeTitle, EpisodeSynopsis, ShowSynopsis, Length, Year
  (no Genres).
- cinemeta: Artwork, EpisodeTitle, EpisodeSynopsis, ShowSynopsis, Genres,
  Year (no series Length — the series `Media` carries `length: ""`).
- episodate: Artwork, EpisodeTitle, ShowSynopsis, Year.
- bangumi: Artwork, ShowSynopsis, Year.
- livechart: Artwork, ShowSynopsis, Year.
- ann: Artwork, ShowSynopsis, Length, Year.
- wikipedia, justwatch: `[]` (movie-only). tmdb, manual: `[]`.

Movie leg (`Kind::MOVIE`):

- cinemeta: Artwork, MovieSynopsis, Genres, Length, Year.
- wikipedia: Artwork, MovieSynopsis, Year.
- justwatch: Artwork, MovieSynopsis, Genres, Length, Year.
- every series provider, tmdb, manual: `[]`.

## 5. Policy order and `priority`

Policy (pre-filter, most capable first):

- Artwork series: AniList, TVmaze, Tenrai, Kitsu, Cinemeta, Episodate,
  Bangumi, Livechart, Ann.
- Artwork movie: Cinemeta, JustWatch, Wikipedia.
- EpisodeTitle: TVmaze, Tenrai, Kitsu, Cinemeta, Episodate, AniList.
- EpisodeSynopsis: TVmaze, Tenrai, Kitsu, Cinemeta.
- ShowSynopsis: TVmaze, AniList, Tenrai, Kitsu, Cinemeta, Episodate, Bangumi,
  Livechart, Ann.
- MovieSynopsis: Wikipedia, Cinemeta, JustWatch.
- Genres series: TVmaze, AniList, Tenrai, Cinemeta.
- Genres movie: Cinemeta, JustWatch.
- Length series (policy): TVmaze, AniList, Tenrai, Kitsu, Cinemeta, Ann.
- Length movie: Cinemeta, JustWatch.
- Year series: Cinemeta, TVmaze, AniList, Tenrai, Kitsu, Episodate, Bangumi,
  Livechart, Ann.
- Year movie: Cinemeta, Wikipedia, JustWatch.
- Title: none.

`priority(kind, field)` returns the policy order filtered through
`capacity`: no entry names an incapable source. The one live filtering is
Length series, where Cinemeta drops out, serving TVmaze, AniList, Tenrai,
Kitsu, Ann. Wrong-kind synopsis/episode slots answer `[]` (movies never walk
episode/show slots; series never walk the movie slot).

`fill_entity` walks `priority` against the supplied sources, then appends
any supplied capable source the policy does not name (in supplied order,
last), so a new provider is never silently ignored. Unsupplied providers are
never fetched.

## 6. Lookup and miss semantics

`BrickCache::lookup` answers four states: `Found` (serve, no fetch),
`MissWaiting` (fresh miss inside the quiet day, no fetch), `RetryDue`
(refetch), `Absent` (fetch). `lookup_at` is the explicit-clock core; tests
drive retry through the stored clock, never the wall clock.

`fill_entity` per applicable field:

- `Found` / `MissWaiting` slots are skipped: no fetch, no write.
- `RetryDue` / `Absent` slots consult capable supplied sources only.
  Incapable sources are skipped before any call and never count toward the
  miss rule — a gated `Ok(None)` from a source that cannot answer the slot
  must never become a 24 h Miss.
- One answer per provider per pass (local fetch map): a provider backing
  several fields is called at most once.
- `Ok(Some(media))`: the cross-entity guard (`answer_matches_target`:
  normalised title must agree; when both sides name a year they must agree; a
  missing year on either side rejects nothing) may reject; otherwise only the
  requested field is extracted. A media without the field is a definitive
  skip to the next provider. A win inserts `Found` keyed to the target
  identity with provider IDs absorbed (`with_media_id`) and free-text
  provenance (`"<provider> <title>[ S01E01|#51]"`).
- `Ok(None)`, a field-less media, or a guard rejection simply continues.
- `Err` records `(field, "provider: message")` in the report and continues;
  it inserts no Miss for that field.
- After the last provider: Found goes to `report.found`; any error with no
  win goes to `report.errored` and the slot stays `Absent`; a Miss is stored
  (bounded `Brick::miss`, `report.missed`) only when `consulted_capable >= 1`
  and every consulted capable source answered definitively with the field
  absent. Zero capable sources supplied/available leaves `Absent` — no 24 h
  penalty, retry next pass.
- Found/MissWaiting skip behavior is unchanged.

All bricks of a pass persist through one `insert_many` call: one save,
all-or-nothing.

## 7. Persistence and atomicity

File: `%LOCALAPPDATA%\Potflix\brick-cache.json` (executable-dir fallback),
envelope `{"version":1,"entities":[...]}`. `BRICK_SCHEMA = 1`;
`MISS_RETRY_AFTER` is exactly 86_400 s.

- Load-or-empty, never fatal: absent, malformed, or foreign-version files
  load as an empty cache; only real I/O failures (permissions, path through
  a file) error. Unknown JSON fields are ignored. A file naming one bucket
  twice loads last-entry-wins; within one entry, Manual beats provider
  regardless of order, same-source duplicates are last-wins; foreign-bucket
  bricks and zero-brick buckets are dropped.
- `save` is atomic: marshal, stage `.tmp-<pid>-<nanos>-<seq>.json` beside
  the target, write, rename over it. A failed write/rename removes the temp
  and leaves the previous file byte-identical. Every `insert`/`clear_*`
  saves in the same call; `insert_many` saves once; a failed save rolls
  memory back so memory always matches the last successful save. An empty
  batch stores and writes nothing.
- Slot precedence: a Miss over a Found slot is refused (the good value
  wins); a provider Found over a Manual Found is refused (the hand correction
  wins); Manual overwrites provider freely. Refused bricks still absorb IDs.
- Force clears (T7-4 seam): `clear_entity` (all episodes/fields),
  `clear_episode` (episode bricks go, entity-level survive), `clear_field`
  (one slot). Absent clears are no-ops with no write; emptied buckets drop
  before the save; failed saves roll back.

## 8. Seams

- T7-3 (merge): builds on `EntityKey` + `EpisodeKey` identity,
  `Brick::found`/`Brick::miss`, `insert`/`insert_many`, `lookup`/`lookup_at`,
  `capacity`/`priority` + `extract_field_bricks`, `fill_entity` +
  `FillReport`, `clear_*`, atomic `save` + load-or-empty. No further brick
  API is planned.
- T7-4 (force / Fix-match): drives `clear_entity`/`clear_episode`/
  `clear_field` from the UI, then refills. Show-level entity keys make force
  invalidation show-scoped by construction. Titles stay manual-only until
  this flow.

## 9. Deviations (frozen)

- No TMDb client exists in this repo: no priority list names TMDb;
  `BrickSource::Tmdb` is the reserved provenance slot and
  `BrickSource::for_provider("tmdb")` answers `None`.
- Wikipedia and JustWatch are movie-only; every anime/series provider
  (TVmaze, AniList, Tenrai, Kitsu, Episodate, Bangumi, LiveChart, ANN) is
  series-only. Kind gating in each `find` is unchanged; capability is where
  the fill engine honors it.
- Cinemeta series carries no runtime (`length: ""`); the Length-series
  policy still names it but filtered `priority` drops it.
- Genres/Length/Year extraction itself stays kind-blind per-provider (it
  yields whatever the `Media` carries); kind-correctness lives in
  `capacity` + the fill skip, not in extraction.

## 10. Invariants (each covered by a test)

- No `priority` entry fails `capacity`.
- Every applicable field has ≥ 1 capable provider for each kind.
- An incapable supplied source is never called and yields `Absent`, not Miss.
- A capable source omitted from policy but supplied is consulted last.
- Miss requires a consulted capable source (`consulted_capable >= 1`);
  zero capable sources store nothing and report no miss.
- Any `Err` yields `errored` + `Absent`, never Miss.
- S01E01 and S02E03 share one entity bucket, hold distinct episode keys,
  and share show-level Found bricks.
- A wrong-title or wrong-year answer files nothing (definitive Miss, never
  another work's facts).
- A Miss waits until its stored `retry_after` (one literal day after its
  stored `fetched_at`) and is due at it; a Found brick never expires and a
  later Miss never evicts it; Manual Found beats provider Found.
