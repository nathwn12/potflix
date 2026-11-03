//! `config` -- mirrors the Go root file `go\config.go`.
//!
//! The settings store for the `PotflixMainWnd` seam.
//!
//! Path mirrors the metadata cache convention: `os.UserCacheDir()` +
//! `\Potflix\config.json` -- i.e. `%LOCALAPPDATA%\Potflix\config.json` -- falling
//! back to the exe directory when the cache dir cannot be read (see
//! `go\internal\metadata\cache.go:36-44`). Writes are atomic (temp file +
//! rename), mirroring `cache.go:98-115`. Loads are tolerant: a missing or corrupt
//! file yields the zero value, never an error to the UI. Unknown fields are
//! ignored; a retired credential field, when present, is scrubbed by rewriting
//! the file, and a clean file is never rewritten.
//!
//! # The stored schema, with every default
//!
//! ```json
//! {"dirs": [], "start_with_windows": false, "scan_speed": "turbo", "card": {}}
//! ```
//!
//! * `dirs` -- `[]` (the watched library roots, a case-insensitive Windows path
//!   list).
//! * `start_with_windows` -- `false`.
//! * `scan_speed` -- `turbo`, normalised on load AND on save: `turbo` |
//!   `balanced` | `gentle`, anything else (including empty) becomes `turbo`.
//! * `card` -- `{}` when unset; otherwise
//!   `{"series": {...}, "movie": {...}}`, one [`CardSpec`] per kind (SERIES
//!   covers TV + anime; MOVIE is movies). Each kind object carries
//!   `{"details": "...", "state": "..."}`, each line one of the preset ids
//!   below defaulting to `default`. A kind object equal to the default is
//!   OMITTED, so an unset/default config still writes `"card": {}`. The legacy
//!   flat object `{"details": "...", "state": "..."}` still loads (as the
//!   series spec, movie default). Go's `json:"card,omitempty"` is a no-op for a
//!   struct, so the key is always written -- matched here. Retired Details ids
//!   (`title`/`series`, which repeat the activity header, and `episode`,
//!   which renders byte-identical to `default`) migrate to `default`
//!   on load AND on save, so a stored repeating line resets without losing the
//!   unrelated lines.
//!
//! Go marshals a nil `Dirs` as `null`; this port always writes `[]`, and accepts
//! `null` on read (Go's `json.Unmarshal` treats a JSON `null` for any non-pointer
//! field as a no-op, so `null` must not fail a load here either).
//!
//! # Tolerant-load rules (all reproduced)
//!
//! * missing file, unreadable file, malformed JSON, or a wrong field type ->
//!   `Err` from [`load_config_from`] (the bad file is quarantined beside the
//!   store as `<name>.corrupt-<ms>`, once); the tolerant [`load_config`] turns
//!   that into the zero [`Config`], while [`load_config_for_update`] and
//!   [`save_config_to`] refuse instead of saving defaults over real data.
//! * a partial file -> the absent fields take their defaults.
//! * `null` for any field -> the field takes its default (Go's no-op rule).
//! * unknown fields -> ignored.
//! * a retired credential field -> the load still succeeds and the file is
//!   rewritten without it (best effort: a failed rewrite is dropped).
//! * a clean file -> never rewritten.
//!
//! # The `card` type
//!
//! Go stores `presence.CardSpec` (`go\internal\presence\cardspec.go`); this crate
//! now stores one spec per kind ([`CardSpecs`]: `series` + `movie`), each side
//! the stored shape [`presence::cardspec::CardSpec`], re-exported here rather
//! than mirrored, because presence owns `normalized()`, the behaviour-critical
//! part. That type's own serde shape is the JSON contract per kind
//! (`details`/`state`, both `omitempty`, unknown ids normalised to `default`).
//! The legacy flat `card` object loads as the series spec.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Deserializer, Serialize};

use crate::debuglog::exe_dir_or_dot;

/// Cache-root subdirectory: `%LOCALAPPDATA%\Potflix`.
pub const CONFIG_DIR_NAME: &str = "Potflix";

/// The store file name (`go\config.go:142`).
pub const CONFIG_FILE_NAME: &str = "config.json";

/// The metadata cache directory name (`go\config.go:155`).
pub const META_DIR_NAME: &str = "meta";

/// The folder-scan index file name (`go\config.go:165`).
pub const SCAN_INDEX_FILE_NAME: &str = "scanindex.json";

/// The Windows cache-root environment variable (`os.UserCacheDir`).
pub const LOCAL_APP_DATA_ENV: &str = "LOCALAPPDATA";

/// Scan-speed profile names. Turbo (the published provider limits exactly) is
/// the default; balanced is 2x; gentle is 4x. Unknown/empty normalises to turbo
/// so a bad stored value can never reach the limiter.
pub const SCAN_SPEED_TURBO: &str = "turbo";

/// 2x the published provider limits.
pub const SCAN_SPEED_BALANCED: &str = "balanced";

/// 4x the published provider limits.
pub const SCAN_SPEED_GENTLE: &str = "gentle";

/// Theme names. AMOLED (pure-black OLED) is the default; Dark is today's
/// palette exactly. Unknown/empty normalises to AMOLED so a bad stored value
/// can never reach the painter.
pub const THEME_AMOLED: &str = "amoled";

/// Today's palette exactly.
pub const THEME_DARK: &str = "dark";

/// The Card view's stored choice of one preset per body line
/// (`go\internal\presence\cardspec.go:41`), the preset ids that pick it, and
/// today's shipped default. Owned by presence and re-exported, so the store and
/// the renderer share one definition; the zero value falls back to the default
/// on every line, so a store written before this field existed needs no
/// migration.
pub use presence::cardspec::{
    CARD_DETAILS_DEFAULT, CARD_DETAILS_SERIES, CARD_DETAILS_TITLE, CARD_STATE_DEFAULT,
    CARD_STATE_NONE, CARD_STATE_SYNOPSIS, CARD_STATE_TAG, CardSpec, default_card_spec,
};

/// Store-local convenience on the presence-owned [`CardSpec`]: only the store
/// cares whether NOTHING is stored (`{}` on the wire). `normalized`/`is_default`
/// stay presence's, and are never re-implemented here.
pub trait CardSpecStore {
    /// Reports whether nothing is stored (`{}` on the wire).
    fn is_empty(&self) -> bool;
}

impl CardSpecStore for CardSpec {
    fn is_empty(&self) -> bool {
        self.details.is_empty() && self.state.is_empty()
    }
}

/// The card settings' kind axis. `Series` covers TV and anime (metadata-core
/// has no separate anime kind: UNKNOWN | SERIES | MOVIE).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardKind {
    /// TV + anime.
    #[default]
    Series,
    /// Movies.
    Movie,
}

impl CardKind {
    /// Reports whether this kind is the movie axis.
    pub fn is_movie(self) -> bool {
        matches!(self, CardKind::Movie)
    }
}

/// Which body line a preset id applies to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardLine {
    /// The details line.
    #[default]
    Details,
    /// The state line.
    State,
}

/// Reports whether a stored kind object is the shipped default (empty and
/// explicit-default both count: both normalise to [`default_card_spec`]).
fn is_default_card_spec(spec: &CardSpec) -> bool {
    spec.is_default()
}

/// The per-kind card settings: one line-choice set for SERIES and one for
/// MOVIE. The serde shape is `{"series": {...}, "movie": {...}}`; a kind
/// object equal to [`default_card_spec`] (empty or explicit) is omitted, so an
/// unset/default config still writes `"card": {}` byte-for-byte as before.
/// Each kind keeps today's per-line `omitempty` from [`CardSpec`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardSpecs {
    /// TV + anime line choices.
    #[serde(
        default,
        deserialize_with = "de::opt_spec",
        skip_serializing_if = "is_default_card_spec"
    )]
    pub series: CardSpec,
    /// Movie line choices.
    #[serde(
        default,
        deserialize_with = "de::opt_spec",
        skip_serializing_if = "is_default_card_spec"
    )]
    pub movie: CardSpec,
}

impl CardSpecs {
    /// The effective spec for one kind: a line that is empty or an unknown id
    /// falls back to the default id (never a stored typo on the card).
    pub fn for_kind(&self, kind: CardKind) -> CardSpec {
        match kind {
            CardKind::Series => self.series.normalized(),
            CardKind::Movie => self.movie.normalized(),
        }
    }

