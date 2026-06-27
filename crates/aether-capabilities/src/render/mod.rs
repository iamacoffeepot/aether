//! `aether.render` cap. Owns the render mailbox surface plus the
//! driver-facing accumulator state ([`RenderHandles`]) and GPU bundle
//! ([`RenderGpu`]). Post-ADR-0082 the chassis gates frame submit on
//! settlement of the `LifecycleAdvance` chain root — render's
//! `DrawTriangle` / `aether.camera` mail are descendants of that root,
//! so they're integrated before submit without a per-mailbox drain
//! counter.
//!
//! Driver-side state (wgpu device, queue, pipeline, offscreen
//! targets, accumulator buffers) lives on [`RenderHandles`] in the
//! `pipeline` submodule. `init` publishes the bundle on the chassis's
//! exported-handle map (`ctx.publish_handle`), and the driver fetches it
//! via `DriverCtx::handle::<RenderHandles>()`. Phase 4 keeps the GPU
//! lifecycle, encoder creation, and presentation in the chassis driver —
//! this capability owns only the mail surface and accumulator state.
//!
//! The cap's drawing + texture mail kinds live in [`kinds`] (ADR-0121):
//! they ride the always-on (marker-only `render`) region so a wasm
//! guest sees the kind types for typed addressing without the
//! `render-native` GPU stack. The capture-request and `FrameCheck`
//! verification kinds stay in `aether-kinds` (consumed upstream by
//! `aether-mcp` and the substrate core), as do the `QuadSpace` /
//! `QuadScale` projection types the `aether.text` kinds share.
//!
//! The decomposition is along the cap's cohesion seams: `pipeline`
//! (GPU bundle + accumulator handles), `texture` (the texture
//! registry), `quad` (the quad-batch accumulator), and `capture`
//! (the cross-thread readback machinery).
//!
//! [`HeadlessRenderCapability`] is the chassis-without-GPU companion:
//! same `aether.render` mailbox, no-op `DrawTriangle` / `Camera`
//! handlers (so desktop-designed components don't warn-storm),
//! `Err`-replying `CaptureFrame` handler. Headless chassis composes it
//! in place of [`RenderCapability`] (issue 603 Phase 2 § Resolved
//! Decision 5).

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]
// Frame-vertex / last-submitted Mutex guards are held through the
// per-frame swap and append sequence on purpose — the swap and
// subsequent length math read the buffer's current state and write
// back; releasing the guard mid-sequence opens a TOCTOU window
// where a sibling tick's producer mutates the buffer in between.
#![allow(clippy::significant_drop_tightening)]

// The cap's drawing + texture mail kinds (ADR-0121). Always-on (the
// `render` marker feature gates the whole module) so a wasm guest on the
// marker-only `render` feature sees the kind types.
pub mod kinds;
pub use kinds::*;

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers always-on
// (outside the `render-native` gate), against the identity. The drawing
// kinds come from the local `kinds` module (via the glob re-export
// above); `CaptureFrame` stays in `aether-kinds` (consumed by
// `aether-mcp`).
use aether_kinds::CaptureFrame;

// Auxiliary native-only types the chassis driver consumes alongside
// `RenderCapability`. The seams (`capture`, `pipeline`, `quad`, `texture`,
// `config`) now live under the `runtime` directory, covered by the one
// `mod runtime;` gate (`render-native`); their re-exports source through
// `runtime` so wasm components that opt into the marker-only `render` feature
// see only the identity ZST + Actor / HandlesKind impls, not these heavy
// GPU-bound types.
#[cfg(feature = "render-native")]
pub use runtime::{CaptureBackend, RenderConfig, RenderGpu, RenderHandles};

// `#[actor]` sits on each capability struct (the struct-hosted ADR-0123
// form): it reads the cap's sibling runtime module off disk and emits the
// always-on addressing markers + handler inventory against the struct here.
// The state-bearing, GPU-bound behavior of each cap — its `#[runtime] impl
// NativeActor`, runtime state struct, the wgpu accumulator helpers, the
// `HubOutbound` — lives in a per-cap runtime module: `runtime` for
// [`RenderCapability`] (gated `render-native`) and `headless_runtime` for
// [`HeadlessRenderCapability`] (gated the default `runtime`). The
// `aether_substrate` ctx types each impl names (`NativeActor` / `NativeCtx`
// / … / `Manual` / `CaptureFrameResult`) are now sourced inside each runtime
// module beside the body, not here — only the handler-argument kinds the
// emitted markers lift verbatim must keep resolving at this file's root.
use aether_actor::actor;

// With the `#[runtime] impl NativeActor` body now living in `runtime.rs`
// (struct-hosted ADR-0123 form), this glob no longer feeds a mod.rs impl — it
// brings the cap's runtime-internal names (`WHITE_TEXTURE_ID` / `QuadBatch` /
// `Mutex` / …) into scope for the co-located `#[cfg(test)] mod tests` below,
// which reaches them via `use super::*`. The lib target compiles that module
// out, so the glob reads as unused there; `unused_imports` is allowed
// alongside the wildcard lint. The headless companion's test moved beside its
// `#[runtime]` impl in `headless_runtime.rs`, so no headless glob is needed
// here.
#[cfg(feature = "render-native")]
#[allow(clippy::wildcard_imports, unused_imports)]
use runtime::*;

// The render runtime half — the wgpu-typed surface (state, ctx imports,
// accumulator helpers) — lives in `runtime.rs`, gated once here on the
// `render-native` override (matching the `#[actor] impl`'s runtime gate).
#[cfg(feature = "render-native")]
mod runtime;

// The headless companion's runtime half lives in `headless_runtime.rs`,
// gated on the default `runtime` feature so a no-GPU headless build still
// compiles it.
#[cfg(feature = "runtime")]
mod headless_runtime;

