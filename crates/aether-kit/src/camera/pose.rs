//! Pose and lens to matrices: the pure functions behind the camera actor.
//!
//! The eye sits `distance` from the target along the pose's orientation,
//! `Quat::from_euler_yxz(yaw, pitch, 0)`, and looks back at it. The view is
//! the inverse of that rigid transform, never a look-at with a fixed up
//! vector, so a pitch of exactly `±PI/2` is an ordinary pose.
//!
//! Everything scales with `distance`: the depth planes, an orthographic
//! lens's visible height, and so the frame fit. One pose therefore drives
//! zoom, framing and depth under either lens.

use aether_math::{Aabb, Mat4, PI, Quat, Ray, TAU, Vec2, Vec3};
use aether_render::{ViewProjection, ViewportExtent};

use super::kinds::{Distance, Lens, Pitch, Pose, Yaw};

/// How far the depth planes reach, as a multiple of the pose's distance.
/// A perspective lens puts `near` at `distance / DEPTH_REACH` and `far` at
/// `distance * DEPTH_REACH`: the target sits at their geometric mean, where a
/// perspective depth buffer's precision is centred, and their ratio is
/// `10^4` at every scale, which a 24-bit or float depth buffer resolves. An
/// orthographic lens puts them at `∓distance * DEPTH_REACH`: orthographic
/// depth is linear and may reach behind the eye, so the slab cannot clip
/// anything in frame however close the eye sits.
const DEPTH_REACH: f32 = 100.0;

/// How much further back than a tight fit [`framed`] puts the eye. With it
/// the framed sphere's near side is at least a tenth of its radius from the
/// eye, in front of a perspective lens's near plane.
const FRAME_MARGIN: f32 = 1.1;

/// The viewport extent of `width` by `height` physical pixels, or `None` when
/// either is zero: a minimised window has no aspect to project for.
pub(super) const fn extent_of(width: u32, height: u32) -> Option<ViewportExtent> {
    if width == 0 || height == 0 {
        None
    } else {
        Some(ViewportExtent { width, height })
    }
}

/// Width over height of an extent [`extent_of`] accepted.
fn aspect_of(extent: ViewportExtent) -> f32 {
    extent.width as f32 / extent.height as f32
}

/// Whether every component of `point` is finite.
pub(super) fn is_finite(point: Vec3) -> bool {
    point.to_array().into_iter().all(f32::is_finite)
}

/// The view `pose` and `lens` give a viewport of `extent`.
pub(super) fn view_projection(pose: Pose, lens: Lens, extent: ViewportExtent) -> ViewProjection {
    let orientation = Quat::from_euler_yxz(pose.yaw.get(), pose.pitch.get(), 0.0);
    let distance = pose.distance.get();
    let eye = pose.target + orientation * Vec3::new(0.0, 0.0, distance);

    let aspect = aspect_of(extent);
    let far = distance * DEPTH_REACH;
    let (near, projection) = match lens {
        Lens::Perspective { fov } => {
            let near = distance / DEPTH_REACH;
            (near, Mat4::perspective_rh(fov.get(), aspect, near, far))
        }
        Lens::Orthographic { extent: per_unit_distance } => {
            let half_height = distance * per_unit_distance.get();
            let half_width = half_height * aspect;
            (-far, Mat4::orthographic_rh(-half_width, half_width, -half_height, half_height, -far, far))
        }
    };

    ViewProjection { view: Mat4::from_rigid(orientation, eye).inverse_rigid(), projection, eye, near, far, extent }
}

