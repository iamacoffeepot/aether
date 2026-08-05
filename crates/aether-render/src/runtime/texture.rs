//! Session-scoped texture registry for the `aether.render` cap
//! (ADR-0105). Staged CPU pixels are the source of truth; the wgpu texture
//! and bind group are realized lazily at record time. `create_texture` /
//! `update_texture` only touch the staging side — the pumped runtime
//! records against the realized side on the driver thread.

use std::collections::HashMap;

use aether_substrate::render::{
    RealizedTexture, TextureBindings, realize_texture, realize_writable_texture, upload_texture_full,
};

use crate::kinds::{CreateTexture, CreateTextureResult, DestroyTexture, UpdateTexture};
use crate::{TextureFormat, TextureSampling, TextureUsage};

/// A texture registered via `create_texture`: the staged pixels (the CPU
/// source of truth), plus the lazily-realized GPU texture + bind group.
/// `create_texture` / `update_texture` only touch the staging side; the
/// wgpu resources are realized at record time (the `RenderGpu` boots lazily
/// on the first frame). `dirty` flags staging that the GPU copy hasn't
/// caught up to yet — the next record re-uploads the whole texture. A
/// `Writable` texture (ADR-0170) has no CPU staging: `pixels` stays
/// empty, `update` warn-drops, and realization clears the GPU render
/// target instead of uploading.
pub struct StagedTexture {
    pub width: u32,
    pub height: u32,
    pub format: TextureFormat,
    pub sampling: TextureSampling,
    pub usage: TextureUsage,
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
    /// device + queue are available. A `Writable` texture realizes as a
    /// cleared render target (ADR-0170) and has no staging to re-upload
    /// (`update` rejects it, so `dirty` never sets).
    pub fn ensure_realized(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, texture_bindings: &TextureBindings) {
        let nearest = self.sampling == TextureSampling::Nearest;
        if let Some(realized) = &self.realized {
            // Already on the GPU; re-upload only if `update_texture`
            // dirtied the staging buffer since the last record.
            if self.dirty {
                upload_texture_full(queue, realized, &self.pixels);
            }
        } else {
            self.realized = Some(match self.usage {
                TextureUsage::Sampled => realize_texture(
                    device,
                    queue,
                    texture_bindings,
                    self.width,
                    self.height,
                    wgpu_texture_format(self.format),
                    nearest,
                    &self.pixels,
                ),
                TextureUsage::Writable => realize_writable_texture(
                    device,
                    queue,
                    texture_bindings,
                    self.width,
                    self.height,
                    wgpu_texture_format(self.format),
                    nearest,
                ),
            });
        }
        self.dirty = false;
    }
}

