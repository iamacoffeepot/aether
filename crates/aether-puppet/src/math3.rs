//! Small deterministic helpers the styling and tone passes need.
//!
//! Everything here is a pure function of its argument. No state, no RNG,
//! no clock — a stroke drawn twice from the same geometry must come out
//! bit-identical, or an orbit boils.

use aether_math::Vec3;

/// splitmix64.
pub fn hash64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// `hash64` mapped to `[0, 1)`.
pub fn hash_unit(seed: u64) -> f32 {
    (hash64(seed) >> 40) as f32 / (1u64 << 24) as f32
}

/// Smooth banded noise in world space, roughly `[-1, 1]`.
///
/// Used to dither the hatch thresholds. Comparing tone against a constant
/// puts the edge of a hatch field exactly on a level curve of the
/// lighting, which reads as a ruled boundary slicing across the figure.
/// Perturbing the threshold lets the field break into dashes as it fades,
/// which is what a hand does. Sampled in world space so the break-up
/// stays fixed to the surface as the camera moves.
pub fn noise(at: Vec3) -> f32 {
    let a = at.dot(Vec3::new(37.3, 24.1, 15.9));
    let b = at.dot(Vec3::new(-13.7, 48.2, 32.6));
    let c = at.dot(Vec3::new(25.1, -12.9, -47.4));

    a.sin() * 0.5 + b.sin() * 0.3 + c.sin() * 0.2
}
