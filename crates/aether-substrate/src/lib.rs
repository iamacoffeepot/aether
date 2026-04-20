//! aether-substrate: the desktop-chassis binary crate.
//!
//! ADR-0035 Phase 1 moved the runtime into `aether-substrate-core`.
//! This crate now holds the winit event loop, wgpu renderer, and the
//! `DesktopChassis` adapter that wires the event loop's proxy back
//! into core's `Chassis` trait. Phase 2 will retire this lib in
//! favour of a dedicated `aether-substrate-desktop` crate; until
//! then the lib re-exports core's surface so binary-level callers
//! (`main.rs`, render/chassis modules) import through one namespace.

pub use aether_substrate_core::{
    AETHER_CONTROL, CaptureQueue, Chassis, Component, ControlPlane, HUB_CLAUDE_BROADCAST,
    HubClient, HubOutbound, InputSubscribers, Mail, MailKind, MailQueue, MailboxEntry, MailboxId,
    NoopChassis, PendingCapture, Registry, Scheduler, SinkHandler, SubstrateCtx, capture, chassis,
    component, control, ctx, host_fns, hub_client, input, kind_manifest, log_capture, mail,
    new_subscribers, noop_chassis, queue, registry, remove_from_all, scheduler, sender_table,
    subscribers_for,
};
