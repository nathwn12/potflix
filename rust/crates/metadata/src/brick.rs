//! `metadata::brick` -- T7-1 persistent per-field brick model, provenance,
//! identity, and cache.
//!
//! A *brick* is one cached fact about one field of one entity (a show, a
//! season, a movie, or one episode): the artwork URL, an episode title, a
//! synopsis, genres, and so on. Each brick carries its own field, value,
//! source, entity/episode identity, Found/Miss status, fetch timestamp, and a
//! free-text provenance note naming the provider record the value came from.
//!
//! # Cache file
//!
//! `%LOCALAPPDATA%\Potflix\brick-cache.json` ([`default_path`]) -- the same
//! convention as the overrides store (`metadata::overrides`), restated here
//! because `metadata` has no edge to `app-support`. The shape is
//! `{"version":1,"entities":[{"entity":{...},"bricks":[...]}]}` and
//! [`BrickCache::save`] always writes version [`BRICK_SCHEMA`]. An absent file
//! is an empty cache; a malformed file or a file whose `version` is not
//! [`BRICK_SCHEMA`] also loads as an empty cache (load-or-empty, never fatal),
//! and only the next successful [`BrickCache::save`] replaces it. Unknown JSON
//! fields are ignored, so a future writer can add fields without breaking
//! this reader.
//!
//! # Key
//!
//! One entity identity ([`EntityKey`]) plus one episode identity
//! ([`EpisodeKey`]). The entity key is show-level (T7-2-B): `series|<title>|
//! <year>` for series (no season -- S01 and S02 of one show share one bucket)
//! and `movie|<title>|<year>` for movies, derived by [`entity_key_for_query`]
//! from the resolved query, plus the external IDs available for it (TMDb,
//! TVDB, IMDb, AniList, TVmaze); the map bucket is the lowercased,
//! whitespace-collapsed id, and [`BrickCache::insert`] rejects a brick whose
//! entity id does not match the cache key it is stored under, so a wiring bug
//! in the future merge can never file one show's artwork under another show.
//! The episode key is `none` for entity-level fields (artwork, synopses,
//! genres, length, year, title) and `episode`/`absolute` for per-episode
//! fields (episode title, episode synopsis); [`insert`] rejects a brick whose
//! episode scope does not fit its field. The resolver's media cache
//! (`cache::cache_key`, per show-season) is untouched: brick identity is
//! deliberately coarser so T7-4 force invalidation is show-scoped.
//!
//! [`insert`]: BrickCache::insert
//!
//! # Status and retry
//!
//! A Found brick is fixed: it never expires, never re-fetches on its own, and
//! a later Miss for the same slot never evicts it (the merge records misses
//! blindly; the good value wins). A Miss is stored separately with its own
//! `fetched_at` plus a bounded `retry_after` ([`MISS_RETRY_AFTER`], one day):
//! [`BrickCache::lookup`] reports a fresh miss as `MissWaiting`, a miss whose
//! `retry_after` has passed (or is unreadable) as `RetryDue`, and a never-seen
//! slot as `Absent`. A miss is therefore never permanent -- at worst one day
//! quiet -- while a found value is never re-fetched behind the caller's back.
//! [`BrickCache::lookup_at`] is the explicit-clock core of [`lookup`] so tests
//! drive retry without hand-editing stored clocks.
//!
//! [`lookup`]: BrickCache::lookup
//!
//! # What this is not (T7-3+ owns the merge)
//!
//! This module stores and serves bricks and fills them from providers
//! ([`fill_entity`]); it performs no resolver/app/card wiring -- the
//! resolver/provider merge that serves cards from the cache is T7-3 and
//! later. The final API the merge builds on is: [`EntityKey`] +
//! [`EpisodeKey`] identity, [`Brick::found`]/[`Brick::miss`] constructors,
//!   [`BrickCache::insert`]/[`BrickCache::insert_many`] (key-checked,
//!   miss-never-evicts-found, manual beats provider),
//!   [`BrickCache::lookup`]/[`lookup_at`] (`Found`/`MissWaiting`/`RetryDue`/`Absent`),
//!   the per-field [`capacity`]/[`priority`] order plus [`extract_field_bricks`], the
//!   [`fill_entity`] pass and its [`FillReport`],
//!   the force-clear [`BrickCache::clear_entity`]/[`clear_episode`]/[`clear_field`],
//!   and atomic [`BrickCache::save`] plus load-or-empty [`load`]/[`load_from`].
//!   Each `insert`/`clear_*` saves in the same call; [`insert_many`] saves
//!   once for the whole batch (T7-4 drives the force/new-Match UI).
//!
//! [`lookup_at`]: BrickCache::lookup_at
//!
//! [`clear_episode`]: BrickCache::clear_episode
//! [`clear_field`]: BrickCache::clear_field
//!
//! # Field capability and priority (T7-2-B)
//!
//! Capability is truth, priority is policy. [`capacity`] names the fields one
//! source can answer for one kind (the frozen matrix; a wrong-kind source
//! answers nothing); [`priority`] returns the policy order for one kind and
//! field, and every entry is capable for the slot. [`fill_entity`] walks that
//! order against the supplied sources matched by [`BrickSource::provider_name`], then appends any
//! supplied capable source the policy list does not name (in supplied order,
//! last), so a new provider is never silently ignored. Providers the caller
//! did not supply are skipped, never fetched; incapable supplied sources are
//! skipped before any call and never count toward the miss rule.
//!
//! Policy order (pre-filter; `priority` drops the incapable entries):
//!
//! * artwork series: AniList, TVmaze, Tenrai, Kitsu, Cinemeta, Episodate,
//!   Bangumi, LiveChart.me, ANN.
//! * artwork movie: Cinemeta, JustWatch, Wikipedia.
//! * episode title: TVmaze, Tenrai, Kitsu, Cinemeta, Episodate, AniList.
//! * episode synopsis: TVmaze, Tenrai, Kitsu, Cinemeta.
//! * show synopsis: TVmaze, AniList, Tenrai, Kitsu, Cinemeta, Episodate,
//!   Bangumi, LiveChart.me, ANN.
//! * movie synopsis: Wikipedia, Cinemeta, JustWatch.
//! * genres series: TVmaze, AniList, Tenrai, Cinemeta.
//! * genres movie: Cinemeta, JustWatch.
//! * length series: TVmaze, AniList, Tenrai, Kitsu, ANN (Cinemeta carries no
//!   series runtime, so the policy never names it -- see [`capacity`]).
//! * length movie: Cinemeta, JustWatch.
//! * year series: Cinemeta, TVmaze, AniList, Tenrai, Kitsu, Episodate,
//!   Bangumi, LiveChart.me, ANN.
//! * year movie: Cinemeta, Wikipedia, JustWatch.
//! * title: no provider (titles are manual-only until the T7-4 Fix flow;
//!   [`extract_field_bricks`] never emits one).
//! * episode fields and show synopsis for movies answer `[]`; movie synopsis
//!   for series answers `[]`: the wrong-kind synopsis/episode slot is never
//!   attempted (see `applicable_fields`) and no priority entry may name a
//!   source incapable of the slot.
//!
//! # Deviations the tables had to make
//!
//! * TMDb: the SPEC's TMDb slots are unavailable -- there is no TMDb client
//!   in this repo, so no priority list names it and [`capacity`] answers `[]`
//!   for it. [`BrickSource::Tmdb`] stays reserved as the provenance slot a
//!   future client will fill, and [`BrickSource::for_provider`] answers `None`
//!   for `"tmdb"`.
//! * Wikipedia/JustWatch for series (and every series provider for movies):
//!   kind gating, not omission. Wikipedia's and JustWatch's `find` return
//!   `Ok(None)` for `Kind::SERIES` and the series providers' `find` returns
//!   `Ok(None)` for `Kind::MOVIE` (provider gating, unchanged by T7-2-B), so
//!   [`capacity`] answers `[]` for those pairs and [`fill_entity`] never
//!   calls them: an incapable gated `Ok(None)` must never become a 24 h Miss.
//!   If a future client answers the other kind, the capacity table -- not the
//!   gating -- is what changes.
//! * Cinemeta series length: the series `Media` carries `length: ""` (see
//!   `cinemeta::series_media`), so neither [`capacity`] nor the Length-series
//!   policy names it.
//!
//! # Atomic saves
//!
//! [`BrickCache::save`] follows the repo's existing atomic-write convention
//! (`metadata::overrides::Store::save`): marshal, create a uniquely named
//! `.tmp-<pid>-<nanos>-<seq>.json` beside the target, write, close, rename
//! over the target. A failed write or rename removes the temp file and leaves
//! the previous file exactly as it was. [`BrickCache::insert`] and the
//! `clear_*` calls save in the same call, and a failed save rolls their
//! in-memory change back, so the cache in memory always matches the last
//! successful save.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use metadata_core::types::{Kind, Media, Query};
use metadata_http::cancel::Cancel;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::resolver::Source;
use crate::timefmt::{format_rfc3339, now_rfc3339, parse_rfc3339};

