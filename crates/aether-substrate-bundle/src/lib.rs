//! aether-substrate-bundle: multi-binary chassis crate (ADR-0073).
//!
//! Standard Cargo layout:
//!
//! - `src/bin/` — the `aether-substrate-harness` chassis entry point
//!   plus the standalone bundle and perf bins. The desktop, headless,
//!   and hub chassis live in `aether-chassis-desktop` /
//!   `aether-chassis-headless` / `aether-chassis-hub`.
//!
//! The substrate-harness chassis machinery and the in-process
//! `SubstrateHarness` live in the `aether-harness-substrate` crate
//! (GPU capture support in `aether-harness-substrate-capture`); this
//! crate keeps the `aether-substrate-harness` binary entry point. The
//! shared chassis-composition layer — boot fragments, config registry,
//! CLI roots, autoload, the bundle-pack format — lives in
//! `aether-chassis` (issue #3809).
//!
//! The lib root re-exports a convenience surface (the most-used
//! `aether-substrate` runtime types) so external consumers —
//! components, integration tests, the scenario runner, demos — can
//! write `use aether_substrate_bundle::{Registry, ...};` instead of
//! chasing through chassis submodules. The shared substrate runtime
//! (mail scheduler, registry, wasmtime host, capabilities) lives in
//! `aether-substrate` — depend on that directly when you don't need
//! chassis surface.

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
