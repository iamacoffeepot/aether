//! aether-substrate-bundle: multi-binary chassis crate.
//!
//! Standard Cargo layout:
//!
//! - `src/<chassis>/` — chassis-specific source (chassis impl,
//!   driver capability, render plumbing, etc.) for desktop, headless,
//!   hub. Mirrors what each former chassis-binary crate held.
//! - `src/hub/` — the hub library (substrate-side client, wire types,
//!   MCP coordinator, hub chassis).
//! - `src/bin/<chassis>.rs` — minimal entry point per binary.
//!
//! The lib root re-exports a convenience surface (the hub types and
//! the most-used `aether-substrate` types) so external consumers
//! — `aether-substrate-test-bench`, integration tests — can write
//! `use aether_substrate_bundle::{HubClient, Registry, ...};` instead of
//! chasing through chassis submodules. The shared substrate runtime
//! (mail scheduler, registry, wasmtime host, capabilities) lives in
//! `aether-substrate` — depend on that directly when you don't need
//! chassis or hub surface.

pub mod desktop;
pub mod headless;
pub mod hub;
pub mod test_bench;

pub use aether_substrate::{
    AETHER_CONTROL, Chassis, ChassisControlHandler, Component, ControlPlane, HUB_CLAUDE_BROADCAST,
    HubOutbound, InputSubscribers, KindId, Mail, MailKind, MailboxEntry, MailboxId, Mailer,
    Registry, ReplyTarget, ReplyTo, Scheduler, SinkHandler, SubstrateBoot, SubstrateCtx,
    capabilities,
    capture::{CaptureQueue, PendingCapture},
    component, control, ctx, frame_loop, host_fns, input, kind_manifest, log_capture, mail, mailer,
    new_subscribers, registry, remove_from_all, reply_table, scheduler, subscribers_for,
};
pub use hub::{HubClient, dispatch_hub_mail_by_id, dispatch_hub_to_engine_mail};
