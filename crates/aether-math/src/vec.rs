use core::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Sub, SubAssign};

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Vec4 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub w: f32,
}

impl Vec2 {
    pub const ZERO: Self = Self::splat(0.0);
    pub const ONE: Self = Self::splat(1.0);
    pub const X: Self = Self::new(1.0, 0.0);
    pub const Y: Self = Self::new(0.0, 1.0);

    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[inline]
    pub const fn splat(v: f32) -> Self {
        Self { x: v, y: v }
    }

    #[inline]
    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y
    }

    #[inline]
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    #[inline]
    pub fn length(self) -> f32 {
        libm::sqrtf(self.length_squared())
    }

    #[inline]
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len > 0.0 {
            self * (1.0 / len)
        } else {
            Self::ZERO
        }
    }

    #[inline]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        self + (other - self) * t
    }
}

impl Vec3 {
    pub const ZERO: Self = Self::splat(0.0);
    pub const ONE: Self = Self::splat(1.0);
    pub const X: Self = Self::new(1.0, 0.0, 0.0);
    pub const Y: Self = Self::new(0.0, 1.0, 0.0);
    pub const Z: Self = Self::new(0.0, 0.0, 1.0);

    #[inline]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    #[inline]
    pub const fn splat(v: f32) -> Self {
        Self { x: v, y: v, z: v }
    }

    /// Construct a `Vec3` from a `[f32; 3]`. Mirror of [`Self::to_array`].
    /// Useful when bridging to legacy storage (mesh AST, OBJ, etc.).
    #[inline]
    pub const fn from_array(a: [f32; 3]) -> Self {
        Self::new(a[0], a[1], a[2])
    }

    #[inline]
    pub const fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    #[inline]
    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    #[inline]
    pub fn cross(self, other: Self) -> Self {
        Self::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }

    #[inline]
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    #[inline]
    pub fn length(self) -> f32 {
        libm::sqrtf(self.length_squared())
    }

    /// Returns `self / length(self)`. Zero-length input returns
    /// `Self::ZERO` rather than NaN; callers that need to distinguish
    /// must check length themselves.
    #[inline]
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len > 0.0 {
            self * (1.0 / len)
        } else {
            Self::ZERO
        }
    }

    /// Like [`Self::normalize`] but returns `fallback` (instead of
    /// `ZERO`) when input is at or near zero length. Uses a relative
    /// `length_squared < 1e-12` threshold so a vector that's nominally
    /// nonzero but f32-noisy still routes to the fallback rather than
    /// blowing up to a near-infinite normalised vector.
    #[inline]
    pub fn normalize_or(self, fallback: Self) -> Self {
        let len_sq = self.length_squared();
        if len_sq < 1e-12 {
            return fallback;
        }
        self * (1.0 / libm::sqrtf(len_sq))
    }

    /// Rotate `self` around unit `axis` by `angle` radians (Rodrigues'
    /// formula). The axis MUST be normalised — caller's responsibility.
    /// Cheaper than constructing a quaternion for one-shot rotates;
    /// for chained rotations or interpolation prefer `Quat`.
    #[inline]
    pub fn rotate_axis_angle(self, axis: Self, angle: f32) -> Self {
        let c = libm::cosf(angle);
        let s = libm::sinf(angle);
        let dot = axis.dot(self);
        let cross = axis.cross(self);
        Self::new(
            self.x * c + cross.x * s + axis.x * dot * (1.0 - c),
            self.y * c + cross.y * s + axis.y * dot * (1.0 - c),
            self.z * c + cross.z * s + axis.z * dot * (1.0 - c),
        )
    }

    /// `Some(+1.0)` when `self` and `other` point in the same
    /// direction, `Some(-1.0)` when opposite, `None` when skew (or
    /// either is the zero vector).
    ///
    /// Tolerance: parallel iff `|a × b|² ≤ 1e-10 · |a|² · |b|²`. This
    /// is purely relative — no absolute floor — and accepts axes that
    /// match to ~5 decimal digits. Picked to fold rotations whose axes
    /// differ only by f32-rounding noise from a normalisation step
    /// while rejecting visibly-different axes.
    #[inline]
    pub fn parallel_sign(self, other: Self) -> Option<f32> {
        let mag_a_sq = self.length_squared();
        let mag_b_sq = other.length_squared();
        if mag_a_sq < 1e-12 || mag_b_sq < 1e-12 {
            return None;
        }
        let cross = self.cross(other);
        if cross.length_squared() > 1e-10 * mag_a_sq * mag_b_sq {
            return None;
        }
        Some(if self.dot(other) >= 0.0 { 1.0 } else { -1.0 })
    }

    #[inline]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        self + (other - self) * t
    }

    #[inline]
    pub const fn extend(self, w: f32) -> Vec4 {
        Vec4::new(self.x, self.y, self.z, w)
    }
}