/// `pose` turned to look at the centre of `bounds` from far enough back that
/// the box's bounding sphere fits the narrower of the viewport's two
/// dimensions with [`FRAME_MARGIN`] to spare. Yaw and pitch are kept.
///
/// `None` for a box with nothing to frame: empty, of zero size, or not
/// finite.
///
/// An orthographic lens whose extent times the narrower aspect passes about
/// 100 frames a sphere that reaches past its depth slab; such a lens shows
/// more than a 179° perspective would.
pub(super) fn framed(pose: Pose, lens: Lens, extent: ViewportExtent, bounds: Aabb) -> Option<Pose> {
    let target = bounds.center();
    let frameable = !bounds.is_empty() && is_finite(target);
    if !frameable {
        return None;
    }

    let aspect = aspect_of(extent);
    // The eye's distance per unit of sphere radius at a tight fit.
    let reach = match lens {
        Lens::Perspective { fov } => {
            let vertical = fov.get() * 0.5;
            let horizontal = (vertical.tan() * aspect).atan();
            vertical.min(horizontal).sin().recip()
        }
        Lens::Orthographic { extent: per_unit_distance } => (per_unit_distance.get() * aspect.min(1.0)).recip(),
    };
    let radius = bounds.extents().length() * 0.5;

    // A zero-size box asks for a zero distance and an unbounded one for an
    // infinite distance; `Distance` refuses both.
    Distance::new(FRAME_MARGIN * radius * reach).ok().map(|distance| Pose { target, distance, ..pose })
}

/// The world-space ray through `pixel` of `view`'s viewport, in physical
/// pixels with the origin at the top-left corner. `None` when the pixel is
/// not finite.
pub(super) fn pixel_ray(view: &ViewProjection, pixel: Vec2) -> Option<Ray> {
    let width = view.extent.width as f32;
    let height = view.extent.height as f32;
    // Pixel rows count down from the top; normalized device `y` counts up.
    let ndc = Vec2::new(2.0 * pixel.x / width - 1.0, 1.0 - 2.0 * pixel.y / height);

    Ray::unproject((view.projection * view.view).inverse()?, ndc)
}

/// A glide in progress: the pose it left, the pose it is going to, and how
/// far along it is.
#[derive(aether_data::Schema, Debug, Clone, Copy, PartialEq)]
pub(super) struct Gliding {
    from: Pose,
    to: Pose,
    over_micros: u64,
    elapsed_micros: u64,
}

impl Gliding {
    /// A glide from `from` to `to` over `over_millis`, which is not zero.
    pub(super) fn start(from: Pose, to: Pose, over_millis: u32) -> Self {
        Self { from, to, over_micros: u64::from(over_millis) * 1000, elapsed_micros: 0 }
    }

    /// Move `delta_micros` further along.
    pub(super) fn advance(&mut self, delta_micros: u32) {
        self.elapsed_micros = self.elapsed_micros.saturating_add(u64::from(delta_micros));
    }

    /// Whether the glide has run its whole time.
    pub(super) const fn finished(&self) -> bool {
        self.elapsed_micros >= self.over_micros
    }

    /// The pose the glide ends at.
    pub(super) const fn destination(&self) -> Pose {
        self.to
    }

    /// The pose reached so far, eased with smoothstep: the target moves in a
    /// straight line, the yaw along the shorter arc, the pitch linearly, and
    /// the distance geometrically, so a zoom reads as a constant speed.
    pub(super) fn pose(&self) -> Pose {
        let linear = (self.elapsed_micros as f32 / self.over_micros as f32).clamp(0.0, 1.0);
        let eased = linear * linear * 2.0_f32.mul_add(-linear, 3.0);

        let turn = shorter_arc(self.from.yaw.get(), self.to.yaw.get());
        let yaw = turn.mul_add(eased, self.from.yaw.get());
        let pitch = (self.to.pitch.get() - self.from.pitch.get()).mul_add(eased, self.from.pitch.get());
        let distance = self.from.distance.get() * (self.to.distance.get() / self.from.distance.get()).powf(eased);

        // Each value lies between two valid ones; one that rounding pushed
        // out of range falls back to the destination's.
        Pose {
            target: self.from.target.lerp(self.to.target, eased),
            yaw: Yaw::new(yaw).unwrap_or(self.to.yaw),
            pitch: Pitch::new(pitch).unwrap_or(self.to.pitch),
            distance: Distance::new(distance).unwrap_or(self.to.distance),
        }
    }
}

/// The signed turn from `from` to `to`, in `[-PI, PI)`: the shorter way
/// round.
fn shorter_arc(from: f32, to: f32) -> f32 {
    (to - from + PI).rem_euclid(TAU) - PI
}

