//! Headless chassis: `HeadlessChassis` (ADR-0035 / ADR-0071), the
//! `Err`-replying capability stubs that fail fast for kinds desktop
//! supports natively (capture/window) plus `Advance`, and the
//! [`HeadlessChassis::build`] entry point that assembles the substrate
//! + tick driver into a [`BuiltChassis`].
//!
//! Issue 603 retired the `chassis_handler` closure: each fail-fast
//! kind moved onto its own cap. `HeadlessRenderCapability` (Phase 2)
//! handles `aether.render`; `HeadlessWindowCapability` (Phase 3)
//! handles `aether.window`; `UnsupportedSubstrateHarnessCapability` (Phase 4)
//! handles `aether.substrate_harness`. `aether.control.platform_info` (now
//! a deleted kind name from a retired namespace) was
//! deleted as a kind in Phase 4 — no replacement, no MCP path until
//! issue 603 §F2 revives the per-domain shape.

use std::mem;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aether_anthropic::AnthropicConfig;
use aether_audio::{SetMasterGain, SetMasterGainResult};
use aether_clipboard::HeadlessClipboardCapability;
use aether_component::ComponentHostParams;
use aether_contentgen::ContentGenConfig;
use aether_data::Kind;
use aether_fs::NamespaceRoots;
use aether_gemini::GeminiConfig;
use aether_harness_substrate::UnsupportedSubstrateHarnessCapability;
use aether_http::{HttpConfig as HttpConf, HttpServerCapability, HttpServerConfig};
use aether_kinds::BinaryManifest;
use aether_kinds::Tick;
use aether_lifecycle::{LifecycleCapability, LifecycleConfig};
use aether_render::HeadlessRenderCapability;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot};
use aether_window::HeadlessWindowCapability;

use aether_chassis::TickConfig;

use super::driver::HeadlessTimerDriverCapability;
use aether_chassis::autoload::{AutoloadComponent, autoload_mail, boot_manifest_autoload};
use aether_chassis::boot::{
    ActorRingConfig, ChassisBootConfig, CommonBoot, SchedulerTuningConfig, chassis_known_keys, load_chassis_config,
    resolve_env_with_file, resolve_teardown_cap_with_file, resolve_with_file, rpc_port_from_env,
    tick_only_lifecycle_params, with_common_caps, with_rpc_server,
};
use aether_chassis::cli::{CommonOverlay, HeadlessCli};
use aether_substrate::config::{ConfigError, RingCapacities, SchedulerTuning, validate_env};
use aether_substrate::mail::registry::MailDispatch;
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use std::path::Path;

/// Marker type for the headless chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HeadlessChassis>`] returned
/// by `Self::build`. Same shape as the desktop chassis marker post
/// ADR-0071 phase 3.
pub struct HeadlessChassis;

impl Chassis for HeadlessChassis {
    const PROFILE: &'static str = "headless";
    type Driver = HeadlessTimerDriverCapability;
    type Env = HeadlessEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
    }
}

impl HeadlessChassis {
    /// The `--describe` manifest (ADR-0115, amended by ADR-0155): the
    /// chassis profile, the mailbox namespaces this binary claims, and the
    /// `build.rs` provenance. Resolves the chassis config the same
    /// argv/env/file way a real boot does, composes the exact capability
    /// chain `build_inner` runs (via `compose` — including the
    /// `aether.audio` fail-fast inline sink), then runs the ADR-0155
    /// claim-only terminal and reads the claimed namespaces straight off
    /// the registry. `--describe` stops before Init, so it opens no audio
    /// device / filesystem roots and binds no socket.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when config resolution ([`HeadlessEnv::from_env`]),
    /// substrate boot, or the claim pass fails.
    pub fn describe_manifest() -> Result<BinaryManifest, BootError> {
        let env = HeadlessEnv::from_env().map_err(|e| BootError::Other(Box::new(e)))?;
        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION")).build()?;
        let caps = Self::compose(&boot, env).claim_namespaces()?;
        Ok(aether_chassis::binary_manifest(Self::PROFILE, caps))
    }
}

