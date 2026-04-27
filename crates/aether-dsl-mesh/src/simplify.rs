//! AST simplification: pure `Node → Node` rewrites that preserve the
//! mesh result while reducing the work the mesher has to do.
//!
//! This module is the foundation for issue 300's BSP fragmentation
//! cliff: when CSG inputs come from disjoint AABBs (or from primitives
//! we can decompose into convex chunks at the AST level), `mesh()` can
//! skip the BSP composition entirely and concat polygon streams. None
//! of that is implemented here — this PR ships only:
//!
//! - [`Aabb`]: an axis-aligned bounding box with the standard set ops.
//! - [`compute_aabb`]: per-`Node` conservative bound, used by future
//!   AABB-pruning rewrites to decide when a CSG composition is trivial.
//! - [`simplify`]: the rewrite driver, currently applying only a few
//!   identity-collapse rules (no-op transforms, single-child wrappers).
//!
//! [`simplify`] is **not** wired into [`crate::mesh::mesh`] yet — the
//! intent is to introduce it as pure infrastructure with full test
//! coverage, then wire it up when the first non-identity rewrite
//! lands. This keeps each PR's risk scoped to one behavior change.

use crate::ast::{Axis, Node};
use crate::mesh::{normalize_or_default, rotate_axis_angle};

/// Axis-aligned bounding box in world coordinates.
///
/// `min[i] > max[i]` along any axis denotes the empty box (no points).
/// [`Aabb::EMPTY`] uses `+∞` / `-∞` so unioning anything with it
/// returns the other operand unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl Aabb {
    pub const EMPTY: Aabb = Aabb {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };

    /// Construct an AABB centered at the origin with the given half-extents.
    /// Half-extents may be zero (degenerate box) or negative (treated as
    /// their absolute value).
    pub fn from_half_extents(hx: f32, hy: f32, hz: f32) -> Aabb {
        let hx = hx.abs();
        let hy = hy.abs();
        let hz = hz.abs();
        Aabb {
            min: [-hx, -hy, -hz],
            max: [hx, hy, hz],
        }
    }

    /// Construct an AABB from explicit bounds. Caller is responsible for
    /// `min[i] <= max[i]`; passing inverted bounds yields an empty AABB.
    pub fn from_min_max(min: [f32; 3], max: [f32; 3]) -> Aabb {
        Aabb { min, max }
    }

    pub fn is_empty(&self) -> bool {
        self.min[0] > self.max[0] || self.min[1] > self.max[1] || self.min[2] > self.max[2]
    }

    /// Smallest AABB containing both `self` and `other`. Either being
    /// empty returns the other unchanged.
    pub fn union(&self, other: &Aabb) -> Aabb {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        Aabb {
            min: [
                self.min[0].min(other.min[0]),
                self.min[1].min(other.min[1]),
                self.min[2].min(other.min[2]),
            ],
            max: [
                self.max[0].max(other.max[0]),
                self.max[1].max(other.max[1]),
                self.max[2].max(other.max[2]),
            ],
        }
    }

    /// Largest AABB contained in both `self` and `other`. Returns an
    /// empty AABB when the inputs don't overlap.
    pub fn intersection(&self, other: &Aabb) -> Aabb {
        let min = [
            self.min[0].max(other.min[0]),
            self.min[1].max(other.min[1]),
            self.min[2].max(other.min[2]),
        ];
        let max = [
            self.max[0].min(other.max[0]),
            self.max[1].min(other.max[1]),
            self.max[2].min(other.max[2]),
        ];
        Aabb { min, max }
    }

    /// `true` if `self` and `other` share at least one point. Touching
    /// (a single shared face / edge / point) counts as intersecting —
    /// callers that want strict separation should test `!intersects`
    /// only when they're OK treating shared-face geometry as disjoint
    /// (which is fine for CSG: zero shared volume = trivial result).
    pub fn intersects(&self, other: &Aabb) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.min[0] <= other.max[0]
            && self.max[0] >= other.min[0]
            && self.min[1] <= other.max[1]
            && self.max[1] >= other.min[1]
            && self.min[2] <= other.max[2]
            && self.max[2] >= other.min[2]
    }

    pub fn translate(&self, offset: [f32; 3]) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        Aabb {
            min: [
                self.min[0] + offset[0],
                self.min[1] + offset[1],
                self.min[2] + offset[2],
            ],
            max: [
                self.max[0] + offset[0],
                self.max[1] + offset[1],
                self.max[2] + offset[2],
            ],
        }
    }

    /// Component-wise scale. Negative factors swap min/max along their
    /// axis so the result still satisfies `min <= max`.
    pub fn scale(&self, factor: [f32; 3]) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        let scaled_lo = [
            self.min[0] * factor[0],
            self.min[1] * factor[1],
            self.min[2] * factor[2],
        ];
        let scaled_hi = [
            self.max[0] * factor[0],
            self.max[1] * factor[1],
            self.max[2] * factor[2],
        ];
        Aabb {
            min: [
                scaled_lo[0].min(scaled_hi[0]),
                scaled_lo[1].min(scaled_hi[1]),
                scaled_lo[2].min(scaled_hi[2]),
            ],
            max: [
                scaled_lo[0].max(scaled_hi[0]),
                scaled_lo[1].max(scaled_hi[1]),
                scaled_lo[2].max(scaled_hi[2]),
            ],
        }
    }

    /// Conservative AABB after rotating around a normalized `axis` by
    /// `angle` radians: rotate the eight corners and take the new bound.
    /// The result is rotation-invariant only for AABBs centered at the
    /// origin or for axis-aligned rotations; off-center boxes get a
    /// strictly larger AABB after rotation, as expected.
    pub fn rotate(&self, axis: [f32; 3], angle: f32) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        let n = normalize_or_default(axis, [0.0, 1.0, 0.0]);
        let corners = [
            [self.min[0], self.min[1], self.min[2]],
            [self.max[0], self.min[1], self.min[2]],
            [self.min[0], self.max[1], self.min[2]],
            [self.max[0], self.max[1], self.min[2]],
            [self.min[0], self.min[1], self.max[2]],
            [self.max[0], self.min[1], self.max[2]],
            [self.min[0], self.max[1], self.max[2]],
            [self.max[0], self.max[1], self.max[2]],
        ];
        let mut out = Aabb::EMPTY;
        for c in corners {
            let r = rotate_axis_angle(c, n, angle);
            out = out.union(&Aabb { min: r, max: r });
        }
        out
    }

    /// Mirror across the plane `axis = 0`. The bounds along `axis` flip
    /// sign and swap; the other axes are unchanged.
    pub fn mirror(&self, axis: Axis) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        let i = match axis {
            Axis::X => 0,
            Axis::Y => 1,
            Axis::Z => 2,
        };
        let mut out = *self;
        let new_min = -self.max[i];
        let new_max = -self.min[i];
        out.min[i] = new_min;
        out.max[i] = new_max;
        out
    }
}

