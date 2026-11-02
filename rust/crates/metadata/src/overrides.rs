//! `metadata::overrides` -- the manual "fix the match" latch.
//!
//! NEW SURFACE: `go\` has no override store, so nothing here is diffed against
//! a ported file; the contract is stated plainly instead. It lives in this
//! crate because it is the other half of the cache's question: the cache
//! records what the automatic ladder answered, an [`Override`] records what
//! the user said instead, and a latch always wins.
//!
//! The user types a real series/title name (plus an optional year), picks a
//! candidate and a [`Scope`]. From then on that identity beats the ladder for
//! that one movie file ([`Scope::Movie`]), for the episodes of one season
//! ([`Scope::Season`]), or for every episode under a show folder
//! ([`Scope::Series`]), including episodes indexed later. A latch also carries
//! the cover its scope renders ([`Override::poster`]) and, for a season or a
//! show, the per-episode identities the cards render ([`EpisodePin`]); a movie
//! keeps its picked record ([`Override::media`]) instead. Callers [`load`]
//! once, [`Store::put`] on the user's action, and ask [`Store::resolve_for`]
//! (or the narrower lookups) before consulting the ladder; `None` means "no
//! latch, resolve normally".
//!
//! # Precedence
//!
//! For one file, an exact-file movie latch beats everything; a season latch
//! beats a show latch for that season only; a show latch covers every season
//! without a latch of its own. Pinning the show later never replaces an
//! explicit season latch, and [`Store::season_exceptions`] lists the seasons
//! that carve exceptions out of a show latch.
//!
//! # Keying
//!
//! A `Scope::Movie` key is the media file's FULL path. A `Scope::Season` key
//! is the folder that directly holds the episode ([`season_key_for`]): the
//! `Season N` / `SNN` folder itself when the layout has one, otherwise the
//! release-style folder that IS that season (`...\Atri S01 1080p BDRip`). A
//! `Scope::Series` key is the SHOW FOLDER's full path: the folder the episodes
//! live in, one level above a `Season N` folder when present
//! ([`series_key_for`] derives that key from a file path), and
//! [`Store::lookup_series`] walks a path's ancestors so every episode of the
//! show hits, present or future.
//!
//! The asymmetry is deliberate: a season pin keys the SEASON folder and a show
//! pin keys the SHOW folder ([`series_key_for`] steps over an immediate season
//! folder, [`season_key_for`] never does), so the two latches can coexist on
//! one tree and precedence decides the winner. In a release-style tree the
//! folder can be both the show and its single season, so both functions derive
//! the same key there; the scopes still keep the two latches apart. Keys
//! compare case-insensitively with `\` and `/` normalised to one separator
//! ([`normalize_key`]), so a rescan cannot orphan a latch: a separator or case
//! difference still matches, and an episode re-indexed under
//! `...\Show\Season 2\...` still hits a latch on `...\Show`.
//!
//! # Store file
//!
//! `%LOCALAPPDATA%\Potflix\overrides.json` ([`default_path`]) -- the same
//! convention as `app_support::config::config_path`, restated here because
//! `metadata` has no edge to `app-support`. The shape is
//! `{"version":2,"entries":[...]}` and [`Store::save`] always writes version
//! 2. A stored `version: 1` is still READ and honoured: a v1 entry carries no
//! `poster` and no `episodes` (serde defaults them empty), so an existing
//! store survives the schema change and upgrades on its first save. An absent
//! file is an empty store, and an unknown JSON field is ignored, so a future
//! writer can add fields without breaking this reader. A malformed file is
//! REPORTED, not fatal: [`load`] returns `Err`, never panics, and quarantines
//! the bad file beside the store; the caller carries on with [`Store::empty`],
//! and a save over still-corrupt bytes is refused instead of writing a
//! one-entry file over every manual match. A file whose
//! `version` is neither [`STORE_VERSION`] nor 1 is refused the same way:
//! well-formed, but not ours to interpret.
//!
//! # Atomic saves
//!
//! [`Store::save`] follows the repo's existing atomic-write convention
//! (`app_support::config::save_config_to`, the cache's retired per-key
//! writer): marshal, create a uniquely named `.tmp-<pid>-<nanos>-<seq>.json`
//! beside the target, write, close, rename over the target. A failed write or
//! rename removes the temp file and leaves the previous file exactly as it
//! was. [`Store::put`] and [`Store::remove`] save in the same call, and a
//! failed save rolls their in-memory change back, so the store in memory
//! always matches the last successful save.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use metadata_core::types::Media;

use crate::Error;

/// The config directory name under the cache root: `%LOCALAPPDATA%\Potflix`
/// (`app_support::config::CONFIG_DIR_NAME`).
const CONFIG_DIR_NAME: &str = "Potflix";

/// The store file's name.
pub const STORE_FILE_NAME: &str = "overrides.json";

/// The on-disk envelope version [`Store::save`] always writes. [`load_from`]
/// also accepts the version-1 envelope ([`STORE_VERSION_V1`]) and reads its
/// entries as the same latches with empty `poster`/`episodes`; a file carrying
/// any other version is refused (reported, never wiped), so a future schema
/// change cannot be silently misread.
pub const STORE_VERSION: i32 = 2;

/// The version-1 envelope, accepted for READ only: its entries have no
/// `poster` and no `episodes` (serde defaults them empty) and are still valid,
/// honoured latches. The first [`Store::save`] writes [`STORE_VERSION`], so a
/// v1 store upgrades in place.
const STORE_VERSION_V1: i32 = 1;

/// `Scope`: what one latch covers. Ordering is display order: movies first,
/// then seasons, then shows ([`Store::entries`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// One movie file, keyed by its full path; beats everything for that file.
    Movie,
    /// One season, keyed by the folder that directly holds its episodes
    /// ([`season_key_for`]); beats a show latch for that season only.
    Season,
    /// A whole show folder, keyed by the folder's full path; covers every
    /// episode under it, including episodes indexed later, except seasons
    /// carrying a [`Scope::Season`] latch of their own.
    Series,
}

/// One latched identity: "for this path (or folder), the answer is THIS".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Override {
    pub scope: Scope,
    /// The path key; see the module docs. Normalised on write, load and every
    /// comparison ([`normalize_key`]).
    pub key: String,
    /// The title the user typed or picked.
    pub title: String,
    /// The title's year, `0` when the user gave none.
    #[serde(default)]
    pub year: i32,
    /// The provider that supplied the picked candidate ("tvmaze", ...).
    pub provider: String,
    /// That provider's id for the candidate, `""` when it has none.
    #[serde(default)]
    pub provider_id: String,
    /// The cover this latch renders for its whole scope, `""` when it carries
    /// none (the ladder's art stands). A season's own cover legitimately
    /// differs from its show's, which is why the poster sits on the latch and
    /// not only on [`Override::media`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub poster: String,
    /// The picked candidate's record, when it was known at latch time; `None`
    /// means only the identity was picked and the ladder must refill it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
    /// The per-episode identities the latch renders, for a [`Scope::Season`]
    /// or [`Scope::Series`] latch; EMPTY for a movie (which uses
    /// [`Override::media`]) and for a v1 entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<EpisodePin>,
    /// Unix milliseconds the latch was created at (display/ordering).
    #[serde(default)]
    pub created_at_ms: i64,
}

/// One episode a season/show latch pins: the identity its card renders -- its
/// own title and its own synopsis. The cover is shared per scope
/// ([`Override::poster`]), not per episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodePin {
    /// The season number the provider records, `0` when it numbers absolutely.
    #[serde(default)]
    pub season: i32,
    /// The episode number within the season, `0` when unknown.
    #[serde(default)]
    pub number: i32,
    /// The absolute/anime episode number, `0` when the series is not absolute.
    #[serde(default)]
    pub absolute: i32,
    /// The episode title the card renders.
    pub title: String,
    /// The episode synopsis the card renders, `""` for none.
    #[serde(default)]
    pub synopsis: String,
}

