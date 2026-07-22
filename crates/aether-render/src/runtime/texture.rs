//! Session-scoped texture registry for the `aether.render` cap
//! (ADR-0105). Staged CPU pixels are the source of truth; the wgpu texture
//! and bind group are realized lazily at record time. `create_texture` /
//! `update_texture` only touch the staging side — the pumped runtime
//! records against the realized side on the driver thread.

use std::collections::HashMap;

use aether_substrate::render::{RealizedTexture, TextureBindings, realize_texture, upload_texture_full};

use crate::TextureFormat;

/// A texture registered via `create_texture`: the staged pixels (the CPU
/// source of truth), plus the lazily-realized GPU texture + bind group.
/// `create_texture` / `update_texture` only touch the staging side; the
/// wgpu resources are realized at record time (the `RenderGpu` boots lazily
/// on the first frame). `dirty` flags staging that the GPU copy hasn't
/// caught up to yet — the next record re-uploads the whole texture.
pub struct StagedTexture {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub pixels: Vec<u8>,
    pub realized: Option<RealizedTexture>,
    pub dirty: bool,
}

impl StagedTexture {
    /// Overwrite the `(x, y, width, height)` sub-rect of the staged
    /// pixels with `pixels` (texture-format row-major) and dirty the texture.
    /// Returns `false` without touching the buffer if the rect is
    /// out of bounds, has a zero dimension, or `pixels` isn't exactly
    /// `width * height * format.bytes_per_pixel()` bytes — the caller logs
    /// and drops.
    pub fn apply_subrect(&mut self, x: u32, y: u32, width: u32, height: u32, pixels: &[u8]) -> bool {
        let Some(rect_bytes) = expected_pixel_bytes(width, height, self.format) else {
            return false;
        };
        let in_bounds = x.checked_add(width).is_some_and(|right| right <= self.width)
            && y.checked_add(height).is_some_and(|bottom| bottom <= self.height);
        if !in_bounds || pixels.len() != rect_bytes {
            return false;
        }
        let bytes_per_pixel = self.format.bytes_per_pixel();
        let row_bytes = width as usize * bytes_per_pixel;
        let dst_stride = self.width as usize * bytes_per_pixel;
        for row in 0..height as usize {
            let src_start = row * row_bytes;
            let dst_row = y as usize + row;
            let dst_start = dst_row * dst_stride + x as usize * bytes_per_pixel;
            self.pixels[dst_start..dst_start + row_bytes].copy_from_slice(&pixels[src_start..src_start + row_bytes]);
        }
        self.dirty = true;
        true
    }

    /// Realize the GPU texture if it isn't yet, or re-upload the
    /// staged pixels if `update_texture` dirtied them since the last
    /// record. Runs at record time on the driver thread, where a
    /// device + queue are available.
    pub fn ensure_realized(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, texture_bindings: &TextureBindings) {
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
                texture_bindings,
                self.width,
                self.height,
                wgpu_texture_format(self.format),
                &self.pixels,
            ));
        }
        self.dirty = false;
    }
}

fn wgpu_texture_format(format: TextureFormat) -> wgpu::TextureFormat {
    match format {
        TextureFormat::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
        TextureFormat::R8 => wgpu::TextureFormat::R8Unorm,
    }
}

/// Reserved sentinel `texture_id` for the internal 1×1 white texture
/// used by `on_draw_solid_quads`. `create_texture` starts at `0` and
/// increments, so `u32::MAX` is outside the range any caller-visible id
/// occupies. Typed `SubstrateHarness` observations expose the sentinel so normalized
/// solid batches remain identifiable, but callers cannot allocate, update,
/// or destroy it and it never collides with a user-created texture.
pub const WHITE_TEXTURE_ID: u32 = u32::MAX;

/// Session-scoped texture registry. `next_id` hands out the
/// `texture_id` a `create_texture` reply carries — assigned in
/// sequence the same way ADR-0103 assigns instrument ids, so ids are
/// stable for the session and depend only on creation order.
pub struct TextureRegistry {
    pub next_id: u32,
    pub entries: HashMap<u32, StagedTexture>,
}

impl TextureRegistry {
    pub fn new() -> Self {
        Self { next_id: 0, entries: HashMap::new() }
    }
}

/// Byte count for a `width x height` texture in `format`, or `None` if
/// the dimensions are zero or the product overflows `usize`. Shared by
/// the `create_texture` validation and the `update_texture` sub-rect
/// check.
pub fn expected_pixel_bytes(width: u32, height: u32, format: TextureFormat) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    (width as usize).checked_mul(height as usize).and_then(|pixels| pixels.checked_mul(format.bytes_per_pixel()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0105 + ADR-0140: `expected_pixel_bytes` is the single source
    /// of the per-format length rule. Zero dimensions and overflowing
    /// products return `None`; a valid texture returns
    /// `width * height * bytes_per_pixel`.
    #[test]
    fn expected_pixel_bytes_validates_dimensions() {
        assert_eq!(expected_pixel_bytes(2, 3, TextureFormat::Rgba8), Some(24));
        assert_eq!(expected_pixel_bytes(2, 3, TextureFormat::R8), Some(6));
        assert_eq!(expected_pixel_bytes(0, 4, TextureFormat::Rgba8), None);
        assert_eq!(expected_pixel_bytes(4, 0, TextureFormat::R8), None);
        assert_eq!(expected_pixel_bytes(u32::MAX, u32::MAX, TextureFormat::Rgba8), None);
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
            format: TextureFormat::Rgba8,
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

    #[test]
    fn staged_texture_apply_subrect_uses_r8_stride() {
        let mut texture = StagedTexture {
            width: 4,
            height: 2,
            format: TextureFormat::R8,
            pixels: vec![0u8; 8],
            realized: None,
            dirty: false,
        };

        assert!(texture.apply_subrect(1, 0, 2, 2, &[10, 20, 30, 40]));
        assert_eq!(&texture.pixels, &[0, 10, 20, 0, 0, 30, 40, 0]);
        assert!(texture.dirty);

        texture.dirty = false;
        assert!(!texture.apply_subrect(0, 0, 2, 1, &[1, 2, 3]));
        assert!(!texture.dirty);
    }
}
