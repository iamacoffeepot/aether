//! The spike's driver kinds — the two mails that make an observation
//! reproducible.
//!
//! Without them a capture would land wherever the free-running loop happened to
//! be, and "the twist at 180°" would be a phase you tried to hit rather than one
//! you asked for. With them the pose is addressable: pin the phase, capture,
//! move on.

use serde::{Deserialize, Serialize};

/// `aether.spike.warp-ears.set_phase` — pin both instances at one point of the
/// program and stop auto-advance.
///
/// `phase` is clamped to `[0, 1]`. The pose is recomputed on receipt rather
/// than on the next tick, so a capture bundled behind this mail sees the phase
/// it asked for and not the one before it. Resume with `set_auto`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.spike.warp-ears.set_phase")]
pub struct SetPhase {
    /// Position in the program: `0.0` rest, `0.45` mid-twist, `0.60` the full
    /// half-turn, `0.90` the deepest fold.
    pub phase: f32,
}

/// `aether.spike.warp-ears.set_auto` — resume (or re-stop) the free-running
/// loop. Auto-advance is on at load; `set_phase` turns it off.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.spike.warp-ears.set_auto")]
pub struct SetAuto {
    /// `true` resumes advancing from the current phase.
    pub auto: bool,
}
