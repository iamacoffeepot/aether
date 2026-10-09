//! The reference asset bundle actor (ADR-0163 §4): the pattern every bundle
//! actor is a copy of. It carries a tile in a wasm custom section, makes it
//! an engine resident in `wire`, keeps a handle and never the bytes, draws the
//! resident every frame, and destroys it in `unwire`, so the
//! loaded-component census and the resident-asset census stay the same
//! question. The engine does not enforce that symmetry; this actor shows it by
//! example, and `aether-render`'s `asset_resident_scenario` proves it.
//!
//! The tile is a fixed 16x16 RGBA8 checkerboard drawn as one screen-space quad
//! in the top-left corner. A raw-pixel asset carries no header, so the
//! dimensions are constants here, and the actor links no image decoder.

// Handler payloads follow the by-value dispatch ABI even when the body only
// borrows their fields.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::{ActorInitError, Assets, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_data::Blob;
use aether_kinds::{QuadSpace, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_math::Rgba;
use aether_render::{
    CreateTexture, CreateTextureResult, DestroyTexture, DrawTexturedQuads, QuadBlend, RenderCapability, TextureFormat,
    TextureSampling, TextureUsage, TexturedQuad,
};

/// The asset's fixed width, in pixels.
const TILE_WIDTH: u32 = 16;
/// The asset's fixed height, in pixels.
const TILE_HEIGHT: u32 = 16;

/// The `aether.asset.<name>` section suffix `wire` pulls, the string
/// `export_asset!` keys the section on.
const TILE_ASSET_NAME: &str = "tile.rgba";

/// On-screen size the tile draws at, in window pixels. Larger than the source
/// so the resident is easy to see in a capture.
const DRAW_SIZE_PIXELS: f32 = 128.0;

// ADR-0163 §2: embed the tile in the `aether.asset.tile.rgba` custom section.
// The path resolves relative to this source file.
aether_actor::export_asset!("tile.rgba");

/// Where the embedded tile stands.
enum Tile {
    /// No texture of this actor's is resident: the upload has not answered,
    /// was refused, or the texture was destroyed.
    Absent,
    /// The texture the upload answered with is resident, and `unwire`
    /// destroys it.
    Resident { texture_id: u32 },
}

/// Makes its embedded tile an engine resident, draws it every frame, and
/// destroys it on teardown.
///
/// # Agent
/// Loads with no config and needs no follow-up mail: `wire` uploads the
/// embedded tile as a texture and the tick handler draws it in the top-left
/// corner from the next frame on. Dropping the component runs `unwire`, which
/// destroys the texture.
pub struct AssetResident {
    tile: Tile,
}

#[actor(root, depends(LifecycleCapability, RenderCapability))]
impl WasmActor for AssetResident {
    const NAMESPACE: &'static str = "test.asset_resident";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(AssetResident { tile: Tile::Absent })
    }

    /// Pull the embedded tile from the instance's own module and mail
    /// `aether.render.create_texture` with its pixels. The bytes are consumed
    /// here; only the `texture_id` of the reply survives. A missing asset or a
    /// byte length that does not match the tile warn-logs and leaves the actor
    /// with no resident.
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.subscribe::<LifecycleCapability, Tick>();

        let Some(pixels) = ctx.asset(TILE_ASSET_NAME) else {
            tracing::warn!(
                target: "aether_test_fixture_asset_resident",
                asset = TILE_ASSET_NAME,
                "embedded tile asset not found in the module; nothing to make resident",
            );
            return Ok(());
        };
        let expected = TILE_WIDTH as usize * TILE_HEIGHT as usize * TextureFormat::Rgba8.bytes_per_pixel();
        if pixels.len() != expected {
            tracing::warn!(
                target: "aether_test_fixture_asset_resident",
                asset = TILE_ASSET_NAME,
                got = pixels.len(),
                expected,
                "embedded tile has an unexpected byte length; skipping texture upload",
            );
            return Ok(());
        }

        ctx.send::<RenderCapability>(&CreateTexture {
            width: TILE_WIDTH,
            height: TILE_HEIGHT,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: Blob::from(pixels),
        });
        Ok(())
    }

    /// Symmetric teardown: destroy exactly the resident `wire` created. The
    /// author upholds this symmetry; the engine does not enforce it.
    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        match self.tile {
            Tile::Resident { texture_id } => ctx.send::<RenderCapability>(&DestroyTexture { texture_id }),
            Tile::Absent => {}
        }

        self.tile = Tile::Absent;
    }

    /// Record the `texture_id` the render cap assigned. An `Err` (a headless
    /// chassis, or a rejected upload) warn-logs and leaves the tile absent
    /// rather than drawing a dangling id.
    #[handler::response]
    fn on_create_texture_result(&mut self, _ctx: &mut WasmCtx<'_>, result: CreateTextureResult) {
        match result {
            CreateTextureResult::Ok { texture_id } => self.tile = Tile::Resident { texture_id },
            CreateTextureResult::Err { error } => {
                tracing::warn!(
                    target: "aether_test_fixture_asset_resident",
                    %error,
                    "create_texture failed; the tile stays absent",
                );
            }
        }
    }

    /// Once the tile is resident, resend its `draw_textured_quads` batch every
    /// frame (immediate mode, ADR-0105): the quad vanishes the frame a send is
    /// skipped.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler::event]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let Tile::Resident { texture_id } = self.tile else {
            return;
        };
        ctx.send::<RenderCapability>(&DrawTexturedQuads {
            texture_id,
            blend: QuadBlend::Straight,
            space: QuadSpace::Screen,
            clip: None,
            quads: vec![TexturedQuad {
                x: 0.0,
                y: 0.0,
                width: DRAW_SIZE_PIXELS,
                height: DRAW_SIZE_PIXELS,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                tint: Rgba::WHITE,
            }],
        });
    }
}
