//! The scripted timeline: one `phase ∈ [0, 1]` in, three bone angles out.
//!
//! Both instances are always at the same phase, and the phase is the only
//! animation state the actor keeps — which is what makes an observation
//! reproducible. Send `aether.spike.warp-ears.set_phase` and the two ears are
//! posed at exactly that point of the program; nothing carries over from the
//! frame before, because nothing here integrates.
//!
//! The program walks through the four things worth seeing, in order:
//!
//! | phase | segment | what it shows |
//! |-------|---------|---------------|
//! | 0.00–0.10 | rest | the two paths agreeing exactly |
//! | 0.10–0.30 | flick | a natural pose; both paths still agree |
//! | 0.30–0.60 | twist ramp | linear-blend skinning's cross-section collapse |
//! | 0.60–0.65 | untwist | a fast return, so the collapse reads as reversible |
//! | 0.65–0.90 | fold to contact | interpenetration, identical on both sides |
//! | 0.90–1.00 | return | back to rest, ready to loop |

use crate::curve::{flick, smoothstep};

/// Seconds for one pass through the program when auto-advance is on.
pub const PERIOD_SECONDS: f32 = 12.0;

const REST_END: f32 = 0.10;
const FLICK_END: f32 = 0.30;
const TWIST_END: f32 = 0.60;
const UNTWIST_END: f32 = 0.65;
const FOLD_END: f32 = 0.90;

/// Peak flick angle, in radians. Negative about the ear's pitch axis carries
/// the tip backward — the way an ear actually flicks.
const FLICK_PEAK_RADIANS: f32 = -35.0 * core::f32::consts::PI / 180.0;

/// Full twist of bone 1 relative to bone 0, in radians. Half a turn is where
/// linear matrix blending has nothing left to interpolate through: the two
/// bones' rotations are antipodal, their average collapses toward a degenerate
/// matrix, and the mid-ear cross-sections pinch shut.
const TWIST_RADIANS: f32 = core::f32::consts::PI;

/// Full fold angle, in radians. Negative about the fold axis rotates the ear
/// *toward* the contact normal's back side — down against the skull — which
/// drives the tip through the contact slab.
const FOLD_RADIANS: f32 = -60.0 * core::f32::consts::PI / 180.0;

/// The three bone angles at one phase. Bone 0 takes the fold; bone 1 takes the
/// flick and the twist, composed in its own local frame.
pub struct Program {
    pub fold_radians: f32,
    pub flick_radians: f32,
    pub twist_radians: f32,
}

impl Program {
    /// Evaluate the timeline. `phase` outside `[0, 1]` is clamped, so a caller
    /// that pokes a phase directly cannot produce a pose the loop never visits.
    #[must_use]
    pub fn at_phase(phase: f32) -> Self {
        let phase = phase.clamp(0.0, 1.0);

        let flick_radians = if (REST_END..FLICK_END).contains(&phase) {
            FLICK_PEAK_RADIANS * flick(segment(phase, REST_END, FLICK_END))
        } else {
            0.0
        };

        let twist_radians = if (FLICK_END..TWIST_END).contains(&phase) {
            TWIST_RADIANS * smoothstep(segment(phase, FLICK_END, TWIST_END))
        } else if (TWIST_END..UNTWIST_END).contains(&phase) {
            TWIST_RADIANS * (1.0 - smoothstep(segment(phase, TWIST_END, UNTWIST_END)))
        } else {
            0.0
        };

        let fold_radians = if (UNTWIST_END..FOLD_END).contains(&phase) {
            FOLD_RADIANS * smoothstep(segment(phase, UNTWIST_END, FOLD_END))
        } else if phase >= FOLD_END {
            FOLD_RADIANS * (1.0 - smoothstep(segment(phase, FOLD_END, 1.0)))
        } else {
            0.0
        };

        Self { fold_radians, flick_radians, twist_radians }
    }
}

/// Position within one segment of the timeline, as `0..1`.
fn segment(phase: f32, start: f32, end: f32) -> f32 {
    (phase - start) / (end - start)
}
