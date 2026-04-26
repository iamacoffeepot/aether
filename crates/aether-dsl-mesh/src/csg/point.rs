//! 3D point in 16:16 fixed-point coordinates — the integer grid the
//! BSP CSG core operates on.

use crate::csg::fixed::{FixedError, f32_to_fixed, fixed_to_f32};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Point3 {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl Point3 {
    pub fn from_f32(p: [f32; 3]) -> Result<Self, FixedError> {
        Ok(Point3 {
            x: f32_to_fixed(p[0])?,
            y: f32_to_fixed(p[1])?,
            z: f32_to_fixed(p[2])?,
        })
    }

    pub fn to_f32(self) -> [f32; 3] {
        [
            fixed_to_f32(self.x),
            fixed_to_f32(self.y),
            fixed_to_f32(self.z),
        ]
    }
}