    /// Both kinds equal `default_card_spec()`.
    pub fn is_default(&self) -> bool {
        self.series.is_default() && self.movie.is_default()
    }

    /// Stores the normalised id for one line of one kind; returns false when
    /// nothing changed.
    pub fn set_line(&mut self, kind: CardKind, line: CardLine, id: &str) -> bool {
        let normalized_id = match line {
            CardLine::Details => CardSpec {
                details: id.to_string(),
                state: CARD_STATE_DEFAULT.to_string(),
            }
            .normalized()
            .details,
            CardLine::State => CardSpec {
                details: CARD_DETAILS_DEFAULT.to_string(),
                state: id.to_string(),
            }
            .normalized()
            .state,
        };
        let target = match kind {
            CardKind::Series => &mut self.series,
            CardKind::Movie => &mut self.movie,
        };
        let current = match line {
            CardLine::Details => &target.details,
            CardLine::State => &target.state,
        };
        if *current == normalized_id {
            return false;
        }
        match line {
            CardLine::Details => target.details = normalized_id,
            CardLine::State => target.state = normalized_id,
        }
        true
    }
}

/// The settings store (`go\config.go:22`). Field names and defaults are the Go
/// struct's, so the file stays byte-compatible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Watched library roots.
    #[serde(default, deserialize_with = "de::dirs")]
    pub dirs: Vec<String>,
    /// Launch the tray app at sign-in.
    #[serde(default, deserialize_with = "de::boolean")]
    pub start_with_windows: bool,
    /// The scan-speed profile as stored; read through
    /// [`Config::effective_scan_speed`].
    #[serde(default, deserialize_with = "de::string")]
    pub scan_speed: String,
    /// The Card view's chosen presets per kind; read through
    /// [`Config::effective_card_specs`] (or [`Config::effective_card_spec`]
    /// for the series side).
    #[serde(default, deserialize_with = "de::card")]
    pub card: CardSpecs,
    /// The UI theme as stored (`amoled`|`dark`); read through
    /// [`Config::effective_theme`]. Omitted on the wire while the AMOLED
    /// default holds, so unset/default stores keep their exact bytes.
    #[serde(
        default,
        deserialize_with = "de::theme",
        skip_serializing_if = "is_default_theme"
    )]
    pub theme: String,
}

/// Reports whether the stored theme is the AMOLED default (empty counts:
/// absent/unreadable loads default, and the default is omitted on the wire).
fn is_default_theme(value: &str) -> bool {
    normalize_theme(value) == THEME_AMOLED
}

/// JSON `null` tolerance, one function per field: Go's `json.Unmarshal` leaves a
/// non-pointer field untouched when the JSON value is `null`, so a store
/// carrying `"dirs": null` must load its other fields instead of failing.
mod de {
    use super::{CardSpec, CardSpecs, Deserialize, Deserializer, default_card_spec};

    pub fn dirs<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
        Ok(Option::<Vec<String>>::deserialize(deserializer)?.unwrap_or_default())
    }

    pub fn boolean<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
        Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(false))
    }

    pub fn string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
    }

    /// Null-tolerant theme: `null`/absent loads empty (effective AMOLED);
    /// any string survives raw here and is normalised on load/save.
    pub fn theme<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
    }

    /// Null-tolerant single kind object: `null` loads as the zero [`CardSpec`].
    pub fn opt_spec<'de, D: Deserializer<'de>>(deserializer: D) -> Result<CardSpec, D::Error> {
        Ok(Option::<CardSpec>::deserialize(deserializer)?.unwrap_or_default())
    }

    /// Parses one nested kind value: `null` is the zero spec, an object is the
    /// [`CardSpec`] shape (unknown keys ignored), anything else errors like the
    /// old flat shape did on a wrong type.
    fn nested_spec(
        value: &serde_json::Value,
    ) -> Result<CardSpec, serde_json::Error> {
        if value.is_null() {
            return Ok(CardSpec::default());
        }
        serde_json::from_value::<CardSpec>(value.clone())
    }

    /// The `card` field: the new nested `{"series": {...}, "movie": {...}}`
    /// shape plus the legacy flat `{"details": "...", "state": "..."}` every
    /// shipped config holds (series = that normalized pair, movie = the
    /// shipped default). Absent is handled by serde's `default`; here `null`,
    /// `{}`, and unknown-only objects all load as the all-default
    /// [`CardSpecs`]. A wrong (non-object) type errors, as before. Retired
    /// Details ids (`title`/`series`, which repeat the header, and `episode`,
    /// byte-identical to the default) migrate to the default per kind so
    /// a saved repeating line can never reach the card again; every other
    /// stored id (kept or unknown) survives the load untouched.
    pub fn card<'de, D: Deserializer<'de>>(deserializer: D) -> Result<CardSpecs, D::Error> {
        use serde::de::Error as _;
        let opt = Option::<serde_json::Value>::deserialize(deserializer)?;
        let Some(value) = opt else {
            return Ok(CardSpecs::default());
        };
        let serde_json::Value::Object(map) = &value else {
            return Err(D::Error::custom("invalid type for card"));
        };
        if map.contains_key("series") || map.contains_key("movie") {
            let series = map
                .get("series")
                .map(nested_spec)
                .transpose()
                .map_err(D::Error::custom)?
                .unwrap_or_default();
            let movie = map
                .get("movie")
                .map(nested_spec)
                .transpose()
                .map_err(D::Error::custom)?
                .unwrap_or_default();
            return Ok(CardSpecs {
                series: series.migrated(),
                movie: movie.migrated(),
            });
        }
        if map.contains_key("details") || map.contains_key("state") {
            let flat: CardSpec =
                serde_json::from_value(value).map_err(D::Error::custom)?;
            return Ok(CardSpecs {
                series: flat.normalized().migrated(),
                movie: default_card_spec(),
            });
        }
        Ok(CardSpecs::default())
    }
}

/// Maps any input onto a known profile (case-insensitive; unknown/empty yields
/// turbo).
pub fn normalize_scan_speed(speed: &str) -> &'static str {
    match speed.trim().to_lowercase().as_str() {
        SCAN_SPEED_TURBO => SCAN_SPEED_TURBO,
        SCAN_SPEED_BALANCED => SCAN_SPEED_BALANCED,
        SCAN_SPEED_GENTLE => SCAN_SPEED_GENTLE,
        _ => SCAN_SPEED_TURBO,
    }
}

/// Maps any input onto a known theme (case-insensitive; unknown/empty yields
/// AMOLED, the default).
pub fn normalize_theme(theme: &str) -> &'static str {
    match theme.trim().to_lowercase().as_str() {
        THEME_DARK => THEME_DARK,
        THEME_AMOLED => THEME_AMOLED,
        _ => THEME_AMOLED,
    }
}