/// The config directory name under the cache root: `%LOCALAPPDATA%\Potflix`.
const CONFIG_DIR_NAME: &str = "Potflix";

/// The cache file's name.
pub const BRICK_FILE_NAME: &str = "brick-cache.json";

/// The on-disk envelope version [`BrickCache::save`] always writes. A file
/// carrying any other version loads as an empty cache (see the module docs):
/// a future schema change cannot be silently misread, and the next save
/// replaces the old file.
pub const BRICK_SCHEMA: i32 = 1;

/// `missRetryAfter` bounds how long a stored Miss stays quiet: one day. A
/// miss is never permanent -- [`BrickCache::lookup`] reports it `RetryDue`
/// once this has passed -- while a Found brick is fixed and has no clock.
pub const MISS_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// `BrickField`: one cacheable fact. Entity-level fields (artwork, show and
/// movie synopses, genres, length, year, title) live under
/// [`EpisodeKey::None`]; per-episode fields (episode title, episode synopsis)
/// live under an `Episode`/`Absolute` key, enforced by [`BrickCache::insert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrickField {
    /// Series/movie poster URL (entity-level; the resolver always prefers the
    /// series/movie poster over episode stills).
    Artwork,
    /// One episode's title (episode-level).
    EpisodeTitle,
    /// One episode's plain-text summary (episode-level).
    EpisodeSynopsis,
    /// Series-level summary (entity-level).
    ShowSynopsis,
    /// Movie-level summary (entity-level).
    MovieSynopsis,
    /// `"Drama . Crime"`, as joined genre text (entity-level).
    Genres,
    /// Show/movie length (`"45 min"`, `"2h 19m"`) (entity-level).
    Length,
    /// Release/first-air year (entity-level).
    Year,
    /// Canonical series/movie title (entity-level).
    Title,
}

/// `BrickSource`: which provider (or hand) a brick came from. `Tmdb`
/// reserves the provenance slot for a TMDb client that does not exist yet
/// (see the TMDb deviation note in the module docs): no provider names itself
/// `tmdb` today. `Manual` marks a user correction, which the merge must
/// never overwrite with a provider answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrickSource {
    Anilist,
    Tvmaze,
    Tmdb,
    Wikipedia,
    Cinemeta,
    Manual,
    Tenrai,
    Kitsu,
    JustWatch,
    Episodate,
    Bangumi,
    Livechart,
    Ann,
}

impl BrickSource {
    /// The provider name as it appears in [`Media::source`] and in
    /// [`Source::name`]: the identity [`fill_entity`] uses to match a
    /// priority slot against a supplied source. `Tmdb` maps to `"tmdb"` and
    /// `Manual` to `"manual"`, neither of which any live [`Source`] reports.
    ///
    /// [`Source::name`]: crate::resolver::Source::name
    /// [`fill_entity`]: crate::brick::fill_entity
    pub fn provider_name(self) -> &'static str {
        match self {
            BrickSource::Anilist => "anilist",
            BrickSource::Tvmaze => "tvmaze",
            BrickSource::Tmdb => "tmdb",
            BrickSource::Wikipedia => "wikipedia",
            BrickSource::Cinemeta => "cinemeta",
            BrickSource::Manual => "manual",
            BrickSource::Tenrai => "tenrai",
            BrickSource::Kitsu => "kitsu",
            BrickSource::JustWatch => "justwatch",
            BrickSource::Episodate => "episodate",
            BrickSource::Bangumi => "bangumi",
            BrickSource::Livechart => "livechart",
            BrickSource::Ann => "ann",
        }
    }

    /// The brick source for a provider name: exhaustive over the eleven live
    /// providers. Unknown names -- including `"tmdb"` (no client exists) and
    /// `"manual"` (no [`Source`] reports it) -- answer `None`, so a foreign
    /// injected source never fills a brick slot.
    ///
    /// [`Source`]: crate::resolver::Source
    pub fn for_provider(name: &str) -> Option<BrickSource> {
        match name {
            "anilist" => Some(BrickSource::Anilist),
            "tvmaze" => Some(BrickSource::Tvmaze),
            "wikipedia" => Some(BrickSource::Wikipedia),
            "cinemeta" => Some(BrickSource::Cinemeta),
            "tenrai" => Some(BrickSource::Tenrai),
            "kitsu" => Some(BrickSource::Kitsu),
            "justwatch" => Some(BrickSource::JustWatch),
            "episodate" => Some(BrickSource::Episodate),
            "bangumi" => Some(BrickSource::Bangumi),
            "livechart" => Some(BrickSource::Livechart),
            "ann" => Some(BrickSource::Ann),
            _ => None,
        }
    }
}

/// `BrickStatus`: whether the fetch answered (`Found`, value fixed) or came
/// up empty (`Miss`, retry clock running). Only a definitive miss is stored
/// as `Miss`; a transient provider failure persists nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrickStatus {
    Found,
    Miss,
}

/// `BrickValue`: the typed payload of a Found brick. A Miss brick carries the
/// `Text("")` placeholder, which is meaningless -- callers must check
/// [`Brick::status`] before reading the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrickValue {
    /// Titles, synopses, artwork URLs, lengths, and the placeholder on a Miss.
    Text {
        #[serde(default)]
        text: String,
    },
    /// The [`BrickField::Year`] payload.
    Year {
        #[serde(default)]
        year: i32,
    },
    /// The [`BrickField::Genres`] payload, one name per entry.
    Genres {
        #[serde(default)]
        genres: Vec<String>,
    },
}

impl Default for BrickValue {
    /// The Miss placeholder: empty text, carrying no fact.
    fn default() -> BrickValue {
        BrickValue::Text {
            text: String::new(),
        }
    }
}

/// `EpisodeKey`: one episode identity. Entity-level bricks carry [`None`];
/// per-episode bricks carry the season/number pair or the absolute anime
/// number, exactly as the query that produced them did.
///
/// [`None`]: EpisodeKey::None
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EpisodeKey {
    /// An entity-level brick (artwork, synopses, genres, length, year,
    /// title): not about one episode.
    #[default]
    None,
    /// `season`/`number` addressing (`S02E03`).
    Episode {
        #[serde(default)]
        season: i64,
        #[serde(default)]
        number: i32,
    },
    /// Absolute anime numbering (episode 51 of a long runner).
    Absolute {
        #[serde(default)]
        number: i32,
    },
}

/// `EntityKey`: one entity identity -- a show/season/movie -- plus the
/// external IDs available for it. `id` is the canonical identity string (an
/// opaque caller-supplied key in T7-1; T7-2 derives it from the resolved
/// query); the external-ID fields are corroborating metadata, filled in as
/// available. `tvdb` covers the TV-database slot; every field is optional so
/// a brick is storable before any provider has answered.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EntityKey {
    /// Canonical identity, e.g. `"series|breaking bad|2008"` (show-level: no
    /// season) or `"movie|dune|2021"`.
    #[serde(default)]
    pub id: String,
    /// TMDb show/movie id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmdb: Option<i64>,
    /// TV-database show id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvdb: Option<i64>,
    /// TVmaze show id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tvmaze: Option<i64>,
    /// AniList media id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anilist: Option<i64>,
    /// IMDb title id (`"tt0903747"`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imdb: Option<String>,
}

impl EntityKey {
    /// A bare identity with no external IDs yet.
    pub fn new(id: impl Into<String>) -> EntityKey {
        EntityKey {
            id: id.into(),
            ..EntityKey::default()
        }
    }

    /// The map-bucket identity: lowercased, whitespace-collapsed -- the same
    /// normalisation as the media cache's title key, so `Breaking  BAD` and
    /// `breaking bad` are one entity.
    pub fn key(&self) -> String {
        crate::cache::cache_key_title(&self.id)
    }

    /// Fills external-ID slots that are still empty from `other` (same
    /// bucket): the merge learns IDs over time without ever clobbering one it
    /// already holds.
    fn absorb(&mut self, other: &EntityKey) {
        if self.tmdb.is_none() {
            self.tmdb = other.tmdb;
        }
        if self.tvdb.is_none() {
            self.tvdb = other.tvdb;
        }
        if self.tvmaze.is_none() {
            self.tvmaze = other.tvmaze;
        }
        if self.anilist.is_none() {
            self.anilist = other.anilist;
        }
        if self.imdb.is_none() {
            self.imdb = other.imdb.clone();
        }
    }
}