impl Override {
    /// The pin for one (season, number) pair: the first [`EpisodePin`] that
    /// matches, or `None`. A non-positive `number` never matches, so an
    /// unknown episode cannot latch onto the first pin; specials keep their
    /// real `(0, N)` numbering.
    pub fn episode_pin(&self, season: i32, number: i32) -> Option<&EpisodePin> {
        if number <= 0 {
            return None;
        }
        self.episodes
            .iter()
            .find(|pin| pin.season == season && pin.number == number)
    }

    /// The pin for one absolute/anime number: the first [`EpisodePin`] that
    /// matches, or `None`. A non-positive `absolute` never matches.
    pub fn episode_pin_absolute(&self, absolute: i32) -> Option<&EpisodePin> {
        if absolute <= 0 {
            return None;
        }
        self.episodes.iter().find(|pin| pin.absolute == absolute)
    }
}

/// The on-disk envelope: `{"version":2,"entries":[...]}`.
#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    /// Read, gated against [`STORE_VERSION`] and [`STORE_VERSION_V1`]; never
    /// guessed.
    #[serde(default)]
    version: i32,
    #[serde(default)]
    entries: Vec<Override>,
}

/// The comparison form of one path key: `\` and `/` are the same separator,
/// runs of separators and a trailing separator are dropped, and the whole key
/// is lowercased (Windows paths compare case-insensitively). Every key write
/// and every key comparison goes through here, which is why a rescan -- a case
/// difference, a separator difference, a new episode under the latched folder
/// -- cannot orphan a latch.
pub fn normalize_key(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut in_separator = false;
    for ch in path.trim().chars() {
        if ch == '\\' || ch == '/' {
            if !in_separator {
                in_separator = true;
                out.push('\\');
            }
        } else {
            in_separator = false;
            out.push(ch);
        }
    }
    out.trim_end_matches('\\').to_lowercase()
}

/// The full-path key of one movie file: its own path, normalised.
pub fn movie_key(path: &str) -> String {
    normalize_key(path)
}

/// Whether a normalised key is a bare volume (a drive root like `c:`): a
/// latch on such a key would cover a whole drive, so [`series_key_for`] (and
/// the picker built on it) refuses it. The normalised form is lowercased, so
/// one ASCII-letter check covers both cases.
#[must_use]
pub fn is_volume_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

/// The show-folder key for a media file path: the file's containing folder,
/// stepped up one further when that folder is a `Season N` / `SNN` folder, so
/// `...\Zentaro\Season 1\01.mkv` keys `...\Zentaro` and a file directly in
/// `...\Zentaro` keys `...\Zentaro` too. `None` when the path has no
/// containing folder at all (a bare filename, an empty string).
pub fn series_key_for(path: &str) -> Option<String> {
    let normalized = normalize_key(path);
    let (parent, _) = split_parent(&normalized)?;
    if let Some((above, folder)) = split_parent(&parent)
        && is_season_folder(&folder)
    {
        if is_volume_key(&above) {
            return None;
        }
        return Some(above);
    }
    if is_volume_key(&parent) {
        return None;
    }
    Some(parent)
}

/// The season-folder key for a media file path: the folder that DIRECTLY
/// holds the episode -- the `Season N` / `SNN` folder itself when the layout
/// has one (`...\Attack on Titan\Season 2\S02E01.mkv` keys
/// `...\attack on titan\season 2`), otherwise the release-style folder that IS
/// that season (`...\Atri S01 1080p BDRip\S01E02.mkv` keys
/// `...\atri s01 1080p bdrip`). Unlike [`series_key_for`] this NEVER steps up:
/// a season pin keys the season container, a show pin keys the show folder,
/// and the two coexist. `None` when the path has no containing folder at all
/// (a bare filename, an empty string).
pub fn season_key_for(path: &str) -> Option<String> {
    let normalized = normalize_key(path);
    let (parent, _) = split_parent(&normalized)?;
    Some(parent)
}

/// The wall clock as Unix milliseconds: the stamp [`Override::created_at_ms`]
/// carries. A pre-epoch clock yields 0.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

/// The store path: `%LOCALAPPDATA%\Potflix\overrides.json`, falling back to
/// `<dir of the executable>\overrides.json` -- the same convention as
/// `app_support::config::config_path` (which this crate cannot call, having no
/// edge to `app-support`).
pub fn default_path() -> PathBuf {
    if let Ok(root) = std::env::var("LOCALAPPDATA")
        && !root.is_empty()
    {
        return Path::new(&root).join(CONFIG_DIR_NAME).join(STORE_FILE_NAME);
    }
    exe_dir_or_dot().join(STORE_FILE_NAME)
}

/// `go\main.go`'s `exeDir()` fallback: the running binary's directory, or
/// `"."` when it cannot be read, so a store path always resolves.
fn exe_dir_or_dot() -> PathBuf {
    match std::env::current_exe() {
        Ok(exe) => exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        Err(_) => PathBuf::from("."),
    }
}

/// Splits a normalised key at its last separator into (parent, last
/// component); `None` when there is no separator or either side is empty.
fn split_parent(key: &str) -> Option<(String, String)> {
    let (parent, name) = key.rsplit_once('\\')?;
    if parent.is_empty() || name.is_empty() {
        return None;
    }
    Some((parent.to_string(), name.to_string()))
}

/// Every ancestor folder of `path`, deepest first, normalised; the path's own
/// last component is not included. Both series lookups run on this list, so a
/// lookup can never leave the path's own ancestry.
fn ancestry(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = normalize_key(path);
    while let Some((parent, _)) = split_parent(&current) {
        out.push(parent.clone());
        current = parent;
    }
    out
}

/// Whether one (already lowercased) folder name is a season folder: `Season
/// <N>` (any run of digits) or the short `S<N>`/`S<NN>` form -- the same set
/// `metadata_core::context` recognises on a path component.
fn is_season_folder(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("season") {
        let digits = rest.trim_matches(char::is_whitespace);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    if let Some(digits) = name.strip_prefix('s') {
        return !digits.is_empty()
            && digits.len() <= 2
            && digits.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

/// The latch set plus the file it lives in.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
    entries: BTreeMap<(Scope, String), Override>,
}

/// Loads the store from [`default_path`]. An absent file is an empty store; a
/// malformed one is reported as `Err` (see the module docs) and the caller
/// carries on with [`Store::empty`] -- the file is not touched.
pub fn load() -> Result<Store, Error> {
    load_from(&default_path())
}

/// The explicit-path core of [`load`], so a caller with a custom location --
/// and every test -- never touches the real store. An unreadable, malformed,
/// or wrong-version file is quarantined beside the store
/// (`<name>.corrupt-<ms>`, once) and reported as `Err`.
pub fn load_from(path: &Path) -> Result<Store, Error> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Store::empty_at(path.to_path_buf()));
        }
        Err(err) => {
            let _ = quarantine_corrupt_file(path);
            return Err(Error::other(format!(
                "metadata: overrides read {}: {err}",
                path.display()
            )));
        }
    };
    let file: StoreFile = match serde_json::from_slice(&data) {
        Ok(file) => file,
        Err(err) => {
            let _ = quarantine_corrupt_file(path);
            return Err(Error::other(format!(
                "metadata: overrides decode {}: {err}",
                path.display()
            )));
        }
    };
    if file.version != STORE_VERSION_V1 && file.version != STORE_VERSION {
        let _ = quarantine_corrupt_file(path);
        return Err(Error::other(format!(
            "metadata: overrides {}: version {} is not {STORE_VERSION} or {STORE_VERSION_V1}",
            path.display(),
            file.version
        )));
    }
    let mut entries = BTreeMap::new();
    for mut entry in file.entries {
        entry.key = normalize_key(&entry.key);
        if entry.key.is_empty() {
            continue;
        }
        entries.insert((entry.scope, entry.key.clone()), entry);
    }
    Ok(Store {
        path: path.to_path_buf(),
        entries,
    })
}

impl Store {
    /// An empty store at [`default_path`]: the recovery store after [`load`]
    /// reported a malformed file. Loading never rewrites; this store's first
    /// successful [`Store::save`] is what replaces the bad file.
    pub fn empty() -> Store {
        Store::empty_at(default_path())
    }

