//! `aether.tcp.session` — instanced actor, one per accepted
//! connection. Owns a `TcpStream` (split for read/write) and a
//! sidecar read thread that loops on blocking `read()`. The read
//! thread pushes byte chunks (or an EOF / error signal) over an
//! mpsc and fires a [`SessionDataReady`] mail at this actor's own
//! mailbox; the dispatcher drains them.
//!
//! Writes go directly from the dispatcher thread (`on_session_write`
//! does a blocking `write_all` on the write half). The read path
//! needs the sidecar because `read()` blocks indefinitely until
//! peer data or close; the write path doesn't need it because the
//! caller initiates writes synchronously and they're typically
//! fast.
//!
//! Shutdown: `unwire` flips the read thread's shutdown flag and
//! calls `stream.shutdown(Both)` on the write half. The kernel
//! aborts any blocked `read()` on the read half, the read thread
//! sees the error / EOF, exits, and the dispatcher joins it.
//!
//! Issue 775 retired the publish path: pre-#775 the dispatcher
//! re-broadcast every chunk as `SessionData` and the close as
//! `SessionClosed` through the `hub.claude.broadcast` mailbox.
//! With `BroadcastCapability` gone the chassis no longer fans
//! observation out, so this actor drops bytes on the floor today.
//! A future user-space TCP observer (monitor-based or session-
//! targeted mail) is the replacement path.

// Handler-signature kinds need to be importable at file root for
// the `#[actor]`-emitted `HandlesKind` markers against the identity
// (always-on, outside the `feature = "runtime"` gate).
use super::kinds::{SessionClose, SessionDataReady, SessionWrite};

/// `aether.tcp.session` **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the instanced
/// `OnePer("connection")` name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`TcpSessionState`, which holds the
/// `TcpStream` write half + the read thread) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names
/// `TcpSessionState` nor pulls `aether_substrate` through this actor.
#[actor(instanced)]
pub struct TcpSessionActor;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` / `std::net` type — the
// handler/init ctx, the runtime state, the read thread, and the
// `#[runtime] impl NativeActor` itself — lives in the `runtime` module below,
// gated once by `feature = "runtime"`.
use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
