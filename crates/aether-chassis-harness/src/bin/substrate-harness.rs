//! Harness chassis binary entry point. The chassis, CLI root, env, and pump
//! loop live in `aether_chassis_harness`; this binary composes them.
//!
//! It cannot use the shared `chassis_main!` body: the harness is a **passive**
//! chassis, so there is no driver for the framework to run and `main` is the
//! driver — it composes through `composed`, terminates in `build_passive`,
//! claims the pumped `aether.render` slot (ADR-0161), and then owns the loop on
//! this thread. Everything ahead of that divergence is the shared ceremony:
//! the ADR-0162 `--describe` / `--print-config` prelude, env resolution off the
//! source stack, the resolved log filter, and the unknown-`AETHER_*` sweep over
//! the composed known-key set.

use std::mem;
use std::sync::Arc;

use aether_chassis::run_describe_prelude;
use aether_chassis_harness::{HarnessChassis, HarnessCli, HarnessDriver, HarnessEnv};
use aether_render::{RenderCapability, RenderParams};
use aether_substrate::Chassis;
use aether_substrate::SubstrateBoot;
use aether_substrate::chassis::settlement::PumpWake;
use aether_substrate::chassis::{BootableChassis, composed};
use aether_substrate::config::validate_env;
use aether_substrate::runtime::log_install::apply_filter;
use aether_substrate_harness_cap::events::ChassisEvent;
use clap::Parser as _;

fn main() -> anyhow::Result<()> {
    let cli = HarnessCli::parse();
    // ADR-0162 shared prelude: `--print-config` (ADR-0090 §4 dump) and
    // `--describe` (ADR-0115 manifest) print and exit before Init; a plain
    // invocation falls through to boot.
    if run_describe_prelude::<HarnessChassis>(&cli.meta)?.is_handled() {
        return Ok(());
    }
    let (mut env, events_rx) = HarnessEnv::resolve(cli)?;

    let mut boot = SubstrateBoot::build()?;
    // #3849: `SubstrateBoot::build` installed the subscriber with an
    // env-or-`info` filter (before the config file loaded); re-apply the
    // fully-resolved `AETHER_LOG_FILTER` directive now.
    apply_filter(&env.runtime.log_filter);

    // ADR-0161: the pumped render actor is claimed post-build, so its wiring is
    // read off the env before composition consumes it. The `assets` root feeds
    // `capture_frame` similarity references; the event sender feeds the slot's
    // wake.
    let (width, height) = env.render_size.to_size();
    let render_config = env.render.clone();
    let assets_dir = env.namespace_roots.assets.clone();
    let render_events = env.events.clone();
    let base = mem::take(&mut env.base);

    let builder = composed::<HarnessChassis>(&mut boot, base, env)?;
    // ADR-0156 §4: warn on any unknown `AETHER_*` env var, swept against the
    // composition-derived known-key set plus the residual hand records.
    validate_env(&builder.config_manifest().known_keys(&HarnessChassis::residual_knobs()))?;
    let passive = builder.build_passive()?;

    // ADR-0161: boot the pumped `aether.render` actor offscreen. It claims the
    // `aether.render` slot post-build (a no-driver chassis reserved none at
    // Claim), owning the surfaceless GPU, accumulators, and pending capture as
    // plain state; the GPU boots lazily on the first frame from
    // `offscreen_size`. `..Default::default()` fills `wireframe` and — under a
    // feature-unified build that enables aether-render/desktop — the
    // desktop-only `window: None`, so this literal is robust to unification.
    let (render_slot, render_wake_slot) = passive.boot_pumped_actor::<RenderCapability>(
        render_config,
        RenderParams {
            observed_kinds: None,
            assets_dir: Some(assets_dir),
            offscreen_size: Some((width, height)),
            ..Default::default()
        },
    )?;

    let mut driver = HarnessDriver::new(&boot, Arc::clone(passive.settlement_registry()), render_slot);
    // ADR-0161 §Decision 2: the unified `PumpWake` channel. The render slot's
    // mailbox wake sends `PumpWake::Mail` so the advance-loop settlement wait
    // drains on mail arrival, and *also* pokes `ChassisEvent::RenderMail` so a
    // render mail landing while the loop is parked (a settled capture pre-mail's
    // `pre_settled` notice) turns the loop and drains the slot.
    let wake_pump_tx = driver.wake_sender();
    render_wake_slot.set(Arc::new(move || {
        let _ = wake_pump_tx.send(PumpWake::Mail);
        let _ = render_events.send(ChassisEvent::RenderMail);
    }));
    driver.prime();

    tracing::info!(
        target: "aether_substrate::boot",
        width,
        height,
        profile = HarnessChassis::PROFILE,
        "substrate-harness componentless boot — drive ticks via aether.substrate_harness.advance; the render runtime boots lazily offscreen on the first frame",
    );

    driver.run(&events_rx);

    // Drop ordering: run the pumped render actor's Closed-path teardown
    // (`unwire` logs the triangle count) BEFORE dropping `passive` (the composed
    // caps shut down) → `boot` (scheduler join).
    driver.shutdown();
    drop(driver);
    drop(passive);
    drop(boot);
    Ok(())
}
