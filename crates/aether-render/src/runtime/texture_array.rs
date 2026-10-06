//! Texture arrays in the `aether.render` texture registry (ADR-0246
//! decision 6): a fixed number of square layers, each written in place.
//!
//! An array shares the registry's id sequence with the plain textures
//! and the volumes and lives in its own map, so an id names one of them
//! and every reader of the plain map already treats an array id as
//! unknown. The
//! received blob of each written layer is the source of truth, as staged
//! pixels are for a plain texture: the wgpu texture is realized lazily at
//! record time and rebuilt from the blobs on a replacement device.

use aether_data::Blob;
use aether_substrate::memory::MemoryCharge;

use super::surface::render_limits;
use super::texture::{TextureRegistry, wgpu_texture_format};
use crate::kinds::{CreateTextureArray, CreateTextureArrayResult, WriteTextureLayer};
use crate::{Mips, TextureFormat};

/// A texture array registered via `create_texture_array`: its fixed
/// shape, the blob of each layer written so far, and the lazily-realized
/// GPU texture. `written` and `dirty` hold one slot per layer; a layer
/// that was never written holds no blob and is never uploaded, so it
/// reads as the zero a fresh wgpu texture holds.
pub struct StagedTextureArray {
    pub format: TextureFormat,
    pub side: u32,
    pub layers: u32,
    pub mips: Mips,
    pub written: Vec<Option<Blob>>,
    pub dirty: Vec<bool>,
    pub realized: Option<wgpu::Texture>,
    /// Every layer's bytes on the render capability's `textures` memory
    /// gauge, written or not: what the array occupies once realized on the
    /// device. Held only to be dropped: the bytes are subtracted when the
    /// entry drops.
    pub _charge: MemoryCharge,
}

/// How many levels an array of `side` has: one for `Mips::Base`, and
/// for `Mips::Chain` every level down to a single texel.
fn level_count(side: u32, mips: Mips) -> u32 {
    match mips {
        Mips::Base => 1,
        Mips::Chain => side.checked_ilog2().map_or(0, |log| log + 1),
    }
}

/// The side of each level, base level first. A level is half the one
/// above it, rounded down and never below one texel.
fn level_sides(side: u32, mips: Mips) -> impl Iterator<Item = u32> {
    (0..level_count(side, mips)).map(move |level| (side >> level).max(1))
}

/// Byte count of one level of side `level_side`, or `None` if it
/// overflows `usize`.
fn level_bytes(format: TextureFormat, level_side: u32) -> Option<usize> {
    (level_side as usize).checked_mul(level_side as usize)?.checked_mul(format.bytes_per_pixel())
}

/// Byte count of one layer across every level it has, which is the
/// length `WriteTextureLayer.pixels` must be, or `None` if it overflows
/// `usize`.
pub fn layer_bytes(format: TextureFormat, side: u32, mips: Mips) -> Option<usize> {
    level_sides(side, mips).try_fold(0usize, |total, level_side| total.checked_add(level_bytes(format, level_side)?))
}

impl StagedTextureArray {
    /// Drop the realization built against the current device and mark
    /// every written layer for upload to the replacement. An unwritten
    /// layer stays clean: the new texture already holds its zeros.
    pub fn invalidate_device_resources(&mut self) {
        self.realized = None;
        for (dirty, written) in self.dirty.iter_mut().zip(&self.written) {
            *dirty = written.is_some();
        }
    }

    /// Create the GPU texture if it does not exist yet and upload every
    /// dirty layer, one `write_texture` per level. Runs at record time on
    /// the driver thread, where a device and queue are available. The
    /// texture is created once per device, so a later write lands in the
    /// texture a cached view already names.
    ///
    /// # Panics
    /// Panics if a dirty layer holds no contiguous blob, fail-fast per
    /// ADR-0063: `write_layer` stores the blob and refuses a
    /// non-contiguous one before it sets the flag.
    pub fn ensure_realized(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let texture: &wgpu::Texture = self.realized.get_or_insert_with(|| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some("aether texture array"),
                size: wgpu::Extent3d { width: self.side, height: self.side, depth_or_array_layers: self.layers },
                mip_level_count: level_count(self.side, self.mips),
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu_texture_format(self.format),
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        });
        let bytes_per_pixel = u32::try_from(self.format.bytes_per_pixel()).expect("bytes per pixel fit u32");

