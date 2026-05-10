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

use alloc::string::String;
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
        /// Issue 734: OS thread name captured at the dispatcher's
        /// receive hook (`std::thread::current().name()`). The
        /// substrate names every actor's dispatcher thread per
        /// ADR-0038 (`aether-instanced-<full_name>` for instanced
        /// actors, `aether-root-<NAMESPACE>` for singletons), so this
        /// gives the chrome trace renderer a stable per-actor tid
        /// without inferring it from the recipient mailbox. `None`
        /// when the OS thread has no name (anonymous test threads,
        /// `std::thread::spawn` without `Builder::new().name(...)`).
        thread_name: Option<String>,
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

/// Issue 718 (ADR-0080 Phase 2): request kind sent to
/// [`TRACE_OBSERVER_MAILBOX_NAME`] to describe the mail tree under a
/// given root. The observer replies with [`DescribeTreeResult`]; the
/// hub MCP `describe_tree` tool wraps the round-trip.
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.trace.describe_tree")]
pub struct DescribeTree {
    pub root: aether_data::MailId,
}

/// Issue 718: reply to [`DescribeTree`]. `Ok` carries the root's
/// current `in_flight` count and one [`MailNodeWire`] per mail in the
/// tree (no ordering guarantee — agents reconstruct via `parent`
/// edges). `Err::not_found` is returned when the root isn't present
/// in the observer (never-seen or evicted past retention).
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.trace.describe_tree_result")]
pub enum DescribeTreeResult {
    Ok {
        root: aether_data::MailId,
        in_flight: u32,
        mails: Vec<MailNodeWire>,
    },
    Err {
        not_found: aether_data::MailId,
    },
}

/// Issue 718: wire shape of one node in [`DescribeTreeResult`]. The
/// observer keeps the same logical fields in its in-memory `MailNode`;
/// this struct mirrors them on the wire so the hub can decode without
/// pulling in the cap's internal type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub struct MailNodeWire {
    pub mail_id: MailId,
    pub parent: Option<MailId>,
    pub sender: MailboxId,
    pub recipient: MailboxId,
    pub kind: KindId,
    pub t_sent: Nanos,
    pub t_received: Option<Nanos>,
    pub t_finished: Option<Nanos>,
    /// Issue 734: OS thread name captured at the dispatcher's receive
    /// hook (`std::thread::current().name()`). `None` until the
    /// `Received` event lands. See [`TraceEvent::Received::thread_name`]
    /// for the producer-side semantics.
    pub thread_name: Option<String>,
}

/// Issue 718: request kind for the recent-roots summary. `since_ms`
/// filters to roots whose originating `Sent` event is no older than
/// the given window (default 60_000 ms). `max` caps the reply length
/// (default 50, hard cap 1000).
#[derive(
    Copy,
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
#[kind(name = "aether.trace.list_active_roots")]
pub struct ListActiveRoots {
    pub since_ms: Option<u32>,
    pub max: Option<u32>,
}

/// Issue 718: reply to [`ListActiveRoots`]. `roots` is sorted by
/// `t_sent` descending (most recent first). Empty when the observer
/// has no roots in the requested window.
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
#[kind(name = "aether.trace.list_active_roots_result")]
pub struct ListActiveRootsResult {
    pub roots: Vec<RootSummaryWire>,
}

/// Issue 718: per-root summary in [`ListActiveRootsResult`]. The
/// non-counter fields (`kind`, `sender`, `recipient`, `t_sent`) come
/// from the root's own `MailNode` — the observer guarantees the root
/// node lives as long as the root entry, so the lookup never misses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub struct RootSummaryWire {
    pub root: MailId,
    pub kind: KindId,
    pub sender: MailboxId,
    pub recipient: MailboxId,
    pub t_sent: Nanos,
    pub in_flight: u32,
}

/// ADR-0080 §6 settlement notification. Emitted by
/// [`crate::trace::BatchedTraceEvents`]'s consumer
/// (`TraceObserverCapability`) when a causal chain's `in_flight`
/// counter hits zero, addressed to
/// [`aether_data::MailboxId::CHASSIS_MAILBOX_ID`]. The chassis-side
/// dispatcher switch routes this kind into the gate-site
/// notification map and signals every subscriber waiting on `root`.
///
/// **Settlement is a hint, not a guarantee.** Per ADR-0080 §6,
/// consumers MUST be idempotent — duplicate `Settled { root }` mail
/// for the same root is a no-op for any waiter that already woke.
/// The observer's eviction may also lose late `Finished` events, in
/// which case settlement is reported earlier than strictly correct;
/// the gate-site contract is "settles eventually," not "settles only
/// once every dependency is provably done."
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.trace.settled")]
pub struct Settled {
    pub root: aether_data::MailId,
}
