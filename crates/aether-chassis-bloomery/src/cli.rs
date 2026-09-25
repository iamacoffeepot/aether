//! The bloomery chassis CLI root (ADR-0090 unit d, issue 1258). [`BloomeryCli`]
//! is journal-driven — no full-stack caps — so it flattens the bloomery,
//! RPC-server, and HTTP-egress overlays plus the four tuning overlays the
//! chassis resolves off its own source stack, alongside the source-selecting
//! [`ChassisMeta`] flags. The
//! shared staging / flag-naming / help-forwarding machinery lives in
//! `aether_chassis::cli`.

use aether_chassis::boot::{ActorRingOverlay, RegistryQueueOverlay, SchedulerTuningOverlay, env_only_after_help};
use aether_chassis::chassis_cli;
use aether_chassis::cli::ChassisMeta;
use aether_http::HttpOverlay;
use aether_rpc::RpcServerOverlay;
use aether_substrate::config::SettlementOverlay;
use clap::Parser;

use crate::config::BloomeryOverlay;

/// Bloomery chassis CLI root — journal-driven, no full-stack caps.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-bloomery",
    about = "Bloomery chassis — journal-driven engine: the bundle driver over one journal root. Issue #6244.",
    long_about = "Bloomery chassis — journal-driven engine: the bundle driver over one journal root. Issue #6244.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = env_only_after_help()
)]
pub struct BloomeryCli {
    /// Bloomery knobs: `--bloomery-journal` / `--bloomery-closure-limit-bytes`.
    #[command(flatten)]
    pub bloomery: BloomeryOverlay,

    /// `--rpc-port` shadows `AETHER_RPC_PORT` — the `aether.rpc.server` bind
    /// port. Absent → the member's `None` default, so a bare local run opens
    /// no socket; the fleet injects the port on spawn.
    #[command(flatten)]
    pub rpc: RpcServerOverlay,

    /// HTTP egress knobs for Sampled programs (ADR-0234 decision 7):
    /// `--http-allowlist` shadows `AETHER_HTTP_ALLOWLIST`, the hosts a fetch
    /// may reach. Absent → the member's empty default, which denies every
    /// fetch. `--http-secrets <host>/bearer=<name>` binds a secret from the
    /// `--secrets-dir` directory to one allowlisted host (ADR-0235), which
    /// `aether.http` attaches over HTTPS only. The other `--http-*` flags
    /// (`--http-disable`, `--http-require-https`, and the body, timeout, and
    /// in-flight bounds) ride the same overlay.
    #[command(flatten)]
    pub http: HttpOverlay,

    /// Per-actor ring-capacity knobs (issue 1990): `--actor-*`. The chassis
    /// resolves `ActorRingConfig` off its own source stack for the actors its
    /// registry hosts.
    #[command(flatten)]
    pub actor_ring: ActorRingOverlay,
    /// Scheduler hot-path tuning knobs (issue 2485): `--scheduler-*`. The
    /// chassis resolves `SchedulerTuningConfig` off its own source stack.
    #[command(flatten)]
    pub scheduler: SchedulerTuningOverlay,
    /// ADR-0165 serialized-queue bounds (issue 4122): the chassis resolves
    /// `RegistryQueueConfig` off its own source stack for the actors its
    /// registry hosts.
    #[command(flatten)]
    pub registry_queues: RegistryQueueOverlay,
    /// Settlement-patience backstop (issue 2062): `--settlement-cap-secs`. The
    /// chassis resolves `SettlementConfig` for its own teardown budget.
    #[command(flatten)]
    pub settlement: SettlementOverlay,

    /// The source-selecting meta flags (`--config` / `--print-config` /
    /// `--describe`); see [`ChassisMeta`].
    #[command(flatten)]
    #[stage(skip)]
    pub meta: ChassisMeta,
}

// The bloomery composes the component host, HTTP egress, and the RPC server;
// `--rpc-port` rides the derive-emitted `RpcServerOverlay` (#3849) and
// `--http-allowlist` the derive-emitted `HttpOverlay`, like every other flag.
// The four tuning overlays the chassis resolves off its own source stack ride
// beside the bloomery's own overlay.
chassis_cli!(BloomeryCli {
    BloomeryOverlay,
    RpcServerOverlay,
    HttpOverlay,
    ActorRingOverlay,
    SchedulerTuningOverlay,
    RegistryQueueOverlay,
    SettlementOverlay,
});
