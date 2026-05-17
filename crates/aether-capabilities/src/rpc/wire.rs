//! `aether.rpc` wire vocabulary (issue 750 phase 1).
//!
//! Length-prefix postcard frames carrying [`WireFrame`] bodies, layered
//! over the generic stream helpers in `aether-codec::frame` (ADR-0072).
//! The `RpcServerCapability` (phase 2) speaks this wire over a TCP
//! socket; the wire is intentionally type-erased — endpoints are mail
//! kinds, not request enums, so any new mail kind both sides understand
//! is reachable without a wire change.
//!
//! The full design (peer model, dispatch flow, settlement signalling)
//! is on issue 750.

use aether_data::{EngineId, KindId, MailboxId};
use serde::{Deserialize, Serialize};

/// Wire-format version negotiated at handshake. Bump on any breaking
/// shape change to [`WireFrame`] or its substructs; mismatched peers
/// get kicked (no downgrade, no negotiation in v1 per issue 750).
pub const WIRE_VERSION: u32 = 1;

/// One frame on the wire. Length-prefix-framed via
/// [`aether_codec::frame`]; postcard-encoded body.
///
/// `cid` correlates a `Call` to its replies. `Call { cid: None }` is
/// fire-and-forget; `Call { cid: Some(n) }` expects zero or more
/// `ReplyEvent { cid: n, .. }` frames followed by exactly one
/// `ReplyEnd { cid: n, .. }` frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireFrame {
    Hello(Hello),
    HelloAck(HelloAck),
    /// Caller-to-server dispatch request. `cid = None` skips reply
    /// tracking entirely; `cid = Some(n)` opens an in-flight entry the
    /// server closes with `ReplyEnd { cid: n, .. }`.
    Call {
        cid: Option<u64>,
        envelope: MailEnvelope,
    },
    /// One reply mail observed in the trace chain of `cid`'s call.
    /// 0..n per cid; the server emits one for every mail addressed
    /// back at the `RpcServer` mailbox with `correlation_id = cid`.
    ReplyEvent {
        cid: u64,
        envelope: MailEnvelope,
    },
    /// Settlement notice for `cid` — the trace root of the original
    /// `Call` has settled (per ADR-0080). Exactly one per cid. After
    /// this frame the server discards all state for `cid` and ignores
    /// any further mail addressed with that correlation id.
    ReplyEnd {
        cid: u64,
        result: Result<(), RpcError>,
    },
    /// Liveness probe. Caller sends a `Ping(token)`; peer mirrors as
    /// `Pong(token)`. Token is opaque — typically a monotonic counter
    /// for round-trip-time measurement.
    Ping(u64),
    Pong(u64),
    /// Graceful shutdown notice. The sender will close the connection
    /// after writing this frame; the receiver drops its in-flight
    /// state for the connection. Not required — TCP close is also a
    /// valid shutdown — but lets the peer log a structured reason.
    Bye {
        reason: String,
    },
}

/// First frame sent by either side on a fresh connection. The server
/// replies with [`HelloAck`]; mismatched `wire_version` kicks the
/// connection (no downgrade).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub wire_version: u32,
    pub peer: PeerKind,
}

/// Server's response to [`Hello`]. Mirrors the wire version (so the
/// caller can confirm the server agrees) and identifies the server's
/// own peer kind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub wire_version: u32,
    pub server: PeerKind,
}

/// Who's on the other end of a connection.
///
/// - `Substrate` peers (chassis hosting actors) declare their engine
///   identity + kind vocabulary so callers know which kinds the engine
///   can dispatch. `kinds` is intentionally shallow for v1 — fuller
///   schema rides in a future `describe_kinds` RPC kind rather than
///   bloating every handshake.
/// - `Client` peers (CLI / TUI / external) just identify themselves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerKind {
    Substrate {
        engine_name: String,
        engine_version: String,
        kinds: Vec<KindDescriptor>,
    },
    Client {
        client_name: String,
        client_version: String,
    },
}

/// Minimal kind-vocabulary entry carried in [`PeerKind::Substrate`].
/// V1 carries id + name only; structural detail (handler list, schema
/// shape) lives behind a `describe_kinds` RPC kind rather than the
/// handshake so the handshake stays cheap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindDescriptor {
    pub id: KindId,
    pub name: String,
}

/// One mail envelope on the wire.
///
/// `to` is the destination — `engine = None` means "this server's
/// local actor system". The hub later cross-routes `engine = Some(_)`
/// envelopes to the named substrate; for v1 the server rejects
/// non-local targets with [`RpcError::UnsupportedTarget`].
///
/// `from` is `Some` when the originator wants replies (mail back at
/// the `RpcServer` with `correlation_id = cid` round-trips to this
/// peer); `None` is fire-and-forget at the envelope layer regardless
/// of whether the outer `Call.cid` is set.
///
/// `correlation_id` is the mail-system correlation that responders
/// use to `ctx.reply()` against. `RpcServer` sets this to the outer
/// `Call.cid` on dispatch so any actor in the trace chain that
/// replies routes back to the originating peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailEnvelope {
    pub to: MailboxAddress,
    pub from: Option<MailboxAddress>,
    pub kind: KindId,
    pub correlation_id: Option<u64>,
    pub payload: Vec<u8>,
}

/// Engine-aware mailbox address. `engine = None` resolves against the
/// local actor system; `engine = Some(_)` is the hub-routing case
/// (parked for v1, see [`RpcError::UnsupportedTarget`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MailboxAddress {
    pub engine: Option<EngineId>,
    pub mailbox: MailboxId,
}

impl MailboxAddress {
    /// Address a local mailbox (no engine routing).
    #[must_use]
    pub const fn local(mailbox: MailboxId) -> Self {
        Self {
            engine: None,
            mailbox,
        }
    }
}

/// Reasons a `Call` can fail before the trace chain settles. v1 keeps
/// the variant set small — most failures (handler panics, decode
/// errors, etc.) surface as a `ReplyEvent` carrying a result kind
/// from the responder, not an `RpcError`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RpcError {
    /// The target mailbox isn't registered in this server's local
    /// actor system.
    UnknownMailbox { mailbox: MailboxId },
    /// The kind id isn't in this server's kind registry.
    UnknownKind { kind: KindId },
    /// Target carried `engine = Some(_)` — cross-engine routing is
    /// a phase-3 concern.
    UnsupportedTarget { reason: String },
    /// Catch-all for anything else (decode failures on the envelope
    /// payload, internal errors).
    Other { reason: String },
}
