//! Shared `Builder` boot fragments for the desktop and headless
//! chassis. Both `Chassis::build` impls pre-extraction wired the same
//! 10-cap base (handle, log, trace, input, component-host, fs, http,
//! tcp + the aborter + worker count) and the same optional RPC
//! server tail, with only their renderer + window stack differing.
//! Qodana flagged the parallel chains as duplicated code; this module
//! pulls the shared scaffolding out so each chassis declares only
//! the parts that genuinely differ.
//!
//! The hub and test-bench chassis don't share this base (hub is a
//! minimal RPC-only chassis, test-bench drives a loopback), so the
//! helper module stays scoped to the two full-stack chassis.

use std::net::SocketAddr;
use std::sync::Arc;

use aether_actor::Actor;
use aether_capabilities::anthropic::AnthropicConfigLayer;
use aether_capabilities::audio::AudioConfigLayer;
use aether_capabilities::fs::NamespaceRootsLayer;
use aether_capabilities::gemini::GeminiConfigLayer;
use aether_capabilities::http::HttpConfigLayer;
use aether_capabilities::lifecycle::LifecycleGraphData;
use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{
    AnthropicCapability, AnthropicConfig, ComponentHostCapability, ComponentHostConfig,
    DagCapability, FsCapability, GeminiCapability, GeminiConfig, HandleCapability, HttpCapability,
    InputCapability, InputConfig, InventoryCapability, LifecycleConfig, TcpCapability,
    fs::NamespaceRoots, http::HttpConfig, trace::TraceDispatchCapability,
};
use aether_data::{Kind, MailboxId as DataMailboxId, mailbox_id_from_name};
use aether_kinds::{Render, Shutdown, Tick};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::config::{KnobKind, KnobRecord, KnownKeys, dump_config, known_keys};
use aether_substrate::handle_store::{ENV_MAX_BYTES, PersistConfig, PersistConfigLayer};
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::scheduler::SCHEDULER_KNOBS;
use confique::Config as _;
use confique::meta::Meta;

/// Chassis-direct env knobs that aren't `#[derive(Config)]` fields —
/// the bare-shadowed knobs the chassis bins read inline
/// (`AETHER_WORKERS`, `AETHER_TICK_HZ`, `AETHER_RPC_PORT`, the desktop
/// window knobs) plus the handle-store in-memory budget
/// (`AETHER_HANDLE_STORE_MAX_BYTES`, which `HandleStore::from_env`
/// parses outside confique). Registered as [`KnobRecord`]s so e1's
/// unknown-`AETHER_*` sweep doesn't flag them and e2's `--config`
/// dump lists them. ADR-0090 §1/§4. The scheduler hot-path knobs are
/// registered separately by unit b2's `SCHEDULER_KNOBS`.
pub const CHASSIS_KNOBS: &[KnobRecord] = &[
    KnobRecord {
        env_key: "AETHER_WORKERS",
        doc: "Worker-pool size override (unset → available_parallelism()-1, min 1).",
        default: None,
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_TICK_HZ",
        doc: "Headless tick cadence in hertz.",
        default: Some("60"),
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_RPC_PORT",
        doc: "aether.rpc.server bind port (desktop/headless skip the server when unset).",
        default: None,
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_WINDOW_MODE",
        doc: "Desktop window mode: windowed[:WxH] / fullscreen-borderless / exclusive:WxH@HZ.",
        default: None,
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: "AETHER_WINDOW_TITLE",
        doc: "Desktop window title text.",
        default: None,
        kind: KnobKind::HandRegistered,
    },
    KnobRecord {
        env_key: ENV_MAX_BYTES,
        doc: "Handle-store in-memory soft byte budget (parsed outside confique; \
              unparseable aborts boot per ADR-0090 §4).",
        default: Some("268435456"),
        kind: KnobKind::HandRegistered,
    },
];

