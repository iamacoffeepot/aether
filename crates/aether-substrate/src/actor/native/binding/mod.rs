// Wire-encode / test-fixture casts: the `as` narrowings in this module
// (today: the stress-test payload fixtures below) are bounded by
// construction, so the cast lints are blanket-allowed module-wide
// rather than annotated per site.
#![allow(clippy::cast_lossless, clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
// `ReplyTable` Mutex guards are intentionally held across the
// register/lookup/dispatch sequence — early-drop opens a TOCTOU
// window where a sibling thread mutates the pending-reply map between
// the lookup and the dispatch decision.
#![allow(clippy::significant_drop_tightening)]

//! ADR-0074 §Decision (revisited by issue 665): native per-actor
//! binding state.
//!
//! [`NativeBinding`] is a regular struct each capability owns. It
//! holds the per-actor state — mailer + self mailbox + inbox +
//! correlation counter — directly as fields,
//! reached via `&self` on every inherent method. No thread-locals,
//! no install/uninstall ceremony, no `RefCell` runtime borrow checks.
//! The actor binding is type-system-tracked through the
//! `&NativeBinding` references the SDK threads into
//! [`super::ctx::NativeCtx`], [`super::mailbox::NativeActorMailbox`],
//! and the substrate-internal helpers below.
//!
//! Capabilities build their `NativeBinding` at boot and pass
//! `&self.transport` (or thread it through to a worker) wherever a
//! `&NativeBinding` is needed. The wasm guest path rides
//! [`aether_actor::wasm::bridge`] free functions instead — issue 665 retired the cross-target
//! `MailTransport` trait that previously unified them, so each side
//! exposes its own dispatch surface and the per-stage capability
//! traits in `aether_actor::model::ctx` are the only cross-target
//! abstraction.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, OnceLock};

use crate::chassis::inbox::{ReplyLineage, SettlingInbox};
use crate::mail::mailer::Mailer;
use crate::runtime::lifecycle::FatalAborter;
use aether_actor::RequestContextTable;

use self::identity::BindingIdentity;
use self::outbound::OutboundBuffer;
use super::spawn::reservation::{ChildReservationTable, LiveChildReservation};

// The modules below reach `actor::native`'s own items through `super::` paths,
// which resolve against this module — so bind them here.
pub(super) use super::{NativeActor, blob, offload};

mod activation;
mod dispatch;
#[cfg(test)]
mod fixture;
mod flush;
mod identity;
mod lifecycle;
mod outbound;
mod pending;
mod reply;
mod reservation;
mod send;

