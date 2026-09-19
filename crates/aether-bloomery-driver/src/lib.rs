//! Sans-io program core for the native bundle driver (ADR-0226).
//!
//! This crate holds the program half of the driver: startup recovery, `Call`
//! handling, the per-digest pipeline (section check, closure read, load,
//! invoke), and `Transition` / `Fault` recording. It owns every ADR-0226
//! program decision (decisions 3, 4, and 9 for programs, and 11 for `Call`)
//! as a state machine over the journal folds.
//!
//! The core is sans-io: calls and typed replies go in, [`Command`]s come
//! out, and the core itself performs no mail, threads, or clock reads. The
//! native actor that sends and receives its mail arrives in #6202; that
//! shell stores each command's ticket as the request context, takes it back
//! from the reply, and routes the reply to the matching [`ProgramCore`]
//! method. A reply whose ticket the core is not waiting on returns no
//! commands.
//!
//! Journal bytes are the only source of fold state: after a `Committed`
//! append the core reads its own records back before its next decision,
//! and at most one fenced [`AppendRecords`](aether_bloomery_kinds::AppendRecords)
//! is ever in flight.
//!
//! The [`BundleDriver`] actor is the native shell around the core. Native code
//! spawns it over a born journal owner, passing the journal's id in
//! [`DriverParams`]; it performs the core's commands as mail to the journal
//! owner, the component host, and loaded program roots, and feeds each reply
//! back through its ticketed continuation. A native `Call` is answered with
//! exactly one `CallOutcome` once its outcome is recorded.

#![forbid(unsafe_code)]

mod actor;
mod bundles;
mod core;
mod programs;
mod recovery;

pub use actor::{BundleDriver, DriverParams, ProgramBundleRoot};
pub use core::{
    AppendTicket, ArtifactTicket, CallerId, ClosureTicket, Command, EVENTS_PAGE, EventsTicket, InvokeTicket,
    LoadOutcome, LoadTicket, ProgramCore,
};