/// `Brick`: one cached fact. `value` is meaningful only when
/// `status == Found`; a Miss brick carries the empty-text placeholder plus
/// `retry_after`. `provenance` is free text naming the provider record the
/// value came from (e.g. `"tvmaze show 169 S01E01"`), so a later reader can
/// tell two same-valued bricks apart and the T7-2 merge can log what it
/// served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brick {
    /// Which fact this brick holds.
    pub field: BrickField,
    /// The fact (placeholder on a Miss -- check `status` first).
    #[serde(default)]
    pub value: BrickValue,
    /// Which provider (or hand) answered.
    pub source: BrickSource,
    /// The entity this fact is about; must match the cache key on [`insert`].
    ///
    /// [`insert`]: BrickCache::insert
    pub entity: EntityKey,
    /// The episode this fact is about (`None` for entity-level fields).
    #[serde(default)]
    pub episode: EpisodeKey,
    /// Found (fixed) or Miss (retry clock running).
    pub status: BrickStatus,
    /// RFC3339 UTC fetch instant, as Go's `time.Time` writes it.
    #[serde(rename = "fetchedAt", default)]
    pub fetched_at: String,
    /// RFC3339 UTC instant after which a Miss may be retried; `None` on a
    /// Found brick, which has no clock.
    #[serde(
        rename = "retryAfter",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub retry_after: Option<String>,
    /// Free-text provider-record note (e.g. `"tvmaze show 169 S01E01"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provenance: String,
}

impl Brick {
    /// A Found brick, stamped now: the value is fixed from here on (no TTL,
    /// never re-fetched on its own).
    pub fn found(
        field: BrickField,
        value: BrickValue,
        source: BrickSource,
        entity: EntityKey,
        episode: EpisodeKey,
        provenance: impl Into<String>,
    ) -> Brick {
        Brick {
            field,
            value,
            source,
            entity,
            episode,
            status: BrickStatus::Found,
            fetched_at: now_rfc3339(),
            retry_after: None,
            provenance: provenance.into(),
        }
    }

    /// A definitive Miss brick, stamped now with `retry_after` one
    /// [`MISS_RETRY_AFTER`] out: quiet for a bounded day, never permanent.
    /// The value is the empty-text placeholder.
    pub fn miss(
        field: BrickField,
        source: BrickSource,
        entity: EntityKey,
        episode: EpisodeKey,
        provenance: impl Into<String>,
    ) -> Brick {
        let now = SystemTime::now();
        Brick {
            field,
            value: BrickValue::default(),
            source,
            entity,
            episode,
            status: BrickStatus::Miss,
            fetched_at: format_rfc3339(now),
            retry_after: now.checked_add(MISS_RETRY_AFTER).map(format_rfc3339),
            provenance: provenance.into(),
        }
    }

    /// Whether this Miss may be retried at `now`: a Found brick never is, a
    /// Miss is once its `retry_after` has passed, and an unreadable or absent
    /// `retry_after` fails open to a retry (stale clocks must not pin a miss
    /// quiet forever).
    pub fn retry_due(&self, now: SystemTime) -> bool {
        if self.status == BrickStatus::Found {
            return false;
        }
        let Some(after) = self.retry_after.as_deref().and_then(parse_rfc3339) else {
            return true;
        };
        now >= after
    }
}

/// Whether `value` is the shape `field` serves: [`BrickField::Year`] takes
/// [`BrickValue::Year`], [`BrickField::Genres`] takes [`BrickValue::Genres`],
/// every other field takes [`BrickValue::Text`].
fn value_shape_fits(field: BrickField, value: &BrickValue) -> bool {
    matches!(
        (field, value),
        (BrickField::Year, BrickValue::Year { .. })
            | (BrickField::Genres, BrickValue::Genres { .. })
            | (
                BrickField::Artwork
                    | BrickField::EpisodeTitle
                    | BrickField::EpisodeSynopsis
                    | BrickField::ShowSynopsis
                    | BrickField::MovieSynopsis
                    | BrickField::Length
                    | BrickField::Title,
                BrickValue::Text { .. },
            )
    )
}

/// Whether a Found `value` carries no fact: blank text, the zero year, or no
/// usable genre name. A caller slip with such a value must never become a
/// permanent cached fact (a Miss brick's placeholder is meaningless by design
/// and skips this check).
fn found_value_is_empty(value: &BrickValue) -> bool {
    match value {
        BrickValue::Text { text } => text.trim().is_empty(),
        BrickValue::Year { year } => *year == 0,
        BrickValue::Genres { genres } => genres.iter().all(|name| name.trim().is_empty()),
    }
}

/// The per-brick storability checks [`BrickCache::insert`] and
/// [`BrickCache::insert_many`] share: the episode scope fits the field, and
/// a Found brick carries the shape its field serves with a non-empty value.
/// Miss bricks skip the value checks: their placeholder is meaningless by
/// design.
///
/// [`BrickCache::insert`]: BrickCache::insert
/// [`BrickCache::insert_many`]: BrickCache::insert_many
fn check_brick_storable(brick: &Brick) -> Result<(), Error> {
    let episode_scoped = field_is_episode_scoped(brick.field);
    let has_episode = !matches!(brick.episode, EpisodeKey::None);
    if episode_scoped != has_episode {
        return Err(Error::other(format!(
            "metadata: brick insert for field {:?}: episode scope mismatch",
            brick.field
        )));
    }
    if brick.status == BrickStatus::Found {
        if !value_shape_fits(brick.field, &brick.value) {
            return Err(Error::other(format!(
                "metadata: brick insert for field {:?}: value shape mismatch",
                brick.field
            )));
        }
        if found_value_is_empty(&brick.value) {
            return Err(Error::other(format!(
                "metadata: brick insert for field {:?}: empty Found value",
                brick.field
            )));
        }
    }
    Ok(())
}
/// Whether `field` is about one episode (`true`: episode title/synopsis) or
/// about the entity itself.
fn field_is_episode_scoped(field: BrickField) -> bool {
    matches!(
        field,
        BrickField::EpisodeTitle | BrickField::EpisodeSynopsis
    )
}

/// One entity's bricks on disk: the canonical identity plus its facts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct EntityEntry {
    #[serde(default)]
    entity: EntityKey,
    #[serde(default)]
    bricks: Vec<Brick>,
}

/// The on-disk envelope: `{"version":1,"entities":[...]}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BrickFile {
    #[serde(default)]
    version: i32,
    #[serde(default)]
    entities: Vec<EntityEntry>,
}

/// `BrickLookup`: the four-state answer of [`BrickCache::lookup`] -- a live
/// fact, a miss still inside its quiet day, a miss due for another attempt,
/// or a slot never seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrickLookup<'a> {
    /// The slot holds a Found brick: serve it, no fetch.
    Found(&'a Brick),
    /// The slot holds a Miss still inside [`MISS_RETRY_AFTER`]: stay quiet.
    MissWaiting(&'a Brick),
    /// The slot holds a Miss past its `retry_after` (or with an unreadable
    /// clock): refetch, then [`insert`] the answer.
    ///
    /// [`insert`]: BrickCache::insert
    RetryDue,
    /// The slot was never stored: fetch, then [`insert`] the answer.
    ///
    /// [`insert`]: BrickCache::insert
    Absent,
}

/// The persistent per-field brick cache: one JSON file
/// (`%LOCALAPPDATA%\Potflix\brick-cache.json`), one bucket per entity id,
/// one brick per (episode, field) slot. No network, no providers, no merge --
/// just key-checked storage with load-or-empty and atomic saves.
#[derive(Debug, Clone)]
pub struct BrickCache {
    path: PathBuf,
    entities: BTreeMap<String, EntityEntry>,
}

/// Loads the cache from [`default_path`]. An absent, malformed, or
/// foreign-version file is an empty cache, never an error; only a real I/O
/// failure (permissions, a path through a file) is reported.
pub fn load() -> Result<BrickCache, Error> {
    load_from(&default_path())
}

/// The explicit-path core of [`load`], so a caller with a custom location --
/// and every test -- never touches the real cache file.
///
/// A file naming the same bucket twice loads last-entry-wins: the later entry
/// replaces the earlier bucket wholesale (its bricks do not merge). Within one
/// entry, a duplicated (episode, field) slot resolves with Manual preferred
/// over any provider source regardless of order (a hand correction wins even
/// if the provider duplicate comes later); same-source duplicates -- and
/// provider-vs-provider duplicates -- stay deterministic last-wins. Buckets
/// left with no bricks are dropped, so a hand-written zero-brick bucket never
/// counts and is never re-saved.
pub fn load_from(path: &Path) -> Result<BrickCache, Error> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BrickCache::empty_at(path.to_path_buf()));
        }
        Err(err) => {
            return Err(Error::other(format!(
                "metadata: brick cache read {}: {err}",
                path.display()
            )));
        }
    };
    let file: BrickFile = match serde_json::from_slice(&data) {
        Ok(file) => file,
        // Malformed: reported-empty, never fatal, never wiped here; the next
        // successful save replaces the bad file.
        Err(_) => return Ok(BrickCache::empty_at(path.to_path_buf())),
    };
    if file.version != BRICK_SCHEMA {
        return Ok(BrickCache::empty_at(path.to_path_buf()));
    }
    let mut entities = BTreeMap::new();
    for mut entry in file.entities {
        let bucket = entry.entity.key();
        if bucket.is_empty() {
            continue;
        }
        // A brick filed under the wrong bucket is foreign data, not ours to
        // serve: drop it rather than misattribute a fact.
        entry.bricks.retain(|brick| brick.entity.key() == bucket);
        // A Manual duplicate wins over a provider one regardless of order, so a
        // hand correction in a hand-built file survives; same-source (and
        // provider-vs-provider) duplicates stay deterministic last-wins.
        let mut slots: BTreeMap<(EpisodeKey, BrickField), Brick> = BTreeMap::new();
        for brick in entry.bricks {
            let slot = (brick.episode, brick.field);
            match slots.get(&slot) {
                Some(old)
                    if old.source == BrickSource::Manual
                        && brick.source != BrickSource::Manual =>
                {
                    // Keep the Manual brick: a later provider duplicate must
                    // not evict a hand correction.
                }
                _ => {
                    slots.insert(slot, brick);
                }
            }
        }
        if slots.is_empty() {
            // No servable bricks: drop the bucket so a hand-written zero-brick
            // bucket never counts in `entity_count()` and is never re-saved.
            continue;
        }
        entry.bricks = slots.into_values().collect();
        entities.insert(bucket, entry);
    }
    Ok(BrickCache {
        path: path.to_path_buf(),
        entities,
    })
}

