// aether-substrate: the thin native base layer that owns hardware and
// hosts the WASM runtime. See ADR-0002 for the architecture and
// ADR-0004 for the scheduler baseline this library embodies.
//
// Milestone 1 (issue #18) provides the library shape only: mail
// envelope, mailbox registry, WASM component wrapper, worker-pool
// scheduler, and the `send_mail` host function. Milestone 1 PR B
// wires these into a real frame-loop binary with a first component.

pub mod capture;
pub mod component;
pub mod control;
pub mod ctx;
pub mod host_fns;
pub mod hub_client;
pub mod input;
pub mod log_capture;
pub mod mail;
pub mod queue;
pub mod registry;
pub mod scheduler;
pub mod sender_table;

pub use capture::{CaptureQueue, PendingCapture};
pub use component::Component;
pub use control::{AETHER_CONTROL, ControlPlane};
pub use input::{InputSubscribers, new_subscribers, remove_from_all, subscribers_for};
// ADR-0019 PR 5: control-plane payload types now live as schema kinds
// in `aether-kinds` (LoadComponent, LoadResult, etc.).
// Re-exports of the old `*Payload` structs are gone — consumers
// import from `aether-kinds` directly.
pub use ctx::SubstrateCtx;
pub use hub_client::{HubClient, HubOutbound};
pub use mail::{Mail, MailKind, MailboxId};
pub use queue::MailQueue;
pub use registry::{MailboxEntry, Registry, SinkHandler};
pub use scheduler::Scheduler;

/// Well-known mailbox name for fan-out to every attached Claude
/// session (ADR-0008). A component or substrate-owned sink sends to
/// this name the same way it sends to any local sink; the forwarder
/// translates to `EngineToHub::Mail { address: Broadcast, ... }`.
pub const HUB_CLAUDE_BROADCAST: &str = "hub.claude.broadcast";