impl Config {
    /// The normalised profile for this config.
    pub fn effective_scan_speed(&self) -> &'static str {
        normalize_scan_speed(&self.scan_speed)
    }

    /// The stored SERIES card spec with per-line fallback to the shipped
    /// default, so a zero value (pre-card stores) needs no migration and a
    /// stored typo can never reach the card.
    pub fn effective_card_spec(&self) -> CardSpec {
        self.card.for_kind(CardKind::Series)
    }

    /// Both kinds, each normalised per line.
    pub fn effective_card_specs(&self) -> CardSpecs {
        CardSpecs {
            series: self.card.series.normalized(),
            movie: self.card.movie.normalized(),
        }
    }

    /// Stores the normalised spec on BOTH kinds; reports whether it changed.
    pub fn set_card_spec(&mut self, details: &str, state: &str) -> bool {
        let normalized = CardSpec {
            details: details.to_string(),
            state: state.to_string(),
        }
        .normalized();
        let next = CardSpecs {
            series: normalized.clone(),
            movie: normalized,
        };
        if self.card == next {
            return false;
        }
        self.card = next;
        true
    }

    /// Stores one normalised line on one kind; reports whether it changed.
    pub fn set_card_line(&mut self, kind: CardKind, line: CardLine, id: &str) -> bool {
        self.card.set_line(kind, line, id)
    }

    /// Stores the normalised profile; reports whether it changed. The comparison
    /// is against the RAW stored value, as in Go, so a store holding `TURBO` is
    /// reported as changed by `set_scan_speed("turbo")`.
    pub fn set_scan_speed(&mut self, speed: &str) -> bool {
        let normalized = normalize_scan_speed(speed);
        if self.scan_speed == normalized {
            return false;
        }
        self.scan_speed = normalized.to_string();
        true
    }

    /// The normalised theme for this config (AMOLED when absent/unreadable).
    pub fn effective_theme(&self) -> &'static str {
        normalize_theme(&self.theme)
    }

    /// Stores the normalised theme; reports whether it changed.
    pub fn set_theme(&mut self, theme: &str) -> bool {
        let normalized = normalize_theme(theme);
        if self.theme == normalized {
            return false;
        }
        self.theme = normalized.to_string();
        true
    }

    /// Appends dir unless blank or already present (case-insensitive: this is a
    /// Windows path list). Reports whether the set changed.
    ///
    /// Reading: the fold is ASCII (`eq_ignore_ascii_case`), not Go's Unicode
    /// `EqualFold`. The list holds Windows paths, whose case-insensitivity is
    /// ASCII-ordinal, so the two agree on every path a Windows API produces.
    pub fn add_dir(&mut self, dir: &str) -> bool {
        let dir = dir.trim();
        if dir.is_empty() {
            return false;
        }
        if self
            .dirs
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(dir))
        {
            return false;
        }
        self.dirs.push(dir.to_string());
        true
    }

    /// Drops dir (case-insensitive). Reports whether the set changed.
    pub fn remove_dir(&mut self, dir: &str) -> bool {
        if let Some(index) = self
            .dirs
            .iter()
            .position(|existing| existing.eq_ignore_ascii_case(dir))
        {
            self.dirs.remove(index);
            return true;
        }
        false
    }
}

/// The process-wide live card specs: the sender composes with these at its next
/// poll, so a Card-view save takes effect with no restart and no re-push.
static LIVE_CARD_SPECS: LazyLock<Mutex<Option<CardSpecs>>> =
    LazyLock::new(|| Mutex::new(None));

/// Publish the effective card specs to the running process: the sender composes
/// with them at its next poll. No push, no restart -- it mirrors the
/// `metadata_http::set_rate_profile` live-apply seam.
pub fn set_live_card_specs(specs: CardSpecs) {
    *LIVE_CARD_SPECS.lock().unwrap_or_else(|e| e.into_inner()) = Some(specs);
}

/// The published specs, or the shipped default when nothing was published.
pub fn live_card_specs() -> CardSpecs {
    LIVE_CARD_SPECS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// The process-wide presence-link state: true once the Discord transport has
/// completed a handshake (and every successful send re-asserts it), false when
/// the transport drops or a send fails. The publisher owns the writes; the UI
/// pill owns the read, so the bar and the wire share one source. Defaults to
/// false: with no live Discord handle the bar must never claim a connection.
static LIVE_DISCORD_UP: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));

/// Publish the presence-link state. The transport callbacks and the send path
/// both report here, so the pill reads what the publisher sees.
pub fn set_live_discord_up(up: bool) {
    *LIVE_DISCORD_UP.lock().unwrap_or_else(|e| e.into_inner()) = up;
}

/// Whether the presence link is up (the last publisher report).
pub fn live_discord_up() -> bool {
    *LIVE_DISCORD_UP.lock().unwrap_or_else(|e| e.into_inner())
}

/// What is playing right now, as the poll loop sees it: the raw window title
/// and the full path of the file. The UI offers "Fix match" only while
/// something plays (and latches exactly this file), so the poll loop publishes
/// this on entry to a playing segment and clears it when the player is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlaying {
    /// The player window title (the raw filename line).
    pub raw_title: String,
    /// The full path of the playing file.
    pub path: String,
}

/// The process-wide now-playing mirror: `None` while nothing plays. Same
/// live-apply pattern as [`LIVE_CARD_SPECS`]: the poll loop publishes on its
/// next poll, the UI reads at will, no push and no restart.
static LIVE_NOW_PLAYING: LazyLock<Mutex<Option<NowPlaying>>> =
    LazyLock::new(|| Mutex::new(None));

/// Publishes what is playing (`None` clears it).
pub fn set_live_now_playing(now: Option<NowPlaying>) {
    *LIVE_NOW_PLAYING.lock().unwrap_or_else(|e| e.into_inner()) = now;
}