        for (layer, (dirty, written)) in self.dirty.iter_mut().zip(&self.written).enumerate() {
            if !*dirty {
                continue;
            }
            let pixels = written
                .as_ref()
                .and_then(Blob::contiguous)
                .expect("write_texture_layer stores a contiguous blob before it dirties the layer");
            let mut offset = 0;
            for (level, level_side) in level_sides(self.side, self.mips).enumerate() {
                let end = offset
                    + level_bytes(self.format, level_side).expect("create_texture_array refuses an overflowing layer");
                queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture,
                        mip_level: u32::try_from(level).expect("mip level fits u32"),
                        origin: wgpu::Origin3d { x: 0, y: 0, z: u32::try_from(layer).expect("layer index fits u32") },
                        aspect: wgpu::TextureAspect::All,
                    },
                    &pixels[offset..end],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(level_side * bytes_per_pixel),
                        rows_per_image: Some(level_side),
                    },
                    wgpu::Extent3d { width: level_side, height: level_side, depth_or_array_layers: 1 },
                );
                offset = end;
            }
            *dirty = false;
        }
    }
}

impl TextureRegistry {
    /// Stage a new texture array, validating its shape before any id is
    /// consumed. `max_layers` is the render device's
    /// `max_texture_array_layers`, passed in so the checks run without a
    /// device. The array starts with every layer unwritten.
    pub fn create_array(&mut self, mail: CreateTextureArray, max_layers: u32) -> CreateTextureArrayResult {
        if mail.side == 0 {
            return CreateTextureArrayResult::Err { error: "texture array side is zero".to_owned() };
        }
        if mail.layers == 0 {
            return CreateTextureArrayResult::Err { error: "texture array has zero layers".to_owned() };
        }
        let max_dimension = render_limits().max_texture_dimension_2d;
        if mail.side > max_dimension {
            return CreateTextureArrayResult::Err {
                error: format!(
                    "texture array side {} exceeds the device limit max_texture_dimension_2d = {max_dimension}",
                    mail.side,
                ),
            };
        }
        if mail.layers > max_layers {
            return CreateTextureArrayResult::Err {
                error: format!(
                    "texture array layer count {} exceeds the device limit max_texture_array_layers = {max_layers}",
                    mail.layers,
                ),
            };
        }
        let Some(bytes_per_layer) = layer_bytes(mail.format, mail.side, mail.mips) else {
            return CreateTextureArrayResult::Err {
                error: format!("a {:?} layer of side {} overflows the addressable byte count", mail.format, mail.side),
            };
        };
        let Some(texture_id) = self.ids.allocate() else {
            return CreateTextureArrayResult::Err {
                error: "this session has run out of texture ids; destroy_texture does not recycle them".to_owned(),
            };
        };

        let layers = mail.layers as usize;
        self.arrays.insert(
            texture_id,
            StagedTextureArray {
                format: mail.format,
                side: mail.side,
                layers: mail.layers,
                mips: mail.mips,
                written: vec![None; layers],
                dirty: vec![false; layers],
                realized: None,
                _charge: self.memory.charge(bytes_per_layer.saturating_mul(layers)),
            },
        );
        CreateTextureArrayResult::Ok { texture_id }
    }

    /// Replace one layer of an array with `mail.pixels`. Fire-and-forget,
    /// so every refusal warns and leaves the layer as it was. An accepted
    /// write keeps the blob and dirties its layer alone; the upload
    /// happens at the next record that binds the array.
    pub fn write_layer(&mut self, mail: WriteTextureLayer) {
        let WriteTextureLayer { texture_id, layer, pixels } = mail;
        let Some(array) = self.arrays.get_mut(&texture_id) else {
            let plain = self.entries.contains_key(&texture_id);
            let volume = self.volumes.contains_key(&texture_id);
            if plain || volume {
                tracing::warn!(
                    target: "aether_render",
                    texture_id,
                    "write_texture_layer names a texture that is not an array; dropping",
                );
            } else {
                tracing::warn!(
                    target: "aether_render",
                    texture_id,
                    "write_texture_layer for unknown texture id; dropping",
                );
            }
            return;
        };
        if layer >= array.layers {
            tracing::warn!(
                target: "aether_render",
                texture_id,
                layer,
                layers = array.layers,
                "write_texture_layer names a layer past the array's layer count; dropping",
            );
            return;
        }
        let Some(supplied) = pixels.contiguous().map(<[u8]>::len) else {
            tracing::warn!(
                target: "aether_render",
                texture_id,
                layer,
                "write_texture_layer pixel bytes are not resident in this process; dropping",
            );
            return;
        };
        let expected = layer_bytes(array.format, array.side, array.mips)
            .expect("create_texture_array refuses an overflowing layer");
        if supplied != expected {
            tracing::warn!(
                target: "aether_render",
                texture_id,
                layer,
                supplied,
                expected,
                "write_texture_layer pixel length is not every level of one layer; dropping",
            );
            return;
        }

        array.written[layer as usize] = Some(pixels);
        array.dirty[layer as usize] = true;
    }
}

