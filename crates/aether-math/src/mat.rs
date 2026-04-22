use core::ops::Mul;

use crate::quat::Quat;
use crate::vec::{Vec3, Vec4};

/// Column-major 4×4 matrix. Internal storage is four column `Vec4`s
/// so `Mat4` lays out in memory identically to a GLSL/HLSL `mat4`
/// uniform — copy directly into a wgpu uniform buffer with no
/// transpose. `M * v` applies `M` to `v` in standard left-multiply
/// convention.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Mat4 {
    pub cols: [Vec4; 4],
}

impl Mat4 {
    pub const IDENTITY: Self = Self {
        cols: [
            Vec4::new(1.0, 0.0, 0.0, 0.0),
            Vec4::new(0.0, 1.0, 0.0, 0.0),
            Vec4::new(0.0, 0.0, 1.0, 0.0),
            Vec4::new(0.0, 0.0, 0.0, 1.0),
        ],
    };

    #[inline]
    pub const fn from_cols(c0: Vec4, c1: Vec4, c2: Vec4, c3: Vec4) -> Self {
        Self {
            cols: [c0, c1, c2, c3],
        }
    }

    #[inline]
    pub const fn from_translation(t: Vec3) -> Self {
        Self {
            cols: [
                Vec4::new(1.0, 0.0, 0.0, 0.0),
                Vec4::new(0.0, 1.0, 0.0, 0.0),
                Vec4::new(0.0, 0.0, 1.0, 0.0),
                Vec4::new(t.x, t.y, t.z, 1.0),
            ],
        }
    }

    #[inline]
    pub const fn from_scale(s: Vec3) -> Self {
        Self {
            cols: [
                Vec4::new(s.x, 0.0, 0.0, 0.0),
                Vec4::new(0.0, s.y, 0.0, 0.0),
                Vec4::new(0.0, 0.0, s.z, 0.0),
                Vec4::new(0.0, 0.0, 0.0, 1.0),
            ],
        }
    }

    #[inline]
    pub fn from_rotation_quat(q: Quat) -> Self {
        let (x, y, z, w) = (q.x, q.y, q.z, q.w);
        let (xx, yy, zz) = (x * x, y * y, z * z);
        let (xy, xz, yz) = (x * y, x * z, y * z);
        let (wx, wy, wz) = (w * x, w * y, w * z);
        Self {
            cols: [
                Vec4::new(1.0 - 2.0 * (yy + zz), 2.0 * (xy + wz), 2.0 * (xz - wy), 0.0),
                Vec4::new(2.0 * (xy - wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz + wx), 0.0),
                Vec4::new(2.0 * (xz + wy), 2.0 * (yz - wx), 1.0 - 2.0 * (xx + yy), 0.0),
                Vec4::new(0.0, 0.0, 0.0, 1.0),
            ],
        }
    }

    /// Rigid transform: rotation then translation. Equivalent to
    /// `from_translation(t) * from_rotation_quat(r)` but composed
    /// in one step.
    #[inline]
    pub fn from_rigid(rotation: Quat, translation: Vec3) -> Self {
        let mut m = Self::from_rotation_quat(rotation);
        m.cols[3] = Vec4::new(translation.x, translation.y, translation.z, 1.0);
        m
    }

    #[inline]
    pub fn transpose(self) -> Self {
        let [c0, c1, c2, c3] = self.cols;
        Self {
            cols: [
                Vec4::new(c0.x, c1.x, c2.x, c3.x),
                Vec4::new(c0.y, c1.y, c2.y, c3.y),
                Vec4::new(c0.z, c1.z, c2.z, c3.z),
                Vec4::new(c0.w, c1.w, c2.w, c3.w),
            ],
        }
    }

