//! aether-substrate-core: runtime that every substrate chassis shares.
//!
//! Hosts the wasmtime engine, the mail scheduler, the per-mailbox
//! component table, the kind manifest, the sender table, the control-
//! plane dispatcher, and the hub-socket client. Chassis-specific
//! peripherals (window, GPU, TCP listener, event loop) live in the
//! chassis crate that binds this as a dependency. See ADR-0035.
//!
//! The chassis surface is the `Chassis` trait re-exported at the
//! crate root: core calls into it when a control-plane kind needs
//! peripheral work (platform info snapshot, window mode change,
//! capture-request wake); the chassis implements the trait over
//! whatever event loop or proxy its environment provides. Tests and
//! host paths that don't drive a real chassis wire `NoopChassis`.

pub mod capture;
pub mod chassis;
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

pub use capture::{CaptureQueue, PendingCapture};
pub use chassis::{Chassis, NoopChassis, noop_chassis};
pub use component::Component;
pub use control::{AETHER_CONTROL, ControlPlane};
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