#[cfg(test)]
mod tests {
    use aether_harness_substrate_capture::test_helpers::has_wgpu_adapter;

    use super::*;
    use crate::kinds::{CreateTexture, CreateTextureResult, DestroyTexture, UpdateTexture};
    use crate::runtime::surface::boot_offscreen;
    use crate::{TextureSampling, TextureUsage};

    /// The layer ceiling the GPU-free tests pass where the handler passes
    /// the device's.
    const MAX_LAYERS: u32 = 8;

    fn array(side: u32, layers: u32, mips: Mips) -> CreateTextureArray {
        CreateTextureArray { format: TextureFormat::Rgba8, side, layers, mips }
    }

    fn created(registry: &mut TextureRegistry, mail: CreateTextureArray) -> u32 {
        match registry.create_array(mail, MAX_LAYERS) {
            CreateTextureArrayResult::Ok { texture_id } => texture_id,
            CreateTextureArrayResult::Err { error } => panic!("create_texture_array must be accepted: {error}"),
        }
    }

    fn refusal(registry: &mut TextureRegistry, mail: CreateTextureArray) -> String {
        match registry.create_array(mail, MAX_LAYERS) {
            CreateTextureArrayResult::Err { error } => error,
            CreateTextureArrayResult::Ok { texture_id } => panic!("create must be refused; got texture {texture_id}"),
        }
    }

    fn write(registry: &mut TextureRegistry, texture_id: u32, layer: u32, pixels: Vec<u8>) {
        registry.write_layer(WriteTextureLayer { texture_id, layer, pixels: Blob::from(pixels) });
    }

    /// A chain over a side that is not a power of two halves and rounds
    /// down: side 5 has levels 5, 2 and 1. The named bug: a level count
    /// or a level side rounded up (levels 5, 3, 2, 1), which admits a
    /// blob whose level offsets the upload then reads past.
    #[test]
    fn a_chain_over_an_odd_side_rounds_each_level_down() {
        assert_eq!(level_sides(5, Mips::Chain).collect::<Vec<_>>(), [5, 2, 1]);
        assert_eq!(layer_bytes(TextureFormat::Rgba8, 5, Mips::Chain), Some((25 + 4 + 1) * 4));
        assert_eq!(layer_bytes(TextureFormat::Rgba8, 5, Mips::Base), Some(25 * 4));
        assert_eq!(layer_bytes(TextureFormat::R16Float, 4, Mips::Chain), Some((16 + 4 + 1) * 2));
        assert_eq!(layer_bytes(TextureFormat::Rgba16Float, u32::MAX, Mips::Chain), None);
    }

    /// Each refusal class replies its own reason and consumes no id. The
    /// named bugs: an array past a device limit reaching
    /// `create_texture`, where it is a device validation error and no
    /// reply; and a refused create burning an id, so ids stop being
    /// dense over accepted textures.
    #[test]
    fn each_create_refusal_has_its_own_reason_and_consumes_no_id() {
        let mut registry = TextureRegistry::new();
        let max_dimension = render_limits().max_texture_dimension_2d;

        let reasons = [
            refusal(&mut registry, array(0, 2, Mips::Base)),
            refusal(&mut registry, array(4, 0, Mips::Base)),
            refusal(&mut registry, array(max_dimension + 1, 2, Mips::Base)),
            refusal(&mut registry, array(4, MAX_LAYERS + 1, Mips::Base)),
        ];

        assert!(reasons[0].contains("side is zero"), "got: {}", reasons[0]);
        assert!(reasons[1].contains("zero layers"), "got: {}", reasons[1]);
        assert!(reasons[2].contains("max_texture_dimension_2d"), "got: {}", reasons[2]);
        assert!(reasons[3].contains("max_texture_array_layers"), "got: {}", reasons[3]);
        assert_eq!(registry.ids.peek(), Some(0), "a refused create must not consume an id");
        assert!(registry.arrays.is_empty(), "a refused create must stage nothing");

        let accepted = created(&mut registry, array(4, MAX_LAYERS, Mips::Base));
        assert_eq!(accepted, 0, "the layer ceiling itself is admitted, under the first id");
    }

