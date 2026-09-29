//! The [`HeadlessRenderCapability`] runtime half (ADR-0122 identity/runtime
//! split). Nested under the `runtime` directory so the one `mod runtime;`
//! gate in the crate root covers it; the identity ZST lives in the
//! crate-root `headless` module, always-on. Unlike the GPU-bound
//! [`crate::RenderCapability`], the headless companion never names wgpu, so
//! this module compiles on a no-GPU headless `runtime` build.

use aether_actor::runtime;

use aether_kinds::{CaptureFrame, CaptureFrameResult};

use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::headless::HeadlessRenderCapability;
use crate::{
    CreateGeometry, CreateGeometryResult, CreateTexture, CreateTextureResult, DestroyGeometry, DestroyTexture,
    DrawMaterialCoverage, DrawMaterialTextured, DrawScreenTriangles, DrawShapes, DrawTexturedQuads, DrawTriangle,
    ProgramDestroy, ProgramDispatch, ProgramRegister, ProgramRegisterResult, ProgramTimings, ProgramTimingsResult,
    UpdateGeometry, UpdateTexture, ViewProjection,
};

/// `HeadlessRenderCapability` runtime state, which is nothing at all — the
/// headless cap replies `Err` to the GPU-bound kinds (`CaptureFrame` /
/// `CreateTexture`) and no-ops the accumulator kinds, and each of those
/// returns its reply rather than answering through a handle held here.
/// The addressing identity is the distinct ZST
/// [`HeadlessRenderCapability`]. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
pub struct HeadlessRenderCapabilityState;

/// What a headless chassis has instead of a GPU, said once: the reply every
/// GPU-bound render kind answers with, so a caller fails fast (ADR-0035
/// §Consequences) instead of waiting on a frame that never comes.
const UNAVAILABLE_ERROR: &str = "unsupported on headless chassis — no GPU";

#[runtime]
impl NativeActor for HeadlessRenderCapability {
    /// The runtime state this identity boots into (ADR-0122 split) —
    /// stateless, since every handler returns its reply.
    type State = HeadlessRenderCapabilityState;

    type Config = ();

    const NAMESPACE: &'static str = "aether.render";

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeadlessRenderCapabilityState, BootError> {
        Ok(HeadlessRenderCapabilityState)
    }

