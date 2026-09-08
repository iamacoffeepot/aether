use crate::vec::Vec2;

/// Axis-aligned rectangle in `f32` coordinates — [`Aabb`](crate::Aabb)
/// one dimension down, with the same conventions.
///
/// `min[i] > max[i]` along either axis denotes the empty rectangle (no
/// points). [`Rect2::EMPTY`] uses `+∞` / `-∞` so unioning anything with
/// it returns the other operand unchanged — convenient as an
/// accumulator. A rectangle whose `min` equals its `max` along an axis
/// is degenerate but *not* empty: it still contains the points on that
/// edge, matching [`Aabb`](crate::Aabb).
///
/// The type is unitless — screen pixels, clip space, layout space and
/// UV space all use it. [`Rect2::clamp_to_pixels`] is the one method
/// that assumes a frame: framebuffer pixels with the origin at the
/// target's top-left.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect2 {
    pub min: Vec2,
    pub max: Vec2,
}

impl Rect2 {
    pub const EMPTY: Self = Self { min: Vec2::splat(f32::INFINITY), max: Vec2::splat(f32::NEG_INFINITY) };

    #[inline]
    #[must_use]
    pub const fn from_min_max(min: Vec2, max: Vec2) -> Self {
        Self { min, max }
    }

    /// Origin-plus-size form: the rectangle spanning `(x, y)` to
    /// `(x + width, y + height)`.
    ///
    /// A negative `width` or `height` puts `max` behind `min` and so
    /// yields an empty rectangle rather than a normalized one — a
    /// caller that hands over a backwards size gets nothing, not
    /// something plausible.
    #[inline]
    #[must_use]
    pub const fn from_xywh(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self { min: Vec2::new(x, y), max: Vec2::new(x + width, y + height) }
    }