/// Bag of resolved configs the headless chassis takes at build time.
/// `main()` populates it from env vars (per ADR-0070's "substrate-core
/// never reads env" invariant); tests construct one directly.
pub struct HeadlessEnv {
    pub namespace_roots: NamespaceRoots,
    pub http: HttpConf,
    /// ADR-0050 `aether.anthropic` cap config (issue 1014). Resolved
    /// from `ANTHROPIC_API_KEY` + `AETHER_ANTHROPIC_*`.
    pub anthropic: AnthropicConfig,
    /// ADR-0050 `aether.gemini` cap config (issue 1015). Resolved from
    /// `GEMINI_API_KEY` + `AETHER_GEMINI_*`.
    pub gemini: GeminiConfig,
    /// Content-gen staging config (ADR-0090). Resolved from
    /// `AETHER_GEN_DIR` / `--gen-dir`; folded into the staging root in
    /// `with_common_caps`.
    pub contentgen: ContentGenConfig,
    pub tick_period: Duration,
    /// The resolved `aether.http.server` init config (ADR-0108, ADR-0155 §3).
    /// The cap is always composed and always claims `aether.http.server`;
    /// its `enabled` flag (`AETHER_HTTP_SERVER_ENABLED` /
    /// `--http-server-enabled`, default off) gates only whether Start binds
    /// the socket, so an unconfigured chassis binds no HTTP port yet still
    /// answers mail rather than warn-dropping it.
    pub http_server: HttpServerConfig,
    /// The resolved `aether.rpc.server` bind address (ADR-0155 §3). The cap
    /// is always composed and always claims `aether.rpc.server`; the
    /// address (from `AETHER_RPC_PORT`) gates only whether Start binds the
    /// socket — `None` (default) leaves the mailbox claimed but unbound.
    pub rpc_addr: Option<SocketAddr>,
    /// Issue 745: optional worker-pool size override. Populated from
    /// `AETHER_WORKERS`; `None` keeps `PoolConfig::default()` behavior
    /// (`available_parallelism() - 1`, min 1).
    pub workers: Option<usize>,
    /// Issue 1990: per-actor ring capacities resolved from the
    /// `ActorRingConfig` knob (`AETHER_ACTOR_LOG_RING_SIZE` /
    /// `AETHER_ACTOR_TRACE_RING_SIZE`). Default is
    /// [`RingCapacities::default`] (the `aether-actor` const caps).
    pub ring_caps: RingCapacities,
    /// Issue 2485: scheduler hot-path tuning resolved from the
    /// `SchedulerTuningConfig` knob (`AETHER_SPIN_WINDOW_USEC` /
    /// `AETHER_LOCAL_STICKY_MAX` / …). Default is
    /// [`SchedulerTuning::default`] (the built-in scheduler literals).
    pub scheduler_tuning: SchedulerTuning,
    /// Issue #2509: cumulative patience for the instanced-actor teardown
    /// close-done gate, resolved from `AETHER_SETTLEMENT_CAP_SECS` /
    /// `[settlement]`.
    pub teardown_cap: Duration,
    /// `AETHER_LIFECYCLE_ADVANCE_TIMEOUT_MS` — timeout for one lifecycle
    /// advance step (Tick) before the scheduler logs a slow-frame warning.
    /// ADR-0156 §3: resolved through the lifecycle cap's own `LifecycleConfig`
    /// (relocated off `ChassisBootConfig`); default is 1000 ms.
    pub lifecycle: LifecycleConfig,
    /// Components to auto-load on boot, in order. A bundled standalone build
    /// populates this so the components come up with no hub; the normal
    /// headless bin leaves it empty and loads components over the hub instead.
    pub autoload: Vec<AutoloadComponent>,
}

