//! Native chassis capabilities (issue 552 stage 2e). Each module
//! implements one of the substrate's chassis-policy mailboxes as a
//! [`NativeActor`] — owning its mailbox name, state, and handlers.
//! The `Builder::with_actor` boot path on `aether-substrate` is the
//! installation site; chassis mains pick which caps to load
//! (Log/Io/Http are universal; the audio and render caps now live in
//! the `aether-audio` / `aether-render` crates).
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

pub mod component;
// Shared infrastructure for capabilities. Native-only — the `net`
// address-parsing helpers lean on the substrate runtime, so the module
// elides cleanly on the wasm-component build. (The content-gen provider
// cluster that also lived here moved to the `aether-contentgen` /
// `aether-anthropic` / `aether-gemini` crates, iamacoffeepot/aether#3705.)
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub mod shared;

pub mod engine;
pub mod game;
// The two HTTP capabilities, co-located under one submodule (ADR-0121):
// the `aether.http` egress client and the `aether.http.server` inbound
// server (a native singleton modeled on `RpcServerCapability` — binds a
// port, parses each HTTP/1.1 request into mail to a handler component,
// writes the handler's reply back as the HTTP response). They own their
// shared wire kinds in `http/kinds.rs`.
pub mod http;
pub mod input;
// `aether.lifecycle` cap (ADR-0082). The non-generic capability the
// chassis drives one frame at a time. Always-native via `#[actor(singleton)]`,
// so a wasm component can address it by name.
pub mod lifecycle;
pub mod rpc;
pub mod tcp;
pub mod test_bench;
pub mod trampoline;

pub use component::{ComponentHostCapability, resolve_embedded};
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
// ADR-0122 split: `LifecycleConfig` configures the runtime-only
// `LifecycleCapabilityState`, so it rides the `runtime` gate with the
// rest of the lifecycle runtime half.
#[cfg(feature = "runtime")]
pub use lifecycle::LifecycleConfig;
pub use lifecycle::{LifecycleCapability, LifecycleMailboxExt};

#[cfg(feature = "runtime")]
pub use game::GameGatewayConfig;
pub use game::{GameGatewayCapability, PlayerSessionActor};
pub use tcp::{TcpCapability, TcpListenerActor};
pub use test_bench::UnsupportedTestBenchCapability;
pub use trampoline::WasmTrampoline;
#[cfg(feature = "runtime")]
pub use trampoline::WasmTrampolineConfig;
