//! `aether.fs` cap. Owns the full ADR-0041 stack — its mail kinds
//! ([`kinds`], ADR-0121), the [`FileAdapter`] trait + [`LocalFileAdapter`]
//! (`adapter`), the [`AdapterRegistry`] + env-driven [`NamespaceRoots`]
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

mod config;
// The native libraries the runtime receive side leans on — the
// `FileAdapter` trait + `LocalFileAdapter`, and the `AdapterRegistry`
// keyed on namespace short name. They carry no wasm-incompatible deps,
// but they exist only to back the `NativeActor` impl, so they ride
// `fs-runtime` (the transport surface never touches them).
#[cfg(feature = "fs-runtime")]
mod adapter;
#[cfg(feature = "fs-runtime")]
mod registry;

pub use kinds::*;

#[cfg(feature = "fs-runtime")]
pub use adapter::{FileAdapter, FsResult, LocalFileAdapter};
pub use config::NamespaceRoots;
// The `Config` derive on `NamespaceRoots` emits these sibling types in
// `config`; chassis CLI / boot wiring addresses them through the `fs::`
// path (e.g. `aether-substrate-bundle`'s `cli` / `chassis_common`), so
// re-export them at the cap root. Native-only — the derive rides
// `fs-runtime`, the gate the native receive side keys on. Inherent shims
// (`from_env` / `from_argv_then_env` / `into_layer`) ride the type and
// need no re-export.
#[cfg(feature = "fs-runtime")]
pub use config::{NamespaceRootsLayer, NamespaceRootsOverlay};
#[cfg(feature = "fs-runtime")]
pub use registry::{AdapterRegistry, build_registry};

// The native receive side: the `NativeActor` impl, its dispatch table,
// the native `FsMailboxExt` send arm, and the ADR-0090 config re-exports
// (`NamespaceRootsLayer` / `Overlay`). Gated behind `fs-runtime` — the
// transport/runtime split #2296 establishes as the template (replacing
// the wasm-target-keyed `#[bridge]`). The `NamespaceRoots` config struct
// + the `FsCapability` marker + kinds + the wasm `FsMailboxExt` arm below
// stay always-on so a transport consumer addresses the cap with no heavy
// deps.
#[cfg(feature = "fs-runtime")]
mod runtime;

use aether_actor::WasmActorMailbox;

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
/// Impl'd for both transports `ctx.actor::<FsCapability>()` can
/// return:
///
/// - [`WasmActorMailbox<FsCapability>`] — always-on, for wasm-component
///   callers.
/// - `NativeActorMailbox<'_, FsCapability>` — native cap-to-cap sends,
///   gated on `#[cfg(feature = "fs-runtime")]` (the arm lives in
///   `fs/runtime.rs` with the rest of the native receive side).
///
/// All methods are fire-and-forget. Replies arrive as
/// `aether.fs.read_result` / `aether.fs.write_result` /
/// `aether.fs.delete_result` / `aether.fs.list_result`, correlated
/// by the echoed `namespace` + `path` (or `prefix`) per ADR-0041.
/// Synchronous `read_sync` / `write_sync` wrappers were on the
/// original issue 580 sketch — parked as a follow-up so this PR
/// stays mechanical.
///
/// The generic escape hatch is unaffected: `mailbox.send(&CustomKind { .. })`
/// still works for any `K` the cap declares via `HandlesKind<K>`,
/// since `send` is an inherent method on the underlying mailbox type.
pub trait FsMailboxExt {
    /// Mail `aether.fs.read { namespace, path }` to the cap.
    fn read(&self, namespace: &str, path: &str);

    /// Mail `aether.fs.write { namespace, path, bytes }` to the cap.
    /// The reply echoes `namespace` + `path` only (bytes are omitted
    /// from the echo so a megabyte write doesn't produce a megabyte
    /// reply).
    fn write(&self, namespace: &str, path: &str, bytes: &[u8]);

    /// Mail `aether.fs.delete { namespace, path }` to the cap.
    fn delete(&self, namespace: &str, path: &str);

    /// Mail `aether.fs.list { namespace, prefix }` to the cap. The
    /// reply enumerates entries under the prefix.
    fn list(&self, namespace: &str, prefix: &str);

    /// Mail `aether.fs.copy { from, to }` to the cap. `from` is a raw
    /// host filesystem path; `to` is a namespace-address destination. The
    /// bytes flow host → namespace inside the substrate — they never ride
    /// the wire. The reply echoes `from` + `to` without bytes, so a
    /// large-file copy produces a small ack.
    fn copy(&self, from: &str, to_namespace: &str, to_path: &str);
}

impl FsMailboxExt for WasmActorMailbox<'_, FsCapability> {
    fn read(&self, namespace: &str, path: &str) {
        self.send(&Read {
            namespace: namespace.into(),
            path: path.into(),
        });
    }
    //noinspection DuplicatedCode
    fn write(&self, namespace: &str, path: &str, bytes: &[u8]) {
        self.send(&Write {
            namespace: namespace.into(),
            path: path.into(),
            bytes: bytes.to_vec(),
        });
    }
    fn delete(&self, namespace: &str, path: &str) {
        self.send(&Delete {
            namespace: namespace.into(),
            path: path.into(),
        });
    }
    fn list(&self, namespace: &str, prefix: &str) {
        self.send(&List {
            namespace: namespace.into(),
            prefix: prefix.into(),
        });
    }
    //noinspection DuplicatedCode
    fn copy(&self, from: &str, to_namespace: &str, to_path: &str) {
        self.send(&Copy {
            from: from.into(),
            to: NamespaceAddr {
                namespace: to_namespace.into(),
                path: to_path.into(),
            },
        });
    }
}
