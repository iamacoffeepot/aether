use crate::vec::Vec3;

/// Axis-aligned bounding box in `f32` world coordinates.
///
/// `min[i] > max[i]` along any axis denotes the empty box (no points).
/// [`Aabb::EMPTY`] uses `+∞` / `-∞` so unioning anything with it
/// returns the other operand unchanged — convenient as an accumulator.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub const EMPTY: Aabb = Aabb {
        min: Vec3::splat(f32::INFINITY),
        max: Vec3::splat(f32::NEG_INFINITY),
    };

    #[inline]
    pub const fn from_min_max(min: Vec3, max: Vec3) -> Aabb {
        Aabb { min, max }
    }

    /// Construct an AABB centered at the origin with the given half-extents.
    /// Half-extents may be zero (degenerate box) or negative (treated as
    /// their absolute value).
    #[inline]
    pub fn from_half_extents(hx: f32, hy: f32, hz: f32) -> Aabb {
        let hx = hx.abs();
        let hy = hy.abs();
        let hz = hz.abs();
        Aabb {
            min: Vec3::new(-hx, -hy, -hz),
            max: Vec3::new(hx, hy, hz),
        }
    }

    /// Smallest AABB containing every supplied point. Returns
    /// [`Aabb::EMPTY`] if the slice is empty.
    pub fn from_points(points: &[Vec3]) -> Aabb {
        let mut out = Self::EMPTY;
        for p in points {
            out.expand_to_point(*p);
        }
        out
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x || self.min.y > self.max.y || self.min.z > self.max.z
    }

    #[inline]
    pub fn center(&self) -> Vec3 {
        (self.min + self.max) * 0.5
    }

    #[inline]
    pub fn extents(&self) -> Vec3 {
        self.max - self.min
    }

    /// The eight corners in a fixed order:
    /// `(min,min,min) (max,min,min) (min,max,min) (max,max,min)
    ///  (min,min,max) (max,min,max) (min,max,max) (max,max,max)`.
    pub fn corners(&self) -> [Vec3; 8] {
        let mn = self.min;
        let mx = self.max;
        [
            Vec3::new(mn.x, mn.y, mn.z),
            Vec3::new(mx.x, mn.y, mn.z),
            Vec3::new(mn.x, mx.y, mn.z),
            Vec3::new(mx.x, mx.y, mn.z),
            Vec3::new(mn.x, mn.y, mx.z),
            Vec3::new(mx.x, mn.y, mx.z),
            Vec3::new(mn.x, mx.y, mx.z),
            Vec3::new(mx.x, mx.y, mx.z),
        ]
    }

    pub fn contains_point(&self, p: Vec3) -> bool {
        if self.is_empty() {
            return false;
        }
        p.x >= self.min.x
            && p.x <= self.max.x
            && p.y >= self.min.y
            && p.y <= self.max.y
            && p.z >= self.min.z
            && p.z <= self.max.z
    }

    /// Grow `self` (in place) to include `p`. No-op if `p` is already
    /// inside; correctly initialises an [`EMPTY`](Self::EMPTY) accumulator.
    pub fn expand_to_point(&mut self, p: Vec3) {
        self.min.x = self.min.x.min(p.x);
        self.min.y = self.min.y.min(p.y);
        self.min.z = self.min.z.min(p.z);
        self.max.x = self.max.x.max(p.x);
        self.max.y = self.max.y.max(p.y);
        self.max.z = self.max.z.max(p.z);
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
            min: Vec3::new(
                self.min.x.min(other.min.x),
                self.min.y.min(other.min.y),
                self.min.z.min(other.min.z),
            ),
            max: Vec3::new(
                self.max.x.max(other.max.x),
                self.max.y.max(other.max.y),
                self.max.z.max(other.max.z),
            ),
        }
    }

    /// Largest AABB contained in both `self` and `other`. Returns an
    /// empty AABB when the inputs don't overlap.
    pub fn intersection(&self, other: &Aabb) -> Aabb {
        Aabb {
            min: Vec3::new(
                self.min.x.max(other.min.x),
                self.min.y.max(other.min.y),
                self.min.z.max(other.min.z),
            ),
            max: Vec3::new(
                self.max.x.min(other.max.x),
                self.max.y.min(other.max.y),
                self.max.z.min(other.max.z),
            ),
        }
    }

    /// `true` if `self` and `other` share at least one point. Touching
    /// (a single shared face / edge / point) counts as intersecting.
    pub fn intersects(&self, other: &Aabb) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
            && self.min.z <= other.max.z
            && self.max.z >= other.min.z
    }

    pub fn translate(&self, offset: Vec3) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        Aabb {
            min: self.min + offset,
            max: self.max + offset,
        }
    }

    /// Component-wise scale. Negative factors swap min/max along their
    /// axis so the result still satisfies `min <= max`.
    pub fn scale(&self, factor: Vec3) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        let lo = Vec3::new(
            self.min.x * factor.x,
            self.min.y * factor.y,
            self.min.z * factor.z,
        );
        let hi = Vec3::new(
            self.max.x * factor.x,
            self.max.y * factor.y,
            self.max.z * factor.z,
        );
        Aabb {
            min: Vec3::new(lo.x.min(hi.x), lo.y.min(hi.y), lo.z.min(hi.z)),
            max: Vec3::new(lo.x.max(hi.x), lo.y.max(hi.y), lo.z.max(hi.z)),
        }
    }

    /// Conservative AABB after rotating around `axis` by `angle`
    /// radians: rotate the eight corners and take the new bound. The
    /// axis is normalized internally (zero-length axis falls back to
    /// `+Y`, matching the convention used elsewhere in the crate).
    ///
    /// The result is rotation-invariant only for AABBs centered at the
    /// origin or for axis-aligned rotations; off-center boxes get a
    /// strictly larger AABB after rotation, as expected.
    pub fn rotate(&self, axis: Vec3, angle: f32) -> Aabb {
        if self.is_empty() {
            return *self;
        }
        let n = axis.normalize_or(Vec3::Y);
        let mut out = Aabb::EMPTY;
        for c in self.corners() {
            out.expand_to_point(c.rotate_axis_angle(n, angle));
        }
        out
    }

    /// Mirror across the plane `axis_index = 0` (`0 = X, 1 = Y, 2 = Z`).
    /// The bounds along that axis flip sign and swap; the others are
    /// unchanged. Panics if `axis_index > 2`.
    pub fn mirror(&self, axis_index: usize) -> Aabb {
        assert!(axis_index < 3, "axis_index must be 0, 1, or 2");
        if self.is_empty() {
            return *self;
        }
        let mut out = *self;
        let (lo, hi) = match axis_index {
            0 => {
                let lo = -self.max.x;
                let hi = -self.min.x;
                out.min.x = lo;
                out.max.x = hi;
                (lo, hi)
            }
            1 => {
                let lo = -self.max.y;
                let hi = -self.min.y;
                out.min.y = lo;
                out.max.y = hi;
                (lo, hi)
            }
            _ => {
                let lo = -self.max.z;
                let hi = -self.min.z;
                out.min.z = lo;
                out.max.z = hi;
                (lo, hi)
            }
        };
        debug_assert!(lo <= hi);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PI;

    const EPS: f32 = 1e-5;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS
    }

    fn approx_aabb(a: Aabb, b: Aabb) -> bool {
        approx_eq(a.min.x, b.min.x)
            && approx_eq(a.min.y, b.min.y)
            && approx_eq(a.min.z, b.min.z)
            && approx_eq(a.max.x, b.max.x)
            && approx_eq(a.max.y, b.max.y)
            && approx_eq(a.max.z, b.max.z)
    }

    #[test]
    fn empty_is_empty() {
        assert!(Aabb::EMPTY.is_empty());
        let unit = Aabb::from_half_extents(1.0, 1.0, 1.0);
        assert!(!unit.is_empty());
    }

    #[test]
    fn from_half_extents_negative_taken_abs() {
        let a = Aabb::from_half_extents(-1.0, -2.0, -3.0);
        assert_eq!(a.min, Vec3::new(-1.0, -2.0, -3.0));
        assert_eq!(a.max, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn from_points_collects_extremes() {
        let pts = [
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(-1.0, 2.0, -3.0),
            Vec3::new(0.5, -1.0, 5.0),
        ];
        let bb = Aabb::from_points(&pts);
        assert_eq!(bb.min, Vec3::new(-1.0, -1.0, -3.0));
        assert_eq!(bb.max, Vec3::new(1.0, 2.0, 5.0));
    }

    #[test]
    fn from_points_empty_returns_empty_aabb() {
        assert!(Aabb::from_points(&[]).is_empty());
    }

    #[test]
    fn center_and_extents() {
        let a = Aabb::from_min_max(Vec3::new(-1.0, 0.0, -2.0), Vec3::new(1.0, 4.0, 0.0));
        assert_eq!(a.center(), Vec3::new(0.0, 2.0, -1.0));
        assert_eq!(a.extents(), Vec3::new(2.0, 4.0, 2.0));
    }

    #[test]
    fn corners_count_and_match_min_max() {
        let a = Aabb::from_half_extents(1.0, 2.0, 3.0);
        let cs = a.corners();
        assert_eq!(cs.len(), 8);
        assert!(cs.contains(&Vec3::new(-1.0, -2.0, -3.0)));
        assert!(cs.contains(&Vec3::new(1.0, 2.0, 3.0)));
    }

    #[test]
    fn contains_point_inside_outside_boundary() {
        let a = Aabb::from_half_extents(1.0, 1.0, 1.0);
        assert!(a.contains_point(Vec3::ZERO));
        assert!(a.contains_point(Vec3::new(1.0, 1.0, 1.0)));
        assert!(!a.contains_point(Vec3::new(1.0001, 0.0, 0.0)));
        assert!(!Aabb::EMPTY.contains_point(Vec3::ZERO));
    }

    #[test]
    fn expand_to_point_grows_accumulator() {
        let mut acc = Aabb::EMPTY;
        acc.expand_to_point(Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(acc.min, Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(acc.max, Vec3::new(1.0, 2.0, 3.0));
        acc.expand_to_point(Vec3::new(-1.0, 5.0, 0.0));
        assert_eq!(acc.min, Vec3::new(-1.0, 2.0, 0.0));
        assert_eq!(acc.max, Vec3::new(1.0, 5.0, 3.0));
    }

    #[test]
    fn union_with_empty_is_other() {
        let a = Aabb::from_half_extents(1.0, 1.0, 1.0);
        assert_eq!(Aabb::EMPTY.union(&a), a);
        assert_eq!(a.union(&Aabb::EMPTY), a);
    }

    #[test]
    fn union_takes_outer_envelope() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        let b = Aabb::from_min_max(Vec3::new(-1.0, 0.5, -2.0), Vec3::new(0.5, 2.0, 0.5));
        let u = a.union(&b);
        assert_eq!(u.min, Vec3::new(-1.0, 0.0, -2.0));
        assert_eq!(u.max, Vec3::new(1.0, 2.0, 1.0));
    }

    #[test]
    fn intersection_disjoint_returns_empty() {
        let a = Aabb::from_min_max(Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.0, 1.0));
        let b = Aabb::from_min_max(Vec3::new(2.0, 2.0, 2.0), Vec3::new(3.0, 3.0, 3.0));
        assert!(a.intersection(&b).is_empty());
    }

    #[test]
    fn intersects_touching_counts_as_overlap() {
        let a = Aabb::from_min_max(Vec3::ZERO, Vec3::ONE);
        let b = Aabb::from_min_max(Vec3::ONE, Vec3::splat(2.0));
        assert!(a.intersects(&b));
    }

    #[test]
    fn intersects_disjoint() {
        let a = Aabb::from_min_max(Vec3::ZERO, Vec3::ONE);
        let b = Aabb::from_min_max(Vec3::splat(1.001), Vec3::splat(2.0));
        assert!(!a.intersects(&b));
    }

    #[test]
    fn intersects_with_empty_is_false() {
        let a = Aabb::from_min_max(Vec3::ZERO, Vec3::ONE);
        assert!(!a.intersects(&Aabb::EMPTY));
        assert!(!Aabb::EMPTY.intersects(&a));
    }

    #[test]
    fn translate_shifts_both_bounds() {
        let a = Aabb::from_half_extents(1.0, 1.0, 1.0);
        let t = a.translate(Vec3::new(10.0, 0.0, -5.0));
        assert_eq!(t.min, Vec3::new(9.0, -1.0, -6.0));
        assert_eq!(t.max, Vec3::new(11.0, 1.0, -4.0));
    }

    #[test]
    fn scale_negative_factor_swaps_min_max() {
        let a = Aabb::from_min_max(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0));
        let s = a.scale(Vec3::new(-1.0, 1.0, 1.0));
        assert_eq!(s.min, Vec3::new(-4.0, 2.0, 3.0));
        assert_eq!(s.max, Vec3::new(-1.0, 5.0, 6.0));
    }

    #[test]
    fn rotate_centered_unit_about_y_quarter_turn_unchanged_aabb() {
        let a = Aabb::from_half_extents(1.0, 1.0, 1.0);
        let r = a.rotate(Vec3::Y, PI * 0.5);
        assert!(approx_aabb(r, a));
    }

    #[test]
    fn rotate_offset_box_grows() {
        let a = Aabb::from_min_max(Vec3::new(1.0, -0.5, -0.5), Vec3::new(2.0, 0.5, 0.5));
        let r = a.rotate(Vec3::Y, PI * 0.5);
        assert!((r.extents().x - 1.0).abs() < EPS);
        assert!((r.extents().z - 1.0).abs() < EPS);
        assert!(approx_eq(r.min.x, -0.5));
        assert!(approx_eq(r.max.x, 0.5));
        assert!(approx_eq(r.min.z, -2.0));
        assert!(approx_eq(r.max.z, -1.0));
    }

    #[test]
    fn rotate_zero_axis_falls_back_to_y() {
        let a = Aabb::from_min_max(Vec3::new(1.0, -0.5, -0.5), Vec3::new(2.0, 0.5, 0.5));
        let r_zero = a.rotate(Vec3::ZERO, PI * 0.5);
        let r_y = a.rotate(Vec3::Y, PI * 0.5);
        assert!(approx_aabb(r_zero, r_y));
    }

    #[test]
    fn mirror_x_flips_x_bounds() {
        let a = Aabb::from_min_max(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0));
        let m = a.mirror(0);
        assert_eq!(m.min, Vec3::new(-4.0, 2.0, 3.0));
        assert_eq!(m.max, Vec3::new(-1.0, 5.0, 6.0));
    }

    #[test]
    fn mirror_each_axis_only_touches_that_axis() {
        let a = Aabb::from_min_max(Vec3::new(1.0, 2.0, 3.0), Vec3::new(4.0, 5.0, 6.0));
        for axis in 0..3 {
            let m = a.mirror(axis);
            for i in 0..3 {
                if i == axis {
                    continue;
                }
                let from_min = match i {
                    0 => (a.min.x, m.min.x),
                    1 => (a.min.y, m.min.y),
                    _ => (a.min.z, m.min.z),
                };
                let from_max = match i {
                    0 => (a.max.x, m.max.x),
                    1 => (a.max.y, m.max.y),
                    _ => (a.max.z, m.max.z),
                };
                assert_eq!(from_min.0, from_min.1);
                assert_eq!(from_max.0, from_max.1);
            }
        }
    }

    #[test]
    #[should_panic(expected = "axis_index")]
    fn mirror_panics_on_bad_axis() {
        Aabb::from_half_extents(1.0, 1.0, 1.0).mirror(3);
    }
}
