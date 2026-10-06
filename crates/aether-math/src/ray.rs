use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

use crate::mat::Mat4;
use crate::vec::{Vec2, Vec3};

/// A half-line: `origin + t * direction` for `t >= 0`. `direction` is unit
/// length when the ray comes from this crate's constructors.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Pod, Zeroable, Serialize, Deserialize, aether_data::Schema, aether_data::StorageLeaf,
)]
pub struct Ray {
    pub origin: Vec3,
    pub direction: Vec3,
}

impl Ray {
    /// The point `distance` along the ray from its origin.
    #[inline]
    #[must_use]
    pub fn at(self, distance: f32) -> Vec3 {
        self.origin + self.direction * distance
    }

    /// The ray through `ndc` (x and y in `[-1, 1]`, y up), from the near
    /// plane toward the far plane, for the inverse of a `projection * view`
    /// built by this crate (clip depth `[0, 1]`).
    ///
    /// Takes the inverse, not the matrix, so a caller that casts many rays
    /// inverts once. The origin is the near point and the direction the unit
    /// vector toward the far point, which is right for a perspective and an
    /// orthographic projection alike. `None` when either point cannot be
    /// projected or the two coincide.
    #[must_use]
    pub fn unproject(inverse_view_projection: Mat4, ndc: Vec2) -> Option<Self> {
        let near = inverse_view_projection.project_point(Vec3::new(ndc.x, ndc.y, 0.0))?;
        let far = inverse_view_projection.project_point(Vec3::new(ndc.x, ndc.y, 1.0))?;
        let span = far - near;

        // A NaN span fails the comparison and is refused with a zero one.
        let separated = span.length_squared() > 0.0;
        if separated {
            Some(Self { origin: near, direction: span.normalize() })
        } else {
            None
        }
    }

    /// Distance along the ray to the plane through `point` with `normal`.
    ///
    /// `None` when the ray is parallel to the plane (the dot of direction and
    /// normal is zero relative to their lengths, the same relative test
    /// [`Vec3::parallel_sign`] makes) or the hit lies behind the origin.
    #[must_use]
    pub fn plane_hit(self, point: Vec3, normal: Vec3) -> Option<f32> {
        let along = self.direction.dot(normal);
        let scale_squared = self.direction.length_squared() * normal.length_squared();
        let parallel = along * along <= 1e-10 * scale_squared;
        if parallel {
            return None;
        }

        let distance = (point - self.origin).dot(normal) / along;
        let ahead = distance >= 0.0;
        if ahead {
            Some(distance)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PI;
    use crate::test_helpers::approx_eq_vec3;

    const EPS: f32 = 1e-4;

    fn inverse_view_projection(view: Mat4, projection: Mat4) -> Mat4 {
        (projection * view).inverse().expect("a view-projection is invertible")
    }

    #[test]
    fn perspective_centre_ray_runs_from_near_plane_to_target() {
        let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        let projection = Mat4::perspective_rh(PI * 0.5, 1.0, 1.0, 10.0);

        let ray = Ray::unproject(inverse_view_projection(view, projection), Vec2::new(0.0, 0.0))
            .expect("the centre of the screen unprojects");

        assert!(approx_eq_vec3(ray.origin, Vec3::new(0.0, 0.0, 4.0), EPS), "origin = {:?}", ray.origin);
        assert!(approx_eq_vec3(ray.direction, Vec3::new(0.0, 0.0, -1.0), EPS), "direction = {:?}", ray.direction);
        assert!(approx_eq_vec3(ray.at(4.0), Vec3::ZERO, EPS), "at(4) = {:?}", ray.at(4.0));
    }

    #[test]
    fn orthographic_rays_are_parallel_and_offset() {
        let view = Mat4::IDENTITY;
        let projection = Mat4::orthographic_rh(-2.0, 2.0, -2.0, 2.0, 0.1, 10.0);
        let inverse = inverse_view_projection(view, projection);

        let left = Ray::unproject(inverse, Vec2::new(-0.5, 0.0)).expect("left unprojects");
        let right = Ray::unproject(inverse, Vec2::new(0.5, 0.0)).expect("right unprojects");

        assert!(approx_eq_vec3(left.direction, Vec3::new(0.0, 0.0, -1.0), EPS), "left = {:?}", left.direction);
        assert!(approx_eq_vec3(right.direction, left.direction, EPS), "right = {:?}", right.direction);
        assert!(approx_eq_vec3(left.origin, Vec3::new(-1.0, 0.0, -0.1), EPS), "left origin = {:?}", left.origin);
        assert!(approx_eq_vec3(right.origin, Vec3::new(1.0, 0.0, -0.1), EPS), "right origin = {:?}", right.origin);
    }

    #[test]
    fn unproject_refuses_a_matrix_that_collapses_depth() {
        let flattened = Mat4::from_scale(Vec3::new(1.0, 1.0, 0.0));

        assert_eq!(Ray::unproject(flattened, Vec2::new(0.0, 0.0)), None);
    }

    #[test]
    fn plane_hit_returns_the_distance_to_a_tilted_plane() {
        let ray = Ray { origin: Vec3::new(0.0, 2.0, 0.0), direction: Vec3::new(0.0, -1.0, 0.0) };
        let normal = Vec3::new(0.0, 1.0, 1.0);

        let distance = ray.plane_hit(Vec3::ZERO, normal).expect("the ray crosses the plane");

        assert!((distance - 2.0).abs() < EPS, "distance = {distance}");
        assert!(approx_eq_vec3(ray.at(distance), Vec3::ZERO, EPS));
    }

    #[test]
    fn plane_hit_distance_does_not_depend_on_the_normal_length() {
        let ray = Ray { origin: Vec3::new(0.0, 2.0, 0.0), direction: Vec3::new(0.0, -1.0, 0.0) };

        let unit = ray.plane_hit(Vec3::ZERO, Vec3::new(0.0, 1.0, 1.0).normalize()).expect("unit normal hits");
        let long = ray.plane_hit(Vec3::ZERO, Vec3::new(0.0, 8.0, 8.0)).expect("long normal hits");

        assert!((unit - long).abs() < EPS, "unit = {unit}, long = {long}");
    }

    #[test]
    fn plane_hit_refuses_a_parallel_ray() {
        let ray = Ray { origin: Vec3::new(0.0, 2.0, 0.0), direction: Vec3::X };

        assert_eq!(ray.plane_hit(Vec3::ZERO, Vec3::Y), None);
    }

    #[test]
    fn plane_hit_refuses_a_plane_behind_the_origin() {
        let ray = Ray { origin: Vec3::new(0.0, 2.0, 0.0), direction: Vec3::Y };

        assert_eq!(ray.plane_hit(Vec3::ZERO, Vec3::Y), None);
    }
}