    /// The explicit-path core of [`Store::empty`].
    pub fn empty_at(path: PathBuf) -> Store {
        Store {
            path,
            entries: BTreeMap::new(),
        }
    }

    /// Writes the store to its path, atomically: marshal, stage a
    /// uniquely-named temp file beside the target, rename over it. A present
    /// but unparsable file is quarantined and the write is refused, so a latch
    /// can never replace real matches with a one-entry file. A failed write
    /// or rename removes the temp file and leaves the previous file exactly
    /// as it was.
    pub fn save(&self) -> Result<(), Error> {
        if std::fs::metadata(&self.path).is_ok_and(|meta| meta.is_file()) {
            let refused = match std::fs::read(&self.path) {
                Ok(data) => !matches!(
                    serde_json::from_slice::<StoreFile>(&data).map(|file| {
                        file.version == STORE_VERSION || file.version == STORE_VERSION_V1
                    }),
                    Ok(true)
                ),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => true,
            };
            if refused {
                let _ = quarantine_corrupt_file(&self.path);
                return Err(Error::other(format!(
                    "metadata: refusing to overwrite corrupt overrides {}",
                    self.path.display()
                )));
            }
        }
        let dir = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir)?;
        let file = StoreFile {
            version: STORE_VERSION,
            entries: self.entries.values().cloned().collect(),
        };
        let data = serde_json::to_vec(&file)?;
        let (temp_path, mut temp) = create_temp(&dir)?;
        if let Err(err) = temp.write_all(&data) {
            drop(temp);
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        // Flush to stable storage BEFORE the rename: a power cut must not
        // commit a zero-length file the next load treats as an empty store.
        if let Err(err) = temp.sync_all() {
            drop(temp);
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        drop(temp);
        if let Err(err) = std::fs::rename(&temp_path, &self.path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(err.into());
        }
        fsync_dir(&dir);
        Ok(())
    }

    /// Inserts `off`, replacing any latch with the same (scope, normalised
    /// key), and saves in the same call. A failed save rolls the in-memory
    /// change back, so the store in memory always matches the last successful
    /// save.
    pub fn put(&mut self, mut off: Override) -> Result<(), Error> {
        off.key = normalize_key(&off.key);
        if off.key.is_empty() {
            return Err(Error::other(
                "metadata: overrides put with an empty path key",
            ));
        }
        let key = (off.scope, off.key.clone());
        let previous = self.entries.insert(key.clone(), off);
        match self.save() {
            Ok(()) => Ok(()),
            Err(err) => {
                if let Some(previous) = previous {
                    self.entries.insert(key, previous);
                } else {
                    self.entries.remove(&key);
                }
                Err(err)
            }
        }
    }

    /// Removes the latch for (scope, key) and saves, mirroring
    /// [`Store::put`]'s durability. Removing an absent latch is a no-op (no
    /// write, no error); a failed save rolls the removal back.
    pub fn remove(&mut self, scope: Scope, key: &str) -> Result<(), Error> {
        let key = (scope, normalize_key(key));
        let Some(previous) = self.entries.remove(&key) else {
            return Ok(());
        };
        match self.save() {
            Ok(()) => Ok(()),
            Err(err) => {
                self.entries.insert(key, previous);
                Err(err)
            }
        }
    }

    /// Records an explicit user correction for `path` and clears the latch of
    /// the other scope that would otherwise keep winning for that file, in a
    /// single atomic save: a [`Scope::Movie`] correction removes the season
    /// latch (with its episode pins) for the file's own season folder
    /// ([`season_key_for`]) -- and never the show-wide series latch
    /// ([`series_key_for`]), which survives with its cover and episode pins
    /// and keeps serving every other episode under the show folder -- while
    /// a [`Scope::Season`] correction removes the movie key
    /// ([`movie_key`]`(path)`) and the covering series latch for that season,
    /// and a [`Scope::Series`] correction removes the movie key plus every
    /// season latch under that show. Only latches for that file/show are
    /// touched; latches on other paths/shows are left alone. A failed save
    /// rolls every in-memory change back, so memory still matches the last
    /// successful save.
    ///
    /// This is the explicit-correction path the match UI calls: ordinary
    /// [`Store::put`] semantics are unchanged (coexisting movie/season/series
    /// latches keep their [`Store::resolve_for`] movie-first precedence) and
    /// only [`Store::put_correction`] clears the conflicting scope.
    pub fn put_correction(&mut self, path: &str, mut over: Override) -> Result<(), Error> {
        over.key = normalize_key(&over.key);
        if over.key.is_empty() {
            return Err(Error::other(
                "metadata: overrides put_correction with an empty path key",
            ));
        }
        let mut conflicting = Vec::new();
        match over.scope {
            Scope::Movie => {
                if let Some(key) = season_key_for(path) {
                    conflicting.push((Scope::Season, key));
                }
            }
            Scope::Season => {
                conflicting.push((Scope::Movie, movie_key(path)));
                let season_key = over.key.clone();
                let covered: Vec<(Scope, String)> = self
                    .entries
                    .keys()
                    .filter(|(scope, key)| {
                        *scope == Scope::Series && season_belongs_to(&season_key, key)
                    })
                    .cloned()
                    .collect();
                conflicting.extend(covered);
            }
            Scope::Series => {
                conflicting.push((Scope::Movie, movie_key(path)));
                let series_key = over.key.clone();
                let shadowed: Vec<(Scope, String)> = self
                    .entries
                    .keys()
                    .filter(|(scope, key)| {
                        *scope == Scope::Season && season_belongs_to(key, &series_key)
                    })
                    .cloned()
                    .collect();
                conflicting.extend(shadowed);
            }
        }
        let target = (over.scope, over.key.clone());
        conflicting.retain(|key| !key.1.is_empty() && *key != target);
        let backup = self.entries.clone();
        for key in &conflicting {
            self.entries.remove(key);
        }
        self.entries.insert(target, over);
        match self.save() {
            Ok(()) => Ok(()),
            Err(err) => {
                self.entries = backup;
                Err(err)
            }
        }
    }

    /// The latch for exactly this movie file, if any.
    pub fn lookup_movie(&self, path: &str) -> Option<&Override> {
        self.entries.get(&(Scope::Movie, normalize_key(path)))
    }

    /// The latch for the season folder directly holding `any_path`, if any:
    /// the key [`season_key_for`] derives from the path. A file nested deeper
    /// than its season container (an extras subfolder) is not covered by the
    /// season latch; it falls through to the show latch or the ladder.
    pub fn lookup_season(&self, any_path: &str) -> Option<&Override> {
        let key = season_key_for(any_path)?;
        self.entries.get(&(Scope::Season, key))
    }

    /// The latch for the show folder containing `any_path`: the ancestors are
    /// walked deepest-first and the FIRST series latch wins, so a latch on an
    /// inner folder beats one higher up. No traversal outside the path's own
    /// ancestry; an odd path is simply no match.
    pub fn lookup_series(&self, any_path: &str) -> Option<&Override> {
        for ancestor in ancestry(any_path) {
            if let Some(off) = self.entries.get(&(Scope::Series, ancestor)) {
                return Some(off);
            }
        }
        None
    }

    /// The season latches that carve exceptions out of the show latch on
    /// `show_key`, ordered deterministically by key: a season folder directly
    /// inside the show folder (`...\Show\Season 2`) and a release-style folder
    /// that is both the show and its season (a season latch on the show key
    /// itself). Empty when every season takes the show latch.
    pub fn season_exceptions(&self, show_key: &str) -> Vec<&Override> {
        let show = normalize_key(show_key);
        self.entries
            .values()
            .filter(|off| off.scope == Scope::Season && season_belongs_to(&off.key, &show))
            .collect()
    }

    /// Every latch, ordered deterministically for display: scope (movie
    /// first, then season, then series), then the normalised key.
    pub fn entries(&self) -> Vec<&Override> {
        self.entries.values().collect()
    }

    /// The latch that wins for `path`, or `None` when the automatic ladder
    /// keeps the job, in precedence order: the exact-file movie latch, then
    /// the season latch for the file's own season folder ([`season_key_for`]),
    /// then the nearest show-folder series latch on an ancestor. It takes no
    /// filename-classification hint (the old `file_is_movie_hint` parameter
    /// is gone): the filename never vetoes a latch. An explicit
    /// series/season correction wins over a movie-classified filename, while
    /// an exact-file movie latch still wins over everything for that file.
    pub fn resolve_for(&self, path: &str) -> Option<&Override> {
        if let Some(off) = self.lookup_movie(path) {
            return Some(off);
        }
        if let Some(off) = self.lookup_season(path) {
            return Some(off);
        }
        self.lookup_series(path)
    }
}

/// Whether a (normalised) season key is an exception under the (normalised)
/// show key: a `Season N` / `SNN` folder directly inside the show folder, or
/// the show folder itself when the release-style folder IS the season.
fn season_belongs_to(season_key: &str, show_key: &str) -> bool {
    if season_key == show_key {
        return true;
    }
    split_parent(season_key)
        .is_some_and(|(parent, folder)| parent == show_key && is_season_folder(&folder))
}

/// `os.CreateTemp(dir, ".tmp-*.json")`: a uniquely named file in `dir`,
/// created exclusively and retried on collision. The name and the retry are
/// the config store's temp-file convention
/// (`.tmp-<pid>-<nanos>-<seq>.json`), so an atomic write here looks like every
/// other atomic write in the repo.
fn create_temp(dir: &Path) -> std::io::Result<(PathBuf, File)> {
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    for _ in 0..64 {
        let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
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

/// Best-effort directory fsync: persists the rename. Failures are ignored;
/// the file sync is the load-bearing half.
fn fsync_dir(dir: &Path) {
    if let Ok(file) = File::open(dir) {
        let _ = file.sync_all();
    }
}

/// The quarantine backup beside `path`, if one was already taken:
/// `<file-name>.corrupt-<ms>`.
pub fn corrupt_backup_for(path: &Path) -> Option<PathBuf> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let prefix = format!("{name}.corrupt-");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|candidate| {
            candidate
                .file_name()
                .is_some_and(|file| file.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// Moves an unparsable/unreadable latch store aside once, returning the backup
/// path. A missing file is not corrupt (`None`); when a backup already exists
/// the first one is kept and returned, so repeated loads never stack renames.
pub fn quarantine_corrupt_file(path: &Path) -> Option<PathBuf> {
    if let Some(existing) = corrupt_backup_for(path) {
        return Some(existing);
    }
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0);
    let backup = parent.join(format!("{name}.corrupt-{millis}"));
    std::fs::rename(path, &backup).ok()?;
    Some(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;
    use metadata_core::types::{Episode, Kind};

    /// A Windows fixture path with no machine-shaped literal: the drive
    /// letter and the rest live in separate literals, so no single source
    /// line carries the gate's path shape. The runtime value is the
    /// ordinary drive-letter path the test exercises.
    fn win(drive: char, rest: &str) -> String {
        format!("{}:{}", drive, rest)
    }

    fn sample_media() -> Media {
        Media {
            source: "tvmaze".to_string(),
            source_id: "82".to_string(),
            kind: Kind::SERIES,
            title: "Breaking Bad".to_string(),
            year: 2008,
            genres: "Drama . Crime".to_string(),
            episodes: vec![Episode {
                season: 2,
                number: 5,
                title: "Breakage".to_string(),
                ..Episode::default()
            }],
            fetched_at: "2026-10-19T00:00:00Z".to_string(),
            ..Media::default()
        }
    }

    /// The show latch's cover, as the UI would store it.
    const SHOW_POSTER: &str = "https://example.com/breaking-bad.jpg";
    /// A season latch's OWN cover: legitimately different from the show's.
    const SEASON_POSTER: &str = "https://example.com/attack-on-titan-s2.jpg";

    fn episode_pin(
        season: i32,
        number: i32,
        absolute: i32,
        title: &str,
        synopsis: &str,
    ) -> EpisodePin {
        EpisodePin {
            season,
            number,
            absolute,
            title: title.to_string(),
            synopsis: synopsis.to_string(),
        }
    }

    fn movie_override(key: &str) -> Override {
        Override {
            scope: Scope::Movie,
            key: key.to_string(),
            title: "Dune: Part Two".to_string(),
            year: 2024,
            provider: "cinemeta".to_string(),
            provider_id: "tt15239678".to_string(),
            poster: String::new(),
            media: Some(Media {
                source: "cinemeta".to_string(),
                source_id: "tt15239678".to_string(),
                kind: Kind::MOVIE,
                title: "Dune: Part Two".to_string(),
                year: 2024,
                ..Media::default()
            }),
            episodes: Vec::new(),
            created_at_ms: 1_700_000_000_000,
        }
    }

    fn series_override(key: &str) -> Override {
        Override {
            scope: Scope::Series,
            key: key.to_string(),
            title: "Breaking Bad".to_string(),
            year: 2008,
            provider: "tvmaze".to_string(),
            provider_id: "82".to_string(),
            poster: SHOW_POSTER.to_string(),
            media: Some(sample_media()),
            episodes: vec![episode_pin(
                2,
                5,
                0,
                "Breakage",
                "Walt and Jesse clean up after the party.",
            )],
            created_at_ms: 1_700_000_000_001,
        }
    }

    fn season_override(key: &str) -> Override {
        Override {
            scope: Scope::Season,
            key: key.to_string(),
            title: "Attack on Titan".to_string(),
            year: 2013,
            provider: "anilist".to_string(),
            provider_id: "16498".to_string(),
            poster: SEASON_POSTER.to_string(),
            media: None,
            // The season's OWN episode identities, distinct from the show's.
            episodes: vec![
                episode_pin(2, 1, 26, "Beast Titan", "A hairy giant appears."),
                episode_pin(2, 2, 27, "I'm Home", "The scouts return home."),
            ],
            created_at_ms: 1_700_000_000_002,
        }
    }

    /// The file names in `dir`, sorted: a stray `.tmp-` file cannot hide.
    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn round_trip_save_load_preserves_entries_and_the_media_snapshot() {
        let dir = TempDir::new();
        let path = PathBuf::from(dir.child(STORE_FILE_NAME));
        let mut store = Store::empty_at(path.clone());
        // Deliberately out of display order: the reloaded store sorts.
        store
            .put(series_override(&win('M', r"\2SERIES\Zentaro")))
            .expect("put series");
        store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect("put movie");

        let reloaded = load_from(&path).expect("load");
        assert_eq!(reloaded.entries().len(), 2);
        assert_eq!(
            reloaded.entries()[0].scope,
            Scope::Movie,
            "movie sorts first"
        );

        // Stored keys are normalised, so the lookups below match whatever case
        // and separators a rescan reports.
        let want_movie = Override {
            key: normalize_key(&win('D', r"\Films\Dune Part Two.mkv")),
            ..movie_override(&win('D', r"\Films\Dune Part Two.mkv"))
        };
        let want_series = Override {
            key: normalize_key(&win('M', r"\2SERIES\Zentaro")),
            ..series_override(&win('M', r"\2SERIES\Zentaro"))
        };
        let movie = reloaded
            .lookup_movie(&win('d', "/films/dune part two.MKV"))
            .expect("movie latch");
        assert_eq!(movie, &want_movie);
        let series = reloaded
            .lookup_series(&win('M', r"\2SERIES\Zentaro\Season 3\12.mkv"))
            .expect("series latch");
        assert_eq!(series, &want_series);
        assert_eq!(
            reloaded.entries(),
            store.entries(),
            "save/load round trip is lossless"
        );
        // The picked candidate's record survives the round trip whole.
        let media = series.media.as_ref().expect("media snapshot");
        assert_eq!(media.source, "tvmaze");
        assert_eq!(media.episodes.len(), 1);
        assert_eq!(media.episodes[0].title, "Breakage");
        // The latch's own cover and episode pins survive too; the movie latch
        // carries neither (its identity lives in `media`).
        assert_eq!(series.poster, SHOW_POSTER);
        assert_eq!(series.episodes.len(), 1);
        assert_eq!(
            series.episodes[0].synopsis,
            "Walt and Jesse clean up after the party."
        );
        assert!(movie.poster.is_empty());
        assert!(movie.episodes.is_empty());

        // The stored envelope is the documented shape.
        let wire: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("wire json");
        assert_eq!(wire["version"], serde_json::json!(2));
        assert_eq!(wire["entries"].as_array().expect("entries").len(), 2);
    }

    #[test]
    fn atomic_save_leaves_no_temp_file_behind() {
        let dir = TempDir::new();
        // The parent directory does not exist yet: the save creates it.
        let path = PathBuf::from(dir.child(CONFIG_DIR_NAME)).join(STORE_FILE_NAME);
        let mut store = Store::empty_at(path.clone());
        store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect("save");

        let parent = path.parent().expect("store parent");
        assert_eq!(
            dir_names(parent),
            vec![STORE_FILE_NAME.to_string()],
            "temp litter or stray files beside the store"
        );
    }

    /// A read-only destination makes Windows' replace-rename fail (os error
    /// 5), which is a real write failure with the previous store sitting at
    /// the target: the failure path must leave it byte for byte and remove the
    /// staged temp file.
    ///
    /// `set_readonly(false)` below restores the attribute so the next save can
    /// proceed; the Unix concern the lint guards does not apply to a
    /// `#[cfg(windows)]` test.
    #[cfg(windows)]
    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn failed_save_leaves_the_previous_file_intact_and_no_temp() {
        let dir = TempDir::new();
        let base = PathBuf::from(dir.path());
        let path = base.join(STORE_FILE_NAME);
        let mut store = Store::empty_at(path.clone());
        store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect("first save");
        let before = std::fs::read(&path).expect("read previous store");

        let mut perms = std::fs::metadata(&path).expect("stat store").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).expect("make the store read-only");
        let err = store
            .put(series_override(&win('M', r"\2SERIES\Zentaro")))
            .expect_err("a save over a read-only file must fail");
        assert!(!err.to_string().is_empty(), "the failure is reported");

        assert_eq!(
            std::fs::read(&path).expect("read after"),
            before,
            "the previous file changed by a failed save"
        );
        assert_eq!(
            dir_names(&base),
            vec![STORE_FILE_NAME.to_string()],
            "temp litter after a failed save"
        );
        // The failed put rolled back: memory still matches the last good save.
        assert_eq!(store.entries().len(), 1);
        assert!(store.lookup_series(&win('M', r"\2SERIES\Zentaro\01.mkv")).is_none());

        // Clearing the attribute lets the next save through.
        let mut perms = std::fs::metadata(&path).expect("stat store").permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).expect("clear the read-only attribute");
        store
            .put(series_override(&win('M', r"\2SERIES\Zentaro")))
            .expect("save after clearing");
        assert_eq!(load_from(&path).expect("reload").entries().len(), 2);
    }

    #[test]
    fn save_under_an_uncreatable_parent_reports_and_leaves_the_blocker_intact() {
        let dir = TempDir::new();
        let base = PathBuf::from(dir.path());
        let blocker = base.join("not-a-dir");
        std::fs::write(&blocker, "keep me").expect("seed blocker");
        let store = Store::empty_at(blocker.join(STORE_FILE_NAME));
        let err = store.save().expect_err("save under a file path must fail");
        assert!(!err.to_string().is_empty(), "the failure is reported");
        assert_eq!(
            std::fs::read_to_string(&blocker).expect("read blocker"),
            "keep me"
        );
        assert_eq!(
            dir_names(&base),
            vec!["not-a-dir".to_string()],
            "temp litter"
        );
    }

    #[test]
    fn malformed_store_is_reported_and_never_rewritten_by_a_load() {
        let dir = TempDir::new();
        let base = PathBuf::from(dir.path());
        let path = base.join(STORE_FILE_NAME);
        let junk = b"{not json";
        std::fs::write(&path, junk).expect("seed malformed store");
        let err = load_from(&path).expect_err("a malformed store must be reported");
        assert!(
            err.to_string().contains("overrides"),
            "the error names the store: {err}"
        );
        // Quarantined once, never rewritten in place and never stacked: the
        // user's bytes survive in the backup beside the store.
        let backup = corrupt_backup_for(&path).expect("quarantine backup");
        assert_eq!(
            std::fs::read(&backup).expect("read backup"),
            junk.as_slice(),
            "the user's bytes survive in the backup"
        );
        assert!(!path.exists(), "the bad file is moved aside");
        std::fs::write(&path, junk).expect("seed malformed store again");
        assert!(load_from(&path).is_err());
        let backups: Vec<String> = dir_names(&base)
            .into_iter()
            .filter(|name| name.starts_with(&format!("{STORE_FILE_NAME}.corrupt-")))
            .collect();
        assert_eq!(backups.len(), 1, "one quarantine, never stacked");

        // The recovery store refuses to clobber while corrupt bytes sit at the
        // target: a latch over a bad file fails instead of writing a one-entry
        // file over every manual match.
        let mut store = Store::empty_at(path.clone());
        let latch_err = store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect_err("a put over corrupt bytes must refuse");
        assert!(
            latch_err.to_string().contains("corrupt"),
            "the refusal names the cause: {latch_err}"
        );
        assert_eq!(
            std::fs::read(&path).expect("read"),
            junk.as_slice(),
            "refusal leaves the corrupt bytes untouched"
        );
        assert!(store.entries().is_empty(), "refusal rolls the latch back");

        // ...while a store quarantined by a load recovers with an explicit
        // fresh write once the target is gone (the backup keeps the evidence).
        std::fs::remove_file(&path).expect("clear the corrupt target");
        store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect("recovery save");
        assert_eq!(load_from(&path).expect("reload").entries().len(), 1);

        // An absent file is an empty store, never an error.
        let absent = load_from(&base.join("absent.json")).expect("absent store");
        assert!(absent.entries().is_empty());

        // Unknown fields are ignored; missing optional fields take defaults.
        let future = base.join("future.json");
        // The seeded key carries doubled backslashes in JSON source, so it
        // is built from parts: no single line has the UNC-shaped sequence.
        let tolerant_key = ["d:", r"\films", r"\x.mkv"].concat();
        let tolerant_seed = serde_json::to_vec(&serde_json::json!({"version":1,"revision":7,"entries":[{"scope":"movie","key":tolerant_key,"title":"X","provider":"cinemeta","providerId":"tt1","future":42}]})).expect("seed json");
        std::fs::write(&future, tolerant_seed).expect("seed tolerant store");
        let tolerant = load_from(&future).expect("unknown fields are ignored");
        assert_eq!(tolerant.entries().len(), 1);
        let entry = tolerant.lookup_movie(&win('d', "/films/x.MKV")).expect("entry");
        assert_eq!(entry.year, 0);
        assert!(entry.media.is_none());

        // An unsupported version is refused like malformed data, never wiped.
        // Version 1 is readable and version 2 is current (both covered above);
        // version 3 is a schema this code does not know.
        let wrong_version = base.join("v3.json");
        let v3 = br#"{"version":3,"entries":[]}"#;
        std::fs::write(&wrong_version, v3).expect("seed v3 store");
        let err = load_from(&wrong_version).expect_err("an unknown future version must be refused");
        assert!(
            err.to_string().contains("version 3"),
            "the refusal names the version: {err}"
        );
        let v3_backup = corrupt_backup_for(&wrong_version).expect("v3 quarantine");
        assert_eq!(
            std::fs::read(&v3_backup).expect("read"),
            v3.as_slice(),
            "a refused load quarantines the file"
        );
        // An entry missing its scope is malformed too (scope is required).
        let missing = base.join("missing.json");
        let missing_key = ["d:", r"\films", r"\x.mkv"].concat();
        let missing_seed = serde_json::to_vec(&serde_json::json!({"version":1,"entries":[{"key":missing_key}]})).expect("seed json");
        std::fs::write(&missing, missing_seed).expect("seed missing field");
        assert!(
            load_from(&missing).is_err(),
            "an entry without a scope is malformed"
        );
    }

    #[test]
    fn key_normalisation_survives_case_slash_and_a_rescan() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        store
            .put(series_override(&win('M', r"\2SERIES\Zentaro")))
            .expect("latch the show");
        store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect("latch the movie");

        // Case and separator differences still hit.
        assert!(store.lookup_series(&win('m', "/2series/ZENTARO/ep.mkv")).is_some());
        assert!(store.lookup_movie(&win('d', r"\films\dune part two.MKV")).is_some());
        // The rescan case: the episode is re-indexed under a Season folder
        // below the latched show folder.
        assert!(
            store
                .lookup_series(&win('M', r"\2SERIES\Zentaro\Season 2\01.mkv"))
                .is_some()
        );

        // Odd paths never panic and never match. The UNC probe and the
        // drive probe are built from parts so no line carries the shape.
        let unc_probe = [r"\\", r"server\share"].concat();
        let dotdot_probe = win('C', r"\..\..\ep.mkv");
        for odd in [
            "",
            "01.mkv",
            "C:",
            "/",
            unc_probe.as_str(),
            dotdot_probe.as_str(),
        ] {
            assert!(store.lookup_series(odd).is_none(), "series lookup {odd:?}");
            assert!(store.lookup_movie(odd).is_none(), "movie lookup {odd:?}");
            assert!(store.resolve_for(odd).is_none(), "resolve {odd:?}");
        }
    }

    #[test]
    fn series_lookup_walks_ancestors_and_the_deepest_latch_wins() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        store
            .put(series_override(&win('M', r"\2SERIES\Zentaro")))
            .expect("outer latch");
        let outer = normalize_key(&win('M', r"\2SERIES\Zentaro"));

        // Directly in the show folder...
        assert_eq!(
            store
                .lookup_series(&win('M', r"\2SERIES\Zentaro\01.mkv"))
                .expect("direct hit")
                .key,
            outer
        );
        // ...and under a Season folder both hit the same outer latch.
        assert_eq!(
            store
                .lookup_series(&win('M', r"\2SERIES\Zentaro\Season 2\07.mkv"))
                .expect("season hit")
                .key,
            outer
        );

        // An inner latch beats the outer one for files under it...
        store
            .put(series_override(&win('M', r"\2SERIES\Zentaro\Season 2")))
            .expect("inner latch");
        assert_eq!(
            store
                .lookup_series(&win('M', r"\2SERIES\Zentaro\Season 2\07.mkv"))
                .expect("inner hit")
                .key,
            normalize_key(&win('M', r"\2SERIES\Zentaro\Season 2"))
        );
        // ...while a file outside it still gets the outer latch.
        assert_eq!(
            store
                .lookup_series(&win('M', r"\2SERIES\Zentaro\Season 3\01.mkv"))
                .expect("outer hit")
                .key,
            outer
        );

        // A sibling folder is NOT under the latched folder: no prefix sweep.
        assert!(
            store
                .lookup_series(&win('M', r"\2SERIES\Zentaro Two\Season 2\07.mkv"))
                .is_none()
        );
    }

    #[test]
    fn movie_override_beats_the_series_override_for_the_same_file() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        store
            .put(series_override(&win('D', r"\Shows\Show")))
            .expect("series latch");
        store
            .put(movie_override(&win('D', r"\Shows\Show\Special.mkv")))
            .expect("movie latch");

        let resolved = store.resolve_for(&win('d', "/shows/show/SPECIAL.mkv")).expect("a latch wins");
        assert_eq!(resolved.scope, Scope::Movie, "the exact-file movie latch");
        let hinted = store
            .resolve_for(&win('D', r"\Shows\Show\Special.mkv"))
            .expect("movie latch");
        assert_eq!(hinted.scope, Scope::Movie);
        // A movie-classified file still takes an explicit show-folder latch:
        // the filename classification never vetoes a correction.
        assert_eq!(
            store
                .resolve_for(&win('D', r"\Shows\Show\other.mkv"))
                .expect("series latch")
                .scope,
            Scope::Series
        );
        // ...while an episode-classified file falls through to it.
        assert_eq!(
            store
                .resolve_for(&win('D', r"\Shows\Show\Season 1\ep.mkv"))
                .expect("series latch")
                .scope,
            Scope::Series
        );
    }

