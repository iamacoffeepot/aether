//! The [`HeadlessRenderCapability`] runtime half (ADR-0122 identity/runtime
//! split). Nested under the `runtime` directory so the one `mod runtime;`
//! gate in the crate root covers it; the identity ZST lives in the
//! crate-root `headless` module, always-on. Unlike the GPU-bound
//! [`crate::RenderCapability`], the headless companion never names wgpu, so
//! this module compiles on a no-GPU headless `runtime` build.

use aether_actor::runtime;

use aether_kinds::{CaptureFrame, CaptureFrameResult};

use aether_substrate::Manual;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::headless::HeadlessRenderCapability;
use crate::{
    CreateTexture, CreateTextureResult, DestroyTexture, DrawMaterialCoverage, DrawMaterialTextured, DrawSolidQuads,
    DrawTexturedQuads, DrawTriangle, ProgramDestroy, ProgramDispatch, ProgramRegister, ProgramRegisterResult,
    UpdateTexture, ViewProjection,
};

/// `HeadlessRenderCapability` runtime state, which is nothing at all — the
/// headless cap replies `Err` to the GPU-bound kinds (`CaptureFrame` /
/// `CreateTexture`) and no-ops the accumulator kinds, and each of those
/// answers through its own inbound rather than through a handle held here.
/// The addressing identity is the distinct ZST
/// [`HeadlessRenderCapability`]. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
pub struct HeadlessRenderCapabilityState;

#[runtime]
impl NativeActor for HeadlessRenderCapability {
    /// The runtime state this identity boots into (ADR-0122 split) —
    /// stateless, since every handler answers through its own inbound.
    type State = HeadlessRenderCapabilityState;

    type Config = ();

    const NAMESPACE: &'static str = "aether.render";

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeadlessRenderCapabilityState, BootError> {
        Ok(HeadlessRenderCapabilityState)
    }

    /// `DrawTriangle` lands here as a no-op so headless boots of
    /// desktop-designed components (which emit `DrawTriangle` every
    /// tick) don't trip the unknown-mailbox warn path.
    #[handler::single]
    fn on_draw_triangle(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mails: &[DrawTriangle]) {}

    /// `ViewProjection` lands here as a no-op for the same reason as
    /// `on_draw_triangle` — desktop-designed components publish
    /// `aether.view_projection` every tick.
    #[handler::single]
    fn on_camera(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ViewProjection) {}

    /// `CaptureFrame` replies `Err` inline so MCP `capture_frame`
    /// fails fast on headless instead of hanging on a reply that
    /// never comes. Mirrors ADR-0035 §Consequences fail-fast shape
    /// for `set_window_mode`.
    ///
    /// Through the inbound guard rather than the hub outbound, which is
    /// what made the fail-fast a hang in practice: an RPC `Call` names the
    /// rpc server's own mailbox as its reply target, and
    /// `HubOutbound::send_reply` answers only `Session` / `EngineMailbox`
    /// senders — it drops a `Component` one and returns `false`
    /// (iamacoffeepot/aether#4341).
    #[handler::manual]
    fn on_capture_frame(_state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: CaptureFrame) {
        ctx.take_inbound()
            .reply(&CaptureFrameResult::Err { error: "unsupported on headless chassis — no GPU".to_owned() });
    }

    /// `CreateTexture` replies `Err` so an agent that creates a texture
    /// against a headless chassis fails fast instead of waiting on a reply
    /// that never comes (ADR-0105). Declared `#[handler::single]` with a
    /// returned reply — matching the pumped [`crate::RenderCapability`]'s
    /// `create_texture` declaration so live `describe_handlers` reports a
    /// single deduped row set for `aether.render`.
    #[handler::single]
    fn on_create_texture(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: CreateTexture,
    ) -> CreateTextureResult {
        CreateTextureResult::Err { error: "unsupported on headless chassis — no GPU".to_owned() }
    }

