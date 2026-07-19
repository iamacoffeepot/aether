//! Per-frame material accumulator for the `aether.render` cap
//! (ADR-0140). Both typed material draw kinds push into one ordered
//! stream so mixed textured/coverage submissions replay in the order
//! the render capability received them.

use super::super::kinds::{MaterialCoverageRect, MaterialTexturedRect, TextureFormat};

#[derive(Clone)]
pub enum MaterialBatch {
    Textured { texture_id: u32, rects: Vec<MaterialTexturedRect> },
    Coverage { texture_id: u32, rects: Vec<MaterialCoverageRect> },
}

#[must_use]
pub fn accepts_coverage_texture(format: TextureFormat) -> bool {
    matches!(format, TextureFormat::R8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_material_accepts_only_r8_textures() {
        assert!(accepts_coverage_texture(TextureFormat::R8));
        assert!(!accepts_coverage_texture(TextureFormat::Rgba8));
    }
}