/// Assemble the chassis-wide [`KnownKeys`] set (ADR-0090 §4): every
/// migrated `*Layer::META` (http / gemini / anthropic / audio / fs /
/// persist) plus the hand-registered chassis knobs ([`CHASSIS_KNOBS`])
/// and scheduler hot-path knobs (b2's
/// `aether_substrate::scheduler::SCHEDULER_KNOBS`). e1's
/// [`validate_env`](aether_substrate::config::validate_env) sweeps the
/// process env against this; e2's `--config` dump walks the same
/// metas + records.
///
/// Lives bundle-side rather than in `aether-substrate::config` because
/// the cap layer types live in `aether-capabilities`, which depends on
/// `aether-substrate` (not the reverse) — the generic `known_keys`
/// assembly fn is in `config`; the concrete chassis registry is here.
#[must_use]
pub fn chassis_known_keys() -> KnownKeys {
    let (metas, records) = chassis_registry();
    known_keys(metas, &records)
}

/// The chassis-wide config registry: the migrated cap layer `Meta`s
/// plus the hand-registered knob records (`CHASSIS_KNOBS` + b2's
/// `SCHEDULER_KNOBS`). Shared by [`chassis_known_keys`] (e1's sweep)
/// and [`chassis_config_dump`] (e2's `--config`) so both read one
/// source of truth.
fn chassis_registry() -> (&'static [&'static Meta], Vec<KnobRecord>) {
    const METAS: &[&Meta] = &[
        &HttpConfigLayer::META,
        &GeminiConfigLayer::META,
        &AnthropicConfigLayer::META,
        &AudioConfigLayer::META,
        &NamespaceRootsLayer::META,
        &PersistConfigLayer::META,
    ];
    let mut records: Vec<KnobRecord> = CHASSIS_KNOBS.to_vec();
    records.extend_from_slice(SCHEDULER_KNOBS);
    (METAS, records)
}

/// Render the `--config` discovery dump for the full-stack chassis
/// (ADR-0090 §4): every cap layer knob + hand-registered knob with its
/// live source-resolved value, default, and doc. The chassis bins call
/// this when `--config` is passed and exit before boot.
#[must_use]
pub fn chassis_config_dump() -> String {
    let (metas, records) = chassis_registry();
    dump_config(metas, &records)
}

/// Chassis-bin verdict on the handle-store persistence config (ADR-0090
/// unit d, issue 1258). [`EnvOnly`](Self::EnvOnly) keeps the pre-d
/// `HandleStore::from_env_persistent` resolution; [`Argv`](Self::Argv)
/// carries the argv-then-env-resolved `PersistConfig` (with the inner
/// `None` meaning "argv said persistence is off"). The two-variant
/// enum avoids `Option<Option<_>>` (`clippy::option_option`).
#[derive(Clone, Debug, Default)]
pub enum PersistOverride {
    /// No argv overlay; resolve persistence from env at build time.
    #[default]
    EnvOnly,
    /// Argv overlay resolved: use this verbatim.
    Argv(Option<PersistConfig>),
}

/// Build the standard single-stage lifecycle config every Tick-driven
/// chassis shares today (ADR-0082 PR 3b): a `Tick` self-loop with a
/// `Quit` escape to a `Shutdown` terminal, relaying `Tick` to
/// `aether.input` so the existing `InputCapability::on_tick` fan-out
/// keeps routing to component subscribers. Headless / `test_bench` /
/// desktop all use this identical shape; a chassis that adds
/// `Render` / `Present` stages (ADR-0082 §11) builds its own graph
/// instead.
///
/// # Panics
/// Panics if the (compile-time-fixed) graph fails to build — it can't,
/// the shape is structurally valid; the `expect` documents the
/// invariant.
#[must_use]
pub fn tick_only_lifecycle_config() -> LifecycleConfig {
    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Tick>()
        .quit::<Shutdown>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("tick-only lifecycle graph is structurally valid");
    let input_mailbox = DataMailboxId(mailbox_id_from_name(InputCapability::NAMESPACE).0);
    LifecycleConfig {
        graph,
        initial_subscribers: vec![(<Tick as Kind>::ID, input_mailbox)],
    }
}

