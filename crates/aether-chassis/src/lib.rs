//! aether-chassis: the shared chassis-composition layer (ADR-0073,
//! issue #3809).
//!
//! Every chassis binary composes the same substrate base — the common
//! cap set, the fleet-wide config registry, the argv/env resolution
//! stack, boot-time component autoload — and only the driver stack
//! (winit + wgpu, std timer, RPC coordinator, loopback harness)
//! differs. This crate owns that shared layer:
//!
//! - [`boot`] — the `Builder` boot fragments, the fleet-wide config
//!   registry behind `--print-config` / known-key sweeps, and the
//!   chassis-wide boot knobs.
//! - [`cli`] — the per-chassis clap roots and per-cap overlay
//!   composition (ADR-0090 unit d).
//! - [`autoload`] — boot-time component autoload shared by the
//!   full-stack chassis (issue #1529).
//! - [`bundle_pack`] — the embedded component-pack format the
//!   standalone bundle binaries decode (their build script compiles
//!   this file in via `#[path]` so encoder and decoder share source).
//! - [`WindowConfig`] / [`TickConfig`] — the desktop window and headless tick boot
//!   knobs, declared here because the fleet-wide registry and the CLI
//!   roots name their derived layers/overlays.
//!
//! The layer sits above the cap crates: they all depend on
//! `aether-substrate` for their runtime halves, so shared composition
//! cannot live in the substrate without a cycle.

use std::sync::atomic::{AtomicU64, Ordering};

pub mod autoload;
pub mod boot;
pub mod bundle_pack;
pub mod cli;
pub mod package;
pub mod tick;
pub mod window;

pub use aether_substrate::chassis::{BuildProvenance, PreludeAction, PreludeFlags};
pub use boot::{
    RenderSizeConfig, build_provenance, chassis_residual_knobs, hub_residual_knobs, resolve_teardown_budget,
    run_describe_prelude,
};
pub use tick::{DEFAULT_TICK_HZ, TickConfig, TickConfigLayer, TickOverlay, apply_manifest_tick_settings};
pub use window::{
    WindowConfig, WindowConfigLayer, WindowOverlay, WindowSettings, apply_manifest_window_settings,
    parse_window_mode_env,
};

/// Atomically advance `counter` and return the next non-zero id for
/// synthetic chassis-root mail (ADR-0080 §6). Both the headless driver
/// and the substrate-harness bin own an `AtomicU64` of these;
/// symmetric with the per-actor counter on `NativeBinding`, with zero
/// reserved as the `MailId::NONE` sentinel.
pub fn next_chassis_correlation(counter: &AtomicU64) -> u64 {
    let id = counter.fetch_add(1, Ordering::Relaxed);
    if id == 0 {
        counter.fetch_add(1, Ordering::Relaxed)
    } else {
        id
    }
}
