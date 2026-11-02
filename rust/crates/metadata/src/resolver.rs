//! `metadata::resolver` -- mirrors `go\internal\metadata\resolver.go`.
//!
//! The provider chain. The poll loop uses the NON-BLOCKING [`Resolver::resolve`]
//! (a cache miss fires one background fetch per cache key, single-flight, and the
//! card falls back to the clean title until it lands); the CLIs use the BLOCKING
//! [`Resolver::lookup_now`]. Order: TVmaze for TV, AniList, Tenrai and Kitsu
//! for anime, Cinemeta as the general fallback and the first movie leg.
//! Wikipedia, JustWatch, Episodate, Bangumi, LiveChart.me and ANN stay as
//! modules (with their tests) but sit OFF the default chain: they answered too
//! slowly or too unreliably to keep in front of a user's lookup.
//! Offline safe: with no cache entry and no
//! network the card is exactly the clean card, and [`Options::offline`] serves
//! cache-only. `force` bypasses and re-attempts.
//!
//! # The chain is behaviour, not detail
//!
//! * [`Resolver::ordered_sources`] ranks providers per kind. TVmaze is first for
//!   EVERY series query including `Absolute` (its season/episode tables are
//!   authoritative, while AniList `streamingEpisodes` are mislabeled for
//!   absolute numbering -- Re:ZERO media 21355 serves 51-66 for an episode-1
//!   query, and a wrong title is worse than no title). Unranked names keep their
//!   relative order at rank 100, so an injected test source is still tried.
//! * `fetchChain` keeps going past an error (`sawError`, first error kept),
//!   past a source that answered but lacks the requested episode, and past a
//!   match with no synopsis (kept as the fallback while the walk continues).
//!   A movie answer with a synopsis returns immediately, as does a series
//!   answer with a number-exact episode match ([`match_episode`], NEVER a
//!   positional index) plus a synopsis; year never gates, cover and length
//!   never gate.
//! * `fetchMedia` retries the SAME chain once with the separator-normalized
//!   alternate title, and ONLY after a definitive primary miss: a transient
//!   primary is unknown, and substituting an alternate match would overwrite a
//!   correct cached positive under the ORIGINAL key.
//! * Only a definitive miss is cached negative (10 minutes in memory, 30 days on
//!   disk). A transient failure -- including cancellation -- persists NOTHING.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use metadata_core::alternate::alternate_query;
use metadata_core::types::{Button, Episode, Kind, Media, Query, Resolution, normalize_url};
use metadata_http::cancel::Cancel;
use metadata_http::http::{DEFAULT_HTTP_TIMEOUT, HttpClient};

use crate::artwork::{cap_synopsis, first_non_empty, strip_html};
use crate::cache::{
    cache_has, cache_key, cache_live, cache_load_entry, cache_store, cache_store_negative,
    negative_mem_expiry, resolve_cache_dir,
};
use crate::tvmaze::TvmazeSource;
use crate::{Error, anilist::AnilistSource, cinemeta::CinemetaSource};
use crate::{animeschedule::AnimescheduleSource, imdb::ImdbClient, imdb::ImdbSource};
use crate::{kitsu::KitsuSource, tenrai::TenraiSource};

/// `negativeBackoff` keeps a failed key from being hammered. In-memory only; the
/// on-disk cache stays additive forever.
pub const NEGATIVE_BACKOFF: Duration = Duration::from_secs(10 * 60);

/// Discord caps `large_image` at 300 chars: an artwork URL longer than that
/// (or empty, non-http(s), or signed/expiring) degrades to the app logo
/// downstream. The backfill budget mirrors the presence guard exactly.
const ARTWORK_BUDGET: usize = 300;

/// The cap on rows one provider's [`Source::find_all`] returns: a picker page's
/// worth of entries, never the provider's whole answer.
pub(crate) const FIND_ALL_LIMIT: usize = 12;

/// The search path a provider belongs to: `Regular` (live-action) or `Anime`.
/// A Regular scope asks only Regular legs, an Anime scope only Anime legs,
/// All asks both. The cache key already carries the scope, so no cross-path
/// bleed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchPath {
    Regular,
    Anime,
}

/// `Source` is one keyless metadata provider. Go's `Source` interface:
/// `Name() string` and `Find(ctx, q) (*Media, error)`.
///
/// `ctx` is [`Cancel`] (`metadata-http`'s `context.Context` stand-in): a source
/// must abort a wait when the scope ends and must return the scope's error
/// rather than a miss.
pub trait Source: Send + Sync {
    /// The provider's name, as it appears in a [`Media::source`] and in the
    /// chain ranking.
    fn name(&self) -> &str;

    /// The path this leg belongs to. Every source tags its own path; the
    /// default maps the known names so test fakes without an explicit tag
    /// still route correctly.
    fn path(&self) -> SearchPath {
        match self.name() {
            "anilist" | "tenrai" | "kitsu" | "animeschedule" => SearchPath::Anime,
            _ => SearchPath::Regular,
        }
    }

    /// `Find`: `Ok(None)` is a definitive miss for this source, `Err` is a
    /// transient failure, `Ok(Some)` is an answer the chain still has to check
    /// against the query.
    fn find(&self, cancel: &Cancel, q: &Query) -> Result<Option<Media>, Error>;

