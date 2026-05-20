//! Native chassis capabilities (issue 552 stage 2e). Each module
//! implements one of the substrate's chassis-policy mailboxes as a
//! [`NativeActor`] — owning its mailbox name, state, and handlers.
//! The `Builder::with_actor` boot path on `aether-substrate` is the
//! installation site; chassis mains pick which caps to load
//! (Log/Handle/Io/Http are universal; Audio + Render gate behind the
//! `audio` / `render` features).
//!
//! Pre-stage-2e these modules lived under
//! `aether_substrate::capabilities`. The split decouples the
//! cap-marker layer from the substrate runtime so wasm components
//! can address caps via `ctx.actor::<R>().send(&kind)` (resolved
//! through `R::NAMESPACE`) without dragging in wasmtime / wgpu /
//! cpal. Today
//! the crate always pulls `aether-substrate` (the `NativeActor`
//! impls live alongside the structs); the header-only wasm build is
//! a follow-up.
//!
//! Issue 576 promoted `BroadcastCapability` into a real catch-all chassis
//! cap — it lives here alongside the rest, holds an
//! `Arc<HubOutbound>`, and dispatches every kind it receives through
//! a `#[fallback]` handler that fans the envelope out to every
//! attached MCP session.
//!
//! [`NativeActor`]: aether_substrate::actor::native::NativeActor
//! [`Actor`]: aether_actor::Actor

// `aether.anthropic` content-gen cap (ADR-0050, issue 1014). Native-
// only — embeds the native-only contentgen dispatch helper and makes
// blocking ureq / subprocess calls.
#[cfg(not(target_arch = "wasm32"))]
pub mod anthropic;
#[cfg(feature = "audio")]
pub mod audio;
pub mod component;
// Shared content-gen infrastructure (ADR-0050 §2). Native-only — the
// dispatch helper, staging, and adapter traits all lean on the
// substrate runtime (`Mailer`, `LocalFileAdapter`), so the module
// elides cleanly on the wasm-component build.
#[cfg(not(target_arch = "wasm32"))]
pub mod contentgen;
pub mod engine;
pub mod fs;
pub mod handle;
pub mod http;
pub mod input;
#[cfg(feature = "render")]
pub mod render;
pub mod rpc;
pub mod tcp;
pub mod test_bench;
#[cfg(test)]
pub(crate) mod test_chassis;
pub mod trace;
pub mod trampoline;
pub mod window;

#[cfg(feature = "audio")]
pub use audio::AudioCapability;
#[cfg(feature = "audio-native")]
pub use audio::AudioConfig;
// ADR-0050 `aether.anthropic` cap (issue 1014). `AnthropicConfig` is
// part of the same native-only module.
#[cfg(not(target_arch = "wasm32"))]
pub use anthropic::{AnthropicCapability, AnthropicConfig};
pub use component::ComponentHostCapability;
// ADR-0050 §2 shared content-gen infrastructure. Native-only — the two
// provider caps (issue 1014 / 1015) embed these.
#[cfg(not(target_arch = "wasm32"))]
pub use contentgen::{
    AnthropicAdapter, BlockingCall, GeminiAdapter, InFlightDispatch, StubAnthropicAdapter,
    StubGeminiAdapter, stage_gen_output,
};
// `ComponentHostConfig` is wasmtime-bound (it holds `Arc<Engine>` /
// `Arc<Linker<ComponentCtx>>`). It re-exports only on the native
// target — wasm-component consumers see the cap stub via
// `ComponentHostCapability` for typed `ctx.actor::<...>()` addressing
// without dragging the wasmtime stack into the wasm graph.
#[cfg(not(target_arch = "wasm32"))]
pub use component::ComponentHostConfig;
pub use engine::EngineProxy;
#[cfg(not(target_arch = "wasm32"))]
pub use engine::EngineProxyConfig;
pub use engine::EngineServer;
pub use handle::HandleCapability;
pub use http::{HttpCapability, HttpConfig};
pub use input::InputCapability;
#[cfg(not(target_arch = "wasm32"))]
pub use input::InputConfig;

pub use fs::FsCapability;
#[cfg(feature = "render")]
pub use render::HeadlessRenderCapability;
#[cfg(feature = "render")]
pub use render::RenderCapability;
#[cfg(feature = "render-native")]
pub use render::{CaptureBackend, RenderConfig, RenderGpu, RenderHandles};
pub use tcp::{TcpCapability, TcpListenerActor};
pub use test_bench::UnsupportedTestBenchCapability;
pub use trampoline::WasmTrampoline;
#[cfg(not(target_arch = "wasm32"))]
pub use trampoline::WasmTrampolineConfig;
pub use window::HeadlessWindowCapability;
