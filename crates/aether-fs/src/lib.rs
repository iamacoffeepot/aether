//! `aether.fs` cap. Owns the full ADR-0041 stack — its mail kinds
//! ([`kinds`], ADR-0121), the [`FileAdapter`] trait + `LocalFileAdapter`
//! (`adapter`), the `AdapterRegistry` + env-driven [`NamespaceRoots`]
//! (`registry`), and the [`FsCapability`] itself. Chassis mains
//! resolve a [`NamespaceRoots`] (typically via `NamespaceRoots::from_env`)
//! and pass it through `with_actor::<FsCapability>(roots)` — `init`
//! builds the adapter registry and returns `BootError` on failure (per
//! ADR-0063 fail-fast).
//!
//! Threading: the actor dispatcher thread pulls envelopes from the
//! `aether.fs` mailbox and routes them through the macro-emitted
//! `NativeDispatch::__aether_dispatch_envelope`. Adapter calls run
//! synchronously on that thread; ADR-0041 flagged a future host-fn
//! fast path for asset-sized streaming.

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

// Handler-signature kinds resolve at file root through the `pub use
// kinds::*` re-export above — `#[actor]` emits the `impl HandlesKind<K>
// for X {}` markers always-on against the identity, outside the
// `feature = "runtime"` gate, so they reference these kinds from here.
use aether_actor::{HandlesKind, Kind, WasmActorMailbox, WasmActorMailboxWithContext};
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::{NativeActorMailbox, NativeActorMailboxWithContext};

trait FsRequestForwarder {
    fn forward<K>(&self, payload: &K)
    where
        FsCapability: HandlesKind<K>,
        K: Kind;
}

impl FsRequestForwarder for WasmActorMailbox<'_, FsCapability> {
    fn forward<K>(&self, payload: &K)
    where
        FsCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

impl<C: Kind> FsRequestForwarder for WasmActorMailboxWithContext<'_, '_, FsCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        FsCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl FsRequestForwarder for NativeActorMailbox<'_, FsCapability> {
    fn forward<K>(&self, payload: &K)
    where
        FsCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl<C: Kind> FsRequestForwarder for NativeActorMailboxWithContext<'_, '_, FsCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        FsCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

/// Sender-side facade for actors addressed via
/// `ctx.actor::<FsCapability>()`.
///
/// Lifts the cap-shaped methods (`read(ns, path)`, `write(ns, path,
/// bytes)`, ...) one indirection above the raw
/// `.send(&Read { ns, path })` so component code stops reconstructing
/// the kind struct (and the `.into()` conversions on every field) at
/// every call site. The cap module owns receive-side
/// ([`FsCapability`]) AND send-side ([`FsMailboxExt`]) so future
/// kind additions land both surfaces in one place.
///
/// Impl'd for both base transports `ctx.actor::<FsCapability>()` can
/// return and their typed request-context adapters:
///
/// - [`WasmActorMailbox<FsCapability>`] — always-on, for wasm-component
///   callers.
/// - [`WasmActorMailboxWithContext<FsCapability, C>`] — a wasm mailbox
///   with a typed request context bound by `.with_context(&context)`.
/// - [`NativeActorMailbox<'_, FsCapability>`] — native cap-to-cap
///   sends, gated on `#[cfg(not(target_family = "wasm"))]`.
/// - [`NativeActorMailboxWithContext<'_, '_, FsCapability, C>`] — the
///   native contextual counterpart, behind the same gate.
///
/// All methods are fire-and-forget. Replies arrive as
/// `aether.fs.read_result` / `aether.fs.write_result` /
/// `aether.fs.delete_result` / `aether.fs.list_result`. Echoed
/// `namespace` + `path` (or `prefix`) fields provide readable domain
/// context; duplicate-safe one-shot matching uses a typed context bound
/// with `.with_context(&context)` and recovered with `take_context`
/// (ADR-0139).
///
/// Contextual facade calls intentionally discard the request id. Call
/// the contextual adapter's generic `send` directly when the minted
/// [`aether_actor::RequestId`] or native [`aether_data::MailId`] is
/// needed.
/// Synchronous `read_sync` / `write_sync` wrappers were on the
/// original issue 580 sketch — parked as a follow-up so this PR
/// stays mechanical.
///
/// The generic escape hatch is unaffected: `mailbox.send(&CustomKind { .. })`
/// still works for any `K` the cap declares via `HandlesKind<K>`,
/// since `send` is an inherent method on the underlying mailbox type.
#[allow(private_bounds)]
pub trait FsMailboxExt: FsRequestForwarder {
    /// Mail `aether.fs.read { namespace, path }` to the cap.
    fn read(&self, namespace: impl Into<String>, path: impl Into<String>) {
        self.forward(&Read { namespace: namespace.into(), path: path.into() });
    }

    /// Mail `aether.fs.write { namespace, path, bytes }` to the cap.
    /// The reply echoes `namespace` + `path` only (bytes are omitted
    /// from the echo so a megabyte write doesn't produce a megabyte
    /// reply).
    fn write(&self, namespace: impl Into<String>, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.forward(&Write { namespace: namespace.into(), path: path.into(), bytes: bytes.into() });
    }

    /// Mail `aether.fs.delete { namespace, path }` to the cap.
    fn delete(&self, namespace: impl Into<String>, path: impl Into<String>) {
        self.forward(&Delete { namespace: namespace.into(), path: path.into() });
    }

    /// Mail `aether.fs.list { namespace, prefix }` to the cap. The
    /// reply enumerates entries under the prefix.
    fn list(&self, namespace: impl Into<String>, prefix: impl Into<String>) {
        self.forward(&List { namespace: namespace.into(), prefix: prefix.into() });
    }

    /// Mail `aether.fs.copy { from, to }` to the cap. `from` is a raw
    /// host filesystem path; `to` is a namespace-address destination. The
    /// bytes flow host → namespace inside the substrate — they never ride
    /// the wire. The reply echoes `from` + `to` without bytes, so a
    /// large-file copy produces a small ack.
    fn copy(&self, from: impl Into<String>, to_namespace: impl Into<String>, to_path: impl Into<String>) {
        self.forward(&Copy {
            from: from.into(),
            to: NamespaceAddr { namespace: to_namespace.into(), path: to_path.into() },
        });
    }
}

impl<T: FsRequestForwarder> FsMailboxExt for T {}

/// `aether.fs` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`FsCapabilityState`, which holds the `aether_substrate`-typed
/// transform registry) lives behind the one `feature = "runtime"` gate, so
/// a transport-only build never names `FsCapabilityState` nor pulls
/// `aether_substrate` through this cap.
#[actor(singleton)]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_fs_mailbox_ext<T: FsMailboxExt>() {}

    #[test]
    fn base_and_contextual_mailbox_shapes_implement_fs_facade() {
        assert_fs_mailbox_ext::<WasmActorMailbox<'static, FsCapability>>();
        assert_fs_mailbox_ext::<WasmActorMailboxWithContext<'static, 'static, FsCapability, Read>>();
        #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
        assert_fs_mailbox_ext::<NativeActorMailbox<'static, FsCapability>>();
        #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
        assert_fs_mailbox_ext::<NativeActorMailboxWithContext<'static, 'static, FsCapability, Read>>();
    }
}
