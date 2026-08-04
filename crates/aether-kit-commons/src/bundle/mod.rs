// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the decoded payload
// and hands it off, so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

//! The reference asset bundle (ADR-0163 §4) — the pattern-setter every
//! future bundle actor is a copy of. It bakes the whole residency
//! lifecycle into the smallest actor that still demonstrates all of it:
//! a bundle carries payload bytes in a wasm custom section, transforms
//! them into an engine resident inside the load window, keeps a handle
//! (never the bytes), draws the resident every frame, and tears down
//! symmetrically so the loaded-component census stays exact.
//!
//! # Residency lifecycle (ADR-0163 §4, the door-and-tiers model)
//!
//! - **Cold** — the tile ships as raw RGBA8 bytes in the
//!   `aether.asset.tile.rgba` custom section, emitted by
//!   [`export_asset!`](aether_actor::export_asset). Never instantiated
//!   into linear memory; addressable only host-side at load.
//! - **The door** — `wire` is the load window. It pulls the bytes through
//!   [`AssetWindow::asset`], hands them to `aether.render.create_texture`,
//!   and keeps only the returned `texture_id` plus the fixed layout. When
//!   `wire` returns the window closes and the payload path is gone; the
//!   bytes were consumed, not retained.
//! - **Warm** — actor state holds a handle and a layout table (the
//!   [`BundleComponent`]'s `tile` field), never payload bytes.
//! - **Hot** — the engine-resident texture the `texture_id` names. The
//!   tick handler resends `aether.render.draw_textured_quads` every frame
//!   (immediate-mode, like `draw_triangle`) so the resident stays
//!   visible.
//! - **Teardown** — `unwire` destroys exactly the resident `wire` created
//!   (`aether.render.destroy_texture`), so
//!   "what components are loaded" and "what assets are resident" answer
//!   the same question. **This symmetry is the convention the reference
//!   actor enforces by example** — the engine does not enforce it, and
//!   every bundle that starts as a copy of this one inherits it.
//!
//! # Surface
//!
//! None beyond the lifecycle. The bundle has no driver mail of its own —
//! it boots, draws its tile, and tears down; there is nothing to steer at
//! runtime — so this module carries no `kinds` submodule. A real bundle
//! adds driver kinds only when its content genuinely needs runtime
//! control (the mesh viewer's `aether.kit.mesh.load` is the counter-case).
//!
//! The tile is a fixed 16×16 RGBA8 checkerboard drawn as one screen-space
//! quad in the top-left corner. Dimensions are compile-time constants
//! ([`TILE_WIDTH`] / [`TILE_HEIGHT`]) because a raw-pixel asset carries no
//! header — the reference pattern is bytes→engine-resident, not format
//! parsing, so the actor never links an image decoder.