#[cfg(test)]
mod tests {
    use core::f32::consts::{FRAC_PI_2, FRAC_PI_3, SQRT_2};

    use super::super::kinds::{FieldOfView, OrthoExtent};
    use super::*;

    const LANDSCAPE: ViewportExtent = ViewportExtent { width: 1600, height: 900 };
    const PORTRAIT: ViewportExtent = ViewportExtent { width: 900, height: 1600 };

    fn pose(target: Vec3, yaw: f32, pitch: f32, distance: f32) -> Pose {
        Pose {
            target,
            yaw: Yaw::new(yaw).expect("a finite yaw"),
            pitch: Pitch::new(pitch).expect("a pitch in range"),
            distance: Distance::new(distance).expect("a positive distance"),
        }
    }

    fn perspective(fov: f32) -> Lens {
        Lens::Perspective { fov: FieldOfView::new(fov).expect("a field of view in range") }
    }

    fn orthographic(extent: f32) -> Lens {
        Lens::Orthographic { extent: OrthoExtent::new(extent).expect("a positive extent") }
    }

    fn near(a: Vec3, b: Vec3, tolerance: f32) -> bool {
        (a - b).length() < tolerance
    }

    /// `point` through `view`, in normalized device coordinates.
    fn projected(view: &ViewProjection, point: Vec3) -> Vec3 {
        (view.projection * view.view).project_point(point).expect("a point in front of the eye projects")
    }

    /// `point` in view space, where the eye looks down `-Z`.
    fn in_view_space(view: &ViewProjection, point: Vec3) -> Vec3 {
        view.view.project_point(point).expect("a rigid transform keeps w at one")
    }

    /// Looking straight down is an ordinary pose: the eye is directly above
    /// the target, the target lies `distance` down the view axis, and a step
    /// down in the world is a step forward in view space. A view built with a
    /// look-at and a world-up vector is NaN here, because the view direction
    /// and the up vector are parallel.
    #[test]
    fn a_top_down_pose_looks_straight_down() {
        let target = Vec3::new(4.0, 1.0, -2.0);
        let view = view_projection(pose(target, 0.0, -FRAC_PI_2, 10.0), orthographic(0.5), LANDSCAPE);

        assert!(near(view.eye, Vec3::new(4.0, 11.0, -2.0), 1e-4), "eye = {:?}", view.eye);
        assert!(view.view.to_cols_array().into_iter().all(f32::is_finite), "view = {:?}", view.view);
        let seen = in_view_space(&view, target);
        assert!(near(seen, Vec3::new(0.0, 0.0, -10.0), 1e-4), "target in view space = {seen:?}");
        let below = in_view_space(&view, view.eye + Vec3::new(0.0, -1.0, 0.0));
        assert!(near(below, Vec3::new(0.0, 0.0, -1.0), 1e-4), "one unit below the eye = {below:?}");
    }

    /// The frame fit accounts for the lens and for the narrower of the two
    /// viewport dimensions. A fit that used only the vertical half-angle
    /// would push the wide box's ends out of a portrait viewport; one that
    /// ignored the lens would leave the largest corner far from where the
    /// margin puts it.
    #[test]
    fn a_framed_box_fits_both_lenses_at_both_aspects() {
        let wide = Aabb::from_min_max(Vec3::new(-1.0, -0.1, -0.1), Vec3::new(1.0, 0.1, 0.1));
        let tall = Aabb::from_min_max(Vec3::new(-0.1, -1.0, -0.1), Vec3::new(0.1, 1.0, 0.1));
        let start = pose(Vec3::new(50.0, 50.0, 50.0), 0.0, 0.0, 1.0);

        for lens in [perspective(FRAC_PI_3), orthographic(0.5)] {
            for (extent, bounds) in [(LANDSCAPE, tall), (PORTRAIT, wide), (LANDSCAPE, wide), (PORTRAIT, tall)] {
                let framed = framed(start, lens, extent, bounds).expect("a box with volume frames");
                let view = view_projection(framed, lens, extent);

                let largest = bounds
                    .corners()
                    .into_iter()
                    .map(|corner| projected(&view, corner))
                    .map(|ndc| ndc.x.abs().max(ndc.y.abs()))
                    .fold(0.0_f32, f32::max);
                assert!(largest <= 1.0, "{lens:?} at {extent:?}: a corner projects to {largest}, outside the frame");
            }
        }
    }

