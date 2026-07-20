use aether_data::{KindId, MailboxId as DataMailboxId};
use aether_kinds::{Present, Render, Shutdown, Tick};

use super::super::LifecycleGraphData;

/// Construction-time configuration for `LifecycleCapability`.
/// Carries the compiled data graph + the initial subscriber wiring.
/// Built per-chassis at builder time and consumed by `init`.
pub struct LifecycleConfig {
    /// The compiled lifecycle graph. Built via
    /// [`LifecycleGraphData::builder`](LifecycleGraphData::builder)
    /// on the chassis side.
    pub graph: LifecycleGraphData,
    /// Initial `(stage_kind, mailbox)` pairs to populate the
    /// subscriber table at boot — a chassis builder can pre-subscribe
    /// a mailbox to a stage this way without round-tripping a
    /// `LifecycleSubscribe` mail. Each pair must
    /// reference a stage kind declared by `graph` — the boot path
    /// verifies this and returns `BootError` otherwise, so
    /// misconfiguration fails fast at chassis-build.
    pub initial_subscribers: Vec<(KindId, DataMailboxId)>,
    /// Force-complete deadline for a pending advance's `Settled`
    /// (iamacoffeepot/aether#1048), in milliseconds. Resolved
    /// chassis-side (env override over [`Self::ADVANCE_TIMEOUT_MS_DEFAULT`])
    /// rather than read from the environment in `init`, so the cap
    /// configures through this struct rather than a naked env read.
    pub advance_timeout_millis: u64,
}

impl LifecycleConfig {
    /// Default force-complete deadline (ms) for a pending advance.
    /// Chassis builders that don't override use this.
    pub const ADVANCE_TIMEOUT_MS_DEFAULT: u64 = super::settlement::ADVANCE_TIMEOUT_MS_DEFAULT;
}

/// Build the three-stage frame lifecycle config the display-driving
/// chassis share (ADR-0082 §11, issues 1378 + 1489):
/// `Tick → Render → Present → Tick` (looping), with the `Quit` escape to
/// a `Shutdown` terminal on the `Present` stage. The chassis drives a
/// full `Tick → Render → Present` cycle per frame; `Render` broadcasts
/// only after the entire `Tick` chain has settled (ADR-0080 §6), so a
/// render producer's `on_render` runs once every actor's per-frame
/// `Tick` compute is done — no submitting against half-updated
/// cross-actor state.
///
/// The `Quit` escape lives on `Present`, not `Tick`: a `quit_pending`
/// flag set mid-frame is consumed only once the cap reaches `Present`,
/// so the in-flight frame has broadcast its full `Tick → Render →
/// Present` cycle before the lifecycle advances to `Shutdown` (ADR-0082
/// §3 "drain the frame before exit"). `Present` is a chassis-GPU-work
/// ordering point with an empty subscriber set today — it exists to host
/// this drain edge; per-stage component subscription lands when a
/// producer needs a post-`Render` hook.
///
/// Components subscribe the `Tick` (and `Render`) stage directly on
/// `aether.lifecycle` (ADR-0082 §7/§11), so the config wires no initial
/// subscribers. The desktop chassis and the substrate harness adopt this
/// graph; headless stays on its tick-only graph (its render cap is a
/// no-op, so a `Render` / `Present` stage would settle to no GPU work).
///
/// `advance_timeout_millis` is the chassis-resolved deadline (or
/// [`LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT`]).
///
/// # Panics
/// Panics if the (compile-time-fixed) graph fails to build — it can't,
/// the shape is structurally valid; the `expect` documents the
/// invariant.
#[must_use]
pub fn frame_lifecycle_config(advance_timeout_millis: u64) -> LifecycleConfig {
    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Render>()
        .state::<Render>()
        .next::<Present>()
        .state::<Present>()
        .next::<Tick>()
        .quit::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("frame lifecycle graph is structurally valid");
    LifecycleConfig { graph, initial_subscribers: vec![], advance_timeout_millis }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_data::Kind;

    #[test]
    fn frame_lifecycle_graph_is_tick_render_present_with_shutdown_terminal() {
        // ADR-0082 §11 / issues 1378 + 1489: the display-driving chassis
        // graph is `Tick → Render → Present → Tick` (looping) with the
        // `Quit` escape to a `Shutdown` terminal on the `Present` stage.
        // The graph's edge accessors (`next` / `quit` per state) are
        // module-private, so this check reads the public `Debug` (start +
        // the non-terminal state kinds + terminals) plus the empty
        // `initial_subscribers` set. Quit-edge *placement* (on `Present`,
        // not `Tick`) is verified by the `resolve_edge` tests and
        // end-to-end by the substrate-harness quit-drain scenario.
        let cfg = frame_lifecycle_config(LifecycleConfig::ADVANCE_TIMEOUT_MS_DEFAULT);
        let graph_dbg = format!("{:?}", cfg.graph);
        let tick = format!("{:?}", <Tick as Kind>::ID);
        let render = format!("{:?}", <Render as Kind>::ID);
        let present = format!("{:?}", <Present as Kind>::ID);
        let shutdown = format!("{:?}", <Shutdown as Kind>::ID);

        // Start state is Tick.
        assert!(graph_dbg.contains(&format!("start: {tick}")), "expected start Tick in {graph_dbg}");
        // Tick, Render, and Present are all non-terminal states.
        assert!(graph_dbg.contains(&render), "expected a Render state in {graph_dbg}");
        assert!(graph_dbg.contains(&present), "expected a Present state in {graph_dbg}");
        // Shutdown is the sole terminal.
        assert!(graph_dbg.contains(&format!("terminals: [{shutdown}]")), "expected Shutdown terminal in {graph_dbg}");

        // No initial subscribers: components subscribe the `Tick` stage
        // directly on `aether.lifecycle` (ADR-0082 §7/§11).
        assert!(cfg.initial_subscribers.is_empty());
    }
}
