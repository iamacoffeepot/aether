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

use aether_audio::{SetMasterGain, SetMasterGainResult};
use aether_clipboard::HeadlessClipboardCapability;
use aether_component::ComponentHostParams;
use aether_data::Kind;
use aether_harness_substrate::UnsupportedSubstrateHarnessCapability;
use aether_http::HttpServerCapability;
use aether_kinds::Tick;
use aether_lifecycle::LifecycleCapability;
use aether_render::HeadlessRenderCapability;
use aether_substrate::chassis::BootableChassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot};
use aether_window::HeadlessWindowCapability;

use aether_chassis::TickConfig;

use super::driver::HeadlessTimerDriverCapability;
use aether_chassis::autoload::{AutoloadComponent, autoload_mail};
use aether_chassis::boot::{
    CommonEnv, chassis_residual_knobs, load_chassis_config, tick_only_lifecycle_params, with_common_caps,
    with_rpc_server,
};
use aether_chassis::cli::HeadlessCli;
use aether_substrate::config::{ConfigError, ConfigSources, KnobRecord, StageArgv, validate_env};
use aether_substrate::mail::registry::MailDispatch;
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use aether_substrate::runtime::log_install::apply_filter;

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

impl BootableChassis for HeadlessChassis {
    fn resolve_env() -> Result<Self::Env, ConfigError> {
        HeadlessEnv::from_env()
    }

    fn residual_knobs() -> Vec<KnobRecord> {
        chassis_residual_knobs()
    }

    /// Compose the headless capability chain — the single claim/build path
    /// (ADR-0155) both [`Chassis::build`] and the describe / config helpers run,
    /// so the manifest roster can never drift from what boots. Registers the
    /// `aether.audio` fail-fast inline sink on the shared registry, then composes
    /// the common caps plus the headless render / clipboard / window /
    /// substrate-harness / lifecycle caps and the always-claim RPC + HTTP servers
    /// (ADR-0155 §3). Returns the composed builder before the driver is installed:
    /// `build_inner` adds the timer driver and starts, while the describe / config
    /// helpers read the claim / config terminals off it.
    ///
    /// Takes the boot handle by reference — `build_inner` moves the same
    /// `boot` into the timer driver afterward. The `tick_period` (driver-only)
    /// and `autoload` (drained post-build) fields ride [`HeadlessEnv`] but
    /// take no part in the claim chain, so they are ignored here.
    fn compose(boot: &SubstrateBoot, env: HeadlessEnv) -> Builder<Self> {
        let HeadlessEnv { common, tick_period: _, autoload: _ } = env;

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
        // `into_common_boot` reads the six env-sourced `CommonBoot` fields off
        // the shared env in one place (the teardown budget honors the same
        // `AETHER_SETTLEMENT_CAP_SECS` knob, `0 → wait forever` sentinel and all,
        // as the settlement gates) and threads the source stack back out.
        let (common, sources) =
            common.into_common_boot(aborter, component_host_params, aether_game::GameGatewayParams::default());
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
        with_rpc_server(builder).with_actor::<HttpServerCapability>(())
    }
}

/// Bag of build-time inputs the headless chassis takes. `main()` populates it
/// from env vars (per ADR-0070's "substrate-core never reads env" invariant);
/// tests construct one directly.
///
/// The shared config (source stack, fs roots, content-gen staging, runtime,
/// pool / ring / scheduler / teardown knobs) lives in the embedded
/// [`CommonEnv`]; only the headless tick cadence and the autoload list are
/// per-chassis. ADR-0156 §5: the operator-resolvable cap `Config`s (`HttpConfig`,
/// `HttpServerConfig`, `AnthropicConfig`, `GeminiConfig`, `LifecycleConfig`) no
/// longer ride as fields — the builder resolves each off [`CommonEnv::sources`],
/// and a test stages programmatic overrides into it (`ConfigSources::set_override`).
pub struct HeadlessEnv {
    /// The config fields every full-stack chassis shares (source stack, fs roots,
    /// content-gen staging, runtime knobs, pool / ring / scheduler / teardown
    /// knobs). Resolved by [`CommonEnv::resolve`] — the single declaration, so a
    /// shared knob can't exist on one chassis and silently not the other.
    pub common: CommonEnv,
    /// Headless tick cadence (the std-timer driver's period). Resolved through
    /// `TickConfig` (argv > env > default 60 Hz); its `nonzero` lowering maps 0
    /// to the default. Driver-only — the desktop chassis frame-drives instead.
    pub tick_period: Duration,
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
        // The bin handles `--print-config` / `--describe` (print + exit) before
        // this resolver runs; `config` names the file source and takes no part
        // in staging.
        let config_file = load_chassis_config(cli.config.clone())?;

