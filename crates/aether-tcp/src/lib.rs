//! `aether.tcp` cap (issue 607 Phase 6a, ADR-0079).
//!
//! Accepted connections use the three-tier lineage [`TcpCapability`]
//! (Singleton control plane) → [`TcpListenerActor`] (Instanced, one per
//! bound port) → [`TcpSessionActor`] (Instanced, one per connection).
//! Outbound connections spawn the same session actor directly beneath
//! the cap. Both session shapes expose the same framed read/write and
//! close surface.
//!
//! ## Supervision shape
//!
//! `TcpCapability` is the supervisor of its listener fleet: it spawns
//! listeners, monitors them, and replies to unbind requests on their
//! close. The cap holds its own listener entries, each with the proof
//! its spawn returned; it does NOT walk the chassis-wide actor registry
//! to enumerate children. Cap handlers don't introspect the registry — the
//! cap-as-supervisor pattern keeps the actor model intact (caps
//! communicate via mail at runtime; chassis-level introspection is a
//! test/embedder affordance, not a handler-side surface).
//!
//! ## Mail surface
//!
//! Control plane (mailed to `aether.tcp`):
//! - `Connect { addr, name?, consumer? }` → `ConnectResult`
//! - `ConnectSelf { addr, name? }` → `ConnectResult` (the sender is the
//!   consumer)
//! - `BindListener { addr, name?, consumer? }` → `BindListenerResult`
//! - `BindListenerSelf { addr, name? }` → `BindListenerResult` (the sender
//!   is the consumer)
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
//!
//! ## Crate shape
//!
//! Extracted by the arc that dissolved the capabilities monolith
//! (iamacoffeepot/aether#3751) as a per-cap crate.
//! Owns the whole three-tier lineage — [`TcpCapability`],
//! [`TcpListenerActor`], [`TcpSessionActor`] — plus the cap's own
//! `aether.tcp.*` mail kinds ([`kinds`]) and the listener / session init
//! configs (`config`).
//!
//! A consumer tier (the shelved `aether-game` player tier was the in-repo
//! example) names these types directly: it sends the cap-root kinds with
//! `ctx.send::<TcpCapability>`, binds or dials itself as consumer with a
//! `_self` kind, and writes framed bytes to a session through the
//! host-stamped sender of the [`SessionData`] it receives (`ctx.sender()`,
//! then `ctx.send_to`). That is a downward leaf→leaf dependency, not a
//! facade: a downstream crate that wants TCP deps here directly, and pulls
//! in nothing else.

#![forbid(unsafe_code)]
// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

#[cfg(feature = "runtime")]
mod config;
pub mod kinds;
mod listener;
mod session;

pub use kinds::*;
pub use listener::TcpListenerActor;
pub use session::TcpSessionActor;
// `TcpListenerConfig` and `TcpSessionConfig` are child-actor init
// bundles holding raw `TcpListener` / `TcpStream` handles, consumed
// only by the runtime halves (`runtime.rs`, `listener/runtime.rs`,
// `session/runtime.rs`), so `config` rides the `feature = "runtime"`
// gate. The actor markers themselves (above) are always-on so consumers
// can name them in `ctx.sender()`-driven code and in harness child lookups.
#[cfg(feature = "runtime")]
pub use config::{TcpListenerConfig, TcpSessionConfig};

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
/// own listener entries, each with the proof its spawn returned; it does NOT
/// walk the chassis-wide actor registry to enumerate children.
#[actor(singleton, root)]
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
