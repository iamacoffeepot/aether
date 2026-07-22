//! Desktop chassis: `DesktopChassis` (ADR-0035 / ADR-0071), the
//! `UserEvent` enum the winit event loop consumes, and the
//! [`DesktopChassis::build`] entry point that assembles the substrate
//! + driver into a [`BuiltChassis`] for `main()` to drive.
//!
//! Issue 603 retired `chassis_handler` entirely: window kinds go through
//! driver-as-actor on `aether.window` (Phase 3), and `platform_info` was
//! deleted as a kind (Phase 4) along with the closure-fallback that served
//! it. ADR-0161 R3 made capture a mail-driven state machine inside the pumped
//! `aether.render` actor (deleting `UserEvent::Capture`), so the sole proxy
//! event is `UserEvent::WindowMail` — the generic "a pumped slot took mail,
//! wake the loop" signal both the window and render slots poke, so
//! `about_to_wait` drains them even under `ControlFlow::Wait`
//! (iamacoffeepot/aether#1318).

use std::io;
use std::mem;
use std::sync::Arc;

use aether_audio::AudioCapability;
use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_component::ComponentHostParams;
use aether_harness_substrate::UnsupportedSubstrateHarnessCapability;
use aether_http::HttpServerCapability;
use aether_lifecycle::{LifecycleCapability, frame_lifecycle_params};
use aether_render::RenderTuningConfig;
use aether_substrate::chassis::BootableChassis;
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot};
use winit::event_loop::EventLoop;

use aether_chassis::{WindowConfig, WindowSettings};

use super::driver::DesktopDriverCapability;
use aether_chassis::autoload::{AutoloadComponent, autoload_mail};
use aether_chassis::boot::{CommonEnv, chassis_residual_knobs, load_chassis_config, with_common_caps, with_rpc_server};
use aether_chassis::cli::DesktopCli;
use aether_substrate::config::{ConfigError, ConfigSources, KnobRecord, StageArgv, validate_env};
use aether_substrate::runtime::lifecycle::FatalAborter;
use aether_substrate::runtime::lifecycle::OutboundFatalAborter;
use winit::event_loop::ControlFlow;

/// Event the event-loop thread consumes from the desktop chassis. Both
/// variants are wake-only — they turn the loop so a handler runs, never
/// carrying work themselves.
#[derive(Debug, Clone)]
pub enum UserEvent {
    /// A pumped slot (`aether.window` or, since ADR-0161 R3, `aether.render`)
    /// took mail; wake the loop so `about_to_wait` drains it even under
    /// `ControlFlow::Wait` (iamacoffeepot/aether#1318). Without this a
    /// window-control mail (`aether.window.focus` / `set_mode` / `set_title`)
    /// or a `capture_frame` sent to an occluded window sits undrained until an
    /// unrelated winit event nudges the loop. ADR-0161 generalized the
    /// ADR-0160 window rule (and deleted the render-specific `Capture` wake) —
    /// every pumped slot's wake pokes this.
    WindowMail,
    /// A SIGINT/SIGTERM was observed by the signal-watcher thread
    /// (iamacoffeepot/aether#1489). Carries no work itself — it only
    /// wakes the loop so `about_to_wait` observes the shutdown flag and
    /// runs the `Quit`-push path, mirroring `WindowMail`. Needed because
    /// an async-signal-safe handler can't poke winit, and a parked
    /// (`ControlFlow::Wait`, occluded) loop otherwise never runs
    /// `about_to_wait` to see the flag.
    Quit,
}

/// Marker type for the desktop chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<DesktopChassis>`] returned
/// by [`Self::build`]. The unit struct exists so the `chassis_builder`
/// machinery can parameterise over a concrete chassis kind for type
/// disambiguation, and so [`Chassis::PROFILE`] has a home.
pub struct DesktopChassis;

impl Chassis for DesktopChassis {
    const PROFILE: &'static str = "desktop";
    type Driver = DesktopDriverCapability;
    type Env = DesktopEnv;

    fn build(env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        Self::build_inner(env)
    }
}

impl BootableChassis for DesktopChassis {
    fn resolve_env() -> Result<Self::Env, ConfigError> {
        DesktopEnv::from_env()
    }

    fn residual_knobs() -> Vec<KnobRecord> {
        chassis_residual_knobs()
    }