    /// Smallest rectangle containing every supplied point. Returns
    /// [`Rect2::EMPTY`] if the slice is empty.
    #[must_use]
    pub fn from_points(points: &[Vec2]) -> Self {
        let mut out = Self::EMPTY;
        for p in points {
            out.expand_to_point(*p);
        }
        out
    }

    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.min.x > self.max.x || self.min.y > self.max.y
    }

    /// `true` when all four bounds are finite. [`Rect2::EMPTY`] is not
    /// finite, and neither is a rectangle built from a NaN coordinate.
    #[inline]
    #[must_use]
    pub fn is_finite(&self) -> bool {
        self.min.x.is_finite() && self.min.y.is_finite() && self.max.x.is_finite() && self.max.y.is_finite()
    }

    /// `max - min`. Negative along an axis the rectangle is empty on.
    #[inline]
    #[must_use]
    pub fn size(&self) -> Vec2 {
        self.max - self.min
    }

    #[inline]
    #[must_use]
    pub fn width(&self) -> f32 {
        self.max.x - self.min.x
    }

    #[inline]
    #[must_use]
    pub fn height(&self) -> f32 {
        self.max.y - self.min.y
    }

    #[inline]
    #[must_use]
    pub fn center(&self) -> Vec2 {
        (self.min + self.max) * 0.5
    }

    /// The four corners in a fixed order:
    /// `(min,min) (max,min) (min,max) (max,max)`.
    #[must_use]
    pub fn corners(&self) -> [Vec2; 4] {
        [self.min, Vec2::new(self.max.x, self.min.y), Vec2::new(self.min.x, self.max.y), self.max]
    }

    /// `true` if `p` lies inside or on the boundary. Always `false` for
    /// an empty rectangle.
    #[must_use]
    pub fn contains_point(&self, p: Vec2) -> bool {
        if self.is_empty() {
            return false;
        }
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    /// Grow `self` (in place) to include `p`. No-op if `p` is already
    /// inside; correctly initialises an [`EMPTY`](Self::EMPTY) accumulator.
    pub fn expand_to_point(&mut self, p: Vec2) {
        self.min.x = self.min.x.min(p.x);
        self.min.y = self.min.y.min(p.y);
        self.max.x = self.max.x.max(p.x);
        self.max.y = self.max.y.max(p.y);
    }

    /// Smallest rectangle containing both `self` and `other`. Either
    /// being empty returns the other unchanged.
    #[must_use]
    pub fn union(&self, other: &Self) -> Self {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        Self {
            min: Vec2::new(self.min.x.min(other.min.x), self.min.y.min(other.min.y)),
            max: Vec2::new(self.max.x.max(other.max.x), self.max.y.max(other.max.y)),
        }
    }

    /// Largest rectangle contained in both `self` and `other`. Returns
    /// an empty rectangle when the inputs don't overlap.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        Self {
            min: Vec2::new(self.min.x.max(other.min.x), self.min.y.max(other.min.y)),
            max: Vec2::new(self.max.x.min(other.max.x), self.max.y.min(other.max.y)),
        }
    }

    /// `true` if `self` and `other` share at least one point. Touching
    /// (a single shared edge or corner) counts as intersecting.
    #[must_use]
    pub fn intersects(&self, other: &Self) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        self.min.x <= other.max.x && self.max.x >= other.min.x && self.min.y <= other.max.y && self.max.y >= other.min.y
    }

    #[must_use]
    pub fn translate(&self, offset: Vec2) -> Self {
        if self.is_empty() {
            return *self;
        }
        Self { min: self.min + offset, max: self.max + offset }
    }

    /// Pull every edge inward by `amount` (per axis). A negative
    /// component grows the rectangle instead. Insetting past the centre
    /// crosses the bounds and so yields an empty rectangle, which is
    /// what a padded box smaller than its padding should be.
    #[must_use]
    pub fn inset(&self, amount: Vec2) -> Self {
        if self.is_empty() {
            return *self;
        }
        Self { min: self.min + amount, max: self.max - amount }
    }

    /// The parts of `self` that `other` does not cover, as up to four
    /// disjoint rectangles: the band above the cut, the band below it,
    /// and the left and right bands between them. A part with no area
    /// comes back as [`EMPTY`](Self::EMPTY), so callers filter on
    /// [`is_empty`](Self::is_empty) rather than reading positions.
    ///
    /// This is how an occluding rectangle is removed from a clip region
    /// without a draw-order layer: subtract the occluder and keep
    /// drawing into what is left.
    #[must_use]
    pub fn subtract(&self, other: &Self) -> [Self; 4] {
        let cut = self.intersection(other);
        if cut.is_empty() {
            return [*self, Self::EMPTY, Self::EMPTY, Self::EMPTY];
        }
        [
            Self::band(self.min, Vec2::new(self.max.x, cut.min.y)),
            Self::band(Vec2::new(self.min.x, cut.max.y), self.max),
            Self::band(Vec2::new(self.min.x, cut.min.y), Vec2::new(cut.min.x, cut.max.y)),
            Self::band(Vec2::new(cut.max.x, cut.min.y), Vec2::new(self.max.x, cut.max.y)),
        ]
    }

    /// One [`subtract`](Self::subtract) part, collapsing a zero-area
    /// sliver to [`EMPTY`](Self::EMPTY) so the caller's filter is a
    /// single predicate.
    #[inline]
    fn band(min: Vec2, max: Vec2) -> Self {
        if min.x >= max.x || min.y >= max.y {
            Self::EMPTY
        } else {
            Self { min, max }
        }
    }

    /// The framebuffer-pixel `[x, y, width, height]` a GPU scissor rect
    /// wants, or `None` when the rectangle covers no pixel of a
    /// `target_width` × `target_height` target.
    ///
    /// The rectangle is read in framebuffer pixels — origin at the
    /// target's top-left, `+Y` down. Bounds are clamped to the target
    /// and snapped outward (`floor` on the minimum, `ceil` on the
    /// maximum) so a sub-pixel rectangle still covers every pixel it
    /// touches. A non-finite bound returns `None`: a NaN clip is a
    /// sender bug, and dropping the draw is the fail-fast answer rather
    /// than handing the GPU an unvalidatable scissor.
    ///
    /// The arithmetic runs in `f64` so the target dimensions convert
    /// exactly; the same computation in `f32` would round a target
    /// wider than `2^24` pixels.
    #[must_use]
    pub fn clamp_to_pixels(&self, target_width: u32, target_height: u32) -> Option<[u32; 4]> {
        if !self.is_finite() {
            return None;
        }
        let (limit_x, limit_y) = (f64::from(target_width), f64::from(target_height));
        let min_x = libm::floor(f64::from(self.min.x).max(0.0).min(limit_x));
        let min_y = libm::floor(f64::from(self.min.y).max(0.0).min(limit_y));
        let max_x = libm::ceil(f64::from(self.max.x).max(0.0).min(limit_x));
        let max_y = libm::ceil(f64::from(self.max.y).max(0.0).min(limit_y));
        if max_x <= min_x || max_y <= min_y {
            return None;
        }

        Some([pixel(min_x), pixel(min_y), pixel(max_x - min_x), pixel(max_y - min_y)])
    }
}

