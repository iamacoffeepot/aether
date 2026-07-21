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
    ActorRingConfig, ChassisBootConfig, CommonBoot, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
    chassis_residual_knobs, install_frame_size, load_chassis_config, stage_rpc_argv, tick_only_lifecycle_params,
    with_common_caps, with_rpc_server,
};
use aether_chassis::cli::{CommonOverlay, HeadlessCli};
use aether_substrate::config::{
    ConfigError, ConfigManifest, ConfigSources, RingCapacities, SchedulerTuning, validate_env,
};
use aether_substrate::mail::registry::MailDispatch;
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use aether_substrate::runtime::log_install::apply_filter;
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

    /// The ADR-0156 §4 composition-derived config aggregate: resolve the
    /// chassis config, compose the exact capability chain `build_inner` runs
    /// (via `compose`), then read [`Builder::config_manifest`] — the sibling
    /// of `describe_manifest`'s claim terminal. The known-keys sweep and
    /// `--print-config` dump read this walk, so headless reports only the
    /// knobs it composes (no window / audio / render knobs).
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when config resolution or substrate boot fails.
    pub fn config_manifest() -> Result<ConfigManifest, BootError> {
        let env = HeadlessEnv::from_env().map_err(|e| BootError::Other(Box::new(e)))?;
        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION")).build()?;
        Ok(Self::compose(&boot, env).config_manifest())
    }

    /// Render the `--print-config` discovery dump from the composition-derived
    /// manifest plus the residual hand-registered knobs. The bin prints this
    /// and exits before boot.
    ///
    /// # Errors
    ///
    /// Returns [`BootError`] when config resolution or substrate boot fails.
    pub fn config_dump() -> Result<String, BootError> {
        Ok(Self::config_manifest()?.dump(&chassis_residual_knobs()))
    }
}

/// Bag of build-time inputs the headless chassis takes. `main()` populates it
/// from env vars (per ADR-0070's "substrate-core never reads env" invariant);
/// tests construct one directly.
///
/// ADR-0156 §5: the operator-resolvable cap `Config`s (`HttpConfig`,
/// `HttpServerConfig`, `AnthropicConfig`, `GeminiConfig`, `LifecycleConfig`) no
/// longer ride as fields — the builder resolves each off [`Self::sources`]. A
/// test constructs one by staging programmatic overrides into `sources`
/// (`ConfigSources::set_override`). What remains as fields is the source stack
/// plus the chassis-side reads of resolved members: the fs roots + content-gen
/// (the derived staging root's inputs), the driver-only tick cadence, and the
/// pool / ring / scheduler / teardown knobs.
pub struct HeadlessEnv {
    /// The config source stack (file + per-cap argv overlays) the builder
    /// resolves each composed cap's `Config` off (ADR-0156 §5).
    pub sources: ConfigSources,
    pub namespace_roots: NamespaceRoots,
    /// Content-gen staging config (ADR-0090). Resolved chassis-side; folded
    /// into the staging root in `with_common_caps`.
    pub contentgen: ContentGenConfig,
    pub tick_period: Duration,
    /// The substrate runtime knobs (#3849), resolved off the source stack. Only
    /// [`RuntimeConfig::log_filter`] is consumed chassis-side (re-applied after
    /// the subscriber installs, in `HeadlessChassis::build_inner`); the field
    /// carries the whole resolved member so its `[runtime]` file / env values
    /// are resolved once. `aether.rpc.server`'s bind port rides the source stack
    /// too now (`RpcServerConfig`), so there is no separate `rpc_addr` field —
    /// the builder resolves it (unset → claimed but unbound, ADR-0155 §3).
    pub runtime: RuntimeConfig,
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
        // ADR-0156 §4: the unknown-`AETHER_*` sweep moved to `build_inner`,
        // where the composed builder's `config_manifest` supplies the
        // per-chassis known-key set (headless no longer "knows" the window /
        // audio / render knobs it never composes).
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
        let CommonOverlay {
            http,
            http_server: http_server_overlay,
            fs,
            anthropic,
            gemini,
            contentgen,
            chassis_boot: chassis_boot_overlay,
            lifecycle: lifecycle_overlay,
            rpc: rpc_overlay,
        } = common;