    /// `UpdateTexture` lands here as a no-op so desktop-designed
    /// components running on headless don't trip the unknown-mailbox
    /// warn path — mirrors `on_draw_triangle`.
    #[handler::single]
    fn on_update_texture(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: UpdateTexture) {}

    /// `DestroyTexture` lands here as a no-op so desktop-designed
    /// components running on headless don't trip the unknown-mailbox
    /// warn path — mirrors `on_update_texture`.
    #[handler::single]
    fn on_destroy_texture(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DestroyTexture) {}

    /// `DrawTexturedQuads` lands here as a no-op for the same reason
    /// as `on_update_texture`.
    #[handler::single]
    fn on_draw_textured_quads(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawTexturedQuads) {}

    /// `DrawSolidQuads` lands here as a no-op for the same reason
    /// as `on_draw_textured_quads`.
    #[handler::single]
    fn on_draw_solid_quads(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawSolidQuads) {}

    /// `DrawMaterialTextured` lands here as a no-op for the same
    /// reason as `on_draw_textured_quads`.
    #[handler::single]
    fn on_draw_material_textured(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawMaterialTextured) {}

    /// `DrawMaterialCoverage` lands here as a no-op for the same
    /// reason as `on_draw_textured_quads`.
    #[handler::single]
    fn on_draw_material_coverage(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawMaterialCoverage) {}

    /// `ProgramRegister` replies `Err` so an agent registering an
    /// authored render program against a headless chassis fails fast
    /// instead of waiting on a reply that never comes (ADR-0170) —
    /// mirrors `on_create_texture`.
    #[handler::single]
    fn on_program_register(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: ProgramRegister,
    ) -> ProgramRegisterResult {
        ProgramRegisterResult::Err { reason: "unsupported on headless chassis — no GPU".to_owned() }
    }

    /// `ProgramDispatch` lands here as a no-op (ADR-0170) for the same
    /// reason as `on_draw_textured_quads` — fire-and-forget kinds are
    /// absorbed, not failed.
    #[handler::single]
    fn on_program_dispatch(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ProgramDispatch) {}

    /// `ProgramDestroy` lands here as a no-op (ADR-0170) for the same
    /// reason as `on_program_dispatch`.
    #[handler::single]
    fn on_program_destroy(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ProgramDestroy) {}
}

#[cfg(all(test, feature = "runtime"))]
mod headless_tests {
    use std::sync::Arc;

    use super::*;
    use crate::{TextureFormat, TextureSampling, TextureUsage};
    use aether_data::MailboxId;
    use aether_substrate::actor::native::NativeCtx;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::testing::test_mailer_and_rx;

    /// ADR-0105: `create_texture` against a headless chassis replies
    /// `Err` (fail-fast, no GPU) rather than hanging on a reply that
    /// never comes — mirrors `capture_frame`'s headless shape. The handler
    /// is `#[handler::single]` with a returned reply (aligning the declared
    /// `create_texture` row with the pumped runtime), so the test asserts on
    /// the returned value directly.
    #[test]
    fn headless_create_texture_replies_err() {
        let (mailer, _rx) = test_mailer_and_rx();
        let mut state = HeadlessRenderCapabilityState;
        let transport = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0)));
        let mut ctx =
            NativeCtx::new(&transport, aether_data::Source::NONE, aether_data::MailId::NONE, aether_data::MailId::NONE);
        let result = HeadlessRenderCapability::on_create_texture(
            &mut state,
            &mut ctx,
            CreateTexture {
                width: 2,
                height: 2,
                format: TextureFormat::Rgba8,
                sampling: TextureSampling::Linear,
                usage: TextureUsage::Sampled,
                pixels: vec![0u8; 16],
            },
        );
        match result {
            CreateTextureResult::Err { error } => {
                assert!(
                    error.contains("headless"),
                    "headless create_texture error should name the chassis; got {error}",
                );
            }
            CreateTextureResult::Ok { .. } => {
                panic!("headless create_texture must reply Err, not assign an id")
            }
        }
    }
}
