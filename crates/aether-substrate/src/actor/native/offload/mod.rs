//! Moving work off the handler thread while staying coherent with the trace
//! pipeline.
//!
//! A native actor is single-threaded, so anything slow a handler does blocks
//! its own mail intake. Three shapes answer that, and they differ in how long
//! the settlement hold has to live:
//!
//! - `thread::spawn_inherit` — a worker that sends nothing and holds the
//!   spawning handler's in-flight chain, so the hold dies when the thread
//!   does (ADR-0080 §12).
//! - `thread::spawn_detached` — a worker that sends nothing and holds no
//!   chain at all.
//! - [`blocking`] — ADR-0093 hold-until-resolve, for work that replies in a
//!   *later* handler turn. The worker pushes a result and dies, and the reply
//!   is sent from a subsequent invocation, so the hold has to outlive the
//!   worker and neither thread shape fits.
//! - [`self_wake`] — the handle a cap's own long-lived thread holds to wake
//!   its actor, in place of a stored mailbox id plus a mailer (ADR-0230), and
//!   the sanctioned spawn for that thread (`SelfWake::spawn_sidecar`).
//! - [`check_in`] — the handle a [`blocking`] worker holds to check bytes
//!   into the engine blob store off the dispatcher, one buffer at a time or
//!   as one slab, in place of a ctx it never gets (ADR-0238). It sends
//!   nothing.
//! - `fail_fast` — the runner every sanctioned spawn above wraps its body
//!   in: a panic on any of these threads is fatal (ADR-0063) and escalates
//!   through the chassis aborter, as the scheduler escalates a handler panic.
//!
//! [`task_queue`] sits above [`blocking`] rather than beside it: the framework
//! owns the spawn, hold, and completion routing, and the one thing it
//! deliberately does not centralise is a per-cap concurrency bound. That bound
//! is what rate-limits the paid provider endpoints (ADR-0050 §2).
//!
//! Not to be confused with [`super::spawn`], which brings new *actors* into
//! being rather than moving work off an existing one.

pub mod blocking;
pub mod check_in;
pub(crate) mod fail_fast;
pub mod self_wake;
pub mod task_queue;
pub mod thread;