    /// The provider's OWN ranked answer for `q` as a list, so a manual search
    /// can offer several pickable entries instead of the one `Find` narrows to.
    /// A provider overrides this with ONE request (its search endpoint) and
    /// maps each row to a [`Media`] with an EMPTY episode table: nothing here
    /// fetches a table, so a search stays one request per leg.
    ///
    /// The default keeps every single-answer provider working unchanged: the
    /// `Find` answer as a one-row list.
    fn find_all(&self, cancel: &Cancel, q: &Query) -> Result<Vec<Media>, Error> {
        Ok(self.find(cancel, q)?.into_iter().collect())
    }
}

/// `Logf`: Go's `Logf func(format string, args ...any)`, with the message
/// already formatted.
pub type LogFn = Arc<dyn Fn(&str) + Send + Sync>;

/// `Options` for [`Resolver::new`]. The zero value is usable: no dir means the
/// default cache dir, no timeout means [`DEFAULT_HTTP_TIMEOUT`], and no sources
/// means [`default_sources`].
pub struct Options {
    /// Cache directory; empty resolves to `%LOCALAPPDATA%\Potflix\meta`.
    pub dir: String,
    /// Per-request HTTP budget; zero means [`DEFAULT_HTTP_TIMEOUT`].
    pub timeout: Duration,
    /// The provider set; `None` selects the built-in seven. `Some(vec![])` is a
    /// deliberately empty chain (every lookup is a definitive miss).
    pub sources: Option<Vec<Arc<dyn Source>>>,
    /// A sink for the resolver's diagnostics (`metadata: source ... error: ...`).
    pub logf: Option<LogFn>,
    /// Cache-only: never touch the network, never spawn a fetch.
    pub offline: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            dir: String::new(),
            timeout: Duration::ZERO,
            sources: None,
            logf: None,
            offline: false,
        }
    }
}

/// `makeDefaultSources`: the built-in keyless providers in their ranking
/// order, sharing one connection pool. Regular primaries (Cinemeta, TVmaze,
/// IMDb) plus anime primaries (AniList, Tenrai, Kitsu, AnimeSchedule).
fn make_default_sources(timeout: Duration) -> Vec<Arc<dyn Source>> {
    let client = HttpClient::new(timeout);
    vec![
        Arc::new(TvmazeSource::new("https://api.tvmaze.com", client.clone())),
        Arc::new(AnilistSource::new(
            "https://graphql.anilist.co",
            client.clone(),
        )),
        Arc::new(TenraiSource::new(
            "https://api.tenrai.org",
            client.clone(),
        )),
        Arc::new(KitsuSource::new(
            "https://kitsu.io/api/edge",
            client.clone(),
        )),
        Arc::new(CinemetaSource::new(
            "https://v3-cinemeta.strem.io",
            client.clone(),
        )),
        Arc::new(AnimescheduleSource::new(
            "https://animeschedule.net",
            client.clone(),
        )),
        Arc::new(ImdbSource::new(
            "https://api.graphql.imdb.com",
            ImdbClient::new(timeout),
        )),
    ]
}

/// `DefaultSources`: the built-in keyless providers (TVmaze, AniList, Tenrai,
/// Kitsu, Cinemeta, AnimeSchedule and IMDb) in their ranking order.
pub fn default_sources() -> Vec<Arc<dyn Source>> {
    make_default_sources(Duration::ZERO)
}

/// A one-shot completion flag, Go's `chan struct{}`: `close` wakes every waiter.
#[derive(Debug, Default)]
pub(crate) struct Done {
    done: Mutex<bool>,
    signal: Condvar,
}

impl Done {
    pub(crate) fn new() -> Arc<Done> {
        Arc::new(Done::default())
    }

    /// `close(ch)`.
    pub(crate) fn close(&self) {
        let mut done = self.lock();
        *done = true;
        self.signal.notify_all();
    }

