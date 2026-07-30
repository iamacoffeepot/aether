//! Desktop chassis: `DesktopChassis` (ADR-0035 / ADR-0071) and the
//! [`DesktopChassis::build`] entry point that assembles the substrate
//! + driver into a [`BuiltChassis`] for `main()` to drive.
//!
//! Issue 603 retired `chassis_handler` entirely: window kinds go through
//! driver-as-actor on `aether.window` (Phase 3), and `platform_info` was
//! deleted as a kind (Phase 4) along with the closure-fallback that served
//! it. ADR-0161 R3 made capture a mail-driven state machine inside the pumped
//! `aether.render` actor. ADR-0164 places the winit handler and its user-event
//! vocabulary in `aether-window`; the chassis only constructs and runs that
//! application.

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
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::{BootableChassis, composed};
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate::{Chassis, SubstrateBoot};
use winit::event_loop::EventLoop;

use aether_chassis::{WindowConfig, apply_manifest_window_settings};

use super::driver::DesktopDriverCapability;
use aether_chassis::autoload::autoload_mail;
use aether_chassis::boot::{ChassisBase, CommonEnv, chassis_residual_knobs, with_full_stack_caps, with_rpc_server};

use crate::cli::DesktopCli;
use aether_substrate::config::{ConfigError, KnobRecord, validate_env};
use winit::event_loop::ControlFlow;

pub use aether_window::DesktopWindowUserEvent as UserEvent;

/// Marker type for the desktop chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<DesktopChassis>`] returned
/// by [`Self::build`]. The unit struct exists so the `chassis_builder`
/// machinery can parameterise over a concrete chassis kind for type
/// disambiguation, and so [`Chassis::PROFILE`] has a home.
pub struct DesktopChassis;

impl Chassis for DesktopChassis {
    const PROFILE: &'static str = "desktop";
    type Driver = DesktopDriverCapability;
    type Env = CommonEnv;

