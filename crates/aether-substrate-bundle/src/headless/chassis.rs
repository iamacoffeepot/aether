//! Headless chassis: `HeadlessChassis` (ADR-0035 / ADR-0071), the
//! `Err`-replying capability stubs that fail fast for kinds desktop
//! supports natively (capture/window) plus `Advance`, and the
//! [`HeadlessChassis::build`] entry point that assembles the substrate
//! + tick driver into a [`BuiltChassis`].
//!
//! Issue 603 retired the `chassis_handler` closure: each fail-fast
//! kind moved onto its own cap. `HeadlessRenderCapability` (Phase 2)
//! handles `aether.render`; `HeadlessWindowCapability` (Phase 3)
//! handles `aether.window`; `UnsupportedTestBenchCapability` (Phase 4)
//! handles `aether.test_bench`. `aether.control.platform_info` (now
//! a deleted kind name from a retired namespace) was
//! deleted as a kind in Phase 4 — no replacement, no MCP path until
//! issue 603 §F2 revives the per-domain shape.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use aether_capabilities::LifecycleCapability;
use aether_capabilities::{
    AnthropicConfig, ComponentHostConfig, GeminiConfig, HeadlessRenderCapability,
    HeadlessWindowCapability, InputConfig, UnsupportedTestBenchCapability, fs::NamespaceRoots,
    http::HttpConfig as HttpConf,
};
use aether_data::Kind;
use aether_kinds::{SetMasterGain, SetMasterGainResult, Tick};
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Chassis, SubstrateBoot};

use super::driver::{HeadlessTimerCapability, parse_tick_hz_env};
use crate::autoload::{AutoloadComponent, autoload_mail};
use crate::chassis_common::{
    CommonBoot, PersistOverride, chassis_known_keys, maybe_with_rpc_server, resolve_persist_state,
    tick_only_lifecycle_config, with_common_caps,
};
use crate::cli::{CommonOverlay, HeadlessCli};
use crate::hub;
use aether_substrate::config::{ConfigError, validate_env};
use aether_substrate::mail::registry::MailDispatch;
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use std::env;

/// Marker type for the headless chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HeadlessChassis>`] returned
/// by `Self::build`. Same shape as `crate::DesktopChassis` post
/// ADR-0071 phase 3.
pub struct HeadlessChassis;

impl Chassis for HeadlessChassis {
    const PROFILE: &'static str = "headless";
    type Driver = HeadlessTimerCapability;
    type Env = HeadlessEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
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
    pub tick_period: Duration,
    /// Issue 763 P2: optional `aether.rpc.server` bind address.
    /// Populated from `AETHER_RPC_PORT`; `None` (default) skips booting
    /// `RpcServerCapability` so existing chassis behavior is unchanged.
    pub rpc_addr: Option<SocketAddr>,
    /// Issue 745: optional worker-pool size override. Populated from
    /// `AETHER_WORKERS`; `None` keeps `PoolConfig::default()` behavior
    /// (`available_parallelism() - 1`, min 1).
    pub workers: Option<usize>,
    /// ADR-0090 unit d (issue 1258): chassis-bin verdict on handle-
    /// store persistence. [`PersistOverride::EnvOnly`] (the default,
    /// what `from_env()` builds) preserves the pre-d env-only path
    /// byte-identically; [`PersistOverride::Argv`] threads the argv
    /// overlay through to `SubstrateBoot`.
    pub persist: PersistOverride,
    /// ADR-0090 unit d (issue 1258): argv overlay for the handle-store
    /// in-memory byte budget. `None` falls through to env-only
    /// `AETHER_HANDLE_STORE_MAX_BYTES`.
    pub handle_store_max_bytes: Option<usize>,
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
            tick_hz: cli_tick_hz,
            // The bin handles `--config` (print + exit) before this
            // resolver runs; ignore it here.
            config: _,
        } = cli;
        let CommonOverlay {
            http,
            fs,
            anthropic,
            gemini,
            persist,
            workers: cli_workers,
            rpc_port: cli_rpc_port,
        } = common;
        let http = HttpConf::try_from_argv_then_env(http.into_layer())?;
        let anthropic = AnthropicConfig::try_from_argv_then_env(anthropic.into_layer())?;
        let gemini = GeminiConfig::try_from_argv_then_env(gemini.into_layer())?;
        let namespace_roots = NamespaceRoots::from_argv_then_env(fs.into_layer());
        // Persistence overlay shared with desktop (issue 1258); headless
        // opts into on-disk persistence per ADR-0049 §9.
        let persist_state = resolve_persist_state(&persist);
        let handle_store_max_bytes = persist.max_bytes;
        // Chassis-wide knobs: argv-then-env shadow (ad-hoc, lifted to
        // confique in unit e1). `cli.tick_hz` wins when `Some`, falls
        // through to `AETHER_TICK_HZ` / default otherwise.
        let tick_hz = cli_tick_hz
            .filter(|hz| *hz > 0)
            .unwrap_or_else(parse_tick_hz_env);
        let tick_period = Duration::from_nanos(1_000_000_000 / u64::from(tick_hz));
        let rpc_addr = cli_rpc_port
            .or_else(hub::rpc_port_from_env)
            .map(|p| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p));
        let workers = cli_workers.or_else(parse_workers_env);
        Ok(Self {
            namespace_roots,
            http,
            anthropic,
            gemini,
            tick_period,
            rpc_addr,
            workers,
            persist: persist_state,
            handle_store_max_bytes,
            autoload: Vec::new(),
        })
    }
}

