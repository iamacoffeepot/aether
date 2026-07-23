// Material discriminants are bounded table indices, so the pedantic cast
// lints do not signal a real truncation or sign hazard here.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
//! The material style table and the flat color each material renders.
//!
//! A [`MaterialStyle`] row carries a material's base color in HSL.
//! [`StyleTable`] holds one row per [`Material`] — [`StyleTable::get`] reads
//! a row — and [`flat_color`] converts a row straight to linear RGB. A
//! cell's color is a pure function of its material alone, so two chunks
//! agree on their shared border with no shared state.

use aether_math::{Hsl, Rgb};

use crate::world::Material;

/// One material's render style — its base color in HSL. Indexed by
/// [`Material`] through [`StyleTable::get`]; the [`Material::Void`] row is a
/// placeholder that is never painted.
pub struct MaterialStyle {
    /// Base color: hue in degrees and saturation/lightness in `[0, 1]`.
    pub base: Hsl,
}

/// Per-material style rows. Base colors are the HSL of the ground palette's
/// sRGB design values (Grass `(0.30, 0.55, 0.25)`, Dirt
/// `(0.45, 0.32, 0.18)`, Stone `(0.55, 0.55, 0.58)`, Sand
/// `(0.85, 0.78, 0.55)`, Water `(0.20, 0.40, 0.70)`).
const STYLES: [MaterialStyle; 6] = [
    // Void — never painted.
    MaterialStyle { base: Hsl::new(0.0, 0.0, 0.0) },
    // Grass — hsl(110, 37.5, 40).
    MaterialStyle { base: Hsl::new(110.0, 0.375, 0.4) },
    // Dirt — hsl(31, 42.9, 31.5).
    MaterialStyle {
        // Preserve the exact `f32` produced by the former percentage
        // conversion so this type migration cannot move rendered colors.
        base: Hsl::new(31.0, 42.9 / 100.0, 0.315),
    },
    // Stone — hsl(240, 3.45, 56.5).
    MaterialStyle { base: Hsl::new(240.0, 0.0345, 0.565) },
    // Sand — hsl(46, 50, 70).
    MaterialStyle { base: Hsl::new(46.0, 0.5, 0.7) },
    // Water — hsl(216, 55.6, 45).
    MaterialStyle { base: Hsl::new(216.0, 0.556, 0.45) },
];

/// Material style rows. `Default` seeds every row from the built-in
/// defaults; a `WorldView` actor holds one instance as its color source.
pub struct StyleTable([MaterialStyle; 6]);

impl Default for StyleTable {
    fn default() -> Self {
        Self(STYLES)
    }
}

impl StyleTable {
    /// The style row for `material`.
    #[must_use]
    pub fn get(&self, material: Material) -> &MaterialStyle {
        &self.0[material as usize]
    }
}

/// The flat linear-RGB color for a material style row.
#[must_use]
pub fn flat_color(style: &MaterialStyle) -> Rgb {
    style.base.to_rgb()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_material_resolves_a_distinct_flat_color() {
        // The base render keys each material to one flat color; if two rows
        // collapsed to the same linear RGB (a duplicated or missing table
        // row, or an HSL conversion that flattened them), two materials would
        // render indistinguishably.
        let styles = StyleTable::default();
        let colors: Vec<Rgb> = [Material::Grass, Material::Dirt, Material::Stone, Material::Sand, Material::Water]
            .into_iter()
            .map(|m| flat_color(styles.get(m)))
            .collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "materials {i} and {j} share a color");
            }
        }
    }
}