/// Build the two-stage frame lifecycle config the display-driving
/// chassis share (ADR-0082 §11, issue 1378): `Tick → Render → Tick`
/// (looping), with the `Quit` escape to a `Shutdown` terminal on the
/// `Tick` stage. The chassis drives a full `Tick → Render` cycle per
/// frame; `Render` broadcasts only after the entire `Tick` chain has
/// settled (ADR-0080 §6), so a render producer's `on_render` runs once
/// every actor's per-frame `Tick` compute is done — no submitting
/// against half-updated cross-actor state.
///
/// Same `initial_subscribers` relay as [`tick_only_lifecycle_config`]
/// (`Tick → aether.input`), so the existing `InputCapability::on_tick`
/// fan-out keeps routing `Tick` to component subscribers. Desktop and
/// `test_bench` adopt this graph; headless stays
/// [`tick_only_lifecycle_config`] (its render cap is a no-op, so a
/// `Render` stage would settle to no GPU work). `Present` is deferred —
/// it would be an empty-subscriber broadcast whose only role is a
/// `Quit → Shutdown` drain edge, and no chassis routes OS-close through
/// `Quit` mail yet.
///
/// # Panics
/// Panics if the (compile-time-fixed) graph fails to build — it can't,
/// the shape is structurally valid; the `expect` documents the
/// invariant.
#[must_use]
pub fn frame_lifecycle_config() -> LifecycleConfig {
    let graph = LifecycleGraphData::builder()
        .state::<Tick>()
        .next::<Render>()
        .quit::<Shutdown>()
        .state::<Render>()
        .next::<Tick>()
        .terminal::<Shutdown>()
        .start::<Tick>()
        .build()
        .expect("frame lifecycle graph is structurally valid");
    let input_mailbox = DataMailboxId(mailbox_id_from_name(InputCapability::NAMESPACE).0);
    LifecycleConfig {
        graph,
        initial_subscribers: vec![(<Tick as Kind>::ID, input_mailbox)],
    }
}

/// Args every full-stack chassis hands to [`with_common_caps`]. Kept
/// as a flat struct (no defaults) so an added cap forces the chassis
/// builders to acknowledge it.
pub struct CommonBoot {
    pub aborter: Arc<dyn FatalAborter>,
    pub workers: Option<usize>,
    pub input_config: InputConfig,
    pub component_host_config: ComponentHostConfig,
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConfig,
    pub anthropic: AnthropicConfig,
    pub gemini: GeminiConfig,
}

/// Wire the aborter, worker count, and the common caps every full-
/// stack chassis carries. The renderer / window caps each chassis
/// adds after this in `.with_actor::<_>()` chains.
///
/// Boot order is declaration order. ADR-0081 retired the central
/// `LogCapability` — every actor owns its own per-actor log ring; no
/// boot ordering is needed for logging anymore.
pub fn with_common_caps<C: Chassis>(builder: Builder<C>, boot: CommonBoot) -> Builder<C> {
    builder
        .with_aborter(boot.aborter)
        .with_workers(boot.workers)
        .with_actor::<HandleCapability>(())
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<DagCapability>(())
        .with_actor::<InputCapability>(boot.input_config)
        .with_actor::<ComponentHostCapability>(boot.component_host_config)
        .with_actor::<FsCapability>(boot.namespace_roots)
        .with_actor::<InventoryCapability>(())
        .with_actor::<HttpCapability>(boot.http)
        .with_actor::<TcpCapability>(())
        .with_actor::<AnthropicCapability>(boot.anthropic)
        .with_actor::<GeminiCapability>(boot.gemini)
}

