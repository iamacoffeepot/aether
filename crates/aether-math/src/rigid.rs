//! Blendable rigid maps shared by native and wasm consumers.
//!
//! A sampled map is rigid when its source operation is a rotation plus
//! translation. A weighted sum built with [`Rigid::add_scaled`] is generally
//! affine instead: it remains useful for skinning, but it does not satisfy
//! the orthonormal precondition of [`Rigid::inverse`].

use crate::Vec3;

/// An affine map held as the three images of the basis and the image of the
/// origin.
#[derive(Clone, Copy, Debug)]
pub struct Rigid {
    columns: [Vec3; 3],
    translation: Vec3,
}

impl Rigid {
    /// The identity map.
    pub const IDENTITY: Self = Self { columns: [Vec3::X, Vec3::Y, Vec3::Z], translation: Vec3::ZERO };

    /// The zero map used to begin a weighted accumulation.
    pub const ZERO: Self = Self { columns: [Vec3::ZERO; 3], translation: Vec3::ZERO };

    /// Sample the affine map performed by `send` at the origin and basis.
    #[must_use]
    pub fn sample(send: impl Fn(Vec3) -> Vec3) -> Self {
        let translation = send(Vec3::ZERO);

        Self { columns: [Vec3::X, Vec3::Y, Vec3::Z].map(|axis| send(axis) - translation), translation }
    }

    /// Where this map sends a point.
    #[must_use]
    pub fn point(&self, point: Vec3) -> Vec3 {
        self.direction(point) + self.translation
    }

    /// Where this map sends a direction.
    #[must_use]
    pub fn direction(&self, direction: Vec3) -> Vec3 {
        self.columns[0] * direction.x + self.columns[1] * direction.y + self.columns[2] * direction.z
    }

    /// Invert this map, provided its linear part is orthonormal.
    ///
    /// Maps produced by [`Self::sample`] from a rotation plus translation
    /// satisfy that precondition. A weighted blend produced by
    /// [`Self::add_scaled`] generally does not.
    #[must_use]
    pub fn inverse(&self) -> Self {
        let [x, y, z] = self.columns;
        let columns = [Vec3::new(x.x, y.x, z.x), Vec3::new(x.y, y.y, z.y), Vec3::new(x.z, y.z, z.z)];
        let inverted = Self { columns, translation: Vec3::ZERO };

        Self { columns, translation: -inverted.direction(self.translation) }
    }

    /// Return `self + other * weight` for linear-blend accumulation.
    #[must_use]
    pub fn add_scaled(self, other: &Self, weight: f32) -> Self {
        Self {
            columns: [0, 1, 2].map(|axis| self.columns[axis] + other.columns[axis] * weight),
            translation: self.translation + other.translation * weight,
        }
    }

    /// Return the three rows used to apply this map in homogeneous form.
    ///
    /// Each row carries the linear coefficients followed by that axis of
    /// translation. A point uses a homogeneous weight of one; a direction
    /// uses zero.
    #[must_use]
    pub fn rows(&self) -> [[f32; 4]; 3] {
        let [x, y, z] = self.columns;
        let translation = self.translation;

        [[x.x, y.x, z.x, translation.x], [x.y, y.y, z.y, translation.y], [x.z, y.z, z.z, translation.z]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: Vec3, expected: Vec3) {
        assert!((actual - expected).length() < 1.0e-5, "got {actual:?}, expected {expected:?}");
    }

    fn rotated_and_translated() -> Rigid {
        let translation = Vec3::new(2.0, -1.0, 0.5);
        Rigid::sample(|point| point.rotate_axis_angle(Vec3::Y, crate::PI * 0.5) + translation)
    }

    #[test]
    fn sample_reproduces_a_rotation_and_translation() {
        let map = rotated_and_translated();
        let point = Vec3::new(0.25, 0.75, -2.0);
        let translation = Vec3::new(2.0, -1.0, 0.5);

        assert_close(map.point(Vec3::ZERO), translation);
        assert_close(map.point(point), point.rotate_axis_angle(Vec3::Y, crate::PI * 0.5) + translation);
    }

    #[test]
    fn inverse_round_trips_a_sampled_map() {
        let map = rotated_and_translated();
        let point = Vec3::new(-1.0, 0.5, 3.0);

        assert_close(map.inverse().point(map.point(point)), point);
    }

    #[test]
    fn weighted_accumulation_and_rows_agree_at_a_point() {
        let first = Rigid::sample(|point| point.rotate_axis_angle(Vec3::X, 0.3) + Vec3::new(1.0, 0.0, 0.0));
        let second = Rigid::sample(|point| point.rotate_axis_angle(Vec3::Z, -0.4) + Vec3::new(0.0, 2.0, 0.5));
        let blend = Rigid::ZERO.add_scaled(&first, 0.25).add_scaled(&second, 0.75);
        let point = Vec3::new(0.4, -0.2, 1.3);
        let expected = first.point(point) * 0.25 + second.point(point) * 0.75;

        assert_close(blend.point(point), expected);

        let rows = blend.rows();
        let homogeneous = [point.x, point.y, point.z, 1.0];
        let applied =
            rows.map(|row| row.into_iter().zip(homogeneous).map(|(coefficient, value)| coefficient * value).sum());
        assert_close(Vec3::new(applied[0], applied[1], applied[2]), expected);
    }
}
