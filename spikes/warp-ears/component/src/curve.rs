//! The two shaping functions the spike reuses: a clamped smoothstep and the
//! damped ring that gives the ear flick its overshoot.
//!
//! Both are pure `f32 → f32` and belong to neither the rig nor the timeline —
//! the weight band and the pose ramps read the same smoothstep, which is why it
//! is here rather than duplicated on both sides.

/// The canonical `3t² − 2t³` smoothstep, clamped at both ends.
#[must_use]
pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * 2.0f32.mul_add(-t, 3.0)
}

/// Fraction of the flick's rise spent getting there. Short, because an ear
/// flick is a snap followed by a ring, not a symmetric swing.
const RISE: f32 = 0.18;

/// Decay of the ring, in e-folds over the post-rise remainder.
const DECAY: f32 = 3.0;

/// Ring frequency over the post-rise remainder, in radians. Two and a half
/// half-cycles, so the curve lands on exactly zero at `t = 1` — the flick
/// therefore hands the ear back at rest, and the segment that follows starts
/// from a clean pose instead of from a residue.
const RING: f32 = 2.5 * core::f32::consts::PI;

/// A unit flick over `t ∈ [0, 1]`: a fast smoothstepped rise to `1`, then a
/// decaying oscillation through zero and back that settles at `0`.
///
/// Scaling this by a peak angle is what makes the flick read as *motion*
/// rather than as a pose change — the back-swing past rest is the part the eye
/// recognizes.
#[must_use]
pub fn flick(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < RISE {
        return smoothstep(t / RISE);
    }
    let tau = (t - RISE) / (1.0 - RISE);
    (-DECAY * tau).exp() * (RING * tau).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tripwire: the flick must hand the ear back at exactly rest, because the
    /// twist segment that follows composes onto whatever bone 1 is left
    /// holding. A curve that ended on a residue would put a permanent tilt into
    /// every later pose — and, worse, put a *different* tilt into the two
    /// instances' shared bone chain depending on where the ramp was sampled.
    #[test]
    fn the_flick_starts_and_settles_at_rest_and_rings_past_it() {
        assert!(flick(0.0).abs() < 1e-6);
        assert!(flick(1.0).abs() < 1e-6);
        assert!((flick(RISE) - 1.0).abs() < 1e-6, "the rise should reach full amplitude");

        let mut back_swing = f32::INFINITY;
        for step in 0..=100u8 {
            back_swing = back_swing.min(flick(f32::from(step) / 100.0));
        }
        assert!(back_swing < -0.1, "the flick should ring past rest, not just decay to it");
    }
}
