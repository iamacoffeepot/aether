// The style layer works in continuous f32 color space; the coordinate and
// index casts (percent values to unit range) are small and the pedantic
// precision / sign / truncation lints flag them as non-issues in this
// bounded domain.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
// Color math reads clearest with the conventional single-letter channel
// names (h/s/l, r/g/b).
#![allow(clippy::many_single_char_names)]
// The color arithmetic is written as explicit multiply-add chains for
// readability; a fused mul_add would need a libm symbol on the wasm target
// and does not change the result meaningfully here.
#![allow(clippy::suboptimal_flops)]

//! The material style table and the flat color each material renders.
//!
//! A [`MaterialStyle`] row carries a material's base color in HSL.
//! [`StyleTable`] holds one row per [`Material`] — [`StyleTable::get`] reads
//! a row — and [`flat_color`] converts a row straight to linear RGB. A
//! cell's color is a pure function of its material alone, so two chunks
//! agree on their shared border with no shared state.

use crate::world::Material;

/// One material's render style — its base color in HSL. Indexed by
/// [`Material`] through [`StyleTable::get`]; the [`Material::Void`] row is a
/// placeholder that is never painted.
pub struct MaterialStyle {
    /// Base hue in degrees `[0, 360)`.
    pub base_hue: f32,
    /// Base saturation in percent `[0, 100]`.
    pub base_sat: f32,
    /// Base lightness in percent `[0, 100]`.
    pub base_light: f32,
}

/// Per-material style rows. Base colors are the HSL of the ground palette's
/// sRGB design values (Grass `(0.30, 0.55, 0.25)`, Dirt
/// `(0.45, 0.32, 0.18)`, Stone `(0.55, 0.55, 0.58)`, Sand
/// `(0.85, 0.78, 0.55)`, Water `(0.20, 0.40, 0.70)`).
const STYLES: [MaterialStyle; 6] = [
    // Void — never painted.
    MaterialStyle {
        base_hue: 0.0,
        base_sat: 0.0,
        base_light: 0.0,
    },
    // Grass — hsl(110, 37.5, 40).
    MaterialStyle {
        base_hue: 110.0,
        base_sat: 37.5,
        base_light: 40.0,
    },
    // Dirt — hsl(31, 42.9, 31.5).
    MaterialStyle {
        base_hue: 31.0,
        base_sat: 42.9,
        base_light: 31.5,
    },
    // Stone — hsl(240, 3.45, 56.5).
    MaterialStyle {
        base_hue: 240.0,
        base_sat: 3.45,
        base_light: 56.5,
    },
    // Sand — hsl(46, 50, 70).
    MaterialStyle {
        base_hue: 46.0,
        base_sat: 50.0,
        base_light: 70.0,
    },
    // Water — hsl(216, 55.6, 45).
    MaterialStyle {
        base_hue: 216.0,
        base_sat: 55.6,
        base_light: 45.0,
    },
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

/// The flat linear-RGB color for a material style row — its base HSL passed
/// straight through [`hsl_to_linear_rgb`].
#[must_use]
pub fn flat_color(style: &MaterialStyle) -> [f32; 3] {
    hsl_to_linear_rgb(style.base_hue, style.base_sat, style.base_light)
}

/// Convert HSL (hue degrees, saturation / lightness percent) to linear RGB
/// in `[0, 1]`. The HSL-to-sRGB step is the standard piecewise chroma
/// construction; the sRGB channels are then squared as an approximate
/// transfer into the linear space the render pipeline multiplies by
/// `view_proj`.
#[must_use]
pub fn hsl_to_linear_rgb(hue: f32, sat: f32, light: f32) -> [f32; 3] {
    let s = (sat / 100.0).clamp(0.0, 1.0);
    let l = (light / 100.0).clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (((hue % 360.0) + 360.0) % 360.0) / 60.0;
    let x = c * (1.0 - ((hp % 2.0) - 1.0).abs());
    let (r, g, b) = if hp < 1.0 {
        (c, x, 0.0)
    } else if hp < 2.0 {
        (x, c, 0.0)
    } else if hp < 3.0 {
        (0.0, c, x)
    } else if hp < 4.0 {
        (0.0, x, c)
    } else if hp < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c / 2.0;
    let srgb = [r + m, g + m, b + m];
    [srgb[0] * srgb[0], srgb[1] * srgb[1], srgb[2] * srgb[2]]
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
        let colors: Vec<[f32; 3]> = [
            Material::Grass,
            Material::Dirt,
            Material::Stone,
            Material::Sand,
            Material::Water,
        ]
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
