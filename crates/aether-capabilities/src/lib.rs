//! Native chassis capabilities (issue 552 stage 2e). Each module
//! implements one of the substrate's chassis-policy mailboxes as a
//! [`NativeActor`] — owning its mailbox name, state, and handlers.
//! The `Builder::with_actor` boot path on `aether-substrate` is the
//! installation site; chassis mains pick which caps to load
//! (Log/Io/Http are universal; Audio + Render gate behind the
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
//! [`Addressable`]: aether_actor::Addressable

// ADR-0131: self-alias so the `#[http::router]` macro's emitted
// `::aether_capabilities::…` paths resolve inside this crate's own
// route fixtures (the pattern `aether-actor` / `aether-substrate`
// already use for their derive-emitted paths).
extern crate alloc;
extern crate self as aether_capabilities;

// `aether.anthropic` content-gen cap (ADR-0050, issue 1014). Native-
// only — embeds the native-only contentgen dispatch helper and makes
// blocking ureq / subprocess calls.
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub mod anthropic;
#[cfg(feature = "audio")]
pub mod audio;
#[cfg(feature = "clipboard")]
pub mod clipboard;
pub mod component;
// Shared infrastructure for capabilities (ADR-0050 §2). Native-only — the
// dispatch helper, staging, and adapter traits all lean on the
// substrate runtime (`Mailer`, `LocalFileAdapter`), so the module
// elides cleanly on the wasm-component build.
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub mod shared;

pub mod engine;
pub mod fs;
pub mod game;
// `aether.gemini` content-gen cap (ADR-0050, issue 1015). Native-only
// for the same reason as `anthropic`.
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub mod gemini;
// The two HTTP capabilities, co-located under one submodule (ADR-0121):
// the `aether.http` egress client and the `aether.http.server` inbound
// server (a native singleton modeled on `RpcServerCapability` — binds a
// port, parses each HTTP/1.1 request into mail to a handler component,
// writes the handler's reply back as the HTTP response). They own their
// shared wire kinds in `http/kinds.rs`.
pub mod http;
pub mod input;
// `aether.inventory` reverse-lookup inventory cap (ADR-0088 §6, issue
// 1122). Serves the per-build name/template manifest + dynamic-instance
// resolve over mail.
pub mod inventory;
// `aether.lifecycle` cap (ADR-0082). The non-generic capability the
// chassis drives one frame at a time. Always-native via `#[actor(singleton)]`,
// so a wasm component can address it by name.
pub mod lifecycle;
#[cfg(feature = "render")]
pub mod render;
pub mod rpc;
pub mod tcp;
pub mod test_bench;
// `aether.text` cap (ADR-0105). CPU-only — composes the render texture
// surface by mail — but feature-gated the two-layer way so a wasm
// component can address it by type without pulling `fontdue` into the
// wasm graph.
#[cfg(feature = "text")]
pub mod text;
pub mod trace;
pub mod trampoline;
pub mod window;

#[cfg(feature = "audio")]
pub use audio::AudioCapability;
#[cfg(feature = "audio-runtime")]
pub use audio::AudioConfig;
#[cfg(feature = "clipboard-runtime")]
pub use clipboard::ClipboardConfig;
#[cfg(feature = "clipboard")]
pub use clipboard::{ClipboardCapability, ClipboardMailboxExt, HeadlessClipboardCapability};
// ADR-0050 `aether.anthropic` cap (issue 1014). `AnthropicConfig` is
// part of the same native-only module.
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use anthropic::{AnthropicCapability, AnthropicConfig};
pub use component::{ComponentHostCapability, resolve_embedded};
// ADR-0050 §2 shared content-gen infrastructure. Native-only — the two
// provider caps (issue 1014 / 1015) embed these.
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use shared::contentgen::ContentGenConfigLayer;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use shared::contentgen::{
    AnthropicAdapter, ContentGenConfig, GeminiAdapter, StubAnthropicAdapter, StubGeminiAdapter, TaskQueue,
};
// `ComponentHostConfig` is wasmtime-bound (it holds `Arc<Engine>` /
// `Arc<Linker<ComponentCtx>>`). Under the ADR-0122 split it lives behind
// the `feature = "runtime"` gate (only the runtime half names it), so it
// re-exports only when that feature is on — a transport-only build sees the
// cap stub via `ComponentHostCapability` for typed `ctx.actor::<...>()`
// addressing without dragging the wasmtime stack in.
#[cfg(feature = "runtime")]
pub use component::ComponentHostConfig;
pub use engine::EngineProxy;
#[cfg(not(target_family = "wasm"))]
pub use engine::EngineProxyConfig;
pub use engine::EngineServer;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use engine::{EngineConfig, EngineConfigLayer, EngineOverlay};
pub use http::{HttpCapability, HttpConfig};
// ADR-0108 `aether.http.server` cap (issue 1760). `HttpServerConfig` is the
// always-on domain struct; the `Config`-derive `HttpServerConfigLayer` /
// `HttpServerOverlay` and the bound-port `HttpServerHandle` are native-only.
#[cfg(feature = "runtime")]
pub use http::HttpServerConfigLayer;
#[cfg(not(target_family = "wasm"))]
pub use http::HttpServerHandle;
#[cfg(feature = "runtime")]
pub use http::HttpServerOverlay;
pub use http::{HttpServerCapability, HttpServerConfig};
pub use input::InputCapability;
#[cfg(feature = "runtime")]
pub use input::InputConfig;
pub use inventory::InventoryCapability;
// ADR-0122 split: `LifecycleConfig` configures the runtime-only
// `LifecycleCapabilityState`, so it rides the `runtime` gate with the
// rest of the lifecycle runtime half.
#[cfg(feature = "runtime")]
pub use lifecycle::LifecycleConfig;
pub use lifecycle::{LifecycleCapability, LifecycleMailboxExt};

pub use fs::FsCapability;
#[cfg(feature = "runtime")]
pub use game::GameGatewayConfig;
pub use game::{GameGatewayCapability, PlayerSessionActor};
// ADR-0050 `aether.gemini` cap (issue 1015).
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use gemini::{GeminiCapability, GeminiConfig};
#[cfg(feature = "render")]
pub use render::HeadlessRenderCapability;
#[cfg(feature = "render")]
pub use render::RenderCapability;
#[cfg(feature = "render-runtime")]
pub use render::{CaptureBackend, RenderConfig, RenderGpu, RenderHandles, RenderTuningConfig};
pub use tcp::{TcpCapability, TcpListenerActor};
pub use test_bench::UnsupportedTestBenchCapability;
#[cfg(feature = "text")]
pub use text::TextCapability;
pub use trampoline::WasmTrampoline;
#[cfg(feature = "runtime")]
pub use trampoline::WasmTrampolineConfig;
pub use window::HeadlessWindowCapability;