impl Vec4 {
    pub const ZERO: Self = Self::splat(0.0);
    pub const ONE: Self = Self::splat(1.0);
    pub const X: Self = Self::new(1.0, 0.0, 0.0, 0.0);
    pub const Y: Self = Self::new(0.0, 1.0, 0.0, 0.0);
    pub const Z: Self = Self::new(0.0, 0.0, 1.0, 0.0);
    pub const W: Self = Self::new(0.0, 0.0, 0.0, 1.0);

    #[inline]
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    #[inline]
    pub const fn splat(v: f32) -> Self {
        Self {
            x: v,
            y: v,
            z: v,
            w: v,
        }
    }

    #[inline]
    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z + self.w * other.w
    }

    #[inline]
    pub fn length_squared(self) -> f32 {
        self.dot(self)
    }

    #[inline]
    pub fn length(self) -> f32 {
        libm::sqrtf(self.length_squared())
    }

    #[inline]
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len > 0.0 {
            self * (1.0 / len)
        } else {
            Self::ZERO
        }
    }

    #[inline]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        self + (other - self) * t
    }

    #[inline]
    pub const fn truncate(self) -> Vec3 {
        Vec3::new(self.x, self.y, self.z)
    }
}

macro_rules! impl_vec_ops {
    ($Vec:ident { $($field:ident),+ }) => {
        impl Add for $Vec {
            type Output = Self;
            #[inline]
            fn add(self, rhs: Self) -> Self { Self { $($field: self.$field + rhs.$field),+ } }
        }
        impl Sub for $Vec {
            type Output = Self;
            #[inline]
            fn sub(self, rhs: Self) -> Self { Self { $($field: self.$field - rhs.$field),+ } }
        }
        impl Neg for $Vec {
            type Output = Self;
            #[inline]
            fn neg(self) -> Self { Self { $($field: -self.$field),+ } }
        }
        impl Mul<f32> for $Vec {
            type Output = Self;
            #[inline]
            fn mul(self, rhs: f32) -> Self { Self { $($field: self.$field * rhs),+ } }
        }
        impl Div<f32> for $Vec {
            type Output = Self;
            #[inline]
            fn div(self, rhs: f32) -> Self { Self { $($field: self.$field / rhs),+ } }
        }
        impl AddAssign for $Vec {
            #[inline]
            fn add_assign(&mut self, rhs: Self) { $(self.$field += rhs.$field;)+ }
        }
        impl SubAssign for $Vec {
            #[inline]
            fn sub_assign(&mut self, rhs: Self) { $(self.$field -= rhs.$field;)+ }
        }
        impl MulAssign<f32> for $Vec {
            #[inline]
            fn mul_assign(&mut self, rhs: f32) { $(self.$field *= rhs;)+ }
        }
        impl DivAssign<f32> for $Vec {
            #[inline]
            fn div_assign(&mut self, rhs: f32) { $(self.$field /= rhs;)+ }
        }
    };
}