impl BrickCache {
    /// An empty cache at [`default_path`]: the recovery cache after [`load`]
    /// met a file it could not interpret.
    pub fn empty() -> BrickCache {
        BrickCache::empty_at(default_path())
    }

    /// The explicit-path core of [`BrickCache::empty`].
    pub fn empty_at(path: PathBuf) -> BrickCache {
        BrickCache {
            path,
            entities: BTreeMap::new(),
        }
    }

    /// The file this cache saves to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// How many entity buckets the cache holds.
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// How many bricks the cache holds across all entities.
    pub fn brick_count(&self) -> usize {
        self.entities.values().map(|entry| entry.bricks.len()).sum()
    }

    /// Writes the cache to its path, atomically: marshal, stage a
    /// uniquely-named temp file beside the target, rename over it. A failed
    /// write or rename removes the temp file and leaves the previous file
    /// exactly as it was.
    pub fn save(&self) -> Result<(), Error> {
        let dir = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir)?;
        let file = BrickFile {
            version: BRICK_SCHEMA,
            entities: self.entities.values().cloned().collect(),
        };
        let data = serde_json::to_vec(&file)?;
        let (temp_path, mut temp) = create_temp(&dir)?;
        if let Err(err) = temp.write_all(&data) {
            drop(temp);
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        drop(temp);
        if let Err(err) = std::fs::rename(&temp_path, &self.path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        Ok(())
    }

    /// Stores one brick under `entity`'s bucket, saving in the same call. A
    /// failed save rolls the in-memory change back, so memory always matches
    /// the last successful save.
    ///
    /// Rejected (no write, `Err`): an empty entity id; a brick whose entity
    /// id does not match `entity` (mixed-entity wiring bug); a brick whose
    /// episode scope does not fit its field (an episode field with
    /// [`EpisodeKey::None`], or an entity field with an episode key); a Found
    /// brick whose value shape does not fit its field (e.g. a year payload on
    /// artwork) or whose Found value is empty (blank text, the zero year, no
    /// usable genre) -- a caller slip must never become a permanent empty
    /// fact. Miss bricks skip the value checks: their placeholder is
    /// meaningless by design.
    ///
    /// A refused value is a silent no-op (`Ok`): a Miss for a slot that
    /// already holds a Found brick never evicts it (the good value wins and
    /// the blind miss-recording of the future merge can never evict it), and
    /// a provider Found for a slot that already holds a Manual Found never
    /// overwrites the hand correction (Manual overwrites provider freely). A
    /// refused brick still teaches the bucket the external IDs it carries:
    /// those save when anything new arrived.
    ///
    /// Anything else replaces the slot.
    pub fn insert(&mut self, entity: &EntityKey, brick: Brick) -> Result<(), Error> {
        let bucket = entity.key();
        if bucket.is_empty() {
            return Err(Error::other(
                "metadata: brick insert with an empty entity id",
            ));
        }
        if brick.entity.key() != bucket {
            return Err(Error::other(format!(
                "metadata: brick insert for {:?} under entity {:?}: entity mismatch",
                brick.entity.id, entity.id
            )));
        }
        check_brick_storable(&brick)?;
        let slot = (brick.episode, brick.field);
        let existing = self
            .entities
            .get(&bucket)
            .and_then(|entry| {
                entry
                    .bricks
                    .iter()
                    .find(|old| (old.episode, old.field) == slot)
            })
            .cloned();
        if let Some(old) = existing {
            let miss_over_found =
                old.status == BrickStatus::Found && brick.status == BrickStatus::Miss;
            let provider_over_manual = old.status == BrickStatus::Found
                && old.source == BrickSource::Manual
                && brick.status == BrickStatus::Found
                && brick.source != BrickSource::Manual;
            if miss_over_found || provider_over_manual {
                return self.absorb_ids_only(&bucket, entity, &brick.entity);
            }
        }
        let backup = self.entities.get(&bucket).cloned();
        let entry = self
            .entities
            .entry(bucket.clone())
            .or_insert_with(|| EntityEntry {
                entity: entity.clone(),
                bricks: Vec::new(),
            });
        entry.entity.absorb(entity);
        entry.entity.absorb(&brick.entity);
        if let Some(old) = entry
            .bricks
            .iter_mut()
            .find(|old| (old.episode, old.field) == slot)
        {
            *old = brick;
        } else {
            entry.bricks.push(brick);
        }
        match self.save() {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Some(previous) = backup {
                    self.entities.insert(bucket, previous);
                } else {
                    self.entities.remove(&bucket);
                }
                Err(err)
            }
        }
    }

    /// Stores a batch of bricks with ONE save for the whole batch (the
    /// [`fill_entity`] path): every brick carries its own entity, so one
    /// batch may span buckets. Each brick takes the same key/shape/precedence
    /// checks as [`insert`]: an empty entity id, a scope mismatch, a shape
    /// mismatch, or an empty Found value fails the WHOLE batch with nothing
    /// applied; a Miss over a Found slot, or a provider Found over a Manual
    /// Found, is refused per-brick exactly as in [`insert`] (external IDs
    /// still absorbed when they teach something new, folded into the one
    /// save). Returns how many bricks stored a slot. A batch that changed
    /// nothing -- every brick refused and no new IDs learned -- saves nothing
    /// and returns `Ok(0)`, even when saving is impossible. A failed save
    /// rolls every in-memory change back, so memory always matches the last
    /// successful save; an empty batch stores nothing and saves nothing
    /// (`Ok(0)`).
    ///
    /// [`insert`]: BrickCache::insert
    /// [`fill_entity`]: crate::brick::fill_entity
    pub fn insert_many(&mut self, bricks: Vec<Brick>) -> Result<usize, Error> {
        if bricks.is_empty() {
            return Ok(0);
        }
        for brick in &bricks {
            if brick.entity.key().is_empty() {
                return Err(Error::other(
                    "metadata: brick insert with an empty entity id",
                ));
            }
            check_brick_storable(brick)?;
        }
        let backup = self.entities.clone();
        let mut stored = 0usize;
        let mut changed = false;
        for brick in bricks {
            let bucket = brick.entity.key();
            let slot = (brick.episode, brick.field);
            let existing = self
                .entities
                .get(&bucket)
                .and_then(|entry| {
                    entry
                        .bricks
                        .iter()
                        .find(|old| (old.episode, old.field) == slot)
                })
                .cloned();
            if let Some(old) = existing {
                let miss_over_found =
                    old.status == BrickStatus::Found && brick.status == BrickStatus::Miss;
                let provider_over_manual = old.status == BrickStatus::Found
                    && old.source == BrickSource::Manual
                    && brick.status == BrickStatus::Found
                    && brick.source != BrickSource::Manual;
                if miss_over_found || provider_over_manual {
                    if let Some(entry) = self.entities.get_mut(&bucket) {
                        let mut next = entry.entity.clone();
                        next.absorb(&brick.entity);
                        if next != entry.entity {
                            entry.entity = next;
                            changed = true;
                        }
                    }
                    continue;
                }
            }
            let entry = self
                .entities
                .entry(bucket.clone())
                .or_insert_with(|| EntityEntry {
                    entity: brick.entity.clone(),
                    bricks: Vec::new(),
                });
            entry.entity.absorb(&brick.entity);
            if let Some(old) = entry
                .bricks
                .iter_mut()
                .find(|old| (old.episode, old.field) == slot)
            {
                *old = brick;
            } else {
                entry.bricks.push(brick);
            }
            stored += 1;
            changed = true;
        }
        if !changed {
            return Ok(0);
        }
        match self.save() {
            Ok(()) => Ok(stored),
            Err(err) => {
                self.entities = backup;
                Err(err)
            }
        }
    }