pub(super) fn wgpu_texture_format(format: TextureFormat) -> wgpu::TextureFormat {
    match format {
        TextureFormat::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
        TextureFormat::R8 => wgpu::TextureFormat::R8Unorm,
        TextureFormat::R32Float => wgpu::TextureFormat::R32Float,
        TextureFormat::R16Float => wgpu::TextureFormat::R16Float,
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

    /// Drop every realization built against the current device while
    /// preserving the session-scoped registry. Sampled textures retain
    /// their CPU pixels and become upload-ready for the replacement
    /// device; writable textures have no staging and therefore restart
    /// unstaged, so their next realization clears them.
    #[allow(dead_code, reason = "device-loss runtime wiring lands in the next recovery slice")]
    pub fn invalidate_device_resources(&mut self) {
        for entry in self.entries.values_mut() {
            entry.realized = None;
            entry.dirty = entry.usage == TextureUsage::Sampled;
        }
    }

    /// Stage a new texture, validating the declared dimensions, format,
    /// sampling, and `pixels` before any id is consumed. A rejected create
    /// leaves `next_id` untouched, so ids stay dense over accepted textures.
    pub fn create(&mut self, mail: CreateTexture) -> CreateTextureResult {
        let Some(expected) = expected_pixel_bytes(mail.width, mail.height, mail.format) else {
            return CreateTextureResult::Err {
                error: format!("texture dimensions {}x{} overflow or are zero", mail.width, mail.height),
            };
        };
        let max_dimension = super::surface::render_limits().max_texture_dimension_2d;
        if mail.width > max_dimension || mail.height > max_dimension {
            return CreateTextureResult::Err {
                error: format!(
                    "texture dimensions {}x{} exceed the device limit max_texture_dimension_2d = {max_dimension}",
                    mail.width, mail.height,
                ),
            };
        }
        if mail.sampling == TextureSampling::Linear && !mail.format.filterable() {
            return CreateTextureResult::Err {
                error: format!("{:?} cannot be linear-filtered; create it with Nearest sampling", mail.format),
            };
        }
        match mail.usage {
            TextureUsage::Sampled if mail.pixels.len() != expected => {
                return CreateTextureResult::Err {
                    error: format!(
                        "pixels length {} does not match {}x{} {:?} = {expected}",
                        mail.pixels.len(),
                        mail.width,
                        mail.height,
                        mail.format
                    ),
                };
            }
            TextureUsage::Writable if !mail.pixels.is_empty() => {
                return CreateTextureResult::Err {
                    error: format!(
                        "writable textures are created without staged pixels, but {} bytes were supplied",
                        mail.pixels.len()
                    ),
                };
            }
            TextureUsage::Sampled | TextureUsage::Writable => {}
        }
        let texture_id = self.next_id;
        self.next_id += 1;
        self.entries.insert(
            texture_id,
            StagedTexture {
                width: mail.width,
                height: mail.height,
                format: mail.format,
                sampling: mail.sampling,
                usage: mail.usage,
                pixels: mail.pixels,
                realized: None,
                dirty: mail.usage == TextureUsage::Sampled,
            },
        );
        CreateTextureResult::Ok { texture_id }
    }

    /// Overwrite a sub-rect of an existing texture. Fire-and-forget, so every
    /// rejection warns and drops rather than replying — the reserved white
    /// texture is not writable, and neither is an id that was never created.
    pub fn update(&mut self, mail: UpdateTexture) {
        if mail.texture_id == WHITE_TEXTURE_ID {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "update_texture for reserved internal texture id; dropping",
            );
            return;
        }
        let Some(entry) = self.entries.get_mut(&mail.texture_id) else {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "update_texture for unknown texture id; dropping",
            );
            return;
        };
        if entry.usage == TextureUsage::Writable {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "update_texture for a writable texture, which has no CPU staging; dropping",
            );
            return;
        }
        if !entry.apply_subrect(mail.x, mail.y, mail.width, mail.height, &mail.pixels) {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "update_texture rect out of bounds, zero-sized, or pixel length mismatch; \
                 dropping",
            );
        }
    }

    /// Release a registered texture. Same fire-and-forget disposition as
    /// [`Self::update`].
    pub fn destroy(&mut self, mail: DestroyTexture) {
        if mail.texture_id == WHITE_TEXTURE_ID {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "destroy_texture for reserved internal texture id; dropping",
            );
            return;
        }
        if self.entries.remove(&mail.texture_id).is_none() {
            tracing::warn!(
                target: "aether_render",
                texture_id = mail.texture_id,
                "destroy_texture for unknown texture id; dropping",
            );
        }
    }

    /// Register the reserved 1x1 opaque white texture if it is not present.
    /// Solid quads are expanded over it (ADR-0107 §4), so it is created on
    /// first use rather than at boot — a runtime that never draws a solid quad
    /// never allocates it.
    pub fn ensure_white(&mut self) {
        self.entries.entry(WHITE_TEXTURE_ID).or_insert_with(|| StagedTexture {
            width: 1,
            height: 1,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: vec![255, 255, 255, 255],
            realized: None,
            dirty: true,
        });
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
    use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;
    use aether_substrate::render::build_texture_bindings;

    use super::*;
    use crate::runtime::surface::boot_offscreen;

    /// ADR-0105 + ADR-0140: `expected_pixel_bytes` is the single source
    /// of the per-format length rule. Zero dimensions and overflowing
    /// products return `None`; a valid texture returns
    /// `width * height * bytes_per_pixel`.
    #[test]
    fn expected_pixel_bytes_validates_dimensions() {
        assert_eq!(expected_pixel_bytes(2, 3, TextureFormat::Rgba8), Some(24));
        assert_eq!(expected_pixel_bytes(2, 3, TextureFormat::R8), Some(6));
        assert_eq!(expected_pixel_bytes(2, 3, TextureFormat::R32Float), Some(24));
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
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
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
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
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

    /// ADR-0170: a writable create must arrive without staged pixels —
    /// letting bytes through would hand `ensure_realized` staging it can
    /// never upload (the realized target has no `COPY_DST`) — and a
    /// rejected create must not consume an id.
    #[test]
    fn create_writable_rejects_staged_pixels() {
        let mut registry = TextureRegistry::new();
        let rejected = registry.create(CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: vec![0u8; 16],
        });
        assert!(matches!(rejected, CreateTextureResult::Err { .. }), "staged pixels on a writable create must reject");
        assert_eq!(registry.next_id, 0, "a rejected create must not consume an id");

        let accepted = registry.create(CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        });
        let CreateTextureResult::Ok { texture_id } = accepted else {
            panic!("an empty-pixels writable create must be accepted");
        };
        let entry = registry.entries.get(&texture_id).expect("accepted create stages an entry");
        assert!(!entry.dirty, "a writable texture has no staging for the record path to re-upload");
    }

    /// ADR-0170: core WebGPU cannot linear-filter `R32Float`, so a
    /// `Linear` create over it must reject at mail time rather than die
    /// as a wgpu validation error at realization.
    #[test]
    fn create_r32float_requires_nearest_sampling() {
        let mut registry = TextureRegistry::new();
        let rejected = registry.create(CreateTexture {
            width: 2,
            height: 1,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: vec![0u8; 8],
        });
        assert!(matches!(rejected, CreateTextureResult::Err { .. }), "linear sampling over R32Float must reject");

        let accepted = registry.create(CreateTexture {
            width: 2,
            height: 1,
            format: TextureFormat::R32Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: vec![0u8; 8],
        });
        assert!(matches!(accepted, CreateTextureResult::Ok { .. }), "nearest-sampled R32Float must be accepted");
    }

    /// ADR-0170: `update_texture` against a writable texture must drop
    /// without dirtying — a set `dirty` would make the next record call
    /// `upload_texture_full` with empty staging against a target that
    /// has no `COPY_DST` usage.
    #[test]
    fn update_writable_texture_drops_without_dirtying() {
        let mut registry = TextureRegistry::new();
        let created = registry.create(CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        });
        let CreateTextureResult::Ok { texture_id } = created else {
            panic!("writable create accepted");
        };

        registry.update(UpdateTexture { texture_id, x: 0, y: 0, width: 1, height: 1, pixels: vec![1, 2, 3, 4] });

        let entry = registry.entries.get(&texture_id).expect("entry survives the dropped update");
        assert!(entry.pixels.is_empty(), "a writable texture must never gain staged pixels");
        assert!(!entry.dirty, "a dropped update must not dirty a writable texture");
    }

    #[test]
    fn device_invalidation_preserves_staging_and_restarts_writable_textures_cleared() {
        if !has_wgpu_adapter() {
            return;
        }
        let booted = boot_offscreen(None);
        let bindings = build_texture_bindings(&booted.device);
        let mut registry = TextureRegistry::new();
        let sampled_pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let CreateTextureResult::Ok { texture_id: sampled_id } = registry.create(CreateTexture {
            width: 2,
            height: 1,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: sampled_pixels.clone(),
        }) else {
            panic!("sampled create accepted");
        };
        let CreateTextureResult::Ok { texture_id: writable_id } = registry.create(CreateTexture {
            width: 3,
            height: 2,
            format: TextureFormat::R16Float,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Writable,
            pixels: Vec::new(),
        }) else {
            panic!("writable create accepted");
        };
        registry.ensure_white();
        for entry in registry.entries.values_mut() {
            entry.ensure_realized(&booted.device, &booted.queue, &bindings);
            assert!(entry.realized.is_some(), "precondition: every entry is realized on the old device");
        }

        registry.invalidate_device_resources();

        assert_eq!(registry.next_id, 2, "device replacement must not rewind public ids");
        assert_eq!(registry.entries.len(), 3, "device replacement must preserve every registered id");
        let sampled = &registry.entries[&sampled_id];
        assert_eq!(sampled.width, 2);
        assert_eq!(sampled.height, 1);
        assert_eq!(sampled.format, TextureFormat::Rgba8);
        assert_eq!(sampled.sampling, TextureSampling::Nearest);
        assert_eq!(sampled.pixels, sampled_pixels);
        assert!(sampled.realized.is_none(), "the old-device texture must be released");
        assert!(sampled.dirty, "sampled pixels must be upload-ready for the replacement device");

        let writable = &registry.entries[&writable_id];
        assert_eq!(writable.width, 3);
        assert_eq!(writable.height, 2);
        assert_eq!(writable.format, TextureFormat::R16Float);
        assert!(writable.pixels.is_empty(), "writable textures remain deliberately unstaged");
        assert!(writable.realized.is_none(), "the old-device writable texture must be released");
        assert!(!writable.dirty, "a writable texture must recreate cleared rather than attempt an upload");

        let white = &registry.entries[&WHITE_TEXTURE_ID];
        assert_eq!(white.pixels, vec![255, 255, 255, 255]);
        assert!(white.realized.is_none());
        assert!(white.dirty, "the internal sampled texture must rebuild with the rest of the registry");
    }
}