    /// Inverse of a rigid transform (rotation + translation only, no
    /// scale or skew): transpose the 3×3 rotation block and apply to
    /// the negated translation. Produces garbage on a non-rigid
    /// matrix; use only on view matrices and similar.
    #[inline]
    pub fn inverse_rigid(self) -> Self {
        let [r0, r1, r2, t] = self.cols;
        let t_xyz = Vec3::new(t.x, t.y, t.z);
        let row0 = Vec3::new(r0.x, r0.y, r0.z);
        let row1 = Vec3::new(r1.x, r1.y, r1.z);
        let row2 = Vec3::new(r2.x, r2.y, r2.z);
        Self {
            cols: [
                Vec4::new(r0.x, r1.x, r2.x, 0.0),
                Vec4::new(r0.y, r1.y, r2.y, 0.0),
                Vec4::new(r0.z, r1.z, r2.z, 0.0),
                Vec4::new(-row0.dot(t_xyz), -row1.dot(t_xyz), -row2.dot(t_xyz), 1.0),
            ],
        }
    }

    /// Right-handed look-at view matrix. Camera at `eye`, looking
    /// toward `target`, with `up` the world-up direction. Produces a
    /// matrix that transforms world-space points into view space
    /// where the camera sits at the origin looking down `-Z`.
    #[inline]
    pub fn look_at_rh(eye: Vec3, target: Vec3, up: Vec3) -> Self {
        let z = (eye - target).normalize();
        let x = up.cross(z).normalize();
        let y = z.cross(x);
        Self {
            cols: [
                Vec4::new(x.x, y.x, z.x, 0.0),
                Vec4::new(x.y, y.y, z.y, 0.0),
                Vec4::new(x.z, y.z, z.z, 0.0),
                Vec4::new(-x.dot(eye), -y.dot(eye), -z.dot(eye), 1.0),
            ],
        }
    }

    /// Right-handed perspective projection, wgpu-style clip space
    /// (depth in `[0, 1]`, not OpenGL's `[-1, 1]`). `fov_y_rad` is
    /// the vertical field of view in radians.
    #[inline]
    pub fn perspective_rh(fov_y_rad: f32, aspect: f32, z_near: f32, z_far: f32) -> Self {
        let f = 1.0 / libm::tanf(fov_y_rad * 0.5);
        let a = z_far / (z_near - z_far);
        let b = z_near * z_far / (z_near - z_far);
        Self {
            cols: [
                Vec4::new(f / aspect, 0.0, 0.0, 0.0),
                Vec4::new(0.0, f, 0.0, 0.0),
                Vec4::new(0.0, 0.0, a, -1.0),
                Vec4::new(0.0, 0.0, b, 0.0),
            ],
        }
    }

    /// Right-handed orthographic projection, wgpu-style clip space
    /// (depth in `[0, 1]`).
    #[inline]
    pub fn orthographic_rh(
        left: f32,
        right: f32,
        bottom: f32,
        top: f32,
        z_near: f32,
        z_far: f32,
    ) -> Self {
        let rl = right - left;
        let tb = top - bottom;
        let nf = z_near - z_far;
        Self {
            cols: [
                Vec4::new(2.0 / rl, 0.0, 0.0, 0.0),
                Vec4::new(0.0, 2.0 / tb, 0.0, 0.0),
                Vec4::new(0.0, 0.0, 1.0 / nf, 0.0),
                Vec4::new(-(right + left) / rl, -(top + bottom) / tb, z_near / nf, 1.0),
            ],
        }
    }
}

impl Mul<Vec4> for Mat4 {
    type Output = Vec4;
    #[inline]
    fn mul(self, v: Vec4) -> Vec4 {
        self.cols[0] * v.x + self.cols[1] * v.y + self.cols[2] * v.z + self.cols[3] * v.w
    }
}