        // ADR-0156 §5: assemble the source stack — the loaded config file plus
        // each cap member's typed argv overlay (`Overlay::into_layer`). The
        // builder resolves the composed cap configs (http / http-server /
        // anthropic / gemini / lifecycle) off this ahead of `init`; section
        // identity comes from each member's `ConfigMember` declaration, so no
        // chassis-side section string survives.
        let mut sources = ConfigSources::new(config_file);
        sources.set_argv::<HttpConf>(http.into_layer());
        sources.set_argv::<HttpServerConfig>(http_server_overlay.into_layer());
        sources.set_argv::<NamespaceRoots>(fs.into_layer());
        sources.set_argv::<AnthropicConfig>(anthropic.into_layer());
        sources.set_argv::<GeminiConfig>(gemini.into_layer());
        sources.set_argv::<ContentGenConfig>(contentgen.into_layer());
        sources.set_argv::<ChassisBootConfig>(chassis_boot_overlay.into_layer());
        sources.set_argv::<LifecycleConfig>(lifecycle_overlay.into_layer());
        sources.set_argv::<TickConfig>(tick_overlay.into_layer());
        // #3849: `aether.rpc.server`'s bind port resolves through the source
        // stack like any member — stage the `--rpc-port` overlay so the builder
        // resolves it (argv > `AETHER_RPC_PORT` > `[rpc]` file > unset/unbound).
        stage_rpc_argv(&mut sources, rpc_overlay);

        // Chassis-side reads of resolved members (ADR-0156 §5) — resolved off
        // the same stack via each member's `ConfigMember` section: the derived
        // staging inputs (fs roots + content-gen), the driver-only tick cadence,
        // and the non-cap pool / ring / scheduler / teardown knobs.
        let chassis_boot = sources.resolve::<ChassisBootConfig>()?;
        let namespace_roots = sources.resolve::<NamespaceRoots>()?;
        let contentgen = sources.resolve::<ContentGenConfig>()?;
        // Tick cadence: resolved through `TickConfig` (argv > env > default).
        // `nonzero` maps 0 to the default (60 Hz); a garbage value hard-errors.
        let tick_period = sources.resolve::<TickConfig>()?.to_tick_period();
        let ring_caps = sources.resolve::<ActorRingConfig>()?.to_ring_capacities();
        let scheduler_tuning = sources.resolve::<SchedulerTuningConfig>()?.to_scheduler_tuning();
        let teardown_cap = sources.resolve::<SettlementConfig>()?.to_cap();
        // #3849: resolve the substrate runtime knobs (log filter + panic-hook
        // knobs) off the same stack. `build_inner` re-applies `log_filter` once
        // the subscriber is installed; the panic-hook knobs are declared members
        // consumed by the panic hook's own env reads.
        let runtime = sources.resolve::<RuntimeConfig>()?;
        // ADR-0156 §6 (#3850): push the resolved wire-frame cap into the codec
        // here, before the RPC server binds and any framing runs — the codec
        // cannot pull the knob itself.
        install_frame_size(&mut sources)?;

        // Boot manifest: argv wins over `AETHER_BOOT_MANIFEST` (resolved
        // through `ChassisBootConfig`). When set, the listed components'
        // wasm + config are read into the autoload list `build_inner`
        // drains into `aether.component.load`; an unreadable manifest
        // aborts boot (ADR-0090 §4) via `ConfigError`.
        let autoload = match chassis_boot.boot_manifest.clone() {
            Some(path) => boot_manifest_autoload(Path::new(&path))?,
            None => Vec::new(),
        };
        let workers = chassis_boot.to_workers();
        Ok(Self {
            sources,
            namespace_roots,
            contentgen,
            tick_period,
            runtime,
            workers,
            ring_caps,
            scheduler_tuning,
            teardown_cap,
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
            sources,
            namespace_roots,
            contentgen,
            workers,
            ring_caps,
            scheduler_tuning,
            teardown_cap,
            tick_period: _,
            runtime: _,
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
            contentgen,
            game_gateway_params: aether_game::GameGatewayParams::default(),
        };
        // ADR-0082 §1 / PR 3b: headless uses the shared Tick-only
        // lifecycle graph (Tick self-loops, Quit escapes to Shutdown);
        // the timer pushes `LifecycleAdvance` and the driver broadcasts
        // Tick to `aether.input` via the relay subscriber.
        //
        // ADR-0156 §5: hand the builder the source stack (`with_config_sources`)
        // so it resolves each composed cap's `Config` (http / http-server /
        // lifecycle) off it; the caps compose with `with_actor(params)` alone.
        let builder = with_common_caps(Builder::<Self>::new(registry, mailer).with_config_sources(sources), common)
            .with_actor::<HeadlessRenderCapability>(())
            .with_actor::<HeadlessClipboardCapability>(())
            .with_actor::<HeadlessWindowCapability>(())
            .with_actor::<UnsupportedSubstrateHarnessCapability>(())
            .with_actor::<LifecycleCapability>(tick_only_lifecycle_params());
        with_rpc_server(builder, "aether-headless").with_actor::<HttpServerCapability>(())
    }

