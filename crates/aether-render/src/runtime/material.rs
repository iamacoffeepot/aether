//! Per-frame material accumulator for the `aether.render` cap
//! (ADR-0140). Both typed material draw kinds push into one ordered
//! stream so mixed textured/coverage submissions replay in the order
//! the render capability received them.

use super::super::kinds::{
    DrawMaterialCoverage, DrawMaterialTextured, MaterialCoverageRect, MaterialTexturedRect, QuadBlend, TextureFormat,
};

#[derive(Clone)]
pub enum MaterialBatch {
    Textured { texture_id: u32, blend: QuadBlend, rects: Vec<MaterialTexturedRect> },
    Coverage { texture_id: u32, rects: Vec<MaterialCoverageRect> },
}

impl MaterialBatch {
    /// The batch a `draw_material_textured` submission accumulates to.
    pub fn textured(mail: DrawMaterialTextured) -> Self {
        Self::Textured { texture_id: mail.texture_id, blend: mail.blend, rects: mail.rects }
    }

    /// The batch a `draw_material_coverage` submission accumulates to. Both
    /// land in the same ordered stream, which is what keeps a mixed
    /// textured/coverage frame replaying in receipt order.
    pub fn coverage(mail: DrawMaterialCoverage) -> Self {
        Self::Coverage { texture_id: mail.texture_id, rects: mail.rects }
    }
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
