//! aether-substrate-core: runtime that every substrate chassis shares.
//!
//! Hosts the wasmtime engine, the mail scheduler, the per-mailbox
//! component table, the kind manifest, the sender table, the control-
//! plane dispatcher, and the hub-socket client. Chassis-specific
//! peripherals (window, GPU, TCP listener, event loop) live in the
//! chassis crate that binds this as a dependency. See ADR-0035.
//!
//! Core does NOT define a `Chassis` trait. The chassis boundary is
//! the control-plane fallback on `ControlPlane::chassis_handler`:
//! core dispatches its own kinds (load/drop/replace/subscribe/
//! unsubscribe) and falls through to the chassis-registered handler
//! for everything else. Each chassis owns the control kinds it cares
//! about (desktop registers capture_frame / set_window_mode /
//! platform_info; headless registers nothing; hub chassis will
//! register its own). No per-chassis no-op methods, no trait surface
//! to keep in sync.
//!
//! Helpers for chassis-side handlers live under `control`:
//! `decode_payload` and `resolve_bundle` are pub so chassis dispatch
//! can validate mail bundles the same way core does.

pub mod component;
pub mod control;
pub mod ctx;
pub mod host_fns;
pub mod hub_client;
pub mod input;
pub mod kind_manifest;
pub mod log_capture;
pub mod mail;
pub mod queue;
pub mod registry;
pub mod scheduler;
pub mod sender_table;

pub use component::Component;
pub use control::{AETHER_CONTROL, ChassisControlHandler, ControlPlane};
pub use ctx::SubstrateCtx;
pub use hub_client::{HubClient, HubOutbound};
pub use input::{InputSubscribers, new_subscribers, remove_from_all, subscribers_for};
pub use mail::{Mail, MailKind, MailboxId};
pub use queue::MailQueue;
pub use registry::{MailboxEntry, Registry, SinkHandler};
pub use scheduler::Scheduler;

/// Well-known mailbox name for fan-out to every attached Claude
/// session (ADR-0008). A component or substrate-owned sink sends to
/// this name the same way it sends to any local sink; the forwarder
/// translates to `EngineToHub::Mail { address: Broadcast, ... }`.
pub const HUB_CLAUDE_BROADCAST: &str = "hub.claude.broadcast";