/// One snapped bound as its pixel index.
///
/// [`Rect2::clamp_to_pixels`] has already floored or ceiled `value` and
/// clamped it into `0..=target`, so the conversion is exact. `core` has
/// no fallible float-to-integer conversion to state that with — a cast
/// is the only spelling — hence the suppression on this one line rather
/// than over the arithmetic that computed the bound.
#[inline]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // aether-suppression-request: clamped integral value
fn pixel(value: f64) -> u32 {
    value as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect2 {
        Rect2::from_min_max(Vec2::new(x0, y0), Vec2::new(x1, y1))
    }

    #[test]
    fn empty_is_empty() {
        assert!(Rect2::EMPTY.is_empty());
        assert!(!rect(0.0, 0.0, 1.0, 1.0).is_empty());
        assert!(!rect(1.0, 1.0, 1.0, 1.0).is_empty(), "a degenerate rect still holds its own edge");
    }

    #[test]
    fn from_xywh_negative_size_is_empty() {
        assert!(Rect2::from_xywh(4.0, 4.0, -1.0, 2.0).is_empty());
        assert!(Rect2::from_xywh(4.0, 4.0, 2.0, -1.0).is_empty());
        assert_eq!(Rect2::from_xywh(1.0, 2.0, 3.0, 4.0), rect(1.0, 2.0, 4.0, 6.0));
    }

    #[test]
    fn from_points_collects_extremes() {
        let pts = [Vec2::new(1.0, 0.0), Vec2::new(-1.0, 2.0), Vec2::new(0.5, -1.0)];
        assert_eq!(Rect2::from_points(&pts), rect(-1.0, -1.0, 1.0, 2.0));
        assert!(Rect2::from_points(&[]).is_empty());
    }

    #[test]
    fn contains_point_inside_outside_boundary() {
        let r = rect(-1.0, -1.0, 1.0, 1.0);
        assert!(r.contains_point(Vec2::ZERO));
        assert!(r.contains_point(Vec2::new(1.0, 1.0)));
        assert!(!r.contains_point(Vec2::new(1.0001, 0.0)));
        assert!(!Rect2::EMPTY.contains_point(Vec2::ZERO));
    }

    #[test]
    fn union_and_intersection_envelopes() {
        let a = rect(0.0, 0.0, 1.0, 1.0);
        let b = rect(-1.0, 0.5, 0.5, 2.0);
        assert_eq!(a.union(&b), rect(-1.0, 0.0, 1.0, 2.0));
        assert_eq!(Rect2::EMPTY.union(&a), a);
        assert_eq!(a.intersection(&b), rect(0.0, 0.5, 0.5, 1.0));
        assert!(a.intersection(&rect(2.0, 2.0, 3.0, 3.0)).is_empty());
    }

    #[test]
    fn intersects_touching_counts_as_overlap() {
        let a = rect(0.0, 0.0, 1.0, 1.0);
        assert!(a.intersects(&rect(1.0, 1.0, 2.0, 2.0)));
        assert!(!a.intersects(&rect(1.001, 0.0, 2.0, 1.0)));
        assert!(!a.intersects(&Rect2::EMPTY));
    }

    #[test]
    fn inset_past_the_centre_is_empty() {
        assert_eq!(rect(0.0, 0.0, 10.0, 10.0).inset(Vec2::splat(2.0)), rect(2.0, 2.0, 8.0, 8.0));
        assert!(rect(0.0, 0.0, 4.0, 10.0).inset(Vec2::splat(3.0)).is_empty());
        assert_eq!(rect(0.0, 0.0, 1.0, 1.0).inset(Vec2::splat(-1.0)), rect(-1.0, -1.0, 2.0, 2.0));
    }

    #[test]
    fn subtract_disjoint_returns_self_only() {
        let a = rect(0.0, 0.0, 10.0, 10.0);
        let parts = a.subtract(&rect(20.0, 20.0, 30.0, 30.0));
        assert_eq!(parts[0], a);
        assert!(parts[1..].iter().all(Rect2::is_empty));
    }

    #[test]
    fn subtract_interior_hole_leaves_four_bands() {
        let parts = rect(0.0, 0.0, 10.0, 10.0).subtract(&rect(4.0, 4.0, 6.0, 6.0));
        assert_eq!(parts[0], rect(0.0, 0.0, 10.0, 4.0));
        assert_eq!(parts[1], rect(0.0, 6.0, 10.0, 10.0));
        assert_eq!(parts[2], rect(0.0, 4.0, 4.0, 6.0));
        assert_eq!(parts[3], rect(6.0, 4.0, 10.0, 6.0));
    }

    #[test]
    fn subtract_edge_cut_drops_the_zero_area_bands() {
        // The occluder covers the whole left half, so only the right
        // band has area; the top, bottom, and left parts are slivers.
        let parts = rect(0.0, 0.0, 10.0, 10.0).subtract(&rect(-5.0, -5.0, 5.0, 15.0));
        assert!(parts[0].is_empty());
        assert!(parts[1].is_empty());
        assert!(parts[2].is_empty());
        assert_eq!(parts[3], rect(5.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn subtract_covering_occluder_leaves_nothing() {
        let parts = rect(0.0, 0.0, 10.0, 10.0).subtract(&rect(-1.0, -1.0, 11.0, 11.0));
        assert!(parts.iter().all(Rect2::is_empty));
    }

    // Tripwire: this predicate decides whether a batch reaches the GPU
    // at all, so the snapping and the rejections are pinned. Outward
    // snapping must keep a sub-pixel rect alive; a clip entirely off the
    // target, a zero-width clip, and a non-finite clip must all drop.
    #[test]
    fn clamp_to_pixels_snaps_outward_and_rejects_non_drawing_rects() {
        assert_eq!(Rect2::from_xywh(0.0, 0.0, 64.0, 48.0).clamp_to_pixels(64, 48), Some([0, 0, 64, 48]));
        assert_eq!(Rect2::from_xywh(-1.0, -1.0, 2.0, 2.0).clamp_to_pixels(64, 48), Some([0, 0, 1, 1]));
        assert_eq!(Rect2::from_xywh(63.5, 47.5, 1.0, 1.0).clamp_to_pixels(64, 48), Some([63, 47, 1, 1]));
        assert_eq!(Rect2::from_xywh(2.25, 3.75, 1.0, 1.0).clamp_to_pixels(64, 48), Some([2, 3, 2, 2]));

        assert_eq!(Rect2::from_xywh(64.0, 0.0, 1.0, 1.0).clamp_to_pixels(64, 48), None);
        assert_eq!(Rect2::from_xywh(0.0, 0.0, 0.0, 1.0).clamp_to_pixels(64, 48), None);
        assert_eq!(Rect2::from_xywh(0.0, 0.0, 1.0, -1.0).clamp_to_pixels(64, 48), None);
        assert_eq!(Rect2::from_xywh(f32::NAN, 0.0, 1.0, 1.0).clamp_to_pixels(64, 48), None);
        assert_eq!(Rect2::EMPTY.clamp_to_pixels(64, 48), None);
    }
}