//noinspection DuplicatedCode
/// Parse `AETHER_WORKERS`. Unset → `None` (chassis falls back to
/// [`aether_substrate::scheduler::PoolConfig::default`]); positive →
/// `Some(n)`; `0` → `Some(1)` with a warn (the pool requires at least
/// one worker); unparseable → `None` with a warn. Issue 745.
fn parse_workers_env() -> Option<usize> {
    let raw = env::var("AETHER_WORKERS").ok()?;
    match raw.trim().parse::<usize>() {
        Ok(0) => {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                "AETHER_WORKERS=0 — clamping to 1",
            );
            Some(1)
        }
        Ok(n) => Some(n),
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::boot",
                value = %raw,
                error = %e,
                "AETHER_WORKERS unparseable — falling back to PoolConfig::default",
            );
            None
        }
    }
}

impl HeadlessChassis {
    /// Build the headless chassis: stand up substrate-core internals,
    /// register the audio fail-fast sink, connect the hub, compose
    /// the native passives (broadcast/handle/log/control/io/http plus
    /// the headless render / window / test-bench fail-fast caps)
    /// through the `chassis_builder` `.with()` chain, then wrap the
    /// timer in a [`HeadlessTimerCapability`] and hand it to the
    /// builder.
    fn build_inner(env: HeadlessEnv) -> Result<BuiltChassis<Self>, BootError> {
        let HeadlessEnv {
            namespace_roots,
            http,
            anthropic,
            gemini,
            tick_period,
            rpc_addr,
            workers,
            persist,
            handle_store_max_bytes,
            autoload,
        } = env;

        // ADR-0049 §9: headless enables on-disk handle persistence.
        // ADR-0090 unit d: when the chassis bin parsed an argv overlay
        // for persist config / max_bytes, those override the env-only
        // resolution `SubstrateBoot` would otherwise run.
        let mut boot_builder = SubstrateBoot::builder("headless", env!("CARGO_PKG_VERSION"))
            .persist_enabled(true)
            .handle_store_max_bytes(handle_store_max_bytes);
        if let PersistOverride::Argv(p) = persist {
            boot_builder = boot_builder.persist_config(p);
        }
        let boot = boot_builder.build()?;
        let component_host_config = ComponentHostConfig {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        let input_config = InputConfig::default();

        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");

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
        let kind_set_master_gain = boot
            .registry
            .kind_id(SetMasterGain::NAME)
            .expect("SetMasterGain registered");
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

        // Tick rates are bounded well below `u32::MAX` Hz (typically
        // 60-240 Hz); the `u128 → u32` narrowing is safe in practice.
        #[allow(clippy::cast_possible_truncation)]
        let tick_hz = (Duration::from_secs(1).as_nanos() / tick_period.as_nanos().max(1)) as u32;
        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            tick_hz = tick_hz,
            "componentless boot — load a component via aether.component.load",
        );

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0063: production chassis configures the fatal-abort
        // aborter so a wasm guest trap exits the substrate via
        // `lifecycle::fatal_abort` instead of unwinding.
        let aborter: Arc<dyn FatalAborter> =
            Arc::new(OutboundFatalAborter::new(Arc::clone(&boot.outbound)));

        let driver = HeadlessTimerCapability {
            boot,
            kind_tick,
            tick_period,
        };

        // ADR-0071 phase B: io / http / log compose through the
        // chassis_builder `.with()` chain. Boot order is declaration
        // order — `with_common_caps` runs log first so other
        // capabilities' boot tracing routes through the log capture.
        let common = CommonBoot {
            aborter,
            workers,
            input_config,
            component_host_config,
            namespace_roots,
            http,
            anthropic,
            gemini,
        };
        // ADR-0082 §1 / PR 3b: headless uses the shared Tick-only
        // lifecycle graph (Tick self-loops, Quit escapes to Shutdown);
        // the timer pushes `LifecycleAdvance` and the driver broadcasts
        // Tick to `aether.input` via the relay subscriber.
        let builder = with_common_caps(Builder::<Self>::new(registry, Arc::clone(&mailer)), common)
            .with_actor::<HeadlessRenderCapability>(())
            .with_actor::<HeadlessWindowCapability>(())
            .with_actor::<UnsupportedTestBenchCapability>(())
            .with_actor::<LifecycleCapability>(tick_only_lifecycle_config());
        let builder = maybe_with_rpc_server(builder, rpc_addr, "aether-headless");
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
mod tests {
    use super::parse_workers_env;
    use std::env;
    use std::sync::Mutex;
    use std::sync::PoisonError;

    /// Process-wide guard around `AETHER_WORKERS` env mutation —
    /// `cargo test` parallelises within a binary, so each parser test
    /// has to serialise its set/remove pair. Shared with the desktop
    /// chassis test would require a crate-level module; one per chassis
    /// is fine given there are four tests total.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env<R>(value: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        // Safety: this test owns the AETHER_WORKERS slot for the
        // duration of the closure via ENV_LOCK; no other thread inside
        // the same test binary mutates it concurrently. Edition-2024
        // marked the env mutators unsafe due to non-test signal-handler
        // races that don't apply here.
        unsafe {
            match value {
                Some(v) => env::set_var("AETHER_WORKERS", v),
                None => env::remove_var("AETHER_WORKERS"),
            }
        }
        let out = f();
        // SAFETY: same justification as the prior block — this test
        // still owns the `AETHER_WORKERS` slot via `ENV_LOCK`.
        unsafe {
            env::remove_var("AETHER_WORKERS");
        }
        out
    }

    #[test]
    fn parse_workers_unset_returns_none() {
        let parsed = with_env(None, parse_workers_env);
        assert_eq!(parsed, None);
    }

    #[test]
    fn parse_workers_positive_returns_some() {
        let parsed = with_env(Some("4"), parse_workers_env);
        assert_eq!(parsed, Some(4));
    }

    #[test]
    fn parse_workers_zero_clamps_to_one() {
        let parsed = with_env(Some("0"), parse_workers_env);
        assert_eq!(parsed, Some(1));
    }

    #[test]
    fn parse_workers_unparseable_returns_none() {
        let parsed = with_env(Some("abc"), parse_workers_env);
        assert_eq!(parsed, None);
    }
}