impl Mul for Mat4 {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        Self {
            cols: [
                self * rhs.cols[0],
                self * rhs.cols[1],
                self * rhs.cols[2],
                self * rhs.cols[3],
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PI;

    const EPS: f32 = 1e-5;

    fn approx_eq_vec4(a: Vec4, b: Vec4) -> bool {
        (a.x - b.x).abs() < EPS
            && (a.y - b.y).abs() < EPS
            && (a.z - b.z).abs() < EPS
            && (a.w - b.w).abs() < EPS
    }

    #[test]
    fn identity_preserves_vec() {
        let v = Vec4::new(1.0, 2.0, 3.0, 1.0);
        assert_eq!(Mat4::IDENTITY * v, v);
    }

    #[test]
    fn translation_moves_point() {
        let m = Mat4::from_translation(Vec3::new(2.0, 3.0, 4.0));
        let p = Vec4::new(1.0, 0.0, 0.0, 1.0);
        assert_eq!(m * p, Vec4::new(3.0, 3.0, 4.0, 1.0));
    }

    #[test]
    fn translation_does_not_move_direction() {
        let m = Mat4::from_translation(Vec3::new(2.0, 3.0, 4.0));
        let dir = Vec4::new(1.0, 0.0, 0.0, 0.0);
        assert_eq!(m * dir, dir);
    }

    #[test]
    fn scale_scales_point() {
        let m = Mat4::from_scale(Vec3::new(2.0, 3.0, 4.0));
        let p = Vec4::new(1.0, 1.0, 1.0, 1.0);
        assert_eq!(m * p, Vec4::new(2.0, 3.0, 4.0, 1.0));
    }

    #[test]
    fn rotation_quat_matches_quat_mul_vec3() {
        let q = Quat::from_axis_angle(Vec3::Y, PI * 0.5);
        let m = Mat4::from_rotation_quat(q);
        let v = Vec3::new(1.0, 2.0, 3.0);
        let via_quat = q * v;
        let via_mat = m * v.extend(0.0);
        assert!(approx_eq_vec4(via_mat, via_quat.extend(0.0)));
    }

    #[test]
    fn mul_is_associative_enough() {
        let a = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0));
        let b = Mat4::from_rotation_quat(Quat::from_axis_angle(Vec3::Y, 0.7));
        let v = Vec4::new(1.0, 0.0, 0.0, 1.0);
        let left = (a * b) * v;
        let right = a * (b * v);
        assert!(approx_eq_vec4(left, right));
    }

    #[test]
    fn inverse_rigid_round_trips() {
        let r = Quat::from_axis_angle(Vec3::new(1.0, 2.0, 3.0).normalize(), 0.7);
        let m = Mat4::from_rigid(r, Vec3::new(4.0, -2.0, 5.0));
        let inv = m.inverse_rigid();
        let v = Vec4::new(1.0, 2.0, 3.0, 1.0);
        let round = inv * (m * v);
        assert!(approx_eq_vec4(round, v));
    }

    #[test]
    fn look_at_places_camera() {
        let view = Mat4::look_at_rh(Vec3::new(0.0, 0.0, 5.0), Vec3::ZERO, Vec3::Y);
        let origin_in_view = view * Vec4::new(0.0, 0.0, 0.0, 1.0);
        assert!(approx_eq_vec4(
            origin_in_view,
            Vec4::new(0.0, 0.0, -5.0, 1.0)
        ));
    }

    #[test]
    fn perspective_maps_near_to_zero_far_to_one() {
        let proj = Mat4::perspective_rh(PI * 0.5, 1.0, 1.0, 10.0);
        let near = proj * Vec4::new(0.0, 0.0, -1.0, 1.0);
        let far = proj * Vec4::new(0.0, 0.0, -10.0, 1.0);
        assert!(
            (near.z / near.w - 0.0).abs() < EPS,
            "near z_ndc = {}",
            near.z / near.w
        );
        assert!(
            (far.z / far.w - 1.0).abs() < EPS,
            "far z_ndc = {}",
            far.z / far.w
        );
    }

    #[test]
    fn orthographic_maps_box() {
        let proj = Mat4::orthographic_rh(-1.0, 1.0, -1.0, 1.0, 1.0, 10.0);
        let near = proj * Vec4::new(0.0, 0.0, -1.0, 1.0);
        let far = proj * Vec4::new(0.0, 0.0, -10.0, 1.0);
        assert!((near.z - 0.0).abs() < EPS);
        assert!((far.z - 1.0).abs() < EPS);
        let corner = proj * Vec4::new(1.0, 1.0, -5.0, 1.0);
        assert!((corner.x - 1.0).abs() < EPS);
        assert!((corner.y - 1.0).abs() < EPS);
    }

    #[test]
    fn transpose_round_trips() {
        let m = Mat4::from_rigid(
            Quat::from_axis_angle(Vec3::X, 0.3),
            Vec3::new(7.0, -3.0, 2.0),
        );
        assert_eq!(m.transpose().transpose(), m);
    }

    #[test]
    fn repr_c_size() {
        assert_eq!(core::mem::size_of::<Mat4>(), 64);
    }
}
