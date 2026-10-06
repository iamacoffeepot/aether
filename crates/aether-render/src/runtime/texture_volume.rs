//! Volume textures in the `aether.render` texture registry (ADR-0246
//! decision 6): width by height by depth texels, given whole at creation
//! and never written again.
//!
//! A volume shares the registry's id sequence with the plain textures
//! and the arrays and lives in its own map, so an id names one of the
//! three and every reader of another map already treats a volume id as
//! unknown. The received blob is the source of truth, as the blob of a
//! written layer is for an array: the wgpu texture is realized lazily at
//! record time and rebuilt from the blob on a replacement device.

use aether_data::Blob;

use super::surface::render_limits;
use super::texture::{TextureRegistry, wgpu_texture_format};
use crate::TextureFormat;
use crate::kinds::{CreateTextureVolume, CreateTextureVolumeResult};

/// A volume registered via `create_texture_volume`: its fixed shape, the
/// blob it was created from, and the lazily-realized GPU texture. `dirty`
/// says the blob has not reached the current device's texture yet.
pub struct StagedTextureVolume {
    pub format: TextureFormat,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub pixels: Blob,
    pub realized: Option<wgpu::Texture>,
    pub dirty: bool,
}

/// Byte count of a whole volume, which is the length
/// `CreateTextureVolume.pixels` must be, or `None` if it overflows
/// `usize`.
fn volume_bytes(format: TextureFormat, width: u32, height: u32, depth: u32) -> Option<usize> {
    (width as usize).checked_mul(height as usize)?.checked_mul(depth as usize)?.checked_mul(format.bytes_per_pixel())
}

impl StagedTextureVolume {
    /// Drop the realization built against the current device and mark
    /// the blob for upload to the replacement.
    pub fn invalidate_device_resources(&mut self) {
        self.realized = None;
        self.dirty = true;
    }

    /// Create the GPU texture if it does not exist yet and upload the
    /// blob if this device's texture has not received it: every slice in
    /// one `write_texture`. Runs at record time on the driver thread,
    /// where a device and queue are available.
    ///
    /// # Panics
    /// Panics if the blob is not contiguous, fail-fast per ADR-0063:
    /// `create_volume` refuses a non-contiguous one before it stages the
    /// volume.
    pub fn ensure_realized(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let size = wgpu::Extent3d { width: self.width, height: self.height, depth_or_array_layers: self.depth };
        let texture: &wgpu::Texture = self.realized.get_or_insert_with(|| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("aether texture volume"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu_texture_format(self.format),
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        });
        if !self.dirty {
            return;
        }

        let bytes_per_pixel = u32::try_from(self.format.bytes_per_pixel()).expect("bytes per pixel fit u32");
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            self.pixels.contiguous().expect("create_texture_volume refuses non-contiguous pixel bytes"),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * bytes_per_pixel),
                rows_per_image: Some(self.height),
            },
            size,
        );
        self.dirty = false;
    }
}

impl TextureRegistry {
    /// Stage a new volume, validating its shape and `pixels` before any
    /// id is consumed. Every check reads [`render_limits`], the floor
    /// each render device is requested at, so none needs a device.
    pub fn create_volume(&mut self, mail: CreateTextureVolume) -> CreateTextureVolumeResult {
        let CreateTextureVolume { format, width, height, depth, pixels } = mail;
        let Some(supplied) = pixels.contiguous().map(<[u8]>::len) else {
            return CreateTextureVolumeResult::Err { error: "pixel bytes are not resident in this process".to_owned() };
        };
        if width == 0 || height == 0 || depth == 0 {
            return CreateTextureVolumeResult::Err {
                error: format!("texture volume dimensions {width}x{height}x{depth} have a zero dimension"),
            };
        }
        let max_dimension = render_limits().max_texture_dimension_3d;
        if width > max_dimension || height > max_dimension || depth > max_dimension {
            return CreateTextureVolumeResult::Err {
                error: format!(
                    "texture volume dimensions {width}x{height}x{depth} exceed the device limit \
                     max_texture_dimension_3d = {max_dimension}",
                ),
            };
        }
        let Some(expected) = volume_bytes(format, width, height, depth) else {
            return CreateTextureVolumeResult::Err {
                error: format!("a {format:?} volume {width}x{height}x{depth} overflows the addressable byte count"),
            };
        };
        if supplied != expected {
            return CreateTextureVolumeResult::Err {
                error: format!(
                    "pixels length {supplied} does not match {width}x{height}x{depth} {format:?} = {expected}",
                ),
            };
        }
        let Some(texture_id) = self.ids.allocate() else {
            return CreateTextureVolumeResult::Err {
                error: "this session has run out of texture ids; destroy_texture does not recycle them".to_owned(),
            };
        };

        self.volumes.insert(
            texture_id,
            StagedTextureVolume { format, width, height, depth, pixels, realized: None, dirty: true },
        );
        CreateTextureVolumeResult::Ok { texture_id }
    }
}

#[cfg(test)]
mod tests {
    use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;

    use super::*;
    use crate::kinds::{
        CreateTexture, CreateTextureArray, CreateTextureArrayResult, CreateTextureResult, DestroyTexture,
        UpdateTexture, WriteTextureLayer,
    };
    use crate::runtime::surface::boot_offscreen;
    use crate::{Mips, TextureSampling, TextureUsage};

    fn volume(width: u32, height: u32, depth: u32, pixels: Vec<u8>) -> CreateTextureVolume {
        CreateTextureVolume { format: TextureFormat::Rgba8, width, height, depth, pixels: Blob::from(pixels) }
    }

