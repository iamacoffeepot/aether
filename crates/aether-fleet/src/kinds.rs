//! `aether.fleet.*` mail kinds the engine capability owns (ADR-0121).
//!
//! The engine-internal control-plane vocabulary — fleet liveness
//! (`EngineHeartbeatTick` / `EngineDied` / `EngineAlive`) and the restart
//! timer (`EngineRestartDue`). Proxy forwarding (`ForwardEnvelope`) is the
//! hub RPC server's kind and lives in `aether-rpc`. Each is consumed only
//! inside this crate and embedded in no kind that stays in `aether-kinds`,
//! so the engine cap owns it here (cap crate → kinds is the allowed
//! dependency direction; the embedded `DeathReason` re-imports back from
//! `aether_kinds`).
//!
//! The engine cap's request / result / descriptor kinds
//! (`SpawnEngine`, `ListEngines`, `TerminateEngine`, the upload / resolve
//! families, and their support descriptors) stay in `aether-kinds`: they
//! are the MCP harness's RPC protocol, and `aether-mcp` consumes them
//! while being barred from depending on a cap crate.

use aether_kinds::DeathReason;

/// `aether.fleet.heartbeat_tick` — the per-engine proxy's own
/// liveness timer wake (issue 1339). Internal control-plane mail,
/// not a user surface: a sidecar thread the proxy spawns at init
/// fires this (empty-payload) at the proxy's own mailbox every
/// heartbeat interval, the same wake-mail shape `RpcInboundReady`
/// uses for the reader sidecar. The handler pings the substrate and
/// counts consecutive misses, evicting the engine once the miss
/// limit is crossed.
#[aether_data::kind(name = "aether.fleet.heartbeat_tick", default)]
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
#[aether_data::kind(name = "aether.fleet.died")]
pub struct EngineDied {
    pub engine_id: String,
    /// Why the proxy is reporting the death, so the cap can record it
    /// into its recently-died ring: `Crashed` for a connection-close
    /// (`Bye` / eof), `Evicted` for a heartbeat miss-limit crossing. A
    /// deliberate terminate never sends `EngineDied` — the cap records
    /// `Terminated` itself at the removal site.
    pub reason: DeathReason,
}

/// `aether.fleet.restart_due` — the engines cap's own restart-backoff
/// timer wake. Internal control-plane mail, not a user surface.
///
/// When the cap decides to restart a dead engine it files the spawn
/// recipe under a token and hands that token to a one-shot timer thread;
/// the thread sleeps out the configured backoff and fires this at the
/// cap's own mailbox, the same wake-mail shape the proxy's heartbeat
/// sidecar uses. The handler looks the token up and re-forks.
///
/// The token, not the recipe, is what crosses: a spawn recipe carries
/// argv and a store hash, and keeping it in cap state means this kind
/// stays a bare alarm and the recipe never needs a wire encoding. A
/// token with no pending entry — the restart was already settled, or the
/// cap was rebuilt around it — is a silent no-op.
#[aether_data::kind(name = "aether.fleet.restart_due", default)]
pub struct EngineRestartDue {
    pub token: u64,
}

/// `aether.fleet.alive` — a per-engine proxy reporting a confirmed
/// liveness signal (a `Pong` answering its heartbeat `Ping`) to the
/// engines cap (issue 1339). The cap stamps the engine's
/// last-seen-alive time so `ListEnginesResult` can report
/// `last_heartbeat_age_millis`. Fire-and-forget; an `alive` for an
/// unknown engine is a no-op. `engine_id` is the plain UUID string.
#[aether_data::kind(name = "aether.fleet.alive")]
pub struct EngineAlive {
    pub engine_id: String,
}