/// Per-actor binding state every native capability owns. Each
/// capability constructs one at boot via [`NativeBinding::new`] and
/// holds it for the lifetime of its dispatcher thread; SDK helpers
/// receive `&self.transport` references.
///
/// The three inherent dispatch methods read/mutate the struct's
/// fields directly:
///
/// - [`Self::send_mail`] — mints a fresh correlation id (atomic
///   monotonic counter), wraps the bytes in a [`Mail`](crate::mail::Mail) with
///   `SourceAddr::Component(self.self_mailbox)` so any reply
///   routes back here, and pushes through the shared
///   `Arc<Mailer>`.
/// - [`Self::prev_correlation`] — reads the atomic counter.
///
/// Reply (the typed `K` shape) goes through
/// [`Self::send_reply_for_handler`] below; persistence
/// (`save_state`) is wasm-component-only (ADR-0016) and never lands
/// here.
pub struct NativeBinding {
    mailer: Arc<Mailer>,
    /// ADR-0165: exactly one typed production identity or one explicitly
    /// untyped test identity.
    identity: BindingIdentity,
    /// The actor's inbox, drained by the dispatcher via
    /// [`Self::recv_blocking`] / [`Self::try_recv`]. [`SettlingInbox`]'s
    /// drop settles any residue queued at teardown, closing the #1716
    /// leak. Held in a `Mutex` so the `&self` dispatcher can take
    /// exclusive access. Wrapped in `OnceLock` so the inbox can be
    /// installed lazily after construction (capabilities sometimes have to
    /// thread the receiver through a builder before the transport sees it).
    /// `OnceLock::get()` returns `None` until
    /// [`NativeBinding::install_inbox`] runs.
    inbox: OnceLock<Mutex<SettlingInbox>>,
    /// Monotonic correlation counter — atomic so `&self` can mint
    /// new ids without `&mut`.
    correlation: AtomicU64,
    /// ADR-0080 §5 / #1695: reply-id allocator in the disjoint top-half
    /// space (see [`ReplyLineage`]). [`Self::send_reply_for_handler`] mints
    /// from it so a reply's trace id never merges with one of this
    /// actor's own sends, and minting a reply leaves the `send`
    /// correlation [`Self::prev_correlation`] reports untouched —
    /// symmetric with the wasm trampoline's reply-lineage counter.
    /// Cloned into the inbox [`SettlingInbox`] at
    /// [`Self::install_inbox`] so both share one coherent counter.
    reply_lineage: ReplyLineage,
    /// Indirection over [`crate::runtime::lifecycle::fatal_abort`] —
    /// invoked by [`Self::fatal_abort`] when a wasm guest traps so a
    /// faulty component brings the substrate down cleanly. Cloned from
    /// [`ChassisCtx::fatal_aborter`](crate::chassis::ctx::ChassisCtx::fatal_aborter) at boot.
    aborter: Arc<dyn FatalAborter>,
    /// Issue 607 Phase 3b (ADR-0079): the chassis's [`crate::Spawner`]
    /// cloned into every booted actor's transport so per-handler
    /// `NativeCtx::spawn_child` can reach the spawn machinery without
    /// separate plumbing. `None` for [`Self::new_for_test`] transports
    /// (those tests never spawn instances); production constructors
    /// (`new` / `from_ctx`) pass `Some` from the chassis.
    spawner: Option<Arc<crate::Spawner>>,
    /// Issue 607 Phase 4a (ADR-0079): self-shutdown flag. The actor's
    /// dispatcher polls this between handler dispatches; flipping it
    /// (via [`Self::signal_shutdown`] / `NativeCtx::shutdown`) tells
    /// the dispatcher to drain the inbox, run `unwire`, and exit.
    /// Substrate-shutdown (channel disconnect) flows through the same
    /// drain → close → exit path without setting the flag.
    shutdown_flag: Arc<AtomicBool>,
    /// ADR-0087 / 2b (iamacoffeepot/aether#1105): per-actor send-side
    /// blob buffer. The per-handler [`super::ctx::NativeCtx`] /
    /// [`super::mailbox::NativeActorMailbox`] send path buffers into
    /// this (via [`Self::push_envelope_buffered`]); the handler-end
    /// flush ([`Self::flush_outbound`], driven by `NativeCtx`'s `Drop`)
    /// forms one ring blob and routes a
    /// [`MailRef::InRing`](crate::mail::MailRef::InRing) per mail.
    ///
    /// `Mutex` only for the `&self` interior-mutability + `Sync`
    /// requirements — the buffer has a single logical producer (this
    /// actor's dispatcher thread, only during its own handler dispatch),
    /// so the lock is uncontended. Spawned-worker sends
    /// ([`super::offload::thread`]) stay on the eager [`Self::send_mail`] route.
    /// Wasm-guest sends are also eager while Live; staged activation retains
    /// their owned payload here without writing the native ring, preserving
    /// its single-writer discipline.
    outbound: Mutex<OutboundBuffer>,
    /// Lock-free rejection hint for component sends on the ordinary Live hot
    /// path. `false` avoids touching [`Self::outbound`]; `true` enters the
    /// mutex and rechecks its authoritative `activation_held` flag before
    /// retaining mail. Lifecycle transitions update both while holding the
    /// mutex, before guest code can run or the actor can become wakeable.
    activation_held: AtomicBool,
    /// iamacoffeepot/aether#1137: this actor's single active cursor-shared
    /// blob + its recruitment. Built lazily on the first deferred flush
    /// from the spawner's [`WakeSink`](crate::scheduler::WakeSink), so a
    /// test binding with no `Spawner` never builds one and stays on the
    /// eager per-mail route. `Mutex` only for `&self` interior mutability —
    /// driven solely from this actor's dispatch thread, so uncontended.
    blob_producer: Mutex<Option<blob::work::BlobProducer>>,
    /// ADR-0093: the hold-until-resolve in-flight ledger. Maps a
    /// [`DispatchId`](super::offload::blocking::DispatchId) minted by
    /// [`super::ctx::NativeCtx::dispatch_blocking`] to its held
    /// `(SettlementHold, Source, context)` plus the worker's eventual
    /// output. The actor thread writes the entry at dispatch and reads +
    /// removes it when the completion-wake lands; the worker thread fills
    /// the output slot once. `Mutex` only for `&self` interior
    /// mutability — the same single-logical-writer discipline as
    /// `outbound` / `blob_producer`.
    inflight: offload::blocking::InflightLedger,
    /// ADR-0165: parent-local uniqueness reservations for staged and live
    /// children. This table is actor-local bookkeeping only; reserving or
    /// releasing a key never writes a global registry and does not imply a
    /// child-lifetime cascade.
    child_reservations: Mutex<ChildReservationTable>,
    /// Live lease back into this actor's spawning parent's local key table.
    /// The lease is weak and creates no parent/child lifetime cascade; the
    /// actor's own close path hands it back through
    /// [`Self::release_parent_child_reservation`].
    parent_child_reservation: Mutex<Option<LiveChildReservation>>,
    /// ADR-0139: typed request contexts keyed by reply correlation id.
    request_contexts: Mutex<RequestContextTable>,
}
