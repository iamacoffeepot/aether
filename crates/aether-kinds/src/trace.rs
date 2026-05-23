//! ADR-0080 substrate-wide mail tracing wire vocabulary.
//!
//! - [`Nanos`] — monotonic timestamp in nanoseconds since substrate
//!   boot. The chassis owns the `SUBSTRATE_START` reference (a
//!   `Once<std::time::Instant>` set at boot); producer-side hooks
//!   compute `now.duration_since(SUBSTRATE_START).as_nanos() as u64`
//!   per ADR-0080 §2.
//! - [`TraceEvent`] — one trace event emitted at a producer site.
//!   `Sent` at the sender (every outbound mail), `Received` at the
//!   receiver's dispatcher entry, `Finished` at the receiver's
//!   dispatcher exit, plus the `HoldOpen` / `Release` settlement-hold
//!   pair (ADR-0080 §12). Post-ADR-0086 Phase 3c these land in the
//!   producing actor's per-actor ring ([`TraceRingEntry`]), queried via
//!   [`TraceTail`] and stitched client-side; there is no central fold.

use alloc::string::String;
use alloc::vec::Vec;

use aether_data::{KindId, MailId, MailboxId};
use serde::{Deserialize, Serialize};

use crate::MailEnvelope;

/// ADR-0080 §3 (slimmed by ADR-0086 Phase 3c): well-known mailbox name
/// the `TraceDispatchCapability` (in `aether-capabilities`) registers
/// against at boot. It now services only [`DispatchTraced`] — the
/// central trace drainer that used to ship `BatchedTraceEvents` here
/// retired with the fold.
pub const TRACE_MAILBOX_NAME: &str = "aether.trace";

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
/// timestamp. `Received` and `Finished` only carry `mail_id` + `t`;
/// the guided walk (`trace_walk`) joins them to the originating `Sent`
/// by the mail-id key while stitching the per-actor ring slices.
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
        /// substrate's default `Pooled` scheduler (post-issue-635)
        /// names worker threads `aether-worker-N`, so the trace
        /// renderer (`hub::mcp::trace`) can distinguish per-thread
        /// rows even when one OS thread serves multiple actors. Actors that opt into
        /// the `Thread` scheduler get `aether-instanced-<full_name>` /
        /// `aether-root-<NAMESPACE>` from `actor::native::spawn` and
        /// `spawn_thread`. `None` when the OS thread has no name
        /// (anonymous test threads, `std::thread::spawn` without
        /// `Builder::new().name(...)`).
        thread_name: Option<String>,
    },
    Finished {
        mail_id: MailId,
        t: Nanos,
    },
    /// ADR-0080 §12 / iamacoffeepot/aether#716: a thread-spawn primitive
    /// (currently `InheritCtx<A>` via `NativeCtx::spawn_inherit`) acquired
    /// a `SettlementHold` against `root`. The observer increments the
    /// root's `held_open` counter and gates `Settled` emission on
    /// `(in_flight == 0 && held_open == 0)`. Pushed by the parent thread
    /// before the worker thread is spawned, so by the time `Finished`
    /// lands for the parent handler the hold is already visible.
    HoldOpen {
        root: MailId,
        t: Nanos,
    },
    /// Companion to [`Self::HoldOpen`]. Pushed by `SettlementHold`'s
    /// `Drop` impl when the worker thread exits; the observer decrements
    /// the root's `held_open` counter and may fire `Settled` if both
    /// counters reached zero.
    Release {
        root: MailId,
        t: Nanos,
    },
}

/// ADR-0086 Phase 3b: reply shape the guided walk (`trace_walk`)
/// produces after stitching the per-actor ring slices for one root.
/// `Ok` carries the root's current `in_flight` count and one
/// [`MailNodeWire`] per mail in the tree (no ordering guarantee —
/// consumers reconstruct via `parent` edges). `Err::not_found` is
/// returned when no ring held the root's own `Sent` (never-seen or
/// lapped past every ring's window).
///
/// Not routed as mail post-3c — the central observer that used to reply
/// with it retired. Kept as the walk's output struct (still a `Kind` so
/// the MCP layer can name/decode it uniformly).
#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.trace.describe_tree_result")]
pub enum DescribeTreeResult {
    Ok {
        root: MailId,
        in_flight: u32,
        mails: Vec<MailNodeWire>,
    },
    Err {
        not_found: MailId,
    },
}

/// One node in a [`DescribeTreeResult`]: a single mail folded from its
/// `Sent` (+ optional `Received` / `Finished`) ring entries. The guided
/// walk (`trace_walk`) builds these from the per-actor ring slices; the
/// MCP layer renders them into the trace tree.
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

/// ADR-0086 Phase 3: one entry in an actor's `ActorTraceRing` as it
/// appears on the wire when a coordinator queries the ring via
/// [`TraceTail`] / [`TraceTailResult`].
///
/// `sequence` is monotonic *per ring*, starting at 1 — the cursor for
/// [`TraceTail::since`]. `root` is stored explicitly even for
/// `Received` / `Finished` events (whose [`TraceEvent`] variants don't
/// carry it on the wire) because the producer hooks have it at push
/// time; this lets a coordinator filter a ring by root server-side and
/// stitch the tree without the central observer's by-mail join.
///
/// Not a `Kind` — only addressable as an element of
/// [`TraceTailResult::Ok::entries`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, aether_data::Schema)]
pub struct TraceRingEntry {
    pub sequence: u64,
    pub root: MailId,
    pub event: TraceEvent,
}