    /// Learns external IDs from a brick whose value the cache refused (a Miss
    /// over a Found slot, a provider Found over a Manual Found): absorbs into
    /// the bucket's stored identity, saving only when something new arrived,
    /// rolling that absorption back when the save fails. A bucket with nothing
    /// new to learn is a pure no-op (no write, `Ok`).
    fn absorb_ids_only(
        &mut self,
        bucket: &str,
        entity: &EntityKey,
        brick_entity: &EntityKey,
    ) -> Result<(), Error> {
        let Some(entry) = self.entities.get(bucket) else {
            return Ok(());
        };
        let mut next = entry.entity.clone();
        next.absorb(entity);
        next.absorb(brick_entity);
        if next == entry.entity {
            return Ok(());
        }
        let backup = entry.clone();
        self.entities
            .get_mut(bucket)
            .expect("bucket checked above")
            .entity = next;
        match self.save() {
            Ok(()) => Ok(()),
            Err(err) => {
                self.entities.insert(bucket.to_string(), backup);
                Err(err)
            }
        }
    }

    /// The cached answer for one slot: `Found` (serve it), `MissWaiting`
    /// (stay quiet), `RetryDue` (refetch), or `Absent` (fetch). Never an
    /// error; an empty entity id is `Absent`. The clock is `SystemTime::now()`;
    /// tests drive it through [`BrickCache::lookup_at`].
    pub fn lookup(
        &self,
        entity: &EntityKey,
        episode: &EpisodeKey,
        field: BrickField,
    ) -> BrickLookup<'_> {
        self.lookup_at(entity, episode, field, SystemTime::now())
    }

    /// The explicit-clock core of [`BrickCache::lookup`], so retry is testable
    /// without hand-editing stored clocks: `now` is the instant the quiet-day
    /// bound is measured against.
    pub fn lookup_at(
        &self,
        entity: &EntityKey,
        episode: &EpisodeKey,
        field: BrickField,
        now: SystemTime,
    ) -> BrickLookup<'_> {
        let bucket = entity.key();
        if bucket.is_empty() {
            return BrickLookup::Absent;
        }
        let Some(entry) = self.entities.get(&bucket) else {
            return BrickLookup::Absent;
        };
        let Some(brick) = entry
            .bricks
            .iter()
            .find(|brick| brick.episode == *episode && brick.field == field)
        else {
            return BrickLookup::Absent;
        };
        if brick.status == BrickStatus::Found {
            return BrickLookup::Found(brick);
        }
        if brick.retry_due(now) {
            BrickLookup::RetryDue
        } else {
            BrickLookup::MissWaiting(brick)
        }
    }

    /// Drops every brick of one entity (all episodes, all fields) and saves
    /// in the same call, reporting how many bricks went. An absent entity is
    /// a no-op (no write, 0); a failed save rolls the removal back.
    pub fn clear_entity(&mut self, entity: &EntityKey) -> Result<usize, Error> {
        let bucket = entity.key();
        let Some(previous) = self.entities.remove(&bucket) else {
            return Ok(0);
        };
        let removed = previous.bricks.len();
        match self.save() {
            Ok(()) => Ok(removed),
            Err(err) => {
                self.entities.insert(bucket, previous);
                Err(err)
            }
        }
    }

    /// Drops one episode's bricks of one entity (entity-level `None` bricks
    /// survive) and saves in the same call, reporting how many bricks went.
    /// An absent entity or episode is a no-op (no write, 0); a failed save
    /// rolls the removal back. An emptied bucket is dropped before the save,
    /// so the file never keeps a zero-brick bucket.
    pub fn clear_episode(
        &mut self,
        entity: &EntityKey,
        episode: &EpisodeKey,
    ) -> Result<usize, Error> {
        let bucket = entity.key();
        let Some(previous) = self.entities.get(&bucket).cloned() else {
            return Ok(0);
        };
        let mut next = previous.clone();
        let before = next.bricks.len();
        next.bricks.retain(|brick| brick.episode != *episode);
        let removed = before - next.bricks.len();
        if removed == 0 {
            return Ok(0);
        }
        if next.bricks.is_empty() {
            self.entities.remove(&bucket);
        } else {
            self.entities.insert(bucket.clone(), next);
        }
        match self.save() {
            Ok(()) => Ok(removed),
            Err(err) => {
                self.entities.insert(bucket, previous);
                Err(err)
            }
        }
    }

    /// Drops one slot (one entity, one episode, one field) and saves in the
    /// same call. Absent is a no-op (no write, `false`); a failed save rolls
    /// the removal back. A bucket left with no bricks is dropped before the
    /// save, so the file never keeps a zero-brick bucket.
    pub fn clear_field(
        &mut self,
        entity: &EntityKey,
        episode: &EpisodeKey,
        field: BrickField,
    ) -> Result<bool, Error> {
        let bucket = entity.key();
        let Some(previous) = self.entities.get(&bucket).cloned() else {
            return Ok(false);
        };
        let mut next = previous.clone();
        let before = next.bricks.len();
        next.bricks
            .retain(|brick| !(brick.episode == *episode && brick.field == field));
        if next.bricks.len() == before {
            return Ok(false);
        }
        if next.bricks.is_empty() {
            self.entities.remove(&bucket);
        } else {
            self.entities.insert(bucket.clone(), next);
        }
        match self.save() {
            Ok(()) => Ok(true),
            Err(err) => {
                self.entities.insert(bucket, previous);
                Err(err)
            }
        }
    }
}

/// The store path: `%LOCALAPPDATA%\Potflix\brick-cache.json`, falling back to
/// `<dir of the executable>\brick-cache.json` -- the same convention as
/// `metadata::overrides::default_path` (which this module cannot call, and
/// which this crate's lack of an `app-support` edge keeps duplicated in both
/// places by design).
pub fn default_path() -> PathBuf {
    if let Ok(root) = std::env::var("LOCALAPPDATA")
        && !root.is_empty()
    {
        return Path::new(&root).join(CONFIG_DIR_NAME).join(BRICK_FILE_NAME);
    }
    exe_dir_or_dot().join(BRICK_FILE_NAME)
}

/// `go\main.go`'s `exeDir()` fallback: the running binary's directory, or
/// `"."` when it cannot be read, so a cache path always resolves.
fn exe_dir_or_dot() -> PathBuf {
    match std::env::current_exe() {
        Ok(exe) => exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        Err(_) => PathBuf::from("."),
    }
}

/// `os.CreateTemp(dir, ".tmp-*.json")`: a uniquely named file in `dir`,
/// created exclusively and retried on collision -- the overrides store's
/// temp-file convention (`.tmp-<pid>-<nanos>-<seq>.json`), so an atomic write
/// here looks like every other atomic write in the repo.
fn create_temp(dir: &Path) -> std::io::Result<(PathBuf, File)> {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..64 {
        let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos())
            .unwrap_or(0);
        let name = format!(".tmp-{}-{nanos}-{sequence}.json", std::process::id());
        let candidate = dir.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique temp file for the atomic write",
    ))
}

// --- T7-2: identity, extraction, priority, and the fill engine ---------------

/// `entityKeyForQuery`: the show-level target identity for a query --
/// `series|<title>|<year>` for series (no season: every season of one show
/// shares one entity bucket) and `movie|<title>|<year>` for movies, with no
/// external IDs yet. The prefix comes from the caller's RESOLVED `kind`, not
/// `q.kind` (which may still be `UNKNOWN`): [`fill_entity`] resolves the kind
/// and passes it in. Providers corroborate IDs later through
/// [`with_media_id`]; the key itself never carries one, so two queries that
/// differ only by what a provider already knows still address one entity.
/// This is deliberately coarser than [`cache::cache_key`] (per show-season):
/// the resolver's media cache stays untouched, while brick show-level facts
/// are reused across episodes and T7-4 force invalidation is show-scoped.
///
/// [`cache::cache_key`]: crate::cache::cache_key
pub fn entity_key_for_query(q: &Query, kind: Kind) -> EntityKey {
    let title = crate::cache::cache_key_title(&q.title);
    if kind == Kind::MOVIE {
        EntityKey::new(format!("movie|{title}|{}", q.year))
    } else {
        EntityKey::new(format!("series|{title}|{}", q.year))
    }
}

