//! aether-substrate: the desktop-chassis binary crate.
//!
//! ADR-0035 Phase 1 moved the runtime into `aether-substrate-core`.
//! This crate holds the winit event loop, wgpu renderer, the
//! `CaptureQueue` handoff slot for async capture, and the chassis-
//! side control-plane handler (`chassis::chassis_control_handler`)
//! that registers the desktop-only kinds (capture_frame,
//! set_window_mode, platform_info) against core's ControlPlane
//! fallback. Phase 2 will retire the `aether-substrate` crate name
//! in favour of `aether-substrate-desktop`; until then the lib is
//! a convenience re-export hub for the binary's own modules + the
//! core surface they lean on.

pub mod capture;
pub mod chassis;

pub use aether_substrate_core::{
    AETHER_CONTROL, Chassis, ChassisCapabilities, ChassisControlHandler, Component, ControlPlane,
    HUB_CLAUDE_BROADCAST, HubClient, HubOutbound, InputSubscribers, Mail, MailKind, MailQueue,
    MailboxEntry, MailboxId, Registry, Scheduler, SinkHandler, SubstrateCtx, component, control,
    ctx, host_fns, hub_client, input, kind_manifest, log_capture, mail, new_subscribers, queue,
    registry, remove_from_all, scheduler, sender_table, subscribers_for,
};

pub use capture::{CaptureQueue, PendingCapture};
pub use chassis::{UserEvent, chassis_control_handler};
