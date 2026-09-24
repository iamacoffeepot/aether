//! `aether.fs` capability: the file-I/O mail surface (ADR-0041).
//!
//! Owns the whole stack: the mail kinds ([`kinds`]), the [`FileAdapter`] trait
//! and its `LocalFileAdapter`, the [`AdapterRegistry`] over the `save`,
//! `assets`, and `config` namespaces, and the [`FsCapability`] itself. A
//! chassis main resolves a [`NamespaceRoots`] (usually through
//! `NamespaceRoots::from_env`) and passes it to
//! `with_actor::<FsCapability>(roots)`; `init` builds the adapter registry and
//! returns `BootError` when a root is unusable, so a misconfigured chassis
//! fails at boot and not at the first read.
//!
//! Adapter calls run synchronously on the actor's dispatcher thread, the one
//! that pulls envelopes from the `aether.fs` mailbox.

#![forbid(unsafe_code)]

pub mod kinds;

mod adapter;
mod config;
mod registry;

pub use kinds::*;

pub use adapter::{Access, LocalFileAdapter};
pub use adapter::{FileAdapter, FsResult};
pub use config::NamespaceRoots;
// The `Config` derive on `NamespaceRoots` emits these sibling types in
// `config`; chassis CLI / boot wiring addresses them through the
// `fs::` path, so re-export them here (native-only — the derive is
// feature-gated). Inherent shims (`from_env` / `from_argv_then_env` /
// `into_layer`) ride the type and need no re-export.
#[cfg(feature = "runtime")]
pub use config::{NamespaceRootsLayer, NamespaceRootsOverlay};
pub use registry::{AdapterRegistry, build_registry};

/// `aether.fs` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`FsCapabilityState`, which holds the `aether_substrate`-typed
/// transform registry) lives behind the one `feature = "runtime"` gate, so
/// a transport-only build never names `FsCapabilityState` nor pulls
/// `aether_substrate` through this cap.
#[actor(singleton, root)]
pub struct FsCapability;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` type — the handler/init
// ctx, the runtime state, the fold helpers, and the `#[runtime] impl` itself —
// lives in the `runtime` module below, gated once by `feature = "runtime"` and
// written cfg-free within. The kind types (`Read` / `ReadResult` / …) stay
// always-on via `pub use kinds::*` at module root — the always-on
// `HandlesKind<K>` markers name them.
use aether_actor::actor;

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `FsCapabilityState`, fold helpers, and the `#[runtime] impl`) lives in
// `runtime.rs`, gated once here.
#[cfg(feature = "runtime")]
mod runtime;
