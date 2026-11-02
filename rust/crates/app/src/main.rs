//! Potflix tray application -- mirrors the Go root package `main` (`go\main.go`).
//!
//! Console-less by design: `#![windows_subsystem = "windows"]` is the equivalent
//! of Go's `-ldflags "-H=windowsgui"`, so `--selftest` output only reaches a
//! parent that redirects stdout -- exactly like the Go build.
//!
//! This is the app's own seam, and the last one: the single-instance lock, the
//! debug log, the tray window and message loop, the Discord transport, the poll
//! loop and the settings window all exist in one process here. The four frozen
//! orderings are reproduced exactly, with two recorded repairs:
//!
//! 1. the logger is created BEFORE the instance lock, so a lock loser can log
//!    its verdict to the debug log (the old "lock before log" order made a
//!    squatted port a silent no-op under `windows_subsystem`);
//! 2. the logger, then the client-id SOURCE only -- never the value;
//! 3. `--selftest` (via [`potflix::selftest`]) bypasses [`run_ui`] wholly;
//! 4. the tray window is created and pumped on one OS thread.
//!
//! # Where `go\events.go` lives
//!
//! The frozen module map (`src\lib.rs`) gives the app library exactly one module,
//! `selftest`. The event-line emitters from `go\events.go` are therefore exposed
//! from [`potflix::selftest`] (`event_connected`, `event_push_attempt`,
//! `event_decision`, ...) and called from here as `selftest::event_*`. That keeps
//! one writer per log line instead of duplicating the formats across the bin.
//!
//! # Known deviations (recorded, not silent)
//!
//! * `tray::clear_activity` now has an installable seam
//!   ([`tray::set_clear_activity`]); [`run_ui`] installs the dc-bound clear
//!   exactly as Go does at `main.go:197-200`, so the tray's "Exit Potflix" path
//!   sends the IPC clear instead of only logging.
//! * `window::set_app_log` and `tray::set_logger` both take a `&'static`
//!   [`Logger`]; [`run_ui`] installs the same logger in both seams, so the
//!   tray and the UI write to one shared log with no second file handle
//!   (Go's `appLog = log` at `main.go:121`).

#![windows_subsystem = "windows"]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app_support::clientid;
use app_support::config;
use app_support::debuglog::{self, Logger};
use app_support::instance;
use metadata::overrides;
use player::control::PlayStatus;
use potflix::selftest;
use potflix_win::{tray, window};
use presence::Tracker;
use presence::gate::Gate;

/// Per-poll sleep while tracking (`go\main.go:31`).
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Play status Undefined -> re-enumerate (`go\main.go:32`).
const UNDEFINED_SLEEP: Duration = Duration::from_millis(160);
/// No player window -> idle (`go\main.go:33`).
const NO_PLAYER_SLEEP: Duration = Duration::from_millis(1000);
/// Sustained stop: how long a still-Stopped tracker action, with the player
/// window still alive, counts as no player at all. A stopped player (or one
/// closed to the tray) looks exactly like this, and the last movie must not stay
/// parked on the card for as long as the window lives (`sustained_stop`).
const STOP_GRACE_MS: i64 = 15_000;

/// The single metadata resolver for the poll loop, set once in [`run_ui`] before
/// the loop starts (unset in tests, which keep the clean card) (`go\main.go:42`).
static META_RESOLVER: OnceLock<metadata::Resolver> = OnceLock::new();

/// The folder-aware query cache keyed by the current raw title
/// (`go\main.go:81-85`): the poll loop is the only writer. The playing file's
/// path rides beside the query, so ONE window query per title change serves
/// both the ladder's query and the latch lookup.
static META_CACHE: LazyLock<Mutex<Option<(String, metadata::Query, String)>>> =
    LazyLock::new(|| Mutex::new(None));

/// Wall-clock milliseconds, Go's `time.Now().UnixMilli()`.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// The stored per-kind card specs: whatever the Card view last published to the
/// process-wide mirror, or the shipped defaults when nothing was published
/// yet. The poll loop reads this every poll, so a preset save takes effect on
/// the next push with no restart.
fn live_card_specs() -> app_support::config::CardSpecs {
    config::live_card_specs()
}

/// The cached query and playing-file path for `raw_title`; on a miss, queries
/// the playing-file path from the player window once and caches both by raw
/// title (`go\main.go:89-102`).
fn meta_cache_get_or_query(raw_title: &str) -> (metadata::Query, String) {
    if let Some((cached_title, cached_query, cached_path)) = META_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        && cached_title == raw_title
    {
        return (cached_query.clone(), cached_path.clone());
    }
    let mut path = String::new();
    if let Some(hwnd) = player::window::find_first()
        && let Ok(p) = player::pathquery::current_file(hwnd)
    {
        path = p;
    }
    let query = metadata_core::context::resolve_query(raw_title, &path);
    *META_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((raw_title.to_string(), query.clone(), path.clone()));
    (query, path)
}

/// The folder-aware query for `raw_title` (`go\main.go:89-102`).
fn meta_query_for(raw_title: &str) -> metadata::Query {
    meta_cache_get_or_query(raw_title).0
}

/// The playing file's full path for `raw_title`: the same window query the
/// card already performs, cached against it -- never a second window query per
/// poll.
fn playing_file_path(raw_title: &str) -> String {
    meta_cache_get_or_query(raw_title).1
}

/// The card path's view of the manual latch store (the launcher's
/// `%LOCALAPPDATA%\Potflix\overrides.json`): loaded once, then reloaded only
/// when the store file's modification time changes, so a "Fix match" save
/// takes effect on the next poll. A malformed store is reported once and
/// treated as empty -- the card path never panics and never blocks on it.
struct LatchStore {
    path: PathBuf,
    mtime: Option<SystemTime>,
    store: overrides::Store,
    reported_bad: bool,
}

impl LatchStore {
    fn new(path: PathBuf) -> LatchStore {
        LatchStore {
            mtime: None,
            store: overrides::Store::empty_at(path.clone()),
            reported_bad: false,
            path,
        }
    }

    /// Reloads when the store file's mtime moved (a save landed, the file
    /// appeared, or it disappeared). The mtime is the one stat'ed BEFORE the
    /// load, so a save landing during the load only means the next refresh
    /// reloads again -- never a missed change.
    fn refresh(&mut self) {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|meta| meta.modified())
            .ok();
        if mtime == self.mtime {
            return;
        }
        match overrides::load_from(&self.path) {
            Ok(store) => {
                self.store = store;
                self.reported_bad = false;
            }
            Err(err) => {
                self.store = overrides::Store::empty_at(self.path.clone());
                if !self.reported_bad {
                    eprintln!(
                        "app: overrides {}: {err} (latch ignored until the store changes)",
                        self.path.display()
                    );
                    self.reported_bad = true;
                }
            }
        }
        self.mtime = mtime;
    }
}

/// The process-wide latch store, built on first use; tests point it at a
/// fixture, production at the store's own default location.
static LATCH_STORE: LazyLock<Mutex<Option<LatchStore>>> = LazyLock::new(|| Mutex::new(None));

/// The latch that wins for one playing file, resolved from the store alone, or
/// `None` when the ladder keeps the job.
fn latch_resolved(path: &str, query: &metadata::Query) -> Option<presence::Resolved> {
    let mut guard = LATCH_STORE.lock().unwrap_or_else(|e| e.into_inner());
    let store = guard.get_or_insert_with(|| LatchStore::new(override_store_path()));
    store.refresh();
    store
        .store
        .resolve_for(path)
        .map(|off| resolved_from_latch(off, query))
}

/// One latch as the card's resolved inputs. A movie latch renders its `media`
/// snapshot, with the latch poster as the artwork fallback; a season/show
/// latch renders the playing episode's pin -- its title, synopsis and episode
/// code -- under the latch's own series name, poster and year. With no pin
/// for the playing episode the show title/poster still resolve and the code
/// rides from the query (the provisional path), so the latch is never a
/// forced movie: `is_movie` follows the latch scope, not the filename.
fn resolved_from_latch(off: &overrides::Override, query: &metadata::Query) -> presence::Resolved {
    match off.scope {
        overrides::Scope::Movie => {
            let mut res = presence::Resolved {
                is_movie: true,
                ..presence::Resolved::default()
            };
            if let Some(media) = off.media.as_ref() {
                res.series = media.title.clone();
                res.artwork = media.artwork.clone();
                res.synopsis = media.synopsis.clone();
                res.genres = media.genres.clone();
                res.length = media.length.clone();
                res.year = media.year;
            }
            if res.series.is_empty() {
                res.series = off.title.clone();
            }
            if res.year <= 0 {
                res.year = off.year;
            }
            if res.artwork.is_empty() {
                res.artwork = off.poster.clone();
            }
            res
        }
        overrides::Scope::Season | overrides::Scope::Series => {
            let mut res = presence::Resolved {
                series: off.title.clone(),
                artwork: off.poster.clone(),
                year: off.year,
                is_movie: false,
                ..presence::Resolved::default()
            };
            let pin = if query.absolute {
                off.episode_pin_absolute(query.episode)
            } else {
                i32::try_from(query.season)
                    .ok()
                    .and_then(|season| off.episode_pin(season, query.episode))
            };
            if let Some(pin) = pin {
                res.episode_name = pin.title.clone();
                res.synopsis = pin.synopsis.clone();
                res.season = i64::from(pin.season);
                res.number = pin.number;
                res.absolute = pin.absolute;
            } else if query.absolute {
                res.absolute = query.episode;
            } else {
                res.season = query.season;
                res.number = query.episode;
            }
            res
        }
    }
}

/// The store path the card path reads: the store's own default in production;
/// a test reads its fixture, or -- with no fixture -- a path that never
/// exists, so no test ever reads the owner's real store.
fn override_store_path() -> PathBuf {
    #[cfg(test)]
    {
        APP_OVERRIDES_PATH_TEST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| {
                std::env::temp_dir().join("potflix-app-test-overrides-absent.json")
            })
    }
    #[cfg(not(test))]
    {
        overrides::default_path()
    }
}

#[cfg(test)]
static APP_OVERRIDES_PATH_TEST: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Installs (or clears) the fixture store path the card path reads, and drops
/// the cached store so the next lookup reloads from the new path; returns the
/// previous fixture.
#[cfg(test)]
fn set_overrides_path_for_test(path: Option<PathBuf>) -> Option<PathBuf> {
    let previous = std::mem::replace(
        &mut *APP_OVERRIDES_PATH_TEST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        path,
    );
    *LATCH_STORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    previous
}

/// `pushActivity` (`go\main.go:332-394`): build the card, ask the gate whether it
/// must go out, log the attempt (the observation boundary), and send only when
/// the gate says so. A failed send is not fatal -- the supervisor reconnects and
/// a later decision resends -- but it is never silent.
///
/// `specs` is the stored per-kind Card view choice (`liveCardSpecs`) and
/// `record` is the preview sink (`recordPreview`), injected so this one
/// implementation is both the production path the poll loop runs and the path
/// the cardview tests drive.
///
/// The preview is recorded only AFTER a successful send (`main.go:391-392`).
/// A rate-limited hold (`rate-limited`) sends nothing and records nothing;
///
/// the newest held card flushes on the first poll after the 5-per-20 s window.
/// Returns true only on a successful real-media send (the preview was
/// recorded); every other path -- idle clear, gate suppression, dormant
/// clear, send error -- returns false so the caller can tell a card from a
/// non-send.
#[allow(clippy::too_many_arguments)]
fn push_activity(
    log: &Logger,
    dc: &discord::Client,
    gate: &mut Gate,
    specs: app_support::config::CardSpecs,
    record: &mut dyn FnMut(
        app_support::config::CardKind,
        String,
        Option<presence::Action>,
        presence::Resolved,
    ),
    raw_title: &str,
    action: Option<&presence::Action>,
    ct: i64,
    total: i64,
    now: i64,
    event: bool,
    debounced: bool,
) -> bool {
    // T5-C: an idle title never becomes a card. The presence composer would
    // render `Browsing`/`Idle` for exactly these inputs; clear instead, record
    // no preview, and leave the gate untouched so the next real media sends
    // fresh.
    if is_no_media_title(raw_title) {
        if let Err(err) = dc.clear_activity() {
            selftest::event_send_error(log, &err);
        }
        return false;
    }
    let query = meta_query_for(raw_title);
    let mut res = presence::Resolved::default();
    let mut found = false;
    // The stored manual latch wins before the ladder, exclusively: a latched
    // file's card is built from the override store alone, with no resolver call
    // and so no network.
    if let Some(latched) = latch_resolved(
        &playing_file_path(raw_title),
        &query,
    ) {
        res = latched;
        found = true;
    } else if let Some(resolver) = META_RESOLVER.get() {
        // Non-blocking: a miss fires at most one background fetch per key.
        // Unknown kinds (themes, extras, bare names) never fetch or cache.
        if query.kind != metadata::Kind::UNKNOWN {
            let cancel = metadata_http::Cancel::background();
            if let Ok(answer) = resolver.resolve(&cancel, &query)
                && answer.found
            {
                res = presence::Resolved {
                    series: answer.series,
                    episode_name: answer.episode,
                    artwork: answer.artwork,
                    synopsis: answer.synopsis,
                    genres: answer.genres,
                    length: answer.length,
                    year: answer.year,
                    is_movie: answer.is_movie,
                    season: answer.season,
                    number: answer.number,
                    absolute: if query.absolute { query.episode } else { 0 },
                    buttons: Vec::new(),
                };
                found = true;
            }
        }
    }
    // The kind follows the answer when one was found, else the query; an
    // UNKNOWN title takes the series ladder.
    let is_movie = if found {
        res.is_movie
    } else {
        query.kind == metadata::Kind::MOVIE
    };
    let kind = if is_movie {
        app_support::config::CardKind::Movie
    } else {
        app_support::config::CardKind::Series
    };
    // Always compose: with the default spec for a kind, `compose_card`
    // reproduces the builder's output byte-for-byte, so no skip branch is
    // needed.
    let card = presence::compose_card(raw_title, action, &res, is_movie, &specs.for_kind(kind));
    // The designer's field choices filter the composed card on the one path the
    // sender uses. The default spec is identity, so the shipped card is
    // unchanged; a click in the designer is read here at the next poll.
    let card = presence::live_designer().apply(card);
    let (out, verdict) = gate.decide(card, ct, total, now, event, debounced);
    selftest::event_push_attempt(log, &out, &verdict);
    if !verdict.send {
        return false;
    }
    // A pause parks the paused card and keeps it; the gate never sets Clear now,
    // so this branch is dormant and retained for the null-activity path only.
    if verdict.clear {
        if let Err(err) = dc.clear_activity() {
            selftest::event_send_error(log, &err);
        }
        return false;
    }
    selftest::event_push(log, &out.details, &out.state, ct, total);
    let wire = discord::Activity {
        name: out.name.clone(),
        details: out.details.clone(),
        state: out.state.clone(),
        large_image: out.large_image.clone(),
        large_text: out.large_text.clone(),
        small_image: out.small_image.clone(),
        small_text: out.small_text.clone(),
        has_start: out.has_start,
        start: out.start,
        end: out.end,
        buttons: Vec::new(),
    };
    match dc.set_activity(wire) {
        Ok(()) => {
            record(
                kind,
                raw_title.to_string(),
                action.cloned(),
                res.clone(),
            );
            true
        }
        Err(err) => {
            selftest::event_send_error(log, &err);
            false
        }
    }
}

/// Whether `generation` still owes a clear: a clear is only proven for the
/// connection generation it went out on, so a changed generation (a reconnect)
/// must re-assert it. The old boolean latch stayed true across a reconnect and
/// nothing ever re-sent the clear, which left the card stuck on the last movie.
fn clear_due(generation: i64, cleared_gen: i64) -> bool {
    generation != cleared_gen
}

/// T5-C: whether `raw_title` carries no media. The presence composer renders
/// `Browsing`/`Idle` for exactly these inputs, so the app lane clears instead
/// of sending: empty/whitespace, the bare player app name (`PotPlayer`, or the
/// renamed `PotFlix`), with either ` - PotPlayer` / ` - PotFlix` suffix
/// stripped first. A real filename that merely mentions the player keeps its
/// card.
fn is_no_media_title(raw_title: &str) -> bool {
    let mut raw = raw_title
        .strip_suffix(player::window::SUFFIX)
        .unwrap_or(raw_title);
    // A renamed window keeps the same convention (`PotFlix` instead of
    // `PotPlayer`); strip it too so the gate does not depend on the suffix.
    if let Some(stripped) = raw.strip_suffix(" - PotFlix") {
        raw = stripped;
    }
    let title = raw.trim();
    title.is_empty() || title == player::window::APP_NAME || title == "PotFlix"
}

/// Advances the sustained-stop anchor: `since_ms` is the epoch-ms anchor where
/// the current still-Stopped run began (0 = none). A stopped poll keeps the
/// anchor and reports whether `STOP_GRACE_MS` has elapsed; any live poll
/// (Running or Paused) clears it, so a later stop starts a fresh grace.
fn sustained_stop(now_ms: i64, since_ms: i64, stopped: bool) -> (i64, bool) {
    if !stopped {
        return (0, false);
    }
    let since = if since_ms == 0 { now_ms } else { since_ms };
    (since, now_ms - since >= STOP_GRACE_MS)
}

/// The window/control surface of the poll loop: the production
/// [`SystemProbe`] calls the `player::*` functions unchanged, and a test
/// injects a scripted player so the REAL loop body -- real [`Tracker`], real
/// [`Gate`], real [`push_activity`], real clear logic -- can run headless.
trait PlayerProbe {
    /// The probe's window identity; opaque to the loop.
    type Handle: Copy + PartialEq;
    /// The first PotPlayer window, if any.
    fn find_first(&self) -> Option<Self::Handle>;
    /// Whether the window is still valid.
    fn alive(&self, id: Self::Handle) -> bool;
    /// The window's current title.
    fn title(&self, id: Self::Handle) -> String;
    /// Whether a title identifies a PotPlayer window.
    fn matches(&self, title: &str) -> bool;
    /// The decoded play state.
    fn status(&self, id: Self::Handle) -> PlayStatus;
    /// The current playback position (ms).
    fn current_time(&self, id: Self::Handle) -> i64;
    /// The media duration (ms).
    fn total_time(&self, id: Self::Handle) -> i64;
}