        // ADR-0156 §5 (issue 3872): assemble the source stack — the loaded
        // config file plus every cap member's typed argv overlay, staged in one
        // derived `StageArgv` call off the CLI declaration itself (each `*Overlay`
        // carries a leaf `StageArgv` and each root delegates to its fields). No
        // hand-maintained per-cap `set_argv` block to forget, and a
        // staged-but-never-composed overlay fails boot loudly. Section identity
        // comes from each member's `ConfigMember` declaration, so no chassis-side
        // section string survives.
        let mut sources = ConfigSources::new(config_file);
        cli.stage(&mut sources);

        // Headless-only tick cadence: resolved through `TickConfig` (argv > env >
        // default) off the shared stack. `nonzero` maps 0 to the default (60 Hz);
        // a garbage value hard-errors. Resolved before the shared block: these
        // are independent reads off the same stack, so the interleaving order is
        // arbitrary.
        let tick_period = sources.resolve::<TickConfig>()?.to_tick_period();

        // The shared block (fs roots, content-gen, runtime, pool / ring /
        // scheduler / teardown knobs) plus the boot-manifest autoload list, both
        // resolved by the single common resolver off the same stack.
        let (common, autoload) = CommonEnv::resolve(sources)?;

        Ok(Self { common, tick_period, autoload })
    }
}

impl HeadlessChassis {
    /// Build the headless chassis: stand up substrate-core internals,
    /// compose the capability chain via [`Self::compose`], then wrap the
    /// timer in a [`HeadlessTimerDriverCapability`] and hand it to the
    /// builder.
    fn build_inner(mut env: HeadlessEnv) -> Result<BuiltChassis<Self>, BootError> {
        let boot = SubstrateBoot::build()?;
        // #3849: `SubstrateBoot::build` installed the subscriber with an
        // env-or-`info` filter (before the config file loaded); re-apply the
        // fully-resolved `AETHER_LOG_FILTER` directive (env > `[runtime]` file >
        // `info`) now so a filter set only in the config file takes effect.
        apply_filter(&env.common.runtime.log_filter);
        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let mailer = Arc::clone(&boot.queue);

        // Driver-only / post-build fields, read out before `compose` consumes
        // `env`: the tick cadence rides the timer driver, the autoload list is
        // drained after build. The `Copy` knobs also feed the boot log line.
        let tick_period = env.tick_period;
        let workers = env.common.workers;
        let ring_capacities = env.common.ring_capacities;
        let autoload = mem::take(&mut env.autoload);

        // Tick rates are bounded well below `u32::MAX` Hz (typically
        // 60-240 Hz); the `u128 → u32` narrowing is safe in practice.
        #[allow(clippy::cast_possible_truncation)]
        let tick_hz = (Duration::from_secs(1).as_nanos() / tick_period.as_nanos().max(1)) as u32;
        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            tick_hz = tick_hz,
            log_ring_capacity = ring_capacities.log,
            trace_ring_capacity = ring_capacities.trace,
            trace_ring_max_capacity = ring_capacities.trace_max,
            "componentless boot — load a component via aether.component.load",
        );

        let builder = Self::compose(&boot, env);
        // ADR-0156 §4 (was ADR-0090 §4 e1): warn on any unknown `AETHER_*` env
        // var, sweeping against the composition-derived known-key set plus the
        // residual hand records. Runs here (not in `from_env`) because the
        // per-chassis known keys come from the composed builder's manifest.
        validate_env(&builder.config_manifest().known_keys(&Self::residual_knobs()))?;
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
    use aether_substrate::chassis::config_manifest;

    #[test]
    fn headless_known_keys_drop_window_and_audio_and_keep_tick() {
        // ADR-0156 §4 acceptance: the aggregate is derived from what headless
        // actually composes, so the over-claim of the old flat registry dies —
        // headless composes no window driver and no audio cap, so its known
        // keys no longer include those knobs (a stale `AETHER_WINDOW_MODE` on a
        // headless box now fails the sweep honestly). Membership is asserted
        // from the composition walk, not a hand list.
        let manifest = config_manifest::<HeadlessChassis>().expect("headless config manifest");
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
