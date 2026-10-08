//! `aether.tcp.session` — instanced actor, one per accepted or dialed
//! connection. Owns a `TcpStream` (split for read/write) and a
//! sidecar read thread that loops on blocking `read()`. The read
//! thread pushes byte chunks (or an EOF / error signal) over an
//! mpsc and fires a [`SessionDataReady`](crate::kinds::SessionDataReady) mail at this actor's own
//! mailbox; the dispatcher drains them.
//!
//! Writes go directly from the dispatcher thread (`on_session_write`
//! does a blocking `write_all` on the write half). The read path
//! needs the sidecar because `read()` blocks indefinitely until
//! peer data or close; the write path doesn't need it because the
//! caller initiates writes synchronously and they're typically
//! fast.
//!
//! Lifetime: a session lives until its peer closes or a read fails, a
//! frame is rejected, a write fails, it is sent
//! [`SessionClose`](crate::kinds::SessionClose), or the engine tears down.
//! It watches nothing itself. Whoever spawned it sends that `SessionClose`
//! when its own reason to keep the session ends: the cap, for a session it
//! dialed, when the consumer closes, and the listener, for a session it
//! accepted, when the listener closes. A session closed that way sends no
//! `SessionClosed`.
//!
//! Shutdown: `unwire` flips the read thread's shutdown flag and
//! calls `stream.shutdown(Both)` on the write half. The kernel
//! aborts any blocked `read()` on the read half, the read thread
//! sees the error / EOF, exits, and the dispatcher joins it.
//!
//! Each session receives its consumer from its listener or outbound
//! `Connect` request, held as a `ProtocolRef` over
//! [`TcpConsumer`](crate::kinds::TcpConsumer) that the cap proved at
//! receipt. The dispatcher appends read chunks to a reassembly
//! buffer, pops complete ADR-0072 length-prefix frames, and delivers one
//! targeted `SessionData` mail per frame. Peer EOF and read errors produce
//! a targeted `SessionClosed`.

use super::{TcpCapability, TcpListenerActor};

/// `aether.tcp.session` **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`), the
/// per-handler `HandlesKind` markers, and the instanced
/// `OnePer("connection")` name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`TcpSessionState`, which holds the
/// `TcpStream` write half + the read thread) lives behind the one
/// `feature = "runtime"` gate, so a transport-only build never names
/// `TcpSessionState` nor pulls `aether_substrate` through this actor.
#[actor(instanced, child_of(TcpCapability, TcpListenerActor))]
pub struct TcpSessionActor;

// The `#[actor]` attribute path stays always-on (the macro divides what it
// emits). Everything that names an `aether_substrate` / `std::net` type — the
// handler/init ctx, the runtime state, the read thread, and the
// `#[runtime] impl NativeActor` itself — lives in the `runtime` module below,
// gated once by `feature = "runtime"`.
use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