/// The production probe: the real `player::*` calls, unchanged.
struct SystemProbe;

impl PlayerProbe for SystemProbe {
    type Handle = player::window::Handle;

    fn find_first(&self) -> Option<Self::Handle> {
        player::window::find_first()
    }

    fn alive(&self, id: Self::Handle) -> bool {
        player::window::alive(id)
    }

    fn title(&self, id: Self::Handle) -> String {
        player::window::title(id)
    }

    fn matches(&self, title: &str) -> bool {
        player::window::matches(title)
    }

    fn status(&self, id: Self::Handle) -> PlayStatus {
        player::control::get_play_status(id)
    }

    fn current_time(&self, id: Self::Handle) -> i64 {
        player::control::get_current_time(id)
    }

    fn total_time(&self, id: Self::Handle) -> i64 {
        player::control::get_total_time(id)
    }
}

/// One tracked window segment: the tracker and gate are made when the window
/// appears and replaced when the Discord generation changes, exactly as the
/// Java/Go loop does per outer iteration.
struct PollSegment<H> {
    id: H,
    tracker: Tracker,
    gate: Gate,
}

/// The poll loop's cross-iteration state: the clear/last-generation anchors and
/// the sustained-stop anchor plus the live segment. [`poll_loop`] owns one for
/// its whole life; a test owns one and drives [`poll_step`] against it.
struct PollState<H> {
    /// The generation a clear went out on; 0 is the pre-connect session.
    cleared_gen: i64,
    /// The generation last observed (a reconnect resets the segment).
    last_gen: i64,
    /// Epoch ms where the current still-Stopped run began; 0 = not stopped.
    stop_since: i64,
    /// The live window segment, if one is being tracked.
    segment: Option<PollSegment<H>>,
}

impl<H> PollState<H> {
    /// The loop's start state: `cleared_gen = 0` keeps Java's start state
    /// (`Main.java:95`: `cleared = true`, the first no-player pass sends no
    /// clear), while every later generation -- a connect or a reconnect --
    /// re-asserts the clear if one is owed.
    fn new() -> PollState<H> {
        PollState {
            cleared_gen: 0,
            last_gen: -1,
            stop_since: 0,
            segment: None,
        }
    }
}

/// One iteration of `pollLoop` (`go\main.go:241-325`): take the window segment
/// forward by one step -- the outer-loop head (find the first PotPlayer window,
/// skip an Undefined play status for 160 ms) when no segment lives, else the
/// inner "track this window" body -- and push every decision to Discord. Two
/// self-healing rules ride along: a clear is re-asserted whenever the client's
/// generation changed since the last clear (`clear_due`), and a window left
/// Stopped past `STOP_GRACE_MS` is treated as no player (`sustained_stop`).
/// The player reads and the clock are injected ([`PlayerProbe`], `clock`) and
/// the sleeps run through `sleep`, so the REAL body is drivable in a test with
/// no player and no wall clock.
fn poll_step<P: PlayerProbe>(
    log: &Logger,
    dc: &discord::Client,
    probe: &P,
    clock: &dyn Fn() -> i64,
    state: &mut PollState<P::Handle>,
    sleep: &dyn Fn(Duration),
) {
    if state.segment.is_none() {
        // A reconnect starts a fresh Discord session. The segment below is reset
        // on every outer iteration anyway (Main.java:109), so only the generation
        // needs noting here.
        if dc.generation() != state.last_gen {
            state.last_gen = dc.generation();
        }

        let Some(id) = probe.find_first() else {
            // No player window: nothing plays, so the UI's latch target is gone.
            config::set_live_now_playing(None);
            if clear_due(dc.generation(), state.cleared_gen) {
                if let Err(err) = dc.clear_activity() {
                    selftest::event_send_error(log, &err);
                }
                selftest::event_cleared_no_player(log);
                state.cleared_gen = dc.generation();
            }
            sleep(NO_PLAYER_SLEEP);
            return;
        };

        if probe.status(id) == PlayStatus::UNDEFINED {
            sleep(UNDEFINED_SLEEP);
            return;
        }

        // Java makes one PresenceTracker per outer iteration (Main.java:109), so
        // a window that disappears and returns starts a fresh segment rather than
        // inheriting the previous segment's last-seen state.
        state.segment = Some(PollSegment {
            id,
            tracker: Tracker::new(),
            gate: Gate::new(),
        });
    }

    // One iteration of the inner "track this window until it dies or stops
    // looking like PotPlayer" loop. `gone` marks the paths that end the segment;
    // the window-gone clear then runs unconditionally, as it always has.
    let mut gone = false;
    {
        let seg = state.segment.as_mut().expect("segment created above");
        if !probe.alive(seg.id) {
            gone = true;
        } else {
            if dc.generation() != state.last_gen {
                state.last_gen = dc.generation();
                seg.tracker = Tracker::new();
                seg.gate = Gate::new();
            }
            let title = probe.title(seg.id);
            if !probe.matches(&title) {
                gone = true;
            } else {
                let status = probe.status(seg.id);
                let ct = probe.current_time(seg.id);
                let total = probe.total_time(seg.id);
                let now = clock();
                let action = seg.tracker.poll(&title, status, ct, now);
                if let Some(a) = action.as_ref() {
                    selftest::event_decision(
                        log,
                        now,
                        &status.to_string(),
                        ct,
                        &title,
                        a.tag(),
                        a.running,
                        a.paused,
                        a.has_start,
                        a.start,
                        &a.suffix,
                    );
                }
                // The card is rebuilt from the current action every poll,
                // including polls where the tracker emits nothing (`current`).
                let cur = action.clone().or_else(|| seg.tracker.current());
                let live = cur.as_ref().is_some_and(|a| a.running || a.paused);
                // T5-C: no idle card, ever. An idle title (empty, the player
                // app name) or a window with no current file is not media:
                // clear (once per generation, like the no-player path),
                // record no preview, and feed nothing to the gate -- so no
                // `Browsing`/`Idle`, no app-name header, and no PotFlix /
                // PotPlayer name ever reaches the wire. A live running/paused
                // action still wins over an empty file read (a transient path
                // query must not clear a real card); a stopped or silent
                // window with no file clears immediately instead of parking.
                let no_media = is_no_media_title(&title)
                    || (!live && playing_file_path(&title).trim().is_empty());
                if no_media {
                    config::set_live_now_playing(None);
                    state.stop_since = 0;
                    if clear_due(dc.generation(), state.cleared_gen) {
                        if let Err(err) = dc.clear_activity() {
                            selftest::event_send_error(log, &err);
                        }
                        log.log("cleared (idle, no media)");
                        state.cleared_gen = dc.generation();
                    }
                } else {
                // The UI's "Fix match" latches the file that is playing: mirror
                // the raw title and its path while a live action (running or
                // paused) is on the card, so the UI reads exactly what the card
                // renders from.
                if let Some(a) = cur.as_ref()
                    && (a.running || a.paused)
                {
                    config::set_live_now_playing(Some(config::NowPlaying {
                        raw_title: title.clone(),
                        path: playing_file_path(&title),
                    }));
                    // Warm the Fix match list for what is playing now: the
                    // poll thread only starts a search when the title/year/scope
                    // changed, so a repeated tick is free and the next episode
                    // of the same show is a cache hit.
                    potflix_win::window::prefetch_now_playing();
                }
                // C: a live window whose action stays Stop past the grace is a
                // stopped player (or one closed to the tray), not a playing one,
                // so clear the card as the no-player branch does -- and record
                // the generation, so a reconnect re-asserts it. The next live
                // poll resets the anchor, and the resume then pushes a live card
                // through the normal path: the stop card carries no anchor, so
                // the running card can never be byte-identical to it and the
                // gate cannot suppress it.
                let stopped = cur.as_ref().is_some_and(|a| !a.running && !a.paused);
                let (since, due) = sustained_stop(now, state.stop_since, stopped);
                state.stop_since = since;
                if due {
                    // A stopped player is not a playing segment: clear the
                    // mirror with the card.
                    config::set_live_now_playing(None);
                    if clear_due(dc.generation(), state.cleared_gen) {
                        if let Err(err) = dc.clear_activity() {
                            selftest::event_send_error(log, &err);
                        }
                        selftest::event_cleared_stopped(log);
                        state.cleared_gen = dc.generation();
                    }
                } else {
                    // The gate is consulted EVERY poll with the current card so
                    // it can re-anchor on divergence, fire the heartbeat, and log
                    // the verdict.
                    let debounced = seg.tracker.pending_debounce();
                    if let Some(cur) = cur {
                        let mut record = |kind,
                                          raw: String,
                                          act: Option<presence::Action>,
                                          res: presence::Resolved| {
                            window::ui().record_preview(kind, raw, act, res);
                        };
                        let sent = push_activity(
                            log,
                            dc,
                            &mut seg.gate,
                            live_card_specs(),
                            &mut record,
                            &title,
                            Some(&cur),
                            ct,
                            total,
                            now,
                            action.is_some(),
                            debounced,
                        );
                        // T5-E: a successful real-media send re-arms the
                        // once-per-generation idle clear. Without this the
                        // next idle/no-media transition on the same
                        // generation is suppressed (`generation ==
                        // cleared_gen`) and the media card stays parked
                        // after the media is gone. A suppressed hold or a
                        // failed send leaves the anchor alone, so idle
                        // polls without an intervening card still clear
                        // exactly once.
                        if sent {
                            state.cleared_gen = -1;
                        }
                    }
                }
                }
            }
        }
    }
    if gone {
        // The window is gone (closed, or no longer PotPlayer): the segment
        // ended, so the clear and the mirror go together.
        state.segment = None;
        config::set_live_now_playing(None);
        if let Err(err) = dc.clear_activity() {
            selftest::event_send_error(log, &err);
        }
        selftest::event_cleared_window_gone(log);
        state.cleared_gen = dc.generation();
        return;
    }
    sleep(POLL_INTERVAL);
}

/// Drives [`poll_step`] for a bounded number of iterations: the seam the
/// poll-loop tests use to run the REAL sequence. Production never calls this.
#[cfg(test)]
fn poll_steps<P: PlayerProbe>(
    log: &Logger,
    dc: &discord::Client,
    probe: &P,
    clock: &dyn Fn() -> i64,
    state: &mut PollState<P::Handle>,
    sleep: &dyn Fn(Duration),
    steps: usize,
) {
    for _ in 0..steps {
        poll_step(log, dc, probe, clock, state, sleep);
    }
}

/// `pollLoop` (`go\main.go:241-325`): run [`poll_step`] on the real player
/// probe, the real wall clock and the loop's real sleeps, until the tray Exit
/// path asks for quit. The Exit dispatch no longer calls `process::exit`: it
/// marks [`tray::quit_requested`] and posts quit, the message loop returns,
/// and the app tail joins this thread before clearing -- so no in-flight
/// `set_activity` races the exit clear.
fn poll_loop(log: &Logger, dc: &discord::Client) {
    let probe = SystemProbe;
    let mut state = PollState::new();
    let sleep = |d: Duration| std::thread::sleep(d);
    loop {
        if tray::quit_requested() {
            break;
        }
        poll_step(log, dc, &probe, &now_ms, &mut state, &sleep);
    }
}

/// The catch-up config the startup worker runs with. Factored out of [`run_ui`]
/// so the one property that must never regress -- a non-empty `app version`,
/// the same one the UI scan path passes (`window.rs`:
/// `settings::APP_VERSION`) -- is testable without launching the app. The empty
/// header in the owner's report came from the `..Default::default()` this
/// replaces.
fn startup_catch_up_config(cfg: &config::Config) -> library::catchup::CatchUpConfig {
    library::catchup::CatchUpConfig {
        dirs: cfg.dirs.clone(),
        cache_dir: config::metadata_dir().to_string_lossy().into_owned(),
        index_path: config::scan_index_path().to_string_lossy().into_owned(),
        app_version: potflix_win::settings::APP_VERSION.to_string(),
        ..Default::default()
    }
}

/// The single-instance handshake magic the primary writes to every lock-port
/// connection. A second instance that reads these exact bytes knows the holder
/// is a live Potflix; anything else (timeout, EOF, wrong bytes, refused) is a
/// foreign listener or a predecessor still tearing down.
const SINGLE_INSTANCE_MAGIC: &[u8] = b"potflix-single-instance-v1\n";

/// How long the lock probe waits for connect and for the magic.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// The lock-loser exit when no live Potflix answered (a real failure, never 0).
const FOREIGN_LOCK_EXIT_CODE: i32 = 1;

/// What the lock-port probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstanceProbe {
    Live,
    Foreign,
}

/// Maps a probe to the loser exit code: a live holder exits 0 (already
/// running, cleanly, with a log line); a foreign holder exits non-zero.
fn lock_failure_exit_code(probe: InstanceProbe) -> i32 {
    match probe {
        InstanceProbe::Live => instance::ALREADY_RUNNING_EXIT_CODE,
        InstanceProbe::Foreign => FOREIGN_LOCK_EXIT_CODE,
    }
}

/// Probes `port`: connects, reads the handshake magic, and reports whether a
/// live Potflix answered. Never binds the live lock port in tests -- callers
/// pass an ephemeral port they staged.
fn probe_instance_port(port: u16) -> InstanceProbe {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(stream) = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return InstanceProbe::Foreign;
    };
    let _ = stream.set_read_timeout(Some(PROBE_TIMEOUT));
    let mut buf = vec![0u8; SINGLE_INSTANCE_MAGIC.len()];
    let mut filled = 0;
    let mut stream = stream;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    if filled == SINGLE_INSTANCE_MAGIC.len() && buf[..] == *SINGLE_INSTANCE_MAGIC {
        InstanceProbe::Live
    } else {
        InstanceProbe::Foreign
    }
}

