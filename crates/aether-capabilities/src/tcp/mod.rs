//! `aether.tcp` cap (issue 607 Phase 6a, ADR-0079).
//!
//! Three-tier shape: [`TcpCapability`] (Singleton control plane) →
//! [`TcpListenerActor`] (Instanced, one per bound port) → eventually
//! `TcpSessionActor` (Instanced, Phase 6b — per connection). Phase 6a
//! lands the singleton + listener and a stub accept handler that
//! drops accepted streams; Phase 6b adds the session spawn and the
//! read/write surface.
//!
//! ## Supervision shape
//!
//! `TcpCapability` is the supervisor of its listener fleet: it spawns
//! listeners, monitors them, and replies to unbind requests on their
//! close. The cap holds its own `MailboxId → ListenerEntry` map; it
//! does NOT walk the chassis-wide actor registry to enumerate
//! children. Cap handlers don't introspect the registry — the
//! cap-as-supervisor pattern keeps the actor model intact (caps
//! communicate via mail at runtime; chassis-level introspection is a
//! test/embedder affordance, not a handler-side surface).
//!
//! ## Mail surface
//!
//! Control plane (mailed to `aether.tcp`):
//! - `Connect { addr, name? }` → `ConnectResult`
//! - `BindListener { addr, name? }` → `BindListenerResult`
//! - `UnbindListener { listener_name }` → `UnbindListenerResult`
//!   (asynchronous reply: the cap monitors the listener at spawn time
//!   and replies only after `MonitorNotice` arrives)
//! - `ListListeners` → `ListListenersResult`
//!
//! Listener (mailed to `aether.tcp.listener:<name>`):
//! - `Close` → cooperative shutdown via `ctx.shutdown()`
//!
//! ## Threading
//!
//! Each listener owns one sidecar OS thread that holds the
//! `std::net::TcpListener` and runs a blocking accept loop. On
//! `unwire` the listener flips a shutdown flag and self-connects
//! to its bound port to wake the blocked accept; the accept returns,
//! sees the flag, breaks; the dispatcher thread joins.

#[cfg(feature = "runtime")]
mod config;
pub mod kinds;
mod listener;
mod route;
mod session;

pub use kinds::*;
pub use listener::TcpListenerActor;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub use route::TcpNativeExt;
pub use route::TcpWasmExt;
pub use session::TcpSessionActor;
// `TcpListenerConfig` and `TcpSessionConfig` are child-actor init
// bundles holding raw `TcpListener` / `TcpStream` handles, consumed
// only by the runtime halves (`runtime.rs`, `listener/runtime.rs`,
// `session/runtime.rs`), so `config` rides the `feature = "runtime"`
// gate. The actor markers themselves (above) are always-on so wasm
// callers can name them in [`TcpWasmExt::listener`] /
// [`TcpWasmExt::session`] type parameters.
#[cfg(feature = "runtime")]
pub use config::{TcpListenerConfig, TcpSessionConfig};

// `MonitorNotice` stays importable at file root because the `#[actor]`
// macro emits an always-on `HandlesKind<MonitorNotice>` marker against
// the identity below (the runtime half handles the listener-fleet
// monitor notices).
use aether_kinds::MonitorNotice;

/// `aether.tcp` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the singleton name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`TcpCapabilityState`, the cap's listener-fleet supervisor map) lives
/// behind the one `feature = "runtime"` gate, so a transport-only build never
/// names `TcpCapabilityState` nor pulls `aether_substrate` through this cap.
///
/// The cap is the supervisor of its listener fleet: it spawns listeners,
/// monitors them, and replies to unbind requests on their close. It holds its
/// own `MailboxId → ListenerEntry` map; it does NOT walk the chassis-wide
/// actor registry to enumerate children.
#[actor(singleton)]
pub struct TcpCapability;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` / `std::net` type — the
// handler/init ctx, the runtime state, the supervisor structs, and the
// `#[runtime] impl NativeActor` itself — lives in the `runtime` module below,
// gated once by `feature = "runtime"`. The handled kinds (`BindListener` /
// `UnbindListener` / `ListListeners`) stay always-on via `pub use kinds::*`
// and `MonitorNotice` via the always-on `aether_kinds` import above — the
// always-on `HandlesKind<K>` markers `#[actor]` emits name them.
use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;

#[cfg(all(test, feature = "runtime"))]
mod tests;
