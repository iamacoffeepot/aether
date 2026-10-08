//! Texture arrays in the `aether.render` texture registry (ADR-0246
//! decision 6): a fixed number of square layers, each written in place.
//!
//! An array shares the registry's id sequence with the plain textures
//! and the volumes and lives in its own map, so an id names one of them
//! and every reader of the plain map already treats an array id as
//! unknown. The
//! received blob of each written layer is the source of truth, as staged
//! pixels are for a plain texture, and the wgpu texture is rebuilt from
//! the blobs on a replacement device.
//!
//! An array reaches the device a layer at a time (ADR-0251). The upload
//! queue uploads one layer per piece, staged layers first, and then
//! clears each layer no mail wrote by writing zeros to it, so the array
//! is whole on the device before anything draws through it. The record
//! path's first use still uploads every staged layer at once, and leaves
//! the unwritten ones to the queue.

use std::ops::Range;

use aether_data::Blob;

use super::surface::render_limits;
use super::texture::{TextureRegistry, wgpu_texture_format};
use super::upload::Piece;
use crate::kinds::{CreateTextureArray, CreateTextureArrayResult, WriteTextureLayer};
use crate::{Mips, TextureFormat};

/// A texture array registered via `create_texture_array`: its fixed
/// shape, what each layer holds, and the GPU texture once a device has
/// made it. `contents` holds one entry per layer, `layers` long.
pub struct StagedTextureArray {
    pub format: TextureFormat,
    pub side: u32,
    pub layers: u32,
    pub mips: Mips,
    pub contents: Vec<Layer>,
    pub realized: Option<wgpu::Texture>,
}

/// What one layer of an array holds, and whether the current device's
/// texture holds it too. A device replacement moves `Uploaded` back to
/// `Staged` and `Cleared` back to `Unwritten`.
pub enum Layer {
    /// No mail wrote it, and this device's texture has not had it
    /// cleared.
    Unwritten,
    /// No mail wrote it, and the upload queue wrote zeros to it on this
    /// device's texture.
    Cleared,
    /// A blob this device's texture has not received.
    Staged(Blob),
    /// A blob this device's texture holds.
    Uploaded(Blob),
}

