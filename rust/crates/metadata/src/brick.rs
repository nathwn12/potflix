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
//! ([`EpisodeKey`]). The entity key is a caller-supplied identity string (T7-2
//! will derive it from the resolved query) plus the external IDs available
//! for it (TMDb, TVDB, IMDb, AniList, TVmaze); the map bucket is the
//! lowercased, whitespace-collapsed id, and [`BrickCache::insert`] rejects a
//! brick whose entity id does not match the cache key it is stored under, so
//! a wiring bug in the future merge can never file one show's artwork under
//! another show. The episode key is `none` for entity-level fields
//! (artwork, synopses, genres, length, year, title) and `episode`/`absolute`
//! for per-episode fields (episode title, episode synopsis); [`insert`]
//! rejects a brick whose episode scope does not fit its field.
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
//! # What this is not (T7-2+ owns the merge)
//!
//! This module stores and serves bricks; it performs no network I/O, runs no
//! provider, and merges nothing. The resolver/provider merge that fills the
//! cache from TVmaze/AniList/TMDb/Wikipedia/Cinemeta (plus manual corrections)
//! is T7-2 and later. The final API that merge builds on is: [`EntityKey`]
//! + [`EpisodeKey`] identity, [`Brick::found`]/[`Brick::miss`] constructors,
//!   [`BrickCache::insert`] (key-checked, miss-never-evicts-found, manual beats
//!   provider),
//!   [`BrickCache::lookup`]/[`lookup_at`] (`Found`/`MissWaiting`/`RetryDue`/`Absent`),
//!   the force-clear [`BrickCache::clear_entity`]/[`clear_episode`]/[`clear_field`],
//!   and atomic [`BrickCache::save`] plus load-or-empty [`load`]/[`load_from`].
//!   Each `insert`/`clear_*` saves in the same call; there is no batch-write
//!   API yet (T7-2/T7-4).
//!
//! [`lookup_at`]: BrickCache::lookup_at
//!
//! [`clear_episode`]: BrickCache::clear_episode
//! [`clear_field`]: BrickCache::clear_field
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

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::Error;
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

/// `BrickSource`: which provider (or hand) a brick came from. TMDb has no
/// provider yet -- the variant reserves the provenance slot the T7-2 merge
/// will fill -- and `Manual` marks a user correction, which the merge must
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
    /// Canonical identity, e.g. `"series|breaking bad|2008|1"`.
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
