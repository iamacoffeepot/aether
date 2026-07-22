//! `aether.rpc` mail kinds owned by the RPC server capability (ADR-0121).

use aether_data::{Kind, KindId, MailboxId, Schema};
use serde::{Deserialize, Serialize};

/// `aether.rpc.inbound_ready` — sidecar accept / read thread →
/// `RpcServerCapability` dispatcher wake. Issue 750. Mirrors the
/// `ConnectionReady` / `SessionDataReady` pattern for `aether.tcp`:
/// the sidecar pushes work over an internal mpsc and fires this
/// (empty-payload) mail at the cap's mailbox so the dispatcher
/// handler drains the queue. The mpsc carries the live data
/// (`TcpStream`, frame bytes, close reason) — a `TcpStream` isn't
/// wire-shaped and a frame's payload may be megabytes, so the mail
/// is only the wakeup signal.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.rpc.inbound_ready")]
pub struct RpcInboundReady {}

/// `aether.rpc.route` — ask the engines cap (`aether.fleet`) to
/// relay one mail to a *specific* engine's substrate. Issue 763 P5a.
///
/// The engine-addressed sibling of `ForwardEnvelope`: where
/// `ForwardEnvelope` already names a proxy and only needs the
/// substrate-local `mailbox` + `kind` + `payload`, `RouteEnvelope`
/// also carries the `engine_id`, because the sender (the hub's
/// `RpcServerCapability`, relaying an `engine = Some(_)` wire
/// `Call`) doesn't know which proxy hosts that engine. The engines
/// cap looks the engine up in its table and re-emits a
/// `ForwardEnvelope` at the right `aether.fleet.proxy:<id>`,
/// propagating the original reply-to so the substrate's reply
/// streams back to the originating `RpcServerCapability`.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.rpc.route")]
pub struct RouteEnvelope {
    pub engine_id: String,
    pub mailbox: MailboxId,
    pub kind: KindId,
    pub payload: Vec<u8>,
}

/// `aether.rpc.call_settled` — a per-engine proxy's signal that
/// a forwarded RPC call has run to completion. Issue 763 P5a.
///
/// When the proxy relays a `ForwardEnvelope` as an RPC `Call`,
/// the substrate eventually answers with a wire `ReplyEnd`. The
/// proxy lifts that terminal frame into this kind and pushes it
/// back to whoever opened the call (correlation preserved) — the
/// hub's `RpcServerCapability` matches it to the in-flight wire
/// call and writes its own `ReplyEnd` to the RPC client. (Local,
/// non-forwarded calls close on chassis settlement instead; a
/// forwarded call has no local chain to settle, so it needs this
/// explicit terminal signal.) `Err` carries the wire `RpcError`
/// rendered as a string, keeping this terminal signal wire-simple.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.rpc.call_settled")]
pub enum CallSettled {
    Ok,
    Err { error: String },
}