    fn created(registry: &mut TextureRegistry, mail: CreateTextureVolume) -> u32 {
        match registry.create_volume(mail) {
            CreateTextureVolumeResult::Ok { texture_id } => texture_id,
            CreateTextureVolumeResult::Err { error } => panic!("create_texture_volume must be accepted: {error}"),
        }
    }

    fn refusal(registry: &mut TextureRegistry, mail: CreateTextureVolume) -> String {
        match registry.create_volume(mail) {
            CreateTextureVolumeResult::Err { error } => error,
            CreateTextureVolumeResult::Ok { texture_id } => panic!("create must be refused; got texture {texture_id}"),
        }
    }

    /// Each refusal class replies its own reason and consumes no id. The
    /// named bugs: a volume past the device limit or with the wrong byte
    /// count reaching `create_texture` and `write_texture`, where it is a
    /// device validation error and no reply; a length check that counts
    /// one slice, which admits a blob the upload then reads past; and a
    /// refused create burning an id, so ids stop being dense over
    /// accepted textures.
    #[test]
    fn each_create_refusal_has_its_own_reason_and_consumes_no_id() {
        let mut registry = TextureRegistry::new();
        let max_dimension = render_limits().max_texture_dimension_3d;

        let reasons = [
            refusal(&mut registry, volume(2, 0, 2, Vec::new())),
            refusal(&mut registry, volume(2, 2, 0, Vec::new())),
            refusal(&mut registry, volume(1, 1, max_dimension + 1, vec![0; 4])),
            refusal(&mut registry, volume(2, 2, 2, vec![0; 16])),
        ];

        assert!(reasons[0].contains("zero dimension"), "got: {}", reasons[0]);
        assert!(reasons[1].contains("zero dimension"), "got: {}", reasons[1]);
        assert!(reasons[2].contains("max_texture_dimension_3d"), "got: {}", reasons[2]);
        assert!(reasons[3].contains("pixels length 16"), "one slice is not the volume: {}", reasons[3]);
        assert_eq!(registry.ids.peek(), Some(0), "a refused create must not consume an id");
        assert!(registry.volumes.is_empty(), "a refused create must stage nothing");

        let accepted = created(&mut registry, volume(2, 2, 2, vec![0; 32]));
        assert_eq!(accepted, 0, "every slice supplied is admitted, under the first id");
    }

    /// A plain texture, an array and a volume draw ids from one sequence,
    /// and each verb reaches only the map it belongs to. The named bugs:
    /// a third map with its own sequence handing out an id another map
    /// holds, so a program binding or a destroy names two resources at
    /// once; and a destroy that stops at the first two maps, which leaks
    /// every volume for the session.
    #[test]
    fn a_volume_shares_no_id_and_only_destroy_reaches_it() {
        let mut registry = TextureRegistry::new();
        let CreateTextureResult::Ok { texture_id: plain_id } = registry.create(CreateTexture {
            width: 1,
            height: 1,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: Blob::from(vec![9; 4]),
        }) else {
            panic!("plain create accepted");
        };
        let array = CreateTextureArray { format: TextureFormat::Rgba8, side: 1, layers: 1, mips: Mips::Base };
        let CreateTextureArrayResult::Ok { texture_id: array_id } = registry.create_array(array, 8) else {
            panic!("array create accepted");
        };
        let volume_id = created(&mut registry, volume(1, 1, 1, vec![7; 4]));
        assert!(plain_id != volume_id && array_id != volume_id, "the three maps must not hand out the same id");

        registry.update(UpdateTexture { texture_id: volume_id, x: 0, y: 0, width: 1, height: 1, pixels: vec![1; 4] });
        registry.write_layer(WriteTextureLayer { texture_id: volume_id, layer: 0, pixels: Blob::from(vec![1; 4]) });
        assert_eq!(
            registry.volumes[&volume_id].pixels.contiguous(),
            Some(&[7u8; 4][..]),
            "update_texture and write_texture_layer naming a volume must leave it untouched",
        );
        assert_eq!(registry.entries[&plain_id].pixels.bytes(), [9; 4], "and must not reach the plain texture");
        assert!(registry.arrays[&array_id].written[0].is_none(), "or the array");

        registry.destroy(DestroyTexture { texture_id: volume_id });
        assert!(!registry.volumes.contains_key(&volume_id), "destroy_texture releases a volume");
        assert!(registry.entries.contains_key(&plain_id), "and leaves the plain texture registered");
        assert!(registry.arrays.contains_key(&array_id), "and the array");
    }

    /// A device replacement drops the realization and re-uploads from
    /// the kept blob. The named bugs: a realization kept across the
    /// replacement, which binds a texture of the dead device; and a
    /// volume left clean, whose replacement texture is never uploaded and
    /// reads as zero.
    #[test]
    fn device_invalidation_drops_the_realization_and_reuploads_the_blob() {
        if !has_wgpu_adapter() {
            return;
        }
        let booted = boot_offscreen(None);
        let mut registry = TextureRegistry::new();
        let volume_id = created(&mut registry, volume(2, 2, 2, vec![3; 32]));
        let staged = registry.volumes.get_mut(&volume_id).expect("the volume is staged");
        staged.ensure_realized(&booted.device, &booted.queue);
        assert!(staged.realized.is_some(), "precondition: the volume is realized on the old device");
        assert!(!staged.dirty, "precondition: realization uploaded the blob");

        registry.invalidate_device_resources();

        let staged = registry.volumes.get_mut(&volume_id).expect("device replacement must preserve the volume");
        assert!(staged.realized.is_none(), "the old-device texture must be released");
        assert!(staged.dirty, "the blob must be upload-ready for the replacement device");
        staged.ensure_realized(&booted.device, &booted.queue);
        assert!(staged.realized.is_some(), "the replacement realization is built from the kept blob");
        assert!(!staged.dirty, "and uploaded");
    }
}
