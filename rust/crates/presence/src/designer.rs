//! `presence::designer` -- the RPC designer's field toggles.
//!
//! One [`DesignerSpec`] decides which composed card fields reach the Discord
//! payload and the local previews. It is a field filter, not a second
//! composer: the card identity, the per-kind lines and the 128-unit budgets
//! are still decided by [`crate::compose_card`], so a toggle can remove a
//! field but can never invent one, rename the header to the application name,
//! or add a corner icon.
//!
//! The shipped default is every field on: applying it to a composed card is
//! byte-identical to the card itself, which is what keeps the preview and the
//! wire in lockstep. The sender reads the process-wide mirror
//! ([`live_designer`]) on every poll, so a designer click takes effect at the
//! next poll tick with no restart and no second push path.

use std::sync::{LazyLock, Mutex};

use crate::Activity;

/// Which composed fields the designer sends. Every field defaults to `true`
/// (the shipped card); turning one off omits it exactly as an empty field is
/// omitted on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesignerSpec {
    /// Send `assets.large_image` (the resolved artwork). Off also drops the
    /// cover tooltip: Discord cannot attach `large_text` without an image.
    pub artwork: bool,
    /// Send `details` (the first body line).
    pub details: bool,
    /// Send `state` (the second body line).
    pub state: bool,
    /// Send `assets.large_text` (the synopsis snippet). Only meaningful with
    /// `artwork`: the tooltip rides the cover.
    pub synopsis: bool,
    /// Send `timestamps` (the elapsed anchor and, while playing, the end).
    pub timers: bool,
}

impl Default for DesignerSpec {
    fn default() -> Self {
        Self {
            artwork: true,
            details: true,
            state: true,
            synopsis: true,
            timers: true,
        }
    }
}

impl DesignerSpec {
    /// Whether every field is sent (the shipped card).
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Apply the field choices to one composed card.
    ///
    /// The header (`name`) is never filtered: the selected title always drives
    /// `Watching <Title>`. The small slot, the buttons and the paused flag are
    /// never populated by the composer; they are cleared here as well, so no
    /// caller can smuggle a corner icon or a link through the designer.
    /// Timestamps are zeroed together with `has_start`, so an omitted time bar
    /// can never leave a dangling anchor.
    #[must_use]
    pub fn apply(&self, mut act: Activity) -> Activity {
        if !self.details {
            act.details.clear();
        }
        if !self.state {
            act.state.clear();
        }
        if !self.artwork {
            act.large_image.clear();
            act.large_text.clear();
        } else if !self.synopsis {
            act.large_text.clear();
        }
        if !self.timers {
            act.has_start = false;
            act.start = 0;
            act.end = 0;
        }
        act.small_image.clear();
        act.small_text.clear();
        act.buttons.clear();
        act
    }
}

/// The process-wide live designer choices. `None` means nothing has been
/// published yet; [`live_designer`] then answers the shipped default.
static LIVE_DESIGNER: LazyLock<Mutex<Option<DesignerSpec>>> = LazyLock::new(|| Mutex::new(None));

/// Publish the designer choices to the running process. The sender applies
/// them to the next composed card, so the wire follows the preview with no
/// restart.
pub fn set_live_designer(spec: DesignerSpec) {
    *LIVE_DESIGNER.lock().unwrap_or_else(|e| e.into_inner()) = Some(spec);
}

