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
//! [`NativeActor`]: aether_substrate::native_actor::NativeActor
//! [`Actor`]: aether_actor::Actor

#[cfg(feature = "audio")]
pub mod audio;
pub mod broadcast;
pub mod component;
pub mod handle;
pub mod http;
pub mod input;
pub mod io;
pub mod log;
#[cfg(feature = "render")]
pub mod render;
pub mod tcp;
pub mod test_bench;
#[cfg(not(target_arch = "wasm32"))]
pub mod wasm_trampoline;
pub mod window;

#[cfg(feature = "audio")]
pub use audio::AudioCapability;
#[cfg(feature = "audio-native")]
pub use audio::AudioConfig;
pub use broadcast::BroadcastCapability;
pub use component::ComponentHostCapability;
// `ComponentHostConfig` is wasmtime-bound (it holds `Arc<Engine>` /
// `Arc<Linker<SubstrateCtx>>`). It re-exports only on the native
// target — wasm-component consumers see the cap stub via
// `ComponentHostCapability` for typed `ctx.actor::<...>()` addressing
// without dragging the wasmtime stack into the wasm graph.
#[cfg(not(target_arch = "wasm32"))]
pub use component::ComponentHostConfig;
pub use handle::HandleCapability;
pub use http::{HttpCapability, HttpConfig};
pub use input::InputCapability;
#[cfg(not(target_arch = "wasm32"))]
pub use input::InputConfig;

pub use io::IoCapability;
pub use log::LogCapability;
#[cfg(feature = "render")]
pub use render::HeadlessRenderCapability;
#[cfg(feature = "render")]
pub use render::RenderCapability;
#[cfg(feature = "render-native")]
pub use render::{CaptureBackend, RenderConfig, RenderGpu, RenderHandles};
pub use tcp::{TcpCapability, TcpListenerActor};
pub use test_bench::UnsupportedTestBenchCapability;
#[cfg(not(target_arch = "wasm32"))]
pub use wasm_trampoline::{WasmTrampoline, full_name as wasm_trampoline_full_name};
pub use window::HeadlessWindowCapability;
