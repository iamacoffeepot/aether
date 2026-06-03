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
use aether_capabilities::rpc::{PeerKind, RpcServerCapability, RpcServerConfig};
use aether_capabilities::{
    AnthropicCapability, AnthropicConfig, ComponentHostCapability, ComponentHostConfig,
    DagCapability, FsCapability, GeminiCapability, GeminiConfig, HandleCapability, HttpCapability,
    InputCapability, InputConfig, InventoryCapability, TcpCapability, fs::NamespaceRoots,
    http::HttpConfig, trace::TraceDispatchCapability,
};
use aether_data::{Kind, MailboxId as DataMailboxId, mailbox_id_from_name};
use aether_kinds::{Shutdown, Tick};
use aether_substrate::chassis::Chassis;
use aether_substrate::chassis::builder::Builder;
use aether_substrate::config::{KnobKind, KnobRecord, KnownKeys, known_keys};
use aether_substrate::handle_store::{ENV_MAX_BYTES, PersistConfig, PersistConfigLayer};
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::scheduler::SCHEDULER_KNOBS;
use aether_substrate::{LifecycleDriverConfig, LifecycleGraph};
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
    let metas: &[&'static Meta] = &[
        &HttpConfigLayer::META,
        &GeminiConfigLayer::META,
        &AnthropicConfigLayer::META,
        &AudioConfigLayer::META,
        &NamespaceRootsLayer::META,
        &PersistConfigLayer::META,
    ];
    // CHASSIS_KNOBS (bare chassis knobs) + the scheduler / lifecycle
    // hot-path tuning knobs (ADR-0090 unit b2): both join the known-key
    // set so e1's sweep doesn't flag them and e2's dump lists them.
    let mut records: Vec<KnobRecord> = CHASSIS_KNOBS.to_vec();
    records.extend_from_slice(SCHEDULER_KNOBS);
    known_keys(metas, &records)
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
pub fn tick_only_lifecycle_config() -> LifecycleDriverConfig<()> {
    let graph = LifecycleGraph::<()>::builder()
        .state::<Tick, _>(|()| Tick {})
        .next::<Tick>()
        .quit::<Shutdown>()
        .terminal::<Shutdown, _>(|()| Shutdown {})
        .start::<Tick>()
        .build()
        .expect("tick-only lifecycle graph is structurally valid");
    let input_mailbox = DataMailboxId(mailbox_id_from_name(InputCapability::NAMESPACE).0);
    LifecycleDriverConfig {
        graph,
        context: (),
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
}
