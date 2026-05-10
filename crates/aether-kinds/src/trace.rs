//! ADR-0080 substrate-wide mail tracing wire vocabulary.
//!
//! - [`Nanos`] — monotonic timestamp in nanoseconds since substrate
//!   boot. The chassis owns the `SUBSTRATE_START` reference (a
//!   `Once<std::time::Instant>` set at boot); producer-side hooks
//!   compute `now.duration_since(SUBSTRATE_START).as_nanos() as u64`
//!   per ADR-0080 §2.
//! - [`TraceEvent`] — one trace event emitted at a producer site.
//!   Three variants: `Sent` at the sender (every outbound mail),
//!   `Received` at the receiver's dispatcher entry, `Finished` at the
//!   receiver's dispatcher exit. The observer folds these into per-
//!   root counters and the parent → mail graph.
//! - [`BatchedTraceEvents`] — what the chassis drainer thread mails
//!   to the [`TRACE_OBSERVER_MAILBOX_NAME`] sink, batching events to
//!   amortise dispatch cost (defaults: BATCH_MAX = 256, BATCH_INTERVAL
//!   = 1ms; see ADR-0080 §3).

use alloc::vec::Vec;

use aether_data::{KindId, MailId, MailboxId};
use serde::{Deserialize, Serialize};

/// ADR-0080 §3: well-known mailbox name the chassis-owned drainer
/// thread sends [`BatchedTraceEvents`] to. The
/// `TraceObserverCapability` (in `aether-capabilities`) registers
/// against this name at boot.
pub const TRACE_OBSERVER_MAILBOX_NAME: &str = "aether.trace";

/// ADR-0080 §2: monotonic-since-boot timestamp in nanoseconds. Cheap
/// to read (~10–20 ns VDSO `clock_gettime(CLOCK_MONOTONIC)` on
/// Linux/macOS); the subtraction against `SUBSTRATE_START` adds ~1–2
/// ns. `u64` covers ~584 years from boot — wraparound is not a
/// concern. Process-global / system-wide clock source means cross-
/// actor events are directly comparable without skew correction.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    aether_data::Schema,
)]
pub struct Nanos(pub u64);

/// ADR-0080 §2: one trace event emitted at a producer site.
///
/// `Sent` carries the full causal-graph context: the outgoing mail's
/// own `mail_id`, the chain `root` it inherits or originates, the
/// optional `parent_mail` at the sender (None for chassis-root), the
/// producer mailbox, the recipient mailbox, the kind, and the
/// timestamp. `Received` and `Finished` only carry `mail_id` + `t` —
/// the observer joins them to the originating `Sent` via the mail-id
/// key in its [`MailNode`](self) graph (defined in the observer cap,
/// not on the wire).
///
/// Wire shape: postcard. The dispatcher delivers this through normal
/// mail routing; no cast-shape optimisation because the variant tag +
/// `Option<MailId>` would force padding gymnastics anyway.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub enum TraceEvent {
    Sent {
        mail_id: MailId,
        root: MailId,
        parent_mail: Option<MailId>,
        sender: MailboxId,
        recipient: MailboxId,
        kind: KindId,
        t: Nanos,
    },
    Received {
        mail_id: MailId,
        t: Nanos,
    },
    Finished {
        mail_id: MailId,
        t: Nanos,
    },
}

/// ADR-0080 §3: a batch of [`TraceEvent`]s the chassis drainer ships
/// to the [`TRACE_OBSERVER_MAILBOX_NAME`] sink. Batching amortises the
/// per-mail observer dispatch cost — defaults `BATCH_MAX = 256` events
/// or `BATCH_INTERVAL = 1ms`, whichever fires first.
///
/// The drainer pushes via `Sender::send_detached` (ADR-0080 §7) so the
/// observer's own outbound mail does not recurse back through the
/// trace pipeline.
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.trace.batched_events")]
pub struct BatchedTraceEvents {
    pub events: Vec<TraceEvent>,
}