    #[test]
    fn series_key_steps_over_season_folders_and_normalises() {
        assert_eq!(
            series_key_for(&win('M', r"\2SERIES\Zentaro\Season 1\01.mkv")).as_deref(),
            Some(win('m', r"\2series\zentaro").as_str())
        );
        assert_eq!(
            series_key_for(&win('M', r"\2SERIES\Zentaro\S02\01.mkv")).as_deref(),
            Some(win('m', r"\2series\zentaro").as_str())
        );
        assert_eq!(
            series_key_for(&win('M', r"\2SERIES\Zentaro\01.mkv")).as_deref(),
            Some(win('m', r"\2series\zentaro").as_str())
        );
        assert_eq!(
            series_key_for(&win('M', r"\2SERIES\Zentaro\Featurettes\clip.mkv")).as_deref(),
            Some(win('m', r"\2series\zentaro\featurettes").as_str())
        );
        assert_eq!(series_key_for("01.mkv"), None);
        assert_eq!(series_key_for(""), None);
        assert_eq!(movie_key(&win('D', r"\Films\Dune.MKV")), win('d', r"\films\dune.mkv"));
        assert_eq!(
            normalize_key(&[r"\\", r"SERVER\Share\Show\"].concat()),
            r"\server\share\show"
        );
        assert!(now_ms() > 1_600_000_000_000, "wall clock after 2020");
    }

    #[test]
    fn default_path_follows_the_config_directory_convention() {
        let path = default_path();
        match std::env::var("LOCALAPPDATA") {
            Ok(root) if !root.is_empty() => assert_eq!(
                path,
                Path::new(&root).join(CONFIG_DIR_NAME).join(STORE_FILE_NAME)
            ),
            _ => assert!(path.ends_with(STORE_FILE_NAME)),
        }
    }

    /// A v1 envelope -- no `poster`, no `episodes` -- still loads and its
    /// latches are honoured as-is; the first save after the load writes the v2
    /// envelope in place.
    #[test]
    fn v1_store_loads_and_is_honoured_and_the_next_save_writes_v2() {
        let dir = TempDir::new();
        let path = PathBuf::from(dir.child(STORE_FILE_NAME));
        let v1_key = ["m:", r"\2series", r"\zentaro"].concat();
        let v1 = serde_json::to_vec(&serde_json::json!({"version":1,"entries":[{"scope":"series","key":v1_key,"title":"Zentaro","year":2024,"provider":"tvmaze","providerId":"77","createdAtMs":1700000000000i64}]})).expect("seed v1 json");
        std::fs::write(&path, v1).expect("seed v1 store");

        let mut store = load_from(&path).expect("a v1 store loads");
        let off = store
            .lookup_series(&win('M', r"\2SERIES\Zentaro\Season 1\01.mkv"))
            .expect("the v1 latch is honoured");
        assert_eq!(off.scope, Scope::Series);
        assert_eq!(off.title, "Zentaro");
        assert!(off.poster.is_empty(), "a v1 entry has no poster");
        assert!(off.episodes.is_empty(), "a v1 entry has no episode pins");
        assert_eq!(
            store
                .resolve_for(&win('M', r"\2SERIES\Zentaro\Season 1\01.mkv"))
                .expect("resolve honours the v1 latch")
                .scope,
            Scope::Series
        );

        // The next save (here a put) upgrades the envelope to v2 and writes
        // the whole store, v1 entry included.
        store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect("save");
        let wire: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("wire json");
        assert_eq!(wire["version"], serde_json::json!(2));
        assert_eq!(wire["entries"].as_array().expect("entries").len(), 2);
        let reloaded = load_from(&path).expect("reload");
        assert_eq!(
            reloaded
                .lookup_series(&win('M', r"\2SERIES\Zentaro\S01\01.mkv"))
                .expect("the v1 latch survives the upgrade")
                .title,
            "Zentaro"
        );
    }

    /// A movie latch beats both; a season latch beats the show latch for its
    /// own season only; the show latch still covers seasons without one.
    /// Pinning the show AFTER the season must not replace the season latch.
    #[test]
    fn season_latch_beats_the_show_latch_for_its_season_and_the_movie_latch_beats_both() {
        let dir = TempDir::new();
        let path = PathBuf::from(dir.child(STORE_FILE_NAME));
        let mut store = Store::empty_at(path);

        // The explicit season latch first, the show latch after it: the order
        // of the user's actions must not change the answer.
        store
            .put(season_override(&win('M', r"\2SERIES\Attack on Titan\Season 2")))
            .expect("season latch");
        store
            .put(series_override(&win('M', r"\2SERIES\Attack on Titan")))
            .expect("show latch");

        let season_file = win('M', r"\2SERIES\Attack on Titan\Season 2\S02E01.mkv");
        let resolved = store.resolve_for(&season_file).expect("season latch");
        assert_eq!(
            resolved.scope,
            Scope::Season,
            "the season latch wins its own season"
        );
        assert_eq!(
            resolved.key,
            normalize_key(&win('M', r"\2SERIES\Attack on Titan\Season 2"))
        );

        // A season with no latch of its own falls through to the show latch.
        let other_file = win('M', r"\2SERIES\Attack on Titan\Season 3\S03E01.mkv");
        let show = store.resolve_for(&other_file).expect("show latch");
        assert_eq!(show.scope, Scope::Series);
        assert_eq!(show.key, normalize_key(&win('M', r"\2SERIES\Attack on Titan")));

        // A file already classified as a movie still consults the latches: an
        // explicit season or series correction wins over the filename, while
        // the exact-file movie latch (pinned below) still beats both.
        assert_eq!(
            store
                .resolve_for(&season_file)
                .expect("season latch")
                .scope,
            Scope::Season,
            "a movie-classified filename keeps the season latch"
        );

        // The exact-file movie latch beats both.
        store.put(movie_override(&season_file)).expect("movie latch");
        assert_eq!(
            store
                .resolve_for(&season_file)
                .expect("movie latch")
                .scope,
            Scope::Movie
        );
    }

    /// A season pin keys the folder that DIRECTLY holds the episode: the
    /// `Season N` / `SNN` folder itself when there is one, otherwise the
    /// release-style folder that IS the season -- never the show above it.
    #[test]
    fn season_key_keys_the_direct_container_and_never_steps_over_it() {
        assert_eq!(
            season_key_for(&win('M', r"\2SERIES\Attack on Titan\Season 2\S02E01.mkv")).as_deref(),
            Some(win('m', r"\2series\attack on titan\season 2").as_str())
        );
        assert_eq!(
            season_key_for(&win('M', r"\2SERIES\Attack on Titan\S02\S02E01.mkv")).as_deref(),
            Some(win('m', r"\2series\attack on titan\s02").as_str())
        );
        // Season-as-folder (release-style) layout: the folder holding the
        // episode IS the season, so it keys itself.
        assert_eq!(
            season_key_for(&win('M', r"\2SERIES\Atri S01 1080p BDRip\S01E02.mkv")).as_deref(),
            Some(win('m', r"\2series\atri s01 1080p bdrip").as_str())
        );
        // The asymmetry by contrast: the show key steps over the season
        // folder, the season key does not.
        assert_eq!(
            series_key_for(&win('M', r"\2SERIES\Attack on Titan\Season 2\S02E01.mkv")).as_deref(),
            Some(win('m', r"\2series\attack on titan").as_str())
        );
        // No containing folder at all: no season key.
        assert_eq!(season_key_for("S02E01.mkv"), None);
        assert_eq!(season_key_for(""), None);
    }

    /// Episode pins round-trip whole (title and synopsis) and both lookups --
    /// (season, number) and absolute -- hit. The scope's own cover rides along.
    #[test]
    fn episode_pins_round_trip_and_look_up_by_pair_and_absolute() {
        let dir = TempDir::new();
        let path = PathBuf::from(dir.child(STORE_FILE_NAME));
        let mut store = Store::empty_at(path.clone());
        store
            .put(season_override(&win('M', r"\2SERIES\Attack on Titan\Season 2")))
            .expect("season latch");

        let reloaded = load_from(&path).expect("load");
        let off = reloaded
            .lookup_season(&win('M', r"\2SERIES\Attack on Titan\Season 2\S02E02.mkv"))
            .expect("season latch");
        assert_eq!(off.poster, SEASON_POSTER, "the scope's cover round-trips");

        let by_pair = off.episode_pin(2, 2).expect("(season, number) lookup");
        assert_eq!(by_pair.title, "I'm Home");
        assert_eq!(by_pair.synopsis, "The scouts return home.");
        assert_eq!(by_pair.absolute, 27);

        let by_absolute = off.episode_pin_absolute(26).expect("absolute lookup");
        assert_eq!(by_absolute.season, 2);
        assert_eq!(by_absolute.number, 1);
        assert_eq!(by_absolute.title, "Beast Titan");
        assert_eq!(by_absolute.synopsis, "A hairy giant appears.");

        // Misses are misses: an unknown episode never falls back to a pin.
        assert!(off.episode_pin(2, 9).is_none());
        assert!(off.episode_pin(9, 2).is_none());
        assert!(
            off.episode_pin(2, 0).is_none(),
            "a zero number never matches"
        );
        assert!(off.episode_pin_absolute(99).is_none());
    }

    /// `season_exceptions` lists exactly the season latches that carve
    /// exceptions out of a show latch, deterministically ordered by key.
    #[test]
    fn season_exceptions_lists_only_the_seasons_pinned_under_that_show() {
        let dir = TempDir::new();
        let path = PathBuf::from(dir.child(STORE_FILE_NAME));
        let mut store = Store::empty_at(path);
        store
            .put(series_override(&win('M', r"\2SERIES\Attack on Titan")))
            .expect("show latch");
        store
            .put(season_override(&win('M', r"\2SERIES\Attack on Titan\S02")))
            .expect("season 2");
        store
            .put(season_override(&win('M', r"\2SERIES\Attack on Titan\S03")))
            .expect("season 3");
        // Other shows' season latches and a movie latch must never leak in.
        store
            .put(season_override(&win('M', r"\2SERIES\Zentaro\S01")))
            .expect("other show's season");
        store
            .put(movie_override(&win('D', r"\Films\Dune Part Two.mkv")))
            .expect("movie latch");

        let exceptions = store.season_exceptions(&win('m', "/2series/ATTACK ON TITAN/"));
        let keys: Vec<&str> = exceptions.iter().map(|off| off.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                normalize_key(&win('M', r"\2SERIES\Attack on Titan\S02")),
                normalize_key(&win('M', r"\2SERIES\Attack on Titan\S03")),
            ],
            "only this show's seasons, ordered by key"
        );
        let other = store.season_exceptions(&win('M', r"\2SERIES\Zentaro"));
        assert_eq!(other.len(), 1, "the other show keeps its own exception");
        assert_eq!(other[0].key, normalize_key(&win('M', r"\2SERIES\Zentaro\S01")));
        assert!(
            store
                .season_exceptions(&win('M', r"\2SERIES\Some Other Show"))
                .is_empty(),
            "a show with no season latch of its own has no exceptions"
        );

        // Release-style layout: the folder is both the show and its single
        // season, so a season latch on the show key is an exception too.
        let atri = win('M', r"\2SERIES\Atri S01 1080p BDRip");
        store.put(series_override(&atri)).expect("show latch");
        store.put(season_override(&atri)).expect("season latch");
        let atri_exceptions = store.season_exceptions(&atri);
        assert_eq!(atri_exceptions.len(), 1);
        assert_eq!(atri_exceptions[0].scope, Scope::Season);
    }

    /// T1: an explicit series/season latch wins even when the query is
    /// movie-classified; only an exact-file movie latch beats it.
    #[test]
    fn explicit_series_latch_wins_over_a_movie_classified_query() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        store
            .put(series_override(&win('D', r"\Shows\Show")))
            .expect("series latch");

        // Movie-classified, yet the series latch still wins: the filename
        // classification never vetoes an explicit correction.
        let resolved = store
            .resolve_for(&win('D', r"\Shows\Show\other.mkv"))
            .expect("series latch wins over the movie hint");
        assert_eq!(resolved.scope, Scope::Series);
        // A season latch wins the same way for its own season.
        store
            .put(season_override(&win('D', r"\Shows\Show\Season 2")))
            .expect("season latch");
        let resolved = store
            .resolve_for(&win('D', r"\Shows\Show\Season 2\S02E01.mkv"))
            .expect("season latch wins over the movie hint");
        assert_eq!(resolved.scope, Scope::Season);
    }

