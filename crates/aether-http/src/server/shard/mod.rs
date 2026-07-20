//! `aether.http.server.shard` — instanced dispatch shard of the HTTP server
//! (ADR-0135). The supervisor (`aether.http.server`) assigns each accepted
//! connection to one shard round-robin; from then on the connection lives
//! entirely here — this actor spawns and controls the per-connection reader,
//! owns the connection / in-flight / stream / websocket tables for its slice,
//! dispatches requests to the handler, and intercepts the replies. N shards
//! run as independent actors on the worker pool, so the per-request dispatch
//! work parallelizes across cores instead of serializing through the one
//! supervisor actor.
//!
//! The external mail surface (route registration, ADR-0130) stays on the
//! supervisor; nothing addresses a shard by name. Handler replies and
//! stream-phase mails arrive here because the shard is the dispatch source
//! (reply correlation and the ADR-0133 handles both follow the dispatching
//! mailbox).

// Handler-signature kinds resolve at file root through these imports —
// `#[actor]` emits the `HandlesKind<K>` markers always-on against the
// identity, and the handler bodies in `runtime` name these kinds.
use crate::kinds::{
    HttpInboundReady, HttpRequestCredit, HttpResponseChunk, HttpResponseStreamEnd, WebSocketClose, WebSocketMessage,
};
use aether_kinds::trace::Settled;

/// `aether.http.server.shard` **identity** (ADR-0122 identity/runtime split,
/// ADR-0135). A ZST carrying only the addressing — `Addressable`, the
/// per-handler `HandlesKind` markers, the `#[fallback]` reply-interception
/// marker, and the instanced name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`HttpShardState`, the
/// per-connection machine) lives behind the one `feature = "runtime"` gate.
#[actor(instanced)]
pub struct HttpDispatchShard;

use aether_actor::actor;

#[cfg(feature = "runtime")]
mod runtime;
