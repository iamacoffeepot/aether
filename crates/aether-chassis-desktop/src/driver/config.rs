//! The desktop driver's own boot knob. It sits beside the driver, not in
//! `aether-chassis` with the window and tick knobs, because the desktop
//! driver is its only consumer and no package manifest overlays it.

use std::io;

use aether_substrate::config::ConfigError;

use super::frame_time::FrameDeltaLimit;

/// Default limit on the game time one frame adds: 250 milliseconds. No frame
/// at four a second or faster is limited, and a stall hands a game with a 20
/// millisecond step at most 13 steps in one frame.
pub const DEFAULT_MAX_FRAME_DELTA_MICROS: u32 = 250_000;

/// The env key of the frame delta limit, named in the refusal of a zero.
const MAX_FRAME_DELTA_MICROS_KEY: &str = "AETHER_DESKTOP_MAX_FRAME_DELTA_MICROS";

/// Desktop driver boot knobs (ADR-0090). The
/// `#[derive(aether_substrate::Config)]` emits the env-shaped
/// `DesktopDriverConfigLayer` and the clap-shaped `DesktopDriverOverlay`;
/// `env_prefix = "AETHER_DESKTOP"` and `cli_prefix = "desktop"` give the one
/// field `AETHER_DESKTOP_MAX_FRAME_DELTA_MICROS`,
/// `--desktop-max-frame-delta-micros`, and `[desktop] max_frame_delta_micros`
/// in a chassis config file.
#[derive(Clone, Debug, aether_substrate::Config)]
#[config(env_prefix = "AETHER_DESKTOP", cli_prefix = "desktop")]
pub struct DesktopDriverConfig {
    /// Most game time in microseconds one frame may add after a stall.
    ///
    /// The desktop driver measures wall time between frames and holds each
    /// frame's share to this limit before stating it to the lifecycle
    /// capability, so game time slows across a stall and no step is skipped.
    /// Default [`DEFAULT_MAX_FRAME_DELTA_MICROS`]. Zero would stop game time
    /// and is refused at boot by [`Self::lower`]; the derive's `nonzero` hint
    /// is not used because it turns a zero into the default without a word.
    #[config(default = 250_000)]
    pub max_frame_delta_micros: u32,
}

impl Default for DesktopDriverConfig {
    fn default() -> Self {
        Self { max_frame_delta_micros: DEFAULT_MAX_FRAME_DELTA_MICROS }
    }
}

impl DesktopDriverConfig {
    /// Lower the resolved knob into the limit the driver applies.
    ///
    /// # Errors
    ///
    /// Returns a hard [`ConfigError`] naming the key and the value when the
    /// limit is zero.
    pub fn lower(self) -> Result<FrameDeltaLimit, ConfigError> {
        FrameDeltaLimit::from_micros(self.max_frame_delta_micros).ok_or_else(|| {
            ConfigError::unparseable(
                MAX_FRAME_DELTA_MICROS_KEY,
                self.max_frame_delta_micros.to_string(),
                io::Error::other("a frame delta limit of zero would stop game time; accepted: a positive integer"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use confique::Config as _;

    use super::*;

    /// A zero limit is refused by name. Accepted, it would count nothing of
    /// any frame and game time would stop with no word to the operator.
    #[test]
    fn lower_refuses_a_zero_limit_naming_its_key() {
        let error = DesktopDriverConfig { max_frame_delta_micros: 0 }.lower().expect_err("a zero limit is refused");

        let rendered = error.to_string();
        assert!(rendered.contains(MAX_FRAME_DELTA_MICROS_KEY), "the refusal names the key: {rendered}");
    }

    /// The derive's literal default, `Default`, and the named constant are
    /// three spellings of one number. No `.env()` source is loaded, so this
    /// reads the literal defaults only.
    // Tripwire: drifts when `DEFAULT_MAX_FRAME_DELTA_MICROS` or the derive
    // literal changes without the other.
    #[test]
    fn config_layer_default_matches_the_named_const() {
        let layer = DesktopDriverConfigLayer::builder().load().expect("defaults load");

        assert_eq!(layer.max_frame_delta_micros, DEFAULT_MAX_FRAME_DELTA_MICROS);
        assert_eq!(DesktopDriverConfig::default().max_frame_delta_micros, DEFAULT_MAX_FRAME_DELTA_MICROS);
    }
}
