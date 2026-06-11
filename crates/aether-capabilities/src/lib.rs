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
// Shared confique `parse_env` helpers + provider defaults (ADR-0090).
// Kept ungated so the per-provider `DEFAULT_MAX_IN_FLIGHT` constants can
// alias `DEFAULT_PROVIDER_MAX_IN_FLIGHT` at file top level; the parser
// fns are tiny pure-Rust dead code on non-native builds.
mod config_env;

// `aether.dag` computation-DAG executor cap (ADR-0047, issue 976).
pub mod dag;
pub mod engine;
pub mod fs;
// `aether.gemini` content-gen cap (ADR-0050, issue 1015). Native-only
// for the same reason as `anthropic`.
#[cfg(not(target_arch = "wasm32"))]
pub mod gemini;
pub mod handle;
pub mod http;
pub mod input;
// `aether.inventory` reverse-lookup inventory cap (ADR-0088 §6, issue
// 1122). Serves the per-build name/template manifest + dynamic-instance
// resolve over mail.
pub mod inventory;
// `aether.lifecycle` cap (ADR-0082). The bridged, non-generic capability
// the chassis drives one frame at a time. Always-native via `#[bridge]`,
// so a wasm component can address it by name.
pub mod lifecycle;
#[cfg(feature = "render")]
pub mod render;
pub mod rpc;
pub mod tcp;
pub mod test_bench;
#[cfg(test)]
pub(crate) mod test_chassis;
pub mod trace;
// ADR-0086 Phase 3b decentralized trace-tree reconstruction. Pure,
// transport-agnostic guided walk + stitch over the per-actor trace
// rings; the MCP and the in-process harness each supply their own
// fetch. Extracted to `aether-rpc` (ADR-0102, no native deps) and
// re-exported here at its original path so
// `aether_capabilities::trace_walk::*` keeps resolving.
pub use aether_rpc::trace_walk;
pub mod trampoline;
// First-party native `#[transform]`s (ADR-0048, issue 1464). The
// link-time inventory submission populates both the headless
// `TransformRegistry` and `describe_transforms` — both native. Native-
// only: the DAG executor that runs transforms is non-wasm, and the
// `#[transform]` inventory entry is itself `cfg(not(wasm32))`-gated, so
// on a wasm-header-only build the fn would be dead. No wasm consumer
// runs transforms, so gate the whole module rather than carry it dead.
#[cfg(not(target_arch = "wasm32"))]
pub mod transforms;
pub mod window;

#[cfg(feature = "audio")]
pub use audio::AudioCapability;
#[cfg(feature = "audio-native")]
pub use audio::AudioConfig;
// ADR-0050 `aether.anthropic` cap (issue 1014). `AnthropicConfig` is
// part of the same native-only module.
#[cfg(not(target_arch = "wasm32"))]
pub use anthropic::{AnthropicCapability, AnthropicConfig};
pub use component::{ComponentHostCapability, resolve_embedded};
pub use dag::DagCapability;
// ADR-0050 §2 shared content-gen infrastructure. Native-only — the two
// provider caps (issue 1014 / 1015) embed these.
#[cfg(not(target_arch = "wasm32"))]
pub use contentgen::{
    AnthropicAdapter, GeminiAdapter, StubAnthropicAdapter, StubGeminiAdapter, TaskQueue,
    stage_gen_output,
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
#[cfg(not(target_arch = "wasm32"))]
pub use engine::{EngineConfig, EngineOverlay};
pub use handle::HandleCapability;
pub use http::{HttpCapability, HttpConfig};
pub use input::InputCapability;
#[cfg(not(target_arch = "wasm32"))]
pub use input::InputConfig;
pub use inventory::InventoryCapability;
#[cfg(not(target_arch = "wasm32"))]
pub use lifecycle::LifecycleConfig;
pub use lifecycle::{LifecycleCapability, LifecycleMailboxExt};

pub use fs::FsCapability;
// ADR-0050 `aether.gemini` cap (issue 1015).
#[cfg(not(target_arch = "wasm32"))]
pub use gemini::{GeminiCapability, GeminiConfig};
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

#[cfg(all(test, feature = "native"))]
mod auto_name_inventory_tests {
    use aether_actor::Actor;
    use aether_data::{build_static_reverse_map, mailbox_id_from_name};

    use crate::fs::FsCapability;

    /// ADR-0088: `#[bridge(singleton)]` auto-emits a `NameEntry` for each
    /// chassis cap's mailbox namespace, so a `MailboxId` reverses to its
    /// real name through the static reverse map — no hand-maintained
    /// registration list. Touching `FsCapability` forces its module (and
    /// the macro-auto-emitted `NameEntry` submission alongside it) into
    /// this unit-test binary, so the map must then reverse `aether.fs`.
    /// Guards the macro -> submit -> reverse-map chain against a future
    /// regression that stops the bridge emitting the entry.
    #[test]
    fn chassis_mailbox_name_reverses_via_macro_auto_emitted_name_entry() {
        assert_eq!(FsCapability::NAMESPACE, "aether.fs");
        let map = build_static_reverse_map();
        let id = mailbox_id_from_name("aether.fs");
        assert_eq!(
            map.get(&id.0).map(String::as_str),
            Some("aether.fs"),
            "FsCapability's mailbox name should reverse via its \
             #[bridge(singleton)] macro-auto-emitted NameEntry",
        );
    }
}