use aether_actor::{ActorInitError, AssetWindow, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_kinds::{QuadSpace, Tick};
use aether_lifecycle::{LifecycleCapability, LifecycleMailboxExt};
use aether_math::Rgba;
use aether_render::QuadBlend;
use aether_render::{
    CreateTexture, CreateTextureResult, DestroyTexture, DrawTexturedQuads, RenderCapability, TextureFormat,
    TextureSampling, TextureUsage, TexturedQuad,
};

/// The asset's fixed width, in pixels. A raw RGBA8 tile has no header, so
/// the transforming actor supplies the dimensions the payload bytes are
/// laid out against.
pub const TILE_WIDTH: u32 = 16;
/// The asset's fixed height, in pixels.
pub const TILE_HEIGHT: u32 = 16;

/// The `aether.asset.<name>` section suffix the bundle pulls in `wire` —
/// the exact string `export_asset!` keyed the section on, matched against
/// the catalog by [`AssetWindow::asset`].
const TILE_ASSET_NAME: &str = "tile.rgba";

/// On-screen size the tile draws at, in window pixels. Larger than the
/// 16×16 source so the resident is easy to see in a capture; the texture
/// samples its full `[0,1]²` uv range, magnified.
const DRAW_SIZE_PIXELS: f32 = 128.0;

// ADR-0163 §2: embed the tile in the `aether.asset.tile.rgba` custom
// section. The path resolves relative to this source file, so the bytes
// are `src/bundle/tile.rgba`. Emitted on the wasm build; on the host
// rlib build it reduces to a compile-checked `include_bytes!` const.
aether_actor::export_asset!("tile.rgba");

/// The warm-tier state a resident tile survives the load window as: the
/// engine handle plus the fixed layout it draws under. Payload bytes are
/// deliberately absent — they were consumed in `wire` and never kept.
#[derive(Debug, Clone, Copy)]
struct ResidentTile {
    /// The `texture_id` `aether.render.create_texture` assigned. The one
    /// handle `unwire` must destroy for the census to stay exact.
    texture_id: u32,
}

/// The reference asset bundle actor. Carries one tile asset, makes it an
/// engine resident in the load window, draws it every frame, and destroys
/// it on teardown.
///
/// # Agent
/// Loads with no config and needs no follow-up mail: `wire` pulls the
/// embedded tile, uploads it as a texture, and the tick handler draws it
/// in the top-left corner from the next frame on. Dropping the component
/// (or a `replace_component`) runs `unwire`, which destroys the texture —
/// the loaded-component list is therefore an exact census of resident
/// tiles. There is no driver surface; the bundle is a fixed lifecycle
/// demonstration, not a configurable widget.
pub struct BundleComponent {
    /// `None` until `create_texture` replies `Ok`; the tick handler draws
    /// nothing until then, and `unwire` has nothing to destroy.
    tile: Option<ResidentTile>,
}

#[actor]
impl WasmActor for BundleComponent {
    const NAMESPACE: &'static str = "aether.kit.bundle";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(BundleComponent { tile: None })
    }

    /// The load window (ADR-0163 §4). Pulls the embedded tile through the
    /// asset window — the only place the payload bytes are reachable —
    /// and starts its transform into an engine resident by mailing
    /// `aether.render.create_texture` with the raw pixels. The bytes are
    /// consumed here and never stored; only the eventual `texture_id`
    /// survives the window (captured in [`on_create_texture_result`]).
    /// Subscribes `Tick` so the draw loop runs every frame.
    ///
    /// A missing asset (the name doesn't match the catalog) or a byte
    /// length that doesn't match `TILE_WIDTH × TILE_HEIGHT × 4` warn-logs
    /// and leaves the actor tile-less — a loud, load-time failure surface,
    /// exactly where ADR-0163 wants asset failures to land.
    ///
    /// [`on_create_texture_result`]: BundleComponent::on_create_texture_result
    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        ctx.actor::<LifecycleCapability>().subscribe::<Tick>();

        let Some(pixels) = ctx.asset(TILE_ASSET_NAME) else {
            tracing::warn!(
                target: "aether_kit_commons",
                asset = TILE_ASSET_NAME,
                "bundle: embedded tile asset not found in the load window; nothing to make resident",
            );
            return;
        };
        let expected = TILE_WIDTH as usize * TILE_HEIGHT as usize * TextureFormat::Rgba8.bytes_per_pixel();
        if pixels.len() != expected {
            tracing::warn!(
                target: "aether_kit_commons",
                asset = TILE_ASSET_NAME,
                got = pixels.len(),
                expected,
                "bundle: embedded tile has an unexpected byte length; skipping texture upload",
            );
            return;
        }

        ctx.actor::<RenderCapability>().send(&CreateTexture {
            width: TILE_WIDTH,
            height: TILE_HEIGHT,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels,
        });
    }

    /// Symmetric teardown (ADR-0163 §4). Destroys exactly the resident
    /// `wire` created — the fire-and-forget `aether.render.destroy_texture`
    /// counterpart to the `create_texture` above — so the resident dies
    /// with the component and the census stays exact. Taking the handle
    /// out of state makes the teardown idempotent. **Upholding this
    /// symmetry is the author's job; the engine does not enforce it.**
    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        if let Some(tile) = self.tile.take() {
            ctx.actor::<RenderCapability>().send(&DestroyTexture { texture_id: tile.texture_id });
        }
    }

    /// Capture the `texture_id` the render cap assigned, promoting the
    /// tile from warm (bytes-in-flight) to hot (an engine resident the
    /// draw loop can name). The reply routes back to this mailbox on the
    /// chain the `wire` `create_texture` send started. An `Err` — a
    /// headless chassis, or a rejected upload — warn-logs and leaves the
    /// actor tile-less rather than drawing a dangling id.
    #[handler::single]
    fn on_create_texture_result(&mut self, _ctx: &mut WasmCtx<'_>, result: CreateTextureResult) {
        match result {
            CreateTextureResult::Ok { texture_id } => {
                self.tile = Some(ResidentTile { texture_id });
            }
            CreateTextureResult::Err { error } => {
                tracing::warn!(
                    target: "aether_kit_commons",
                    %error,
                    "bundle: create_texture failed; the tile stays non-resident",
                );
            }
        }
    }

    /// Steady state. Once the tile is resident, resend its
    /// `draw_textured_quads` batch every frame — the immediate-mode
    /// contract (ADR-0105), the same as `draw_triangle`: the quad vanishes
    /// the frame a send is skipped. One screen-space quad in the top-left
    /// corner, sampling the whole texture, drawn unmodified (`Rgba::WHITE`
    /// tint). Does nothing until `create_texture` has replied.
    ///
    /// # Agent
    /// Tick-driven; not useful to send manually.
    #[handler::single]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, _tick: Tick) {
        let Some(tile) = self.tile else {
            return;
        };
        ctx.actor::<RenderCapability>().send(&DrawTexturedQuads {
            texture_id: tile.texture_id,
            blend: QuadBlend::Straight,
            space: QuadSpace::Screen,
            clip: None,
            quads: alloc::vec![TexturedQuad {
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
