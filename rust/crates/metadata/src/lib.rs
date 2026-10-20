//! `metadata` -- mirrors the Go package `go\internal\metadata`.
//!
//! Canonical series/episode titles from free, keyless sources (TVmaze for TV,
//! AniList, Tenrai and Kitsu plus Cinemeta, Episodate, Bangumi, LiveChart.me
//! and ANN for anime, Cinemeta then Wikipedia then JustWatch
//! for movies) behind a
//! permanent on-disk cache. The poll loop uses the non-blocking
//! [`Resolver::resolve`]; the CLIs use the blocking [`Resolver::lookup_now`];
//! [`Options::offline`] serves cache-only.
//!
//! Workspace seam: this crate carries the provider chain, the eleven providers and
//! the cache; the shared record types, the filename parser, candidate matching,
//! alternate titles and the path context live in `metadata-core`, and the
//! transport in `metadata-http`.
//!
//! # Cancellation
//!
//! Go threads a `context.Context` through every provider `Find` and both
//! blocking points (`waitForHost`, the `Retry-After` sleep). `metadata-core`
//! deliberately left Go's `Source` interface and `Options` unported for exactly
//! that reason. Here `context.Context` is [`metadata_http::cancel::Cancel`], and
//! [`resolver::Source`] takes it: `ctx.Err()` is [`Cancel::err`].
//!
//! # Three-state outcomes
//!
//! Every lookup ends FOUND, DEFINITIVE-MISS, or TRANSIENT
//! ([`Error::Unavailable`], Go's `ErrLookupUnavailable`). Only a definitive miss
//! is cached negative; a transient failure -- including cancellation --
//! persists nothing. Callers must never bin the sentinel as not-found.

pub mod anilist;
pub mod ann;
pub mod artwork;
pub mod bangumi;
pub mod brick;
pub mod cache;
pub mod cinemeta;
pub mod episodate;
pub mod kitsu;
pub mod justwatch;
pub mod livechart;
pub mod overrides;
pub mod resolver;
pub mod search;
mod serdeutil;
pub mod tenrai;
pub mod timefmt;
pub mod tvmaze;
pub mod urlutil;
pub mod wikipedia;
mod yearpref;

#[cfg(test)]
mod anilist_test;
#[cfg(test)]
mod ann_test;
#[cfg(test)]
mod artwork_backfill_test;
#[cfg(test)]
mod artwork_test;
#[cfg(test)]
mod bangumi_test;
#[cfg(test)]
mod brick_fill_test;
#[cfg(test)]
mod brick_test;
#[cfg(test)]
mod cache_test;
#[cfg(test)]
mod chaincount_test;
#[cfg(test)]
mod cinemeta_test;
#[cfg(test)]
mod connreuse_test;
#[cfg(test)]
mod episodate_test;
#[cfg(test)]
mod force_test;
#[cfg(test)]
mod justwatch_test;
#[cfg(test)]
mod kitsu_test;
#[cfg(test)]
mod livechart_test;
#[cfg(test)]
mod resolver_test;
#[cfg(test)]
mod testsupport;
#[cfg(test)]
mod tenrai_test;
#[cfg(test)]
mod tvmaze_test;
#[cfg(test)]
mod wikipedia_test;

use std::fmt;

use metadata_http::cancel::CancelError;

pub use metadata_core::types::{Button, Episode, Kind, Media, Query, Resolution};
pub use brick::{
    BRICK_FILE_NAME, BRICK_SCHEMA, Brick, BrickCache, BrickField, BrickLookup, BrickSource,
    BrickStatus, BrickValue, EntityKey, EpisodeKey, FillReport, MISS_RETRY_AFTER, capacity,
    entity_key_for_query, episode_key_for_query, extract_field_bricks, fill_entity, priority,
    with_media_id,
};
pub use overrides::{Override, Scope, Store, load};
pub use resolver::{Options, Resolver, Source, default_sources};
pub use search::{Candidate, pin_from_candidate, refresh, search};

/// The text of Go's `ErrLookupUnavailable` sentinel.
pub const LOOKUP_UNAVAILABLE_MESSAGE: &str = "metadata lookup unavailable";

/// The crate's error type: Go's `error` return, with the one distinction this
/// package's callers must make preserved as a variant rather than a sentinel
/// value they could compare against the wrong thing.
#[derive(Debug, Clone)]
pub enum Error {
    /// A transient provider failure (timeout, 429, DNS, outage): the lookup
    /// could not be completed, so the outcome is unknown -- NOT a confirmed
    /// miss. Go's `ErrLookupUnavailable`, wrapped around the cause.
    Unavailable(String),
    /// The scope was cancelled or its deadline passed (`ctx.Err()`).
    Cancelled(CancelError),
    /// Any other failure: transport, decode, store or filesystem.
    Other(String),
}

impl Error {
    /// A transient failure carrying `message` (already sentinel-qualified).
    pub fn unavailable(message: impl Into<String>) -> Error {
        Error::Unavailable(message.into())
    }

    /// Any non-transient failure.
    pub fn other(message: impl Into<String>) -> Error {
        Error::Other(message.into())
    }

    /// `errors.Is(err, ErrLookupUnavailable)`.
    pub fn is_lookup_unavailable(&self) -> bool {
        matches!(self, Error::Unavailable(_))
    }

    /// The message without the variant wrapper.
    pub fn message(&self) -> &str {
        match self {
            Error::Unavailable(m) | Error::Other(m) => m,
            Error::Cancelled(reason) => reason_text(*reason),
        }
    }
}

fn reason_text(reason: CancelError) -> &'static str {
    match reason {
        CancelError::Cancelled => "context canceled",
        CancelError::DeadlineExceeded => "context deadline exceeded",
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Error {}

impl From<metadata_http::Error> for Error {
    fn from(err: metadata_http::Error) -> Error {
        match err {
            metadata_http::Error::Cancelled(reason) => Error::Cancelled(reason),
            other => Error::Other(other.to_string()),
        }
    }
}

impl From<store::Error> for Error {
    fn from(err: store::Error) -> Error {
        Error::Other(err.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Error {
        Error::Other(format!("metadata: decode: {err}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::Other(format!("metadata: io: {err}"))
    }
}

impl From<CancelError> for Error {
    fn from(reason: CancelError) -> Error {
        Error::Cancelled(reason)
    }
}