/// Maps a provider answer's `source`/`source_id` into the right [`EntityKey`]
/// slot: `cinemeta` carries an IMDb id (`tt...`) into `imdb`, `tvmaze` a
/// numeric id into `tvmaze`, `anilist` a numeric id into `anilist`. Every
/// other provider (tenrai/kitsu/episodate/bangumi/livechart/ann/wikipedia/
/// justwatch) has no `EntityKey` slot and absorbs nothing. A slot that
/// already holds an ID is never overwritten: the merge learns IDs over time
/// without ever clobbering one it holds.
pub fn with_media_id(key: &EntityKey, media: &Media) -> EntityKey {
    let mut out = key.clone();
    match media.source.as_str() {
        "cinemeta" => {
            let id = media.source_id.trim();
            if out.imdb.is_none() && id.starts_with("tt") && id.len() > 2 {
                out.imdb = Some(id.to_string());
            }
        }
        "tvmaze" => {
            if out.tvmaze.is_none()
                && let Ok(id) = media.source_id.trim().parse::<i64>()
                && id > 0
            {
                out.tvmaze = Some(id);
            }
        }
        "anilist" => {
            if out.anilist.is_none()
                && let Ok(id) = media.source_id.trim().parse::<i64>()
                && id > 0
            {
                out.anilist = Some(id);
            }
        }
        _ => {}
    }
    out
}

/// The episode identity for a query: `Episode{season,number}` when both are
/// meaningful, `Absolute{number}` when the query carries absolute anime
/// numbering, `None` for movies and for queries without an episode. An
/// absolute number NEVER becomes `Episode{season: 0, ...}` (no `S00`): the
/// key keeps the query's own addressing, and extraction matches it
/// number-exact.
pub fn episode_key_for_query(q: &Query) -> EpisodeKey {
    if q.kind == Kind::MOVIE {
        return EpisodeKey::None;
    }
    if q.absolute && q.episode > 0 {
        return EpisodeKey::Absolute { number: q.episode };
    }
    if q.season > 0 && q.episode > 0 {
        return EpisodeKey::Episode {
            season: q.season,
            number: q.episode,
        };
    }
    EpisodeKey::None
}

/// Whether `year` names a plausible release year: exactly the
/// `wikipedia::wiki_year` bound (1900..=2100), so extraction and the provider
/// agree on what counts as a year. The zero year (unknown) and garbage never
/// pass.
fn is_plausible_year(year: i32) -> bool {
    (1900..=2100).contains(&year)
}

/// Pure per-provider field extraction: the `(field, value)` pairs one
/// provider answer yields for one query, in a fixed order (artwork, episode
/// title, episode synopsis, synopsis, genres, length, year). Only
/// provider-returned values appear here -- nothing is fabricated from the
/// query or the title -- and every value is non-empty (blank text, an
/// implausible year, no usable genre name yield nothing), so whatever this
/// returns is storable as a Found brick.
///
/// * artwork: `media.artwork` when non-empty (always the series/movie
///   poster; episode stills never override).
/// * episode title/synopsis: only for the episode
///   [`resolver::match_episode`] names for the query -- season-exact, with
///   the resolver's own season-aware absolute fallback and season-0 specials
///   guard, shared by construction (the call below IS the resolver's
///   function, not a copy). An episode the table does not carry yields
///   nothing. AniList episode rows carry titles only (their synopsis is
///   empty), so they naturally yield no synopsis.
/// * show vs movie synopsis: chosen by kind exactly as the resolver serves
///   it (a movie answer or a movie query takes `MovieSynopsis`); a series
///   synopsis never lands in `movie_synopsis` or vice versa -- and
///   [`fill_entity`] additionally skips any answer whose `kind` contradicts
///   the resolved kind, so a wrong-kind synopsis never reaches this choice.
/// * genres: `media.genres` split on the `join_genres` middle-dot
///   separator, blank names dropped; empty when the provider carries none.
/// * length: `media.length` when non-empty.
/// * year: `media.year` when plausible.
/// * title: never (titles are manual-only until the T7-4 Fix flow).
///
/// Provenance is attached at insert time by [`fill_entity`], where the winning
/// source is known.
pub fn extract_field_bricks(media: &Media, q: &Query) -> Vec<(BrickField, BrickValue)> {
    let mut out = Vec::new();
    let artwork = media.artwork.trim();
    if !artwork.is_empty() {
        out.push((
            BrickField::Artwork,
            BrickValue::Text {
                text: artwork.to_string(),
            },
        ));
    }
    if let Some(episode) = crate::resolver::match_episode(&media.episodes, q) {
        let title = episode.title.trim();
        if !title.is_empty() {
            out.push((
                BrickField::EpisodeTitle,
                BrickValue::Text {
                    text: title.to_string(),
                },
            ));
        }
        let synopsis = episode.synopsis.trim();
        if !synopsis.is_empty() {
            out.push((
                BrickField::EpisodeSynopsis,
                BrickValue::Text {
                    text: synopsis.to_string(),
                },
            ));
        }
    }
    let synopsis = media.synopsis.trim();
    if !synopsis.is_empty() {
        let field = if media.kind == Kind::MOVIE || q.kind == Kind::MOVIE {
            BrickField::MovieSynopsis
        } else {
            BrickField::ShowSynopsis
        };
        out.push((
            field,
            BrickValue::Text {
                text: synopsis.to_string(),
            },
        ));
    }
    let genres: Vec<String> = media
        .genres
        .split('·')
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    if !genres.is_empty() {
        out.push((BrickField::Genres, BrickValue::Genres { genres }));
    }
    let length = media.length.trim();
    if !length.is_empty() {
        out.push((
            BrickField::Length,
            BrickValue::Text {
                text: length.to_string(),
            },
        ));
    }
    if is_plausible_year(media.year) {
        out.push((BrickField::Year, BrickValue::Year { year: media.year }));
    }
    out
}

/// Capability matrix (T7-2-B, frozen): the fields one source can answer for
/// one kind. A wrong-kind source answers nothing (its `find` gates with
/// `Ok(None)` before any fetch); `Tmdb` is reserved (no client exists) and
/// `Manual` is never fetched, so both answer nothing for every kind. An
/// `UNKNOWN` kind buckets like series (only movies take the movie rows).
///
/// * tvmaze (series): Artwork, EpisodeTitle, EpisodeSynopsis, ShowSynopsis,
///   Genres, Length, Year.
/// * anilist (series): Artwork, EpisodeTitle, ShowSynopsis, Genres, Length,
///   Year (no EpisodeSynopsis: its rows carry titles only).
/// * tenrai (series): all seven series fields.
/// * kitsu (series): Artwork, EpisodeTitle, EpisodeSynopsis, ShowSynopsis,
///   Length, Year (no Genres).
/// * cinemeta (series): Artwork, EpisodeTitle, EpisodeSynopsis, ShowSynopsis,
///   Genres, Year (no series Length: the series `Media` carries `""`).
/// * cinemeta (movie): Artwork, MovieSynopsis, Genres, Length, Year.
/// * wikipedia (movie): Artwork, MovieSynopsis, Year.
/// * justwatch (movie): Artwork, MovieSynopsis, Genres, Length, Year.
/// * episodate (series): Artwork, EpisodeTitle, ShowSynopsis, Year.
/// * bangumi (series): Artwork, ShowSynopsis, Year.
/// * livechart (series): Artwork, ShowSynopsis, Year.
/// * ann (series): Artwork, ShowSynopsis, Length, Year.
pub fn capacity(source: BrickSource, kind: Kind) -> &'static [BrickField] {
    let movie = kind == Kind::MOVIE;
    match (source, movie) {
        (BrickSource::Tvmaze, false) => &[
            BrickField::Artwork,
            BrickField::EpisodeTitle,
            BrickField::EpisodeSynopsis,
            BrickField::ShowSynopsis,
            BrickField::Genres,
            BrickField::Length,
            BrickField::Year,
        ],
        (BrickSource::Anilist, false) => &[
            BrickField::Artwork,
            BrickField::EpisodeTitle,
            BrickField::ShowSynopsis,
            BrickField::Genres,
            BrickField::Length,
            BrickField::Year,
        ],
        (BrickSource::Tenrai, false) => &[
            BrickField::Artwork,
            BrickField::EpisodeTitle,
            BrickField::EpisodeSynopsis,
            BrickField::ShowSynopsis,
            BrickField::Genres,
            BrickField::Length,
            BrickField::Year,
        ],
        (BrickSource::Kitsu, false) => &[
            BrickField::Artwork,
            BrickField::EpisodeTitle,
            BrickField::EpisodeSynopsis,
            BrickField::ShowSynopsis,
            BrickField::Length,
            BrickField::Year,
        ],
        (BrickSource::Cinemeta, false) => &[
            BrickField::Artwork,
            BrickField::EpisodeTitle,
            BrickField::EpisodeSynopsis,
            BrickField::ShowSynopsis,
            BrickField::Genres,
            BrickField::Year,
        ],
        (BrickSource::Cinemeta, true) => &[
            BrickField::Artwork,
            BrickField::MovieSynopsis,
            BrickField::Genres,
            BrickField::Length,
            BrickField::Year,
        ],
        (BrickSource::Wikipedia, true) => &[
            BrickField::Artwork,
            BrickField::MovieSynopsis,
            BrickField::Year,
        ],
        (BrickSource::JustWatch, true) => &[
            BrickField::Artwork,
            BrickField::MovieSynopsis,
            BrickField::Genres,
            BrickField::Length,
            BrickField::Year,
        ],
        (BrickSource::Episodate, false) => &[
            BrickField::Artwork,
            BrickField::EpisodeTitle,
            BrickField::ShowSynopsis,
            BrickField::Year,
        ],
        (BrickSource::Bangumi, false) => &[
            BrickField::Artwork,
            BrickField::ShowSynopsis,
            BrickField::Year,
        ],
        (BrickSource::Livechart, false) => &[
            BrickField::Artwork,
            BrickField::ShowSynopsis,
            BrickField::Year,
        ],
        (BrickSource::Ann, false) => &[
            BrickField::Artwork,
            BrickField::ShowSynopsis,
            BrickField::Length,
            BrickField::Year,
        ],
        _ => &[],
    }
}

