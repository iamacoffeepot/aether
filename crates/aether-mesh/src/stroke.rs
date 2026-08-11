//! Renderer-neutral eye-facing stroke ribbon geometry.
//!
//! A stroke is an ordered series of weighted positions. The solve turns it
//! into rails that face the eye, hold a constant angular width through depth,
//! taper like pen pressure, and carry stable world-space wobble. Consumers
//! choose colour and pack the returned positions for their renderer.

#![allow(clippy::cast_precision_loss, clippy::suboptimal_flops)]

use aether_math::{TAU, Vec3};

/// What [`reference_depth`] answers for a stroke that is not drawn.
pub const NOT_DRAWN: f32 = -1.0;

/// The pressure ramp in radians of arc.
pub const RAMP: f32 = 0.0064;

const DEPTH_WEIGHT_FLOOR: f32 = 0.82;
const DEPTH_WEIGHT_CEILING: f32 = 1.22;

/// One renderer-neutral point along a stroke.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrokePoint {
    /// World-space position of the stroke centreline.
    pub pos: Vec3,
    /// Width multiplier at this point.
    pub weight: f32,
}

/// Resolved numeric policy for one stroke.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StrokeParameters {
    /// Half-width at unit depth, in radians, before per-point weight.
    pub angular_half_width: f32,
    /// Maximum world-space wobble at unit depth, in radians.
    pub angular_wobble: f32,
    /// Late wobble multiplier retained separately to preserve float operation order.
    pub wobble_scale: f32,
    /// Shortest projected arc that should be drawn, in radians.
    pub minimum_angular_length: f32,
}

/// One point's rail pair, solved but not yet widened.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rail {
    /// Wobble-displaced centre of the stroke.
    pub centre: Vec3,
    /// Offset from the centre to the right rail at full pressure.
    pub offset: Vec3,
    /// Pressure multiplier applied to the offset.
    pub taper: f32,
}

/// One point's rail solve with the eye factored out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    /// Position before wobble displaces it.
    pub pos: Vec3,
    /// Chord across the point, used with the view to find the rail direction.
    pub along: Vec3,
    /// Angular half-width at full pressure, per unit of depth.
    pub half: f32,
    /// Wobble displacement, per unit of depth.
    pub drift: f32,
}

/// Stable world-space wobble for one stroke seed and position.
#[must_use]
pub fn wander(seed: u64, at: Vec3) -> f32 {
    let (p1, p2) = (hash_unit(seed) * TAU, hash_unit(seed ^ 0x5bf0_3635) * TAU);
    let (f1, f2) = (5.5 + hash_unit(seed ^ 0x1234_9abc) * 2.5, 15.0 + hash_unit(seed ^ 0xfeed_beef) * 6.0);
    let (a, b) = (Vec3::new(0.71, 0.52, 0.47), Vec3::new(-0.44, 0.63, 0.64));

    (at.dot(a) * f1 + p1).sin() * 0.72 + (at.dot(b) * f2 + p2).sin() * 0.28
}

/// Pencil pressure: light at the entry, full through the middle, tapering at the exit.
#[must_use]
pub fn pressure(travelled: f32, total: f32) -> f32 {
    let ramp = RAMP.min(total * 0.45);
    if ramp <= 1e-6 {
        return 1.0;
    }

    let ends = (travelled / ramp).min((total - travelled) / ramp).clamp(0.0, 1.0);
    0.42 + 0.58 * ends.sqrt()
}

/// Append eye-free anchors for a stroke, returning false for fewer than two points.
pub fn anchors(
    points: impl IntoIterator<Item = StrokePoint>,
    parameters: StrokeParameters,
    seed: u64,
    jitter: u64,
    out: &mut Vec<Anchor>,
) -> bool {
    let mut points = points.into_iter();
    let Some(first) = points.next() else {
        return false;
    };
    let Some(mut current) = points.next() else {
        return false;
    };

    let seed = seed ^ jitter;
    out.push(anchor(first, current.pos - first.pos, parameters, seed));

    let mut previous = first;
    for next in points {
        out.push(anchor(current, next.pos - previous.pos, parameters, seed));
        previous = current;
        current = next;
    }
    out.push(anchor(current, current.pos - previous.pos, parameters, seed));

    true
}

/// Return the stroke's average eye distance, or [`NOT_DRAWN`] if it is too short.
pub fn reference_depth(points: impl IntoIterator<Item = StrokePoint>, parameters: StrokeParameters, eye: Vec3) -> f32 {
    let mut points = points.into_iter().peekable();
    let (mut count, mut total, mut arc) = (0usize, 0.0f32, 0.0f32);
    while let Some(point) = points.next() {
        let depth = (point.pos - eye).length();
        count += 1;
        total += depth;
        if let Some(next) = points.peek() {
            arc += (next.pos - point.pos).length() / depth.max(1e-4);
        }
    }
    if count < 2 || arc < parameters.minimum_angular_length {
        return NOT_DRAWN;
    }

    total / count as f32
}

/// Solve one anchor against an eye into its centre and full-pressure offset.
#[must_use]
pub fn rail(anchor: &Anchor, reference: f32, eye: Vec3) -> (Vec3, Vec3) {
    let to_eye = eye - anchor.pos;
    let depth = to_eye.length().max(1e-4);
    let across = anchor.along.cross(to_eye);
    let across = if across.length() < 1e-9 {
        Vec3::ZERO
    } else {
        across.normalize()
    };
    let depth_weight = (reference / depth).clamp(DEPTH_WEIGHT_FLOOR, DEPTH_WEIGHT_CEILING);

    (anchor.pos + across * (anchor.drift * depth), across * (anchor.half * depth * depth_weight))
}