/// The published choices, or the shipped default when nothing was published.
#[must_use]
pub fn live_designer() -> DesignerSpec {
    LIVE_DESIGNER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{DesignerSpec, live_designer, set_live_designer};
    use crate::{Action, Resolved, compose_card};
    use player::window::APP_NAME;

    fn movie_card() -> crate::Activity {
        let res = Resolved {
            series: "Dune: Part Two".to_string(),
            year: 2024,
            is_movie: true,
            genres: "Sci-Fi, Adventure".to_string(),
            length: "2h 46m".to_string(),
            synopsis: "Paul Atreides unites with the Fremen.".to_string(),
            synopsis_is_work: true,
            artwork: "https://example.com/dune.jpg".to_string(),
            ..Resolved::default()
        };
        let act = Action {
            running: true,
            has_start: true,
            start: 1_700_000_000_000,
            ct: 1000,
            ..Action::default()
        };
        compose_card(
            "Dune.Part.Two.2024.1080p.mkv - PotPlayer",
            Some(&act),
            &res,
            true,
            &crate::cardspec::default_card_spec(),
        )
    }

    fn series_card() -> crate::Activity {
        let res = Resolved {
            series: "Breaking Bad".to_string(),
            episode_name: "Problem Dog".to_string(),
            season: 4,
            number: 7,
            synopsis: "Walt faces the consequences.".to_string(),
            artwork: "https://example.com/bb.jpg".to_string(),
            ..Resolved::default()
        };
        let act = Action {
            running: true,
            has_start: true,
            start: 1_700_000_000_000,
            ct: 1000,
            ..Action::default()
        };
        compose_card(
            "Breaking.Bad.S04E07.Problem.Dog.1080p.mkv - PotPlayer",
            Some(&act),
            &res,
            false,
            &crate::cardspec::default_card_spec(),
        )
    }

    #[test]
    fn default_designer_is_identity_on_the_composed_card() {
        let spec = DesignerSpec::default();
        assert!(spec.is_default());
        for card in [movie_card(), series_card()] {
            assert_eq!(spec.apply(card.clone()), card, "default must be identity");
        }
        assert_eq!(live_designer(), DesignerSpec::default());
    }

    #[test]
    fn field_toggles_remove_only_their_own_fields() {
        let card = movie_card();
        let off = DesignerSpec {
            details: false,
            ..DesignerSpec::default()
        }
        .apply(card.clone());
        assert!(off.details.is_empty());
        assert_eq!(off.state, card.state);
        assert_eq!(off.name, card.name);
        assert_eq!(off.large_image, card.large_image);

        let off = DesignerSpec {
            state: false,
            ..DesignerSpec::default()
        }
        .apply(card.clone());
        assert!(off.state.is_empty());
        assert_eq!(off.details, card.details);

        let off = DesignerSpec {
            artwork: false,
            ..DesignerSpec::default()
        }
        .apply(card.clone());
        assert!(off.large_image.is_empty());
        assert!(
            off.large_text.is_empty(),
            "no cover, no cover tooltip: {:?}",
            off.large_text
        );
        assert_eq!(off.details, card.details);
        assert_eq!(off.state, card.state);

        let off = DesignerSpec {
            synopsis: false,
            ..DesignerSpec::default()
        }
        .apply(card.clone());
        assert_eq!(off.large_image, card.large_image, "image stays");
        assert!(off.large_text.is_empty(), "tooltip off");

        let off = DesignerSpec {
            timers: false,
            ..DesignerSpec::default()
        }
        .apply(card);
        assert!(!off.has_start);
        assert_eq!(off.start, 0);
        assert_eq!(off.end, 0);
    }

    #[test]
    fn header_is_always_the_media_title_never_the_app_name() {
        for card in [movie_card(), series_card()] {
            assert_ne!(card.name, APP_NAME);
            for spec in [
                DesignerSpec::default(),
                DesignerSpec {
                    artwork: false,
                    details: false,
                    state: false,
                    synopsis: false,
                    timers: false,
                },
            ] {
                let got = spec.apply(card.clone());
                assert_eq!(got.name, card.name, "the header is never filtered");
                assert_ne!(got.name, APP_NAME, "the header is never the app name");
                assert!(got.small_image.is_empty(), "no corner icon");
                assert!(got.small_text.is_empty(), "no corner icon text");
                assert!(got.buttons.is_empty(), "no click-through links");
            }
        }
    }

    #[test]
    fn movie_episode_identity_and_episode_state_survive_the_designer() {
        let movie = movie_card();
        assert_eq!(movie.name, "Dune: Part Two");
        assert_eq!(movie.details, "2024 \u{b7} Sci-Fi, Adventure");

        let episode = series_card();
        assert_eq!(episode.name, "Breaking Bad", "episode header is the series");
        assert!(
            episode.state.contains("S04E07") && episode.state.contains("Problem Dog"),
            "episode state must keep the episode facts: {:?}",
            episode.state
        );
        let got = DesignerSpec {
            details: false,
            ..DesignerSpec::default()
        }
        .apply(episode.clone());
        assert_eq!(got.name, "Breaking Bad");
        assert_eq!(got.state, episode.state);
    }

    #[test]
    fn live_mirror_round_trips_and_can_be_reset() {
        let spec = DesignerSpec {
            artwork: false,
            details: true,
            state: false,
            synopsis: false,
            timers: true,
        };
        set_live_designer(spec);
        assert_eq!(live_designer(), spec);
        set_live_designer(DesignerSpec::default());
        assert_eq!(live_designer(), DesignerSpec::default());
    }
}