    /// The largest corner of a panel facing the eye lands where the margin
    /// says, worked out by hand. The panel spans 2 by 2, so its bounding
    /// sphere has radius `sqrt(2)` and its corners sit one unit off the axis.
    /// Perspective, half-angle `h` in the narrower dimension: the eye is
    /// `1.1 * sqrt(2) / sin(h)` back, so a corner projects to
    /// `cos(h) / (1.1 * sqrt(2))` in that dimension. Orthographic: the
    /// narrower half-extent is `1.1 * sqrt(2)`, so it projects to
    /// `1 / (1.1 * sqrt(2))`.
    #[test]
    fn a_framed_panel_s_corner_lands_at_the_margin() {
        let panel = Aabb::from_min_max(Vec3::new(-1.0, -1.0, 0.0), Vec3::new(1.0, 1.0, 0.0));
        let start = pose(Vec3::ZERO, 0.0, 0.0, 1.0);
        let fit = 1.0 / (FRAME_MARGIN * SQRT_2);
        let narrow_half_angle = ((FRAC_PI_3 * 0.5).tan() * 9.0 / 16.0).atan();
        let corner = Vec3::new(1.0, 1.0, 0.0);

        let landscape = perspective(FRAC_PI_3);
        let view = view_projection(framed(start, landscape, LANDSCAPE, panel).expect("frames"), landscape, LANDSCAPE);
        let expected = (FRAC_PI_3 * 0.5).cos() * fit;
        assert!((projected(&view, corner).y - expected).abs() < 1e-4, "landscape perspective y");

        let view = view_projection(framed(start, landscape, PORTRAIT, panel).expect("frames"), landscape, PORTRAIT);
        let expected = narrow_half_angle.cos() * fit;
        assert!((projected(&view, corner).x - expected).abs() < 1e-4, "portrait perspective x");

        let flat = orthographic(0.25);
        let view = view_projection(framed(start, flat, LANDSCAPE, panel).expect("frames"), flat, LANDSCAPE);
        assert!((projected(&view, corner).y - fit).abs() < 1e-4, "landscape orthographic y");

        let view = view_projection(framed(start, flat, PORTRAIT, panel).expect("frames"), flat, PORTRAIT);
        assert!((projected(&view, corner).x - fit).abs() < 1e-4, "portrait orthographic x");
    }

    /// The depth planes follow the distance: a box 2000 units across, framed,
    /// has every corner between them under either lens. Constant planes of
    /// 0.1 and 100 put the whole box past the far plane.
    #[test]
    fn a_framed_large_box_lies_between_the_depth_planes() {
        let bounds = Aabb::from_min_max(Vec3::new(-1000.0, -200.0, -1000.0), Vec3::new(1000.0, 200.0, 1000.0));
        let start = pose(Vec3::ZERO, 0.7, -0.6, 3.0);

        for lens in [perspective(FRAC_PI_3), orthographic(0.5)] {
            let view = view_projection(framed(start, lens, LANDSCAPE, bounds).expect("frames"), lens, LANDSCAPE);

            for corner in bounds.corners() {
                let depth = -in_view_space(&view, corner).z;
                let between = depth > view.near && depth < view.far;
                assert!(between, "{lens:?}: a corner at depth {depth} is outside {}..{}", view.near, view.far);
            }
        }
    }