impl HeadlessEnv {
    /// Read every chassis-relevant env var into a fresh `HeadlessEnv`.
    /// The single env-reading edge for the headless chassis (per
    /// issue 464). Tests bypass this by constructing `HeadlessEnv`
    /// directly.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known `AETHER_*` env var holds
    /// an unparseable value (ADR-0090 §4); an unknown `AETHER_*` var
    /// only warns (non-fatal).
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_argv(HeadlessCli::default())
    }

    /// ADR-0090 unit d (issue 1258): resolve every cap config through
    /// the argv-then-env overlay. `cli` carries `Option<T>` flags;
    /// unset fields fall through to env-only resolution, so an empty
    /// argv (the path the integration tests and existing `from_env`
    /// callers exercise) is byte-identical to the pre-d behaviour.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known `AETHER_*` env var (or an
    /// argv overlay value) holds an unparseable value (ADR-0090 §4).
    pub fn from_env_with_argv(cli: HeadlessCli) -> Result<Self, ConfigError> {
        use std::net::{IpAddr, Ipv4Addr};
        // ADR-0090 §4 (e1): warn on any unknown AETHER_ env var before
        // resolving — a typo / stale export is loud but non-fatal.
        validate_env(&chassis_known_keys())?;
        let HeadlessCli {
            common,
            tick: tick_overlay,
            // The bin handles `--print-config` / `--describe` (print + exit)
            // before this resolver runs.
            config,
            print_config: _,
            describe: _,
        } = cli;
        let config_file = load_chassis_config(config)?;
        let config_file = config_file.as_ref();
        let CommonOverlay {
            http,
            http_server: http_server_overlay,
            fs,
            anthropic,
            gemini,
            contentgen,
            chassis_boot: chassis_boot_overlay,
            lifecycle: lifecycle_overlay,
            rpc_port: cli_rpc_port,
        } = common;

        let chassis_boot =
            resolve_with_file::<ChassisBootConfig>(chassis_boot_overlay.into_layer(), config_file, "chassis")?;
        let lifecycle = resolve_with_file::<LifecycleConfig>(lifecycle_overlay.into_layer(), config_file, "lifecycle")?;
        let tick_config = resolve_with_file::<TickConfig>(tick_overlay.into_layer(), config_file, "tick")?;

        // Boot manifest: argv wins over `AETHER_BOOT_MANIFEST` (resolved
        // through `ChassisBootConfig`). When set, the listed components'
        // wasm + config are read into the autoload list `build_inner`
        // drains into `aether.component.load`; an unreadable manifest
        // aborts boot (ADR-0090 §4) via `ConfigError`.
        let autoload = match chassis_boot.boot_manifest.clone() {
            Some(path) => boot_manifest_autoload(Path::new(&path))?,
            None => Vec::new(),
        };
        let http = resolve_with_file::<HttpConf>(http.into_layer(), config_file, "http")?;
        let anthropic = resolve_with_file::<AnthropicConfig>(anthropic.into_layer(), config_file, "anthropic")?;
        let gemini = resolve_with_file::<GeminiConfig>(gemini.into_layer(), config_file, "gemini")?;
        let contentgen = resolve_with_file::<ContentGenConfig>(contentgen.into_layer(), config_file, "contentgen")?;
        let namespace_roots = resolve_with_file::<NamespaceRoots>(fs.into_layer(), config_file, "fs")?;
        // ADR-0155 §3: the HTTP server cap is always composed and always
        // claims its mailbox; its `enabled` flag (default off) gates only
        // whether Start binds the socket. Resolve the whole config and hand
        // it over — an unconfigured chassis binds no HTTP port but still
        // answers `aether.http.server` mail with a fail-fast `Err`.
        let http_server =
            resolve_with_file::<HttpServerConfig>(http_server_overlay.into_layer(), config_file, "http-server")?;
        // Tick cadence: resolved through `TickConfig` (argv > env > default).
        // `nonzero` maps 0 to the default (60 Hz); a garbage value hard-errors.
        let tick_period = tick_config.to_tick_period();
        let rpc_addr =
            cli_rpc_port.or_else(rpc_port_from_env).map(|p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p));
        let workers = chassis_boot.to_workers();
        // Issue 1990: resolve the per-actor ring capacities from
        // `AETHER_ACTOR_{LOG,TRACE}_RING_SIZE` (ADR-0090 §4 hard-error on
        // an unparseable known value, surfaced as `ConfigError`).
        let ring_caps = resolve_env_with_file::<ActorRingConfig>(config_file, "actor")?.to_ring_capacities();
        // Issue 2485: resolve the scheduler hot-path tuning (ADR-0090 §4
        // hard-error on an unparseable known value, surfaced as
        // `ConfigError`).
        let scheduler_tuning =
            resolve_env_with_file::<SchedulerTuningConfig>(config_file, "scheduler")?.to_scheduler_tuning();
        let teardown_cap = resolve_teardown_cap_with_file(config_file)?;
        Ok(Self {
            namespace_roots,
            http,
            anthropic,
            gemini,
            contentgen,
            tick_period,
            http_server,
            rpc_addr,
            workers,
            ring_caps,
            scheduler_tuning,
            teardown_cap,
            lifecycle,
            autoload,
        })
    }
}