/// `aether.render` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — `Addressable`, the per-handler
/// `HandlesKind` markers, and the name-inventory entry, all emitted
/// always-on by `#[actor]` so a wasm guest on the marker-only `render`
/// feature can `ctx.actor::<RenderCapability>().send(&triangle)` without
/// dragging the GPU stack. The state-bearing runtime
/// (`RenderCapabilityState`, which holds the wgpu-typed
/// [`RenderHandles`] plus the substrate registry + mailer) lives behind
/// the `render-native` gate in the `runtime` module, so a transport- or
/// marker-only build never names it nor pulls `aether_substrate`/wgpu
/// through this cap.
#[actor(singleton)]
pub struct RenderCapability;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::test_chassis::TestChassis;
    use aether_actor::Addressable;
    use aether_kinds::QuadSpace;
    use aether_kinds::trace::Nanos;
    use aether_substrate::chassis::builder::Builder;
    use aether_substrate::mail::MailId;
    use aether_substrate::mail::MailRef;
    use aether_substrate::mail::registry::OwnedDispatch;
    use aether_substrate::mail::registry::{MailboxEntry, Registry};
    use aether_substrate::mail::{KindId, Source};
    use std::thread;

    use crate::test_chassis::fresh_substrate;

    fn deliver(registry: &Registry, name: &str, kind: KindId, payload: &[u8]) {
        let id = registry.lookup(name).expect("mailbox registered");
        let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry exists") else {
            panic!("expected mailbox entry for {name}");
        };
        handler.enqueue(OwnedDispatch::disarmed(
            kind,
            "test.kind".to_owned(),
            None,
            Source::NONE,
            MailRef::from(payload.to_vec()),
            1,
            MailId::NONE,
            MailId::NONE,
            None,
            Nanos(0),
            0,
            aether_data::MailboxId(0),
        ));
    }

    // ADR-0082 retired the frame-bound pending counter; the
    // DrawTriangle → render dispatch path is now covered end-to-end
    // by the bundle scenario tests (`tick_roundtrip_component_to_sink`
    // and the `test_bench_scenario` suite), which exercise it through
    // real settlement rather than a per-mailbox counter poll.

    /// ADR-0107 §4: `draw_solid_quads` accumulates into `quad_frame` under
    /// the reserved `WHITE_TEXTURE_ID` and records its kind name in
    /// `observed_kinds`. Verifies the expand-to-TexturedQuad path and the
    /// lazy white-texture insertion without a GPU.
    #[test]
    fn draw_solid_quads_accumulates_and_observed() {
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let config = RenderConfig {
            observed_kinds: Some(Arc::clone(&observed)),
            ..RenderConfig::default()
        };
        let (registry, mailer) = fresh_substrate();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<RenderCapability>(config)
            .build_passive()
            .expect("build succeeds");
        let handles = chassis
            .handle::<RenderHandles>()
            .expect("RenderCapability publishes RenderHandles");

        let mail = DrawSolidQuads {
            space: QuadSpace::Screen,
            quads: vec![SolidQuad {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
                color: [1.0, 0.0, 0.5, 0.8],
            }],
        };
        let payload = mail.encode_into_bytes();
        deliver(
            &registry,
            RenderCapability::NAMESPACE,
            <DrawSolidQuads as Kind>::ID,
            &payload,
        );

        thread::sleep(Duration::from_millis(50));

        let seen = observed
            .lock()
            .expect("observed_kinds mutex is not poisoned")
            .clone();
        assert!(
            seen.contains(&DrawSolidQuads::NAME.to_owned()),
            "draw_solid_quads handler should push its kind name; observed: {seen:?}",
        );

        let batches = handles
            .quad_frame
            .lock()
            .expect("quad_frame mutex is not poisoned")
            .clone();
        assert_eq!(
            batches.len(),
            1,
            "one QuadBatch should be in the accumulator"
        );
        assert_eq!(
            batches[0].texture_id, WHITE_TEXTURE_ID,
            "batch must use the reserved white texture id",
        );
        assert_eq!(
            batches[0].quads.len(),
            1,
            "batch must contain the one expanded quad"
        );
        assert_eq!(
            batches[0].quads[0].tint,
            [1.0, 0.0, 0.5, 0.8],
            "expanded quad tint must match the SolidQuad color",
        );
        assert_eq!(batches[0].quads[0].width, 30.0);

        let tex_present = handles
            .textures
            .lock()
            .expect("textures mutex is not poisoned")
            .entries
            .contains_key(&WHITE_TEXTURE_ID);
        assert!(
            tex_present,
            "white texture must be lazily inserted on first send"
        );

        drop(chassis);
    }
}

/// `HeadlessRenderCapability` **identity** (ADR-0122 identity/runtime
/// split). The chassis-without-GPU companion to [`RenderCapability`],
/// claiming the same `aether.render` mailbox so desktop-designed
/// components loaded on headless can mail `DrawTriangle` / `aether.camera`
/// / `aether.render.capture_frame` against a known recipient —
/// `DrawTriangle` and `Camera` no-op (the warn-storm sink-replacement role
/// pre-issue-603 Phase 2), `CaptureFrame` replies `Err` so MCP
/// `capture_frame` fails fast instead of timing out.
///
/// A ZST carrying only the addressing; the state-bearing runtime
/// (`HeadlessRenderCapabilityState`, holding the captured `HubOutbound`)
/// lives behind the default `runtime` gate in `headless_runtime` — no
/// `render-native` dep, so it compiles on a no-GPU headless build.
///
/// Headless chassis composes one of [`Self`] / [`RenderCapability`], never
/// both — the chassis builder rejects double-claiming a mailbox.
#[actor(singleton, headless_runtime)]
pub struct HeadlessRenderCapability;
