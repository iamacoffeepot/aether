//! Hub-chassis loopback: the hub as one of its own engines.
//!
//! ADR-0034 Phase 2 sub-phase A. The hub chassis boots its own
//! `SubstrateBoot` (wasmtime + scheduler + mailer + registry +
//! control plane) and registers itself in its own `EngineRegistry`
//! under the reserved `HUB_SELF_ENGINE_ID`. After this, every
//! existing MCP tool (`list_engines`, `describe_kinds`,
//! `load_component`, `send_mail`, `engine_logs`,
//! `describe_component`) works uniformly against the hub — no
//! per-tool special-casing, no new protocol.
//!
//! Two channels bridge the in-process engine with the hub's routing
//! layer:
//!
//!   - **Inbound** (`tokio::mpsc<HubToEngine>`): `mail_tx` goes into
//!     the hub-self `EngineRecord`. MCP tools push frames at the
//!     hub the same way they push at any other engine; the
//!     inbound-drainer task resolves each `Mail` frame against the
//!     substrate's `Registry` and pushes onto the substrate's
//!     `Mailer` via `dispatch_hub_to_engine_mail` — the exact path a
//!     remote engine's `HubClient` reader would take over TCP.
//!
//!   - **Outbound** (`std::sync::mpsc<EngineToHub>`): attached to
//!     `SubstrateBoot.outbound` via `HubOutbound::attach`. Any frame
//!     the substrate emits (component observation `Mail`, control
//!     plane `KindsChanged`, captured `LogBatch`) drains through the
//!     outbound-drainer thread and is dispatched into the hub's
//!     `SessionRegistry` / `EngineRegistry` / `LogStore` the same
//!     way `engine.rs::read_loop` handles frames arriving off a
//!     remote engine's TCP socket.
//!
//! Self-dialling is explicitly avoided: the boot uses
//! `SubstrateBootBuilder::skip_upstream_hub` so the substrate does
//! not try to `HubClient::connect` to its own TCP listener (which
//! wouldn't be bound yet at `new()` time anyway).

use std::collections::HashMap;
use std::sync::Arc;

use aether_hub_protocol::{EngineId, EngineToHub, HubToEngine, Uuid};
use aether_substrate_core::{SubstrateBoot, dispatch_hub_to_engine_mail};
use tokio::sync::mpsc;

use crate::log_store::LogStore;
use crate::registry::{EngineRecord, EngineRegistry};
use crate::session::SessionRegistry;

/// Reserved `EngineId` for the hub's own loopback engine. A nil UUID
/// is externally unreachable (engines minted by the TCP handshake
/// always get `Uuid::new_v4()`), and it surfaces uniquely in
/// `list_engines` output so agents can pick it out without a runtime
/// branch.
pub const HUB_SELF_ENGINE_ID: EngineId = EngineId(Uuid::nil());

/// Bound on the hub-self inbound mpsc. Matches the per-engine TCP
/// writer capacity in `engine.rs` so MCP tools see the same
/// back-pressure shape against the hub as against any remote engine.
const INBOUND_CHANNEL_CAPACITY: usize = 256;

/// Everything the hub-chassis needs to drive the in-process engine:
/// the `SubstrateBoot` whose workers and runtime handles must
/// outlive the chassis, plus the two receiver ends of the bridge
/// channels. The chassis spawns two drainer tasks in its tokio
/// runtime that pull from these receivers.
pub struct LoopbackEngine {
    pub boot: SubstrateBoot,
    pub inbound_rx: mpsc::Receiver<HubToEngine>,
    pub outbound_rx: std::sync::mpsc::Receiver<EngineToHub>,
}

