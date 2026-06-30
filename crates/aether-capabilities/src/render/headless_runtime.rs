//! The `HeadlessRenderCapability` runtime half (ADR-0122 identity/runtime
//! split). Compiled under the default `feature = "runtime"` gate (the
//! `mod headless_runtime;` declaration in the parent carries it) — unlike
//! the GPU-bound [`super::RenderCapability`], the headless companion has no
//! `render-runtime` dep, so its runtime half must compile on a no-GPU
//! headless `runtime` build. The substrate-typed imports are gated once by
//! this module; the `#[actor] impl` reaches the state + ctx types through
//! the single `use headless_runtime::*` glob in the parent.

// `io` is named by the parent's `init` body (`io::Error::other`); `Arc` and
// `HubOutbound` only by the state struct's field. The substrate ctx types
// the `#[actor] impl` names (`NativeActor` / `NativeCtx` / `NativeInitCtx` /
// `BootError` / `Manual` / `CaptureFrameResult`) come from the shared
// `any(render-runtime, runtime)` seam in `mod.rs`, not from here, so a
// desktop build doesn't re-export them through two globs.
pub(super) use std::io;

use std::sync::Arc;

use aether_substrate::mail::outbound::HubOutbound;

// The moved `#[runtime] impl NativeActor for HeadlessRenderCapability` body
// names the `#[runtime]` attribute, the cap kinds (the drawing kinds via the
// parent's `kinds` re-export, `CaptureFrame` / `CaptureFrameResult` from
// `aether_kinds`), and the substrate ctx types it previously reached through
// the parent's shared `any(render-runtime, runtime)` seam — now sourced here.
use aether_actor::runtime;

use aether_kinds::{CaptureFrame, CaptureFrameResult};

use aether_substrate::Manual;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use super::{
    Camera, CreateTexture, CreateTextureResult, DrawSolidQuads, DrawTexturedQuads, DrawTriangle,
    HeadlessRenderCapability, UpdateTexture,
};

/// `HeadlessRenderCapability` runtime state. Holds only the [`HubOutbound`]
/// captured at init — the headless cap replies `Err` to the GPU-bound
/// kinds (`CaptureFrame` / `CreateTexture`) and no-ops the accumulator
/// kinds, so it needs no handles. The addressing identity is the distinct
/// ZST [`super::HeadlessRenderCapability`]. Living in this private module
/// keeps it `pub`-enough to satisfy the `NativeActor::State` interface
/// without exposing it as crate-public API.
pub struct HeadlessRenderCapabilityState {
    pub(super) outbound: Arc<HubOutbound>,
}

#[runtime]
impl NativeActor for HeadlessRenderCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// captured `HubOutbound` the `Err`/no-op handlers reply through.
    type State = HeadlessRenderCapabilityState;

    type Config = ();

    const NAMESPACE: &'static str = "aether.render";

    fn init(
        _config: (),
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<HeadlessRenderCapabilityState, BootError> {
        let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "HubOutbound must be wired on Mailer before \
                     HeadlessRenderCapability::init (chassis main connects the hub before \
                     the Builder chain)",
            )))
        })?;
        Ok(HeadlessRenderCapabilityState { outbound })
    }

    /// `DrawTriangle` lands here as a no-op so headless boots of
    /// desktop-designed components (which emit `DrawTriangle` every
    /// tick) don't trip the unknown-mailbox warn path.
    #[handler]
    fn on_draw_triangle(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mails: &[DrawTriangle],
    ) {
    }

    /// `Camera` lands here as a no-op for the same reason as
    /// `on_draw_triangle` — desktop-designed components publish
    /// `aether.camera` every tick.
    #[handler]
    fn on_camera(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: Camera) {}

    /// `CaptureFrame` replies `Err` inline so MCP `capture_frame`
    /// fails fast on headless instead of hanging on a reply that
    /// never comes. Mirrors ADR-0035 §Consequences fail-fast shape
    /// for `set_window_mode`.
    #[handler::manual]
    fn on_capture_frame(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        _mail: CaptureFrame,
    ) {
        state.outbound.send_reply(
            ctx.reply_target(),
            &CaptureFrameResult::Err {
                error: "unsupported on headless chassis — no GPU".to_owned(),
            },
        );
    }

    /// `CreateTexture` replies `Err` inline so an agent that creates a
    /// texture against a headless chassis fails fast instead of
    /// waiting on a reply that never comes — same fail-fast shape as
    /// `on_capture_frame` (ADR-0105).
    #[handler::manual]
    fn on_create_texture(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Manual>,
        _mail: CreateTexture,
    ) {
        state.outbound.send_reply(
            ctx.reply_target(),
            &CreateTextureResult::Err {
                error: "unsupported on headless chassis — no GPU".to_owned(),
            },
        );
    }

    /// `UpdateTexture` lands here as a no-op so desktop-designed
    /// components running on headless don't trip the unknown-mailbox
    /// warn path — mirrors `on_draw_triangle`.
    #[handler]
    fn on_update_texture(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: UpdateTexture) {
    }

    /// `DrawTexturedQuads` lands here as a no-op for the same reason
    /// as `on_update_texture`.
    #[handler]
    fn on_draw_textured_quads(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: DrawTexturedQuads,
    ) {
    }

    /// `DrawSolidQuads` lands here as a no-op for the same reason
    /// as `on_draw_textured_quads`.
    #[handler]
    fn on_draw_solid_quads(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: DrawSolidQuads,
    ) {
    }
}

#[cfg(all(test, feature = "runtime"))]
mod headless_tests {
    use super::*;
    use crate::test_chassis::{decode_reply, test_mailer_and_rx};
    use aether_data::{MailboxId, Source, SourceAddr};
    use aether_data::{SessionToken, Uuid};
    use aether_substrate::actor::native::NativeCtx;
    use aether_substrate::actor::native::binding::NativeBinding;

    /// ADR-0105: `create_texture` against a headless chassis replies
    /// `Err` (fail-fast, no GPU) rather than hanging on a reply that
    /// never comes — mirrors `capture_frame`'s headless shape.
    #[test]
    fn headless_create_texture_replies_err() {
        let (mailer, rx) = test_mailer_and_rx();
        let outbound = mailer
            .outbound()
            .cloned()
            .expect("test_mailer_and_rx wires a loopback outbound");
        let mut state = HeadlessRenderCapabilityState { outbound };
        let transport = Arc::new(NativeBinding::new_for_test(
            Arc::clone(&mailer),
            MailboxId(0),
        ));
        let mut ctx = NativeCtx::new_dispatching(
            &transport,
            Source::to(SourceAddr::Session(SessionToken(Uuid::nil()))),
            aether_data::MailId::NONE,
            aether_data::MailId::NONE,
        );
        HeadlessRenderCapability::on_create_texture(
            &mut state,
            &mut ctx,
            CreateTexture {
                width: 2,
                height: 2,
                pixels: vec![0u8; 16],
            },
        );
        match decode_reply::<CreateTextureResult>(&rx) {
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