/// ADR-0086 Phase 3: `aether.trace.tail` — query one actor's
/// `ActorTraceRing`. Routed to a specific actor by `MailboxId`; the
/// framework dispatch loop services it directly (every native actor and
/// every wasm trampoline answers without the author writing a handler),
/// the same surface [`crate::LogTail`] established for log rings. The
/// trace-tree coordinator fans this out across live actors and stitches
/// the per-ring slices by lineage keys. Reply: [`TraceTailResult`].
///
/// - `max == 0` resolves to the substrate-default cap; the reply slice
///   never exceeds the ring's hard ceiling even on a full ring.
/// - `since: None` returns from the oldest entry; `Some(n)` returns only
///   entries with `sequence > n` (the per-ring cursor).
/// - `root: None` returns every event in the ring; `Some(r)` returns
///   only the events tagged with root `r` — the targeted/guided-walk
///   strategy that touches only the actors in one tree.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.trace.tail")]
pub struct TraceTail {
    pub max: u32,
    pub since: Option<u64>,
    pub root: Option<MailId>,
}

/// Reply to [`TraceTail`]. `Ok::entries` slices the responder's ring
/// matching `(since, root)`, ordered oldest-to-newest (ascending
/// `sequence`). `next_since` is the highest `sequence` in `entries` (or
/// the caller's `since` echoed back on an empty reply) — thread it into
/// the next [`TraceTail::since`] for a stable per-ring cursor.
/// `truncated_before` is set when the ring evicted entries the caller
/// hadn't seen yet (the lowest `sequence` still in the ring), so a
/// reconstructed tree can flag itself known-incomplete rather than fail
/// silently.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.trace.tail_result")]
pub enum TraceTailResult {
    Ok {
        entries: Vec<TraceRingEntry>,
        next_since: u64,
        truncated_before: Option<u64>,
    },
    Err {
        error: String,
    },
}

/// ADR-0080 §6 settlement notification. Post-ADR-0086 Phase 2 it is
/// fired by the emit-time `SettlementCounter` on the chassis
/// `TraceHandle` (not a trace fold) the instant a causal chain's
/// `(in_flight, held_open)` packed counter reaches zero: the producer
/// hook calls `SettlementRegistry::fire_settled(root)` synchronously on
/// the finishing thread. Channel subscribers
/// (`subscribe_settlement`) wake directly; mail subscribers
/// (`subscribe_settlement_mail`) receive a `Settled { root }` mail at
/// their target.
///
/// **Settlement is a hint, not a guarantee.** Per ADR-0080 §6,
/// consumers MUST be idempotent — a duplicate `Settled { root }` for
/// the same root is a no-op for any waiter that already woke (the
/// registry's `settled` set dedups). The gate-site contract is
/// "settles eventually," not "settles only once every dependency is
/// provably done."
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
    pub root: MailId,
}

/// Issue 749: request kind for the atomic batched-dispatch MCP tool
/// `send_mail_traced`. Sent to [`TRACE_MAILBOX_NAME`]; the
/// `TraceDispatchCapability` dispatches every envelope inheriting the
/// inbound chain so all children share a single root with the inbound
/// itself, then replies synchronously with [`DispatchTracedAck`]
/// carrying that root id.
///
/// Carries [`MailEnvelope`]s — the same name-addressed batch shape
/// `CaptureFrame` uses. The substrate-side handler resolves the
/// recipient and kind names against its registry at dispatch time.
///
/// **Two-call protocol.** The synchronous ack closes round 1. The
/// caller waits for the wire `ReplyEnd` (substrate-side chain
/// settlement) and then reconstructs the populated tree by walking the
/// per-actor trace rings from the returned root ([`TraceTail`], stitched
/// client-side — ADR-0086 Phase 3b). This sidesteps the settle/reply
/// race that a single-call shape would inherit from
/// `RpcServerCapability`'s settlement-driven `ReplyEnd`.
#[derive(Clone, Debug, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.trace.dispatch_traced")]
pub struct DispatchTraced {
    pub mails: Vec<MailEnvelope>,
}

/// Issue 749: synchronous reply to [`DispatchTraced`]. `Ok` carries
/// the chassis-root [`MailId`] every dispatched envelope inherited, so
/// the caller can walk the per-actor trace rings from that root once
/// the wire `ReplyEnd` signals chain settlement. `Err` aborts the batch
/// before any mail moved — typically a bad recipient or kind name in
/// the batch (matches `CaptureFrameResult::Err`'s bundle-resolution
/// failure shape).
#[derive(Clone, Debug, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "aether.trace.dispatch_traced_ack")]
pub enum DispatchTracedAck {
    Ok { root: MailId },
    Err { error: String },
}
