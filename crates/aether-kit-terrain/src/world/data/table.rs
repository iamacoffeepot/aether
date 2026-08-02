//! The world's positional side tables — the region, smoothing-profile and
//! water-plane rows the per-cell planes reference by 1-based id.

use alloc::string::String;

use super::material::Material;

/// A semantic group of cells with a default ground material. Regions are
/// referenced by 1-based id from the per-cell region plane (`0` = no
/// region); the region table is positional, so a region's id is its
/// index in [`World`](crate::world::World)'s table plus one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub name: String,
    pub default_material: Material,
    /// The material a cliff face wears where this region's ground breaks
    /// past the step ceiling — the skirt color, and the future hook for
    /// generated rock banding. Defaults to [`Material::Stone`].
    pub cliff_material: Material,
}

/// Ceiling on a smoothing profile's iteration count. The contour pass
/// reads `2 × iterations` subcells outward, so this cap keeps every
/// field-driven read inside the mesher's fixed two-cell apron — the
/// invariant the `R = 1` neighbor remesh relies on.
pub const MAX_SMOOTHING_ITERATIONS: u32 = 4;

/// A contour-smoothing profile — the number and the degrees a cell's
/// smoothing plane points at. Referenced by 1-based id from the per-cell
/// smoothing plane (`0` = no override, the material default applies); the
/// table is positional like the region table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmoothingProfile {
    /// Corner-minimization iteration count (`0` = raw blocky contours).
    /// Clamped to [`MAX_SMOOTHING_ITERATIONS`] at registration.
    pub iterations: u32,
    /// Corner angle in degrees the cellular passes flatten down to (`90`
    /// rounds only true right angles; smaller rounds gentler junctions).
    /// Clamped to `[45, 90]` at registration — the windowed rule's
    /// threshold derivation assumes at least 45.
    pub degrees: u32,
}

/// An authored water surface level. Water cells reference a plane by
/// 1-based id from the per-cell water plane (`0` = the datum-0 level); the
/// table is positional like the region table, so a plane's id is its index
/// in [`World`](crate::world::World)'s table plus one. Disconnected water bodies can share a
/// plane (one sea row every coastal cell points at); the level is authored,
/// not derived from the ground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaterPlane {
    /// The surface height in octimeters the referencing water cells lie at,
    /// regardless of their lakebed [`Chunk::height`](crate::world::Chunk::height).
    pub level_octimeters: i32,
}