impl_vec_ops!(Vec2 { x, y });
impl_vec_ops!(Vec3 { x, y, z });
impl_vec_ops!(Vec4 { x, y, z, w });

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec3_dot_cross() {
        assert_eq!(Vec3::X.dot(Vec3::Y), 0.0);
        assert_eq!(Vec3::X.dot(Vec3::X), 1.0);
        assert_eq!(Vec3::X.cross(Vec3::Y), Vec3::Z);
        assert_eq!(Vec3::Y.cross(Vec3::Z), Vec3::X);
        assert_eq!(Vec3::Z.cross(Vec3::X), Vec3::Y);
    }

    #[test]
    fn vec3_length_normalize() {
        let v = Vec3::new(3.0, 4.0, 0.0);
        assert_eq!(v.length_squared(), 25.0);
        assert_eq!(v.length(), 5.0);
        let n = v.normalize();
        assert!((n.length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn vec3_normalize_zero_returns_zero() {
        assert_eq!(Vec3::ZERO.normalize(), Vec3::ZERO);
    }

    #[test]
    fn vec3_arith() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);
        assert_eq!(a + b, Vec3::new(5.0, 7.0, 9.0));
        assert_eq!(b - a, Vec3::new(3.0, 3.0, 3.0));
        assert_eq!(a * 2.0, Vec3::new(2.0, 4.0, 6.0));
        assert_eq!(b / 2.0, Vec3::new(2.0, 2.5, 3.0));
        assert_eq!(-a, Vec3::new(-1.0, -2.0, -3.0));
    }

    #[test]
    fn vec3_lerp() {
        let a = Vec3::ZERO;
        let b = Vec3::new(10.0, 20.0, 30.0);
        assert_eq!(a.lerp(b, 0.0), a);
        assert_eq!(a.lerp(b, 1.0), b);
        assert_eq!(a.lerp(b, 0.5), Vec3::new(5.0, 10.0, 15.0));
    }

    #[test]
    fn vec_extend_truncate() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(v.extend(4.0), Vec4::new(1.0, 2.0, 3.0, 4.0));
        assert_eq!(Vec4::new(1.0, 2.0, 3.0, 4.0).truncate(), v);
    }

    #[test]
    fn vec3_from_to_array_round_trip() {
        let a = [1.0_f32, 2.0, 3.0];
        assert_eq!(Vec3::from_array(a).to_array(), a);
        assert_eq!(Vec3::from_array(a), Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn vec3_normalize_or_zero_returns_fallback() {
        let fb = Vec3::new(0.0, 1.0, 0.0);
        assert_eq!(Vec3::ZERO.normalize_or(fb), fb);
    }

    #[test]
    fn vec3_normalize_or_near_zero_returns_fallback() {
        let near_zero = Vec3::new(1e-7, 0.0, 0.0);
        let fb = Vec3::new(0.0, 0.0, 1.0);
        // length² = 1e-14, well under the 1e-12 threshold.
        assert_eq!(near_zero.normalize_or(fb), fb);
    }

    #[test]
    fn vec3_normalize_or_unit_input_unchanged() {
        let n = Vec3::new(3.0, 4.0, 0.0).normalize_or(Vec3::Y);
        assert!((n.length() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn vec3_rotate_axis_angle_y_quarter_turn_takes_x_to_negz() {
        let r = Vec3::X.rotate_axis_angle(Vec3::Y, core::f32::consts::FRAC_PI_2);
        assert!(r.x.abs() < 1e-6);
        assert!(r.y.abs() < 1e-6);
        assert!((r.z + 1.0).abs() < 1e-6);
    }

    #[test]
    fn vec3_rotate_axis_angle_zero_angle_is_identity() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        let r = v.rotate_axis_angle(Vec3::Y, 0.0);
        assert!((r - v).length() < 1e-6);
    }

    #[test]
    fn vec3_parallel_sign_same_direction_positive() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = a * 2.5;
        assert_eq!(a.parallel_sign(b), Some(1.0));
    }

    #[test]
    fn vec3_parallel_sign_opposite_direction_negative() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = a * -0.7;
        assert_eq!(a.parallel_sign(b), Some(-1.0));
    }

    #[test]
    fn vec3_parallel_sign_skew_returns_none() {
        assert_eq!(Vec3::X.parallel_sign(Vec3::Y), None);
    }

    #[test]
    fn vec3_parallel_sign_zero_input_returns_none() {
        assert_eq!(Vec3::ZERO.parallel_sign(Vec3::X), None);
        assert_eq!(Vec3::X.parallel_sign(Vec3::ZERO), None);
    }

    #[test]
    fn repr_c_layout() {
        assert_eq!(core::mem::size_of::<Vec2>(), 8);
        assert_eq!(core::mem::size_of::<Vec3>(), 12);
        assert_eq!(core::mem::size_of::<Vec4>(), 16);
        assert_eq!(core::mem::align_of::<Vec4>(), 4);
    }
}
