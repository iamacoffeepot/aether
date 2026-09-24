//! Sans-io driver core for the native bundle driver (ADR-0226).
//!
//! This crate holds both roles of the driver: startup recovery, `Call`
//! handling, the per-digest program pipeline (section check, closure read,
//! invoke) over one load per digest serving both roles, and `Transition` /
//! `Fault` recording, plus the reactor half (journal following, membership,
//! activation, live delivery, and reaction records). It owns every ADR-0226
//! program decision (decisions 3,
//! 4, and 9 for programs, and 11 for `Call`) and every reactor routing
//! decision (decisions 5-9 for reactors, and 10-11 for `WatchHead` and
//! `AwaitProcessed`) as a state machine over the journal folds.
//!
//! The core is sans-io: calls and typed replies go in, [`Command`]s come
//! out, and the core itself performs no mail, threads, or clock reads. The
//! native actor that sends and receives its mail stores each command's ticket
//! as the request context, takes it back from the reply, and routes the reply
//! to the matching [`ProgramCore`] method. A reply whose ticket the core is
//! not waiting on returns no commands.
//!
//! Journal bytes are the only source of fold state: after a `Committed`
//! append the core reads its own records back before its next decision,
//! and at most one fenced [`AppendRecords`](aether_bloomery_kinds::AppendRecords)
//! is ever in flight.
//!
//! The [`BundleDriver`] actor is the native shell around the core. Native code
//! spawns it over a born journal owner, passing the journal's reference in
//! [`DriverParams`]; it performs journal reads, appends, and the watch as mail
//! to the journal owner, bundle loads for both roles to the component host,
//! `Invoke` to loaded program roots, and `Warm` / `Event` / `StatusQuery` to
//! loaded reactor roots — each root the stamped sender of its bundle's load
//! reply, kept by digest — and feeds each reply back through its ticketed
//! continuation. A native `Call` is answered with exactly one `CallOutcome`
//! once its outcome is recorded, and `AwaitProcessed` is answered with
//! `Processed` once its bound is quiescent. A bundle root's fetch-on-miss
//! is answered by the driver itself, from a byte-bounded cache of found
//! artifacts or one journal read shared by every fetch of that digest.

#![forbid(unsafe_code)]

mod actor;
mod bundles;
mod core;
mod programs;
mod reactors;
mod recovery;

pub use actor::{BundleDriver, DriverParams};
pub use core::{
    AppendTicket, ArtifactTicket, CallerId, ClosureTicket, Command, EVENTS_PAGE, EvaluateTicket, EventsTicket,
    InvokeTicket, LoadOutcome, LoadTicket, ProgramCore, StatusTicket, WarmTicket, WatchTicket,
};
