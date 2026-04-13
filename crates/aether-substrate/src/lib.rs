// aether-substrate: the thin native base layer that owns hardware and
// hosts the WASM runtime. See ADR-0002 for the architecture and
// ADR-0004 for the scheduler baseline this library embodies.
//
// Milestone 1 (issue #18) provides the library shape only: mail
// envelope, mailbox registry, WASM component wrapper, worker-pool
// scheduler, and the `send_mail` host function. Milestone 1 PR B
// wires these into a real frame-loop binary with a first component.

pub mod component;
pub mod ctx;
pub mod host_fns;
pub mod mail;
pub mod queue;
pub mod registry;
pub mod scheduler;

pub use component::Component;
pub use ctx::SubstrateCtx;
pub use mail::{Mail, MailKind, MailboxId};
pub use queue::MailQueue;
pub use registry::{MailboxEntry, Registry, SinkHandler};
pub use scheduler::Scheduler;