/// Conservative AABB enclosing the polygon stream `node` would emit
/// when meshed at the origin (caller-supplied offsets aren't applied —
/// they're folded into surrounding `Translate` nodes by construction).
///
/// "Conservative" means: the returned AABB is guaranteed to contain
/// every emitted polygon, but may be larger than the tight bound — the
/// rotated-AABB-of-corners step in particular never under-estimates.
/// CSG ops use the pre-CSG bounds (union of inputs) since BSP can't
/// produce geometry outside the input solids.
pub fn compute_aabb(node: &Node) -> Aabb {
    match node {
        Node::Box { x, y, z, .. } => Aabb::from_half_extents(*x * 0.5, *y * 0.5, *z * 0.5),
        Node::Cylinder { radius, height, .. } => {
            Aabb::from_half_extents(*radius, *height * 0.5, *radius)
        }
        Node::Cone { radius, height, .. } => {
            Aabb::from_half_extents(*radius, *height * 0.5, *radius)
        }
        Node::Wedge { x, y, z, .. } => Aabb::from_half_extents(*x * 0.5, *y * 0.5, *z * 0.5),
        Node::Sphere { radius, .. } => Aabb::from_half_extents(*radius, *radius, *radius),
        Node::Lathe { profile, .. } => {
            // Lathe revolves around Y axis. Radial extent is max |x| of
            // the profile; Y extent spans the profile's y range.
            let mut max_r = 0.0f32;
            let mut min_y = f32::INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for &[r, y] in profile {
                max_r = max_r.max(r.abs());
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            if !min_y.is_finite() {
                return Aabb::EMPTY;
            }
            Aabb::from_min_max([-max_r, min_y, -max_r], [max_r, max_y, max_r])
        }
        Node::Extrude { profile, depth, .. } => {
            let mut min_x = f32::INFINITY;
            let mut max_x = f32::NEG_INFINITY;
            let mut min_y = f32::INFINITY;
            let mut max_y = f32::NEG_INFINITY;
            for &[x, y] in profile {
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
            if !min_x.is_finite() {
                return Aabb::EMPTY;
            }
            // Extrude pushes profile along +Z by `depth` from z=0.
            let (z0, z1) = if *depth >= 0.0 {
                (0.0, *depth)
            } else {
                (*depth, 0.0)
            };
            Aabb::from_min_max([min_x, min_y, z0], [max_x, max_y, z1])
        }
        Node::Torus {
            major_radius,
            minor_radius,
            ..
        } => {
            // Torus is around Y axis. Radial extent = major + minor;
            // Y extent = ±minor.
            let r = major_radius + minor_radius;
            Aabb::from_half_extents(r, *minor_radius, r)
        }
        Node::Sweep {
            profile,
            path,
            scales,
            ..
        } => {
            // Conservative bound: at each waypoint, take the worst-case
            // ring radius (max |profile| × scale at that waypoint) and
            // produce a sphere-like box around the waypoint. Union them.
            let mut max_profile_r = 0.0f32;
            for &[x, y] in profile {
                max_profile_r = max_profile_r.max((x * x + y * y).sqrt());
            }
            let mut out = Aabb::EMPTY;
            for (k, p) in path.iter().enumerate() {
                let s = scales
                    .as_ref()
                    .and_then(|s| s.get(k))
                    .copied()
                    .unwrap_or(1.0);
                let r = max_profile_r * s.abs();
                out = out.union(&Aabb::from_min_max(
                    [p[0] - r, p[1] - r, p[2] - r],
                    [p[0] + r, p[1] + r, p[2] + r],
                ));
            }
            out
        }
        Node::Composition(children) => children
            .iter()
            .map(compute_aabb)
            .fold(Aabb::EMPTY, |acc, b| acc.union(&b)),
        Node::Translate { offset, child } => compute_aabb(child).translate(*offset),
        Node::Rotate { axis, angle, child } => compute_aabb(child).rotate(*axis, *angle),
        Node::Scale { factor, child } => compute_aabb(child).scale(*factor),
        Node::Mirror { axis, child } => compute_aabb(child).mirror(*axis),
        Node::Array {
            count,
            spacing,
            child,
        } => {
            if *count == 0 {
                return Aabb::EMPTY;
            }
            let base = compute_aabb(child);
            let mut out = Aabb::EMPTY;
            for i in 0..*count {
                let f = i as f32;
                out = out.union(&base.translate([spacing[0] * f, spacing[1] * f, spacing[2] * f]));
            }
            out
        }
        // CSG result is contained by the union of its inputs (BSP can
        // only carve, never extrude). For union/intersection that's
        // exactly tight; for difference it's conservatively the base
        // bound (subtractors can't enlarge).
        Node::Union { children } => children
            .iter()
            .map(compute_aabb)
            .fold(Aabb::EMPTY, |acc, b| acc.union(&b)),
        Node::Intersection { children } => {
            let mut iter = children.iter();
            let first = match iter.next() {
                Some(n) => compute_aabb(n),
                None => return Aabb::EMPTY,
            };
            iter.fold(first, |acc, n| acc.intersection(&compute_aabb(n)))
        }
        Node::Difference { base, .. } => compute_aabb(base),
    }
}

/// Apply identity-collapse rewrites bottom-up. Each rewrite preserves
/// the meshed output exactly — a `mesh(simplify(n))` is `mesh(n)` for
/// every input.
///
/// Currently fires on:
///
/// - `(translate (0 0 0) child)` → `child`
/// - `(rotate axis 0.0 child)` → `child`
/// - `(scale (1 1 1) child)` → `child`
/// - `(array 1 ... child)` → `child` (single-element array applies no
///   spacing; same as the bare child)
/// - `(composition (single))` → `single` (when the composition has
///   exactly one child)
///
/// Identity rules for `Mirror` are intentionally absent — every mirror
/// flips winding regardless of axis, so there's no zero-effect form.
pub fn simplify(node: &Node) -> Node {
    match node {
        // Primitives have no children to rewrite.
        Node::Box { .. }
        | Node::Cylinder { .. }
        | Node::Cone { .. }
        | Node::Wedge { .. }
        | Node::Sphere { .. }
        | Node::Lathe { .. }
        | Node::Extrude { .. }
        | Node::Torus { .. }
        | Node::Sweep { .. } => node.clone(),

        Node::Composition(children) => {
            let simplified: Vec<Node> = children.iter().map(simplify).collect();
            if simplified.len() == 1 {
                return simplified.into_iter().next().unwrap();
            }
            Node::Composition(simplified)
        }

        Node::Translate { offset, child } => {
            let child = simplify(child);
            if offset == &[0.0, 0.0, 0.0] {
                return child;
            }
            Node::Translate {
                offset: *offset,
                child: Box::new(child),
            }
        }

        Node::Rotate { axis, angle, child } => {
            let child = simplify(child);
            if *angle == 0.0 {
                return child;
            }
            Node::Rotate {
                axis: *axis,
                angle: *angle,
                child: Box::new(child),
            }
        }

        Node::Scale { factor, child } => {
            let child = simplify(child);
            if factor == &[1.0, 1.0, 1.0] {
                return child;
            }
            Node::Scale {
                factor: *factor,
                child: Box::new(child),
            }
        }

        Node::Mirror { axis, child } => Node::Mirror {
            axis: *axis,
            child: Box::new(simplify(child)),
        },

        Node::Array {
            count,
            spacing,
            child,
        } => {
            let child = simplify(child);
            if *count == 1 {
                return child;
            }
            Node::Array {
                count: *count,
                spacing: *spacing,
                child: Box::new(child),
            }
        }

        Node::Union { children } => Node::Union {
            children: children.iter().map(simplify).collect(),
        },
        Node::Intersection { children } => Node::Intersection {
            children: children.iter().map(simplify).collect(),
        },
        Node::Difference { base, subtract } => Node::Difference {
            base: Box::new(simplify(base)),
            subtract: subtract.iter().map(simplify).collect(),
        },
    }
}

#[cfg(test)]
mod aabb_tests {
    use super::*;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn aabb_approx_eq(a: &Aabb, b: &Aabb) -> bool {
        approx_eq(a.min[0], b.min[0])
            && approx_eq(a.min[1], b.min[1])
            && approx_eq(a.min[2], b.min[2])
            && approx_eq(a.max[0], b.max[0])
            && approx_eq(a.max[1], b.max[1])
            && approx_eq(a.max[2], b.max[2])
    }

    #[test]
    fn empty_is_empty() {
        assert!(Aabb::EMPTY.is_empty());
    }

    #[test]
    fn from_half_extents_is_centered() {
        let b = Aabb::from_half_extents(2.0, 3.0, 4.0);
        assert_eq!(b.min, [-2.0, -3.0, -4.0]);
        assert_eq!(b.max, [2.0, 3.0, 4.0]);
    }

    #[test]
    fn from_half_extents_takes_absolute_value() {
        // Negative extents would otherwise produce inverted (= empty)
        // bounds. Take abs so callers can't accidentally produce empty
        // boxes from primitives with negative size.
        let b = Aabb::from_half_extents(-2.0, 3.0, -4.0);
        assert_eq!(b.min, [-2.0, -3.0, -4.0]);
        assert_eq!(b.max, [2.0, 3.0, 4.0]);
    }

    #[test]
    fn union_with_empty_is_identity() {
        let b = Aabb::from_half_extents(1.0, 1.0, 1.0);
        assert_eq!(b.union(&Aabb::EMPTY), b);
        assert_eq!(Aabb::EMPTY.union(&b), b);
    }

    #[test]
    fn union_of_two_disjoint_boxes_spans_both() {
        let a = Aabb::from_min_max([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let b = Aabb::from_min_max([10.0, 10.0, 10.0], [11.0, 11.0, 11.0]);
        let u = a.union(&b);
        assert_eq!(u.min, [0.0, 0.0, 0.0]);
        assert_eq!(u.max, [11.0, 11.0, 11.0]);
    }

    #[test]
    fn intersection_of_overlapping_boxes_is_overlap() {
        let a = Aabb::from_min_max([0.0, 0.0, 0.0], [2.0, 2.0, 2.0]);
        let b = Aabb::from_min_max([1.0, 1.0, 1.0], [3.0, 3.0, 3.0]);
        let i = a.intersection(&b);
        assert_eq!(i.min, [1.0, 1.0, 1.0]);
        assert_eq!(i.max, [2.0, 2.0, 2.0]);
    }

    #[test]
    fn intersection_of_disjoint_boxes_is_empty() {
        let a = Aabb::from_min_max([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let b = Aabb::from_min_max([10.0, 10.0, 10.0], [11.0, 11.0, 11.0]);
        let i = a.intersection(&b);
        assert!(i.is_empty());
    }

    #[test]
    fn intersects_overlapping_returns_true() {
        let a = Aabb::from_min_max([0.0, 0.0, 0.0], [2.0, 2.0, 2.0]);
        let b = Aabb::from_min_max([1.0, 1.0, 1.0], [3.0, 3.0, 3.0]);
        assert!(a.intersects(&b));
        assert!(b.intersects(&a));
    }

    #[test]
    fn intersects_disjoint_returns_false() {
        let a = Aabb::from_min_max([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let b = Aabb::from_min_max([10.0, 10.0, 10.0], [11.0, 11.0, 11.0]);
        assert!(!a.intersects(&b));
    }

    #[test]
    fn intersects_touching_face_returns_true() {
        // Pin: AABBs that share a face (max == min along one axis) are
        // treated as intersecting. CSG callers can still optimize this
        // case if they want to — the intersects API just gives the
        // closed-set answer.
        let a = Aabb::from_min_max([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let b = Aabb::from_min_max([1.0, 0.0, 0.0], [2.0, 1.0, 1.0]);
        assert!(a.intersects(&b));
    }

    #[test]
    fn intersects_with_empty_is_false() {
        let a = Aabb::from_min_max([0.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        assert!(!a.intersects(&Aabb::EMPTY));
        assert!(!Aabb::EMPTY.intersects(&a));
        assert!(!Aabb::EMPTY.intersects(&Aabb::EMPTY));
    }

    #[test]
    fn translate_shifts_bounds() {
        let b = Aabb::from_half_extents(1.0, 1.0, 1.0);
        let t = b.translate([5.0, -3.0, 2.0]);
        assert_eq!(t.min, [4.0, -4.0, 1.0]);
        assert_eq!(t.max, [6.0, -2.0, 3.0]);
    }

    #[test]
    fn translate_empty_is_empty() {
        let t = Aabb::EMPTY.translate([5.0, 5.0, 5.0]);
        assert!(t.is_empty());
    }

    #[test]
    fn scale_positive_factors_scales_in_place() {
        let b = Aabb::from_half_extents(1.0, 1.0, 1.0);
        let s = b.scale([2.0, 3.0, 4.0]);
        assert_eq!(s.min, [-2.0, -3.0, -4.0]);
        assert_eq!(s.max, [2.0, 3.0, 4.0]);
    }

    #[test]
    fn scale_negative_factor_swaps_min_max_along_axis() {
        // Pin: a -1 scale on x flips a [0, 5] range to [-5, 0]. Without
        // swap-on-negative the result would be the inverted (empty)
        // [0, -5].
        let b = Aabb::from_min_max([0.0, 0.0, 0.0], [5.0, 1.0, 1.0]);
        let s = b.scale([-1.0, 1.0, 1.0]);
        assert_eq!(s.min, [-5.0, 0.0, 0.0]);
        assert_eq!(s.max, [0.0, 1.0, 1.0]);
    }

    #[test]
    fn rotate_180_around_y_swaps_x_and_z_signs() {
        // Off-center box rotated 180° around Y: end up on the opposite
        // side along x and z, but still the same shape.
        let b = Aabb::from_min_max([1.0, 0.0, 1.0], [3.0, 1.0, 3.0]);
        let r = b.rotate([0.0, 1.0, 0.0], std::f32::consts::PI);
        // Floating-point Rodrigues introduces small ULPs even for
        // exact 180° rotations; assert via approx.
        let expected = Aabb::from_min_max([-3.0, 0.0, -3.0], [-1.0, 1.0, -1.0]);
        assert!(
            aabb_approx_eq(&r, &expected),
            "rotated bounds {:?} ≠ expected {:?}",
            r,
            expected
        );
    }

    #[test]
    fn rotate_centered_box_45_degrees_around_z_grows_xy_extent() {
        // A 1x1 box rotated 45° around Z should fit in a 1.414x1.414
        // bound (the corner-to-corner diagonal). Pins the worst-case
        // conservative-bound behavior.
        let b = Aabb::from_half_extents(0.5, 0.5, 0.0);
        let r = b.rotate([0.0, 0.0, 1.0], std::f32::consts::FRAC_PI_4);
        // sqrt(0.5) ≈ 0.7071
        let half_diag = std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (r.max[0] - half_diag).abs() < 1e-4,
            "expected ~{half_diag} along x, got {}",
            r.max[0]
        );
        assert!(
            (r.max[1] - half_diag).abs() < 1e-4,
            "expected ~{half_diag} along y, got {}",
            r.max[1]
        );
    }

    #[test]
    fn mirror_x_flips_x_bounds() {
        let b = Aabb::from_min_max([1.0, 2.0, 3.0], [4.0, 5.0, 6.0]);
        let m = b.mirror(Axis::X);
        assert_eq!(m.min, [-4.0, 2.0, 3.0]);
        assert_eq!(m.max, [-1.0, 5.0, 6.0]);
    }

    #[test]
    fn mirror_y_flips_y_bounds() {
        let b = Aabb::from_min_max([1.0, 2.0, 3.0], [4.0, 5.0, 6.0]);
        let m = b.mirror(Axis::Y);
        assert_eq!(m.min, [1.0, -5.0, 3.0]);
        assert_eq!(m.max, [4.0, -2.0, 6.0]);
    }

    #[test]
    fn mirror_z_flips_z_bounds() {
        let b = Aabb::from_min_max([1.0, 2.0, 3.0], [4.0, 5.0, 6.0]);
        let m = b.mirror(Axis::Z);
        assert_eq!(m.min, [1.0, 2.0, -6.0]);
        assert_eq!(m.max, [4.0, 5.0, -3.0]);
    }
}

#[cfg(test)]
mod compute_aabb_tests {
    use super::*;

    #[test]
    fn box_is_centered_at_origin_with_half_extents() {
        let b = compute_aabb(&Node::Box {
            x: 2.0,
            y: 4.0,
            z: 6.0,
            color: 0,
        });
        assert_eq!(b.min, [-1.0, -2.0, -3.0]);
        assert_eq!(b.max, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn cylinder_radius_dominates_xz() {
        let b = compute_aabb(&Node::Cylinder {
            radius: 2.5,
            height: 3.0,
            segments: 16,
            color: 0,
        });
        assert_eq!(b.min, [-2.5, -1.5, -2.5]);
        assert_eq!(b.max, [2.5, 1.5, 2.5]);
    }

    #[test]
    fn cone_aabb_is_full_cylinder_bound() {
        // The bound is conservative: the cone sits inside a cylinder of
        // the same radius and height, so AABB matches a cylinder.
        let b = compute_aabb(&Node::Cone {
            radius: 1.0,
            height: 2.0,
            segments: 12,
            color: 0,
        });
        assert_eq!(b.min, [-1.0, -1.0, -1.0]);
        assert_eq!(b.max, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn sphere_is_radius_in_all_axes() {
        let b = compute_aabb(&Node::Sphere {
            radius: 0.5,
            subdivisions: 12,
            color: 0,
        });
        assert_eq!(b.min, [-0.5, -0.5, -0.5]);
        assert_eq!(b.max, [0.5, 0.5, 0.5]);
    }

    #[test]
    fn lathe_radial_extent_is_max_abs_x() {
        let b = compute_aabb(&Node::Lathe {
            profile: vec![[0.0, -0.5], [0.7, -0.5], [0.7, 0.5], [0.0, 0.5]],
            segments: 16,
            color: 0,
        });
        assert_eq!(b.min, [-0.7, -0.5, -0.7]);
        assert_eq!(b.max, [0.7, 0.5, 0.7]);
    }

    #[test]
    fn lathe_with_empty_profile_is_empty() {
        let b = compute_aabb(&Node::Lathe {
            profile: vec![],
            segments: 16,
            color: 0,
        });
        assert!(b.is_empty());
    }

    #[test]
    fn torus_radial_is_major_plus_minor() {
        let b = compute_aabb(&Node::Torus {
            major_radius: 2.0,
            minor_radius: 0.3,
            major_segments: 16,
            minor_segments: 8,
            color: 0,
        });
        assert_eq!(b.min, [-2.3, -0.3, -2.3]);
        assert_eq!(b.max, [2.3, 0.3, 2.3]);
    }

    #[test]
    fn extrude_z_spans_zero_to_depth() {
        let b = compute_aabb(&Node::Extrude {
            profile: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 2.0], [0.0, 2.0]],
            depth: 5.0,
            color: 0,
        });
        assert_eq!(b.min, [0.0, 0.0, 0.0]);
        assert_eq!(b.max, [1.0, 2.0, 5.0]);
    }

    #[test]
    fn extrude_negative_depth_swaps_z_bounds() {
        // Degenerate but defined: negative depth extrudes backward.
        let b = compute_aabb(&Node::Extrude {
            profile: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]],
            depth: -3.0,
            color: 0,
        });
        assert_eq!(b.min[2], -3.0);
        assert_eq!(b.max[2], 0.0);
    }

    #[test]
    fn translate_offsets_child_bounds() {
        let b = compute_aabb(&Node::Translate {
            offset: [10.0, 20.0, 30.0],
            child: Box::new(Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            }),
        });
        assert_eq!(b.min, [9.0, 19.0, 29.0]);
        assert_eq!(b.max, [11.0, 21.0, 31.0]);
    }

    #[test]
    fn rotate_grows_off_axis_box_bound() {
        // 4x1x1 box rotated 90° around Z: x-extent becomes 1, y-extent
        // becomes 4. Pin the conservative bound matches the rotated
        // shape.
        let b = compute_aabb(&Node::Rotate {
            axis: [0.0, 0.0, 1.0],
            angle: std::f32::consts::FRAC_PI_2,
            child: Box::new(Node::Box {
                x: 4.0,
                y: 1.0,
                z: 1.0,
                color: 0,
            }),
        });
        assert!((b.max[0] - 0.5).abs() < 1e-4, "x_max {}", b.max[0]);
        assert!((b.max[1] - 2.0).abs() < 1e-4, "y_max {}", b.max[1]);
    }

    #[test]
    fn scale_multiplies_child_bound() {
        let b = compute_aabb(&Node::Scale {
            factor: [2.0, 3.0, 4.0],
            child: Box::new(Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 0,
            }),
        });
        assert_eq!(b.min, [-1.0, -1.5, -2.0]);
        assert_eq!(b.max, [1.0, 1.5, 2.0]);
    }

    #[test]
    fn mirror_flips_child_bound_along_axis() {
        let b = compute_aabb(&Node::Mirror {
            axis: Axis::X,
            child: Box::new(Node::Translate {
                offset: [5.0, 0.0, 0.0],
                child: Box::new(Node::Box {
                    x: 2.0,
                    y: 2.0,
                    z: 2.0,
                    color: 0,
                }),
            }),
        });
        // Box translated to (5,0,0), bounds [4..6]; mirror around x=0
        // gives [-6..-4].
        assert_eq!(b.min[0], -6.0);
        assert_eq!(b.max[0], -4.0);
    }

    #[test]
    fn array_unions_translated_child_bounds() {
        let b = compute_aabb(&Node::Array {
            count: 3,
            spacing: [10.0, 0.0, 0.0],
            child: Box::new(Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            }),
        });
        // i=0: [-1..1], i=1: [9..11], i=2: [19..21] → union [-1..21].
        assert_eq!(b.min[0], -1.0);
        assert_eq!(b.max[0], 21.0);
    }

    #[test]
    fn array_count_zero_is_empty() {
        let b = compute_aabb(&Node::Array {
            count: 0,
            spacing: [1.0, 0.0, 0.0],
            child: Box::new(Node::Box {
                x: 1.0,
                y: 1.0,
                z: 1.0,
                color: 0,
            }),
        });
        assert!(b.is_empty());
    }

    #[test]
    fn composition_unions_children() {
        let b = compute_aabb(&Node::Composition(vec![
            Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            },
            Node::Translate {
                offset: [10.0, 0.0, 0.0],
                child: Box::new(Node::Box {
                    x: 2.0,
                    y: 2.0,
                    z: 2.0,
                    color: 0,
                }),
            },
        ]));
        assert_eq!(b.min[0], -1.0);
        assert_eq!(b.max[0], 11.0);
    }

    #[test]
    fn empty_composition_is_empty() {
        assert!(compute_aabb(&Node::Composition(vec![])).is_empty());
    }

    #[test]
    fn union_takes_union_of_child_bounds() {
        let b = compute_aabb(&Node::Union {
            children: vec![
                Node::Box {
                    x: 2.0,
                    y: 2.0,
                    z: 2.0,
                    color: 0,
                },
                Node::Translate {
                    offset: [10.0, 0.0, 0.0],
                    child: Box::new(Node::Box {
                        x: 2.0,
                        y: 2.0,
                        z: 2.0,
                        color: 0,
                    }),
                },
            ],
        });
        assert_eq!(b.min[0], -1.0);
        assert_eq!(b.max[0], 11.0);
    }

    #[test]
    fn intersection_takes_intersection_of_child_bounds() {
        let b = compute_aabb(&Node::Intersection {
            children: vec![
                Node::Box {
                    x: 4.0,
                    y: 4.0,
                    z: 4.0,
                    color: 0,
                },
                Node::Translate {
                    offset: [1.0, 0.0, 0.0],
                    child: Box::new(Node::Box {
                        x: 4.0,
                        y: 4.0,
                        z: 4.0,
                        color: 0,
                    }),
                },
            ],
        });
        // Box[-2..2] ∩ Box[-1..3] = [-1..2]
        assert_eq!(b.min[0], -1.0);
        assert_eq!(b.max[0], 2.0);
    }

    #[test]
    fn intersection_of_disjoint_inputs_is_empty_bound() {
        let b = compute_aabb(&Node::Intersection {
            children: vec![
                Node::Box {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                    color: 0,
                },
                Node::Translate {
                    offset: [10.0, 0.0, 0.0],
                    child: Box::new(Node::Box {
                        x: 1.0,
                        y: 1.0,
                        z: 1.0,
                        color: 0,
                    }),
                },
            ],
        });
        assert!(b.is_empty());
    }

    #[test]
    fn difference_uses_base_bound_only() {
        // Pin: subtractor's bound doesn't enlarge the difference's
        // bound. Otherwise a tiny base minus a giant subtractor would
        // claim AABB(giant) — wrong, since BSP can't enlarge geometry.
        let b = compute_aabb(&Node::Difference {
            base: Box::new(Node::Box {
                x: 2.0,
                y: 2.0,
                z: 2.0,
                color: 0,
            }),
            subtract: vec![Node::Box {
                x: 100.0,
                y: 100.0,
                z: 100.0,
                color: 1,
            }],
        });
        assert_eq!(b.min, [-1.0, -1.0, -1.0]);
        assert_eq!(b.max, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn sweep_bound_covers_every_waypoint_with_max_profile_radius() {
        let b = compute_aabb(&Node::Sweep {
            profile: vec![[1.0, 0.0], [0.0, 1.0], [-1.0, 0.0], [0.0, -1.0]],
            path: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]],
            scales: None,
            color: 0,
        });
        // Profile worst-case radius = 1; bound at each waypoint extends
        // ±1 in every direction → union [(-1, -1, -1), (11, 1, 1)].
        assert_eq!(b.min, [-1.0, -1.0, -1.0]);
        assert_eq!(b.max, [11.0, 1.0, 1.0]);
    }

    #[test]
    fn sweep_with_scales_uses_per_waypoint_scale() {
        let b = compute_aabb(&Node::Sweep {
            profile: vec![[1.0, 0.0], [0.0, 1.0], [-1.0, 0.0], [0.0, -1.0]],
            path: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]],
            scales: Some(vec![1.0, 3.0]),
            color: 0,
        });
        // Waypoint 0: ±1; waypoint 1 (scaled 3x): ±3. y/z bounds come
        // from waypoint 1 alone (largest).
        assert_eq!(b.min[1], -3.0);
        assert_eq!(b.max[1], 3.0);
    }
}

#[cfg(test)]
mod simplify_tests {
    use super::*;

    fn unit_box() -> Node {
        Node::Box {
            x: 1.0,
            y: 1.0,
            z: 1.0,
            color: 0,
        }
    }

    #[test]
    fn primitive_unchanged() {
        let n = unit_box();
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn zero_translate_is_stripped() {
        let n = Node::Translate {
            offset: [0.0, 0.0, 0.0],
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn nonzero_translate_is_preserved() {
        let n = Node::Translate {
            offset: [1.0, 0.0, 0.0],
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn zero_rotate_is_stripped_regardless_of_axis() {
        // Pin: even a non-normalized axis is fine — angle=0 means no
        // rotation no matter what axis is named.
        let n = Node::Rotate {
            axis: [1.0, 1.0, 1.0],
            angle: 0.0,
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn nonzero_rotate_is_preserved() {
        let n = Node::Rotate {
            axis: [0.0, 1.0, 0.0],
            angle: 0.5,
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn unit_scale_is_stripped() {
        let n = Node::Scale {
            factor: [1.0, 1.0, 1.0],
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn non_unit_scale_is_preserved() {
        let n = Node::Scale {
            factor: [2.0, 1.0, 1.0],
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn array_count_one_is_stripped() {
        let n = Node::Array {
            count: 1,
            spacing: [10.0, 0.0, 0.0],
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn array_count_two_is_preserved() {
        let n = Node::Array {
            count: 2,
            spacing: [10.0, 0.0, 0.0],
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn single_child_composition_collapses_to_child() {
        let n = Node::Composition(vec![unit_box()]);
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn multi_child_composition_is_preserved() {
        let n = Node::Composition(vec![unit_box(), unit_box()]);
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn nested_identities_collapse_through_recursion() {
        // (translate (0,0,0) (rotate any 0 (scale (1,1,1) box))) → box
        let n = Node::Translate {
            offset: [0.0, 0.0, 0.0],
            child: Box::new(Node::Rotate {
                axis: [1.0, 0.0, 0.0],
                angle: 0.0,
                child: Box::new(Node::Scale {
                    factor: [1.0, 1.0, 1.0],
                    child: Box::new(unit_box()),
                }),
            }),
        };
        assert_eq!(simplify(&n), unit_box());
    }

    #[test]
    fn mirror_is_never_an_identity() {
        // Pin: mirror always flips, so even a trivial-looking child
        // isn't stripped.
        let n = Node::Mirror {
            axis: Axis::X,
            child: Box::new(unit_box()),
        };
        assert_eq!(simplify(&n), n);
    }

    #[test]
    fn composition_recurses_into_children() {
        // Inner identity gets stripped even though the parent
        // composition stays.
        let n = Node::Composition(vec![
            unit_box(),
            Node::Scale {
                factor: [1.0, 1.0, 1.0],
                child: Box::new(unit_box()),
            },
        ]);
        let s = simplify(&n);
        match s {
            Node::Composition(ref children) => {
                assert_eq!(children.len(), 2);
                assert_eq!(children[0], unit_box());
                assert_eq!(children[1], unit_box());
            }
            _ => panic!("expected Composition, got {s:?}"),
        }
    }

    #[test]
    fn csg_recurses_into_operands() {
        let n = Node::Difference {
            base: Box::new(Node::Translate {
                offset: [0.0, 0.0, 0.0],
                child: Box::new(unit_box()),
            }),
            subtract: vec![Node::Scale {
                factor: [1.0, 1.0, 1.0],
                child: Box::new(unit_box()),
            }],
        };
        let s = simplify(&n);
        match s {
            Node::Difference { base, subtract } => {
                assert_eq!(*base, unit_box());
                assert_eq!(subtract, vec![unit_box()]);
            }
            _ => panic!("expected Difference, got {s:?}"),
        }
    }

    #[test]
    fn simplify_is_idempotent() {
        // Pin: applying simplify twice gives the same result as once.
        // Catches future rewrites that might leave the tree in a state
        // a second pass would further reduce — at which point the rule
        // should be reapplied internally instead of leaking.
        let n = Node::Translate {
            offset: [0.0, 0.0, 0.0],
            child: Box::new(Node::Rotate {
                axis: [0.0, 1.0, 0.0],
                angle: 0.0,
                child: Box::new(Node::Composition(vec![unit_box()])),
            }),
        };
        let once = simplify(&n);
        let twice = simplify(&once);
        assert_eq!(once, twice);
    }
}
