//! `aether.fleet.*` mail kinds the engine capability owns (ADR-0121).
//!
//! The engine-internal control-plane vocabulary — proxy forwarding
//! (`ForwardEnvelope`) and fleet liveness (`EngineHeartbeatTick` /
//! `EngineDied` / `EngineAlive`). Each is consumed only inside this crate
//! and embedded in no kind that stays in `aether-kinds`, so the engine cap
//! owns it here (cap crate → kinds is the allowed dependency direction; the
//! embedded `DeathReason` re-imports back from `aether_kinds`).
//!
//! The engine cap's request / result / descriptor kinds
//! (`SpawnEngine`, `ListEngines`, `TerminateEngine`, the upload / resolve
//! families, and their support descriptors) stay in `aether-kinds`: they
//! are the MCP harness's RPC protocol, and `aether-mcp` consumes them
//! while being barred from depending on a cap crate.

use aether_data::{Kind, KindId, MailboxId, Schema};
use aether_kinds::DeathReason;
use serde::{Deserialize, Serialize};

/// `aether.fleet.forward` — hand a per-engine proxy
/// (`aether.fleet.proxy:<id>`) one mail to relay to its substrate
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
#[kind(name = "aether.fleet.forward")]
pub struct ForwardEnvelope {
    pub mailbox: MailboxId,
    pub kind: KindId,
    pub payload: Vec<u8>,
}

/// `aether.fleet.heartbeat_tick` — the per-engine proxy's own
/// liveness timer wake (issue 1339). Internal control-plane mail,
/// not a user surface: a sidecar thread the proxy spawns at init
/// fires this (empty-payload) at the proxy's own mailbox every
/// heartbeat interval, the same wake-mail shape `RpcInboundReady`
/// uses for the reader sidecar. The handler pings the substrate and
/// counts consecutive misses, evicting the engine once the miss
/// limit is crossed.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone, Default)]
#[kind(name = "aether.fleet.heartbeat_tick")]
pub struct EngineHeartbeatTick {}

/// `aether.fleet.died` — a per-engine proxy telling the engines
/// cap (`aether.fleet`) that its substrate is gone, so the cap
/// drops it from the supervised-engine table (issue 1339). The
/// proxy sends this when it observes the connection close (`Bye` /
/// `eof`) or when the liveness heartbeat crosses its miss limit —
/// the positive signal the lazy connection-drop path misses for a
/// wedged-but-alive engine. Idempotent on the cap side: a `died`
/// for an already-removed engine (e.g. one a concurrent
/// `TerminateEngine` already dropped) is a no-op. `engine_id` is
/// the plain UUID string, matching `TerminateEngine`.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fleet.died")]
pub struct EngineDied {
    pub engine_id: String,
    /// Why the proxy is reporting the death, so the cap can record it
    /// into its recently-died ring: `Crashed` for a connection-close
    /// (`Bye` / eof), `Evicted` for a heartbeat miss-limit crossing. A
    /// deliberate terminate never sends `EngineDied` — the cap records
    /// `Terminated` itself at the removal site.
    pub reason: DeathReason,
}

/// `aether.fleet.alive` — a per-engine proxy reporting a confirmed
/// liveness signal (a `Pong` answering its heartbeat `Ping`) to the
/// engines cap (issue 1339). The cap stamps the engine's
/// last-seen-alive time so `ListEnginesResult` can report
/// `last_heartbeat_age_millis`. Fire-and-forget; an `alive` for an
/// unknown engine is a no-op. `engine_id` is the plain UUID string.
#[derive(Kind, Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.fleet.alive")]
pub struct EngineAlive {
    pub engine_id: String,
}