    /// Go's `select { case <-ch: ...; case <-ctx.Done(): ... }`: returns when the
    /// flag is closed or the scope ends, whichever happens first.
    pub(crate) fn wait(&self, cancel: &Cancel) {
        let mut done = self.lock();
        loop {
            if *done || cancel.is_cancelled() {
                return;
            }
            let (guard, _) = self
                .signal
                .wait_timeout(done, Duration::from_millis(25))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            done = guard;
        }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        self.done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Closes the flag on every exit path, Go's `defer close(done)`.
struct DoneGuard(Arc<Done>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[derive(Default)]
struct State {
    mem: HashMap<String, Media>,
    inflight: HashMap<String, Arc<Done>>,
    neg: HashMap<String, SystemTime>,
    probed: HashSet<String>,
}

struct Inner {
    mu: Mutex<State>,
    dir: String,
    timeout: Duration,
    sources: Vec<Arc<dyn Source>>,
    logf: Option<LogFn>,
    offline: bool,
}

/// `Resolver` resolves `Query`s to episode titles without ever blocking on the
/// network in [`Resolver::resolve`]. Safe for concurrent use; cloneable, and
/// every clone shares the same memory, in-flight set and suppression map.
#[derive(Clone)]
pub struct Resolver {
    inner: Arc<Inner>,
}

impl Resolver {
    /// `New`: builds a resolver. A zero timeout takes
    /// [`DEFAULT_HTTP_TIMEOUT`]; `None` sources selects [`default_sources`].
    pub fn new(opts: Options) -> Resolver {
        let timeout = if opts.timeout.is_zero() {
            DEFAULT_HTTP_TIMEOUT
        } else {
            opts.timeout
        };
        let sources = match opts.sources {
            Some(sources) => sources,
            None => make_default_sources(timeout),
        };
        Resolver {
            inner: Arc::new(Inner {
                mu: Mutex::new(State::default()),
                dir: resolve_cache_dir(&opts.dir),
                timeout,
                sources,
                logf: opts.logf,
                offline: opts.offline,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner
            .mu
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn log(&self, message: &str) {
        if let Some(logf) = &self.inner.logf {
            logf(message);
        }
    }

    /// The resolver's cache directory.
    pub fn dir(&self) -> &str {
        &self.inner.dir
    }

    /// `orderedSources`: ranks providers per Kind; unknown names keep their
    /// relative order at the end so injected test sources are still tried.
    /// Series: TVmaze, AniList, Tenrai, Kitsu, AnimeSchedule, Cinemeta, IMDb.
    /// Movie: Cinemeta, AniList, Tenrai, Kitsu, AnimeSchedule, IMDb.
    pub fn ordered_sources(&self, q: &Query) -> Vec<Arc<dyn Source>> {
        let pref: &[(&str, i32)] = if q.kind == Kind::MOVIE {
            &[
                ("cinemeta", 0),
                ("anilist", 1),
                ("tenrai", 2),
                ("kitsu", 3),
                ("animeschedule", 4),
                ("imdb", 5),
            ]
        } else {
            &[
                ("tvmaze", 0),
                ("anilist", 1),
                ("tenrai", 2),
                ("kitsu", 3),
                ("animeschedule", 4),
                ("cinemeta", 5),
                ("imdb", 6),
            ]
        };
        let mut out = self.inner.sources.clone();
        out.sort_by_key(|source| {
            pref.iter()
                .find(|(name, _)| *name == source.name())
                .map_or(100, |(_, rank)| *rank)
        });
        out
    }

    /// `ResolveCached`: [`Resolver::resolve`] without the fetch. `Some` when
    /// memory or disk can answer `q`, `None` otherwise -- never touching the
    /// network and never spawning a background fetch. A stale key (a live entry
    /// that cannot answer `q`) reports `None` so the caller routes it through its
    /// bounded fetch path instead of fanning out one uncancellable fetch per
    /// stale key.
    pub fn resolve_cached(&self, q: &Query) -> Option<Resolution> {
        let key = cache_key(q);
        let mut state = self.lock();
        if let Some(media) = cached_answer(&mut state, &self.inner.dir, &key, q) {
            return Some(resolve_from_media(&media, q));
        }
        if let Some(expiry) = state.neg.get(&key).copied() {
            if SystemTime::now() < expiry {
                if probed_live(&mut state, &self.inner.dir, &key) {
                    return Some(Resolution::default());
                }
            } else {
                state.neg.remove(&key);
            }
        }
        if let Some(entry) = cache_load_entry(&self.inner.dir, &key) {
            if entry.not_found {
                let expiry =
                    negative_mem_expiry(&entry.checked_at, SystemTime::now(), NEGATIVE_BACKOFF);
                state.neg.insert(key, expiry);
                return Some(Resolution::default());
            }
            if let Some(media) = entry.media
                && media_answers(&media, q)
            {
                state.mem.insert(key, media.clone());
                return Some(resolve_from_media(&media, q));
            }
        }
        None
    }

    /// `Resolve` NEVER blocks on network: it returns from the memory/disk cache
    /// when present, else fires ONE background fetch per cache key
    /// (single-flight) and returns `Found=false`.
    pub fn resolve(&self, cancel: &Cancel, q: &Query) -> Result<Resolution, Error> {
        let key = cache_key(q);
        let mut state = self.lock();
        if let Some(media) = cached_answer(&mut state, &self.inner.dir, &key, q) {
            return Ok(resolve_from_media(&media, q));
        }
        // A cached entry that cannot answer q is treated as a miss so a
        // stale/wrong source never pins the key; the fetch below may overwrite it
        // with the source that does answer. The stale entry stays in mem for
        // other episodes sharing this key.
        if let Some(expiry) = state.neg.get(&key).copied() {
            if SystemTime::now() < expiry {
                if probed_live(&mut state, &self.inner.dir, &key) {
                    return Ok(Resolution::default());
                }
            } else {
                state.neg.remove(&key);
            }
        }
        if let Some(entry) = cache_load_entry(&self.inner.dir, &key) {
            if entry.not_found {
                // Fresh persisted miss: no network, same observable result as a
                // fresh not-found lookup. Repopulate the in-memory fast path,
                // bounded by the stored TTL.
                let expiry =
                    negative_mem_expiry(&entry.checked_at, SystemTime::now(), NEGATIVE_BACKOFF);
                state.neg.insert(key, expiry);
                return Ok(Resolution::default());
            }
            if let Some(media) = entry.media
                && media_answers(&media, q)
            {
                state.mem.insert(key, media.clone());
                return Ok(resolve_from_media(&media, q));
            }
            // Stale on-disk entry for this episode: fall through to the fetch.
        }
        if self.inner.offline {
            return Ok(Resolution::default());
        }
        if state.inflight.contains_key(&key) {
            return Ok(Resolution::default());
        }
        let done = Done::new();
        state.inflight.insert(key.clone(), Arc::clone(&done));
        drop(state);
        let me = self.clone();
        let cancel = cancel.clone();
        let q = q.clone();
        std::thread::spawn(move || me.background_fetch(cancel, q, &key, done));
        Ok(Resolution::default())
    }

    /// `backgroundFetch`: one fetch per key, in its own scope
    /// (`timeout * 4`), persisting a negative only for a definitive miss.
    pub(crate) fn background_fetch(&self, cancel: Cancel, q: Query, key: &str, done: Arc<Done>) {
        let _guard = DoneGuard(Arc::clone(&done));
        let fetch_cancel = child_scope(&cancel, self.inner.timeout * 4);
        let chain = self.fetch_media(&fetch_cancel, &q);
        let mut state = self.lock();
        state.inflight.remove(key);
        let timed_out = fetch_cancel.err().is_some();
        // Release the propagation watcher: this scope is finished.
        fetch_cancel.cancel();
        let Some(media) = chain.media else {
            // A cancelled/timed-out fetch or any provider error is transient,
            // never a confirmed not-found: persist nothing, suppress nothing.
            if timed_out || !chain.definitive {
                return;
            }
            state
                .neg
                .insert(key.to_string(), SystemTime::now() + NEGATIVE_BACKOFF);
            drop(state);
            if let Err(err) = cache_store_negative(&self.inner.dir, key) {
                self.log(&format!("metadata: cache store negative {key:?}: {err}"));
            }
            return;
        };
        state.mem.insert(key.to_string(), media.clone());
        state.neg.remove(key);
        drop(state);
        if let Err(err) = cache_store(&self.inner.dir, key, &media) {
            self.log(&format!("metadata: cache store {key:?}: {err}"));
        }
    }

    /// `LookupNow`: the blocking form for CLI/tests -- fetch if needed, then
    /// resolve.
    pub fn lookup_now(&self, cancel: &Cancel, q: &Query) -> Result<Resolution, Error> {
        self.lookup_now_force(cancel, q, false)
    }

    /// `LookupNowForce`: `force=true` bypasses the memory/disk cache and the
    /// negative backoff for this query, fetches fresh, and rewrites the cache
    /// entry on success. Offline still serves a miss.
    pub fn lookup_now_force(
        &self,
        cancel: &Cancel,
        q: &Query,
        force: bool,
    ) -> Result<Resolution, Error> {
        self.lookup_now_force_src(cancel, q, force)
            .map(|(resolution, _)| resolution)
    }

    /// `LookupNowForceSrc` with provenance: the flag reports whether a FOUND
    /// resolution was served from the memory/disk cache without a fetch.
    /// `force=true` never reports `true`; a miss reports `false`.
    pub fn lookup_now_force_src(
        &self,
        cancel: &Cancel,
        q: &Query,
        force: bool,
    ) -> Result<(Resolution, bool), Error> {
        let key = cache_key(q);
        let mut prev_neg: Option<SystemTime> = None;
        let mut state = self.lock();
        if !force {
            if let Some(media) = cached_answer(&mut state, &self.inner.dir, &key, q) {
                return Ok((resolve_from_media(&media, q), true));
            }
            if let Some(expiry) = state.neg.get(&key).copied() {
                if SystemTime::now() < expiry {
                    if probed_live(&mut state, &self.inner.dir, &key) {
                        return Ok((Resolution::default(), false));
                    }
                } else {
                    state.neg.remove(&key);
                }
            }
            if let Some(entry) = cache_load_entry(&self.inner.dir, &key) {
                if entry.not_found {
                    // Fresh persisted miss: no network, exactly the observable
                    // result of a fresh not-found lookup.
                    let expiry =
                        negative_mem_expiry(&entry.checked_at, SystemTime::now(), NEGATIVE_BACKOFF);
                    state.neg.insert(key.clone(), expiry);
                    return Ok((Resolution::default(), false));
                }
                if let Some(media) = entry.media
                    && media_answers(&media, q)
                {
                    state.mem.insert(key.clone(), media.clone());
                    return Ok((resolve_from_media(&media, q), true));
                }
                // Stale on-disk entry: fall through to the fetch below.
            }
        } else {
            // A forced refetch must never be pinned by a stale answer or a
            // recent miss; the fetch below overwrites both on success. Save the
            // cleared negative so a transient failure can restore it.
            prev_neg = state.neg.get(&key).copied();
            state.neg.remove(&key);
        }
        if self.inner.offline {
            return Ok((Resolution::default(), false));
        }
        if let Some(done) = state.inflight.get(&key).cloned() {
            drop(state);
            done.wait(cancel);
            if let Some(reason) = cancel.err() {
                return Err(Error::Cancelled(reason));
            }
            let state = self.lock();
            if let Some(media) = state.mem.get(&key) {
                // Served by a concurrent fetch, not the cache.
                return Ok((resolve_from_media(media, q), false));
            }
            if let Some(expiry) = state.neg.get(&key).copied()
                && SystemTime::now() < expiry
            {
                // The owning fetch confirmed the miss (it recorded suppression
                // before releasing the joiners).
                return Ok((Resolution::default(), false));
            }
            // The owning fetch ended without an answer and without recording a
            // miss: its outcome was transient, so this join is transient too.
            return Err(Error::unavailable(format!(
                "metadata: concurrent fetch failed transiently for {key:?}: {}",
                crate::LOOKUP_UNAVAILABLE_MESSAGE
            )));
        }
        let done = Done::new();
        state.inflight.insert(key.clone(), Arc::clone(&done));
        drop(state);

        // The blocking lookup's total budget, the same order as the poll path's
        // `timeout * 4` cap: a genuinely-unknown file must resolve in bounded
        // time instead of walking every provider at the full per-request
        // timeout. Reuses `child_scope` (the `Cancel::with_deadline`
        // machinery); no new subsystem. The child carries the parent's
        // cancellation, so its reason covers both.
        let budget = child_scope(cancel, self.inner.timeout * 4);
        let chain = self.fetch_media(&budget, q);
        let reason = budget.err();
        // Release the propagation watcher: this scope is finished.
        budget.cancel();

        let mut state = self.lock();
        state.inflight.remove(&key);
        let _guard = DoneGuard(done);
        let Some(media) = chain.media else {
            if let Some(reason) = reason {
                // A cancelled fetch persists nothing; restore the negative
                // cleared above so a failed forced refresh leaves prior
                // suppression intact.
                if force && let Some(prev) = prev_neg {
                    state.neg.insert(key.clone(), prev);
                }
                // The child scope carries BOTH the parent's cancellation and
                // its own total-budget deadline: only a dead parent is a user
                // cancel. A budget expiry is bounded and transient, so it is
                // lookup-unavailable, never `Cancelled` (which callers book
                // as not-found).
                if cancel.err().is_some() {
                    let parent = cancel.err().unwrap_or(reason);
                    return Err(Error::Cancelled(parent));
                }
                return Err(Error::unavailable(format!(
                    "metadata: lookup budget exceeded for {key:?} ({reason}): {}",
                    crate::LOOKUP_UNAVAILABLE_MESSAGE
                )));
            }
            if !chain.definitive {
                // Transient provider error/timeout: not a confirmed not-found,
                // so persist nothing and suppress nothing. A valid positive on
                // disk is left intact. The sentinel keeps this distinguishable
                // from a miss.
                if force && let Some(prev) = prev_neg {
                    state.neg.insert(key.clone(), prev);
                }
                return Err(match chain.err {
                    Some(fetch_err) => Error::unavailable(format!(
                        "metadata: transient lookup failure for {key:?} ({fetch_err}): {}",
                        crate::LOOKUP_UNAVAILABLE_MESSAGE
                    )),
                    None => Error::unavailable(format!(
                        "metadata: transient lookup failure for {key:?}: {}",
                        crate::LOOKUP_UNAVAILABLE_MESSAGE
                    )),
                });
            }
            state
                .neg
                .insert(key.clone(), SystemTime::now() + NEGATIVE_BACKOFF);
            drop(state);
            if let Err(err) = cache_store_negative(&self.inner.dir, &key) {
                self.log(&format!("metadata: cache store negative {key:?}: {err}"));
            }
            return Ok((Resolution::default(), false));
        };
        state.mem.insert(key.clone(), media.clone());
        state.neg.remove(&key);
        drop(state);
        if let Err(err) = cache_store(&self.inner.dir, &key, &media) {
            self.log(&format!("metadata: cache store {key:?}: {err}"));
        }
        Ok((resolve_from_media(&media, q), false))
    }

    /// `(*Resolver).CacheLive`: probes the resolver's own cache directory for a
    /// valid live answer to `key`.
    pub fn cache_live(&self, key: &str) -> bool {
        cache_live(&self.inner.dir, key)
    }

    /// `ProbeCache`: whether `key` has a live cache answer without ever fetching,
    /// warming the in-memory fast path so a later lookup for the same key is
    /// served without another store read. The verdict matches [`cache_live`]
    /// exactly; only the side effect differs. Warmed entries are marked probed so
    /// an entry removed after the probe is never served from memory.
    pub fn probe_cache(&self, key: &str) -> bool {
        if key.is_empty() {
            return false;
        }
        let Some(entry) = cache_load_entry(&self.inner.dir, key) else {
            return false;
        };
        let mut state = self.lock();
        if entry.not_found {
            let expiry =
                negative_mem_expiry(&entry.checked_at, SystemTime::now(), NEGATIVE_BACKOFF);
            state.neg.insert(key.to_string(), expiry);
            state.probed.insert(key.to_string());
            return true;
        }
        match entry.media {
            Some(media) => {
                state.mem.insert(key.to_string(), media);
                state.probed.insert(key.to_string());
                true
            }
            None => false,
        }
    }

    /// Registers an in-flight fetch for `key` (the test seam Go's direct
    /// `r.inflight[key] = done` provides).
    #[cfg(test)]
    pub(crate) fn set_inflight(&self, key: &str, done: Arc<Done>) {
        self.lock().inflight.insert(key.to_string(), done);
    }

    /// Whether `key` is suppressed in memory (Go's `_, ok := r.neg[key]`).
    #[cfg(test)]
    pub(crate) fn neg_suppressed(&self, key: &str) -> bool {
        self.lock().neg.contains_key(key)
    }

    /// The in-memory suppression expiry for `key`.
    #[cfg(test)]
    pub(crate) fn neg_expiry(&self, key: &str) -> Option<SystemTime> {
        self.lock().neg.get(key).copied()
    }
}

/// The cached positive that can answer `q`, with a probe-warmed entry
/// revalidated exactly as Go's `probedLive` does: an entry invalidated after the
/// probe must never be served from warmed memory.
fn cached_answer(state: &mut State, dir: &str, key: &str, q: &Query) -> Option<Media> {
    if !state
        .mem
        .get(key)
        .is_some_and(|media| media_answers(media, q))
    {
        return None;
    }
    if !probed_live(state, dir, key) {
        return None;
    }
    state.mem.get(key).cloned()
}

/// `probedLive`: revalidates a probe-warmed entry -- an entry invalidated after
/// the probe must never be served from warmed memory. Keys never warmed by
/// `ProbeCache` report true without any I/O.
fn probed_live(state: &mut State, dir: &str, key: &str) -> bool {
    if !state.probed.contains(key) {
        return true;
    }
    if cache_has(dir, key) {
        return true;
    }
    state.mem.remove(key);
    state.neg.remove(key);
    state.probed.remove(key);
    false
}

/// The outcome of one chain walk: Go's `(media, definitive, error)` triple.
struct Chain {
    media: Option<Media>,
    definitive: bool,
    err: Option<Error>,
}

impl Chain {
    fn found(media: Media) -> Chain {
        Chain {
            media: Some(media),
            definitive: true,
            err: None,
        }
    }

    fn missed() -> Chain {
        Chain {
            media: None,
            definitive: true,
            err: None,
        }
    }

    fn transient(err: Option<Error>) -> Chain {
        Chain {
            media: None,
            definitive: false,
            err,
        }
    }
}

impl Resolver {
    /// `fetchMedia`: the chain, then ONE bounded retry under the alternate title
    /// and ONLY after a definitive primary miss. A winning media whose artwork
    /// is unusable for Discord gets ONE bounded AniList backfill attempt
    /// ([`Resolver::backfill_artwork`]) before it is returned, so both the
    /// blocking and the background paths persist the stable cover.
    fn fetch_media(&self, cancel: &Cancel, q: &Query) -> Chain {
        let primary = self.fetch_chain(cancel, q);
        if let Some(media) = primary.media {
            return Chain::found(self.backfill_artwork(cancel, q, media));
        }
        // An unknown primary outcome (transient failure) must never be replaced
        // by a substituted alternate match that the caller then persists under
        // the ORIGINAL key, overwriting a correct cached positive.
        if !primary.definitive {
            return Chain::transient(primary.err);
        }
        let Some(alt) = alternate_query(q) else {
            return Chain {
                media: None,
                definitive: true,
                err: primary.err,
            };
        };
        if cancel.err().is_some() {
            return Chain {
                media: None,
                definitive: true,
                err: primary.err,
            };
        }
        // One bounded retry of the SAME source chain under the
        // separator-normalized title ("Show - Arc" -> "Show: Arc"). The sources
        // match against the alternate title, while the caller caches and
        // resolves under the ORIGINAL query key.
        let secondary = self.fetch_chain(cancel, &alt);
        if let Some(media) = secondary.media {
            return Chain::found(self.backfill_artwork(cancel, &alt, media));
        }
        // The primary was a definitive miss, so a transient alternate is still
        // unknown and surfaces the alternate's error; the primary error is nil
        // here.
        if !secondary.definitive {
            return Chain::transient(secondary.err);
        }
        Chain::missed()
    }

    /// `backfillArtwork`: when the winner's artwork is unusable for Discord and
    /// the winner is not AniList itself, try ONE direct AniList lookup (never
    /// `fetch_media`: no recursion) in a bounded child scope, and keep its
    /// cover only on an exact title match against the winner. Every other
    /// outcome -- a miss, timeout, parse error, a different title, or an
    /// unusable/absent cover -- leaves the artwork untouched, so the presence
    /// guards still degrade to `logo`. Never fails the lookup: the winner is
    /// always returned, with or without a backfilled cover.
    fn backfill_artwork(&self, cancel: &Cancel, q: &Query, mut media: Media) -> Media {
        if media.source == "anilist" || !artwork_unusable(&media.artwork) {
            return media;
        }
        let Some(anilist) = self.inner.sources.iter().find(|s| s.name() == "anilist") else {
            return media;
        };
        if cancel.err().is_some() {
            return media;
        }
        // Bounded by the resolver's per-request budget inside the existing
        // scope; released as soon as the find returns. A miss or a failure
        // keeps the winner untouched and is never cached negative here.
        let scope = child_scope(cancel, self.inner.timeout);
        let found = anilist.find(&scope, q);
        scope.cancel();
        let Ok(Some(hit)) = found else {
            return media;
        };
        // Exact-title match only: a looser match could backfill the wrong
        // work's poster (the 2015 `Dragon Ball Super` series over the 2026
        // `Dragon Ball Super: Beerus` season).
        if hit.title.trim() != media.title.trim() {
            return media;
        }
        // `AnilistSource` already picks `extra_large → large → banner_image`;
        // only a usable cover replaces the winner's.
        if artwork_unusable(&hit.artwork) {
            return media;
        }
        media.artwork = hit.artwork;
        media
    }

    /// `fetchChain`: iterate the ordered sources; an error is recorded and the
    /// walk continues (the FIRST error is kept), a `None` answer continues, a
    /// movie answer returns immediately when it is complete, and a series
    /// answer returns immediately on a number-exact episode match with a
    /// synopsis. Completeness beats order: an answer that matches but carries
    /// no synopsis is kept as the fallback while the walk continues, so a
    /// later provider carrying the episode AND a synopsis wins; only when no
    /// later source answers complete does the bare match settle. Cover and
    /// length are preferred, never gating (a missing cover degrades to the
    /// app logo downstream); year never gates at all.
    fn fetch_chain(&self, cancel: &Cancel, q: &Query) -> Chain {
        let mut saw_error = false;
        let mut saw_ok = false;
        let mut first_err: Option<Error> = None;
        let mut bare: Option<Media> = None;
        for source in self.ordered_sources(q) {
            match source.find(cancel, q) {
                Err(err) => {
                    self.log(&format!("metadata: source {} error: {err}", source.name()));
                    saw_error = true;
                    if first_err.is_none() {
                        first_err = Some(err);
                    }
                    continue;
                }
                Ok(None) => {
                    saw_ok = true;
                    continue;
                }
                Ok(Some(mut media)) => {
                    if media.source.is_empty() {
                        media.source = source.name().to_string();
                    }
                    if media.kind == Kind::MOVIE || q.kind == Kind::MOVIE {
                        if !media.synopsis.trim().is_empty() {
                            return Chain::found(media);
                        }
                        // A bare title must not settle the lookup when a later
                        // source may hold the synopsis: keep it and walk on.
                        self.log(&format!(
                            "metadata: source {} answers {:?} without a synopsis, trying next",
                            source.name(),
                            q.title
                        ));
                        if bare.is_none() {
                            bare = Some(media);
                        }
                        saw_ok = true;
                        continue;
                    }
                    if match_episode(&media.episodes, q).is_some() {
                        if episode_complete(&media, q) {
                            return Chain::found(media);
                        }
                        // The episode matches but neither it nor the show
                        // carries a synopsis: a later provider with the
                        // complete record wins; this stays the fallback.
                        self.log(&format!(
                            "metadata: source {} answers episode {} for {:?} without a synopsis, trying next",
                            source.name(),
                            q.episode,
                            q.title
                        ));
                        if bare.is_none() {
                            bare = Some(media);
                        }
                        saw_ok = true;
                        continue;
                    }
                    // The show matched but its Numbers lack the requested
                    // episode: fall through to the next source rather than
                    // caching/returning a wrong (positional) title. The leg
                    // did answer, so a lone error elsewhere must not turn
                    // this genuine not-found into Unavailable.
                    self.log(&format!(
                        "metadata: source {} lacks episode {} for {:?}, trying next",
                        source.name(),
                        q.episode,
                        q.title
                    ));
                    saw_ok = true;
                }
            }
        }
        if let Some(media) = bare {
            return Chain::found(media);
        }
        // One erroring leg never turns a genuine not-found into Unavailable:
        // only a chain with NO definitive leg outcome (every leg errored) is
        // transient. A single failed leg alongside real misses still stores
        // the definitive miss.
        if saw_error && !saw_ok {
            return Chain::transient(first_err);
        }
        if let Some(reason) = cancel.err() {
            return Chain::transient(Some(Error::Cancelled(reason)));
        }
        Chain::missed()
    }
}

/// `mediaAnswers`: movies answer on presence; series require a number-exact
/// episode match.
pub(crate) fn media_answers(media: &Media, q: &Query) -> bool {
    if media.kind == Kind::MOVIE || q.kind == Kind::MOVIE {
        return true;
    }
    match_episode(&media.episodes, q).is_some()
}

/// `episodeComplete`: the matched episode -- or, failing that, the show --
/// carries a synopsis. Mirrors [`resolve_from_media`]'s synopsis preference
/// (episode first, then show): a bare match keeps the walk going so a later
/// provider with the complete record wins.
pub(crate) fn episode_complete(media: &Media, q: &Query) -> bool {
    let Some(ep) = match_episode(&media.episodes, q) else {
        return false;
    };
    !ep.synopsis.trim().is_empty() || !media.synopsis.trim().is_empty()
}

/// `artworkUnusable`: the artwork can never survive Discord -- empty, not
/// http(s), over the 300-char `large_image` budget, or a signed/expiring URL
/// (Kitsu serves Backblaze URLs carrying `Expires=`).
fn artwork_unusable(artwork: &str) -> bool {
    let art = artwork.trim();
    if art.is_empty()
        || !(art.starts_with("http://") || art.starts_with("https://"))
        || art.len() > ARTWORK_BUDGET
    {
        return true;
    }
    art.contains("X-Amz-Signature")
        || art.contains("X-Amz-Credential")
        || art.contains("Expires=")
}

/// `matchEpisode` is number-exact: only `Number == q.Episode` (plus
/// `Season == q.Season` when `q.Season > 0`) answers. It never returns a
/// different episode via a positional index -- a wrong title is worse than no
/// title (AniList 21355 serves Numbers 51-66 for an episode-1 query).
///
/// Season-aware absolute fallback: a provider that numbers a multi-season
/// show's later episodes by their TOTAL number (S2E1 == 26) or flattens the
/// seasons it does carry into `season = 0` is still matchable. When no
/// season-exact episode exists, the wanted episode's total number is
/// `q.Episode + the count of episodes the SAME table reports for strictly
/// earlier seasons`, and the episode carrying that number answers. A table
/// carrying only one season (or only the wanted season) never establishes
/// such a total, so a wrong-season absolute list cannot fabricate a match.
///
/// A season-0 row never answers a wanted season above 0 when the table
/// already carries that season: a special numbered `earlier + q.Episode`
/// must not bind as the genuinely absent S3E1. The flattened case (no rows
/// at all for the wanted season) still answers through its season-0 row.
pub(crate) fn match_episode(eps: &[Episode], q: &Query) -> Option<Episode> {
    if eps.is_empty() || q.episode <= 0 {
        return None;
    }
    if q.season > 0 {
        if let Some(ep) = eps
            .iter()
            .find(|e| i64::from(e.season) == q.season && e.number == q.episode)
        {
            return Some(ep.clone());
        }
        let earlier = eps
            .iter()
            .filter(|e| e.season > 0 && i64::from(e.season) < q.season)
            .count();
        if earlier == 0 {
            return None;
        }
        let total = i32::try_from(earlier).unwrap_or(i32::MAX) + q.episode;
        let found = eps
            .iter()
            .find(|e| e.number == total && i64::from(e.season) <= q.season)
            .cloned()?;
        if found.season == 0 && eps.iter().any(|e| i64::from(e.season) == q.season) {
            return None;
        }
        return Some(found);
    }
    eps.iter().find(|e| e.number == q.episode).cloned()
}

/// `resolveFromMedia`: the user-facing [`Resolution`]. Artwork is ALWAYS the
/// series/movie poster -- episode stills never override (they look
/// unofficial) -- while the synopsis and length prefer the matched episode.
pub(crate) fn resolve_from_media(media: &Media, q: &Query) -> Resolution {
    let is_movie = media.kind == Kind::MOVIE || q.kind == Kind::MOVIE;
    let mut resolution = Resolution {
        series: media.title.clone(),
        episode: String::new(),
        season: q.season,
        number: q.episode,
        found: false,
        artwork: media.artwork.clone(),
        synopsis: cap_synopsis(&strip_html(&media.synopsis)),
        genres: media.genres.clone(),
        length: media.length.clone(),
        year: media.year,
        is_movie,
        buttons: Vec::new(),
    };
    if is_movie {
        resolution.season = 0;
        resolution.number = 0;
        resolution.found = true;
        resolution.buttons = resolution_buttons(media, None, false, true);
        return resolution;
    }
    if let Some(ep) = match_episode(&media.episodes, q) {
        resolution.season = i64::from(ep.season);
        resolution.number = ep.number;
        resolution.episode = ep.title.clone();
        let synopsis = first_non_empty(&[&ep.synopsis, &media.synopsis]);
        if !synopsis.is_empty() {
            resolution.synopsis = cap_synopsis(&strip_html(&synopsis));
        }
        if !ep.length.trim().is_empty() {
            resolution.length = ep.length.trim().to_string();
        }
        resolution.buttons = resolution_buttons(media, Some(&ep), true, false);
        resolution.found = true;
        return resolution;
    }
    // Every source lacked the requested Number: not found, so the play path
    // falls back to today's clean card. Never report a wrong title.
    resolution
}

/// `buttonOK`: a non-empty label (<= 32 runes) plus an http(s) URL forced to
/// https. Invalid pairs are dropped.
fn button_ok(label: &str, raw_url: &str) -> Option<Button> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 32 {
        return None;
    }
    let url = normalize_url(raw_url);
    if url.is_empty() {
        return None;
    }
    Some(Button {
        label: label.to_string(),
        url,
    })
}

/// `resolutionButtons`: the max-2 card buttons.
///
/// * movie: `[View Movie, MovieURL]` + `[View Trailer, TrailerURL]`
/// * anime (anilist): `[View Episode, streaming URL]` + `[View Anime, ShowURL]`
/// * series (tvmaze + others): `[View Episode, episode URL]` + `[View Show, ShowURL]`
fn resolution_buttons(
    media: &Media,
    ep: Option<&Episode>,
    has_ep: bool,
    is_movie: bool,
) -> Vec<Button> {
    let candidates: [(&str, String); 2] = if is_movie {
        [
            ("View Movie", media.movie_url.clone()),
            ("View Trailer", media.trailer_url.clone()),
        ]
    } else if media.source == "anilist" {
        let episode_url = if has_ep {
            ep.map(|ep| ep.url.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        [
            ("View Episode", episode_url),
            ("View Anime", media.show_url.clone()),
        ]
    } else {
        let mut episode_url = media.episode_url.clone();
        if let Some(ep) = ep
            && has_ep
            && !ep.url.trim().is_empty()
        {
            episode_url = ep.url.clone();
        }
        [
            ("View Episode", episode_url),
            ("View Show", media.show_url.clone()),
        ]
    };
    let mut out: Vec<Button> = Vec::with_capacity(2);
    for (label, url) in candidates {
        if let Some(button) = button_ok(label, &url) {
            out.push(button);
            if out.len() == 2 {
                break;
            }
        }
    }
    out
}

/// Go's `context.WithTimeout(parent, d)`: a scope that ends when the parent is
/// cancelled or when its own deadline passes.
///
/// `metadata-http`'s [`Cancel`] has no child constructor, so the propagation is a
/// bounded watcher thread that exits as soon as either scope ends -- the same
/// work Go's `propagateCancel` goroutine does. The caller cancels the child when
/// the fetch finishes, which ends the watcher immediately.
fn child_scope(parent: &Cancel, timeout: Duration) -> Cancel {
    let child = Cancel::with_deadline(timeout);
    if parent.is_cancelled() {
        child.cancel();
        return child;
    }
    let parent = parent.clone();
    let watcher = child.clone();
    std::thread::spawn(move || {
        loop {
            if parent.is_cancelled() {
                watcher.cancel();
                return;
            }
            if watcher.wait(Duration::from_millis(25)).is_err() {
                return;
            }
        }
    });
    child
}
