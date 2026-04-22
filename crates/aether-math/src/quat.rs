use core::ops::Mul;

use crate::vec::Vec3;

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Quat {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Quat {
    pub const IDENTITY: Self = Self::new(0.0, 0.0, 0.0, 1.0);

    #[inline]
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    #[inline]
    pub fn from_axis_angle(axis: Vec3, angle_rad: f32) -> Self {
        let half = angle_rad * 0.5;
        let s = libm::sinf(half);
        let c = libm::cosf(half);
        let axis = axis.normalize();
        Self::new(axis.x * s, axis.y * s, axis.z * s, c)
    }

    /// YXZ Euler order: yaw around Y, then pitch around local X, then
    /// roll around local Z. Angles in radians.
    #[inline]
    pub fn from_euler_yxz(yaw: f32, pitch: f32, roll: f32) -> Self {
        let (sy, cy) = (libm::sinf(yaw * 0.5), libm::cosf(yaw * 0.5));
        let (sp, cp) = (libm::sinf(pitch * 0.5), libm::cosf(pitch * 0.5));
        let (sr, cr) = (libm::sinf(roll * 0.5), libm::cosf(roll * 0.5));
        Self::new(
            cr * sp * cy - sr * cp * sy,
            cr * cp * sy + sr * sp * cy,
            cr * sp * sy + sr * cp * cy,
            cr * cp * cy - sr * sp * sy,
        )
    }

    #[inline]
    pub fn length_squared(self) -> f32 {
        self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w
    }

    #[inline]
    pub fn length(self) -> f32 {
        libm::sqrtf(self.length_squared())
    }

    /// Returns `self / length(self)`. Zero-length input returns
    /// `IDENTITY` rather than NaN. Opinionated but avoids propagating
    /// NaN through camera state; callers that need the distinction
    /// must check length themselves.
    #[inline]
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len > 0.0 {
            let inv = 1.0 / len;
            Self::new(self.x * inv, self.y * inv, self.z * inv, self.w * inv)
        } else {
            Self::IDENTITY
        }
    }

    #[inline]
    pub const fn conjugate(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, self.w)
    }

    /// `conjugate() / length_squared()`. For unit quaternions this
    /// equals `conjugate()` exactly — prefer that when you know the
    /// quat is normalized.
    #[inline]
    pub fn inverse(self) -> Self {
        let ls = self.length_squared();
        if ls > 0.0 {
            let inv = 1.0 / ls;
            Self::new(-self.x * inv, -self.y * inv, -self.z * inv, self.w * inv)
        } else {
            Self::IDENTITY
        }
    }

    #[inline]
    pub fn rotate_vec3(self, v: Vec3) -> Vec3 {
        let xyz = Vec3::new(self.x, self.y, self.z);
        let t = xyz.cross(v) * 2.0;
        v + t * self.w + xyz.cross(t)
    }
}

impl Mul for Quat {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        Self::new(
            self.w * rhs.x + self.x * rhs.w + self.y * rhs.z - self.z * rhs.y,
            self.w * rhs.y - self.x * rhs.z + self.y * rhs.w + self.z * rhs.x,
            self.w * rhs.z + self.x * rhs.y - self.y * rhs.x + self.z * rhs.w,
            self.w * rhs.w - self.x * rhs.x - self.y * rhs.y - self.z * rhs.z,
        )
    }
}

impl Mul<Vec3> for Quat {
    type Output = Vec3;
    #[inline]
    fn mul(self, rhs: Vec3) -> Vec3 {
        self.rotate_vec3(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PI;

    const EPS: f32 = 1e-5;

    fn approx_eq_vec3(a: Vec3, b: Vec3) -> bool {
        (a.x - b.x).abs() < EPS && (a.y - b.y).abs() < EPS && (a.z - b.z).abs() < EPS
    }

    #[test]
    fn identity_leaves_vec_unchanged() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(Quat::IDENTITY * v, v);
    }

    #[test]
    fn from_axis_angle_y_rotates_x_to_negz() {
        let q = Quat::from_axis_angle(Vec3::Y, PI * 0.5);
        assert!(approx_eq_vec3(q * Vec3::X, Vec3::new(0.0, 0.0, -1.0)));
    }

    #[test]
    fn from_axis_angle_x_rotates_y_to_z() {
        let q = Quat::from_axis_angle(Vec3::X, PI * 0.5);
        assert!(approx_eq_vec3(q * Vec3::Y, Vec3::Z));
    }

    #[test]
    fn from_axis_angle_z_rotates_x_to_y() {
        let q = Quat::from_axis_angle(Vec3::Z, PI * 0.5);
        assert!(approx_eq_vec3(q * Vec3::X, Vec3::Y));
    }

    #[test]
    fn yaw_only_matches_axis_angle_y() {
        let yaw = 0.7;
        let a = Quat::from_euler_yxz(yaw, 0.0, 0.0);
        let b = Quat::from_axis_angle(Vec3::Y, yaw);
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert!(approx_eq_vec3(a * v, b * v));
    }

    #[test]
    fn pitch_only_matches_axis_angle_x() {
        let pitch = 0.4;
        let a = Quat::from_euler_yxz(0.0, pitch, 0.0);
        let b = Quat::from_axis_angle(Vec3::X, pitch);
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert!(approx_eq_vec3(a * v, b * v));
    }

    #[test]
    fn conjugate_is_inverse_for_unit_quats() {
        let q = Quat::from_axis_angle(Vec3::new(1.0, 2.0, 3.0).normalize(), 0.7);
        let r = q * q.conjugate();
        assert!((r.x).abs() < EPS);
        assert!((r.y).abs() < EPS);
        assert!((r.z).abs() < EPS);
        assert!((r.w - 1.0).abs() < EPS);
    }

    #[test]
    fn normalize_zero_returns_identity() {
        let zero = Quat::new(0.0, 0.0, 0.0, 0.0);
        assert_eq!(zero.normalize(), Quat::IDENTITY);
    }

    #[test]
    fn repr_c_size() {
        assert_eq!(core::mem::size_of::<Quat>(), 16);
    }
}