    /// Compose the desktop capability chain — the single claim/build path
    /// (ADR-0155) both [`Chassis::build`] and the describe / config helpers run,
    /// so the manifest roster can never drift from what boots. Composes the
    /// common caps plus the audio / clipboard / render / substrate-harness /
    /// lifecycle caps and the always-claim RPC + HTTP servers (ADR-0155 §3).
    /// Returns the composed builder before the driver is installed:
    /// `build_inner` adds the desktop driver and starts (the driver's
    /// `aether.window` claim rides its Claim-stage hook either way), while the
    /// describe / config helpers read the claim / config terminals off it.
    ///
    /// Takes the boot handle by reference — `build_inner` moves the same
    /// `boot` into the driver afterward. The window boot knobs (mode / size /
    /// title / wireframe) and the `autoload` list ride [`DesktopEnv`] but take
    /// no part in the claim chain, so they are ignored here.
    fn compose(boot: &SubstrateBoot, env: DesktopEnv) -> Builder<Self> {
        let DesktopEnv { common, window: _, render: _, autoload: _ } = env;

        let component_host_params = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        // ADR-0161 R3: render no longer composes on the pooled `with_actor`
        // path on desktop — the driver boots the pumped `aether.render` actor
        // itself (owning the surface + capture as plain state on the winit
        // thread). The render `Config` (vertex-buffer cap) and the `assets`
        // root ride `DesktopEnv` → `DesktopDriverCapability` instead of a
        // `RenderParams` handoff here.

        let registry = Arc::clone(&boot.registry);
        let mailer = Arc::clone(&boot.queue);
        // ADR-0063: production chassis configures the fatal-abort
        // aborter so a wasm guest trap exits the substrate via
        // `lifecycle::fatal_abort` instead of unwinding.
        let aborter: Arc<dyn FatalAborter> = Arc::new(OutboundFatalAborter::new(Arc::clone(&boot.outbound)));

        // Boot order is declaration order — `with_common_caps` runs
        // log first so other capabilities' boot tracing routes
        // through the log capture; render last so it claims its
        // mailboxes after every other chassis cap. `into_common_boot` reads the
        // six env-sourced `CommonBoot` fields off the shared env in one place
        // (the teardown budget honors the same `AETHER_SETTLEMENT_CAP_SECS` knob,
        // `0 → wait forever` sentinel and all, as the settlement gates) and
        // threads the source stack back out for the builder.
        let (common, sources) =
            common.into_common_boot(aborter, component_host_params, aether_game::GameGatewayParams::default());
        // ADR-0082 §11 / issues 1378 + 1489: desktop drives the shared
        // `Tick → Render → Present → Tick` frame graph, with the `Quit`
        // escape to `Shutdown` on `Present` so OS-close / ctrlc drain the
        // in-flight frame before shutting down (see the driver's
        // `CloseRequested` → `Quit` bridge and terminal-reached exit).
        //
        // ADR-0156 §5: hand the builder the source stack (`with_config_sources`)
        // so it resolves each composed cap's `Config` (audio / lifecycle /
        // http-server) off it; the caps compose with `with_actor(params)`
        // alone. The render tuning `Config` is resolved off the same stack in
        // `DesktopEnv::from_env_with_argv` and threaded to the driver, which
        // boots the pumped render actor (ADR-0161 R3).
        let builder = with_common_caps(Builder::<Self>::new(registry, mailer).with_config_sources(sources), common)
            .with_actor::<AudioCapability>(())
            .with_actor::<ClipboardCapability>(ClipboardParams::System)
            .with_actor::<UnsupportedSubstrateHarnessCapability>(())
            .with_actor::<LifecycleCapability>(frame_lifecycle_params());
        with_rpc_server(builder).with_actor::<HttpServerCapability>(())
    }
}

