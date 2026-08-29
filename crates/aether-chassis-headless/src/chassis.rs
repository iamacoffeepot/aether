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
use aether_substrate::chassis::builder::{Builder, BuiltChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::chassis::{BootableChassis, composed};
use aether_substrate::{Chassis, SubstrateBoot};
use aether_window::HeadlessWindowCapability;

use aether_chassis::{TickConfig, apply_manifest_tick_settings};

use super::driver::HeadlessTimerDriverCapability;
use aether_chassis::autoload::autoload_mail;
use aether_chassis::boot::{
    ChassisBase, CommonEnv, chassis_residual_knobs, tick_only_lifecycle_params, with_full_stack_caps, with_rpc_server,
};
use aether_substrate::config::{ConfigError, KnobRecord, validate_env};

use crate::cli::HeadlessCli;
use aether_substrate::mail::registry::MailDispatch;
use aether_substrate::runtime::log_install::apply_filter;

/// Marker type for the headless chassis. Carries no fields — the
/// chassis instance is the [`BuiltChassis<HeadlessChassis>`] returned
/// by `Self::build`. Same shape as the desktop chassis marker post
/// ADR-0071 phase 3.
pub struct HeadlessChassis;

impl Chassis for HeadlessChassis {
    const PROFILE: &'static str = "headless";
    type Driver = HeadlessTimerDriverCapability;
    type Env = CommonEnv;

    /// Build the headless chassis: stand up substrate-core internals,
    /// compose the capability chain via [`BootableChassis::compose`], then
    /// wrap the timer in a [`HeadlessTimerDriverCapability`] and hand it to
    /// the builder.
    fn build(mut env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        let mut boot = SubstrateBoot::build()?;
        // #3849: `SubstrateBoot::build` installed the subscriber with an
        // env-or-`info` filter (before the config file loaded); re-apply the
        // fully-resolved `AETHER_LOG_FILTER` directive (env > `[runtime]` file >
        // `info`) now so a filter set only in the config file takes effect.
        apply_filter(&env.runtime.log_filter);
        let kind_tick = boot.registry.kind_id(Tick::NAME).expect("Tick registered");
        let mailer = Arc::clone(&boot.queue);

        // ADR-0162 §config-at-its-seam: the tick cadence is driver config — its
        // consumer is the std-timer driver's loop period, not a composed cap — so
        // it resolves HERE, off the base's source stack, at the seam that
        // constructs the timer driver rather than pre-resolved into a per-chassis
        // env bag. `TickConfig`'s `nonzero` lowering maps 0 to the default (60 Hz).
        //
        // Issue 4001: a depot package (`--package`) manifest's tick cadence
        // overlays onto the stack BELOW argv/env, ABOVE the compiled default,
        // before the resolve — so a shipped package runs at its cadence while an
        // operator's `AETHER_TICK_HZ` / `--tick-hz` still wins.
        let package_settings = env.package_settings.clone();
        apply_manifest_tick_settings(&mut env.base.sources, &package_settings)?;
        let tick_period = env.base.sources.resolve::<TickConfig>()?.to_tick_period();
        // #3930: the non-cap members ride as resolved structs now; lower the two
        // the boot log line reports (the same lowered values as before). The fused
        // `with_chassis_config_member` install re-lowers `workers` onto the builder
        // seam during `compose`.
        let workers = env.chassis_boot.to_workers();
        let ring_capacities = env.base.actor_ring.to_ring_capacities();
        // The autoload list is drained after build; lift the base stratum out so
        // `composed` installs the aborter + base ahead of `compose` (the leftover
        // default `env.base` is never re-read).
        let autoload = mem::take(&mut env.autoload);
        let base = mem::take(&mut env.base);

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

        let builder = composed::<Self>(&mut boot, base, env)?;
        // ADR-0156 §4 (was ADR-0090 §4 e1): warn on any unknown `AETHER_*` env
        // var, sweeping against the composition-derived known-key set plus the
        // residual hand records. Runs here (not in `resolve_env`) because the
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

impl BootableChassis for HeadlessChassis {
    type Base = ChassisBase;

    /// Resolve the shared env off the source stack (ADR-0162): the lone
    /// per-chassis token is the `HeadlessCli` type. `CommonEnv::resolve` embeds
    /// the base stratum; splitting it out is what the describe / config helpers
    /// hand `composed`, while `Chassis::build` keeps it embedded so it can resolve
    /// the tick driver knob off `base.sources` first.
    fn resolve_env() -> Result<(Self::Base, Self::Env), ConfigError> {
        let mut env = CommonEnv::resolve(HeadlessCli::default())?;
        let base = mem::take(&mut env.base);
        Ok((base, env))
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
    /// [`Chassis::build`] adds the timer driver and starts, while the describe /
    /// config helpers read the claim / config terminals off it.
    ///
    /// Takes the boot handle by reference — [`Chassis::build`] moves the same
    /// `boot` into the timer driver afterward. The tick cadence is driver config
    /// that [`Chassis::build`] resolves at the driver seam (ADR-0162), so it
    /// takes no part in this claim chain. Headless's delta resolves nothing
    /// itself, so it returns `Ok`.
    fn compose(builder: Builder<Self>, boot: &SubstrateBoot, env: CommonEnv) -> Result<Builder<Self>, BootError> {
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
        //
        // iamacoffeepot/aether#4171: the direct mutator is named through the
        // boot's authority, borrowed for this call. `composed` spends the token
        // the moment this delta returns, so the sink registration is inside the
        // window and nothing after it — including the driver that ends up owning
        // this `boot` — can reach the direct write path.
        let authority = boot.authority().ok_or(BootError::AlreadyComposed)?;
        let kind_set_master_gain = boot.registry.kind_id(SetMasterGain::NAME).expect("SetMasterGain registered");
        let outbound_for_audio_sink = Arc::clone(&boot.outbound);
        boot.registry.register_inline(
            authority,
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

        // Boot order is declaration order. `into_common_boot` reads the
        // env-sourced `CommonBoot` fields off the shared env in one place; the
        // aborter and source stack are supplied earlier by `composed` /
        // `ChassisBase` (the base + autoload were lifted out in `Chassis::build`).
        let common = env.into_common_boot(component_host_params);
        // ADR-0082 §1 / PR 3b: headless uses the shared Tick-only
        // lifecycle graph (Tick self-loops, Quit escapes to Shutdown);
        // the timer pushes `LifecycleAdvance` and the driver broadcasts
        // Tick to `aether.input` via the relay subscriber.
        let builder = with_full_stack_caps(builder, common)
            .with_actor::<HeadlessRenderCapability>(())
            .with_actor::<HeadlessClipboardCapability>(())
            .with_actor::<HeadlessWindowCapability>(())
            .with_actor::<UnsupportedSubstrateHarnessCapability>(())
            .with_actor::<LifecycleCapability>(tick_only_lifecycle_params());
        Ok(with_rpc_server(builder).with_actor::<HttpServerCapability>(()))
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
        // `FrameSizeConfig` declared in `ChassisBase`), so the composition
        // walk — not `chassis_residual_knobs` — is what now claims them. Catches a
        // dropped `declare_config_member` or a de-composed RPC server reintroducing
        // a false unknown-key warning.
        assert!(
            known.contains("AETHER_MAX_FRAME_SIZE"),
            "headless must claim the frame-size knob via the composed FrameSizeConfig member"
        );
        assert!(known.contains("AETHER_RPC_PORT"), "headless must claim the RPC port via the composed RpcServerConfig");
        assert!(known.contains("AETHER_LOG_FILTER"), "headless must claim the log filter via the RuntimeConfig member");
        assert!(known.contains("AETHER_CRASH_LOG_DIR"), "headless must claim the panic-hook knobs via RuntimeConfig");
    }
}