/// Whether `source` can answer `field` for `kind`: the one gate
/// [`fill_entity`] applies before any call.
fn is_capable(source: BrickSource, kind: Kind, field: BrickField) -> bool {
    capacity(source, kind).contains(&field)
}

/// Artwork policy for series/anime queries.
static POLICY_ARTWORK_SERIES: &[BrickSource] = &[
    BrickSource::Anilist,
    BrickSource::Tvmaze,
    BrickSource::Tenrai,
    BrickSource::Kitsu,
    BrickSource::Cinemeta,
    BrickSource::Episodate,
    BrickSource::Bangumi,
    BrickSource::Livechart,
    BrickSource::Ann,
];

/// Artwork policy for movie queries.
static POLICY_ARTWORK_MOVIE: &[BrickSource] = &[
    BrickSource::Cinemeta,
    BrickSource::JustWatch,
    BrickSource::Wikipedia,
];

/// Episode-title policy: tables with real per-episode titles first, AniList
/// (mislabeled absolute tables) last.
static POLICY_EPISODE_TITLE: &[BrickSource] = &[
    BrickSource::Tvmaze,
    BrickSource::Tenrai,
    BrickSource::Kitsu,
    BrickSource::Cinemeta,
    BrickSource::Episodate,
    BrickSource::Anilist,
];

/// Episode-synopsis policy: only the four tables that carry one.
static POLICY_EPISODE_SYNOPSIS: &[BrickSource] = &[
    BrickSource::Tvmaze,
    BrickSource::Tenrai,
    BrickSource::Kitsu,
    BrickSource::Cinemeta,
];

/// Show-synopsis policy: every series-capable provider with a synopsis.
static POLICY_SHOW_SYNOPSIS: &[BrickSource] = &[
    BrickSource::Tvmaze,
    BrickSource::Anilist,
    BrickSource::Tenrai,
    BrickSource::Kitsu,
    BrickSource::Cinemeta,
    BrickSource::Episodate,
    BrickSource::Bangumi,
    BrickSource::Livechart,
    BrickSource::Ann,
];

/// Movie-synopsis policy: the movie chain in resolver order.
static POLICY_MOVIE_SYNOPSIS: &[BrickSource] = &[
    BrickSource::Wikipedia,
    BrickSource::Cinemeta,
    BrickSource::JustWatch,
];

/// Genre policy, series leg.
static POLICY_GENRES_SERIES: &[BrickSource] = &[
    BrickSource::Tvmaze,
    BrickSource::Anilist,
    BrickSource::Tenrai,
    BrickSource::Cinemeta,
];

/// Genre policy, movie leg.
static POLICY_GENRES_MOVIE: &[BrickSource] = &[BrickSource::Cinemeta, BrickSource::JustWatch];

/// Length policy, series leg: Cinemeta carries no series runtime (see
/// [`capacity`]), so the policy never names it.
static POLICY_LENGTH_SERIES: &[BrickSource] = &[
    BrickSource::Tvmaze,
    BrickSource::Anilist,
    BrickSource::Tenrai,
    BrickSource::Kitsu,
    BrickSource::Ann,
];

/// Length policy, movie leg.
static POLICY_LENGTH_MOVIE: &[BrickSource] = &[BrickSource::Cinemeta, BrickSource::JustWatch];

/// Year policy, series leg.
static POLICY_YEAR_SERIES: &[BrickSource] = &[
    BrickSource::Cinemeta,
    BrickSource::Tvmaze,
    BrickSource::Anilist,
    BrickSource::Tenrai,
    BrickSource::Kitsu,
    BrickSource::Episodate,
    BrickSource::Bangumi,
    BrickSource::Livechart,
    BrickSource::Ann,
];

/// Year policy, movie leg.
static POLICY_YEAR_MOVIE: &[BrickSource] = &[
    BrickSource::Cinemeta,
    BrickSource::Wikipedia,
    BrickSource::JustWatch,
];

/// `priority`: the policy order for one kind and field, most capable first.
/// Every entry is capable for the slot (see [`capacity`]); the
/// [`fill_entity`] walk keeps the capability check as defense in depth.
/// [`fill_entity`] walks this against the supplied sources, then appends any
/// supplied capable source the policy does not name (last); providers the
/// caller did not supply are skipped, never fetched. Title answers an empty
/// list: titles are manual-only until the T7-4 Fix flow. Wrong-kind synopsis
/// and episode slots answer `[]`: a movie never walks episode/show slots and
/// a series never walks the movie slot.
pub fn priority(kind: Kind, field: BrickField) -> &'static [BrickSource] {
    let movie = kind == Kind::MOVIE;
    match (field, movie) {
        (BrickField::Artwork, true) => POLICY_ARTWORK_MOVIE,
        (BrickField::Artwork, false) => POLICY_ARTWORK_SERIES,
        (BrickField::EpisodeTitle, false) => POLICY_EPISODE_TITLE,
        (BrickField::EpisodeSynopsis, false) => POLICY_EPISODE_SYNOPSIS,
        (BrickField::ShowSynopsis, false) => POLICY_SHOW_SYNOPSIS,
        (BrickField::MovieSynopsis, true) => POLICY_MOVIE_SYNOPSIS,
        (BrickField::Genres, true) => POLICY_GENRES_MOVIE,
        (BrickField::Genres, false) => POLICY_GENRES_SERIES,
        (BrickField::Length, true) => POLICY_LENGTH_MOVIE,
        (BrickField::Length, false) => POLICY_LENGTH_SERIES,
        (BrickField::Year, true) => POLICY_YEAR_MOVIE,
        (BrickField::Year, false) => POLICY_YEAR_SERIES,
        _ => &[],
    }
}

/// `FillReport`: one [`fill_entity`] pass over one entity: the target
/// identity, the fields that gained a Found brick, the fields that proved
/// definitively absent (a bounded Miss brick now guards them), the fields
/// left `Absent` because at least one provider failed transiently, and the
/// fields no capable supplied source covers at all.
///
/// [`fill_entity`]: crate::brick::fill_entity
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FillReport {
    /// The target identity the pass filled (bare: no external IDs).
    pub key: EntityKey,
    /// Fields that gained a Found brick this pass.
    pub found: Vec<BrickField>,
    /// Fields that proved definitively absent (Miss bricks stored).
    pub missed: Vec<BrickField>,
    /// Fields with at least one provider error (field, provider errors
    /// joined). A win does NOT clear this: when a higher-priority provider
    /// errored and a lower-priority one later won the field, the field is
    /// both `found` and `errored`, so the outage stays visible.
    pub errored: Vec<(BrickField, String)>,
    /// Fields with zero capable supplied sources: nothing was attempted,
    /// nothing stored, no retry clock. T7-3 uses this to diagnose a
    /// misconfigured chain rather than retrying blindly.
    pub unsupported: Vec<BrickField>,
}

/// The fields one fill pass attempts for a RESOLVED kind: entity-level facts
/// always, episode fields only for series fills that address one episode, the
/// synopsis slot chosen by kind. Title is never attempted (manual-only).
fn applicable_fields(kind: Kind, episode_key: EpisodeKey) -> Vec<BrickField> {
    let mut fields = vec![BrickField::Artwork];
    if kind != Kind::MOVIE && !matches!(episode_key, EpisodeKey::None) {
        fields.push(BrickField::EpisodeTitle);
        fields.push(BrickField::EpisodeSynopsis);
    }
    fields.push(if kind == Kind::MOVIE {
        BrickField::MovieSynopsis
    } else {
        BrickField::ShowSynopsis
    });
    fields.push(BrickField::Genres);
    fields.push(BrickField::Length);
    fields.push(BrickField::Year);
    fields
}

