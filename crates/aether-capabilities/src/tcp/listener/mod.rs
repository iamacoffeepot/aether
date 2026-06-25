//! `aether.tcp.listener` — instanced actor, one per bound port. Owns
//! a `std::net::TcpListener` and a sidecar accept thread that loops
//! on blocking `accept()`. Phase 6b: each accepted connection spawns
//! a `TcpSessionActor` as a child; the sidecar can't call
//! `spawn_child` (no dispatcher ctx), so it pushes the `TcpStream`
//! over an mpsc and fires a `ConnectionReady` wake mail at this
//! actor's own mailbox. The wake handler drains the mpsc and does
//! the spawn on the dispatcher thread.
//!
//! Shutdown: `unwire` flips the accept thread's shutdown flag, then
//! self-connects to the bound port to wake the blocked accept call.
//! The accept returns, sees the flag, breaks; the dispatcher thread
//! (in `unwire`) joins the accept thread.

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers against the
// identity (always-on, outside the `feature = "runtime"` gate).
use super::kinds::{Close, ConnectionReady};

/// `aether.tcp.listener` **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the instanced
/// `OnePer("listener")` name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`TcpListenerState`, which holds the
/// `std::net::TcpListener`'s accept thread + the connection channel) lives
/// behind the one `feature = "runtime"` gate, so a transport-only build never
/// names `TcpListenerState` nor pulls `aether_substrate` through this actor.
#[actor(instanced)]
pub struct TcpListenerActor;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` / `std::net` type — the
// handler/init ctx, the runtime state, the accept thread, and the
// `#[runtime] impl NativeActor` itself — lives in the `runtime` module below,
// gated once by `feature = "runtime"`.
use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