    /// T1: a movie override still wins when present.
    #[test]
    fn movie_latch_still_wins_when_present() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        store
            .put(series_override(&win('D', r"\Shows\Show")))
            .expect("series latch");
        let file = win('D', r"\Shows\Show\Special.mkv");
        store.put(movie_override(&file)).expect("movie latch");
        assert_eq!(
            store.resolve_for(&file).expect("movie latch").scope,
            Scope::Movie
        );
    }

    /// T2-C: correcting a movie-latched file to a series clears the movie
    /// latch, so the series wins even for a movie-classified query.
    #[test]
    fn put_correction_series_clears_the_movie_latch_for_that_file() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        let file = win('D', r"\Shows\Show\Special.mkv");
        store.put(movie_override(&file)).expect("movie latch");
        assert_eq!(
            store.resolve_for(&file).expect("movie latch").scope,
            Scope::Movie
        );

        let show_key = series_key_for(&file).expect("show key");
        store
            .put_correction(&file, series_override(&show_key))
            .expect("correction saves");
        assert!(
            store.lookup_movie(&file).is_none(),
            "the old movie latch is gone"
        );
        let resolved = store.resolve_for(&file).expect("series latch");
        assert_eq!(resolved.scope, Scope::Series);
        // Downstream a series scope never renders as a movie, even when the
        // file name classifies as one.
        let hinted = store.resolve_for(&file).expect("series latch");
        assert_eq!(hinted.scope, Scope::Series);
        assert_ne!(hinted.scope, Scope::Movie, "is_movie is false downstream");
    }

    /// A single-file movie correction inside a latched show removes only the
    /// season latch for that file's own season folder: the show-wide series
    /// latch -- cover and episode pins -- survives, keeps serving every other
    /// episode, and the corrected file resolves movie.
    #[test]
    fn put_correction_movie_keeps_the_show_latch_and_its_pins() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        let show_key = win('D', r"\Shows\Show");
        store.put(series_override(&show_key)).expect("series latch");
        store
            .put(season_override(&win('D', r"\Shows\Show\Season 1")))
            .expect("season latch");
        let file = win('D', r"\Shows\Show\Season 1\S01E01.mkv");
        let sibling = win('D', r"\Shows\Show\Season 1\S01E02.mkv");
        assert_eq!(
            store.resolve_for(&file).expect("season latch").scope,
            Scope::Season
        );

        store
            .put_correction(&file, movie_override(&file))
            .expect("correction saves");
        assert!(
            store.lookup_season(&file).is_none(),
            "the season latch for that path is gone"
        );
        // The show-wide latch survives with its cover and episode pins: a
        // one-file correction must not orphan the whole show.
        let show = store.lookup_series(&file).expect("show latch survives");
        assert_eq!(show.scope, Scope::Series);
        assert_eq!(show.poster, SHOW_POSTER, "the show cover survives");
        assert_eq!(show.episodes.len(), 1, "the episode pins survive");
        assert_eq!(show.episodes[0].title, "Breakage");
        assert_eq!(
            store.resolve_for(&file).expect("movie latch").scope,
            Scope::Movie,
            "the corrected file resolves movie"
        );
        assert_eq!(
            store.resolve_for(&sibling).expect("series latch").scope,
            Scope::Series,
            "a sibling episode still resolves through the surviving show latch"
        );
    }

    /// T2-C: a correction touches only its own path; another show/file keeps
    /// its latch.
    #[test]
    fn put_correction_leaves_other_paths_untouched() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        let file_a = win('D', r"\Shows\Alpha\movie.mkv");
        let file_b = win('D', r"\Shows\Beta\episode.mkv");
        store.put(movie_override(&file_a)).expect("movie latch A");
        store.put(movie_override(&file_b)).expect("movie latch B");

        let show_b = series_key_for(&file_b).expect("show key B");
        store
            .put_correction(&file_b, series_override(&show_b))
            .expect("correction saves");
        assert!(
            store.lookup_movie(&file_a).is_some(),
            "the first path keeps its movie latch"
        );
        assert_eq!(
            store.resolve_for(&file_a).expect("movie latch A").scope,
            Scope::Movie
        );
        assert!(store.lookup_movie(&file_b).is_none());
        assert_eq!(
            store.resolve_for(&file_b).expect("series latch B").scope,
            Scope::Series
        );
    }

    /// T2-C: a failed correction save rolls every in-memory change back, so
    /// the store still matches the last successful save.
    #[cfg(windows)]
    #[test]
    #[allow(clippy::permissions_set_readonly_false)]
    fn put_correction_failure_leaves_the_prior_store_intact() {
        let dir = TempDir::new();
        let base = PathBuf::from(dir.path());
        let path = base.join(STORE_FILE_NAME);
        let mut store = Store::empty_at(path.clone());
        let file = win('D', r"\Shows\Show\Special.mkv");
        store.put(movie_override(&file)).expect("first save");
        let before = std::fs::read(&path).expect("read previous store");

        let mut perms = std::fs::metadata(&path).expect("stat store").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).expect("make the store read-only");
        let show_key = series_key_for(&file).expect("show key");
        let err = store
            .put_correction(&file, series_override(&show_key))
            .expect_err("a correction over a read-only file must fail");
        assert!(!err.to_string().is_empty(), "the failure is reported");

        assert_eq!(
            std::fs::read(&path).expect("read after"),
            before,
            "the previous file changed by a failed correction"
        );
        assert_eq!(
            dir_names(&base),
            vec![STORE_FILE_NAME.to_string()],
            "temp litter after a failed correction"
        );
        // The failed correction rolled back: memory still matches the last
        // good save.
        assert!(store.lookup_movie(&file).is_some());
        assert!(store.lookup_series(&file).is_none());
        assert_eq!(
            store.resolve_for(&file).expect("movie latch").scope,
            Scope::Movie
        );

        let mut perms = std::fs::metadata(&path).expect("stat store").permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).expect("clear the read-only attribute");
        store
            .put_correction(&file, series_override(&show_key))
            .expect("save after clearing");
        assert!(store.lookup_movie(&file).is_none());
        assert_eq!(
            store.resolve_for(&file).expect("series latch").scope,
            Scope::Series
        );
    }

    #[test]
    fn put_correction_series_and_season_retire_each_other() {
        let dir = TempDir::new();
        let mut store = Store::empty_at(PathBuf::from(dir.child(STORE_FILE_NAME)));
        let season_key = win('D', r"\Shows\Show\Season 1");
        let show_key = win('D', r"\Shows\Show");
        let file = win('D', r"\Shows\Show\Season 1\S01E01.mkv");
        store
            .put(season_override(&season_key))
            .expect("season latch");
        store
            .put_correction(&file, series_override(&show_key))
            .expect("series correction");
        assert!(
            store.lookup_season(&file).is_none(),
            "a series correction retires the season latch"
        );
        assert_eq!(
            store.resolve_for(&file).expect("series latch").scope,
            Scope::Series,
            "the whole show wins after its correction"
        );
        store
            .put_correction(&file, season_override(&season_key))
            .expect("season correction");
        assert!(
            store.lookup_series(&file).is_none(),
            "a season correction retires the series latch"
        );
        assert_eq!(
            store.resolve_for(&file).expect("season latch").scope,
            Scope::Season,
            "the season wins after its correction"
        );
    }
}