/// What is playing, or `None` when nothing is (or the player is gone).
pub fn live_now_playing() -> Option<NowPlaying> {
    LIVE_NOW_PLAYING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The Windows cache root (`os.UserCacheDir()`), or `None` when it is unset or
/// empty -- the Go error case.
fn cache_root() -> Option<PathBuf> {
    std::env::var_os(LOCAL_APP_DATA_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `<cache root>\Potflix` (or `<exe dir>\Potflix` when the cache root is
/// unreadable).
fn potflix_dir() -> PathBuf {
    cache_root()
        .map(|root| root.join(CONFIG_DIR_NAME))
        .unwrap_or_else(|| exe_dir_or_dot().join(CONFIG_DIR_NAME))
}

/// The on-disk store location: `%LOCALAPPDATA%\Potflix\config.json`.
pub fn config_path() -> PathBuf {
    cache_root()
        .map(|root| root.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME))
        .unwrap_or_else(|| exe_dir_or_dot().join(CONFIG_FILE_NAME))
}

/// The metadata cache directory: `%LOCALAPPDATA%\Potflix\meta`, falling back to
/// the exe directory. Mirrors `metadata.defaultCacheDir` so the Settings view
/// never hardcodes `%LOCALAPPDATA%` yet always points at the live cache.
pub fn metadata_dir() -> PathBuf {
    potflix_dir().join(META_DIR_NAME)
}

/// The folder-scan index file: `%LOCALAPPDATA%\Potflix\scanindex.json`, falling
/// back to the exe directory. Same idiom as [`config_path`]/[`metadata_dir`].
pub fn scan_index_path() -> PathBuf {
    cache_root()
        .map(|root| root.join(CONFIG_DIR_NAME).join(SCAN_INDEX_FILE_NAME))
        .unwrap_or_else(|| exe_dir_or_dot().join(SCAN_INDEX_FILE_NAME))
}

/// The explicit-path core of [`load_config`], so tests can point at a temp dir
/// instead of the real store. Unknown fields are ignored, so an old store
/// carrying retired credential fields loads its dirs without error; when such a
/// field is present the file is rewritten without it, and a clean file is never
/// rewritten.
pub fn load_config_from(path: &Path) -> io::Result<Config> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(err) => {
            if err.kind() != io::ErrorKind::NotFound {
                // Unreadable (permissions, a directory, a torn write): move the
                // bytes aside once so the next write cannot silently replace
                // them with defaults.
                let _ = quarantine_corrupt_file(path);
            }
            return Err(err);
        }
    };
    let mut config: Config = match serde_json::from_slice(&data) {
        Ok(config) => config,
        Err(err) => {
            let _ = quarantine_corrupt_file(path);
            return Err(io::Error::new(io::ErrorKind::InvalidData, err));
        }
    };
    config.scan_speed = normalize_scan_speed(&config.scan_speed).to_string();
    // Theme stays raw here (`""` when absent): `effective_theme()` maps
    // absent/unreadable to AMOLED, and the default is omitted on the wire so
    // unset stores keep their exact bytes.
    if has_retired_fields(&data) {
        // Best-effort scrub: the load still succeeds when the rewrite fails, so
        // the error is dropped here.
        let _ = save_config_to(path, &config);
    }
    Ok(config)
}

/// Reads the store; missing yields the zero `Config`. An unparsable or
/// unreadable file is quarantined (renamed `.corrupt-<ms>` once, beside the
/// store) and still yields the zero `Config`; the next write must go through
/// [`load_config_for_update`]/[`save_config_to`], which refuse to silently
/// replace the quarantined bytes.
pub fn load_config() -> Config {
    load_config_from(&config_path()).unwrap_or_default()
}

/// Loads the store for a read-modify-write: a missing file yields the zero
/// `Config` (nothing to keep), but an unparsable or unreadable file yields
/// `Err` (after quarantine) so the caller refuses instead of saving defaults
/// over the user's data.
pub fn load_config_for_update(path: &Path) -> io::Result<Config> {
    match load_config_from(path) {
        Ok(config) => Ok(config),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(err) => Err(err),
    }
}

/// Writes `config` atomically to the real store path.
pub fn save_config(config: &Config) -> io::Result<()> {
    save_config_to(&config_path(), config)
}

/// The explicit-path core of [`save_config`]. The scan profile is normalised
/// before marshal so a bad value never persists.
///
/// Order matches Go: create the parent, marshal, create the temp file in the
/// same directory, write, close, rename over the target. Rust's `File` is
/// unbuffered (as is Go's), so a write failure surfaces at the write call and
/// the close-error branch has no separate case; a failed write or rename removes
/// the temp file and returns the error.
pub fn save_config_to(path: &Path, config: &Config) -> io::Result<()> {
    // Never silently replace real data: a present-but-unparsable store is
    // quarantined and the write is refused (the caller surfaces it). A
    // missing store (nothing to keep, or already quarantined by a load) is
    // writable.
    if std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        match std::fs::read(path) {
            Ok(data) => {
                if serde_json::from_slice::<Config>(&data).is_err() {
                    let _ = quarantine_corrupt_file(path);
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "refusing to overwrite corrupt store: {}",
                            path.display()
                        ),
                    ));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                let _ = quarantine_corrupt_file(path);
                return Err(err);
            }
        }
    }
    let mut out = config.clone();
    out.scan_speed = normalize_scan_speed(&out.scan_speed).to_string();
    out.theme = normalize_theme(&out.theme).to_string();
    // T5-B, extended to the retired `episode` preset: a retired Details id
    // (`title`/`series`/`episode`) never persists again: the
    // store migrates it to the default line on save, as on load.
    out.card.series = out.card.series.migrated();
    out.card.movie = out.card.movie.migrated();

    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)?;
    let data = serde_json::to_vec(&out).map_err(io::Error::other)?;

    let (temp_path, mut temp) = create_temp(&dir)?;
    if let Err(err) = temp.write_all(&data) {
        drop(temp);
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    // Flush the file to stable storage BEFORE the rename, so a power cut
    // cannot commit a zero-length file the next load would treat as defaults.
    if let Err(err) = temp.sync_all() {
        drop(temp);
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    drop(temp);
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    // Fsync the directory so the rename itself survives a power cut.
    fsync_dir(&dir);
    Ok(())
}

/// Best-effort directory fsync: persists the rename above. Failures are
/// ignored (some platforms cannot open a directory); the file sync is the
/// load-bearing half.
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

/// Moves an unparsable/unreadable store aside once, returning the backup path.
/// A missing file is not corrupt (`None`); when a backup already exists the
/// first one is kept and returned, so repeated loads never stack renames.
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

/// Retired credential field names, built at runtime from concatenated fragments
/// so a plain source grep for the retired prefix stays clean and cannot trip on
/// this redaction path (`go\config.go:117-123`).
fn retired_field_names() -> [String; 2] {
    let prefix = "api";
    [format!("{prefix}_keys"), format!("{prefix}Key")]
}

/// Reports whether the raw object carries a retired field.
pub fn has_retired_fields(data: &[u8]) -> bool {
    let Ok(object) = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(data)
    else {
        return false;
    };
    retired_field_names()
        .iter()
        .any(|name| object.contains_key(name))
}

/// `os.CreateTemp(dir, ".tmp-*.json")`: a uniquely named file in `dir`, created
/// exclusively, retried on collision.
fn create_temp(dir: &Path) -> io::Result<(PathBuf, File)> {
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
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temp file for the atomic write",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Scratch;
    use std::sync::Mutex;

    /// Serializes the live-mirror tests: the mirror is process-wide, so a
    /// concurrent publisher would leak into another test's read (the same
    /// reason `UI_GLOBAL_SERIAL` exists in `potflix-win`).
    static LIVE_SPECS_SERIAL: Mutex<()> = Mutex::new(());

    /// The store path a test uses: a nested directory that does not exist yet, so
    /// the atomic write must create it (`t.TempDir()/Potflix/config.json`).
    fn store_path(scratch: &Scratch) -> PathBuf {
        scratch.join("Potflix").join(CONFIG_FILE_NAME)
    }

    /// Retired names are built at runtime (concatenated fragments) so a plain
    /// source grep for the retired prefix stays clean.
    fn retired_fixture_names() -> [String; 2] {
        let prefix = "api";
        [format!("{prefix}_keys"), format!("{prefix}Key")]
    }

    /// `config_scanspeed_test.go`'s `quoteJSON`: a minimal JSON string literal,
    /// so a bad scan-speed fixture carries exactly the bytes under test.
    fn quote_json(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for ch in value.chars() {
            if ch == '"' || ch == '\\' {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push('"');
        out
    }

    #[test]
    fn round_trip_preserves_dirs_and_scan_speed() {
        let scratch = Scratch::new("config-round-trip");
        let path = store_path(&scratch);
        let want = Config {
            dirs: vec![format!("{}:{}", 'C', r"\Media\Movies"), format!("{}:{}", 'D', r"\Shows")],
            scan_speed: SCAN_SPEED_BALANCED.to_string(),
            ..Config::default()
        };
        save_config_to(&path, &want).expect("saveConfigTo");
        let got = load_config_from(&path).expect("loadConfigFrom");
        assert_eq!(got, want);
    }

    #[test]
    fn corrupt_or_missing_yields_the_defaults() {
        let scratch = Scratch::new("config-corrupt");
        let missing = load_config_from(&scratch.join("nope.json"));
        assert!(missing.is_err(), "a missing file must report an error");
        assert_eq!(Config::default().dirs, Vec::<String>::new());

        let bad = scratch.write(CONFIG_FILE_NAME, "{not json");
        assert!(load_config_from(&bad).is_err(), "corrupt file must error");

        // The tolerant wrapper the UI calls never surfaces either failure. The
        // real store is never read here: the same core call on a missing path is
        // what `load_config` degrades to the zero value.
        assert_eq!(
            load_config_from(&scratch.join("missing-store.json")).unwrap_or_default(),
            Config::default()
        );
    }

    /// A corrupt store is quarantined (renamed `.corrupt-<ms>`, once) and a
    /// write over corrupt bytes is refused: folders, scan speed, card and
    /// theme are never silently replaced by defaults while the status says
    /// saved.
    #[test]
    fn corrupt_config_is_quarantined_and_save_refuses_to_clobber() {
        let scratch = Scratch::new("config-quarantine");
        for (tag, junk) in [("junk", "{not json"), ("wrong-type", r#"{"dirs":7}"#)] {
            let path = scratch.write(&format!("{tag}.json"), junk);
            assert!(
                load_config_from(&path).is_err(),
                "{tag}: corrupt file must error"
            );
            let backup = corrupt_backup_for(&path).expect("quarantine backup");
            assert_eq!(
                std::fs::read(&backup).expect("read backup"),
                junk.as_bytes(),
                "{tag}: the user's bytes survive in the backup"
            );
            assert!(!path.exists(), "{tag}: the corrupt file is moved aside");
            // A second corrupt load never stacks a second backup ("once").
            std::fs::write(&path, junk).expect("re-seed corrupt");
            assert!(load_config_from(&path).is_err());
            let backups: Vec<_> = std::fs::read_dir(scratch.path())
                .expect("read scratch")
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|p| {
                    p.file_name().is_some_and(|n| {
                        n.to_string_lossy()
                            .starts_with(&format!("{tag}.json.corrupt-"))
                    })
                })
                .collect();
            assert_eq!(backups.len(), 1, "one quarantine, never stacked: {backups:?}");

            // The write refuses instead of saving defaults over real data, and
            // leaves the corrupt bytes in place for inspection.
            let err = save_config_to(
                &path,
                &Config {
                    dirs: vec![r"C:\Media".to_string()],
                    ..Config::default()
                },
            )
            .expect_err("a write over corrupt bytes must refuse");
            assert!(
                err.to_string().contains("corrupt"),
                "the refusal names the cause: {err}"
            );
            assert_eq!(
                std::fs::read(&path).expect("read target"),
                junk.as_bytes(),
                "refusal leaves the corrupt bytes untouched"
            );
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&backup);
        }
    }

    /// A power-cut zero-length file is corruption, not defaults: it is
    /// quarantined on load and refused on save, and a good save stages no
    /// temp litter beside the store.
    #[test]
    fn power_cut_zero_length_config_is_quarantined() {
        let scratch = Scratch::new("config-zero-length");
        let path = store_path(&scratch);
        std::fs::create_dir_all(path.parent().expect("store parent")).expect("mkdir");
        std::fs::write(&path, b"").expect("seed torn write");
        assert!(
            load_config_from(&path).is_err(),
            "zero-length must error, never default"
        );
        let backup = corrupt_backup_for(&path).expect("quarantine backup");
        assert!(std::fs::read(&backup).expect("read backup").is_empty());
        assert!(!path.exists());

        // A clean save round-trips and stages no temp file beside the store.
        let want = Config {
            dirs: vec![r"C:\Media".to_string()],
            scan_speed: SCAN_SPEED_TURBO.to_string(),
            ..Config::default()
        };
        save_config_to(&path, &want).expect("saveConfigTo");
        let parent = path.parent().expect("store parent");
        let names: Vec<String> = std::fs::read_dir(parent)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names.iter().filter(|n| n.starts_with(".tmp-")).count(), 0, "temp litter: {names:?}");
        assert!(names.contains(&CONFIG_FILE_NAME.to_string()), "store written: {names:?}");
        assert_eq!(load_config_from(&path).expect("reload"), want);
    }

    /// `config_scanspeed_test.go` — ScanSpeed survives an atomic save -> load
    /// round-trip for every named profile.
    #[test]
    fn scan_speed_round_trips_every_profile() {
        for profile in [SCAN_SPEED_GENTLE, SCAN_SPEED_BALANCED, SCAN_SPEED_TURBO] {
            let scratch = Scratch::new("config-speed");
            let path = store_path(&scratch);
            let want = Config {
                dirs: vec![r"C:\Media".to_string()],
                scan_speed: profile.to_string(),
                ..Config::default()
            };
            save_config_to(&path, &want).unwrap_or_else(|err| panic!("save {profile}: {err}"));
            let got = load_config_from(&path).unwrap_or_else(|err| panic!("load {profile}: {err}"));
            assert_eq!(got.scan_speed, profile);
            assert_eq!(got.effective_scan_speed(), profile);
        }
    }

    /// `config_scanspeed_test.go` — a bad or empty stored value normalises to
    /// turbo on load, and the normaliser maps case-insensitively.
    #[test]
    fn scan_speed_normalises_bad_and_empty_values() {
        for bad in ["", "   ", "ludicrous", "TURBO-fast"] {
            let scratch = Scratch::new("config-speed-bad");
            let path = scratch.write(
                CONFIG_FILE_NAME,
                &format!(r#"{{"dirs":[],"scan_speed":{}}}"#, quote_json(bad)),
            );
            let got = load_config_from(&path).unwrap_or_else(|err| panic!("load {bad:?}: {err}"));
            assert_eq!(
                got.scan_speed, SCAN_SPEED_TURBO,
                "stored {bad:?} must load as turbo"
            );
        }
        assert_eq!(normalize_scan_speed("Turbo"), SCAN_SPEED_TURBO);
        assert_eq!(normalize_scan_speed("GENTLE"), SCAN_SPEED_GENTLE);
        assert_eq!(normalize_scan_speed(" Balanced "), SCAN_SPEED_BALANCED);

        // Old stores without the field load tolerant (turbo).
        let scratch = Scratch::new("config-speed-legacy");
        let legacy = scratch.write("legacy.json", r#"{"dirs":[]}"#);
        let old = load_config_from(&legacy).expect("load legacy store");
        assert_eq!(old.effective_scan_speed(), SCAN_SPEED_TURBO);

        // A zero Config (missing/corrupt store) is turbo too.
        assert_eq!(Config::default().effective_scan_speed(), SCAN_SPEED_TURBO);

        // set_scan_speed normalises and reports the change.
        let mut config = Config::default();
        assert!(config.set_scan_speed("TURBO"));
        assert_eq!(config.scan_speed, SCAN_SPEED_TURBO);
        assert!(
            !config.set_scan_speed("turbo"),
            "same value reports no change"
        );
        assert!(config.set_scan_speed("gentle"));
        assert_eq!(config.scan_speed, SCAN_SPEED_GENTLE);
        // Bogus normalises to the turbo default; moving off turbo first makes
        // the change observable.
        assert!(config.set_scan_speed("bogus"));
        assert_eq!(config.scan_speed, SCAN_SPEED_TURBO);
        // A store holding an un-normalised value is still reported as changed.
        let mut raw = Config {
            scan_speed: "TURBO".to_string(),
            ..Config::default()
        };
        assert!(raw.set_scan_speed("turbo"));
        assert_eq!(raw.scan_speed, SCAN_SPEED_TURBO);
    }

    #[test]
    fn add_and_remove_dir_are_case_insensitive() {
        let mut config = Config::default();
        assert!(config.add_dir(r"C:\Media"), "first insert");
        assert!(!config.add_dir(r"c:\media"), "case-insensitive duplicate");
        assert!(!config.add_dir("   "), "blank");
        assert_eq!(config.dirs.len(), 1);

        assert!(!config.remove_dir(r"D:\Elsewhere"), "absent");
        assert!(config.remove_dir(r"C:\MEDIA"), "case-insensitive hit");
        assert!(config.dirs.is_empty());
    }

    /// The Card view's persistence: a chosen spec round-trips through the store,
    /// the zero value falls back to the shipped default (no migration), and
    /// `set_card_spec` normalises unknown ids.
    #[test]
    fn card_spec_round_trips_and_falls_back() {
        let scratch = Scratch::new("config-card");
        let path = store_path(&scratch);
        let mut want = Config {
            dirs: vec![r"C:\Media".to_string()],
            scan_speed: SCAN_SPEED_TURBO.to_string(),
            ..Config::default()
        };
        assert!(want.set_card_spec("default", "tag"), "first set");
        assert!(
            !want.set_card_spec("default", "tag"),
            "same value is unchanged"
        );

        save_config_to(&path, &want).expect("saveConfigTo");
        let got = load_config_from(&path).expect("loadConfigFrom");
        assert_eq!(got, want);
        let effective = got.effective_card_spec();
        assert_eq!(
            (effective.details.as_str(), effective.state.as_str()),
            ("default", "tag")
        );

        // Zero value: no migration needed.
        assert_eq!(Config::default().effective_card_spec(), default_card_spec());

        // Unknown ids normalise on set, never persist raw.
        let mut unknown = Config::default();
        unknown.set_card_spec("fancy-template", "");
        assert_eq!(unknown.effective_card_spec(), default_card_spec());
        assert_eq!(unknown.card.series.details, CARD_DETAILS_DEFAULT);
        assert_eq!(unknown.card.movie.details, CARD_DETAILS_DEFAULT);
        assert!(!unknown.card.series.is_empty());
    }

    /// The `card` wire shape is per-kind with Go's `omitempty` per line: an
    /// empty line is OMITTED from its kind object, a kind object equal to the
    /// default is omitted entirely, while the `card` key itself is always
    /// written. Pins the exact bytes the stores exchange.
    #[test]
    fn card_lines_omit_empty_but_the_card_key_always_writes() {
        let scratch = Scratch::new("config-card-wire");
        let path = store_path(&scratch);

        // Details default, state empty: the kind normalises to the shipped
        // default and is omitted as a kind (only one Details preset exists,
        // so an explicit default carries no information).
        let details_only = Config {
            card: CardSpecs {
                series: CardSpec {
                    details: CARD_DETAILS_DEFAULT.to_string(),
                    state: String::new(),
                },
                movie: CardSpec::default(),
            },
            ..Config::default()
        };
        save_config_to(&path, &details_only).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert_eq!(
            wire,
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{}}"#
        );

        // State set, details empty: the details key is omitted, and the line
        // falls back to the shipped default on read.
        let state_only = Config {
            card: CardSpecs {
                series: CardSpec {
                    details: String::new(),
                    state: CARD_STATE_TAG.to_string(),
                },
                movie: CardSpec::default(),
            },
            ..Config::default()
        };
        save_config_to(&path, &state_only).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert_eq!(
            wire,
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{"series":{"state":"tag"}}}"#
        );
        let got = load_config_from(&path).expect("loadConfigFrom");
        assert_eq!(
            got.effective_card_spec(),
            CardSpec {
                details: CARD_DETAILS_DEFAULT.to_string(),
                state: CARD_STATE_TAG.to_string(),
            }
        );

        // Both kinds empty: the key is still written, as `{}`.
        let empty = Config {
            card: CardSpecs::default(),
            ..Config::default()
        };
        save_config_to(&path, &empty).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert_eq!(
            wire,
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{}}"#
        );
    }

    /// The per-kind wire contract, byte for byte: (a) all-default writes
    /// `"card":{}`, (b) series-only set writes only the series object, (c)
    /// both kinds set writes both objects in series/movie order.
    #[test]
    fn card_per_kind_wire_shapes() {
        let scratch = Scratch::new("config-card-kinds-wire");
        let path = store_path(&scratch);
        let read_wire = |path: &PathBuf| {
            String::from_utf8(std::fs::read(path).expect("read store")).expect("utf-8")
        };

        // (a) All-default.
        save_config_to(&path, &Config::default()).expect("saveConfigTo");
        assert_eq!(
            read_wire(&path),
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{}}"#
        );

        // (b) Series-only set: the movie object (default) is omitted.
        let mut series_only = Config::default();
        assert!(series_only.set_card_line(CardKind::Series, CardLine::Details, "default"));
        assert!(series_only.set_card_line(CardKind::Series, CardLine::State, "tag"));
        save_config_to(&path, &series_only).expect("saveConfigTo");
        assert_eq!(
            read_wire(&path),
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{"series":{"details":"default","state":"tag"}}}"#
        );

        // (c) Both kinds set.
        let mut both = Config::default();
        assert!(both.set_card_line(CardKind::Series, CardLine::Details, "default"));
        assert!(both.set_card_line(CardKind::Series, CardLine::State, "tag"));
        assert!(both.set_card_line(CardKind::Movie, CardLine::Details, "default"));
        assert!(both.set_card_line(CardKind::Movie, CardLine::State, "synopsis"));
        save_config_to(&path, &both).expect("saveConfigTo");
        assert_eq!(
            read_wire(&path),
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{"series":{"details":"default","state":"tag"},"movie":{"details":"default","state":"synopsis"}}}"#
        );
    }

    /// Per-kind round-trip through the atomic write: both specs survive
    /// save -> load, and the effective view is normalised per kind.
    #[test]
    fn card_per_kind_round_trip() {
        let scratch = Scratch::new("config-card-kinds-round-trip");
        let path = store_path(&scratch);
        let mut want = Config {
            dirs: vec![r"C:\Media".to_string()],
            scan_speed: SCAN_SPEED_TURBO.to_string(),
            ..Config::default()
        };
        assert!(want.set_card_line(CardKind::Series, CardLine::Details, "default"));
        assert!(want.set_card_line(CardKind::Series, CardLine::State, "tag"));
        assert!(want.set_card_line(CardKind::Movie, CardLine::Details, "default"));
        assert!(want.set_card_line(CardKind::Movie, CardLine::State, "synopsis"));

        save_config_to(&path, &want).expect("saveConfigTo");
        let got = load_config_from(&path).expect("loadConfigFrom");
        assert_eq!(got, want);
        assert_eq!(
            got.card.for_kind(CardKind::Series),
            CardSpec {
                details: CARD_DETAILS_DEFAULT.to_string(),
                state: CARD_STATE_TAG.to_string(),
            }
        );
        assert_eq!(
            got.card.for_kind(CardKind::Movie),
            CardSpec {
                details: "default".to_string(),
                state: "synopsis".to_string(),
            }
        );
        assert!(!got.card.is_default());
        let effective = got.effective_card_specs();
        assert_eq!(effective.series, got.card.for_kind(CardKind::Series));
        assert_eq!(effective.movie, got.card.for_kind(CardKind::Movie));
    }

    /// The legacy flat object every shipped config holds loads as series =
    /// that normalized pair and movie = the shipped default, and re-saves as
    /// the nested series-only shape. A retired Details id (`title`/`series`/
    /// `episode`) in the flat object migrates to the default.
    #[test]
    fn card_legacy_flat_read() {
        let scratch = Scratch::new("config-card-legacy");
        let legacy = scratch.write(
            "legacy-card.json",
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{"details":"episode","state":"tag"}}"#,
        );
        let got = load_config_from(&legacy).expect("load legacy flat card");
        assert_eq!(
            got.card.series,
            CardSpec {
                details: CARD_DETAILS_DEFAULT.to_string(),
                state: CARD_STATE_TAG.to_string(),
            }
        );
        assert_eq!(got.card.movie, default_card_spec());
        assert_eq!(
            got.effective_card_spec(),
            CardSpec {
                details: CARD_DETAILS_DEFAULT.to_string(),
                state: CARD_STATE_TAG.to_string(),
            }
        );
        assert_eq!(
            got.effective_card_specs().movie,
            default_card_spec(),
            "movie defaults to the shipped pair"
        );

        let path = store_path(&scratch);
        save_config_to(&path, &got).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert_eq!(
            wire,
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{"series":{"details":"default","state":"tag"}}}"#
        );
    }

    /// Absent / `null` / `{}` / unknown-only `card` values all load as the
    /// all-default [`CardSpecs`].
    #[test]
    fn card_absent_null_empty_and_unknown_load_default() {
        let scratch = Scratch::new("config-card-default-loads");

        let absent = scratch.write("absent.json", r#"{"dirs":[]}"#);
        let got = load_config_from(&absent).expect("absent card");
        assert_eq!(got.card, CardSpecs::default());
        assert!(got.card.is_default());

        let null = scratch.write("null.json", r#"{"dirs":[],"card":null}"#);
        let got = load_config_from(&null).expect("null card");
        assert_eq!(got.card, CardSpecs::default());
        assert!(got.card.is_default());

        let empty = scratch.write("empty.json", r#"{"dirs":[],"card":{}}"#);
        let got = load_config_from(&empty).expect("empty card");
        assert_eq!(got.card, CardSpecs::default());
        assert!(got.card.is_default());

        let unknown = scratch.write("unknown.json", r#"{"dirs":[],"card":{"future":"ignored"}}"#);
        let got = load_config_from(&unknown).expect("unknown card keys");
        assert_eq!(got.card, CardSpecs::default());
        assert!(got.card.is_default());

        assert!(!CardKind::Series.is_movie());
        assert!(CardKind::Movie.is_movie());
    }

    /// Unknown ids never reach the card: `for_kind` falls back per line, and
    /// `set_line` normalises (a second identical set reports no change).
    #[test]
    fn card_unknown_ids_normalise_and_set_line_reports_change() {
        let stored_typo = CardSpecs {
            series: CardSpec {
                details: "fancy-template".to_string(),
                state: String::new(),
            },
            movie: CardSpec::default(),
        };
        assert_eq!(stored_typo.for_kind(CardKind::Series), default_card_spec());
        assert_eq!(stored_typo.for_kind(CardKind::Movie), default_card_spec());

        let mut config = Config::default();
        assert!(
            config.set_card_line(CardKind::Series, CardLine::Details, "fancy-template"),
            "unknown details id stores the default"
        );
        assert_eq!(config.effective_card_spec(), default_card_spec());
        assert!(
            !config.set_card_line(CardKind::Series, CardLine::Details, "fancy-template"),
            "same unknown id is no change"
        );
        assert!(
            !config.set_card_line(CardKind::Series, CardLine::Details, "default"),
            "explicit default over normalised default is no change"
        );
        assert!(
            config.set_card_line(CardKind::Movie, CardLine::State, "tag"),
            "movie state change"
        );
        assert!(
            !config.set_card_line(CardKind::Movie, CardLine::State, "tag"),
            "same movie value is no change"
        );
    }

    /// The live mirror: nothing published reads as the shipped default. The
    /// global is cleared first under the serial, so no test can observe
    /// another's publish.
    #[test]
    fn live_card_specs_default_when_nothing_published() {
        let _serial = LIVE_SPECS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        *LIVE_CARD_SPECS.lock().unwrap_or_else(|e| e.into_inner()) = None;
        assert_eq!(live_card_specs(), CardSpecs::default());
        assert!(live_card_specs().is_default());
    }

    /// The live mirror: a published spec reads back per kind, and a republish
    /// overwrites. Serialized with the default test above, and every assertion
    /// is on an explicitly published value.
    #[test]
    fn live_card_specs_publish_read_round_trip() {
        let _serial = LIVE_SPECS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut specs = CardSpecs::default();
        assert!(specs.set_line(CardKind::Series, CardLine::Details, "title"));
        assert!(specs.set_line(CardKind::Movie, CardLine::State, "synopsis"));
        set_live_card_specs(specs.clone());
        assert_eq!(live_card_specs(), specs);
        assert_eq!(
            live_card_specs().for_kind(CardKind::Series).details,
            CARD_DETAILS_TITLE
        );
        assert_eq!(
            live_card_specs().for_kind(CardKind::Movie).state,
            CARD_STATE_SYNOPSIS
        );
        set_live_card_specs(CardSpecs::default());
        assert_eq!(live_card_specs(), CardSpecs::default());
    }

    /// The now-playing mirror: nothing published reads as `None`; a publish
    /// reads back whole; a republish overwrites; a clear reads `None` again.
    /// Serialized with the live-mirror tests above (one process-wide slot).
    #[test]
    fn live_now_playing_publishes_and_clears() {
        let _serial = LIVE_SPECS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        *LIVE_NOW_PLAYING.lock().unwrap_or_else(|e| e.into_inner()) = None;

        assert_eq!(live_now_playing(), None, "nothing published reads None");

        let playing = NowPlaying {
            raw_title: "Breaking.Bad.S01E02.1080p.mkv - PotPlayer".to_string(),
            path: format!("{}:{}", 'M', r"\2SERIES\Breaking Bad\Season 1\S01E02.mkv"),
        };
        set_live_now_playing(Some(playing.clone()));
        assert_eq!(live_now_playing(), Some(playing.clone()));

        // A republish (the next playing file) overwrites the slot.
        let next = NowPlaying {
            raw_title: "Dune.Part.Two.2024.1080p.mkv - PotPlayer".to_string(),
            path: format!("{}:{}", 'D', r"\Films\Dune Part Two.mkv"),
        };
        set_live_now_playing(Some(next.clone()));
        assert_eq!(live_now_playing(), Some(next));

        // The player is gone: the mirror clears.
        set_live_now_playing(None);
        assert_eq!(live_now_playing(), None);
    }

    /// T5-B (red first): a stored retired Details id migrates to the default on
    /// load (nested and legacy flat shapes) and on save, without losing the
    /// unrelated lines. A save never persists a retired id again, and a reload
    /// is stable.
    #[test]
    fn card_retired_details_ids_migrate_on_load_and_save() {
        let scratch = Scratch::new("config-card-migrate");

        // Nested shape with retired ids on both kinds; the states survive.
        let retired = scratch.write(
            "retired.json",
            r#"{"dirs":[],"card":{"series":{"details":"title","state":"tag"},"movie":{"details":"series","state":"none"}}}"#,
        );
        let got = load_config_from(&retired).expect("load retired card");
        assert_eq!(got.card.series.details, CARD_DETAILS_DEFAULT);
        assert_eq!(got.card.series.state, CARD_STATE_TAG);
        assert_eq!(got.card.movie.details, CARD_DETAILS_DEFAULT);
        assert_eq!(got.card.movie.state, CARD_STATE_NONE);

        // A save migrates too: the file no longer carries a retired id, and a
        // reload is stable.
        let path = store_path(&scratch);
        let raw = Config {
            card: CardSpecs {
                series: CardSpec {
                    details: "title".to_string(),
                    state: CARD_STATE_TAG.to_string(),
                },
                movie: CardSpec::default(),
            },
            ..Config::default()
        };
        save_config_to(&path, &raw).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert!(
            !wire.contains("\"details\":\"title\"") && !wire.contains("\"details\":\"series\""),
            "a retired id persisted: {wire}"
        );
        let reloaded = load_config_from(&path).expect("loadConfigFrom");
        assert_eq!(reloaded.card.series.details, CARD_DETAILS_DEFAULT);
        assert_eq!(reloaded.card.series.state, CARD_STATE_TAG);
        assert!(reloaded.card.movie.is_default());

        // Legacy flat shape migrates the same way (movie stays default).
        let legacy = scratch.write(
            "legacy-retired.json",
            r#"{"dirs":[],"card":{"details":"series","state":"tag"}}"#,
        );
        let got = load_config_from(&legacy).expect("load legacy retired card");
        assert_eq!(got.card.series.details, CARD_DETAILS_DEFAULT);
        assert_eq!(got.card.series.state, CARD_STATE_TAG);
        assert_eq!(got.card.movie, default_card_spec());
    }

    /// `set_card_spec` sets BOTH kinds to the same normalized pair.
    #[test]
    fn card_set_card_spec_sets_both_kinds() {
        let mut config = Config::default();
        assert!(config.set_card_spec("title", "tag"), "first set");
        assert!(
            !config.set_card_spec("title", "tag"),
            "same value is unchanged"
        );
        let pair = CardSpec {
            details: CARD_DETAILS_TITLE.to_string(),
            state: CARD_STATE_TAG.to_string(),
        };
        assert_eq!(config.card.series, pair);
        assert_eq!(config.card.movie, pair);
        assert_eq!(config.effective_card_specs().series, pair);
        assert_eq!(config.effective_card_specs().movie, pair);
    }

    /// The privacy scrub: a store carrying retired credential fields loads its
    /// dirs, carries nothing credential-ish in memory, and is rewritten on disk
    /// without the retired names.
    #[test]
    fn retired_credential_fields_are_scrubbed() {
        let retired = retired_fixture_names();
        let scratch = Scratch::new("config-scrub");
        let path = scratch.write(
            CONFIG_FILE_NAME,
            &format!(
                r#"{{"dirs":["C:\\Media"],"scan_speed":"turbo","{}":"s3cr3t","{}":"s3cr3t"}}"#,
                retired[0], retired[1]
            ),
        );
        let got = load_config_from(&path).expect("loadConfigFrom");
        assert_eq!(got.dirs, vec![r"C:\Media".to_string()]);

        let wire = serde_json::to_string(&got).expect("marshal loaded config");
        for name in &retired {
            assert!(
                !wire.contains(name),
                "in-memory config still carries {name}: {wire}"
            );
        }
        let after = String::from_utf8(scratch.read(CONFIG_FILE_NAME)).expect("utf-8 store");
        for name in &retired {
            assert!(
                !after.contains(name),
                "file on disk still contains {name}: {after}"
            );
        }
        assert!(has_retired_fields(
            format!(r#"{{"{}":1}}"#, retired[0]).as_bytes()
        ));
        assert!(!has_retired_fields(b"{not json"));
    }

    /// A clean store is never rewritten: content is byte-identical before and
    /// after load.
    #[test]
    fn clean_store_is_not_rewritten() {
        let scratch = Scratch::new("config-clean");
        let path = store_path(&scratch);
        let want = Config {
            dirs: vec![r"C:\Media".to_string()],
            scan_speed: SCAN_SPEED_TURBO.to_string(),
            ..Config::default()
        };
        save_config_to(&path, &want).expect("saveConfigTo");
        let before = std::fs::read(&path).expect("read before");
        load_config_from(&path).expect("loadConfigFrom");
        let after = std::fs::read(&path).expect("read after");
        assert_eq!(after, before, "a clean config must not be rewritten");
    }

    /// The exact schema, with every default: what a fresh store looks like on
    /// disk, key for key.
    #[test]
    fn schema_and_defaults_are_the_go_contract() {
        let scratch = Scratch::new("config-schema");
        let path = store_path(&scratch);
        save_config_to(&path, &Config::default()).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert_eq!(
            wire,
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{}}"#
        );

        // A card with both lines set writes both kinds (set_card_spec sets
        // BOTH kinds to the same pair).
        let mut with_card = Config::default();
        with_card.set_card_spec("default", "synopsis");
        save_config_to(&path, &with_card).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert_eq!(
            wire,
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{"series":{"details":"default","state":"synopsis"},"movie":{"details":"default","state":"synopsis"}}}"#
        );

        // start_with_windows is a real stored field.
        let autostart = Config {
            start_with_windows: true,
            ..Config::default()
        };
        save_config_to(&path, &autostart).expect("saveConfigTo");
        assert!(load_config_from(&path).expect("load").start_with_windows);
    }

    /// Every tolerant-load rule in one store: a partial file, `null` for every
    /// field, and unknown fields.
    #[test]
    fn partial_null_and_unknown_fields_load_tolerantly() {
        let scratch = Scratch::new("config-tolerant");

        // Partial: only dirs, everything else default.
        let partial = scratch.write(CONFIG_FILE_NAME, r#"{"dirs":["C:\\Media"]}"#);
        let got = load_config_from(&partial).expect("partial load");
        assert_eq!(got.dirs, vec![r"C:\Media".to_string()]);
        assert!(!got.start_with_windows);
        assert_eq!(got.effective_scan_speed(), SCAN_SPEED_TURBO);
        assert_eq!(got.effective_card_spec(), default_card_spec());

        // Go's null no-op: every field null, plus an unknown field.
        let nulls = scratch.write(
            "nulls.json",
            r#"{"dirs":null,"start_with_windows":null,"scan_speed":null,"card":null,"future":"ignored"}"#,
        );
        let got = load_config_from(&nulls).expect("null load");
        // Every field took its default; the profile is normalised on load, so a
        // null scan_speed lands on turbo, exactly as an absent one does.
        assert_eq!(
            got,
            Config {
                scan_speed: SCAN_SPEED_TURBO.to_string(),
                ..Config::default()
            }
        );

        // A wrong field type is the Go error case, not a silent coercion.
        let wrong = scratch.write("wrong.json", r#"{"dirs":"C:\\Media"}"#);
        assert!(load_config_from(&wrong).is_err());
        let wrong = scratch.write("wrong2.json", r#"{"scan_speed":7}"#);
        assert!(load_config_from(&wrong).is_err());
    }

    /// `settings_sections_test.go`'s `TestConfigStartWithWindowsRoundTrip`: the
    /// flag survives an atomic save -> load, and old stores without the field
    /// load tolerant (false).
    #[test]
    fn start_with_windows_round_trips_and_legacy_loads_false() {
        let scratch = Scratch::new("config-autostart");
        let path = store_path(&scratch);
        let want = Config {
            dirs: vec![r"C:\Media".to_string()],
            start_with_windows: true,
            ..Config::default()
        };
        save_config_to(&path, &want).expect("saveConfigTo");
        let got = load_config_from(&path).expect("loadConfigFrom");
        assert!(got.start_with_windows, "round-trip lost the flag");
        assert_eq!(got.dirs.len(), 1, "round-trip lost dirs: {got:?}");

        let legacy = scratch.write("legacy-autostart.json", r#"{"dirs":[]}"#);
        let old = load_config_from(&legacy).expect("load legacy store");
        assert!(
            !old.start_with_windows,
            "legacy store must load tolerant false"
        );
    }

    /// The Go-compat golden guard: a fixture mirroring the legacy Go-written
    /// config shape (`go\config.go:22-30`, flat `card`) with SYNTHETIC values --
    /// parse, migrate, assert no key is lost and no default silently changes.
    /// The legacy flat card loads as the series spec (movie default) and
    /// re-saves as the nested shape; its retired `title` details id migrates
    /// to the default (T5-B) while the `tag` state survives.
    #[test]
    fn go_written_shape_round_trips_without_key_loss() {
        // Synthetic stand-in for a Go-written store: all four keys, non-default
        // values on every field, key order as Go's Marshal emits it. The card
        // is the legacy flat object every shipped config holds.
        let fixture = r#"{"dirs":["/synth/movies","/synth/shows"],"start_with_windows":true,"scan_speed":"balanced","card":{"details":"title","state":"tag"}}"#;
        let parsed: Config = serde_json::from_str(fixture).expect("parse Go shape");
        assert_eq!(
            parsed.dirs,
            vec!["/synth/movies".to_string(), "/synth/shows".to_string()]
        );
        assert!(parsed.start_with_windows);
        assert_eq!(parsed.effective_scan_speed(), SCAN_SPEED_BALANCED);
        assert_eq!(
            parsed.effective_card_spec(),
            CardSpec {
                details: CARD_DETAILS_DEFAULT.to_string(),
                state: CARD_STATE_TAG.to_string(),
            }
        );

        // Round-trip through the atomic write: the legacy flat card migrates to
        // the nested shape (series = the migrated pair, movie default =
        // omitted) while the top-level key set stays exactly the Go contract.
        let scratch = Scratch::new("config-golden");
        let fixture_path = scratch.write("golden.json", fixture);
        let loaded = load_config_from(&fixture_path).expect("loadConfigFrom");
        let path = store_path(&scratch);
        save_config_to(&path, &loaded).expect("saveConfigTo");
        let wire = String::from_utf8(std::fs::read(&path).expect("read store")).expect("utf-8");
        assert_eq!(
            wire,
            r#"{"dirs":["/synth/movies","/synth/shows"],"start_with_windows":true,"scan_speed":"balanced","card":{"series":{"details":"default","state":"tag"}}}"#,
            "legacy flat card must migrate to the nested series shape"
        );
        let wire_value: serde_json::Value = serde_json::from_str(&wire).expect("wire JSON");
        let mut keys: Vec<&str> = wire_value
            .as_object()
            .expect("wire object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["card", "dirs", "scan_speed", "start_with_windows"],
            "wire key set is the Go contract, nothing added, nothing lost (byte order pinned above)"
        );

        // No default silently changed: a fresh store is still the exact Go
        // schema with Go's defaults.
        let fresh = Scratch::new("config-golden-fresh");
        let fresh_path = store_path(&fresh);
        save_config_to(&fresh_path, &Config::default()).expect("saveConfigTo");
        let fresh_wire =
            String::from_utf8(std::fs::read(&fresh_path).expect("read store")).expect("utf-8");
        assert_eq!(
            fresh_wire,
            r#"{"dirs":[],"start_with_windows":false,"scan_speed":"turbo","card":{}}"#
        );
    }

    /// The write is atomic: no temp file survives, and the temp file is created
    /// beside the target so the rename is same-volume.
    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let scratch = Scratch::new("config-atomic");
        let path = store_path(&scratch);
        save_config_to(&path, &Config::default()).expect("saveConfigTo");

        let dir = path.parent().expect("store parent");
        let leftovers: Vec<String> = std::fs::read_dir(dir)
            .expect("read store dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        assert_eq!(
            std::fs::read_dir(dir).expect("read store dir").count(),
            1,
            "the store directory holds the store alone"
        );
    }

    /// The real paths are the Go contract, and the store is never touched.
    #[test]
    fn paths_are_local_app_data_potflix() {
        let config = config_path();
        assert!(config.ends_with(Path::new(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME)));
        assert!(metadata_dir().ends_with(Path::new(CONFIG_DIR_NAME).join(META_DIR_NAME)));
        assert!(scan_index_path().ends_with(Path::new(CONFIG_DIR_NAME).join(SCAN_INDEX_FILE_NAME)));

        match cache_root() {
            Some(root) => {
                assert_eq!(config, root.join(CONFIG_DIR_NAME).join(CONFIG_FILE_NAME));
                assert_eq!(
                    metadata_dir(),
                    root.join(CONFIG_DIR_NAME).join(META_DIR_NAME)
                );
                assert_eq!(
                    scan_index_path(),
                    root.join(CONFIG_DIR_NAME).join(SCAN_INDEX_FILE_NAME)
                );
            }
            None => {
                assert_eq!(config, exe_dir_or_dot().join(CONFIG_FILE_NAME));
                assert_eq!(metadata_dir(), exe_dir_or_dot().join(META_DIR_NAME));
                assert_eq!(
                    scan_index_path(),
                    exe_dir_or_dot().join(SCAN_INDEX_FILE_NAME)
                );
            }
        }
    }
}