    /// Build the headless chassis: stand up substrate-core internals,
    /// compose the capability chain via [`Self::compose`], then wrap the
    /// timer in a [`HeadlessTimerDriverCapability`] and hand it to the
    /// builder.
    fn build_inner(mut env: HeadlessEnv) -> Result<BuiltChassis<Self>, BootError> {
        let boot = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION")).build()?;
        // #3849: `SubstrateBoot::build` installed the subscriber with an
        // env-or-`info` filter (before the config file loaded); re-apply the
        // fully-resolved `AETHER_LOG_FILTER` directive (env > `[runtime]` file >
        // `info`) now so a filter set only in the config file takes effect.
        apply_filter(&env.runtime.log_filter);
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
        // ADR-0156 §4 (was ADR-0090 §4 e1): warn on any unknown `AETHER_*` env
        // var, sweeping against the composition-derived known-key set plus the
        // residual hand records. Runs here (not in `from_env`) because the
        // per-chassis known keys come from the composed builder's manifest.
        validate_env(&builder.config_manifest().known_keys(&chassis_residual_knobs()))?;
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

#[cfg(test)]
mod config_manifest_tests {
    use super::HeadlessChassis;
    use aether_chassis::boot::chassis_residual_knobs;

    #[test]
    fn headless_known_keys_drop_window_and_audio_and_keep_tick() {
        // ADR-0156 §4 acceptance: the aggregate is derived from what headless
        // actually composes, so the over-claim of the old flat registry dies —
        // headless composes no window driver and no audio cap, so its known
        // keys no longer include those knobs (a stale `AETHER_WINDOW_MODE` on a
        // headless box now fails the sweep honestly). Membership is asserted
        // from the composition walk, not a hand list.
        let manifest = HeadlessChassis::config_manifest().expect("headless config manifest");
        let known = manifest.known_keys(&chassis_residual_knobs());
        assert!(!known.contains("AETHER_WINDOW_MODE"), "headless must not claim the desktop window-driver knob");
        assert!(
            !known.contains("AETHER_AUDIO_DISABLE"),
            "headless must not claim the audio cap knob it never composes"
        );
        assert!(known.contains("AETHER_TICK_HZ"), "headless must claim its own timer-driver tick knob");
        assert!(known.contains("AETHER_HTTP_DISABLE"), "headless must claim a composed common-cap knob");
        // #3849 + #3850: the RPC port, the runtime knobs, and the frame-size knob
        // migrated off the residual hand records onto derive-`Config` members
        // (`RpcServerConfig` composed via `with_rpc_server`, `RuntimeConfig` +
        // `FrameSizeConfig` declared in `with_common_caps`), so the composition
        // walk — not `chassis_residual_knobs` — is what now claims them. Catches a
        // dropped `with_config_member` or a de-composed RPC server reintroducing a
        // false unknown-key warning.
        assert!(
            known.contains("AETHER_MAX_FRAME_SIZE"),
            "headless must claim the frame-size knob via the composed FrameSizeConfig member"
        );
        assert!(known.contains("AETHER_RPC_PORT"), "headless must claim the RPC port via the composed RpcServerConfig");
        assert!(known.contains("AETHER_LOG_FILTER"), "headless must claim the log filter via the RuntimeConfig member");
        assert!(known.contains("AETHER_CRASH_LOG_DIR"), "headless must claim the panic-hook knobs via RuntimeConfig");
    }
}