/// The one layer the upload queue's next piece of an array is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayerPiece {
    /// Upload the blob staged for `layer`.
    Write { layer: usize },
    /// Write zeros to `layer`, which no mail has written.
    Clear { layer: usize },
    /// Every layer is uploaded or cleared.
    Nothing,
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
    /// Drop the realization built against the current device. Every
    /// written layer is staged for upload to the replacement, and every
    /// layer the queue cleared is unwritten again: the replacement
    /// device's texture has had neither.
    pub fn invalidate_device_resources(&mut self) {
        self.realized = None;
        for layer in &mut self.contents {
            let lost = match layer {
                Layer::Staged(pixels) | Layer::Uploaded(pixels) => Layer::Staged(pixels.clone()),
                Layer::Unwritten | Layer::Cleared => Layer::Unwritten,
            };
            *layer = lost;
        }
    }

    /// The layer the upload queue's next piece is: the first layer with a
    /// staged blob, and only when none is staged the first unwritten one.
    /// Staged content goes ahead of clears because a draw may be waiting
    /// on it, and nothing waits on a layer no mail wrote.
    #[must_use]
    pub fn next_piece(&self) -> LayerPiece {
        let staged = self.contents.iter().position(|layer| matches!(layer, Layer::Staged(_)));
        let unwritten = self.contents.iter().position(|layer| matches!(layer, Layer::Unwritten));

        match (staged, unwritten) {
            (Some(layer), _) => LayerPiece::Write { layer },
            (None, Some(layer)) => LayerPiece::Clear { layer },
            (None, None) => LayerPiece::Nothing,
        }
    }

    /// Whether the device holds the whole array: its texture exists and
    /// every layer is uploaded or cleared on it (ADR-0251 section 3). An
    /// array a draw realized is not resident while a layer no mail wrote
    /// is still to be cleared.
    #[must_use]
    pub fn is_resident(&self) -> bool {
        let realized = self.realized.is_some();
        let complete = self.next_piece() == LayerPiece::Nothing;

        realized && complete
    }

    /// What the array holds on the device once resident: every layer's
    /// bytes, written or not, the count its memory charge uses.
    ///
    /// # Panics
    /// Panics if a layer's byte count overflows, fail-fast per ADR-0063:
    /// `create_array` refuses such an array before it stages it.
    #[must_use]
    pub fn device_bytes(&self) -> u64 {
        layer_bytes(self.format, self.side, self.mips)
            .expect("create_texture_array refuses an overflowing layer")
            .saturating_mul(self.layers as usize) as u64
    }

    /// First use by the record path: create the GPU texture if this
    /// device has none and upload every staged layer, one `write_texture`
    /// per level. Runs at record time on the driver thread, where a
    /// device and queue are available. The texture is created once per
    /// device, so a later write lands in the texture a cached view
    /// already names.
    ///
    /// An unwritten layer is left unwritten, and is never marked cleared
    /// here. The record path's first use is not wgpu's first use: a
    /// program dispatched every frame with nothing to draw realizes the
    /// arrays it binds at once, while wgpu clears a never-written layer
    /// only in the first submit that draws through the array. Marking
    /// the layers cleared here would take the array off the upload queue
    /// with that clear still owed, and it would land, all layers at once,
    /// in the first drawing frame (ADR-0251 section 4). Only the queue's
    /// zero write, [`Self::upload_layer`], moves a layer to `Cleared`.
    ///
    /// # Panics
    /// Panics if a staged layer holds no contiguous blob, fail-fast per
    /// ADR-0063: `write_layer` refuses a non-contiguous one before it
    /// stages the layer.
    pub fn ensure_realized(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let texture = self.device_texture(device);
        for layer in 0..self.contents.len() {
            self.upload_staged(queue, &texture, layer);
        }
    }

    /// One piece for the upload queue's step (ADR-0251 sections 2 and 3):
    /// create the GPU texture if this device has none, then write one
    /// layer, either its staged blob or, for a layer no mail wrote, zeros
    /// taken from `zeros`. Answers `More` while any layer is still staged
    /// or unwritten, `Landed` when that write was the last, and
    /// `Resident` when there was nothing to write.
    ///
    /// A layer is cleared by writing zeros over every level of it, and
    /// not by leaving it to wgpu: a `write_texture` that covers a whole
    /// level marks that level of the layer initialized, so the first
    /// submit that draws through the array has nothing left to clear.
    ///
    /// # Panics
    /// Panics if the staged layer holds no contiguous blob, fail-fast per
    /// ADR-0063: `write_layer` refuses a non-contiguous one before it
    /// stages the layer.
    pub(super) fn upload_layer(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, zeros: &mut Vec<u8>) -> Piece {
        let texture = self.device_texture(device);
        match self.next_piece() {
            LayerPiece::Nothing => return Piece::Resident { bytes: self.device_bytes() },
            LayerPiece::Write { layer } => self.upload_staged(queue, &texture, layer),
            LayerPiece::Clear { layer } => self.clear(queue, &texture, layer, zeros),
        }

        match self.next_piece() {
            LayerPiece::Nothing => Piece::Landed { bytes: self.device_bytes() },
            LayerPiece::Write { .. } | LayerPiece::Clear { .. } => Piece::More,
        }
    }

    /// The current device's texture, created here if it has none.
    fn device_texture(&mut self, device: &wgpu::Device) -> wgpu::Texture {
        let texture = self.realized.get_or_insert_with(|| {
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

        texture.clone()
    }

    /// Upload `layer`'s blob if it is staged, and mark it uploaded. A
    /// layer in any other state is left as it is.
    fn upload_staged(&mut self, queue: &wgpu::Queue, texture: &wgpu::Texture, layer: usize) {
        let Layer::Staged(pixels) = &self.contents[layer] else {
            return;
        };
        let bytes = pixels.contiguous().expect("write_texture_layer stages only a contiguous blob");
        let uploaded = Layer::Uploaded(pixels.clone());

        self.write_levels(queue, texture, layer, |level| &bytes[level]);
        self.contents[layer] = uploaded;
    }

    /// Write zeros to every level of `layer` and mark it cleared. `zeros`
    /// is grown to the base level, the largest, when it is shorter.
    fn clear(&mut self, queue: &wgpu::Queue, texture: &wgpu::Texture, layer: usize, zeros: &mut Vec<u8>) {
        let base_level =
            level_bytes(self.format, self.side).expect("create_texture_array refuses an overflowing layer");
        if zeros.len() < base_level {
            zeros.resize(base_level, 0);
        }
        let zeros = zeros.as_slice();

        self.write_levels(queue, texture, layer, |level| &zeros[..level.len()]);
        self.contents[layer] = Layer::Cleared;
    }

    /// Write every level of `layer`, one `write_texture` per level, each
    /// covering its whole level. `level_pixels` is given a level's byte
    /// range within one layer's bytes, base level first, and answers that
    /// many bytes.
    fn write_levels<'a>(
        &self,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        layer: usize,
        level_pixels: impl Fn(Range<usize>) -> &'a [u8],
    ) {
        let bytes_per_pixel = u32::try_from(self.format.bytes_per_pixel()).expect("bytes per pixel fit u32");
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
                level_pixels(offset..end),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(level_side * bytes_per_pixel),
                    rows_per_image: Some(level_side),
                },
                wgpu::Extent3d { width: level_side, height: level_side, depth_or_array_layers: 1 },
            );
            offset = end;
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

        let array = StagedTextureArray {
            format: mail.format,
            side: mail.side,
            layers: mail.layers,
            mips: mail.mips,
            contents: (0..mail.layers).map(|_| Layer::Unwritten).collect(),
            realized: None,
        };
        // Every layer is counted, written or not: the realized array holds
        // them all on the device.
        self.arrays
            .insert(texture_id, self.memory.charged(bytes_per_layer.saturating_mul(mail.layers as usize), array));
        CreateTextureArrayResult::Ok { texture_id }
    }

    /// Replace one layer of an array with `mail.pixels`. Fire-and-forget,
    /// so every refusal warns and leaves the layer as it was. An accepted
    /// write keeps the blob and stages its layer alone; the upload is the
    /// upload queue's, or the next record's that binds the array if that
    /// comes first.
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

        array.contents[layer as usize] = Layer::Staged(pixels);
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

    /// One layer's state as a test reads it: the case, and for a layer
    /// holding a blob that blob's first byte, which the tests make
    /// distinct per write.
    #[derive(Debug, Eq, PartialEq)]
    enum State {
        Unwritten,
        Cleared,
        Staged(u8),
        Uploaded(u8),
    }

    fn states(array: &StagedTextureArray) -> Vec<State> {
        let first_byte = |pixels: &Blob| pixels.contiguous().expect("test blobs are contiguous")[0];

        array
            .contents
            .iter()
            .map(|layer| match layer {
                Layer::Unwritten => State::Unwritten,
                Layer::Cleared => State::Cleared,
                Layer::Staged(pixels) => State::Staged(first_byte(pixels)),
                Layer::Uploaded(pixels) => State::Uploaded(first_byte(pixels)),
            })
            .collect()
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
        assert_eq!(
            states(&registry.arrays[&array_id]),
            [State::Staged(7), State::Unwritten],
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

        assert_eq!(
            states(&registry.arrays[&array_id]),
            [State::Unwritten, State::Unwritten],
            "no refused write may store its blob or stage a layer",
        );
        assert_eq!(registry.entries[&plain_id].pixels.bytes(), [9; 16], "a layer write must not reach a plain texture");

        write(&mut registry, array_id, 1, vec![1; whole_chain]);
        assert_eq!(
            states(&registry.arrays[&array_id]),
            [State::Unwritten, State::Staged(1)],
            "an accepted write stages its own layer and no other",
        );
    }

    /// A device replacement drops the realization, re-uploads what was
    /// written and owes the clears again. The named bugs: a realization
    /// kept across the replacement, which binds a texture of the dead
    /// device; an unwritten layer staged, which uploads from a blob that
    /// does not exist; and a cleared layer still called cleared, though
    /// the replacement device's texture never had it cleared, so the
    /// array would be called resident with wgpu's clear still owed.
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
        let mut zeros = Vec::new();
        let staged = registry.arrays.get_mut(&array_id).expect("the array is staged");
        staged.ensure_realized(&booted.device, &booted.queue);
        assert_eq!(staged.upload_layer(&booted.device, &booted.queue, &mut zeros), Piece::Landed { bytes: 60 });
        assert!(staged.realized.is_some(), "precondition: the array is realized on the old device");
        assert_eq!(
            states(staged),
            [State::Uploaded(1), State::Cleared, State::Uploaded(2)],
            "precondition: every layer is on the old device",
        );

        registry.invalidate_device_resources();

        let staged = registry.arrays.get_mut(&array_id).expect("device replacement must preserve the array");
        assert!(staged.realized.is_none(), "the old-device texture must be released");
        assert_eq!(
            states(staged),
            [State::Staged(1), State::Unwritten, State::Staged(2)],
            "written layers re-upload from their blobs; the cleared one is owed its clear again",
        );
        staged.ensure_realized(&booted.device, &booted.queue);
        assert_eq!(
            states(staged),
            [State::Uploaded(1), State::Unwritten, State::Uploaded(2)],
            "the replacement realization uploads from the kept blobs",
        );
    }

    /// The queue's choice of layer, with no device. Layers 1 and 3 of
    /// four are written: the choice is the write of layer 1, though the
    /// unwritten layer 0 comes first by index. The named bugs: a choice
    /// by index alone, which runs clears ahead of staged content a draw
    /// may be waiting on; and a clear chosen for a layer that holds a
    /// blob, which would write zeros over it.
    #[test]
    fn written_layers_are_chosen_before_unwritten_ones_and_never_cleared() {
        let mut registry = TextureRegistry::new();
        let array_id = created(&mut registry, array(1, 4, Mips::Base));
        write(&mut registry, array_id, 1, vec![5; 4]);
        write(&mut registry, array_id, 3, vec![6; 4]);
        let staged = registry.arrays.get_mut(&array_id).expect("the array is staged");

        assert_eq!(staged.next_piece(), LayerPiece::Write { layer: 1 });

        staged.contents[1] = Layer::Uploaded(Blob::from(vec![5; 4]));
        assert_eq!(staged.next_piece(), LayerPiece::Write { layer: 3 }, "staged layer 3 goes ahead of unwritten 0");
        staged.contents[3] = Layer::Uploaded(Blob::from(vec![6; 4]));
        assert_eq!(staged.next_piece(), LayerPiece::Clear { layer: 0 }, "clears start once nothing is staged");
        staged.contents[0] = Layer::Cleared;
        assert_eq!(staged.next_piece(), LayerPiece::Clear { layer: 2 }, "an uploaded layer is never chosen to clear");
        staged.contents[2] = Layer::Cleared;
        assert_eq!(staged.next_piece(), LayerPiece::Nothing, "every layer is uploaded or cleared");
    }

    /// The queue's pieces of an array, on a device. Two of three layers
    /// are written. The named bugs: every staged layer uploaded in one
    /// piece, which puts a whole array's bytes in one frame; the array
    /// called resident with a layer still uncleared, which leaves wgpu's
    /// clear for the first drawing submit; and the texture re-created per
    /// piece, which drops the layers already written.
    #[test]
    fn one_piece_uploads_one_layer_and_the_array_is_resident_only_when_every_layer_is() {
        if !has_wgpu_adapter() {
            return;
        }
        let booted = boot_offscreen(None);
        let mut registry = TextureRegistry::new();
        let array_id = created(&mut registry, array(2, 3, Mips::Chain));
        write(&mut registry, array_id, 0, vec![1; 20]);
        write(&mut registry, array_id, 2, vec![2; 20]);
        let mut zeros = Vec::new();
        let staged = registry.arrays.get_mut(&array_id).expect("the array is staged");

        assert_eq!(staged.upload_layer(&booted.device, &booted.queue, &mut zeros), Piece::More);
        let texture = staged.realized.clone().expect("the first piece creates the texture");
        assert_eq!(states(staged), [State::Uploaded(1), State::Unwritten, State::Staged(2)]);

        assert_eq!(staged.upload_layer(&booted.device, &booted.queue, &mut zeros), Piece::More);
        assert_eq!(states(staged), [State::Uploaded(1), State::Unwritten, State::Uploaded(2)]);

        assert_eq!(staged.upload_layer(&booted.device, &booted.queue, &mut zeros), Piece::Landed { bytes: 60 });
        assert_eq!(states(staged), [State::Uploaded(1), State::Cleared, State::Uploaded(2)]);
        assert_eq!(zeros.len(), 16, "the clear writes from zeros as long as the base level, the largest");

        assert_eq!(staged.upload_layer(&booted.device, &booted.queue, &mut zeros), Piece::Resident { bytes: 60 });
        assert_eq!(staged.realized.as_ref(), Some(&texture), "every piece writes into the one texture");
    }

    /// The record path's first use of an array, on a device. One of three
    /// layers is written. The named bug: unwritten layers marked cleared
    /// at first use, which takes an array that is bound but not yet drawn
    /// off the upload queue and leaves wgpu's clear of those layers in
    /// the first submit that draws through it.
    #[test]
    fn first_use_uploads_written_layers_and_leaves_unwritten_ones_for_the_queue() {
        if !has_wgpu_adapter() {
            return;
        }
        let booted = boot_offscreen(None);
        let mut registry = TextureRegistry::new();
        let array_id = created(&mut registry, array(2, 3, Mips::Chain));
        write(&mut registry, array_id, 1, vec![3; 20]);
        let staged = registry.arrays.get_mut(&array_id).expect("the array is staged");

        staged.ensure_realized(&booted.device, &booted.queue);

        assert_eq!(states(staged), [State::Unwritten, State::Uploaded(3), State::Unwritten]);
        assert_eq!(
            staged.upload_layer(&booted.device, &booted.queue, &mut Vec::new()),
            Piece::More,
            "the queue still owes the array its two clears",
        );
    }
}