impl HeadlessChassis {
    /// Compose the headless capability chain — the single claim/build path
    /// (ADR-0155) both [`Self::build_inner`] and [`Self::describe_manifest`]
    /// run, so the manifest roster can never drift from what boots. Registers
    /// the `aether.audio` fail-fast inline sink on the shared registry, then
    /// composes the common caps plus the headless render / clipboard / window
    /// / substrate-harness / lifecycle caps and the always-claim RPC + HTTP
    /// servers (ADR-0155 §3). Returns the composed builder before the driver
    /// is installed: `build_inner` adds the timer driver and starts, while
    /// `describe_manifest` calls `claim_namespaces` on it.
    ///
    /// Takes the boot handle by reference — `build_inner` moves the same
    /// `boot` into the timer driver afterward. The `tick_period` (driver-only)
    /// and `autoload` (drained post-build) fields ride [`HeadlessEnv`] but
    /// take no part in the claim chain, so they are ignored here.
    fn compose(boot: &SubstrateBoot, env: HeadlessEnv) -> Builder<Self> {
        let HeadlessEnv {
            namespace_roots,
            http,
            http_server,
            anthropic,
            gemini,
            contentgen,
            rpc_addr,
            workers,
            ring_caps,
            scheduler_tuning,
            teardown_cap,
            lifecycle,
            tick_period: _,
            autoload: _,
        } = env;

        let component_host_params = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };

        // Audio nop sink — NoteOn/NoteOff fall through silently;
        // SetMasterGain replies Err so agents fail fast rather than
        // hang on a chassis with no audio device.
        //
        // Issue 838: registered as `Sink` (not `Closure`) so the
        // `Mailer::push` route brackets the inline handler with
        // `Received`/`Finished`. The handler does its work
        // synchronously (calls `send_reply` directly); there's no
        // actor dispatch loop behind it, so without the bracket
        // any chain that mails `aether.audio` from the headless
        // chassis leaks `in_flight` and never settles. Same shape
        // as the AETHER_DIAGNOSTICS sink in `boot.rs::register_inline`.
        //
        // ADR-0155: registering the sink here (Compose) is what puts
        // `aether.audio` in the claim-derived `--describe` roster — an
        // inline sink is a claim like any other.
        let kind_set_master_gain = boot.registry.kind_id(SetMasterGain::NAME).expect("SetMasterGain registered");
        let outbound_for_audio_sink = Arc::clone(&boot.outbound);
        boot.registry.register_inline(
            "aether.audio",
            Arc::new(move |dispatch: MailDispatch<'_>| {
                if dispatch.kind == kind_set_master_gain {
                    outbound_for_audio_sink.send_reply(
                        dispatch.sender,
                        &SetMasterGainResult::Err {
                            error: "unsupported on headless chassis — no audio device".to_owned(),
                        },
                    );
                }
            }),
        );

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0063: production chassis configures the fatal-abort
        // aborter so a wasm guest trap exits the substrate via
        // `lifecycle::fatal_abort` instead of unwinding.
        let aborter: Arc<dyn FatalAborter> = Arc::new(OutboundFatalAborter::new(Arc::clone(&boot.outbound)));

        // ADR-0071 phase B: io / http / log compose through the
        // chassis_builder `.with()` chain. Boot order is declaration
        // order — `with_common_caps` runs log first so other
        // capabilities' boot tracing routes through the log capture.
        let common = CommonBoot {
            aborter,
            workers,
            ring_caps,
            scheduler_tuning,
            // Issue #2509: the instanced-actor teardown gate honors the
            // same `AETHER_SETTLEMENT_CAP_SECS` knob (including its
            // `0 → wait forever` sentinel) as the settlement gates.
            teardown_cap,
            component_host_params,
            namespace_roots,
            http,
            anthropic,
            gemini,
            contentgen,
            game_gateway: aether_game::GameGatewayConfig::default(),
            game_gateway_params: aether_game::GameGatewayParams::default(),
        };
        // ADR-0082 §1 / PR 3b: headless uses the shared Tick-only
        // lifecycle graph (Tick self-loops, Quit escapes to Shutdown);
        // the timer pushes `LifecycleAdvance` and the driver broadcasts
        // Tick to `aether.input` via the relay subscriber.
        let builder = with_common_caps(Builder::<Self>::new(registry, mailer), common)
            .with_actor::<HeadlessRenderCapability>((), ())
            .with_actor::<HeadlessClipboardCapability>((), ())
            .with_actor::<HeadlessWindowCapability>((), ())
            .with_actor::<UnsupportedSubstrateHarnessCapability>((), ())
            .with_actor::<LifecycleCapability>(lifecycle, tick_only_lifecycle_params());
        with_rpc_server(builder, rpc_addr, "aether-headless").with_actor::<HttpServerCapability>(http_server, ())
    }

    /// Build the headless chassis: stand up substrate-core internals,
    /// compose the capability chain via [`Self::compose`], then wrap the
    /// timer in a [`HeadlessTimerDriverCapability`] and hand it to the
    /// builder.
    fn build_inner(mut env: HeadlessEnv) -> Result<BuiltChassis<Self>, BootError> {
        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION")).build()?;
        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let mailer = Arc::clone(&boot.queue);

        // Driver-only / post-build fields, read out before `compose` consumes
        // `env`: the tick cadence rides the timer driver, the autoload list is
        // drained after build. The `Copy` knobs also feed the boot log line.
        let tick_period = env.tick_period;
        let workers = env.workers;
        let ring_caps = env.ring_caps;
        let autoload = mem::take(&mut env.autoload);

        // Tick rates are bounded well below `u32::MAX` Hz (typically
        // 60-240 Hz); the `u128 → u32` narrowing is safe in practice.
        #[allow(clippy::cast_possible_truncation)]
        let tick_hz = (Duration::from_secs(1).as_nanos() / tick_period.as_nanos().max(1)) as u32;
        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            tick_hz = tick_hz,
            log_ring_capacity = ring_caps.log,
            trace_ring_capacity = ring_caps.trace,
            trace_ring_max_capacity = ring_caps.trace_max,
            "componentless boot — load a component via aether.component.load",
        );

        let builder = Self::compose(&boot, env);
        let driver = HeadlessTimerDriverCapability { boot, kind_tick, tick_period };
        let built = builder.driver(driver).build()?;
        // Auto-load any bundled components, in order, before the run loop
        // starts. Fire-and-forward: the component host dispatches each load
        // off the worker pool (already up after `build`), so the components
        // are live shortly after `run` begins — no hub required. Mirrors the
        // desktop chassis drain (#1520, generalized in #1529).
        for component in autoload {
            mailer.push(autoload_mail(component));
        }
        Ok(built)
    }
}
