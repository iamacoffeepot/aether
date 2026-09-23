//! `aether.rpc` mail kinds owned by the RPC server capability (ADR-0121).

use aether_data::{EngineId, KindId, MailboxId};

/// `aether.rpc.inbound_ready` — sidecar accept / read thread →
/// `RpcServerCapability` dispatcher wake. Issue 750. Mirrors the
/// `ConnectionReady` / `SessionDataReady` pattern for `aether.tcp`:
/// the sidecar pushes work over an internal mpsc and fires this
/// (empty-payload) mail at the cap's mailbox so the dispatcher
/// handler drains the queue. The mpsc carries the live data
/// (`TcpStream`, frame bytes, close reason) — a `TcpStream` isn't
/// wire-shaped and a frame's payload may be megabytes, so the mail
/// is only the wakeup signal.
#[aether_data::kind(name = "aether.rpc.inbound_ready", default)]
pub struct RpcInboundReady {}

/// `aether.rpc.forward` — hand a per-engine proxy one mail to relay to
/// its substrate over the proxy's outbound RPC connection. Issue 763 P3.
///
/// Carries the *remote* target explicitly: a plain mail to the
/// proxy is only `kind` + `payload` — it can't say *which mailbox
/// on the substrate* to deliver to. `ForwardEnvelope` is that
/// carrier. The hub's `RpcServerCapability` sends it to the proxy
/// registered for an `engine = Some(_)` wire `Call`'s engine; the proxy
/// wraps `mailbox` + `kind` + the already-encoded `payload` into an RPC
/// `Call`, and the substrate's own `RpcServerCapability` proves `mailbox`
/// at receipt and dispatches it into its local actor system. Any reply
/// streams back through the proxy and routes to whoever sent this
/// `ForwardEnvelope` — the proxy keys reply correlation off the inbound
/// mail's `Source`. Hub-internal: it never crosses the RPC wire.
#[aether_data::kind(name = "aether.rpc.forward")]
pub struct ForwardEnvelope {
    pub mailbox: MailboxId,
    pub kind: KindId,
    #[serde(with = "aether_data::bytes")]
    pub payload: Vec<u8>,
}

/// `aether.rpc.register_engine_route` — a per-engine proxy asking the
/// hub's `RpcServerCapability` to forward every `engine = Some(engine_id)`
/// wire `Call` to it.
///
/// A proxy sends this from its `wire` hook, for its own engine. The
/// registrant is the envelope sender, kept as a proven reference, so the
/// kind names no position. One registrant owns one engine, and an engine
/// id already held by a different registrant is refused. The route lasts
/// until the registrant departs. The answer is
/// [`RegisterEngineRouteResult`].
#[aether_data::kind(name = "aether.rpc.register_engine_route")]
pub struct RegisterEngineRoute {
    pub engine_id: EngineId,
}

/// `aether.rpc.register_engine_route_result` — the answer to
/// [`RegisterEngineRoute`]: `Ok` once the route is recorded, or `Err`
/// naming the engine the registration could not take.
#[aether_data::kind(name = "aether.rpc.register_engine_route_result")]
pub enum RegisterEngineRouteResult {
    Ok,
    Err { error: String },
}

/// `aether.rpc.call_settled` — a per-engine proxy's signal that
/// a forwarded RPC call has run to completion. Issue 763 P5a.
///
/// When the proxy relays a `ForwardEnvelope` as an RPC `Call`,
/// the substrate eventually answers with a wire `ReplyEnd`. The
/// proxy lifts that terminal frame into this kind and pushes it
/// back to whoever opened the call (correlation preserved) — the
/// hub's `RpcServerCapability`, which forwarded the call, matches it
/// to the in-flight wire call and writes its own `ReplyEnd` to the
/// RPC client. (Local,
/// non-forwarded calls close on chassis settlement instead; a
/// forwarded call has no local chain to settle, so it needs this
/// explicit terminal signal.) `Err` carries the wire `RpcError`
/// rendered as a string, keeping this terminal signal wire-simple.
#[aether_data::kind(name = "aether.rpc.call_settled")]
pub enum CallSettled {
    Ok,
    Err { error: String },
}