    /// `CaptureFrame` replies `Err` inline so MCP `capture_frame`
    /// fails fast on headless instead of hanging on a reply that
    /// never comes. Mirrors ADR-0035 §Consequences fail-fast shape
    /// for `set_window_mode`.
    ///
    /// The returned reply goes to the request's own reply target rather
    /// than the hub outbound, which is what made the fail-fast a hang in
    /// practice: an RPC `Call` names the rpc server's own mailbox as its
    /// reply target, and `HubOutbound::send_reply` answers only `Session` /
    /// `EngineMailbox` senders — it drops a `Component` one and returns
    /// `false` (iamacoffeepot/aether#4341). Declared `#[handler::single]`
    /// with a returned reply, matching the pumped [`crate::RenderCapability`]'s
    /// `-> Pending<CaptureFrameResult>` row so live `describe_handlers`
    /// reports one deduped `capture_frame` row for `aether.render`.
    #[handler::single]
    fn on_capture_frame(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: CaptureFrame) -> CaptureFrameResult {
        CaptureFrameResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
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
        CreateTextureResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

    /// `CreateGeometry` replies `Err` so an agent that creates a
    /// geometry against a headless chassis fails fast instead of waiting
    /// on a reply that never comes (ADR-0171) — mirrors
    /// `on_create_texture`, including the `#[handler::single]`-with-reply
    /// declaration that keeps `describe_handlers` deduped against the
    /// pumped runtime.
    #[handler::single]
    fn on_create_geometry(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: CreateGeometry,
    ) -> CreateGeometryResult {
        CreateGeometryResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

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
        ProgramRegisterResult::Err { error: UNAVAILABLE_ERROR.to_owned() }
    }

    /// `ProgramTimings` replies `Absent`, not `Err`
    /// (iamacoffeepot/aether#4423): a headless chassis has no GPU to time
    /// passes on, which is exactly the "this device cannot answer" the
    /// `Absent` arm exists to say. `Err` is reserved for a request that
    /// was wrong — an unknown program id — and no program can be
    /// registered here to be wrong about.
    #[handler::single]
    fn on_program_timings(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: ProgramTimings,
    ) -> ProgramTimingsResult {
        ProgramTimingsResult::Absent { reason: UNAVAILABLE_ERROR.to_owned() }
    }

    // The absorbed kinds. Every one of them is fire-and-forget on the pumped
    // runtime — it accumulates into a frame this chassis never records, or
    // releases a resource this chassis never realized — so dropping it here is
    // honest silence rather than a swallowed request, and answers the whole
    // reason this cap claims `aether.render` at all: a desktop-designed
    // component running headless emits these every tick, and an unclaimed
    // mailbox would warn-storm. A new fire-and-forget render verb belongs in
    // this run.

    /// `DrawTriangle` is absorbed (ADR-0066).
    #[handler::single]
    fn on_draw_triangle(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mails: &[DrawTriangle]) {}

    /// `ViewProjection` is absorbed (ADR-0066).
    #[handler::single]
    fn on_camera(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ViewProjection) {}

    /// `UpdateTexture` is absorbed (ADR-0105).
    #[handler::single]
    fn on_update_texture(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: UpdateTexture) {}

    /// `DestroyTexture` is absorbed (ADR-0105).
    #[handler::single]
    fn on_destroy_texture(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DestroyTexture) {}

    /// `UpdateGeometry` is absorbed (ADR-0171).
    #[handler::single]
    fn on_update_geometry(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: UpdateGeometry) {}

    /// `DestroyGeometry` is absorbed (ADR-0171).
    #[handler::single]
    fn on_destroy_geometry(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DestroyGeometry) {}

    /// `DrawTexturedQuads` is absorbed (ADR-0105).
    #[handler::single]
    fn on_draw_textured_quads(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawTexturedQuads) {}

    /// `DrawScreenTriangles` is absorbed (iamacoffeepot/aether#5504).
    #[handler::single]
    fn on_draw_screen_triangles(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawScreenTriangles) {}

    /// `DrawShapes` is absorbed (ADR-0213).
    #[handler::single]
    fn on_draw_shapes(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawShapes) {}

    /// `DrawMaterialTextured` is absorbed (ADR-0140).
    #[handler::single]
    fn on_draw_material_textured(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawMaterialTextured) {}

    /// `DrawMaterialCoverage` is absorbed (ADR-0140).
    #[handler::single]
    fn on_draw_material_coverage(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: DrawMaterialCoverage) {}

    /// `ProgramDispatch` is absorbed (ADR-0170).
    #[handler::single]
    fn on_program_dispatch(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ProgramDispatch) {}

    /// `ProgramDestroy` is absorbed (ADR-0170).
    #[handler::single]
    fn on_program_destroy(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ProgramDestroy) {}
}

#[cfg(all(test, feature = "runtime"))]
mod headless_tests {
    use super::*;
    use crate::{TextureFormat, TextureSampling, TextureUsage, VertexAttribute, VertexFormat};
    use aether_actor::HandlesKind;
    use aether_data::{Kind, SessionToken, Uuid};
    use aether_substrate::chassis::builder::ReplyTarget;
    use aether_substrate::testing::{boot_test_chassis_with, decode_session_reply, fresh_substrate_and_rx};

    /// Boot `HeadlessRenderCapability` the way a headless chassis composes
    /// it, send `mail` with its reply routed to a session, and decode the
    /// reply the dispatch answers with.
    fn request<K: Kind, R: Kind>(mail: &K) -> R
    where
        HeadlessRenderCapability: HandlesKind<K>,
    {
        let (registry, mailer, egress) = fresh_substrate_and_rx();
        let chassis = boot_test_chassis_with::<HeadlessRenderCapability>(&registry, &mailer, (), ());
        let reply = ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0x7045)), correlation: 1 };
        chassis.send_for_reply(chassis.actor_ref::<HeadlessRenderCapability>(), mail, reply);
        decode_session_reply(&egress)
    }

    /// ADR-0105. Catches a headless `create_texture` that assigns an id or
    /// never answers: a caller must fail fast rather than wait on a GPU the
    /// chassis does not have.
    #[test]
    fn headless_create_texture_replies_err() {
        let result: CreateTextureResult = request(&CreateTexture {
            width: 2,
            height: 2,
            format: TextureFormat::Rgba8,
            sampling: TextureSampling::Linear,
            usage: TextureUsage::Sampled,
            pixels: vec![0u8; 16],
        });

        let CreateTextureResult::Err { error } = result else {
            panic!("headless create_texture must reply Err, not assign an id");
        };
        assert!(error.contains("headless"), "headless create_texture error should name the chassis; got {error}");
    }

    /// ADR-0171. Catches a headless `create_geometry` that assigns an id or
    /// never answers, the same fail-fast as `create_texture`.
    #[test]
    fn headless_create_geometry_replies_err() {
        let result: CreateGeometryResult = request(&CreateGeometry {
            layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }],
            vertices: vec![0u8; 36],
            indices: (0u32..3).flat_map(u32::to_le_bytes).collect(),
        });

        let CreateGeometryResult::Err { error } = result else {
            panic!("headless create_geometry must reply Err, not assign an id");
        };
        assert!(error.contains("headless"), "headless create_geometry error should name the chassis; got {error}");
    }
}
