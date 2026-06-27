//! Session-scoped texture registry for the `aether.render` cap
//! (ADR-0105). Staged CPU RGBA8 pixels are the source of truth; the
//! wgpu texture + bind group are realized lazily at record time on the
//! driver thread. `create_texture` / `update_texture` run on the cap
//! dispatcher thread and only touch the staging side.

use std::collections::HashMap;

use aether_substrate::render::{
    QuadPipeline, RealizedTexture, realize_texture, upload_texture_full,
};

/// A texture registered via `create_texture`: the staged RGBA8 pixels
/// (the CPU source of truth), plus the lazily-realized GPU texture +
/// bind group. `create_texture` / `update_texture` run on the cap
/// dispatcher thread and only touch the staging side; the wgpu
/// resources are realized at record time on the driver thread (the
/// `RenderGpu` `OnceLock` isn't filled until the chassis driver boots
/// the GPU). `dirty` flags staging that the GPU copy hasn't caught up
/// to yet — the next record re-uploads the whole texture.
pub(super) struct StagedTexture {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pixels: Vec<u8>,
    pub(super) realized: Option<RealizedTexture>,
    pub(super) dirty: bool,
}

impl StagedTexture {
    /// Overwrite the `(x, y, width, height)` sub-rect of the staged
    /// pixels with `pixels` (RGBA8, row-major) and dirty the texture.
    /// Returns `false` without touching the buffer if the rect is
    /// out of bounds, has a zero dimension, or `pixels` isn't exactly
    /// `width * height * 4` bytes — the caller logs and drops.
    pub(super) fn apply_subrect(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> bool {
        let Some(rect_bytes) = expected_pixel_bytes(width, height) else {
            return false;
        };
        let in_bounds = x
            .checked_add(width)
            .is_some_and(|right| right <= self.width)
            && y.checked_add(height)
                .is_some_and(|bottom| bottom <= self.height);
        if !in_bounds || pixels.len() != rect_bytes {
            return false;
        }
        let row_bytes = width as usize * 4;
        let dst_stride = self.width as usize * 4;
        for row in 0..height as usize {
            let src_start = row * row_bytes;
            let dst_row = y as usize + row;
            let dst_start = dst_row * dst_stride + x as usize * 4;
            self.pixels[dst_start..dst_start + row_bytes]
                .copy_from_slice(&pixels[src_start..src_start + row_bytes]);
        }
        self.dirty = true;
        true
    }

    /// Realize the GPU texture if it isn't yet, or re-upload the
    /// staged pixels if `update_texture` dirtied them since the last
    /// record. Runs at record time on the driver thread, where a
    /// device + queue are available.
    pub(super) fn ensure_realized(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipeline: &QuadPipeline,
    ) {
        if let Some(realized) = &self.realized {
            // Already on the GPU; re-upload only if `update_texture`
            // dirtied the staging buffer since the last record.
            if self.dirty {
                upload_texture_full(queue, realized, &self.pixels);
            }
        } else {
            self.realized = Some(realize_texture(
                device,
                queue,
                pipeline,
                self.width,
                self.height,
                &self.pixels,
            ));
        }
        self.dirty = false;
    }
}

/// Reserved sentinel `texture_id` for the internal 1×1 white texture
/// used by `on_draw_solid_quads`. `create_texture` starts at `0` and
/// increments, so `u32::MAX` is outside the range any caller-visible id
/// occupies — the white texture is never handed to a caller and never
/// collides with a user-created texture.
pub(super) const WHITE_TEXTURE_ID: u32 = u32::MAX;

/// Session-scoped texture registry. `next_id` hands out the
/// `texture_id` a `create_texture` reply carries — assigned in
/// sequence the same way ADR-0103 assigns instrument ids, so ids are
/// stable for the session and depend only on creation order.
pub(super) struct TextureRegistry {
    pub(super) next_id: u32,
    pub(super) entries: HashMap<u32, StagedTexture>,
}

impl TextureRegistry {
    pub(super) fn new() -> Self {
        Self {
            next_id: 0,
            entries: HashMap::new(),
        }
    }
}

/// RGBA8 byte count for a `width x height` texture, or `None` if the
/// dimensions are zero or the product overflows `usize`. Shared by the
/// `create_texture` validation and the `update_texture` sub-rect
/// check.
pub(super) fn expected_pixel_bytes(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0105: `expected_pixel_bytes` is the single source of the
    /// RGBA8 length rule. Zero dimensions and overflowing products
    /// return `None`; a valid texture returns `width * height * 4`.
    #[test]
    fn expected_pixel_bytes_validates_dimensions() {
        assert_eq!(expected_pixel_bytes(2, 3), Some(24));
        assert_eq!(expected_pixel_bytes(0, 4), None);
        assert_eq!(expected_pixel_bytes(4, 0), None);
        assert_eq!(expected_pixel_bytes(u32::MAX, u32::MAX), None);
    }

    /// `apply_subrect` writes an in-bounds rect into the staged pixels
    /// and dirties the texture; an out-of-bounds rect, a zero
    /// dimension, or a pixel-length mismatch leaves the buffer
    /// untouched and returns `false`.
    #[test]
    fn staged_texture_apply_subrect_bounds() {
        let mut texture = StagedTexture {
            width: 2,
            height: 2,
            pixels: vec![0u8; 16],
            realized: None,
            dirty: false,
        };
        // Overwrite the bottom-right pixel (1, 1) with 0xAA bytes.
        assert!(texture.apply_subrect(1, 1, 1, 1, &[0xAA, 0xAA, 0xAA, 0xAA]));
        assert!(texture.dirty);
        assert_eq!(&texture.pixels[12..16], &[0xAA, 0xAA, 0xAA, 0xAA]);
        // The other three pixels are untouched.
        assert_eq!(&texture.pixels[0..12], &[0u8; 12]);

        // Out of bounds (rect extends past the right edge).
        texture.dirty = false;
        assert!(!texture.apply_subrect(1, 0, 2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]));
        assert!(!texture.dirty);
        // Pixel-length mismatch for the declared rect.
        assert!(!texture.apply_subrect(0, 0, 1, 1, &[1, 2, 3]));
        // Zero-sized rect.
        assert!(!texture.apply_subrect(0, 0, 0, 1, &[]));
    }
}
