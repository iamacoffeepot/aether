//! The turntable's own configuration vocabulary.
//!
//! It drives the puppet through the puppet's existing
//! [`Look`](crate::Look) kind, so there is no drive kind here — only
//! [`TurntableConfig`], the init-config shape read once at instantiation
//! (ADR-0090). A bare load with no `config_path` boots the compiled
//! [`Default`], which is the framing the puppet itself starts at, turning.

use serde::{Deserialize, Serialize};

/// Init-config for [`Turntable`](crate::Turntable): how fast to turn and the
/// rest of the pose it holds fixed while the azimuth sweeps.
///
/// # Agent
/// Encode one of these to the turntable's `Config` shape and pass it as the
/// `config` bytes of the `aether.component.load` that instantiates it (or
/// `load_component`'s `config_path`). Omitting config bytes boots
/// [`TurntableConfig::default()`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.puppet-turntable.config")]
pub struct TurntableConfig {
    /// Whether the motor is engaged. `false` parks the turntable — it stays
    /// loaded, holds the pose it was configured with, and sends nothing at
    /// all, so a parked turntable costs the frame nothing beyond an empty
    /// handler call.
    pub running: bool,
    /// Sweep rate, degrees per second, converted against [`Self::tick_hz`].
    /// Negative turns the other way.
    pub degrees_per_second: f32,
    /// Tick cadence the rate is converted against. `aether.lifecycle.tick`
    /// carries no elapsed time, so a rate stated in seconds needs the cadence
    /// stated alongside it; on the headless chassis this is `AETHER_TICK_HZ`.
    ///
    /// The desktop chassis ticks per frame rather than on a timer, so there
    /// the realized sweep rate is the configured one only while the frame
    /// rate matches this — and the gap between the two is a reading of the
    /// frame rate, not an error to correct.
    pub tick_hz: f32,
    /// Azimuth the sweep starts from, degrees. The turntable owns this one
    /// field of the pose and advances it; the three below it holds.
    pub azimuth: f32,
    /// Degrees above the horizon, held for the whole sweep.
    pub elevation: f32,
    /// Distance from the framing target, in model units, held for the whole
    /// sweep.
    pub distance: f32,
    /// Height of the point the camera aims at, held for the whole sweep.
    pub height: f32,
    /// Emit one log line every `n` ticks, carrying the tick counter and the
    /// azimuth. `0` disables it, which is the default.
    ///
    /// This is the actor's second job. Per-actor log entries are stamped with
    /// a host wall-clock millisecond as they land in the ring (ADR-0081), and
    /// on a frame-driven chassis one tick is one frame — so the differences
    /// between consecutive entries are the frame periods of a window nothing
    /// is holding back, read out through `actor_logs`. `1` samples every
    /// frame at millisecond resolution; a larger `n` trades per-frame detail
    /// for resolution on the mean.
    pub log_every: u32,
}

impl Default for TurntableConfig {
    fn default() -> Self {
        Self {
            running: true,
            // A twelve-second revolution: slow enough to read the drawing as
            // it turns, quick enough that a recording of one full turn is
            // shorter than a shot anybody would sit through twice.
            degrees_per_second: 30.0,
            tick_hz: 60.0,
            // The puppet's own boot framing, so a bare load changes the
            // subject's motion and nothing about how it is composed.
            azimuth: 0.0,
            elevation: 3.0,
            distance: 5.4,
            height: 0.0,
            log_every: 0,
        }
    }
}
