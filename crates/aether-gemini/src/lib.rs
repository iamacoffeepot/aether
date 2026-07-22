//! `aether.gemini` media-generation provider (ADR-0050) — two request kinds,
//! `aether.gemini.nanobanana.generate` (image) and
//! `aether.gemini.lyria.generate` (music), with no text completion (the user
//! defaults to the Claude CLI per ADR-0050 §3).
//!
//! Per ADR-0159 the provider is dual-homed over one shared crate:
//!
//! - **Guest component** (`component`, the default) — [`GeminiComponent`], a
//!   wasm [`WasmActor`](aether_actor::WasmActor) that reaches HTTPS through
//!   `aether.http.fetch`, stages generated artifacts through `aether.fs.write`
//!   (`gen/<uuid>.{png,wav}`), and reads reference images through
//!   `aether.fs.read`. It holds the API key in its init-config
//!   ([`GeminiComponentConfig`]) and owns no socket; each request/reply flow is
//!   the ADR-0139 `send_with_context` / `take_context` two-handler shape. Its
//!   egress is bounded per-sender at the `aether.http` edge (ADR-0158), so the
//!   guest queues nothing itself.
//! - **Native cap** (`runtime`) — `GeminiCapability`, the legacy chassis-owned
//!   actor over the `ureq` Nano Banana / Lyria backends and the ADR-0093
//!   `TaskQueue`. It stays intact for this PR; its retirement is issue #3893.
//!
//! Both halves sit over the same wire kinds (`kinds`, byte-identical across
//! the move) and the same pure provider logic — request-body construction
//! (`body`), response parsing + per-model validation (`nanobanana` /
//! `lyria`), and the error taxonomy (`error`).

// Always-on: the wire kinds + the native `GeminiConfig` domain struct carry
// the marker face.
mod config;
mod kinds;

// Shared pure provider logic (no I/O): the request-body builders + base64
// codec, the per-model validation tables + response parsers, and the error
// taxonomy. Compiled for both halves; a marker-only build (neither feature)
// carries none of it.
#[cfg(any(feature = "component", feature = "runtime"))]
mod body;
#[cfg(any(feature = "component", feature = "runtime"))]
mod error;
#[cfg(any(feature = "component", feature = "runtime"))]
mod lyria;
#[cfg(any(feature = "component", feature = "runtime"))]
mod nanobanana;

// ADR-0159 guest component (the default): the `GeminiComponent` actor + its
// init-config kind + the request-context kinds, and `export!`.
#[cfg(feature = "component")]
mod component;

// ADR-0050 native cap runtime half: the `ureq` backends (`adapter`) and the
// `aether_substrate`-typed `NativeActor` state (`runtime`). Gated so the guest
// wasm build never names `aether_substrate` / `ureq`.
#[cfg(feature = "runtime")]
mod adapter;
#[cfg(feature = "runtime")]
mod runtime;

pub use config::GeminiConfig;
pub use kinds::*;

#[cfg(feature = "component")]
pub use component::{GeminiComponent, GeminiComponentConfig};

#[cfg(feature = "runtime")]
pub use adapter::{DisabledGeminiAdapter, UreqGeminiAdapter};
#[cfg(feature = "runtime")]
pub use config::{GeminiConfigLayer, GeminiOverlay};
#[cfg(feature = "runtime")]
pub use runtime::GeminiParams;

/// Default per-cap concurrency bound when `AETHER_GEMINI_MAX_IN_FLIGHT`
/// is unset. Conservative — image / music generation is multi-second
/// and paid.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 2;

/// Default per-request timeout when `AETHER_GEMINI_TIMEOUT_MS` (native cap) or
/// the component's `timeout_millis` init-config is unset. Media generation can
/// run a couple minutes.
pub const DEFAULT_TIMEOUT_MILLIS: u32 = 180_000;

/// `aether.gemini` mailbox cap **identity** (ADR-0122 identity/runtime
/// split), the native half. A ZST carrying only the addressing —
/// `Addressable` (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind`
/// markers, and the name-inventory entry, all emitted by `#[actor]`. The
/// state-bearing runtime (`GeminiCapabilityState`, which holds the
/// `aether_substrate`-typed adapter + task queue) lives in the `runtime`
/// module. Gated with the runtime half so the guest wasm build carries only
/// the `GeminiComponent` guest actor.
//
// Handler-signature kinds (`LyriaGenerate` / `NanobananaGenerate`) resolve
// at file root through the `pub use kinds::*` re-export above — `#[actor]`
// emits the `impl HandlesKind<K>` markers against the identity.
#[cfg(feature = "runtime")]
#[actor(singleton)]
pub struct GeminiCapability;

// The `#[actor]` / `#[handler]` attribute path stays with the runtime half.
// Everything that names an `aether_substrate` type — the handler/init ctx, the
// runtime state, the reply helpers — lives in the `runtime` module; the
// `#[runtime] impl` sits beside its state there.
#[cfg(feature = "runtime")]
use aether_actor::actor;