    /// A plain texture and an array draw ids from one sequence, and each
    /// verb reaches only the map it belongs to. The named bug: two maps
    /// with two sequences handing out the same id, so a program binding
    /// or a destroy names two resources at once.
    #[test]
    fn a_texture_and_an_array_never_share_an_id() {
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
        let array_id = created(&mut registry, array(1, 2, Mips::Base));
        assert_ne!(plain_id, array_id, "the two maps must not hand out the same id");

        write(&mut registry, array_id, 0, vec![7; 4]);
        registry.update(UpdateTexture { texture_id: array_id, x: 0, y: 0, width: 1, height: 1, pixels: vec![1; 4] });
        let staged = &registry.arrays[&array_id];
        assert_eq!(
            staged.written[0].as_ref().and_then(Blob::contiguous),
            Some(&[7u8; 4][..]),
            "update_texture naming an array must leave its layer untouched",
        );
        assert_eq!(registry.entries[&plain_id].pixels.bytes(), [9; 4], "and must not reach the plain texture either");

        registry.destroy(DestroyTexture { texture_id: array_id });
        assert!(!registry.arrays.contains_key(&array_id), "destroy_texture releases an array");
        assert!(registry.entries.contains_key(&plain_id), "and leaves the plain texture registered");
    }

    /// Each write refusal class leaves the layer unwritten and clean. The
    /// named bug: a blob of the wrong length reaching `write_texture`,
    /// a device validation error that drops the whole frame. A blob
    /// holding only the base level of a `Chain` array is the case a
    /// length check against the base level alone lets through.
    #[test]
    fn a_refused_write_leaves_the_layer_unwritten_and_clean() {
        let mut registry = TextureRegistry::new();
        let CreateTextureResult::Ok { texture_id: plain_id } = registry.create(CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Nearest,
            usage: TextureUsage::Sampled,
            pixels: Blob::from(vec![9; 16]),
        }) else {
            panic!("plain create accepted");
        };
        let array_id = created(&mut registry, array(2, 2, Mips::Chain));
        let whole_chain = (4 + 1) * 4;

        write(&mut registry, array_id + 1, 0, vec![1; whole_chain]);
        write(&mut registry, plain_id, 0, vec![1; whole_chain]);
        write(&mut registry, array_id, 2, vec![1; whole_chain]);
        write(&mut registry, array_id, 0, vec![1; 16]);
        write(&mut registry, array_id, 0, vec![1; whole_chain + 1]);

        let staged = &registry.arrays[&array_id];
        assert!(staged.written.iter().all(Option::is_none), "no refused write may store its blob");
        assert_eq!(staged.dirty, [false, false], "no refused write may dirty a layer");
        assert_eq!(registry.entries[&plain_id].pixels.bytes(), [9; 16], "a layer write must not reach a plain texture");

        write(&mut registry, array_id, 1, vec![1; whole_chain]);
        let staged = &registry.arrays[&array_id];
        assert_eq!(staged.dirty, [false, true], "an accepted write dirties its own layer and no other");
    }

    /// A device replacement drops the realization and re-uploads what
    /// was written. The named bugs: a realization kept across the
    /// replacement, which binds a texture of the dead device; and an
    /// unwritten layer marked dirty, which uploads from a blob that does
    /// not exist.
    #[test]
    fn device_invalidation_marks_written_layers_for_upload_and_no_others() {
        if !has_wgpu_adapter() {
            return;
        }
        let booted = boot_offscreen(None);
        let mut registry = TextureRegistry::new();
        let array_id = created(&mut registry, array(2, 3, Mips::Chain));
        write(&mut registry, array_id, 0, vec![1; 20]);
        write(&mut registry, array_id, 2, vec![2; 20]);
        let staged = registry.arrays.get_mut(&array_id).expect("the array is staged");
        staged.ensure_realized(&booted.device, &booted.queue);
        assert!(staged.realized.is_some(), "precondition: the array is realized on the old device");
        assert_eq!(staged.dirty, [false, false, false], "precondition: realization uploaded every dirty layer");

        registry.invalidate_device_resources();

        let staged = registry.arrays.get_mut(&array_id).expect("device replacement must preserve the array");
        assert!(staged.realized.is_none(), "the old-device texture must be released");
        assert_eq!(staged.dirty, [true, false, true], "written layers re-upload; the unwritten one has no blob to");
        staged.ensure_realized(&booted.device, &booted.queue);
        assert_eq!(staged.dirty, [false, false, false], "the replacement realization uploads from the kept blobs");
    }
}