impl LoopbackEngine {
    /// Boot the in-process substrate, wire the two bridge channels,
    /// and register the hub-self engine record. Must be called
    /// before the hub's TCP + MCP listeners start so the registry
    /// contains the hub-self entry by the time an MCP client
    /// connects.
    pub fn boot(engines: &EngineRegistry) -> wasmtime::Result<Self> {
        let boot = SubstrateBoot::builder("aether-substrate-hub", env!("CARGO_PKG_VERSION"))
            .skip_upstream_hub()
            .build()?;

        let (outbound_tx, outbound_rx) = std::sync::mpsc::channel::<EngineToHub>();
        boot.outbound.attach(outbound_tx);

        let (inbound_tx, inbound_rx) = mpsc::channel::<HubToEngine>(INBOUND_CHANNEL_CAPACITY);

        engines.insert(EngineRecord {
            id: HUB_SELF_ENGINE_ID,
            name: "aether-substrate-hub".to_owned(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            kinds: boot.boot_descriptors.clone(),
            components: HashMap::new(),
            mail_tx: inbound_tx,
            spawned: false,
        });

        Ok(Self {
            boot,
            inbound_rx,
            outbound_rx,
        })
    }
}

/// Drain `inbound_rx` and push each `Mail` frame onto the
/// substrate's scheduler. Runs for the lifetime of the tokio
/// runtime; exits when every `mail_tx` clone is dropped (which in
/// practice means the hub is shutting down).
///
/// Non-`Mail` variants are frames the hub would normally send to a
/// remote engine over TCP. They're harmless to ignore here: the hub
/// will never send `Welcome` twice (we never minted it in the first
/// place), heartbeats aren't meaningful to ourselves, and `Goodbye`
/// is handled by runtime shutdown rather than a frame.
pub async fn run_inbound_drainer(
    mut inbound_rx: mpsc::Receiver<HubToEngine>,
    registry: Arc<aether_substrate_core::Registry>,
    queue: Arc<aether_substrate_core::Mailer>,
) {
    while let Some(frame) = inbound_rx.recv().await {
        match frame {
            HubToEngine::Mail(mail) => dispatch_hub_to_engine_mail(mail, &registry, &queue),
            HubToEngine::Heartbeat | HubToEngine::Welcome(_) | HubToEngine::Goodbye(_) => {}
        }
    }
}

/// Drain `outbound_rx` and dispatch each frame into the hub's
/// routing layer exactly as `engine.rs::read_loop` would for a
/// remote engine. Runs on a dedicated std::thread because the
/// `std::sync::mpsc::Receiver::recv` call blocks — a tokio task
/// would stall a runtime worker for the thread's lifetime. Exits
/// when the channel closes (every `Sender` clone held inside
/// `HubOutbound` has dropped, which happens at process shutdown).
///
/// `Mail` frames are routed to Claude sessions via the same
/// `crate::engine::route_engine_mail` path remote engines' mail
/// takes (the function is sync — `tokio::sync::mpsc::Sender::try_send`
/// is non-async — so no runtime handle is needed here);
/// `KindsChanged` updates the hub's per-engine descriptor cache;
/// `LogBatch` appends into the shared `LogStore`. `Hello` /
/// `Heartbeat` / `Goodbye` are silently ignored — the loopback
/// substrate never emits them (no handshake to initiate, heartbeat
/// is meaningless against ourselves, shutdown happens via channel
/// close rather than a frame).
pub fn spawn_outbound_drainer(
    outbound_rx: std::sync::mpsc::Receiver<EngineToHub>,
    engines: EngineRegistry,
    sessions: SessionRegistry,
    logs: LogStore,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("hub-loopback-outbound".to_owned())
        .spawn(move || {
            while let Ok(frame) = outbound_rx.recv() {
                match frame {
                    EngineToHub::Mail(m) => {
                        crate::engine::route_engine_mail(&sessions, HUB_SELF_ENGINE_ID, m);
                    }
                    EngineToHub::KindsChanged(kinds) => {
                        engines.update_kinds(&HUB_SELF_ENGINE_ID, kinds);
                    }
                    EngineToHub::LogBatch(entries) => {
                        logs.append(HUB_SELF_ENGINE_ID, entries);
                    }
                    EngineToHub::Hello(_) | EngineToHub::Heartbeat | EngineToHub::Goodbye(_) => {}
                }
            }
        })
        .expect("spawn hub-loopback-outbound thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LoopbackEngine::boot` registers the hub in its own engine
    /// registry under the reserved id, so subsequent MCP tool calls
    /// (which look up engines through the same registry) can reach
    /// the hub without any per-tool special-casing. Smoke-checks
    /// the presence, name, and that declared kinds are non-empty
    /// (the boot seeds `aether_kinds::descriptors::all()`).
    #[test]
    fn boot_registers_self_in_engine_registry() {
        let engines = EngineRegistry::new();
        assert!(engines.is_empty());

        let _loopback = LoopbackEngine::boot(&engines).expect("loopback boot");

        let record = engines
            .get(&HUB_SELF_ENGINE_ID)
            .expect("hub-self registered");
        assert_eq!(record.name, "aether-substrate-hub");
        assert_eq!(record.id, HUB_SELF_ENGINE_ID);
        assert!(!record.spawned, "hub-self is not a spawned child");
        assert!(
            !record.kinds.is_empty(),
            "boot descriptors should be non-empty",
        );
    }
}