    /// A box with nothing to frame is refused instead of producing a pose at
    /// a zero or infinite distance.
    #[test]
    fn a_box_with_nothing_to_frame_is_refused() {
        let start = pose(Vec3::ZERO, 0.0, 0.0, 1.0);
        let lens = perspective(FRAC_PI_3);
        let point = Aabb::from_min_max(Vec3::new(1.0, 1.0, 1.0), Vec3::new(1.0, 1.0, 1.0));
        let unbounded = Aabb::from_min_max(Vec3::splat(f32::NEG_INFINITY), Vec3::splat(f32::INFINITY));

        assert_eq!(framed(start, lens, LANDSCAPE, Aabb::EMPTY), None);
        assert_eq!(framed(start, lens, LANDSCAPE, point), None);
        assert_eq!(framed(start, lens, LANDSCAPE, unbounded), None);
    }

    /// The ray through the centre pixel passes through the target, and the
    /// ray through the top-left pixel runs along the frustum's top-left edge:
    /// `(-tan(h) * aspect, tan(h), -1)` for a camera looking down `-Z`. A
    /// flipped pixel `y` sends it along the bottom edge, and a pixel divided
    /// by the full extent without the factor of two lands it inside the
    /// frustum.
    #[test]
    fn pixel_rays_run_through_the_target_and_along_the_frustum_edge() {
        let target = Vec3::new(0.0, 2.0, -3.0);
        let view = view_projection(pose(target, 0.0, 0.0, 20.0), perspective(FRAC_PI_3), LANDSCAPE);

        let centre = pixel_ray(&view, Vec2::new(800.0, 450.0)).expect("the centre pixel unprojects");
        let along = (target - centre.origin).dot(centre.direction);
        assert!(near(centre.at(along), target, 1e-2), "centre ray passes {:?}", centre.at(along));

        let corner = pixel_ray(&view, Vec2::new(0.0, 0.0)).expect("the corner pixel unprojects");
        let half = (FRAC_PI_3 * 0.5).tan();
        let edge = Vec3::new(-half * 16.0 / 9.0, half, -1.0).normalize();
        assert!(near(corner.direction, edge, 1e-3), "corner ray runs {:?}, the edge {edge:?}", corner.direction);
    }

    /// A pixel that is not a number has no ray.
    #[test]
    fn a_pixel_that_is_not_finite_has_no_ray() {
        let view = view_projection(pose(Vec3::ZERO, 0.0, 0.0, 5.0), perspective(FRAC_PI_3), LANDSCAPE);

        assert_eq!(pixel_ray(&view, Vec2::new(f32::NAN, 10.0)), None);
    }

    /// A glide from a yaw just past zero to one just short of a full turn
    /// goes back through zero, the short way: halfway, the eye is where a
    /// yaw of zero puts it. Interpolating the raw angles goes the long way,
    /// through `PI`, on the opposite side of the target.
    #[test]
    fn a_glide_turns_along_the_shorter_arc() {
        let from = pose(Vec3::ZERO, 0.1, 0.0, 4.0);
        let to = pose(Vec3::ZERO, TAU - 0.1, 0.0, 4.0);
        let mut glide = Gliding::start(from, to, 1000);

        glide.advance(500_000);
        let halfway = view_projection(glide.pose(), perspective(FRAC_PI_3), LANDSCAPE);

        assert!(near(halfway.eye, Vec3::new(0.0, 0.0, 4.0), 1e-3), "halfway eye = {:?}", halfway.eye);
        assert!(!glide.finished());
    }

    /// A glide's distance moves geometrically, so halfway between 1 and 100
    /// is 10, and it ends on the destination once its time has run.
    #[test]
    fn a_glide_zooms_geometrically_and_ends_on_its_destination() {
        let from = pose(Vec3::ZERO, 0.0, 0.0, 1.0);
        let to = pose(Vec3::new(8.0, 0.0, 0.0), 0.0, -0.5, 100.0);
        let mut glide = Gliding::start(from, to, 2000);

        glide.advance(1_000_000);
        let halfway = glide.pose();
        assert!((halfway.distance.get() - 10.0).abs() < 1e-3, "halfway distance = {}", halfway.distance.get());
        assert!(near(halfway.target, Vec3::new(4.0, 0.0, 0.0), 1e-4), "halfway target = {:?}", halfway.target);

        glide.advance(1_500_000);
        assert!(glide.finished());
        assert_eq!(glide.pose(), to);
    }
}