/// Answers every lock-port connection with the handshake magic, so a second
/// instance can tell a live Potflix from a foreign squatter. Runs detached;
/// the listener itself is held for life by the caller.
fn spawn_instance_responder(listener: &TcpListener) {
    if let Ok(clone) = listener.try_clone() {
        std::thread::spawn(move || {
            for incoming in clone.incoming() {
                match incoming {
                    Ok(mut stream) => {
                        let _ = stream.set_write_timeout(Some(PROBE_TIMEOUT));
                        let _ = stream.write_all(SINGLE_INSTANCE_MAGIC);
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        });
    }
}

/// The lock-loser path: a bind failure is probed, logged to the debug log,
/// and surfaced with the matching exit -- 0 for a live Potflix (cleanly, with
/// a log line), non-zero for a foreign listener (a real failure). Never
/// returns.
fn handle_lock_bind_failure(log: &'static Logger, err: std::io::Error) -> ! {
    let probe = probe_instance_port(instance::LOCK_PORT);
    let code = lock_failure_exit_code(probe);
    match probe {
        InstanceProbe::Live => {
            log.log(&format!(
                "instance lock {}:{} busy: live Potflix answered the probe; exiting ({})",
                instance::LOCK_HOST,
                instance::LOCK_PORT,
                instance::ALREADY_RUNNING_MSG
            ));
            log.close();
            std::process::exit(code);
        }
        InstanceProbe::Foreign => {
            let msg = format!(
                "single-instance bind {}:{} failed: {err} (no live Potflix answered; refusing a foreign listener)",
                instance::LOCK_HOST,
                instance::LOCK_PORT
            );
            log.log(&msg);
            log.close();
            eprintln!("{msg}");
            std::process::exit(code);
        }
    }
}

/// `runUI` (`go\main.go:137-216`): build the hidden window + tray icon, start the
/// Discord transport and the poll loop, then pump messages until exit.
///
/// The tray window and the message loop must share one OS thread; this runs on
/// the process main thread, so that thread law holds without `LockOSThread`.
fn run_ui(log: &'static Logger) {
    let tray_window = match tray::create_tray_window() {
        Ok(window) => window,
        Err(err) => {
            log.log(&format!("tray init failed: {err}"));
            return;
        }
    };
    let tray_failed = !tray_window.added;
    if tray_failed {
        // Deliberate departure from Java :550-551 (see README deviation 16):
        // a tray-less process keeps serving the card with the window visible
        // instead of exiting silently.
        eprintln!("TrayIcon could not be added.");
        log.log("notify-icon add result=false");
    } else {
        log.log(&format!(
            "notify-icon add result=true tooltip=Potflix hwnd=0x{:x} icon=0x{:x}",
            tray_window.hwnd as usize, tray_window.icon as usize
        ));
    }

    // Tray is up: start the Discord transport and the poll loop on their own
    // threads; the message loop below owns this OS thread.
    let (client_id, _) = clientid::resolve_client_id();
    let dc = discord::Client::new(discord::Config {
        client_id: client_id.to_string(),
        on_connect: Some(Box::new(move || selftest::event_connected(log))),
        on_reply: Some(Box::new(move |reply: discord::Reply| {
            if reply.error {
                selftest::event_push_error(
                    log,
                    &reply.nonce,
                    &reply.evt,
                    reply.code,
                    &reply.message,
                );
            } else {
                selftest::event_push_ack(log, &reply.nonce);
            }
        })),
        on_ack_timeout: Some(Box::new(move |nonce: &str| {
            selftest::event_push_ack_timeout(log, nonce);
        })),
        on_drop: Some(Box::new(move |err: &discord::Error| {
            selftest::event_session_dropped(log, err);
        })),
        ..Default::default()
    });
    dc.start();

    let cfg = config::load_config();
    config::set_live_card_specs(cfg.effective_card_specs());
    metadata_http::set_rate_profile(cfg.effective_scan_speed());
    log.log(&format!(
        "scan speed profile: {}",
        cfg.effective_scan_speed()
    ));

    let logf: metadata::resolver::LogFn = Arc::new(move |message: &str| log.log(message));
    let _ = META_RESOLVER.set(metadata::Resolver::new(metadata::Options {
        logf: Some(logf),
        ..Default::default()
    }));

    // Library catch-up: resolve whatever metadata the store-backed index still
    // owes, on a background worker, resuming where the last run stopped. The
    // config carries the app version like the UI scan path does, so the
    // needs-repair report's `app version:` is never empty.
    let _ = library::catchup::start_catch_up(startup_catch_up_config(&cfg), None);

    let dc = Arc::new(dc);
    let poll_dc = Arc::clone(&dc);
    tray::reset_quit();
    let poll_handle = std::thread::spawn(move || poll_loop(log, &poll_dc));

    // Fleet decision 6 (deviation): clear the Discord card before exiting. Go
    // installs the dc-bound override at `go\main.go:197-200`, replacing the tray
    // stub; the poll loop's own clears are already live, so this repairs only
    // the tray Exit path. The Exit dispatch itself no longer clears: the quit
    // tail below clears after the poll thread stops, so no in-flight
    // `set_activity` races it.
    let clear_dc = Arc::clone(&dc);
    let _ = tray::set_clear_activity(Box::new(move || {
        let _ = clear_dc.clear_activity();
        log.log("cleared (exit)");
    }));

    // Settings window: created hidden, shown on manual launch, kept hidden to
    // the tray on the autostart path. `--selftest` never reaches here.
    match window::create_main_window() {
        Err(err) => log.log(&format!("main window init failed: {err}")),
        Ok(()) => {
            let args: Vec<String> = std::env::args().collect();
            if tray_failed {
                // No tray icon, no tray menu, no other way back in: leave the
                // window visible so the app is reachable and quittable (X).
                log.log("tray icon missing: window left visible");
                window::show_main_window();
            } else if potflix_win::settings::should_show_main_window(&args) {
                window::show_main_window();
            } else {
                log.log("autostart launch: window kept hidden (tray only)");
            }
        }
    }

    tray::message_loop();
    // Ordered quit tail (the old `process::exit` in the menu dispatch skipped
    // all of this): stop the poll thread first so no new `set_activity` can
    // start, then clear with ordering, delete the icon, and close Discord.
    // A window-close (X) that bypassed Exit still lands here, so force the
    // quit flag before joining.
    if !tray::quit_requested() {
        tray::request_quit();
    }
    let _ = poll_handle.join();
    let _ = dc.clear_activity();
    log.log("cleared (exit)");
    tray::tray_delete(tray_window.hwnd);
    let _ = dc.close();
}

fn snapshot_request(args: &[String]) -> Option<Result<(String, String), String>> {
    let pos = args.iter().position(|a| a == "--snapshot")?;
    let frame = match args.get(pos + 1) {
        Some(f) => f.clone(),
        None => return Some(Err("snapshot: --snapshot <frame> --out <path.png>".into())),
    };
    let out_pos = args.iter().position(|a| a == "--out");
    let out = match out_pos.and_then(|i| args.get(i + 1)) {
        Some(o) => o.clone(),
        None => return Some(Err("snapshot: --snapshot <frame> --out <path.png>".into())),
    };
    Some(Ok((frame, out)))
}

fn main() {
    // argv is ignored except for the M1 test switch.
    let args: Vec<String> = std::env::args().collect();
    if let Some(req) = snapshot_request(&args) {
        selftest::attach_console();
        let code = match req {
            Ok((frame, out)) => match potflix_win::snapshot::capture_snapshot(&frame, &out) {
                Ok(()) => {
                    println!("SNAPSHOT-OK {frame} {out}");
                    0
                }
                Err(e) => {
                    eprintln!("SNAPSHOT-FAIL: {e}");
                    1
                }
            },
            Err(e) => {
                eprintln!("SNAPSHOT-FAIL: {e}");
                1
            }
        };
        std::process::exit(code);
    }
    let selftest_run = potflix_win::settings::has_arg(&args, "--selftest");
    if selftest_run {
        selftest::attach_console(); // make --selftest output visible from the parent console
    }

    // (1) Debug log beside the executable, FIRST: a lock loser must log its
    // verdict to the debug log (the old lock-before-log order plus `eprintln`
    // under `windows_subsystem` made a squatted port a silent no-op).
    let log: &'static Logger = Box::leak(Box::new(Logger::new(
        debuglog::default_log_path(),
        debuglog::DEFAULT_LOG_CAP_BYTES,
        debuglog::DEFAULT_LOG_MAX_FILES,
    )));

    // (2) Single-instance lock: 127.0.0.1:17843, held for life. A bind failure
    // is probed: a live Potflix answers the handshake and the loser exits 0
    // cleanly with a log line; a foreign listener (or a teardown race with no
    // answer) is a logged failure with a non-zero exit.
    let held_listener = match instance::acquire_instance_lock_on(instance::LOCK_PORT) {
        Ok(listener) => listener,
        Err(err) => handle_lock_bind_failure(log, err),
    };
    spawn_instance_responder(&held_listener);
    std::mem::forget(held_listener);
    log.log(&format!(
        "instance lock acquired {}:{} backlog={}",
        instance::LOCK_HOST,
        instance::LOCK_PORT,
        instance::LOCK_BACKLOG
    ));

    // (3) Client id: log only the source, never the value.
    let (_, source) = clientid::resolve_client_id();
    clientid::log_client_id_source(log, source);

    // `appLog = log` for the tray window's message/menu callbacks and the
    // settings window's `app_logf` (Go `main.go:121`: one shared logger).
    let _ = tray::set_logger(log);
    let _ = window::set_app_log(log);
    let _ = tray::set_show_main_window(window::show_main_window);

    if selftest_run {
        selftest::run_self_test(log);
    }

    run_ui(log);
    log.close();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    /// Serializes the tests that touch the process-wide test state: the live
    /// card mirror, the fixture latch store, and the seeded query cache. A
    /// concurrent publisher would leak into another test's read; a concurrent
    /// `meta_query_for` would evict a seeded query-cache entry (the cache is a
    /// single title slot).
    static LIVE_SERIAL: Mutex<()> = Mutex::new(());

    use player::control::PlayStatus;

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "potflix-s13-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// `gate_integration_test.go` -> `TestGateFakePipeSequence`: headless
    /// end-to-end proof of the push gate through the in-process fake pipe. It
    /// drives the REAL tracker, gate and pushActivity over a scripted position
    /// sequence and asserts the exact SET_ACTIVITY frames.
    #[test]
    fn gate_fake_pipe_sequence() {
        // Reads the live mirror on every drive; the serial keeps a concurrent
        // publisher out of this long run.
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        const IT_TITLE: &str = "Breaking Bad (2008) - S04E07 - Problem Dog (1080p BluRay x265 Silence).mkv - PotPlayer";
        const IT_TOTAL: i64 = 2_832_979;
        const IT_N: i64 = 1_000_000_000_000;

        let srv = discord::FakeServer::new(0);
        let base = srv.base.clone();
        let (accept_tx, accept_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = accept_tx.send(srv.accept().map_err(|e| e.to_string()));
        });

        let dc = discord::Client::new(discord::Config {
            client_id: "123456789012345678".to_string(),
            pipe_base: base,
            retry_delay: Duration::from_millis(30),
            ..Default::default()
        });
        dc.start();

        let sc = match accept_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(sc)) => sc,
            Ok(Err(err)) => panic!("fake server accept: {err}"),
            Err(_) => panic!("client did not connect to the fake pipe"),
        };

        let (op, _) = sc.read_frame().expect("handshake frame");
        assert_eq!(op, discord::OP_HANDSHAKE, "first frame is the handshake");
        sc.write_frame(
            discord::OP_FRAME,
            br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
        )
        .expect("write READY");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while dc.generation() < 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "client never became ready after the READY dispatch"
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        // Collect every post-READY frame the client sends.
        let (frames_tx, frames_rx) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::spawn(move || {
            loop {
                match sc.read_frame() {
                    Ok((discord::OP_FRAME, payload)) => {
                        if frames_tx.send(payload).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });

        let dir = unique_dir("gate");
        let log_path = dir.join("gate-integration.log");
        let log = Logger::new(&log_path, 0, 1);

        let mut tracker = Tracker::new();
        let mut gate = Gate::new();
        {
            let mut drive = |status: PlayStatus, ct: i64, now: i64| {
                let action = tracker.poll(IT_TITLE, status, ct, now);
                let debounced = tracker.pending_debounce();
                if let Some(cur) = action.clone().or_else(|| tracker.current()) {
                    let mut record =
                        |kind, raw: String, act: Option<presence::Action>, res: presence::Resolved| {
                            window::ui().record_preview(kind, raw, act, res);
                        };
                    push_activity(
                        &log,
                        &dc,
                        &mut gate,
                        live_card_specs(),
                        &mut record,
                        IT_TITLE,
                        Some(&cur),
                        ct,
                        IT_TOTAL,
                        now,
                        action.is_some(),
                        debounced,
                    );
                }
            };

            const R: PlayStatus = PlayStatus::RUNNING;
            const P: PlayStatus = PlayStatus::PAUSED;

            drive(R, 1000, IT_N); // 0 first card -> sent
            drive(R, 1250, IT_N + 250); // 1 steady -> suppressed heartbeat-not-due
            drive(R, 2800, IT_N + 500); // 2 small seek inside the window -> held
            drive(R, 2800, IT_N + 750); // 3 still held -> suppressed debounce
            drive(R, 2800, IT_N + 1000); // 4 trailing edge lands -> sent
            drive(R, 3050, IT_N + 1250); // 5 steady
            drive(R, 9000, IT_N + 1500); // 6 big seek inside window -> divergence sent
            drive(R, 9250, IT_N + 1750); // 7 steady
            drive(R, 15000, IT_N + 2000); // 8 seek after window -> tracker event sent
            drive(R, 15250, IT_N + 2250); // 9 steady
            drive(P, 15250, IT_N + 2500); // 10 flap -> still running
            drive(P, 15250, IT_N + 2750); // 11 real pause -> parked paused card
            drive(P, 15250, IT_N + 3000); // 12 paused steady
            // T2-B: the resume waits past the 5-per-20 s window (drives 0-11
            // already sent 5), so the burst never trips the rate guard here;
            // the guard's own burst test lives in `gate_test`.
            drive(R, 15250, IT_N + 21000); // 13 resume -> sent
            for i in 1..=120i64 {
                drive(R, 15250 + 250 * i, IT_N + 21000 + 250 * i);
            }
        }

        // Positive control: force one push through the real path.
        gate.force_next();
        let cur = tracker.current();
        {
            let mut record =
                |kind, raw: String, act: Option<presence::Action>, res: presence::Resolved| {
                    window::ui().record_preview(kind, raw, act, res);
                };
            push_activity(
                &log,
                &dc,
                &mut gate,
                live_card_specs(),
                &mut record,
                IT_TITLE,
                cur.as_ref(),
                15250 + 250 * 120,
                IT_TOTAL,
                IT_N + 21000 + 250 * 120,
                false,
                false,
            );
        }

        std::thread::sleep(Duration::from_millis(200));

        #[derive(PartialEq, Debug, Clone, Default)]
        struct Sent {
            clear: bool,
            name: String,
            details: String,
            state: String,
            has_start: bool,
            start: i64,
            has_end: bool,
            end: i64,
        }

        let mut got: Vec<Sent> = Vec::new();
        let mut raws: Vec<String> = Vec::new();
        while let Ok(payload) = frames_rx.try_recv() {
            raws.push(String::from_utf8_lossy(&payload).into_owned());
            let wire: serde_json::Value = serde_json::from_slice(&payload).expect("decode frame");
            assert_eq!(wire["cmd"], "SET_ACTIVITY", "unexpected frame");
            if wire["args"]["activity"].is_null() {
                got.push(Sent {
                    clear: true,
                    ..Sent::default()
                });
                continue;
            }
            let activity = &wire["args"]["activity"];
            let mut sent = Sent {
                name: activity["name"].as_str().unwrap_or("").to_string(),
                details: activity["details"].as_str().unwrap_or("").to_string(),
                state: activity["state"].as_str().unwrap_or("").to_string(),
                ..Sent::default()
            };
            if let Some(start) = activity["timestamps"]["start"].as_i64() {
                sent.has_start = true;
                sent.start = start;
            }
            if let Some(end) = activity["timestamps"]["end"].as_i64() {
                sent.has_end = true;
                sent.end = end;
            }
            got.push(sent);
        }

        const START0: i64 = IT_N - 1000;
        const START1: i64 = IT_N - 1800;
        const START2: i64 = IT_N - 7500;
        const START3: i64 = IT_N - 13000;
        const START4: i64 = IT_N + 5750;
        const PAUSE_START: i64 = IT_N + 2750;
        const END0: i64 = IT_N + (IT_TOTAL - 1000);
        const END1: i64 = IT_N + 1000 + (IT_TOTAL - 2800);
        const END2: i64 = IT_N + 1500 + (IT_TOTAL - 9000);
        const END3: i64 = IT_N + 2000 + (IT_TOTAL - 15000);
        const END5: i64 = IT_N + 21000 + (IT_TOTAL - 15250);
        const END6: i64 = IT_N + 51000 + (IT_TOTAL - 45250);

        // The header carries the media title, so a series' line 1 blanks.
        let playing = |title: &str, state: &str, start: i64, end: i64| Sent {
            clear: false,
            name: title.to_string(),
            details: String::new(),
            state: state.to_string(),
            has_start: true,
            start,
            has_end: true,
            end,
        };
        let want = [
            playing("Breaking Bad", "S4 E7 \u{b7} Problem Dog", START0, END0),
            playing("Breaking Bad", "S4 E7 \u{b7} Problem Dog", START1, END1),
            playing("Breaking Bad", "S4 E7 \u{b7} Problem Dog", START2, END2),
            playing("Breaking Bad", "S4 E7 \u{b7} Problem Dog", START3, END3),
            Sent {
                clear: false,
                name: "Breaking Bad".to_string(),
                details: String::new(),
                state: "S4 E7 \u{b7} Problem Dog - Paused".to_string(),
                has_start: true,
                start: PAUSE_START,
                has_end: false,
                end: 0,
            },
            playing("Breaking Bad", "S4 E7 \u{b7} Problem Dog", START4, END5),
            playing("Breaking Bad", "S4 E7 \u{b7} Problem Dog", START4, END6),
            playing("Breaking Bad", "S4 E7 \u{b7} Problem Dog", START4, END6),
        ];

        assert_eq!(
            got.len(),
            want.len(),
            "sent {} frames, want {}\n got: {got:?}",
            got.len(),
            want.len()
        );
        for (i, wanted) in want.iter().enumerate() {
            assert_eq!(&got[i], wanted, "frame {i} raw: {}", raws[i]);
        }

        // R1: playing frames carry BOTH timestamps; the paused frame carries
        // start only, never end.
        for (i, sent) in got.iter().enumerate() {
            if i == 4 {
                assert!(!sent.has_end, "paused frame {i} carries end: {}", raws[i]);
                continue;
            }
            assert!(
                sent.has_start && sent.has_end,
                "playing frame {i} must carry both timestamps: {}",
                raws[i]
            );
        }
        // R2: the pause frame is a parked paused card, and NO frame is a clear.
        assert!(
            !got[4].clear
                && got[4].name == "Breaking Bad"
                && got[4].state == "S4 E7 \u{b7} Problem Dog - Paused",
            "frame 4 is not the parked paused card: {got:?}"
        );
        for (i, sent) in got.iter().enumerate() {
            assert!(
                !sent.clear,
                "frame {i} is an activity:null clear, want a card"
            );
        }
        for (i, raw) in raws.iter().enumerate() {
            assert!(
                !raw.contains("00:15"),
                "frame {i} looks like a frozen-position paused card: {raw}"
            );
        }
        const RESUME_NOW: i64 = IT_N + 21000;
        const RESUME_CT: i64 = 15250;
        assert_eq!(
            got[5].start,
            RESUME_NOW - RESUME_CT,
            "resume start must re-anchor to now-ct"
        );
        const SEEK_NOW: i64 = IT_N + 2000;
        const SEEK_CT: i64 = 15000;
        assert_eq!(
            got[3].start,
            SEEK_NOW - SEEK_CT,
            "post-seek start must re-anchor to now-ct"
        );

        // The observation boundary: every poll logged an attempt, suppressed
        // polls included, with the reason.
        let log_text = std::fs::read_to_string(&log_path).expect("read log");
        const TOTAL_POLLS: usize = 14 + 120 + 1;
        assert_eq!(
            log_text.matches("gate=sent").count(),
            want.len(),
            "log gate=sent lines (every frame, the parked pause included)"
        );
        assert_eq!(log_text.matches("gate=clear").count(), 0, "no clear lines");
        assert_eq!(
            log_text.matches("gate=suppressed").count(),
            TOTAL_POLLS - want.len(),
            "gate=suppressed lines"
        );
        assert!(
            log_text.contains("reason=debounce"),
            "missing reason=debounce"
        );
        assert!(
            log_text.contains("reason=heartbeat-not-due"),
            "missing reason=heartbeat-not-due"
        );
        assert!(log_text.contains("reason=paused"), "missing reason=paused");

        // F1: the logged payload must agree with the frame on the wire.
        let mut logged_ends: Vec<(bool, i64)> = Vec::new();
        for line in log_text.split('\n') {
            if !line.contains("gate=sent") {
                continue;
            }
            let at = line.find("payload=").expect("sent line carries payload=");
            let decoded: serde_json::Value =
                serde_json::from_str(line[at + "payload=".len()..].trim())
                    .unwrap_or_else(|e| panic!("decode logged payload {line:?}: {e}"));
            match decoded["timestamps"]["end"].as_i64() {
                Some(end) => logged_ends.push((true, end)),
                None => logged_ends.push((false, 0)),
            }
        }
        assert_eq!(
            logged_ends.len(),
            got.len(),
            "logged sent payloads (one per frame)"
        );
        for (i, sent) in got.iter().enumerate() {
            assert!(
                logged_ends[i].0 == sent.has_end && (!sent.has_end || logged_ends[i].1 == sent.end),
                "frame {i}: logged end must match the wire"
            );
        }

        let _ = dc.close();
        let _ = reader.join();
        drop(log);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `m3_reply_test.go` -> `TestPushReplyLoggedVerbatim`: proves the reply path
    /// end to end over the in-process fake pipe.
    #[test]
    fn push_reply_logged_verbatim() {
        fn wait_log_contains(path: &std::path::Path, want: &str, within: Duration) {
            let deadline = std::time::Instant::now() + within;
            loop {
                let text = std::fs::read_to_string(path).unwrap_or_default();
                if text.contains(want) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "log {} missing {want:?}; contents:\n{text}",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn frame_nonce(payload: &[u8]) -> String {
            let value: serde_json::Value = serde_json::from_slice(payload).expect("decode frame");
            let nonce = value["nonce"].as_str().unwrap_or("").to_string();
            assert!(!nonce.is_empty(), "frame carries no nonce");
            nonce
        }

        let dir = unique_dir("m3-reply");
        let log_path = dir.join("potflix-m3-reply.log");
        // The client callbacks must be `'static`; the log is leaked so the ONE
        // handle on the file is shared by every callback (never a second handle).
        let log: &'static Logger = Box::leak(Box::new(Logger::new(&log_path, 1 << 20, 2)));

        let srv = discord::FakeServer::new(0);
        let base = srv.base.clone();
        const CLIENT_ID: &str = "123456789012345678";
        let (accept_tx, accept_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = accept_tx.send(srv.accept().map_err(|e| e.to_string()));
        });

        let c = discord::Client::new(discord::Config {
            client_id: CLIENT_ID.to_string(),
            pipe_base: base,
            retry_delay: Duration::from_millis(30),
            on_connect: Some(Box::new(move || selftest::event_connected(log))),
            on_reply: Some(Box::new(move |reply: discord::Reply| {
                if reply.error {
                    selftest::event_push_error(
                        log,
                        &reply.nonce,
                        &reply.evt,
                        reply.code,
                        &reply.message,
                    );
                } else {
                    selftest::event_push_ack(log, &reply.nonce);
                }
            })),
            ..Default::default()
        });
        c.start();

        let sc = match accept_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(sc)) => sc,
            Ok(Err(err)) => panic!("fake server accept: {err}"),
            Err(_) => panic!("client did not connect to the fake pipe"),
        };

        let (op, _) = sc.read_frame().expect("handshake frame");
        assert_eq!(op, discord::OP_HANDSHAKE);

        let act = discord::Activity {
            details: "Show".to_string(),
            state: r#"S1 E2 "Pilot""#.to_string(),
            small_image: "play".to_string(),
            has_start: true,
            start: 12345,
            ..Default::default()
        };

        // Before Discord's READY dispatch a command is refused, not written.
        match c.set_activity(act.clone()) {
            Err(discord::Error::NotReady) => {}
            other => panic!("pre-READY SetActivity = {other:?}, want NotReady"),
        }

        sc.write_frame(
            discord::OP_FRAME,
            br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
        )
        .expect("write READY");
        wait_log_contains(
            &log_path,
            "connected clientId=<redacted>",
            Duration::from_secs(3),
        );

        // (1) a clean reply logs `push ack nonce=<n>`.
        c.set_activity(act.clone()).expect("SetActivity");
        let (op, frame) = sc.read_frame().expect("read SET_ACTIVITY");
        assert_eq!(op, discord::OP_FRAME);
        let nonce1 = frame_nonce(&frame);
        let ack = format!(r#"{{"cmd":"SET_ACTIVITY","evt":null,"nonce":"{nonce1}","data":null}}"#);
        sc.write_frame(discord::OP_FRAME, ack.as_bytes())
            .expect("write ack");
        wait_log_contains(
            &log_path,
            &format!("push ack nonce={nonce1}"),
            Duration::from_secs(3),
        );

        // (2) Discord's ERROR frame is logged verbatim, client id redacted.
        c.set_activity(act).expect("SetActivity 2");
        let (op, frame) = sc.read_frame().expect("read SET_ACTIVITY 2");
        assert_eq!(op, discord::OP_FRAME);
        let nonce2 = frame_nonce(&frame);
        let raw = format!("Invalid activity for client {CLIENT_ID}: bad assets");
        let err_frame = format!(
            r#"{{"cmd":"SET_ACTIVITY","evt":"ERROR","nonce":"{nonce2}","data":{{"code":4003,"message":"{raw}"}}}}"#
        );
        sc.write_frame(discord::OP_FRAME, err_frame.as_bytes())
            .expect("write error frame");
        let want = format!(
            "push error nonce={nonce2} evt=ERROR code=4003 message=Invalid activity for client <redacted>: bad assets"
        );
        wait_log_contains(&log_path, &want, Duration::from_secs(3));

        let _ = c.close();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cardview test, repointed to the APP's `pushActivity`
    /// (`go\main.go:332-394`): drives the ONE production implementation over a
    /// fake pipe and asserts the preview composes to the payload the send path
    /// produced, under the stored per-kind Card view specs (`specs` param =
    /// `liveCardSpecs`).
    #[test]
    fn preview_is_payload_through_send() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let srv = discord::FakeServer::new(0);
        let base = srv.base.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(srv.accept());
        });
        let dc = discord::Client::new(discord::Config {
            client_id: "123456789012345678".into(),
            pipe_base: base,
            retry_delay: Duration::from_millis(30),
            ..Default::default()
        });
        dc.start();
        let sc = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("client connects")
            .expect("accept");
        let (op, _) = sc.read_frame().expect("handshake frame");
        assert_eq!(op, discord::OP_HANDSHAKE);
        sc.write_frame(
            discord::OP_FRAME,
            br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while dc.generation() < 1 {
            assert!(Instant::now() < deadline, "never became ready");
            std::thread::sleep(Duration::from_millis(2));
        }
        let (ftx, frx) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::spawn(move || {
            loop {
                match sc.read_frame() {
                    Ok((discord::OP_FRAME, payload)) => {
                        if ftx.send(payload).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });

        let dir = unique_dir("preview");
        let log = Logger::new(dir.join("preview.log"), 0, 1);
        let now = 1_000_000_000_000i64;
        let drive = |specs: config::CardSpecs,
                     raw: &str,
                     act: &presence::Action,
                     ct: i64|
         -> (presence::Activity, serde_json::Value, Vec<u8>) {
            let mut st = window::UiState::default();
            st.card_specs_state = specs.clone();
            let mut gate = Gate::new();
            let mut record =
                |kind, raw: String, a: Option<presence::Action>, res: presence::Resolved| {
                    st.record_preview(kind, raw, a, res);
                };
            push_activity(
                &log,
                &dc,
                &mut gate,
                specs,
                &mut record,
                raw,
                Some(act),
                ct,
                2_832_979,
                now,
                true,
                false,
            );
            drop(record);
            let payload = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("SET_ACTIVITY frame");
            let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(v["cmd"], "SET_ACTIVITY");
            assert!(!v["args"]["activity"].is_null());
            // The push records exactly one kind; the preview for that kind must
            // compose to the payload. Probe both kinds: exactly one is present.
            let mut matched: Option<presence::Activity> = None;
            for kind in [config::CardKind::Series, config::CardKind::Movie] {
                st.set_card_kind(kind);
                let (got, have) = st.preview_activity();
                if have {
                    assert!(
                        matched.is_none(),
                        "both kinds recorded for {raw:?}, want exactly one"
                    );
                    matched = Some(got);
                }
            }
            let got = matched.expect("preview not recorded after a successful send");
            (got, v, payload)
        };
        let check = |got: &presence::Activity, v: &serde_json::Value| {
            let a = &v["args"]["activity"];
            assert_eq!(got.name, a["name"].as_str().unwrap_or(""));
            assert_eq!(got.details, a["details"].as_str().unwrap_or(""));
            assert_eq!(got.state, a["state"].as_str().unwrap_or(""));
            let assets = &a["assets"];
            // A resolved http(s) artwork rides; otherwise the image is omitted
            // (never the branded logo), so the whole assets key may be absent.
            let want_large = if got.large_image.starts_with("http") {
                got.large_image.as_str()
            } else {
                ""
            };
            assert_eq!(assets["large_image"].as_str().unwrap_or(""), want_large);
            assert_eq!(assets["large_text"].as_str().unwrap_or(""), got.large_text);
            assert!(
                assets["small_image"].as_str().unwrap_or("").is_empty()
                    && assets["small_text"].as_str().unwrap_or("").is_empty(),
                "no icon rides the preview/wire: {assets}"
            );
            // The preview composes through the same composer (name, lines,
            // assets, start anchor) but carries no gate projection: `end` is
            // computed by the send gate from ct/total, not from the recorded
            // inputs, so the preview's `end` stays 0 while a running wire
            // frame carries the projection.
            if got.has_start {
                assert_eq!(a["timestamps"]["start"].as_i64(), Some(got.start));
            }
            assert_eq!(got.end, 0, "preview carries no gate-projected end");
            assert!(
                a["timestamps"]["end"].as_i64().is_some(),
                "running wire frame must carry the gate-projected end"
            );
            assert_eq!(
                a["buttons"].as_array().map_or(0, Vec::len),
                got.buttons.len()
            );
        };

        let series_raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
        let act = presence::Action {
            running: true,
            has_start: true,
            start: now - 1000,
            ct: 1000,
            ..presence::Action::default()
        };
        let (got, v, _) = drive(config::CardSpecs::default(), series_raw, &act, 1000);
        check(&got, &v);

        let act2 = presence::Action {
            running: true,
            has_start: true,
            start: now - 2000,
            ct: 2000,
            ..presence::Action::default()
        };
        let mut series_specs = config::CardSpecs::default();
        assert!(series_specs.set_line(
            config::CardKind::Series,
            config::CardLine::Details,
            "title"
        ));
        assert!(series_specs.set_line(config::CardKind::Series, config::CardLine::State, "tag"));
        let (got2, v2, _) = drive(series_specs, series_raw, &act2, 2000);
        check(&got2, &v2);
        assert!(
            got2.details.is_empty(),
            "the title preset repeats the header and blanks: {:?}",
            got2.details
        );
        assert_eq!(got2.state, "S1 E2", "the tag preset rides the wire");

        let act3 = presence::Action {
            running: true,
            has_start: true,
            start: now - 3000,
            ct: 3000,
            ..presence::Action::default()
        };
        let mut minimal_specs = config::CardSpecs::default();
        assert!(minimal_specs.set_line(
            config::CardKind::Series,
            config::CardLine::State,
            "none"
        ));
        let (got3, v3, frame) = drive(minimal_specs, series_raw, &act3, 3000);
        check(&got3, &v3);
        assert_eq!(got3.state, "");
        let env: serde_json::Value = serde_json::from_slice(&frame).unwrap();
        assert!(
            env["args"]["activity"].get("state").is_none(),
            "empty state reached the wire"
        );
        assert!(
            env["args"]["activity"].get("details").is_none(),
            "empty details reached the wire"
        );

        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The designer boundary: a published field choice must reach the wire AND
    /// the recorded preview through the ONE production push path. Turning every
    /// optional field off leaves only the header (the media title), because the
    /// selected title always drives `Watching <Title>`; no app-name header, no
    /// assets, no timestamps.
    #[test]
    fn designer_field_choices_reach_the_wire_and_the_preview() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        presence::set_live_designer(presence::designer::DesignerSpec::default());
        let srv = discord::FakeServer::new(0);
        let base = srv.base.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(srv.accept());
        });
        let dc = discord::Client::new(discord::Config {
            client_id: "123456789012345678".into(),
            pipe_base: base,
            retry_delay: Duration::from_millis(30),
            ..Default::default()
        });
        dc.start();
        let sc = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("client connects")
            .expect("accept");
        let (op, _) = sc.read_frame().expect("handshake frame");
        assert_eq!(op, discord::OP_HANDSHAKE);
        sc.write_frame(
            discord::OP_FRAME,
            br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while dc.generation() < 1 {
            assert!(Instant::now() < deadline, "never became ready");
            std::thread::sleep(Duration::from_millis(2));
        }
        let (ftx, frx) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::spawn(move || {
            loop {
                match sc.read_frame() {
                    Ok((discord::OP_FRAME, payload)) => {
                        if ftx.send(payload).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });

        let dir = unique_dir("designer-wire");
        let log = Logger::new(dir.join("designer.log"), 0, 1);
        let now = 1_000_000_000_000i64;
        let mut st = window::UiState::default();
        st.record_preview(
            config::CardKind::Movie,
            "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
            None,
            presence::Resolved::default(),
        );
        presence::set_live_designer(presence::designer::DesignerSpec {
            artwork: false,
            details: false,
            state: false,
            synopsis: false,
            timers: false,
        });
        let mut gate = Gate::new();
        let mut record =
            |kind, raw: String, a: Option<presence::Action>, res: presence::Resolved| {
                st.record_preview(kind, raw, a, res);
            };
        let act = presence::Action {
            running: true,
            has_start: true,
            start: now - 1000,
            ct: 1000,
            ..presence::Action::default()
        };
        push_activity(
            &log,
            &dc,
            &mut gate,
            config::CardSpecs::default(),
            &mut record,
            "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
            Some(&act),
            1000,
            2_832_979,
            now,
            true,
            false,
        );
        drop(record);
        let payload = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("SET_ACTIVITY frame");
        let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let a = &v["args"]["activity"];
        let header = a["name"].as_str().unwrap_or("");
        assert!(
            header.contains("Dune Part Two") && !header.is_empty(),
            "the header is the media title, got {header:?}"
        );
        assert!(a.get("details").is_none(), "details toggle reached wire");
        assert!(a.get("state").is_none(), "state toggle reached wire");
        assert!(a.get("assets").is_none(), "artwork toggle reached wire");
        assert!(a.get("timestamps").is_none(), "timer toggle reached wire");
        assert_ne!(a["name"], "Potflix");
        assert_eq!(
            a["name"].as_str().unwrap_or(""),
            st.preview_activity().0.name,
            "the preview and the wire share the header"
        );
        presence::set_live_designer(presence::designer::DesignerSpec::default());
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The per-kind wire test: a movie title composes with the movie spec and a
    /// series title with the series spec, through the ONE production push path
    /// with no resolver (kind from the query).
    #[test]
    fn per_kind_specs_reach_the_wire() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let srv = discord::FakeServer::new(0);
        let base = srv.base.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(srv.accept());
        });
        let dc = discord::Client::new(discord::Config {
            client_id: "123456789012345678".into(),
            pipe_base: base,
            retry_delay: Duration::from_millis(30),
            ..Default::default()
        });
        dc.start();
        let sc = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("client connects")
            .expect("accept");
        let (op, _) = sc.read_frame().expect("handshake frame");
        assert_eq!(op, discord::OP_HANDSHAKE);
        sc.write_frame(
            discord::OP_FRAME,
            br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while dc.generation() < 1 {
            assert!(Instant::now() < deadline, "never became ready");
            std::thread::sleep(Duration::from_millis(2));
        }
        let (ftx, frx) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::spawn(move || {
            loop {
                match sc.read_frame() {
                    Ok((discord::OP_FRAME, payload)) => {
                        if ftx.send(payload).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });

        let dir = unique_dir("per-kind");
        let log = Logger::new(dir.join("per-kind.log"), 0, 1);
        let now = 1_000_000_000_000i64;
        // Series keeps a visible state; movies go minimal. With no resolver the
        // inputs are empty, so `none` (empty) is the only state preset that
        // stays distinct on both ladders.
        let mut specs = config::CardSpecs::default();
        assert!(specs.set_line(config::CardKind::Series, config::CardLine::State, "tag"));
        assert!(specs.set_line(config::CardKind::Movie, config::CardLine::State, "none"));

        let drive_raw = |raw: &str, ct: i64| -> serde_json::Value {
            let mut st = window::UiState::default();
            st.card_specs_state = specs.clone();
            let mut gate = Gate::new();
            let act = presence::Action {
                running: true,
                has_start: true,
                start: now - ct,
                ct,
                ..presence::Action::default()
            };
            let mut record =
                |kind, raw: String, a: Option<presence::Action>, res: presence::Resolved| {
                    st.record_preview(kind, raw, a, res);
                };
            push_activity(
                &log,
                &dc,
                &mut gate,
                specs.clone(),
                &mut record,
                raw,
                Some(&act),
                ct,
                2_832_979,
                now,
                true,
                false,
            );
            drop(record);
            let payload = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("SET_ACTIVITY frame");
            let v: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(v["cmd"], "SET_ACTIVITY");
            // The preview for the recorded kind equals the wire payload.
            let mut matched = false;
            for kind in [config::CardKind::Series, config::CardKind::Movie] {
                st.set_card_kind(kind);
                let (got, have) = st.preview_activity();
                if have {
                    assert!(!matched, "both kinds recorded for {raw:?}");
                    assert_eq!(
                        got.details,
                        v["args"]["activity"]["details"].as_str().unwrap_or("")
                    );
                    if got.state.is_empty() {
                        assert!(
                            v["args"]["activity"].get("state").is_none(),
                            "empty state reached the wire for {raw:?}"
                        );
                    } else {
                        assert_eq!(
                            got.state,
                            v["args"]["activity"]["state"].as_str().unwrap_or("")
                        );
                    }
                    matched = true;
                }
            }
            assert!(matched, "no preview recorded for {raw:?}");
            v
        };

        let series_raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
        let movie_raw = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        // Positive control: the query kinds really are series vs movie here.
        assert_eq!(
            meta_query_for(series_raw).kind,
            metadata::Kind::SERIES,
            "series fixture query kind"
        );
        assert_eq!(
            meta_query_for(movie_raw).kind,
            metadata::Kind::MOVIE,
            "movie fixture query kind"
        );

        let v_series = drive_raw(series_raw, 1000);
        assert!(
            v_series["args"]["activity"].get("state").is_some(),
            "series title must carry the series (tag) state, got {v_series}"
        );
        let v_movie = drive_raw(movie_raw, 2000);
        assert!(
            v_movie["args"]["activity"].get("state").is_none(),
            "movie title must carry the movie (none) state, got {v_movie}"
        );

        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `window.rs` -> the failure-side cardview test, repointed to the APP's
    /// `pushActivity`: a failed send records no preview.
    #[test]
    fn preview_not_recorded_on_send_failure() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let dir = unique_dir("preview-fail");
        let dc = discord::Client::new(discord::Config {
            client_id: "123456789012345678".into(),
            pipe_base: format!("{}-no-pipe-", dir.display()),
            retry_delay: Duration::from_secs(3600),
            ..Default::default()
        });
        let log = Logger::new(dir.join("preview-fail.log"), 0, 1);
        let mut st = window::UiState::default();
        let mut gate = Gate::new();
        let act = presence::Action {
            running: true,
            has_start: true,
            start: 999,
            ct: 1000,
            ..presence::Action::default()
        };
        let mut record =
            |kind, raw: String, a: Option<presence::Action>, res: presence::Resolved| {
                st.record_preview(kind, raw, a, res);
            };
        push_activity(
            &log,
            &dc,
            &mut gate,
            config::CardSpecs::default(),
            &mut record,
            "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
            Some(&act),
            1000,
            2_832_979,
            1_000_000_000_000,
            true,
            false,
        );
        drop(record);
        assert!(
            !st.preview_activity().1,
            "preview recorded after a failed send"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2-B: a single correction sends immediately under the 5-per-20 s window
    /// (it never waits for the 30 s heartbeat) and records the preview.
    #[test]
    fn rate_single_correction_sends_and_previews() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (dc, frx, reader, dir) = live_pipe_harness("rate-single");
        let log = Logger::new(dir.join("rate-single.log"), 0, 1);
        const TOTAL: i64 = 2_832_979;
        const NOW: i64 = 1_000_000_000_000;
        let mut gate = Gate::new();
        let raw_a = "Rate.Show.S01E01.First.1080p.mkv - PotPlayer";
        let raw_b = "Rate.Show.S01E02.Second.1080p.mkv - PotPlayer";
        let act = presence::Action {
            running: true,
            has_start: true,
            start: NOW - 1000,
            ct: 1000,
            ..presence::Action::default()
        };
        let previews = std::cell::Cell::new(0);
        let mut record = |_: config::CardKind,
                          _: String,
                          _: Option<presence::Action>,
                          _: presence::Resolved| {
            previews.set(previews.get() + 1);
        };
        push_activity(&log, &dc, &mut gate, config::CardSpecs::default(), &mut record, raw_a, Some(&act), 1000, TOTAL, NOW, true, false);
        let first = frx.recv_timeout(Duration::from_secs(2)).expect("first card");
        let (d1, s1) = wire_lines(&first);
        // One poll later the corrected title sends at once, not at heartbeat.
        let act2 = presence::Action {
            running: true,
            has_start: true,
            start: NOW + 250 - 1250,
            ct: 1250,
            ..presence::Action::default()
        };
        push_activity(&log, &dc, &mut gate, config::CardSpecs::default(), &mut record, raw_b, Some(&act2), 1250, TOTAL, NOW + 250, true, false);
        let second = frx.recv_timeout(Duration::from_secs(2)).expect("correction must send immediately");
        let (d2, s2) = wire_lines(&second);
        assert_ne!(
            (d1, s1),
            (d2, s2),
            "the wire must carry the corrected lines"
        );
        assert_eq!(previews.get(), 2, "preview records after each successful send");
        assert!(frx.recv_timeout(Duration::from_millis(250)).is_err(), "no duplicate frame");
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2-B: 6 line changes inside 20 s send 5 and coalesce the latest; the
    /// newest flushes on the first poll after the window with no duplicate and
    /// no preview for the held poll.
    #[test]
    fn rate_burst_coalesces_latest_through_push() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (dc, frx, reader, dir) = live_pipe_harness("rate-burst");
        let log = Logger::new(dir.join("rate-burst.log"), 0, 1);
        const TOTAL: i64 = 2_832_979;
        const NOW: i64 = 1_000_000_000_000;
        let mut gate = Gate::new();
        let previews = std::cell::Cell::new(0);
        let mut record = |_: config::CardKind,
                          _: String,
                          _: Option<presence::Action>,
                          _: presence::Resolved| {
            previews.set(previews.get() + 1);
        };
        let raws = [
            "Rate.Burst.S01E01.A.1080p.mkv - PotPlayer",
            "Rate.Burst.S01E02.B.1080p.mkv - PotPlayer",
            "Rate.Burst.S01E03.C.1080p.mkv - PotPlayer",
            "Rate.Burst.S01E04.D.1080p.mkv - PotPlayer",
            "Rate.Burst.S01E05.E.1080p.mkv - PotPlayer",
            "Rate.Burst.S01E06.F.1080p.mkv - PotPlayer",
        ];
        for (i, raw) in raws.iter().enumerate() {
            let ct = 1000 + 250 * i as i64;
            let now = NOW + 250 * i as i64;
            let act = presence::Action {
                running: true,
                has_start: true,
                start: now - ct,
                ct,
                ..presence::Action::default()
            };
            push_activity(&log, &dc, &mut gate, config::CardSpecs::default(), &mut record, raw, Some(&act), ct, TOTAL, now, true, false);
        }
        // 5 frames go out; the 6th change is held (coalesced, no preview).
        let mut got_details = Vec::new();
        for _ in 0..5 {
            let frame = frx.recv_timeout(Duration::from_secs(2)).expect("burst frame");
            got_details.push(wire_lines(&frame).0);
        }
        assert!(frx.recv_timeout(Duration::from_millis(250)).is_err(), "6th change must hold, not send a 6th frame");
        assert_eq!(previews.get(), 5, "held poll records no preview");
        // First poll after the window flushes the newest (F); intermediates
        // never go out twice and no identical duplicate follows.
        let flush_now = NOW + 20_000 + 250;
        let flush_ct = 1000 + 250 * 5 + 20_000;
        let flush_act = presence::Action {
            running: true,
            has_start: true,
            start: flush_now - flush_ct,
            ct: flush_ct,
            ..presence::Action::default()
        };
        push_activity(&log, &dc, &mut gate, config::CardSpecs::default(), &mut record, raws[5], Some(&flush_act), flush_ct, TOTAL, flush_now, false, false);
        let flushed = frx.recv_timeout(Duration::from_secs(2)).expect("newest must flush after the window");
        let (fd, _) = wire_lines(&flushed);
        let (want_d, _) = expect_lines(raws[5], &flush_act, &config::CardSpecs::default());
        assert_eq!(fd, want_d, "flush carries the newest lines");
        assert_eq!(previews.get(), 6, "flush records the preview");
        assert!(!got_details.contains(&fd) || got_details.len() == 5, "flush is the coalesced newest");
        assert!(frx.recv_timeout(Duration::from_millis(250)).is_err(), "no duplicate after flush");
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2-B: stop grace still clears and the resume still sends through the
    /// real poll body while the rate guard is active.
    #[test]
    fn rate_stop_resume_still_works() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (dc, frx, reader, dir) = live_pipe_harness("rate-stop");
        let log = Logger::new(dir.join("rate-stop.log"), 0, 1);
        const TOTAL: i64 = 2_832_979;
        const NOW: i64 = 1_000_000_000_000;
        const RAW: &str = "Rate.Stop.S01E02.Pilot.1080p.mkv - PotPlayer";
        let mut tracker = Tracker::new();
        let mut gate = Gate::new();
        let mut stop_since = 0i64;
        let mut cleared_gen = -1i64;
        let previews = std::cell::Cell::new(0);
        let mut drive = |status: PlayStatus, ct: i64, now: i64| {
            let action = tracker.poll(RAW, status, ct, now);
            let cur = action.clone().or_else(|| tracker.current());
            let stopped = cur.as_ref().is_some_and(|a| !a.running && !a.paused);
            let (since, due) = sustained_stop(now, stop_since, stopped);
            stop_since = since;
            if due {
                if clear_due(dc.generation(), cleared_gen) {
                    dc.clear_activity().expect("clear after the sustained stop");
                    cleared_gen = dc.generation();
                }
                return;
            }
            if let Some(cur) = cur {
                let mut record = |_: config::CardKind,
                                  _: String,
                                  _: Option<presence::Action>,
                                  _: presence::Resolved| {
                    previews.set(previews.get() + 1);
                };
                push_activity(&log, &dc, &mut gate, live_card_specs(), &mut record, RAW, Some(&cur), ct, TOTAL, now, action.is_some(), tracker.pending_debounce());
            }
        };
        drive(PlayStatus::RUNNING, 1000, NOW);
        drive(PlayStatus::RUNNING, 1250, NOW + 250);
        drive(PlayStatus::STOPPED, 1250, NOW + 500);
        drive(PlayStatus::STOPPED, 1250, NOW + 750);
        let _ = frx.recv_timeout(Duration::from_secs(2)).expect("running card");
        let _ = frx.recv_timeout(Duration::from_secs(2)).expect("stop card");
        drive(PlayStatus::STOPPED, 1250, NOW + 16_000);
        let cleared: serde_json::Value = serde_json::from_slice(&frx.recv_timeout(Duration::from_secs(2)).expect("clear")).expect("decode clear");
        assert!(cleared["args"]["activity"].is_null(), "sustained stop clears");
        drive(PlayStatus::RUNNING, 1300, NOW + 16_250);
        let resume = frx.recv_timeout(Duration::from_secs(2)).expect("resume sends under the rate guard");
        let resume: serde_json::Value = serde_json::from_slice(&resume).expect("decode resume");
        assert_eq!(resume["args"]["activity"]["timestamps"]["start"].as_i64(), Some(NOW + 16_250 - 1300));
        assert!(previews.get() >= 3, "running, stop and resume previews recorded, got {}", previews.get());
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real push path over an in-process fake pipe: the caller drives
    /// `push_activity` and reads the exact SET_ACTIVITY frames. Returns the
    /// client, the frame channel, the reader thread, and the scratch dir the
    /// log lives in; the caller closes, joins, and removes them.
    fn live_pipe_harness(
        tag: &str,
    ) -> (
        discord::Client,
        mpsc::Receiver<Vec<u8>>,
        std::thread::JoinHandle<()>,
        std::path::PathBuf,
    ) {
        let srv = discord::FakeServer::new(0);
        let base = srv.base.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(srv.accept());
        });
        let dc = discord::Client::new(discord::Config {
            client_id: "123456789012345678".into(),
            pipe_base: base,
            retry_delay: Duration::from_millis(30),
            ..Default::default()
        });
        dc.start();
        let sc = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("client connects")
            .expect("accept");
        let (op, _) = sc.read_frame().expect("handshake frame");
        assert_eq!(op, discord::OP_HANDSHAKE);
        sc.write_frame(
            discord::OP_FRAME,
            br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while dc.generation() < 1 {
            assert!(Instant::now() < deadline, "never became ready");
            std::thread::sleep(Duration::from_millis(2));
        }
        let (ftx, frx) = mpsc::channel::<Vec<u8>>();
        let reader = std::thread::spawn(move || {
            loop {
                match sc.read_frame() {
                    Ok((discord::OP_FRAME, payload)) => {
                        if ftx.send(payload).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });
        let dir = unique_dir(tag);
        (dc, frx, reader, dir)
    }

    /// The body lines of one SET_ACTIVITY frame: details plus the state when
    /// the wire carries it (an empty state is omitted, never sent blank).
    fn wire_lines(payload: &[u8]) -> (String, Option<String>) {
        let v: serde_json::Value = serde_json::from_slice(payload).expect("decode frame");
        assert_eq!(v["cmd"], "SET_ACTIVITY");
        let a = &v["args"]["activity"];
        (
            a["details"].as_str().unwrap_or("").to_string(),
            a.get("state")
                .and_then(|s| s.as_str())
                .map(str::to_string),
        )
    }

    /// What `push_activity` must compose for these inputs under these specs (no
    /// resolver in tests, so the resolved inputs are empty and the kind follows
    /// the query).
    fn expect_lines(
        raw: &str,
        act: &presence::Action,
        specs: &config::CardSpecs,
    ) -> (String, String) {
        let is_movie = meta_query_for(raw).kind == metadata::Kind::MOVIE;
        let kind = if is_movie {
            config::CardKind::Movie
        } else {
            config::CardKind::Series
        };
        let card = presence::compose_card(
            raw,
            Some(act),
            &presence::Resolved::default(),
            is_movie,
            &specs.for_kind(kind),
        );
        (card.details, card.state)
    }

    /// The Card-view-preset regression test on the real path: push once under
    /// the published specs, publish a different spec as a save would, and the
    /// NEXT poll with identical inputs sends the new lines -- no restart, no
    /// force flag, no re-push. Series and movie variants (the specs are per
    /// kind).
    #[test]
    fn live_spec_change_reaches_next_poll_series_and_movie() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        let (dc, frx, reader, dir) = live_pipe_harness("live-specs");
        let log = Logger::new(dir.join("live-specs.log"), 0, 1);
        const TOTAL: i64 = 2_832_979;
        const NOW: i64 = 1_000_000_000_000;
        let mut noop = |_: config::CardKind,
                        _: String,
                        _: Option<presence::Action>,
                        _: presence::Resolved| {};
        let mut gate = Gate::new();
        let series_raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
        let act = presence::Action {
            running: true,
            has_start: true,
            start: NOW - 1000,
            ct: 1000,
            ..presence::Action::default()
        };

        // Push once under the startup (default) specs, read through the app's
        // live reader exactly as the poll loop does.
        push_activity(
            &log,
            &dc,
            &mut gate,
            live_card_specs(),
            &mut noop,
            series_raw,
            Some(&act),
            1000,
            TOTAL,
            NOW,
            true,
            false,
        );
        let first = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("first SET_ACTIVITY");
        let (d1, s1) = wire_lines(&first);
        let (ed1, es1) = expect_lines(series_raw, &act, &config::CardSpecs::default());
        assert_eq!(d1, ed1);
        assert_eq!(s1.as_deref().unwrap_or(""), es1);

        // A save publishes new specs; the NEXT poll with the same title/ct/total
        // and no event sends the new lines by itself.
        let mut specs_b = config::CardSpecs::default();
        assert!(specs_b.set_line(config::CardKind::Series, config::CardLine::Details, "title"));
        assert!(specs_b.set_line(config::CardKind::Series, config::CardLine::State, "tag"));
        config::set_live_card_specs(specs_b.clone());
        push_activity(
            &log,
            &dc,
            &mut gate,
            live_card_specs(),
            &mut noop,
            series_raw,
            Some(&act),
            1000,
            TOTAL,
            NOW + 250,
            false,
            false,
        );
        let second = frx.recv_timeout(Duration::from_secs(2)).expect(
            "a spec change must send on the next poll with no event",
        );
        let (d2, s2) = wire_lines(&second);
        let (ed2, es2) = expect_lines(series_raw, &act, &specs_b);
        assert_eq!(d2, ed2);
        assert_eq!(s2.as_deref().unwrap_or(""), es2);
        assert_ne!((d1, s1), (d2, s2), "the wire must carry the new lines");

        // Movie variant: same-media spec change, so the send can only come from
        // the published specs (the gate's content-change rule), not from a
        // media change or a forced event. First push a MOVIE baseline and
        // record the card the wire carried.
        let movie_raw = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        let act_m = presence::Action {
            running: true,
            has_start: true,
            start: NOW + 500 - 2000,
            ct: 2000,
            ..presence::Action::default()
        };
        push_activity(
            &log,
            &dc,
            &mut gate,
            live_card_specs(),
            &mut noop,
            movie_raw,
            Some(&act_m),
            2000,
            TOTAL,
            NOW + 500,
            true,
            false,
        );
        let movie_first = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("movie baseline SET_ACTIVITY");
        let (dm1, sm1) = wire_lines(&movie_first);
        let (edm1, esm1) = expect_lines(movie_raw, &act_m, &specs_b);
        assert_eq!(dm1, edm1);
        assert_eq!(sm1.as_deref().unwrap_or(""), esm1);

        // Movie variant: without metadata the default movie card is already
        // title-led, so a published Details "title" preset cannot change the
        // unresolved wire. Prove the mirror directly instead: publish the
        // spec, read it back, and compose a resolved movie -- the title preset
        // drops the year the default would carry.
        let mut specs_c = specs_b.clone();
        assert!(specs_c.set_line(config::CardKind::Movie, config::CardLine::Details, "title"));
        config::set_live_card_specs(specs_c.clone());
        assert_eq!(
            live_card_specs()
                .for_kind(config::CardKind::Movie)
                .details,
            "title",
            "the published movie spec is visible to the composer"
        );
        let resolved_movie = presence::Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi".to_string(),
            length: "2h 46m".to_string(),
            ..presence::Resolved::default()
        };
        let movie_card = presence::compose_card(
            movie_raw,
            Some(&act_m),
            &resolved_movie,
            true,
            &live_card_specs().for_kind(config::CardKind::Movie),
        );
        assert_eq!(
            movie_card.name, "Dune: Part Two",
            "the header carries the movie title"
        );
        assert_eq!(
            movie_card.details, "",
            "the title preset repeats the header and blanks"
        );
        assert_eq!(
            movie_card.state, "Sci-Fi \u{b7} 2h 46m",
            "state survives the details preset"
        );

        config::set_live_card_specs(config::CardSpecs::default());
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pinned fact: YES -- while the card is parked paused, a spec change that
    /// alters the composed lines still goes out on the next poll. `gate.rs`
    /// parks only byte-identical paused content (name/details/state/paused/
    /// images/buttons/has_start), so new lines fall through and re-park.
    #[test]
    fn live_spec_change_reaches_paused_card() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        let (dc, frx, reader, dir) = live_pipe_harness("live-paused");
        let log = Logger::new(dir.join("live-paused.log"), 0, 1);
        const TOTAL: i64 = 2_832_979;
        const NOW: i64 = 1_000_000_000_000;
        let mut noop = |_: config::CardKind,
                        _: String,
                        _: Option<presence::Action>,
                        _: presence::Resolved| {};
        let mut gate = Gate::new();
        let raw = "Show.S01E02.Pilot.1080p.mkv - PotPlayer";
        let paused = presence::Action {
            paused: true,
            ct: 1000,
            ..presence::Action::default()
        };
        push_activity(
            &log,
            &dc,
            &mut gate,
            live_card_specs(),
            &mut noop,
            raw,
            Some(&paused),
            1000,
            TOTAL,
            NOW,
            true,
            false,
        );
        let first = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("paused SET_ACTIVITY");
        let (d1, s1) = wire_lines(&first);
        let parked = s1.as_deref().unwrap_or("");
        assert!(
            parked.ends_with(" - Paused"),
            "first frame is the parked pause card, got {parked:?}"
        );
        // A save publishes the Minimal state preset; the next paused poll with
        // the same title/ct/total and no event still sends the new lines.
        let mut specs = config::CardSpecs::default();
        assert!(specs.set_line(config::CardKind::Series, config::CardLine::State, "none"));
        config::set_live_card_specs(specs.clone());
        push_activity(
            &log,
            &dc,
            &mut gate,
            live_card_specs(),
            &mut noop,
            raw,
            Some(&paused),
            1000,
            TOTAL,
            NOW + 250,
            false,
            false,
        );
        let second = frx.recv_timeout(Duration::from_secs(2)).expect(
            "a paused spec change must still go out with no event",
        );
        let (d2, s2) = wire_lines(&second);
        let (ed2, es2) = expect_lines(raw, &paused, &specs);
        assert_eq!(d2, ed2);
        assert_eq!(s2.as_deref().unwrap_or(""), es2);
        assert_eq!(d2, d1, "details unchanged");
        assert!(s2.is_none(), "paused 'none' state must omit the state line");

        config::set_live_card_specs(config::CardSpecs::default());
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B: the clear anchor follows the client generation, so a reconnect
    /// re-asserts a clear the previous session never delivered.
    #[test]
    fn clear_due_reasserts_after_reconnect() {
        assert!(
            !clear_due(0, 0),
            "the pre-connect generation carries no clear"
        );
        assert!(clear_due(1, 0), "the first connected session clears");
        assert!(!clear_due(1, 1), "already cleared on this session");
        assert!(clear_due(2, 1), "a reconnect re-asserts the clear");
    }

    /// C: the stop grace arms on the first stopped poll, fires only after the
    /// whole grace, and a live poll resets the anchor.
    #[test]
    fn sustained_stop_grace() {
        const NOW: i64 = 1_000_000_000_000;
        assert_eq!(
            sustained_stop(NOW, 0, false),
            (0, false),
            "a live poll carries no anchor"
        );
        assert_eq!(
            sustained_stop(NOW, 0, true),
            (NOW, false),
            "the first stopped poll arms the anchor, not due yet"
        );
        assert_eq!(
            sustained_stop(NOW + 14_999, NOW, true),
            (NOW, false),
            "inside the grace"
        );
        assert_eq!(
            sustained_stop(NOW + 15_000, NOW, true),
            (NOW, true),
            "the grace elapsed"
        );
        assert_eq!(
            sustained_stop(NOW + 60_000, NOW, true),
            (NOW, true),
            "a stop past the grace stays due"
        );
        assert_eq!(
            sustained_stop(NOW + 60_000, NOW, false),
            (0, false),
            "a live poll resets the anchor"
        );
        assert_eq!(
            sustained_stop(NOW + 60_250, 0, true),
            (NOW + 60_250, false),
            "a fresh stop re-arms from scratch"
        );
    }

    /// C's resume requirement, proved on the real path: after a sustained stop
    /// clears the card, playback resuming in the SAME window pushes a live card
    /// again. The stop card carries no anchor and the running card does, so the
    /// gate can never suppress the resume as byte-identical to the stop card.
    #[test]
    fn sustained_stop_then_resume_resends_live_card() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (dc, frx, reader, dir) = live_pipe_harness("stop-resume");
        let log = Logger::new(dir.join("stop-resume.log"), 0, 1);
        const TOTAL: i64 = 2_832_979;
        const NOW: i64 = 1_000_000_000_000;
        const RAW: &str = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        let mut tracker = Tracker::new();
        let mut gate = Gate::new();
        let mut stop_since = 0i64;
        // Nothing is cleared yet on this generation, so the first due clear
        // goes out (a fresh generation is the poll loop's start state too).
        let mut cleared_gen = -1i64;

        {
            // One poll of the loop's decision: tracker -> sustained stop -> push.
            let mut drive = |status: PlayStatus, ct: i64, now: i64| {
                let action = tracker.poll(RAW, status, ct, now);
                let cur = action.clone().or_else(|| tracker.current());
                let stopped = cur.as_ref().is_some_and(|a| !a.running && !a.paused);
                let (since, due) = sustained_stop(now, stop_since, stopped);
                stop_since = since;
                if due {
                    if clear_due(dc.generation(), cleared_gen) {
                        dc.clear_activity().expect("clear after the sustained stop");
                        cleared_gen = dc.generation();
                    }
                    return;
                }
                if let Some(cur) = cur {
                    let mut record =
                        |kind, raw: String, act: Option<presence::Action>, res: presence::Resolved| {
                            window::ui().record_preview(kind, raw, act, res);
                        };
                    push_activity(
                        &log,
                        &dc,
                        &mut gate,
                        live_card_specs(),
                        &mut record,
                        RAW,
                        Some(&cur),
                        ct,
                        TOTAL,
                        now,
                        action.is_some(),
                        tracker.pending_debounce(),
                    );
                }
            };

            // Play, then the position freezes and the Stopped state lands (the
            // tracker's glitch rule keeps reporting Running while it still moves).
            drive(PlayStatus::RUNNING, 1000, NOW);
            drive(PlayStatus::RUNNING, 1250, NOW + 250);
            drive(PlayStatus::STOPPED, 1250, NOW + 500);
            drive(PlayStatus::STOPPED, 1250, NOW + 750);

            let playing = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("frame 1: the running card");
            let playing: serde_json::Value =
                serde_json::from_slice(&playing).expect("decode frame 1");
            assert!(
                playing["args"]["activity"]["timestamps"]["start"]
                    .as_i64()
                    .is_some(),
                "frame 1 must be the running card: {playing}"
            );
            let stop = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("frame 2: the stop card");
            let stop: serde_json::Value = serde_json::from_slice(&stop).expect("decode frame 2");
            assert!(
                stop["args"]["activity"].get("timestamps").is_none(),
                "frame 2 must be the stop card with no anchor: {stop}"
            );

            // The grace elapses with the window still Stopped: the clear goes out
            // and no stop card is pushed again.
            drive(PlayStatus::STOPPED, 1250, NOW + 16_000);
            let cleared = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("frame 3: the clear after the grace");
            let cleared: serde_json::Value =
                serde_json::from_slice(&cleared).expect("decode frame 3");
            assert!(
                cleared["args"]["activity"].is_null(),
                "frame 3 must be the null clear: {cleared}"
            );

            // Playback resumes from the same window: the live card goes out
            // again, re-anchored to the resume.
            drive(PlayStatus::RUNNING, 1300, NOW + 16_250);
            let resume = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("the resume must send a live card");
            let resume: serde_json::Value = serde_json::from_slice(&resume).expect("decode frame 4");
            assert_eq!(
                resume["args"]["activity"]["timestamps"]["start"].as_i64(),
                Some(NOW + 16_250 - 1300),
                "the resume card must re-anchor to now - ct: {resume}"
            );
            assert!(
                frx.recv_timeout(Duration::from_millis(250)).is_err(),
                "exactly four frames go out (running, stop, clear, resume)"
            );
        }

        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The startup catch-up writer must carry a non-empty `app version`, exactly
    /// like the UI scan writer (`window.rs` passes `settings::APP_VERSION`).
    /// The owner's report showed the empty header that `..Default::default()`
    /// produced; this test walks the replacement end to end -- startup config ->
    /// report -> rendered txt -- and both writers must name the same version.
    #[test]
    fn startup_catch_up_render_carries_the_app_version() {
        let c = startup_catch_up_config(&config::Config::default());
        assert_eq!(c.app_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(c.app_version, potflix_win::settings::APP_VERSION);
        let dir = unique_dir("startup-version");
        let r = repair::report::Report {
            app_version: c.app_version.clone(),
            ..Default::default()
        };
        let (_, txt) = repair::report::write_report(&dir, &r).expect("write report");
        let body = std::fs::read_to_string(&txt).expect("read txt");
        assert!(
            body.contains(&format!("app version: {}", env!("CARGO_PKG_VERSION"))),
            "rendered report lost the app version:\n{body}"
        );
        assert!(
            !body.contains("app version: \n") && !body.contains("app version:\n"),
            "rendered report carries an empty app version:\n{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Restores the previous fixture store path on drop.
    struct OverridesFixture(Option<PathBuf>);

    impl OverridesFixture {
        fn install(path: PathBuf) -> OverridesFixture {
            OverridesFixture(set_overrides_path_for_test(Some(path)))
        }
    }

    impl Drop for OverridesFixture {
        fn drop(&mut self) {
            set_overrides_path_for_test(self.0.take());
        }
    }

    /// Seeds the query cache exactly as the poll loop fills it on a title
    /// change (a test binary has no player window), so `push_activity` and the
    /// latch lookup see the fixture's playing file. Returns the query.
    fn seed_playing_file(raw: &str, path: &str) -> metadata::Query {
        let query = metadata_core::context::resolve_query(raw, path);
        *META_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((raw.to_string(), query.clone(), path.to_string()));
        query
    }

    /// Stamps an explicit mtime on the fixture store, so a later rewrite is
    /// always a visible change to the mtime-driven reload.
    fn set_store_mtime(path: &std::path::Path, secs: u64) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open the store to stamp its mtime");
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .expect("stamp the store mtime");
    }

    /// A latched series file renders FROM THE LATCH: the episode name and
    /// synopsis are the stored pin's and the artwork is the latch poster.
    /// Details keeps the pin's episode title alone (the show lives in the
    /// header); the state is the pin's episode code plus the pin's synopsis.
    /// META_RESOLVER is only ever set by `run_ui`, never in a test binary, so
    /// the latch branch is the ONLY possible source of these values -- the
    /// exclusivity is structural.
    #[test]
    fn latched_series_file_renders_the_pin_and_poster() {
        // The fixture store path and the seeded query cache are process-wide:
        // the serial keeps every other cache-touching test out.
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = unique_dir("latch-series");
        let show_dir = root.join("Latch Show");
        let file = show_dir.join("Season 2").join("S02E05.mkv");
        std::fs::create_dir_all(file.parent().expect("episode parent")).expect("media dirs");
        std::fs::write(&file, "x").expect("media file");

        let store_path = root.join("overrides.json");
        let mut store = metadata::overrides::Store::empty_at(store_path.clone());
        store
            .put(metadata::overrides::Override {
                scope: metadata::overrides::Scope::Series,
                key: show_dir.to_string_lossy().into_owned(),
                title: "Latch Show".to_string(),
                year: 2011,
                provider: "tvmaze".to_string(),
                provider_id: "7".to_string(),
                poster: "https://example.com/latch-series.jpg".to_string(),
                media: None,
                episodes: vec![metadata::overrides::EpisodePin {
                    season: 2,
                    number: 5,
                    absolute: 0,
                    title: "The Pin Episode".to_string(),
                    synopsis: "A synopsis from the pin.".to_string(),
                }],
                created_at_ms: 1_700_000_000_000,
            })
            .expect("latch the show folder");
        let _fixture = OverridesFixture::install(store_path);

        let raw = "Latch.Show.S02E05.The.Pin.Episode.1080p.mkv - PotPlayer";
        let query = seed_playing_file(raw, &file.to_string_lossy());
        // Positive control: the fixture really is the pinned episode.
        assert_eq!(query.kind, metadata::Kind::SERIES, "series fixture kind");
        assert_eq!((query.season, query.episode), (2, 5), "pinned episode");
        assert!(
            META_RESOLVER.get().is_none(),
            "a test binary never installs the resolver"
        );

        let (dc, frx, reader, dir) = live_pipe_harness("latch-series");
        let log = Logger::new(dir.join("latch-series.log"), 0, 1);
        const NOW: i64 = 1_000_000_000_000;
        let act = presence::Action {
            running: true,
            has_start: true,
            start: NOW - 1000,
            ct: 1000,
            ..presence::Action::default()
        };
        let mut gate = Gate::new();
        let mut noop = |_: config::CardKind,
                        _: String,
                        _: Option<presence::Action>,
                        _: presence::Resolved| {};
        push_activity(
            &log,
            &dc,
            &mut gate,
            config::CardSpecs::default(),
            &mut noop,
            raw,
            Some(&act),
            1000,
            2_832_979,
            NOW,
            true,
            false,
        );
        let frame = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("a latched series card must go out");
        let v: serde_json::Value = serde_json::from_slice(&frame).expect("decode frame");
        let a = &v["args"]["activity"];
        assert_eq!(
            a["name"].as_str().unwrap_or(""),
            "Latch Show",
            "the latch's series name is the header: {v}"
        );
        assert!(
            a.get("details").is_none() || a["details"].as_str() == Some(""),
            "line 1 blanks: the header carries the series title: {v}"
        );
        assert_eq!(
            a["state"].as_str().unwrap_or(""),
            "S02E05 \u{b7} The Pin Episode",
            "the pin's episode code and title are line 2: {v}"
        );
        assert_eq!(
            a["assets"]["large_text"].as_str().unwrap_or(""),
            "A synopsis from the pin.",
            "the pin's synopsis: {v}"
        );
        assert_eq!(
            a["assets"]["large_image"].as_str().unwrap_or(""),
            "https://example.com/latch-series.jpg",
            "the latch poster: {v}"
        );

        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A latched movie renders FROM its `media` snapshot (the resolver-free
    /// exclusivity is structural, as above). Rewriting the store -- the
    /// snapshot loses its artwork, so the card must fall back to the latch
    /// poster -- only takes effect through the mtime-driven reload, so this one
    /// test pins both.
    #[test]
    fn latched_movie_file_renders_its_media_snapshot() {
        // Same process-wide fixture state as the series test: one serial.
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = unique_dir("latch-movie");
        let file = root.join("Latch Movie (2019).mkv");
        std::fs::write(&file, "x").expect("media file");

        let movie = |artwork: &str| metadata::overrides::Override {
            scope: metadata::overrides::Scope::Movie,
            key: file.to_string_lossy().into_owned(),
            title: "Latch Movie".to_string(),
            year: 2019,
            provider: "cinemeta".to_string(),
            provider_id: "tt1".to_string(),
            poster: "https://example.com/latch-movie.jpg".to_string(),
            media: Some(metadata::Media {
                source: "cinemeta".to_string(),
                source_id: "tt1".to_string(),
                kind: metadata::Kind::MOVIE,
                title: "Latch Movie".to_string(),
                year: 2019,
                artwork: artwork.to_string(),
                synopsis: "The media snapshot's synopsis.".to_string(),
                genres: "Action".to_string(),
                length: "1h 40m".to_string(),
                ..metadata::Media::default()
            }),
            episodes: Vec::new(),
            created_at_ms: 1_700_000_000_001,
        };

        let store_path = root.join("overrides.json");
        let mut store = metadata::overrides::Store::empty_at(store_path.clone());
        store
            .put(movie("https://example.com/latch-media.jpg"))
            .expect("latch the movie");
        set_store_mtime(&store_path, 1_000_000);
        let _fixture = OverridesFixture::install(store_path.clone());

        let raw = "Latch.Movie.2019.1080p.mkv - PotPlayer";
        let query = seed_playing_file(raw, &file.to_string_lossy());
        assert_eq!(query.kind, metadata::Kind::MOVIE, "movie fixture kind");

        let (dc, frx, reader, dir) = live_pipe_harness("latch-movie");
        let log = Logger::new(dir.join("latch-movie.log"), 0, 1);
        const NOW: i64 = 1_000_000_000_000;
        let act = presence::Action {
            running: true,
            has_start: true,
            start: NOW - 2000,
            ct: 2000,
            ..presence::Action::default()
        };
        let mut gate = Gate::new();
        let mut noop = |_: config::CardKind,
                        _: String,
                        _: Option<presence::Action>,
                        _: presence::Resolved| {};
        push_activity(
            &log,
            &dc,
            &mut gate,
            config::CardSpecs::default(),
            &mut noop,
            raw,
            Some(&act),
            2000,
            2_832_979,
            NOW,
            true,
            false,
        );
        let frame = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("a latched movie card must go out");
        let v: serde_json::Value = serde_json::from_slice(&frame).expect("decode frame");
        let a = &v["args"]["activity"];
        assert_eq!(
            a["name"].as_str().unwrap_or(""),
            "Latch Movie",
            "the snapshot's title is the header: {v}"
        );
        assert_eq!(
            a["details"].as_str().unwrap_or(""),
            "Latch Movie (2019)",
            "the snapshot's title and year lead the card: {v}"
        );
        assert_eq!(
            a["state"].as_str().unwrap_or(""),
            "Action \u{b7} 1h 40m",
            "the snapshot's genres and runtime: {v}"
        );
        assert_eq!(
            a["assets"]["large_text"].as_str().unwrap_or(""),
            "The media snapshot's synopsis.",
            "the snapshot's synopsis is the artwork tooltip: {v}"
        );
        assert_eq!(
            a["assets"]["large_image"].as_str().unwrap_or(""),
            "https://example.com/latch-media.jpg",
            "the snapshot's artwork: {v}"
        );

        // Rewrite the store without media artwork: only the mtime-driven
        // reload can see the new latch, and the latch poster is the fallback.
        store.put(movie("")).expect("rewrite the movie latch");
        set_store_mtime(&store_path, 2_000_000);
        push_activity(
            &log,
            &dc,
            &mut gate,
            config::CardSpecs::default(),
            &mut noop,
            raw,
            Some(&act),
            2000,
            2_832_979,
            NOW + 250,
            true,
            false,
        );
        let frame = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("the rewritten latch must reach the card");
        let v: serde_json::Value = serde_json::from_slice(&frame).expect("decode frame");
        assert_eq!(
            v["args"]["activity"]["assets"]["large_image"]
                .as_str()
                .unwrap_or(""),
            "https://example.com/latch-movie.jpg",
            "the latch poster fallback after reload: {v}"
        );

        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // Poll-loop sequence tests over the injected probe seam. The REAL loop body
    // (`poll_step`) runs -- real tracker, real gate, real push_activity, real
    // clear logic -- against a scripted player, a fake clock and the in-process
    // fake Discord pipe; only the player reads and the sleeps are injected.
    // -----------------------------------------------------------------------

    /// The scripted player the poll-loop tests drive: one window whose identity,
    /// aliveness, title, status and times the test mutates between steps. The
    /// title predicate stays the real one (`player::window::matches`).
    struct ScriptedPlayer {
        id: player::window::Handle,
        state: std::cell::RefCell<ScriptedWindow>,
    }

    /// The mutable half of [`ScriptedPlayer`].
    struct ScriptedWindow {
        /// `find_first` reports the window.
        present: bool,
        /// `alive` reports it still valid.
        alive: bool,
        title: String,
        status: PlayStatus,
        ct: i64,
        total: i64,
    }

    impl ScriptedPlayer {
        fn new(title: &str) -> ScriptedPlayer {
            ScriptedPlayer {
                id: player::window::Handle::dangling(),
                state: std::cell::RefCell::new(ScriptedWindow {
                    present: false,
                    alive: false,
                    title: title.to_string(),
                    status: PlayStatus::RUNNING,
                    ct: 0,
                    total: 2_832_979,
                }),
            }
        }

        fn set(&self, f: impl FnOnce(&mut ScriptedWindow)) {
            f(&mut self.state.borrow_mut());
        }
    }

    impl PlayerProbe for ScriptedPlayer {
        type Handle = player::window::Handle;

        fn find_first(&self) -> Option<Self::Handle> {
            self.state.borrow().present.then_some(self.id)
        }

        fn alive(&self, _id: Self::Handle) -> bool {
            self.state.borrow().alive
        }

        fn title(&self, _id: Self::Handle) -> String {
            self.state.borrow().title.clone()
        }

        fn matches(&self, title: &str) -> bool {
            player::window::matches(title)
        }

        fn status(&self, _id: Self::Handle) -> PlayStatus {
            self.state.borrow().status
        }

        fn current_time(&self, _id: Self::Handle) -> i64 {
            self.state.borrow().ct
        }

        fn total_time(&self, _id: Self::Handle) -> i64 {
            self.state.borrow().total
        }
    }

    /// One frame the fake Discord peer received: the connection it arrived on
    /// (1 = the first session, 2 = the reconnect) and whether it is the
    /// null-activity clear.
    struct PeerFrame {
        conn: usize,
        clear: bool,
        raw: Vec<u8>,
    }

    /// Accepts the next connection on `srv`, consumes the handshake, completes
    /// Discord's READY dispatch, and waits for the client's generation to reach
    /// `want`.
    fn accept_ready(srv: &discord::FakeServer, dc: &discord::Client, want: i64) -> discord::FakeConn {
        let sc = srv.accept().expect("client connects to the fake pipe");
        let (op, _) = sc.read_frame().expect("handshake frame");
        assert_eq!(op, discord::OP_HANDSHAKE, "first frame is the handshake");
        sc.write_frame(
            discord::OP_FRAME,
            br#"{"cmd":"DISPATCH","evt":"READY","data":{"v":1}}"#,
        )
        .expect("write READY");
        let deadline = Instant::now() + Duration::from_secs(5);
        while dc.generation() < want {
            assert!(
                Instant::now() < deadline,
                "client never reached generation {want}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        sc
    }

    /// Reads frames off one accepted connection and forwards each to `tx`. A
    /// card frame is acked so its ack timer is disarmed; a clear only when
    /// `ack_clears` -- the deaf-pipe test needs the clear to go unanswered.
    fn spawn_frame_reader(
        sc: discord::FakeConn,
        conn: usize,
        ack_clears: bool,
        tx: mpsc::Sender<PeerFrame>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            loop {
                let Ok((op, payload)) = sc.read_frame() else {
                    return;
                };
                if op != discord::OP_FRAME {
                    continue;
                }
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&payload) else {
                    continue;
                };
                let clear = v["args"]["activity"].is_null();
                if (!clear || ack_clears)
                    && let Some(nonce) = v["nonce"].as_str()
                    && !nonce.is_empty()
                {
                    let reply = format!(r#"{{"cmd":"SET_ACTIVITY","nonce":"{nonce}"}}"#);
                    let _ = sc.write_frame(discord::OP_FRAME, reply.as_bytes());
                }
                if tx
                    .send(PeerFrame {
                        conn,
                        clear,
                        raw: payload,
                    })
                    .is_err()
                {
                    return;
                }
            }
        })
    }

    /// The client the poll-loop tests drive: a no-op reply and ack-timeout sink
    /// so acks disarm pending commands and an unanswered one really reconnects.
    fn poll_test_client(base: String) -> discord::Client {
        discord::Client::new(discord::Config {
            client_id: "123456789012345678".to_string(),
            pipe_base: base,
            retry_delay: Duration::from_millis(30),
            on_reply: Some(Box::new(|_: discord::Reply| {})),
            on_ack_timeout: Some(Box::new(|_: &str| {})),
            ..Default::default()
        })
    }

    /// Drains the frames the peer has recorded so far (a short settle first, so
    /// the reader thread can forward everything the client has already written).
    fn drain_frames(rx: &mpsc::Receiver<PeerFrame>) -> Vec<PeerFrame> {
        std::thread::sleep(Duration::from_millis(150));
        let mut frames = Vec::new();
        while let Ok(f) = rx.try_recv() {
            frames.push(f);
        }
        frames
    }

    /// A: the clear is recorded per generation. Playing sends the card (acked);
    /// the window dies; the clear reaches the pipe exactly once and the
    /// no-player iterations after it do NOT re-clear.
    #[test]
    fn poll_window_gone_clears_once_per_generation() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        const RAW: &str = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        const NOW: i64 = 1_000_000_000_000;
        // T5-C: real media carries a current file; the no-file case clears on
        // its own path (t5c_poll_window_with_no_file_clears).
        let query = seed_playing_file(RAW, r"Q:\Media\Dune.Part.Two.2024.1080p.BluRay.x264.mkv");
        assert_eq!(query.kind, metadata::Kind::MOVIE, "movie fixture kind");

        let srv = discord::FakeServer::new(0);
        let dc = poll_test_client(srv.base.clone());
        dc.start();
        let sc = accept_ready(&srv, &dc, 1);
        let (ftx, frx) = mpsc::channel::<PeerFrame>();
        let reader = spawn_frame_reader(sc, 1, true, ftx);

        let dir = unique_dir("poll-clear-once");
        let log_path = dir.join("poll-clear-once.log");
        let log = Logger::new(&log_path, 0, 1);
        let now_clock = std::cell::Cell::new(NOW);
        let clock = || now_clock.get();
        let sleep = |_: Duration| {};
        let player = ScriptedPlayer::new(RAW);
        let mut state = PollState::new();

        // Playing: the first poll sends the card, the steady followers do not.
        player.set(|w| {
            w.present = true;
            w.alive = true;
            w.status = PlayStatus::RUNNING;
            w.ct = 1000;
        });
        for i in 0..3i64 {
            if i > 0 {
                now_clock.set(NOW + 250 * i);
                player.set(|w| w.ct = 1000 + 250 * i);
            }
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        }

        // The window dies: the clear goes out ONCE (and records the generation),
        // then the no-player iterations stay silent.
        player.set(|w| {
            w.alive = false;
            w.present = false;
        });
        for i in 0..6i64 {
            now_clock.set(NOW + 1000 + 250 * i);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        }

        let frames = drain_frames(&frx);
        assert_eq!(
            frames.len(),
            2,
            "want exactly the card and one clear, got {} frames ({} clears)",
            frames.len(),
            frames.iter().filter(|f| f.clear).count()
        );
        assert!(!frames[0].clear, "frame 1 is the playing card");
        assert!(frames[1].clear, "frame 2 is the clear");
        assert_eq!(frames[1].conn, 1, "the clear goes out on the live session");
        let v: serde_json::Value = serde_json::from_slice(&frames[1].raw).expect("decode clear");
        assert_eq!(v["cmd"], "SET_ACTIVITY", "the clear is a SET_ACTIVITY command");
        assert!(
            v["args"]["activity"].is_null(),
            "the clear carries a null activity: {v}"
        );

        // Positive control: the clear really came from the window-gone path.
        let log_text = std::fs::read_to_string(&log_path).expect("read log");
        assert_eq!(
            log_text.matches("cleared (player window gone)").count(),
            1,
            "the window-gone clear must be logged exactly once:\n{log_text}"
        );

        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// B: the exact stuck-card scenario. The peer accepts, READYs, acks the
    /// card, then goes deaf to the clear; the unacknowledged clear times out,
    /// the client reconnects (generation bump), and the clear is RE-ASSERTED on
    /// the new generation -- once -- because the recorded cleared generation no
    /// longer matches.
    #[test]
    fn poll_clear_reasserted_after_deaf_pipe_reconnect() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        const RAW: &str = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        const NOW: i64 = 1_000_000_000_000;
        // T5-C: real media carries a current file (see above).
        let query = seed_playing_file(RAW, r"Q:\Media\Dune.Part.Two.2024.1080p.BluRay.x264.mkv");
        assert_eq!(query.kind, metadata::Kind::MOVIE, "movie fixture kind");

        let srv = discord::FakeServer::new(0);
        let dc = poll_test_client(srv.base.clone());
        dc.start();
        let sc = accept_ready(&srv, &dc, 1);
        let (ftx, frx) = mpsc::channel::<PeerFrame>();
        let reader1 = spawn_frame_reader(sc, 1, false, ftx.clone());

        let dir = unique_dir("poll-deaf-pipe");
        let log_path = dir.join("poll-deaf-pipe.log");
        let log = Logger::new(&log_path, 0, 1);
        let now_clock = std::cell::Cell::new(NOW);
        let clock = || now_clock.get();
        let sleep = |_: Duration| {};
        let player = ScriptedPlayer::new(RAW);
        let mut state = PollState::new();

        // Playing: one card, acked by the peer.
        player.set(|w| {
            w.present = true;
            w.alive = true;
            w.status = PlayStatus::RUNNING;
            w.ct = 1000;
        });
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let card = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("the playing card");
        assert!(!card.clear && card.conn == 1, "frame 1 is the card");

        // The window dies; the clear goes out on generation 1 and is never
        // answered.
        player.set(|w| {
            w.alive = false;
            w.present = false;
        });
        now_clock.set(NOW + 1000);
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let first_clear = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("the clear on generation 1");
        assert!(
            first_clear.clear && first_clear.conn == 1,
            "frame 2 is the clear on the first session"
        );

        // One generation, one clear: the no-player polls stay silent.
        for i in 1..=4i64 {
            now_clock.set(NOW + 1000 + 250 * i);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        }
        let repeated = drain_frames(&frx);
        assert!(
            repeated.is_empty(),
            "the clear repeated on the same generation: {} frames",
            repeated.len()
        );

        // The ack timeout drops the deaf session; the client reconnects and the
        // READY dispatch bumps the generation.
        let sc2 = accept_ready(&srv, &dc, 2);
        let reader2 = spawn_frame_reader(sc2, 2, true, ftx);

        // The next no-player poll must re-assert the clear on the new session.
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let reasserted = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("the clear re-asserted after the reconnect");
        assert!(
            reasserted.clear && reasserted.conn == 2,
            "the re-asserted clear must arrive on the new session"
        );

        // ...exactly once: the new generation is recorded too.
        for i in 1..=4i64 {
            now_clock.set(NOW + 4000 + 250 * i);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        }
        let repeated2 = drain_frames(&frx);
        assert!(
            repeated2.is_empty(),
            "the re-asserted clear repeated: {} frames",
            repeated2.len()
        );

        // Positive control: both clear paths really ran, one clear each.
        let log_text = std::fs::read_to_string(&log_path).expect("read log");
        assert_eq!(
            log_text.matches("cleared (player window gone)").count(),
            1,
            "the window-gone clear must be logged once:\n{log_text}"
        );
        assert_eq!(
            log_text.matches("cleared (no PotPlayer)").count(),
            1,
            "the re-asserted no-player clear must be logged once:\n{log_text}"
        );

        let _ = dc.close();
        let _ = reader1.join();
        let _ = reader2.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// C: the sustained-stop clear on the real path. A window that stays Stopped
    /// past `STOP_GRACE_MS` clears the card (once); a window whose status
    /// flickers Running/Paused/Running with short stopped blips across the same
    /// span never does, because every live poll resets the stop anchor.
    #[test]
    fn poll_sustained_stop_clears_and_flicker_does_not() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        const RAW: &str = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        const NOW: i64 = 1_000_000_000_000;

        // Case 1: a still-Stopped window clears after the grace, exactly once.
        {
            // T5-C: real media carries a current file, so the grace (not the
            // no-file path) decides this clear.
            let query = seed_playing_file(RAW, r"Q:\Media\Dune.Part.Two.2024.1080p.BluRay.x264.mkv");
            assert_eq!(query.kind, metadata::Kind::MOVIE, "movie fixture kind");
            let srv = discord::FakeServer::new(0);
            let dc = poll_test_client(srv.base.clone());
            dc.start();
            let sc = accept_ready(&srv, &dc, 1);
            let (ftx, frx) = mpsc::channel::<PeerFrame>();
            let reader = spawn_frame_reader(sc, 1, true, ftx);
            let dir = unique_dir("poll-stop-grace");
            let log_path = dir.join("poll-stop-grace.log");
            let log = Logger::new(&log_path, 0, 1);
            let now_clock = std::cell::Cell::new(NOW);
            let clock = || now_clock.get();
            let sleep = |_: Duration| {};
            let player = ScriptedPlayer::new(RAW);
            let mut state = PollState::new();

            // Playing, then Stopped with the position frozen. The tracker keeps
            // reporting Running while the ct window still moves, then emits the
            // stop card and arms the grace at the poll where the window settles.
            player.set(|w| {
                w.present = true;
                w.alive = true;
                w.status = PlayStatus::RUNNING;
                w.ct = 1000;
            });
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            now_clock.set(NOW + 250);
            player.set(|w| w.ct = 1250);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            player.set(|w| w.status = PlayStatus::STOPPED);
            now_clock.set(NOW + 500);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            now_clock.set(NOW + 750);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);

            // Drive to just inside the grace (anchor at NOW+750, due at
            // NOW+15_750): still no clear.
            for i in 4..=62i64 {
                now_clock.set(NOW + 250 * i);
                poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            }
            let before = drain_frames(&frx);
            assert!(
                before.iter().all(|f| !f.clear),
                "a clear fired inside the grace: {}",
                before.iter().filter(|f| f.clear).count()
            );

            // The boundary: the grace has elapsed, so the clear goes out.
            now_clock.set(NOW + 15_750);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            let cleared = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("the sustained-stop clear");
            assert!(cleared.clear, "the grace elapsed, so the clear goes out");

            // ...exactly once: the generation is recorded.
            for i in 1..=3i64 {
                now_clock.set(NOW + 15_750 + 250 * i);
                poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            }
            let after = drain_frames(&frx);
            assert!(
                after.is_empty(),
                "the sustained-stop clear repeated: {} frames",
                after.len()
            );

            // Positive controls: the stop really happened, and the clear is the
            // one logged line.
            let log_text = std::fs::read_to_string(&log_path).expect("read log");
            assert!(
                log_text.contains("action=STOP"),
                "the script never reached a stop decision:\n{log_text}"
            );
            assert_eq!(
                log_text.matches("cleared (stopped player)").count(),
                1,
                "the sustained-stop clear must be logged once:\n{log_text}"
            );

            let _ = dc.close();
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&dir);
        }

        // Case 2: the flicker. Running/Paused/Running with short stopped blips
        // across a 20 s span (past the grace): the live polls reset the stop
        // anchor, so no clear ever fires.
        {
            // T5-C: real media carries a current file (see case 1).
            let query = seed_playing_file(RAW, r"Q:\Media\Dune.Part.Two.2024.1080p.BluRay.x264.mkv");
            assert_eq!(query.kind, metadata::Kind::MOVIE, "movie fixture kind");
            let srv = discord::FakeServer::new(0);
            let dc = poll_test_client(srv.base.clone());
            dc.start();
            let sc = accept_ready(&srv, &dc, 1);
            let (ftx, frx) = mpsc::channel::<PeerFrame>();
            let reader = spawn_frame_reader(sc, 1, true, ftx);
            let dir = unique_dir("poll-flicker");
            let log_path = dir.join("poll-flicker.log");
            let log = Logger::new(&log_path, 0, 1);
            let now_clock = std::cell::Cell::new(NOW);
            let clock = || now_clock.get();
            let sleep = |_: Duration| {};
            let player = ScriptedPlayer::new(RAW);
            let mut state = PollState::new();

            player.set(|w| {
                w.present = true;
                w.alive = true;
                w.status = PlayStatus::RUNNING;
                w.ct = 1000;
            });
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            let card = frx
                .recv_timeout(Duration::from_secs(2))
                .expect("the running card");
            assert!(!card.clear, "frame 1 is the running card");

            // 80 polls, 250 ms apart (20 s, past the 15 s grace). Per 8-poll
            // cycle: Running, Paused, Running (the owner's log flicker), then
            // five stopped polls with the position frozen. Each cycle's live
            // polls reset the stop anchor.
            for i in 1..=80i64 {
                now_clock.set(NOW + 250 * i);
                let phase = (i - 1) % 8;
                if phase == 1 {
                    player.set(|w| {
                        w.status = PlayStatus::PAUSED;
                        w.ct = 1000 + 250 * i;
                    });
                } else if phase >= 3 {
                    let frozen = 1000 + 250 * (i - phase + 2);
                    player.set(|w| {
                        w.status = PlayStatus::STOPPED;
                        w.ct = frozen;
                    });
                } else {
                    player.set(|w| {
                        w.status = PlayStatus::RUNNING;
                        w.ct = 1000 + 250 * i;
                    });
                }
                poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
            }

            let frames = drain_frames(&frx);
            assert!(
                frames.iter().all(|f| !f.clear),
                "the flickering window cleared {} times",
                frames.iter().filter(|f| f.clear).count()
            );
            // Positive control: the stopped blips really produced stop
            // decisions (the anchor armed and reset), not a vacuous pass.
            let log_text = std::fs::read_to_string(&log_path).expect("read log");
            assert!(
                log_text.contains("action=STOP"),
                "the flicker script never reached a stop decision:\n{log_text}"
            );
            assert_eq!(
                log_text.matches("cleared (stopped player)").count(),
                0,
                "a clear fired across the flicker:\n{log_text}"
            );

            let _ = dc.close();
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// T1: a series latch with no episode pin still resolves the show
    /// title/poster and the episode code from the query (the
    /// provisional/text-only path): no panic, no forced movie.
    #[test]
    fn series_latch_without_pin_resolves_show_and_code() {
        let latch = overrides::Override {
            scope: overrides::Scope::Series,
            key: "show".to_string(),
            title: "Latch Show".to_string(),
            year: 2011,
            provider: "tvmaze".to_string(),
            provider_id: "7".to_string(),
            poster: "https://example.com/latch-series.jpg".to_string(),
            media: None,
            episodes: Vec::new(),
            created_at_ms: 1_700_000_000_000,
        };
        // Season/episode query: the code comes from the query.
        let query = metadata::Query {
            kind: metadata::Kind::SERIES,
            title: "Latch Show".to_string(),
            season: 1,
            episode: 2,
            ..metadata::Query::default()
        };
        let res = resolved_from_latch(&latch, &query);
        assert_eq!(res.series, "Latch Show", "show title");
        assert_eq!(
            res.artwork, "https://example.com/latch-series.jpg",
            "poster"
        );
        assert!(!res.is_movie, "a series latch is never a movie");
        assert_eq!((res.season, res.number), (1, 2), "code from the query");
        assert!(res.episode_name.is_empty(), "no pin means no episode title");
        // Absolute query: the absolute number rides along, no season fabricated.
        let absolute = metadata::Query {
            kind: metadata::Kind::SERIES,
            title: "Latch Show".to_string(),
            episode: 7,
            absolute: true,
            ..metadata::Query::default()
        };
        let res = resolved_from_latch(&latch, &absolute);
        assert!(!res.is_movie, "absolute series latch is never a movie");
        assert_eq!(res.absolute, 7, "absolute from the query");
        assert_eq!((res.season, res.number), (0, 0), "no fabricated season");
        // The provisional card composes: the show title in the header (line 1
        // blanks) and the absolute code on line 2; never a panic.
        let card = presence::compose_card(
            "Latch.Show.07.1080p.mkv - PotPlayer",
            None,
            &res,
            res.is_movie,
            &presence::cardspec::default_card_spec(),
        );
        assert_eq!(card.name, "Latch Show", "provisional header");
        assert_eq!(card.details, "", "provisional details blank");
        assert_eq!(card.state, "E07", "provisional state carries the code");
    }

    /// T1: an explicit series latch beats a movie-classified file on the
    /// latch path, so `push_activity`'s `is_movie` yields a series card.
    #[test]
    fn explicit_series_latch_beats_movie_classified_file() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = unique_dir("latch-beats-movie");
        let show_dir = root.join("Latch Show");
        std::fs::create_dir_all(&show_dir).expect("media dirs");
        let file = show_dir.join("Latch.Movie.2019.1080p.mkv");
        std::fs::write(&file, "x").expect("media file");

        let store_path = root.join("overrides.json");
        let mut store = metadata::overrides::Store::empty_at(store_path.clone());
        store
            .put(overrides::Override {
                scope: overrides::Scope::Series,
                key: show_dir.to_string_lossy().into_owned(),
                title: "Latch Show".to_string(),
                year: 2011,
                provider: "tvmaze".to_string(),
                provider_id: "7".to_string(),
                poster: String::new(),
                media: None,
                episodes: Vec::new(),
                created_at_ms: 1_700_000_000_000,
            })
            .expect("latch the show folder");
        let _fixture = OverridesFixture::install(store_path);

        // The filename classifies as a movie, yet the series latch wins.
        let query = metadata::Query {
            kind: metadata::Kind::MOVIE,
            title: "Latch Movie".to_string(),
            year: 2019,
            ..metadata::Query::default()
        };
        let latched = latch_resolved(&file.to_string_lossy(), &query)
            .expect("the series latch wins over the movie hint");
        assert!(
            !latched.is_movie,
            "is_movie follows the latch, not the filename"
        );
        assert_eq!(latched.series, "Latch Show", "the latch's series name");

        let _ = std::fs::remove_dir_all(&root);
    }

    // -----------------------------------------------------------------------
    // T5-C (red first): no idle `Browsing`/`Idle` card, no `Watching
    // PotFlix`/`Watching PotPlayer`. An idle title (empty/whitespace, the
    // player app name) or a window with no current file must never send the
    // idle card: clear instead, record no preview, and keep real media
    // sending with the stop grace intact.
    // -----------------------------------------------------------------------

    /// T5-C: the idle-title predicate covers the app names and the empty
    /// cases (suffix-stripped, trimmed).
    #[test]
    fn t5c_idle_title_predicate() {
        for raw in [
            "",
            "   ",
            "\t\n ",
            "PotPlayer",
            "PotFlix",
            " - PotPlayer",
            "PotPlayer - PotPlayer",
            "PotFlix - PotPlayer",
            "  PotPlayer  ",
        ] {
            assert!(
                is_no_media_title(raw),
                "idle title {raw:?} must gate, got live"
            );
        }
        for raw in [
            "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer",
            "Show.S01E02.Pilot.1080p.mkv - PotPlayer",
            "Some Movie - PotPlayer",
        ] {
            assert!(
                !is_no_media_title(raw),
                "real media {raw:?} must not gate, got idle"
            );
        }
    }

    /// T5-C: `push_activity` with an idle raw title sends no card, records no
    /// preview, and never puts `Browsing`/`Idle` (or the app name) on the
    /// wire. Any frame it emits must be the null clear.
    #[test]
    fn t5c_push_idle_title_sends_no_card() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for raw in ["PotPlayer", "PotFlix", "", "   "] {
            let (dc, frx, reader, dir) = live_pipe_harness("t5c-idle-push");
            let log = Logger::new(dir.join("t5c-idle-push.log"), 0, 1);
            let mut gate = Gate::new();
            let previews = std::cell::Cell::new(0);
            let mut record = |_: config::CardKind,
                              _: String,
                              _: Option<presence::Action>,
                              _: presence::Resolved| {
                previews.set(previews.get() + 1);
            };
            let act = presence::Action {
                running: true,
                has_start: true,
                start: 1_000_000_000_000 - 1000,
                ct: 1000,
                ..presence::Action::default()
            };
            push_activity(
                &log,
                &dc,
                &mut gate,
                config::CardSpecs::default(),
                &mut record,
                raw,
                Some(&act),
                1000,
                2_832_979,
                1_000_000_000_000,
                true,
                false,
            );
            std::thread::sleep(Duration::from_millis(150));
            let mut frames = Vec::new();
            while let Ok(payload) = frx.try_recv() {
                frames.push(payload);
            }
            for payload in &frames {
                let v: serde_json::Value =
                    serde_json::from_slice(payload).expect("decode frame");
                assert_eq!(v["cmd"], "SET_ACTIVITY", "raw {raw:?}: {v}");
                let activity = &v["args"]["activity"];
                if activity.is_null() {
                    continue;
                }
                let details = activity["details"].as_str().unwrap_or("");
                let state = activity.get("state").and_then(|s| s.as_str()).unwrap_or("");
                let name = activity["name"].as_str().unwrap_or("");
                let text = format!("{name} {details} {state} {v}");
                assert!(
                    !text.contains("Browsing")
                        && !text.contains("Idle")
                        && !text.contains("PotPlayer")
                        && !text.contains("PotFlix"),
                    "raw {raw:?}: idle payload reached the wire: {v}"
                );
                panic!("raw {raw:?}: idle title sent a card, want no card (clear only): {v}");
            }
            assert_eq!(
                previews.get(),
                0,
                "raw {raw:?}: idle push recorded a preview"
            );
            let _ = dc.close();
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// T5-C: real media still sends through the same path (positive control)
    /// with a preview and no idle lines.
    #[test]
    fn t5c_push_real_media_still_sends() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (dc, frx, reader, dir) = live_pipe_harness("t5c-real-push");
        let log = Logger::new(dir.join("t5c-real-push.log"), 0, 1);
        let mut gate = Gate::new();
        let previews = std::cell::Cell::new(0);
        let mut record = |_: config::CardKind,
                          _: String,
                          _: Option<presence::Action>,
                          _: presence::Resolved| {
            previews.set(previews.get() + 1);
        };
        const RAW: &str = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        let act = presence::Action {
            running: true,
            has_start: true,
            start: 1_000_000_000_000 - 1000,
            ct: 1000,
            ..presence::Action::default()
        };
        push_activity(
            &log,
            &dc,
            &mut gate,
            config::CardSpecs::default(),
            &mut record,
            RAW,
            Some(&act),
            1000,
            2_832_979,
            1_000_000_000_000,
            true,
            false,
        );
        let frame = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("real media must send a card");
        let v: serde_json::Value = serde_json::from_slice(&frame).expect("decode frame");
        assert!(
            !v["args"]["activity"].is_null(),
            "real media must send a card, got a clear: {v}"
        );
        assert_eq!(previews.get(), 1, "real media must record the preview");
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T5-C: a live window titled `PotPlayer` (no media) clears on the real
    /// poll path with no card and no idle lines.
    #[test]
    fn t5c_poll_idle_title_clears_no_card() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        const NOW: i64 = 1_000_000_000_000;
        let srv = discord::FakeServer::new(0);
        let dc = poll_test_client(srv.base.clone());
        dc.start();
        let sc = accept_ready(&srv, &dc, 1);
        let (ftx, frx) = mpsc::channel::<PeerFrame>();
        let reader = spawn_frame_reader(sc, 1, true, ftx);
        let dir = unique_dir("t5c-poll-idle");
        let log = Logger::new(dir.join("t5c-poll-idle.log"), 0, 1);
        let now_clock = std::cell::Cell::new(NOW);
        let clock = || now_clock.get();
        let sleep = |_: Duration| {};
        let player = ScriptedPlayer::new("PotPlayer");
        let mut state = PollState::new();
        player.set(|w| {
            w.present = true;
            w.alive = true;
            w.title = "PotPlayer".to_string();
            w.status = PlayStatus::RUNNING;
            w.ct = 1000;
        });
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let frames = drain_frames(&frx);
        assert!(
            !frames.is_empty(),
            "idle window must clear (no frames at all)"
        );
        for f in &frames {
            let v: serde_json::Value =
                serde_json::from_slice(&f.raw).expect("decode frame");
            if v["args"]["activity"].is_null() {
                continue;
            }
            let text = String::from_utf8_lossy(&f.raw).into_owned();
            assert!(
                !text.contains("Browsing") && !text.contains("Idle"),
                "idle window sent a card: {text}"
            );
            panic!("idle window sent a card, want clear only: {text}");
        }
        assert!(
            frames.iter().any(|f| f.clear),
            "idle window must emit the null clear, got {} card frame(s)",
            frames.len()
        );
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T5-C: a window with a real-looking title but no current file clears
    /// instead of parking a card (stopped, so the stop grace is not the path).
    #[test]
    fn t5c_poll_window_with_no_file_clears() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        const RAW: &str = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        const NOW: i64 = 1_000_000_000_000;
        let query = seed_playing_file(RAW, "");
        assert_eq!(query.kind, metadata::Kind::MOVIE, "movie fixture kind");
        let srv = discord::FakeServer::new(0);
        let dc = poll_test_client(srv.base.clone());
        dc.start();
        let sc = accept_ready(&srv, &dc, 1);
        let (ftx, frx) = mpsc::channel::<PeerFrame>();
        let reader = spawn_frame_reader(sc, 1, true, ftx);
        let dir = unique_dir("t5c-poll-nofile");
        let log = Logger::new(dir.join("t5c-poll-nofile.log"), 0, 1);
        let now_clock = std::cell::Cell::new(NOW);
        let clock = || now_clock.get();
        let sleep = |_: Duration| {};
        let player = ScriptedPlayer::new(RAW);
        let mut state = PollState::new();
        player.set(|w| {
            w.present = true;
            w.alive = true;
            w.status = PlayStatus::STOPPED;
            w.ct = 1250;
        });
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let frames = drain_frames(&frx);
        assert!(
            !frames.is_empty(),
            "window with no file must clear (no frames at all)"
        );
        for f in &frames {
            assert!(
                f.clear,
                "window with no file must send only the clear, got card: {}",
                String::from_utf8_lossy(&f.raw)
            );
        }
        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T5-E: one generation, idle -> clear -> real media -> idle clears
    /// again. A successful media send re-arms the once-per-generation idle
    /// clear; without the re-arm the second idle is suppressed and the media
    /// card stays parked after the media is gone.
    #[test]
    fn t5e_idle_media_idle_clears_twice_on_one_generation() {
        let _live_serial = LIVE_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        config::set_live_card_specs(config::CardSpecs::default());
        const RAW: &str = "Dune.Part.Two.2024.1080p.BluRay.x264.mkv - PotPlayer";
        const NOW: i64 = 1_000_000_000_000;
        let query = seed_playing_file(RAW, r"Q:\Media\Dune.Part.Two.2024.1080p.BluRay.x264.mkv");
        assert_eq!(query.kind, metadata::Kind::MOVIE, "movie fixture kind");

        let srv = discord::FakeServer::new(0);
        let dc = poll_test_client(srv.base.clone());
        dc.start();
        let sc = accept_ready(&srv, &dc, 1);
        let (ftx, frx) = mpsc::channel::<PeerFrame>();
        let reader = spawn_frame_reader(sc, 1, true, ftx);
        let dir = unique_dir("t5e-rearm");
        let log = Logger::new(dir.join("t5e-rearm.log"), 0, 1);
        let now_clock = std::cell::Cell::new(NOW);
        let clock = || now_clock.get();
        let sleep = |_: Duration| {};
        let player = ScriptedPlayer::new("PotPlayer");
        let mut state = PollState::new();

        // Phase 1: idle title on a live window. The first poll clears; the
        // steady idle followers stay silent (once per generation).
        player.set(|w| {
            w.present = true;
            w.alive = true;
            w.title = "PotPlayer".to_string();
            w.status = PlayStatus::RUNNING;
            w.ct = 1000;
        });
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let first = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("phase 1: the first idle must clear");
        assert!(first.clear && first.conn == 1, "phase 1: want the null clear");
        for i in 1..=2i64 {
            now_clock.set(NOW + 250 * i);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        }
        let silent = drain_frames(&frx);
        assert!(
            silent.is_empty(),
            "phase 1: idle must clear once per generation, got {} extra frame(s)",
            silent.len()
        );

        // Phase 2: real media in the SAME window on the SAME generation. The
        // card must go out (and re-arm the clear); it must not be a clear and
        // must not carry idle lines.
        now_clock.set(NOW + 1000);
        player.set(|w| {
            w.title = RAW.to_string();
            w.status = PlayStatus::RUNNING;
            w.ct = 2000;
        });
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let card = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("phase 2: real media must send a card");
        assert!(!card.clear, "phase 2: real media must send a card, got a clear");
        let v: serde_json::Value =
            serde_json::from_slice(&card.raw).expect("decode the media card");
        assert!(
            !v["args"]["activity"].is_null(),
            "phase 2: the card must be present: {v}"
        );
        let text = String::from_utf8_lossy(&card.raw).into_owned();
        assert!(
            !text.contains("Browsing") && !text.contains("Idle"),
            "phase 2: media card carries idle lines: {text}"
        );

        // Phase 3: back to idle on the same generation. The re-armed clear
        // must go out again; further idle polls stay silent.
        now_clock.set(NOW + 1250);
        player.set(|w| {
            w.title = "PotPlayer".to_string();
            w.status = PlayStatus::RUNNING;
            w.ct = 2250;
        });
        poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        let second = frx
            .recv_timeout(Duration::from_secs(2))
            .expect("phase 3: idle after media must clear again");
        assert!(
            second.clear && second.conn == 1,
            "phase 3: want the second null clear on the same generation"
        );
        for i in 1..=2i64 {
            now_clock.set(NOW + 1250 + 250 * i);
            poll_steps(&log, &dc, &player, &clock, &mut state, &sleep, 1);
        }
        let tail = drain_frames(&frx);
        assert!(
            tail.is_empty(),
            "phase 3: the second clear must fire once, got {} extra frame(s)",
            tail.len()
        );

        let _ = dc.close();
        let _ = reader.join();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lock probe tells a live Potflix from a foreign squatter: a holder
    /// that answers the handshake magic probes Live; a silent holder (or a
    /// free port) probes Foreign. Staged on ephemeral ports -- the live lock
    /// port 17843 is never bound by a test.
    #[test]
    fn single_instance_probe_distinguishes_live_from_foreign() {
        let live = app_support::instance::acquire_instance_lock_on(0)
            .expect("ephemeral live holder");
        let live_port = live.local_addr().expect("live port").port();
        assert_ne!(
            live_port,
            app_support::instance::LOCK_PORT,
            "a test must never stage the live lock port"
        );
        spawn_instance_responder(&live);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            probe_instance_port(live_port),
            InstanceProbe::Live,
            "live responder must probe Live"
        );

        let foreign = app_support::instance::acquire_instance_lock_on(0)
            .expect("ephemeral foreign holder");
        let foreign_port = foreign.local_addr().expect("foreign port").port();
        assert_ne!(
            foreign_port,
            app_support::instance::LOCK_PORT,
            "a test must never stage the live lock port"
        );
        assert_ne!(foreign_port, live_port, "staged ports collided");
        assert_eq!(
            probe_instance_port(foreign_port),
            InstanceProbe::Foreign,
            "silent holder must probe Foreign"
        );
    }

    /// The loser exit codes: a live holder exits 0 (already running, cleanly,
    /// with a log line); a foreign holder exits non-zero (a real failure).
    #[test]
    fn lock_failure_exit_codes_are_0_for_live_1_for_foreign() {
        assert_eq!(
            lock_failure_exit_code(InstanceProbe::Live),
            app_support::instance::ALREADY_RUNNING_EXIT_CODE
        );
        assert_eq!(app_support::instance::ALREADY_RUNNING_EXIT_CODE, 0);
        assert_eq!(
            lock_failure_exit_code(InstanceProbe::Foreign),
            FOREIGN_LOCK_EXIT_CODE
        );
        assert_ne!(FOREIGN_LOCK_EXIT_CODE, 0);
    }

    /// With no holder at all the probe is Foreign too: a teardown race is a
    /// failure with a non-zero exit, never a silent no-op.
    #[test]
    fn probe_unreachable_port_is_foreign() {
        let stage = app_support::instance::acquire_instance_lock_on(0).expect("stage free port");
        let port = stage.local_addr().expect("port").port();
        assert_ne!(
            port,
            app_support::instance::LOCK_PORT,
            "a test must never stage the live lock port"
        );
        drop(stage);
        let probe = probe_instance_port(port);
        assert_eq!(
            probe,
            InstanceProbe::Foreign,
            "free port {port} must probe Foreign"
        );
    }
}