/// Append solved rails for a stroke, returning false when it should not be drawn.
pub fn rails<I>(points: I, parameters: StrokeParameters, eye: Vec3, seed: u64, jitter: u64, out: &mut Vec<Rail>) -> bool
where
    I: IntoIterator<Item = StrokePoint> + Clone,
{
    let reference = reference_depth(points.clone(), parameters, eye);
    let mut anchored = Vec::new();
    if reference < 0.0 || !anchors(points.clone(), parameters, seed, jitter, &mut anchored) {
        return false;
    }

    let mut points = points.into_iter();
    let Some(mut previous) = points.next() else {
        return false;
    };
    let mut angular = Vec::with_capacity(anchored.len().saturating_sub(1));
    let mut total = 0.0f32;
    for point in points {
        total += (point.pos - previous.pos).length() / (previous.pos - eye).length().max(1e-4);
        angular.push(total);
        previous = point;
    }
    let length = angular.last().copied().unwrap_or(0.0);

    out.extend(anchored.iter().enumerate().map(|(index, anchor)| {
        let (centre, offset) = rail(anchor, reference, eye);
        Rail { centre, offset, taper: pressure(angular.get(index.saturating_sub(1)).copied().unwrap_or(0.0), length) }
    }));

    true
}

/// Build an eye-facing triangle strip from a stroke's positions.
pub fn ribbon<I>(points: I, parameters: StrokeParameters, eye: Vec3, seed: u64, jitter: u64) -> Vec<[Vec3; 3]>
where
    I: IntoIterator<Item = StrokePoint> + Clone,
{
    let mut solved = Vec::new();
    if !rails(points, parameters, eye, seed, jitter, &mut solved) {
        return Vec::new();
    }

    let widened = |rail: &Rail| (rail.centre - rail.offset * rail.taper, rail.centre + rail.offset * rail.taper);
    let mut triangles = Vec::with_capacity((solved.len() - 1) * 2);
    let mut previous = widened(&solved[0]);
    for rail in &solved[1..] {
        let current = widened(rail);
        triangles.push([previous.0, current.0, current.1]);
        triangles.push([previous.0, current.1, previous.1]);
        previous = current;
    }

    triangles
}

fn anchor(point: StrokePoint, along: Vec3, parameters: StrokeParameters, seed: u64) -> Anchor {
    Anchor {
        pos: point.pos,
        along,
        half: parameters.angular_half_width * point.weight,
        drift: wander(seed, point.pos) * parameters.angular_wobble * parameters.wobble_scale,
    }
}

fn hash64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = value;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

fn hash_unit(seed: u64) -> f32 {
    (hash64(seed) >> 40) as f32 / (1u64 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARAMETERS: StrokeParameters = StrokeParameters {
        angular_half_width: 0.002,
        angular_wobble: 0.0005,
        wobble_scale: 1.0,
        minimum_angular_length: 0.0,
    };

    fn point(x: f32, y: f32, z: f32, weight: f32) -> StrokePoint {
        StrokePoint { pos: Vec3::new(x, y, z), weight }
    }

    #[test]
    fn rail_offset_is_perpendicular_to_chord_and_view() {
        let points = [point(-1.0, 0.0, 0.0, 1.0), point(1.0, 0.0, 0.0, 1.0)];
        let mut anchored = Vec::new();
        assert!(anchors(points, PARAMETERS, 7, 11, &mut anchored));

        let view = Vec3::new(0.0, 0.0, 4.0) - anchored[0].pos;
        let (_, offset) = rail(&anchored[0], view.length(), Vec3::new(0.0, 0.0, 4.0));

        assert!(offset.length() > 0.0);
        assert!(offset.dot(anchored[0].along).abs() < 1e-7);
        assert!(offset.dot(view).abs() < 1e-7);
    }

    #[test]
    fn world_half_width_scales_with_depth() {
        let anchor = Anchor { pos: Vec3::ZERO, along: Vec3::new(1.0, 0.0, 0.0), half: 0.002, drift: 0.0 };
        let (_, near) = rail(&anchor, 2.0, Vec3::new(0.0, 0.0, 2.0));
        let (_, far) = rail(&anchor, 4.0, Vec3::new(0.0, 0.0, 4.0));

        assert_eq!((far.length() / 4.0).to_bits(), (near.length() / 2.0).to_bits());
        assert_eq!(far.length().to_bits(), (near.length() * 2.0).to_bits());
    }

    #[test]
    fn fixed_inputs_are_bit_identical() {
        let points = [point(-0.7, 0.2, 0.4, 0.65), point(0.1, 0.8, 0.6, 1.2), point(0.9, 0.3, 1.1, 0.8)];
        let eye = Vec3::new(2.4, -1.3, 5.7);
        let first = ribbon(points, PARAMETERS, eye, 0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321);
        let second = ribbon(points, PARAMETERS, eye, 0x1234_5678_9abc_def0, 0x0fed_cba9_8765_4321);

        let bits = |triangles: Vec<[Vec3; 3]>| {
            triangles
                .into_iter()
                .map(|triangle| triangle.map(|vertex| [vertex.x.to_bits(), vertex.y.to_bits(), vertex.z.to_bits()]))
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(first), bits(second));
    }
}