/// Bag of resolved config *data* the desktop chassis takes at build time.
/// `main()` populates it from env vars (per ADR-0070's "substrate-core
/// never reads env" invariant); tests construct one directly.
///
/// ADR-0155 §4: this is config only — every field resolves through the
/// argv/env/file path a real boot uses, so `--describe` can resolve it on
/// a headless host. The Start-stage winit `EventLoop` that rides the boot
/// path is not config: it is constructed in `DesktopChassis::build_inner`
/// (winit's `EventLoop` is `!Send` on macOS and is the chassis's main
/// thread, so it stays local to the boot call `main()` makes).
pub struct DesktopEnv {
    /// The config fields every full-stack chassis shares (source stack, fs roots,
    /// content-gen staging, runtime knobs, pool / ring / scheduler / teardown
    /// knobs). Resolved by [`CommonEnv::resolve`] — the single declaration, so a
    /// shared knob can't exist on one chassis and silently not the other.
    pub common: CommonEnv,
    /// Lowered desktop window boot knobs (mode / size / title / wireframe),
    /// grouped into one embedded unit like the other knob groups and
    /// threaded to the desktop driver. Produced by [`WindowConfig::lower`];
    /// `wireframe` reaches the pumped render actor's lazy wgpu boot through
    /// `RenderParams::wireframe`.
    pub window: WindowSettings,
    /// Resolved render tuning `Config` (the vertex-buffer cap). ADR-0161 R3
    /// boots the pumped `aether.render` actor with it from the driver, so it
    /// resolves off the same source stack as every other cap `Config` and
    /// rides to the driver alongside the window knobs rather than through the
    /// pooled `with_actor::<RenderCapability>` compose that the swap removed.
    pub render: RenderTuningConfig,
    /// Components to auto-load on boot, in order. A bundled standalone build
    /// populates this so the game comes up with no hub; the normal desktop bin
    /// leaves it empty and loads components over the hub instead.
    pub autoload: Vec<AutoloadComponent>,
}

impl DesktopEnv {
    /// Resolve every chassis-relevant env var into a fresh `DesktopEnv` of
    /// config *data*. The single env-reading edge for the desktop chassis
    /// (per issue 464). Tests bypass this by constructing `DesktopEnv`
    /// directly.
    ///
    /// ADR-0155 §4: env resolution produces config only — the winit
    /// `EventLoop` is a Start-stage runtime handle constructed on the boot
    /// path in `DesktopChassis::build_inner`, not here — so the only fallible
    /// step is the ADR-0090 §4 config validation / parse path.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a known `AETHER_*` env var (or argv
    /// overlay value) holds an unparseable value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_env_with_argv(DesktopCli::default())
    }

    /// ADR-0090 unit d (issue 1258): resolve every cap config through
    /// the argv-then-env overlay. `cli` carries `Option<T>` flags;
    /// unset fields fall through to env-only resolution, so an empty
    /// argv (the path the existing `from_env` callers exercise) is
    /// byte-identical to the pre-d behaviour.
    ///
    /// # Errors
    ///
    /// See [`Self::from_env`].
    pub fn from_env_with_argv(cli: DesktopCli) -> Result<Self, ConfigError> {
        // ADR-0156 §4: the unknown-`AETHER_*` sweep moved to `build_inner`,
        // where the composed builder's `config_manifest` supplies the
        // per-chassis known-key set (desktop no longer "knows" the headless
        // tick knob).
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

        // Desktop-only window boot knobs: resolved through `WindowConfig` (argv >
        // env > default) and lowered as a unit off the shared stack. `lower`
        // delegates the mode to `parse_window_mode_env` — a present-but-bad
        // `AETHER_WINDOW_MODE` aborts boot via `ConfigError` (ADR-0090 §4), an
        // absent value resolves to `Windowed` — and maps `None` / empty title to
        // `"aether"`. Resolved before the shared block: these are independent
        // reads off the same stack, so the interleaving order is arbitrary.
        let window = sources.resolve::<WindowConfig>()?.lower()?;

        // ADR-0161 R3: the render tuning `Config` resolves off the same stack
        // (argv > env > file), the way the pooled `with_actor::<RenderCapability>`
        // path resolved it before the swap — an independent read, so its order
        // relative to the window read is arbitrary.
        let render = sources.resolve::<RenderTuningConfig>()?;

        // The shared block (fs roots, content-gen, runtime, pool / ring /
        // scheduler / teardown knobs) plus the boot-manifest autoload list, both
        // resolved by the single common resolver off the same stack.
        let (common, autoload) = CommonEnv::resolve(sources)?;

        Ok(Self { common, window, render, autoload })
    }
}