    /// Build the desktop chassis: construct the Start-stage runtime handle
    /// (the winit event loop), stand up substrate-core internals, compose the
    /// capability chain via [`BootableChassis::compose`], then wrap everything
    /// in a [`DesktopDriverCapability`] and hand it to the builder. Returns a
    /// [`BuiltChassis`] whose [`BuiltChassis::run`] blocks on the winit event
    /// loop.
    fn build(mut env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
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

        let mut boot = SubstrateBoot::build()?;
        // #3849: `SubstrateBoot::build` installed the subscriber with an
        // env-or-`info` filter (before the config file loaded); re-apply the
        // fully-resolved `AETHER_LOG_FILTER` directive (env > `[runtime]` file >
        // `info`) now so a filter set only in the config file takes effect.
        apply_filter(&env.runtime.log_filter);
        let mailer = Arc::clone(&boot.queue);

        // ADR-0162 §config-at-its-seam: the window boot knobs and the render
        // tuning `Config` are driver config — their consumer is the desktop
        // driver (which boots the pumped `aether.render` actor, ADR-0161 R3), not
        // a composed cap — so they resolve HERE, off the base's source stack, at
        // the seam that constructs the driver rather than pre-resolved into a
        // per-chassis env bag. `lower` delegates the window mode to
        // `parse_window_mode_env`: a present-but-bad `AETHER_WINDOW_MODE` aborts
        // boot (ADR-0090 §4), an absent value resolves to `Windowed`.
        //
        // Issue 4001: a depot package (`--package`) manifest's title / window
        // mode overlay onto the stack BELOW argv/env, ABOVE the compiled
        // defaults, before the resolve — so a shipped package comes up titled and
        // in its window mode while an operator's `AETHER_WINDOW_*` still wins.
        let package_settings = env.package_settings.clone();
        apply_manifest_window_settings(&mut env.base.sources, &package_settings)?;
        let window = env.base.sources.resolve::<WindowConfig>()?.lower()?;
        // ADR-0161 R3: the render tuning `Config` (vertex-buffer cap) resolves off
        // the same stack the pooled `with_actor::<RenderCapability>` path resolved
        // it before the swap, and rides to the driver alongside the window knobs.
        let render_config = env.base.sources.resolve::<RenderTuningConfig>()?;
        // The `assets` root threads into the pumped render actor's params for
        // `capture_frame` similarity references.
        let assets_dir = env.namespace_roots.assets.clone();
        // #3930: the non-cap members ride as resolved structs now; lower `workers`
        // for the boot log line (the same lowered value as before). The fused
        // `with_chassis_config_member` install re-lowers it onto the builder seam
        // during `compose`.
        let workers = env.chassis_boot.to_workers();
        // The autoload list is drained after build; lift the base stratum out so
        // the framework mints the builder and installs the aborter + base ahead of
        // `compose` (the leftover default `env.base` is never re-read).
        let autoload = mem::take(&mut env.autoload);
        let base = mem::take(&mut env.base);

        tracing::info!(
            target: "aether_substrate::boot",
            workers_override = ?workers,
            "componentless boot — close window to exit; load a component via aether.component.load",
        );

        let builder = composed::<Self>(&mut boot, base, env)?;
        // ADR-0156 §4 (was ADR-0090 §4 e1): warn on any unknown `AETHER_*` env
        // var, sweeping against the composition-derived known-key set plus the
        // residual hand records. Runs here (not in `resolve_env`) because the
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

impl BootableChassis for DesktopChassis {
    type Base = ChassisBase;

    /// Resolve the shared env off the source stack (ADR-0162): the lone
    /// per-chassis token is the `DesktopCli` type. `CommonEnv::resolve` embeds
    /// the base stratum; splitting it out is what the describe / config helpers
    /// hand `composed`, while `Chassis::build` keeps it embedded so it can resolve
    /// the window / render driver knobs off `base.sources` first.
    fn resolve_env() -> Result<(Self::Base, Self::Env), ConfigError> {
        let mut env = CommonEnv::resolve(DesktopCli::default())?;
        let base = mem::take(&mut env.base);
        Ok((base, env))
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
    /// [`Chassis::build`] adds the desktop driver and starts (the driver's
    /// `aether.window` claim rides its Claim-stage hook either way), while the
    /// describe / config helpers read the claim / config terminals off it.
    ///
    /// Takes the boot handle by reference — [`Chassis::build`] moves the same
    /// `boot` into the driver afterward. The window boot knobs (mode / size /
    /// title / wireframe) and the render tuning `Config` are driver config that
    /// [`Chassis::build`] resolves at the driver seam (ADR-0162), so they take no
    /// part in this claim chain. Desktop's delta resolves nothing itself, so it
    /// returns `Ok`.
    fn compose(builder: Builder<Self>, boot: &SubstrateBoot, env: CommonEnv) -> Result<Builder<Self>, BootError> {
        let component_host_params = ComponentHostParams {
            engine: Arc::clone(&boot.engine),
            linker: Arc::clone(&boot.linker),
            hub_outbound: Arc::clone(&boot.outbound),
        };
        // ADR-0161 R3: render no longer composes on the pooled `with_actor`
        // path on desktop — the driver boots the pumped `aether.render` actor
        // itself (owning the surface + capture as plain state on the winit
        // thread). The render `Config` (vertex-buffer cap) and the `assets`
        // root resolve in `Chassis::build` and ride `DesktopDriverCapability`
        // instead of a `RenderParams` handoff here.

        // Boot order is declaration order — `with_full_stack_caps` runs the base
        // app caps first, render last so it claims its mailboxes after every
        // other chassis cap. `into_common_boot` reads the env-sourced
        // `CommonBoot` fields off the shared env in one place; the aborter and
        // source stack are supplied earlier by `composed` / `ChassisBase` (the
        // base + autoload were lifted out in `Chassis::build`).
        let common = env.into_common_boot(component_host_params, aether_game::GameGatewayParams::default());
        // ADR-0082 §11 / issues 1378 + 1489: desktop drives the shared
        // `Tick → Render → Present → Tick` frame graph, with the `Quit`
        // escape to `Shutdown` on `Present` so OS-close / ctrlc drain the
        // in-flight frame before shutting down (see the driver's
        // `CloseRequested` → `Quit` bridge and terminal-reached exit).
        let builder = with_full_stack_caps(builder, common)
            .with_actor::<AudioCapability>(())
            .with_actor::<ClipboardCapability>(ClipboardParams::System)
            .with_actor::<UnsupportedSubstrateHarnessCapability>(())
            .with_actor::<LifecycleCapability>(frame_lifecycle_params());
        Ok(with_rpc_server(builder).with_actor::<HttpServerCapability>(()))
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