/// The episode scope one field stores under: episode fields under the
/// query's episode key, entity-level fields under `None` (enforced again by
/// [`BrickCache::insert`]).
///
/// [`BrickCache::insert`]: BrickCache::insert
fn brick_episode_for(field: BrickField, episode_key: EpisodeKey) -> EpisodeKey {
    if field_is_episode_scoped(field) {
        episode_key
    } else {
        EpisodeKey::None
    }
}

/// The free-text provider-record note on a filled brick: the provider name,
/// the record title, and the episode address for per-episode fields.
fn provenance_for(
    source: BrickSource,
    media: &Media,
    field: BrickField,
    episode_key: &EpisodeKey,
) -> String {
    let title = media.title.trim();
    if !field_is_episode_scoped(field) {
        return format!("{} {title}", source.provider_name());
    }
    let address = match *episode_key {
        EpisodeKey::Episode { season, number } => format!("S{season:02}E{number:02}"),
        EpisodeKey::Absolute { number } => format!("#{number}"),
        EpisodeKey::None => String::new(),
    };
    if address.is_empty() {
        format!("{} {title}", source.provider_name())
    } else {
        format!("{} {title} {address}", source.provider_name())
    }
}

/// The cache-filling pass: for every applicable field of the query's target
/// entity, serve what the cache already holds and fetch only the rest.
///
/// `kind` is the caller's RESOLVED kind (`SERIES` or `MOVIE`; `UNKNOWN` is
/// rejected with an error -- the caller resolves it before calling). The
/// entity-key prefix, the applicable fields, and every capability check derive
/// from it, never from `q.kind` (which may still be `UNKNOWN`). The episode
/// key is derived inside from `q` ([`episode_key_for_query`], forced to
/// `None` for movies): callers never supply it, so no caller can file an
/// `S00` slot or place an episode under another season's address.
///
/// * A slot holding `Found` or a fresh miss (`MissWaiting`) is skipped: no
///   fetch, no write.
/// * A `RetryDue`/`Absent` slot walks the field's [`priority`] list against
///   the supplied sources mapped through [`BrickSource::for_provider`]
///   (unknown names are skipped, never consulted), then any supplied capable
///   source the policy does not name (in supplied order, last). Incapable
///   sources ([`capacity`]) are skipped BEFORE any call and never count: a
///   gated `Ok(None)` from a source that cannot answer the slot must never
///   become a 24 h Miss. One answer per provider is fetched per pass (a local
///   map): a provider backing several fields is still called at most once.
/// * `Ok(Some(media))`: the provider's own `find` is the match authority for
///   title and year -- the fill engine second-guesses neither (year is a
///   preference per `yearpref`, never a gate; romaji, localised and alternate
///   titles are the provider matcher's business). The engine enforces only
///   kind: an answer whose `media.kind` contradicts the resolved kind is a
///   definitive skip to the next provider, so a same-title answer of the
///   opposite kind can never file artwork, genres, length, year, or a
///   wrong-kind synopsis into the bucket. Otherwise only the requested field
///   is extracted ([`extract_field_bricks`]); a media without the field is a
///   definitive skip. A win inserts `Found` keyed to the TARGET identity
///   (with the provider's IDs absorbed via [`with_media_id`]), never the
///   provider's.
/// * `Ok(None)` or a media without the field simply continues down the list.
/// * `Err` records the error and continues -- and inserts NO miss for that
///   field: a transient failure must never become a 24 h miss. An error is
///   recorded even when a lower-priority provider later wins the field (the
///   field lands in both `found` and `errored`). After the last provider, a
///   field no provider filled is a bounded [`Brick::miss`] ONLY when at least
///   one consulted CAPABLE source answered definitively (`consulted_capable
///   >= 1` with every answer definitive); any error with no win leaves the
///   slot `Absent` so the next pass retries it.
/// * A field with zero capable supplied sources is reported `unsupported`:
///   nothing attempted, nothing stored, no retry clock.
///
/// All bricks of the pass persist through one [`BrickCache::insert_many`]
/// call: one save, all-or-nothing. `sources` is the caller's provider set
/// (usually the resolver's ordered chain); `cancel` bounds every `find`.
/// This performs the provider I/O itself -- it is the one `brick` entry
/// point that is not pure storage.
///
/// [`Brick::miss`]: Brick::miss
/// [`BrickCache::insert_many`]: BrickCache::insert_many
pub fn fill_entity(
    cache: &mut BrickCache,
    q: &Query,
    kind: Kind,
    sources: &[&dyn Source],
    cancel: &Cancel,
) -> Result<FillReport, Error> {
    if kind != Kind::SERIES && kind != Kind::MOVIE {
        return Err(Error::other(
            "metadata: brick fill with an unresolved kind",
        ));
    }
    let target = entity_key_for_query(q, kind);
    let episode_key = if kind == Kind::MOVIE {
        EpisodeKey::None
    } else {
        episode_key_for_query(q)
    };
    let mut report = FillReport {
        key: target.clone(),
        ..FillReport::default()
    };
    let mut fetched: HashMap<String, Result<Option<Media>, String>> = HashMap::new();
    let mut bricks: Vec<Brick> = Vec::new();
    // The supplied chain mapped once through `for_provider`: unknown names
    // are skipped here, never consulted, never counted.
    let mut supplied: Vec<(BrickSource, &dyn Source)> = Vec::new();
    for source in sources.iter() {
        let Some(mapped) = BrickSource::for_provider(source.name()) else {
            continue;
        };
        if supplied.iter().any(|(have, _)| *have == mapped) {
            continue;
        }
        supplied.push((mapped, *source));
    }
    for field in applicable_fields(kind, episode_key) {
        let episode = brick_episode_for(field, episode_key);
        match cache.lookup(&target, &episode, field) {
            BrickLookup::Found(_) | BrickLookup::MissWaiting(_) => continue,
            BrickLookup::RetryDue | BrickLookup::Absent => {}
        }
        let capable: Vec<(BrickSource, &dyn Source)> = supplied
            .iter()
            .copied()
            .filter(|(source, _)| is_capable(*source, kind, field))
            .collect();
        if capable.is_empty() {
            report.unsupported.push(field);
            continue;
        }
        // Policy order first, then any supplied capable source the policy
        // does not name (in supplied order, last): a new provider is never
        // silently ignored. Incapable entries never reach the wire.
        let mut order: Vec<(BrickSource, &dyn Source)> = Vec::new();
        for want in priority(kind, field) {
            if let Some(hit) = capable.iter().find(|(source, _)| source == want) {
                order.push((hit.0, hit.1));
            }
        }
        for (source, provider) in &capable {
            if !order.iter().any(|(have, _)| have == source) {
                order.push((*source, *provider));
            }
        }
        let mut consulted_capable = 0usize;
        let mut miss_source = None::<BrickSource>;
        let mut errors: Vec<String> = Vec::new();
        let mut won: Option<Brick> = None;
        for (want, provider) in &order {
            if miss_source.is_none() {
                miss_source = Some(*want);
            }
            consulted_capable += 1;
            let name = provider.name().to_string();
            let answer =
                fetched
                    .entry(name.clone())
                    .or_insert_with(|| match provider.find(cancel, q) {
                        Ok(media) => Ok(media),
                        Err(err) => Err(err.to_string()),
                    });
            let media = match answer {
                Err(message) => {
                    errors.push(format!("{name}: {message}"));
                    continue;
                }
                Ok(None) => continue,
                Ok(Some(media)) => media,
            };
            if media.kind != kind {
                continue;
            }
            let value = extract_field_bricks(media, q)
                .into_iter()
                .find(|(found_field, _)| *found_field == field)
                .map(|(_, value)| value);
            let Some(value) = value else { continue };
            won = Some(Brick::found(
                field,
                value,
                *want,
                with_media_id(&target, media),
                episode,
                provenance_for(*want, media, field, &episode_key),
            ));
            break;
        }
        match won {
            Some(brick) => {
                report.found.push(field);
                if !errors.is_empty() {
                    report.errored.push((field, errors.join("; ")));
                }
                bricks.push(brick);
            }
            None if !errors.is_empty() => {
                report.errored.push((field, errors.join("; ")));
            }
            None if consulted_capable > 0 => {
                report.missed.push(field);
                bricks.push(Brick::miss(
                    field,
                    miss_source.expect("consulted_capable > 0 implies a miss source"),
                    target.clone(),
                    episode,
                    format!(
                        "fill: no {field:?} from {consulted_capable} provider(s) for {}",
                        target.id
                    ),
                ));
            }
            None => {}
        }
    }
    cache.insert_many(bricks)?;
    Ok(report)
}
