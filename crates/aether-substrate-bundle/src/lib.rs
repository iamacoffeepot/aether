//! aether-substrate-bundle: multi-binary chassis crate (ADR-0073).
//!
//! Standard Cargo layout:
//!
//! - `src/<chassis>/` — chassis-specific source (chassis impl,
//!   driver capability, render plumbing, etc.) for the desktop,
//!   headless, and hub chassis.
//! - `src/hub/` — the hub chassis (the `aether-substrate-hub` binary's
//!   thin Chassis impl post-issue-763 P5f).
//! - `src/bin/<chassis>.rs` — minimal entry point per binary
//!   (`aether-substrate`, `aether-substrate-headless`,
//!   `aether-substrate-hub`, `aether-substrate-harness`).
//!
//! The substrate-harness chassis machinery and the in-process
//! `SubstrateHarness` live in the `aether-harness-substrate` crate
//! (GPU capture support in `aether-harness-substrate-capture`); this
//! crate keeps the `aether-substrate-harness` binary entry point.
//!
//! The lib root re-exports a convenience surface (the most-used
//! `aether-substrate` runtime types) so external consumers —
//! components, integration tests, the scenario runner, demos — can
//! write `use aether_substrate_bundle::{Registry, ...};` instead of
//! chasing through chassis submodules. The shared substrate runtime
//! (mail scheduler, registry, wasmtime host, capabilities) lives in
//! `aether-substrate` — depend on that directly when you don't need
//! chassis surface.

pub mod autoload;
pub mod bundle_pack;
mod chassis_common;
pub use chassis_common::{
    RenderSizeConfig, binary_manifest, chassis_config_dump, common_cap_namespaces, hub_config_dump, hub_known_keys,
    resolve_teardown_cap,
};
pub mod chassis_root;
pub mod cli;
pub mod desktop;
pub mod headless;
pub mod hub;

pub use aether_component::{ComponentHostCapability, ComponentHostConfig};
pub use aether_substrate::{
    Chassis, Component, ComponentCtx, HubOutbound, InboxHandler, InlineHandler, KindId, Mail, MailKind, MailboxEntry,
    MailboxId, Mailer, OwnedDispatch, Registry, RingCapacities, SchedulerTuning, Source, SourceAddr, SubstrateBoot,
    actor::wasm::{component, host_fns, kind_manifest, reply_table},
    capture::{CaptureQueue, PendingCapture},
    chassis::frame_loop,
    mail,
    mail::mailer,
    mail::registry,
    runtime::log_install,
};
