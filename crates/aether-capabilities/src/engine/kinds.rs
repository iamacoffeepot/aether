//! `aether.engine.*` mail kinds the engine capability owns (ADR-0121).
//!
//! The engine-internal control-plane vocabulary — proxy forwarding
//! (`ForwardEnvelope` / `RouteEnvelope`), call settlement (`CallSettled`),
//! and fleet liveness (`EngineHeartbeatTick` / `EngineDied` /
//! `EngineAlive`). Each is consumed only inside `aether-capabilities` and
//! embedded in no kind that stays in `aether-kinds`, so the engine cap
//! owns it here (capabilities → kinds is the allowed dependency
//! direction; the embedded `DeathReason` re-imports back from
//! `aether_kinds`).
//!
//! The engine cap's request / result / descriptor kinds
//! (`SpawnEngine`, `ListEngines`, `TerminateEngine`, the upload / resolve
//! families, and their support descriptors) stay in `aether-kinds`: they
//! are the MCP harness's RPC protocol, and `aether-mcp` consumes them
//! while being barred from depending on `aether-capabilities`.

use aether_data::{Kind, KindId, MailboxId, Schema};
use aether_kinds::DeathReason;
use serde::{Deserialize, Serialize};

/// `aether.engine.forward` — hand a per-engine proxy
/// (`aether.engine.proxy:<id>`) one mail to relay to its substrate
/// over the proxy's outbound RPC connection. Issue 763 P3.
///
/// Carries the *remote* target explicitly: a plain mail to the
/// proxy is only `kind` + `payload` — it can't say *which mailbox
/// on the substrate* to deliver to. `ForwardEnvelope` is that
/// carrier. The proxy wraps `mailbox` + `kind` + the already-encoded
/// `payload` into an RPC `Call`; the substrate's
/// `RpcServerCapability` dispatches it into its local actor system.
/// Any reply streams back through the proxy and routes to whoever
/// sent this `ForwardEnvelope` — the proxy keys reply correlation
/// off the inbound mail's `Source`.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.engine.forward")]
pub struct ForwardEnvelope {
    pub mailbox: MailboxId,
    pub kind: KindId,
    pub payload: Vec<u8>,
}

/// `aether.engine.route` — ask the engines cap (`aether.engine`) to
/// relay one mail to a *specific* engine's substrate. Issue 763 P5a.
///
/// The engine-addressed sibling of [`ForwardEnvelope`]: where
/// `ForwardEnvelope` already names a proxy and only needs the
/// substrate-local `mailbox` + `kind` + `payload`, `RouteEnvelope`
/// also carries the `engine_id`, because the sender (the hub's
/// `RpcServerCapability`, relaying an `engine = Some(_)` wire
/// `Call`) doesn't know which proxy hosts that engine. The engines
/// cap looks the engine up in its table and re-emits a
/// `ForwardEnvelope` at the right `aether.engine.proxy:<id>`,
/// propagating the original reply-to so the substrate's reply
/// streams back to the originating `RpcServerCapability`.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.engine.route")]
pub struct RouteEnvelope {
    pub engine_id: String,
    pub mailbox: MailboxId,
    pub kind: KindId,
    pub payload: Vec<u8>,
}

/// `aether.engine.call_settled` — a per-engine proxy's signal that
/// a forwarded RPC call has run to completion. Issue 763 P5a.
///
/// When the proxy relays a [`ForwardEnvelope`] as an RPC `Call`,
/// the substrate eventually answers with a wire `ReplyEnd`. The
/// proxy lifts that terminal frame into this kind and pushes it
/// back to whoever opened the call (correlation preserved) — the
/// hub's `RpcServerCapability` matches it to the in-flight wire
/// call and writes its own `ReplyEnd` to the RPC client. (Local,
/// non-forwarded calls close on chassis settlement instead; a
/// forwarded call has no local chain to settle, so it needs this
/// explicit terminal signal.) `Err` carries the wire `RpcError`
/// rendered as a string — the structured variant doesn't survive
/// the `aether-kinds` layer, which can't depend on the RPC crate.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.engine.call_settled")]
pub enum CallSettled {
    Ok,
    Err { error: String },
}

/// `aether.engine.heartbeat_tick` — the per-engine proxy's own
/// liveness timer wake (issue 1339). Internal control-plane mail,
/// not a user surface: a sidecar thread the proxy spawns at init
/// fires this (empty-payload) at the proxy's own mailbox every
/// heartbeat interval, the same wake-mail shape `RpcInboundReady`
/// uses for the reader sidecar. The handler pings the substrate and
/// counts consecutive misses, evicting the engine once the miss
/// limit is crossed.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.engine.heartbeat_tick")]
pub struct EngineHeartbeatTick {}

/// `aether.engine.died` — a per-engine proxy telling the engines
/// cap (`aether.engine`) that its substrate is gone, so the cap
/// drops it from the supervised-engine table (issue 1339). The
/// proxy sends this when it observes the connection close (`Bye` /
/// `eof`) or when the liveness heartbeat crosses its miss limit —
/// the positive signal the lazy connection-drop path misses for a
/// wedged-but-alive engine. Idempotent on the cap side: a `died`
/// for an already-removed engine (e.g. one a concurrent
/// `TerminateEngine` already dropped) is a no-op. `engine_id` is
/// the plain UUID string, matching `TerminateEngine`.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.engine.died")]
pub struct EngineDied {
    pub engine_id: String,
    /// Why the proxy is reporting the death, so the cap can record it
    /// into its recently-died ring: `Crashed` for a connection-close
    /// (`Bye` / eof), `Evicted` for a heartbeat miss-limit crossing. A
    /// deliberate terminate never sends `EngineDied` — the cap records
    /// `Terminated` itself at the removal site.
    pub reason: DeathReason,
}

/// `aether.engine.alive` — a per-engine proxy reporting a confirmed
/// liveness signal (a `Pong` answering its heartbeat `Ping`) to the
/// engines cap (issue 1339). The cap stamps the engine's
/// last-seen-alive time so `ListEnginesResult` can report
/// `last_heartbeat_age_millis`. Fire-and-forget; an `alive` for an
/// unknown engine is a no-op. `engine_id` is the plain UUID string.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.engine.alive")]
pub struct EngineAlive {
    pub engine_id: String,
}
