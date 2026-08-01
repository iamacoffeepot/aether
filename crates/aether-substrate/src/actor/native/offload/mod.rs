//! Moving work off the handler thread while staying coherent with the trace
//! pipeline.
//!
//! A native actor is single-threaded, so anything slow a handler does blocks
//! its own mail intake. Three shapes answer that, and they differ in how long
//! the settlement hold has to live:
//!
//! - `thread::spawn_inherit` — a thread that captures the spawning handler's
//!   in-flight chain, so the hold dies when the thread does (ADR-0080 §12).
//! - `thread::spawn_detached` — the same without the inheritance; no hold at
//!   all, a fresh chain.
//! - [`blocking`] — ADR-0093 hold-until-resolve, for work that replies in a
//!   *later* handler turn. The worker pushes a result and dies, and the reply
//!   is sent from a subsequent invocation, so the hold has to outlive the
//!   worker and neither thread shape fits.
//!
//! [`task_queue`] sits above [`blocking`] rather than beside it: the framework
//! owns the spawn, hold, and completion routing, and the one thing it
//! deliberately does not centralise is a per-cap concurrency bound. That bound
//! is what rate-limits the paid provider endpoints (ADR-0050 §2).
//!
//! Not to be confused with [`super::spawn`], which brings new *actors* into
//! being rather than moving work off an existing one.

pub mod blocking;
pub mod task_queue;
pub mod thread;