/// Issue 763 P2: boot the RPC server only when `rpc_addr` is set,
/// mirroring the hub chassis. Substrate becomes an RPC server peer
/// that a hub (or any client) connects out to. `engine_name`
/// identifies the chassis profile in the `HelloAck` peer-kind.
pub fn maybe_with_rpc_server<C: Chassis>(
    builder: Builder<C>,
    rpc_addr: Option<SocketAddr>,
    engine_name: &str,
) -> Builder<C> {
    let Some(rpc_addr) = rpc_addr else {
        return builder;
    };
    builder.with_actor::<RpcServerCapability>(RpcServerConfig {
        bind_addr: rpc_addr.to_string(),
        peer_kind: PeerKind::Substrate {
            engine_name: engine_name.into(),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::chassis_known_keys;

    #[test]
    fn frame_lifecycle_graph_is_tick_render_with_shutdown_terminal() {
        // ADR-0082 §11 / issue 1378: the display-driving chassis graph is
        // `Tick → Render → Tick` (looping) with the `Quit` escape to a
        // `Shutdown` terminal on the `Tick` stage. The graph's edge
        // accessors are `pub(crate)` to `aether-capabilities`, so this
        // crate-boundary check reads the public `Debug` (start + the
        // non-terminal state kinds + terminals) plus the preserved
        // `initial_subscribers` relay.
        use aether_data::Kind;
        use aether_kinds::{Render, Shutdown, Tick};

        let cfg = super::frame_lifecycle_config();
        let graph_dbg = format!("{:?}", cfg.graph);
        let tick = format!("{:?}", <Tick as Kind>::ID);
        let render = format!("{:?}", <Render as Kind>::ID);
        let shutdown = format!("{:?}", <Shutdown as Kind>::ID);

        // Start state is Tick.
        assert!(
            graph_dbg.contains(&format!("start: {tick}")),
            "expected start Tick in {graph_dbg}",
        );
        // Both Tick and Render are non-terminal states.
        assert!(
            graph_dbg.contains(&render),
            "expected a Render state in {graph_dbg}",
        );
        // Shutdown is the sole terminal.
        assert!(
            graph_dbg.contains(&format!("terminals: [{shutdown}]")),
            "expected Shutdown terminal in {graph_dbg}",
        );

        // The `Tick → aether.input` relay is preserved, identical to the
        // tick-only config, so the InputCapability fan-out keeps routing
        // Tick to component subscribers.
        assert_eq!(cfg.initial_subscribers.len(), 1);
        assert_eq!(cfg.initial_subscribers[0].0, <Tick as Kind>::ID);
    }

    #[test]
    fn chassis_known_keys_includes_scheduler_hot_path_knobs() {
        // ADR-0090 unit b2: the six scheduler / lifecycle hot-path
        // knobs join the known-key set, so e1's unknown-AETHER_ sweep
        // doesn't flag them.
        let known = chassis_known_keys();
        for key in [
            "AETHER_LOCAL_STICKY_MAX",
            "AETHER_LOCAL_MAIL_BUDGET",
            "AETHER_LOCAL_TIME_BUDGET_US",
            "AETHER_PEER_STEAL",
            "AETHER_HANDOFF_COST_NS",
            "AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS",
        ] {
            assert!(known.contains(key), "chassis_known_keys missing {key}");
        }
    }

    #[test]
    fn chassis_known_keys_includes_a_representative_cap_key() {
        // The cap layer META walk lands the per-cap env keys (a
        // representative from each migrated cap) plus the bare chassis
        // knobs — the set is non-empty and covers more than scheduler.
        let known = chassis_known_keys();
        assert!(known.contains("AETHER_HTTP_DISABLE"));
        assert!(known.contains("AETHER_WORKERS"));
        assert!(!known.is_empty());
    }

    #[test]
    fn chassis_config_dump_lists_a_knob_from_each_cap_plus_scheduler() {
        // ADR-0090 §4 `--config`: the dump walks the same registry as
        // the sweep, so it lists a representative knob from each cap, a
        // bare chassis knob, and a scheduler hot-path knob — with a
        // header row.
        let dump = super::chassis_config_dump();
        assert!(dump.contains("KEY"));
        assert!(dump.contains("AETHER_HTTP_DISABLE")); // http cap
        assert!(dump.contains("AETHER_GEMINI_TIMEOUT_MS")); // gemini cap
        assert!(dump.contains("AETHER_AUDIO_DISABLE")); // audio cap
        assert!(dump.contains("AETHER_WORKERS")); // bare chassis knob
        assert!(dump.contains("AETHER_LOCAL_STICKY_MAX")); // scheduler knob
    }
}