impl DesktopChassis {
    /// Build the desktop chassis: construct the Start-stage runtime handles
    /// (winit event loop + capture queue), stand up substrate-core internals,
    /// compose the capability chain via [`Self::compose`], then wrap
    /// everything in a [`DesktopDriverCapability`] and hand it to the builder.
    /// Returns a [`BuiltChassis`] whose [`BuiltChassis::run`] blocks on the
    /// winit event loop.
    ///
    /// The trait method [`Chassis::build`] forwards here.
    fn build_inner(mut env: DesktopEnv) -> Result<BuiltChassis<Self>, BootError> {
        // ADR-0155 §4: the winit `EventLoop` is a Start-stage runtime handle,
        // not config — construct it here on the boot path (`main()` calls this
        // on the chassis main thread, where winit's `!Send` `EventLoop` must
        // live). `--describe` never reaches this method, so it opens no event
        // loop. The `EventLoop` build fault (never `Send + Sync` across winit's
        // platform impls) is stringified into `BootError::Other`, the same
        // shape a wasmtime boot fault takes. Capture is plain state on the
        // pumped render actor (ADR-0161), so there is no cross-thread queue to
        // hand over.
        let event_loop = EventLoop::<UserEvent>::with_user_event().build().map_err(|e| {
            BootError::Other(Box::new(io::Error::other(format!("desktop event loop build failed: {e}"))))
        })?;
        event_loop.set_control_flow(ControlFlow::Poll);

        let boot = SubstrateBoot::build()?;
        // #3849: `SubstrateBoot::build` installed the subscriber with an
        // env-or-`info` filter (before the config file loaded); re-apply the
        // fully-resolved `AETHER_LOG_FILTER` directive (env > `[runtime]` file >
        // `info`) now so a filter set only in the config file takes effect.
        apply_filter(&env.common.runtime.log_filter);
        let mailer = Arc::clone(&boot.queue);

        // Driver-only / post-build fields, read out before `compose` consumes
        // `env`: the window boot knobs and the render tuning `Config` ride the
        // desktop driver (which boots the pumped render actor, ADR-0161 R3),
        // the `assets` root threads into that actor's params for `capture_frame`
        // similarity references, and the autoload list is drained after build.
        // `WindowSettings` / `RenderTuningConfig` are `Clone` and tiny, so the
        // clones are free.
        let workers = env.common.workers;
        let window = env.window.clone();
        let render_config = env.render.clone();
        let assets_dir = env.common.namespace_roots.assets.clone();
        let autoload = mem::take(&mut env.autoload);

        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            "componentless boot — close window to exit; load a component via aether.component.load",
        );

        let builder = Self::compose(&boot, env);
        // ADR-0156 §4 (was ADR-0090 §4 e1): warn on any unknown `AETHER_*` env
        // var, sweeping against the composition-derived known-key set plus the
        // residual hand records. Runs here (not in `from_env`) because the
        // per-chassis known keys come from the composed builder's manifest.
        validate_env(&builder.config_manifest().known_keys(&Self::residual_knobs()))?;
        // ADR-0161 R3: the driver boots the pumped `aether.render` actor from
        // its Claim-stage `aether.render` reservation, so it carries the render
        // tuning `Config` and the `assets` root here. `boot` moves into the
        // driver, after `compose` finished borrowing it.
        let driver = DesktopDriverCapability { event_loop, boot, window, render_config, assets_dir };
        let built = builder.driver(driver).build()?;
        // Auto-load any bundled components, in order, before the run loop
        // starts. Fire-and-forward: the component host dispatches each load off
        // the worker pool (already up after `build`), so the game is live
        // shortly after `run` begins — no hub required.
        for component in autoload {
            mailer.push(autoload_mail(component));
        }
        Ok(built)
    }
}

#[cfg(test)]
mod config_manifest_tests {
    use super::DesktopChassis;
    use aether_chassis::boot::chassis_residual_knobs;
    use aether_substrate::chassis::config_manifest;

    #[test]
    fn desktop_known_keys_drop_tick_and_keep_window_audio_render() {
        // ADR-0156 §4 acceptance: desktop drives from winit, not a std timer,
        // so its aggregate no longer includes the headless tick knob (the old
        // flat registry over-claimed it). Membership is asserted from the
        // composition walk, not a hand list.
        let manifest = config_manifest::<DesktopChassis>().expect("desktop config manifest");
        let known = manifest.known_keys(&chassis_residual_knobs());
        assert!(!known.contains("AETHER_TICK_HZ"), "desktop drives from winit — must not claim the headless tick knob");
        assert!(known.contains("AETHER_WINDOW_MODE"), "desktop must claim its window-driver knob");
        assert!(known.contains("AETHER_AUDIO_DISABLE"), "desktop must claim the composed audio cap knob");
        assert!(known.contains("AETHER_RENDER_VERTEX_BUFFER_BYTES"), "desktop must claim the composed render cap knob");
    }
}
