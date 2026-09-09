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

use aether_actor::MailboxForward;

/// Sender-side facade for actors addressed via
/// `ctx.actor::<FsCapability>()`.
///
/// Lifts the cap-shaped methods (`read(ns, path)`, `write(ns, path,
/// bytes)`, ...) one indirection above the raw
/// `.send(&Read { addr })` so component code stops assembling the kind
/// struct and its [`NamespaceAddr`] at every call site. The cap module owns receive-side
/// ([`FsCapability`]) AND send-side ([`FsMailboxExt`]) so future
/// kind additions land both surfaces in one place.
///
/// Blanket-impl'd over [`MailboxForward<FsCapability>`], so it reaches every
/// handle `ctx.actor::<FsCapability>()` can return — the wasm and native
/// mailboxes and their typed request-context adapters alike — without this
/// crate naming any of them.
///
/// All methods are fire-and-forget. Replies arrive as
/// `aether.fs.read_result` / `aether.fs.write_result` /
/// `aether.fs.delete_result` / `aether.fs.list_result`. The echoed
/// `addr` provides readable domain context; duplicate-safe one-shot
/// matching uses a typed context bound with `.with_context(&context)`
/// and recovered with `take_context` (ADR-0139).
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
pub trait FsMailboxExt: MailboxForward<FsCapability> {
    /// Mail `aether.fs.read { addr }` to the cap.
    fn read(&self, namespace: impl Into<String>, path: impl Into<String>) {
        self.forward(&Read { addr: NamespaceAddr::new(namespace, path) });
    }

    /// Mail `aether.fs.write { addr, bytes }` to the cap. The reply
    /// echoes `addr` only (bytes are omitted from the echo so a
    /// megabyte write doesn't produce a megabyte reply).
    fn write(&self, namespace: impl Into<String>, path: impl Into<String>, bytes: impl Into<Vec<u8>>) {
        self.forward(&Write { addr: NamespaceAddr::new(namespace, path), bytes: bytes.into() });
    }

    /// Mail `aether.fs.delete { addr }` to the cap.
    fn delete(&self, namespace: impl Into<String>, path: impl Into<String>) {
        self.forward(&Delete { addr: NamespaceAddr::new(namespace, path) });
    }

    /// Mail `aether.fs.list { addr }` to the cap. `addr.path` is the
    /// prefix; the reply enumerates entries under it.
    fn list(&self, namespace: impl Into<String>, prefix: impl Into<String>) {
        self.forward(&List { addr: NamespaceAddr::new(namespace, prefix) });
    }

    /// Mail `aether.fs.copy { from, to }` to the cap. `from` is a raw
    /// host filesystem path; `to` is a namespace-address destination. The
    /// bytes flow host → namespace inside the substrate — they never ride
    /// the wire. The reply echoes `from` + `to` without bytes, so a
    /// large-file copy produces a small ack.
    fn copy(&self, from: impl Into<String>, to_namespace: impl Into<String>, to_path: impl Into<String>) {
        self.forward(&Copy { from: from.into(), to: NamespaceAddr::new(to_namespace, to_path) });
    }
}

impl<T: MailboxForward<FsCapability>> FsMailboxExt for T {}

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

#[cfg(test)]
mod tests {
    use super::*;
    use aether_actor::{WasmActorMailbox, WasmActorMailboxWithContext};
    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    use aether_substrate::actor::native::{NativeActorMailbox, NativeActorMailboxWithContext};

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
