// Wire-encode / test-fixture casts: the `as` narrowings in this module
// (today: the stress-test payload fixtures below) are bounded by
// construction, so the cast lints are blanket-allowed module-wide
// rather than annotated per site.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]
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
//! `&NativeBinding` is needed. The FFI guest path rides
//! [`aether_actor::ffi::bridge`] static ZSTs (`MAIL`, `PERSIST`,
//! `SYNC_WAIT`) instead — issue 665 retired the cross-target
//! `MailTransport` trait that previously unified them, so each side
//! exposes its own dispatch surface and the per-stage capability
//! traits in `aether_actor::actor::ctx` are the only cross-target
//! abstraction.

use std::any::Any;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, OnceLock};

use aether_kinds::trace::Nanos;

use crate::actor::native::envelope::Envelope;
use crate::chassis::ctx::ChassisCtx;
use crate::mail::mailer::Mailer;
use crate::mail::ring::{MailLoc, MailRing, RingFull};
use crate::mail::{KindId, Mail, MailId, MailRef, MailboxId, ReplyTarget, ReplyTo};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};
use crate::runtime::trace::SettlementHold;

/// Per-actor outbound ring capacity (ADR-0087). Sized to hold a typical
/// handler's small-mail fan-out as one blob; a mail that doesn't fit (a
/// large payload, or a very wide fan-out that fills the ring) degrades to
/// the [`MailRef::Owned`] copy-out valve in
/// [`NativeBinding::flush_outbound`] / `push_envelope_buffered` rather
/// than blocking — the large-payload zero-copy path is the deferred fork
/// on iamacoffeepot/aether#1101.
const ACTOR_RING_BYTES: usize = 64 * 1024;

/// Where a buffered mail's payload lives until flush (2c,
/// iamacoffeepot/aether#1110).
enum PendingPayload {
    /// Written into the actor's ring in place at send time; carries the
    /// location to mint a [`MailRef::InRing`] from at flush.
    InRing(MailLoc),
    /// The copy-out fallback when the ring could not take the mail
    /// (transiently full, or a payload larger than the ring).
    Owned(Vec<u8>),
}

/// One outbound mail a handler buffered, pending flush. The payload is
/// already in the ring (`InRing`) or copied out (`Owned`); the rest is
/// route metadata (correlation-derived `reply_to`/`mail_id`, inherited
/// lineage) the flush stamps onto the [`Mail`] it builds.
struct PendingMail {
    recipient: u64,
    kind: u64,
    payload: PendingPayload,
    count: u32,
    reply_to: ReplyTo,
    mail_id: MailId,
    root: MailId,
    parent_mail: Option<MailId>,
}

/// Per-actor send-side buffer that builds blobs **in place** (2c,
/// iamacoffeepot/aether#1110). `push_envelope_buffered` writes each send
/// straight into the ring as it happens — the blob is opened lazily on
/// the first send of a flush window — and records only route metadata
/// here. `flush_outbound` seals the blob and routes. There is no payload
/// staging buffer: the bytes land in the ring exactly once (the only
/// copy is out of the caller's slice, which is unavoidable since it is
/// not stable past the call).
///
/// `mails` is **reused** across windows (cleared, not freed). `ring` is
/// lazily created on the first buffered send, so actors that never buffer
/// (wasm trampolines, inline-only caps) pay no ring allocation.
struct OutboundBuffer {
    /// Lazily created on the first buffered send. `Arc` so each minted
    /// [`MailRef::InRing`] carries the ring's lifetime by refcount.
    ring: Option<Arc<MailRing>>,
    /// Whether a ring blob is currently open — between the first send of
    /// a flush window and the flush's `seal`.
    blob_open: bool,
    /// iamacoffeepot/aether#1158: the instant this flush window's blob
    /// **opened** — stamped at the first buffered send (the `blob_open`
    /// false→true transition), shared by every mail in the window. The
    /// flush reads it as each deferred `Sent`'s `t_construct_start`
    /// (falling back to `flush_begin` if somehow unset) so `t_sent −
    /// t_construct_start` is the **construct** span, and resets it to
    /// `None` after draining so the next window re-stamps.
    construct_start: Option<Nanos>,
    /// Per-mail route metadata for the current flush window.
    mails: Vec<PendingMail>,
}

impl OutboundBuffer {
    fn new() -> Self {
        Self {
            ring: None,
            blob_open: false,
            construct_start: None,
            mails: Vec::new(),
        }
    }
}

/// Per-actor binding state every native capability owns. Each
/// capability constructs one at boot via [`NativeBinding::new`] and
/// holds it for the lifetime of its dispatcher thread; SDK helpers
/// receive `&self.transport` references.
///
/// The three inherent dispatch methods read/mutate the struct's
/// fields directly:
///
/// - [`Self::send_mail`] — mints a fresh correlation id (atomic
///   monotonic counter), wraps the bytes in a [`Mail`] with
///   `ReplyTarget::Component(self.self_mailbox)` so any reply
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
    self_mailbox: MailboxId,
    /// ADR-0099 §3: this actor's rolling lineage carry — the running
    /// FNV-1a fold state over its lineage of `ActorId`s, root to leaf.
    /// `with_tag(Mailbox, carry) == self_mailbox`. `spawn_child` folds a
    /// child's `ActorId` onto this carry to derive the child's id, so an
    /// actor passes its whole lineage forward as one `u64`. A root cap
    /// is the depth-1 fixed point: its carry is its own `ActorId.0`
    /// (== `self_mailbox.0`), so it keeps the exact id it has today.
    carry: u64,
    /// The actor's inbox receiver, drained by the dispatcher via
    /// [`Self::recv_blocking`] / [`Self::try_recv`]. Held in a `Mutex`
    /// so the `&self` receiver can take exclusive access. Wrapped in
    /// `OnceLock` so the inbox can be installed lazily after
    /// construction (capabilities sometimes have to thread the receiver
    /// through a builder before the transport sees it). `OnceLock::get()`
    /// returns `None` until [`NativeBinding::install_inbox`] runs.
    inbox: OnceLock<Mutex<Receiver<Envelope>>>,
    /// Monotonic correlation counter — atomic so `&self` can mint
    /// new ids without `&mut`.
    correlation: AtomicU64,
    /// Indirection over [`crate::runtime::lifecycle::fatal_abort`] —
    /// invoked by [`Self::fatal_abort`] when a wasm guest traps so a
    /// faulty component brings the substrate down cleanly. Cloned from
    /// [`ChassisCtx::fatal_aborter`] at boot.
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
    /// [`MailRef::InRing`] per mail.
    ///
    /// `Mutex` only for the `&self` interior-mutability + `Sync`
    /// requirements — the buffer has a single logical producer (this
    /// actor's dispatcher thread, only during its own handler dispatch),
    /// so the lock is uncontended. Spawned-worker sends
    /// ([`super::spawn_thread`]) and wasm-guest sends run on other
    /// threads / a different path and stay on the eager [`Self::send_mail`]
    /// route, preserving the ring's single-writer discipline.
    outbound: Mutex<OutboundBuffer>,
    /// iamacoffeepot/aether#1137: this actor's single active cursor-shared
    /// blob + its recruitment. Built lazily on the first deferred flush
    /// from the spawner's [`WakeSink`](crate::scheduler::WakeSink), so a
    /// test binding with no `Spawner` never builds one and stays on the
    /// eager per-mail route. `Mutex` only for `&self` interior mutability —
    /// driven solely from this actor's dispatch thread, so uncontended.
    blob_producer: Mutex<Option<super::blob_work::BlobProducer>>,
    /// ADR-0093: the hold-until-resolve in-flight ledger. Maps a
    /// [`DispatchId`](super::dispatch_blocking::DispatchId) minted by
    /// [`super::ctx::NativeCtx::dispatch_blocking`] to its held
    /// `(SettlementHold, ReplyTo, context)` plus the worker's eventual
    /// output. The actor thread writes the entry at dispatch and reads +
    /// removes it when the completion-wake lands; the worker thread fills
    /// the output slot once. `Mutex` only for `&self` interior
    /// mutability — the same single-logical-writer discipline as
    /// `outbound` / `blob_producer`.
    inflight: super::dispatch_blocking::InflightLedger,
}

impl NativeBinding {
    /// Build a fresh transport. Pair `self_mailbox` with the id the
    /// `MailboxClaim` returned (the substrate routes replies back
    /// to it via the `ReplyTarget::Component(self_mailbox)` tag the
    /// transport stamps onto outbound mail). The inbox is installed
    /// separately via [`Self::install_inbox`] so capabilities that
    /// build the transport before pulling the receiver out of their
    /// claim aren't forced into a specific construction order.
    ///
    /// `aborter` backs [`Self::fatal_abort`] (wasm trap → clean
    /// substrate exit). Capabilities authored under a [`ChassisCtx`]
    /// should prefer [`Self::from_ctx`], which inherits the chassis's
    /// aborter + spawner automatically; the explicit constructor is
    /// for harnesses that don't go through a chassis (`TestBench`
    /// internals) or for tests that want to substitute a custom
    /// aborter.
    pub fn new(
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        carry: u64,
        aborter: Arc<dyn FatalAborter>,
        spawner: Option<Arc<crate::Spawner>>,
    ) -> Self {
        Self {
            mailer,
            self_mailbox,
            carry,
            inbox: OnceLock::new(),
            correlation: AtomicU64::new(0),
            aborter,
            spawner,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            outbound: Mutex::new(OutboundBuffer::new()),
            blob_producer: Mutex::new(None),
            inflight: Mutex::new(super::dispatch_blocking::InflightTable::new()),
        }
    }

    /// Convenience constructor that pulls the aborter + spawner from a
    /// [`ChassisCtx`]. The natural call site is inside a
    /// [`crate::DriverCapability::boot`] body:
    ///
    /// ```ignore
    /// let claim = ctx.claim_mailbox_drop_on_shutdown(NAME)?;
    /// let transport = NativeBinding::from_ctx(ctx, claim.id);
    /// ```
    #[must_use]
    pub fn from_ctx(ctx: &ChassisCtx<'_>, self_mailbox: MailboxId) -> Self {
        Self::new(
            ctx.mail_send_handle(),
            self_mailbox,
            // A cap built under a `ChassisCtx` is a root-pinned chassis
            // capability (depth-1), so its lineage carry is its own
            // `ActorId.0` == `self_mailbox.0` — it keeps today's id.
            self_mailbox.0,
            ctx.fatal_aborter(),
            Some(Arc::clone(ctx.spawner_arc())),
        )
    }

    /// Test-only constructor with a [`PanicAborter`] and no spawner.
    /// Lets unit tests build a transport without a chassis; not
    /// appropriate for production capabilities, which should go
    /// through [`Self::from_ctx`].
    pub fn new_for_test(mailer: Arc<Mailer>, self_mailbox: MailboxId) -> Self {
        // Test bindings never spawn children; seed the carry at the
        // depth-1 fixed point so `self_mailbox` and the carry agree.
        Self::new(
            mailer,
            self_mailbox,
            self_mailbox.0,
            Arc::new(PanicAborter),
            None,
        )
    }

    /// Install the receiver half of the actor's inbox so the
    /// dispatcher's [`Self::recv_blocking`] / [`Self::try_recv`] have
    /// somewhere to pull from. Called once per transport, before the
    /// dispatcher starts draining. Subsequent calls panic — the slot
    /// is single-claim by construction.
    ///
    /// # Panics
    /// Panics if called more than once — fail-fast per ADR-0063: the
    /// inbox slot is single-claim, so a second install indicates a
    /// chassis-wiring bug.
    pub fn install_inbox(&self, inbox: Receiver<Envelope>) {
        self.inbox
            .set(Mutex::new(inbox))
            .unwrap_or_else(|_| panic!("NativeBinding::install_inbox called twice"));
    }

    /// The mailbox id the substrate routes inbound mail through to
    /// reach this actor. Exposed for capabilities that need to
    /// publish their address to peers without going through the
    /// transport's send path.
    pub fn self_mailbox(&self) -> MailboxId {
        self.self_mailbox
    }

    /// This actor's lineage carry (ADR-0099 §3) — the rolling fold
    /// state `spawn_child` extends to derive a child's id. Surfaced so
    /// [`super::ctx::NativeCtx::spawn_child`] can pass it as the parent
    /// carry the spawn machinery folds the new node's `ActorId` onto.
    pub fn carry(&self) -> u64 {
        self.carry
    }

    /// Borrow the wired `Mailer`. Surfaced so cross-file producer
    /// hooks (`dispatch`, `dispatcher_slot`, `spawn_thread`) can
    /// reach the trace handle via `binding.mailer().record_*(...)`
    /// without the field having to be `pub(crate)`. Filed under
    /// iamacoffeepot/aether#953 (per-chassis trace state).
    pub fn mailer(&self) -> &Arc<Mailer> {
        &self.mailer
    }

    /// The chassis's [`crate::Spawner`], if one was wired in at
    /// construction. `Some` for production transports built through
    /// [`Self::from_ctx`] (the chassis builds + threads its `Spawner`
    /// into every cap); `None` for [`Self::new_for_test`] transports
    /// (those tests don't exercise spawn). Used by
    /// `NativeCtx::spawn_child` to reach the spawn machinery without
    /// separate per-handler plumbing.
    pub fn spawner(&self) -> Option<&Arc<crate::Spawner>> {
        self.spawner.as_ref()
    }

    /// Issue 607 Phase 4a (ADR-0079): set the self-shutdown flag the
    /// actor's dispatcher polls between handler dispatches. Subsequent
    /// `recv_blocking` calls still process incoming mail, but
    /// `should_shutdown` reports `true` so the trampoline can drain
    /// the inbox synchronously, run `unwire`, and exit. Idempotent.
    pub fn signal_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Release);
    }

    /// ADR-0063 fail-fast: bring the substrate down with `reason`.
    /// Diverging — does not return. Production substrates exit via
    /// [`crate::runtime::lifecycle::fatal_abort`] (broadcasts `SubstrateDying`
    /// then calls `process::exit(2)`); test substrates panic instead.
    /// The trampoline calls this when the wasm guest traps, so a
    /// faulty component takes down the substrate cleanly with a useful
    /// log message rather than leaving a tombstoned trampoline whose
    /// failure mode is invisible to callers.
    pub fn fatal_abort(&self, reason: String) -> ! {
        self.aborter.abort(reason);
    }

    /// Read the self-shutdown flag. Polled by the dispatcher trampoline
    /// after each handler dispatch — substrate-shutdown
    /// (channel-disconnect) flows through the same drain path without
    /// setting this flag, so the trampoline takes either signal as a
    /// trigger to wind down.
    pub fn should_shutdown(&self) -> bool {
        self.shutdown_flag.load(Ordering::Acquire)
    }

    /// Block until the next envelope arrives on this actor's inbox.
    /// Returns `None` when the channel disconnects (the channel-drop
    /// shutdown signal — capability's `RunningCapability::shutdown`
    /// dropped its [`crate::chassis::ctx::MailboxSender`], the registry
    /// handler can no longer upgrade its [`std::sync::Weak`], the
    /// inbox's last sender is gone) or when no inbox is installed.
    ///
    /// The natural shape for a dispatcher loop:
    ///
    /// ```ignore
    /// while let Some(env) = transport.recv_blocking() {
    ///     handle_envelope(env);
    /// }
    /// ```
    ///
    /// # Panics
    /// Panics if the inbox mutex is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked inside the
    /// guard, which is itself a substrate-level invariant violation.
    pub fn recv_blocking(&self) -> Option<Envelope> {
        let inbox = self.inbox.get()?;
        // The mutex guard stays held across `recv()`. Dispatcher
        // threads are single-tasked while parked here; nothing else
        // on this thread contends.
        inbox
            .lock()
            .expect("inbox mutex poisoned; fail-fast per ADR-0063")
            .recv()
            .ok()
    }

    /// Non-blocking variant of [`Self::recv_blocking`]. Returns
    /// `None` for "no envelope available right now" or "channel
    /// disconnected" or "no inbox installed". A capability that
    /// needs to distinguish drains via repeated calls until `None`.
    ///
    /// # Panics
    /// Panics if the inbox mutex is poisoned — fail-fast per ADR-0063:
    /// a poisoned mutex means a prior holder panicked inside the
    /// guard, which is itself a substrate-level invariant violation.
    pub fn try_recv(&self) -> Option<Envelope> {
        let inbox = self.inbox.get()?;
        inbox
            .lock()
            .expect("inbox mutex poisoned; fail-fast per ADR-0063")
            .try_recv()
            .ok()
    }

    /// Reply path for native actors. Routes through the substrate's
    /// [`Mailer::send_reply`] so a handler's `ctx.reply(&result)`
    /// reaches the originator the same way a pre-actor-model cap's
    /// `self.mailer.send_reply(sender, &result)` did. Issue 665
    /// retired the FFI-shaped `reply_mail` stub the prior
    /// `MailTransport` impl carried — it took `sender: u32`, a wasm
    /// handle shape that doesn't fit native's [`ReplyTo`]. This typed
    /// entry is the only reply API native actors reach for.
    pub fn send_reply_for_handler<K>(&self, sender: ReplyTo, payload: &K)
    where
        K: aether_data::Kind + serde::Serialize,
    {
        self.mailer.send_reply(sender, payload);
    }
}

/// Inherent send / `prev_correlation` entry points the
/// per-handler [`super::ctx::NativeCtx`] / [`super::ctx::NativeInitCtx`]
/// route through. Issue 665 retired the prior `MailTransport` trait
/// impl; the FFI-shaped wrapper served no purpose for native (Mailer
/// dispatch is direct), and `save_state` / `reply_mail` were stubs the
/// trait forced on us. The capability traits in
/// [`aether_actor::actor::ctx`] are the only cross-target trait surface
/// post-665.
impl NativeBinding {
    /// Push a typed payload at `recipient`. Mints a fresh correlation
    /// id (atomic monotonic counter), wraps the bytes in a [`Mail`]
    /// with `ReplyTarget::Component(self.self_mailbox)` so any reply
    /// routes back here, and pushes through the shared
    /// `Arc<Mailer>`. Returns `0` (channel-send failures collapse to
    /// the same scalar — there is no FFI surface here to differentiate).
    ///
    /// Stamps `MailId`/`root`/`parent_mail` as a chassis-root send
    /// (no inheritance). Per-handler ctxs that have an in-flight mail
    /// to inherit from go through [`Self::send_mail_with_lineage`]
    /// instead — the four-arg shape preserves wire stability for the
    /// FFI bridge and chassis-side log push paths that do not carry
    /// a per-handler context.
    pub fn send_mail(&self, recipient: u64, kind: u64, bytes: &[u8], count: u32) -> u32 {
        self.send_mail_with_lineage(recipient, kind, bytes, count, None, None)
    }

    /// ADR-0080 §1 / §5: variant of [`Self::send_mail`] that accepts
    /// the in-flight handler's lineage so the outgoing [`Mail`] picks
    /// up the correct `parent_mail` and inherited `root`. The
    /// per-handler [`super::ctx::NativeCtx`]'s
    /// [`aether_actor::actor::sender::Sender`] impl reads from its
    /// `in_flight_mail_id()` / `in_flight_root()` accessors and threads
    /// them in.
    ///
    /// `parent_mail = None` and `inherited_root = None` mean
    /// chassis-root: the outgoing mail's `MailId` becomes its own
    /// `root`, marking the start of a new causal chain.
    pub fn send_mail_with_lineage(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
    ) -> u32 {
        let _ = self.push_envelope_returning_root(
            recipient,
            kind,
            bytes,
            count,
            parent_mail,
            inherited_root,
        );
        0
    }

    /// Like [`Self::send_mail_with_lineage`] but returns the minted
    /// `MailId` (== the new root when `inherited_root.is_none()`) so the
    /// caller can subscribe to its settlement via the chassis
    /// [`crate::chassis::settlement::SettlementRegistry`].
    ///
    /// Same semantics as the `u32`-returning variant; the success-path
    /// `0` was vestigial at this layer (channel-send failures collapse to
    /// the same scalar).
    ///
    /// # Panics
    /// Panics if the `pending_recipients` mutex is poisoned — fail-fast
    /// per ADR-0063: a poisoned mutex means a prior holder panicked
    /// inside the guard, which is itself a substrate-level invariant
    /// violation.
    pub fn push_envelope_returning_root(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
    ) -> MailId {
        let correlation = self.correlation.fetch_add(1, Ordering::AcqRel) + 1;
        let recipient_id = MailboxId(recipient);
        let reply_to =
            ReplyTo::with_correlation(ReplyTarget::Component(self.self_mailbox), correlation);
        let mail_id = MailId::new(self.self_mailbox, correlation);
        let root = inherited_root.unwrap_or(mail_id);
        // ADR-0080 §2 producer hook: emit `Sent` before pushing the
        // mail. Every `Mailer` carries a trace handle by default
        // (per-chassis post iamacoffeepot/aether#953), so producer
        // calls are unconditional; the drainer is the optional piece.
        self.mailer.record_sent(
            mail_id,
            root,
            parent_mail,
            self.self_mailbox,
            recipient_id,
            KindId(kind),
        );
        let mail = Mail::new(recipient_id, KindId(kind), bytes.to_vec(), count)
            .with_reply_to(reply_to)
            .with_lineage(mail_id, root, parent_mail);
        self.mailer.push(mail);
        mail_id
    }

    /// ADR-0087 / 2b: the buffering counterpart to
    /// [`Self::push_envelope_returning_root`], used by the per-handler
    /// send surface ([`super::ctx::NativeCtx`] /
    /// [`super::mailbox::NativeActorMailbox`]). Rather than allocating an
    /// owned `Vec` and routing immediately, it copies the bytes into the
    /// reused per-actor scratch arena and records the route
    /// metadata; [`Self::flush_outbound`] forms the blob and routes at
    /// handler end.
    ///
    /// The settlement-counter increment stays **eager** (fired here, at
    /// send time, not at flush) so the chain's `in_flight` is exact and
    /// settlement (ADR-0082) never settles early. The `Sent` *trace*
    /// event, by contrast, is deferred to [`Self::flush_outbound`] and
    /// stamped with the frame-level flush-begin instant
    /// (iamacoffeepot/aether#1150) — anchoring it there instead of this
    /// smeared per-send call site, which otherwise absorbs the rest of
    /// the handler that ran after the send. Returns the minted `MailId`
    /// (== the new root when `inherited_root.is_none()`) exactly like the
    /// eager variant, so settlement subscription works unchanged.
    ///
    /// # Panics
    /// Panics if the outbound-buffer mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub fn push_envelope_buffered(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
    ) -> MailId {
        self.push_envelope_buffered_with_reply_to(
            recipient,
            kind,
            bytes,
            count,
            parent_mail,
            inherited_root,
            None,
        )
    }

    /// Re-dispatcher variant of [`Self::push_envelope_buffered`] that
    /// accepts an explicit `reply_to` instead of stamping the default
    /// `ReplyTo::with_correlation(ReplyTarget::Component(self_mailbox),
    /// auto_correlation)`. The minted [`MailId`] and the `in_flight`
    /// settlement increment are unaffected — they still use this
    /// actor's correlation counter — only the recipient's
    /// `OutboundReply::reply_target()` view changes.
    ///
    /// Used by re-dispatch caps (today: `TraceDispatchCapability`
    /// servicing `DispatchTraced`) that forward someone else's call:
    /// the children's deferred replies must bubble up to the original
    /// caller (e.g. the RPC server holding the wire `cid`'s in-flight
    /// entry), not get stranded at the re-dispatcher's mailbox where
    /// no handler exists for them.
    ///
    /// `reply_to_override = None` is the same shape as
    /// [`Self::push_envelope_buffered`].
    ///
    /// # Panics
    /// Panics if the outbound-buffer mutex is poisoned — fail-fast per
    /// ADR-0063.
    #[allow(
        clippy::too_many_arguments,
        reason = "re-dispatch variant adds reply_to_override to the existing 6-arg shape; \
                  splitting would force callers through two separate code paths"
    )]
    pub fn push_envelope_buffered_with_reply_to(
        &self,
        recipient: u64,
        kind: u64,
        bytes: &[u8],
        count: u32,
        parent_mail: Option<MailId>,
        inherited_root: Option<MailId>,
        reply_to_override: Option<ReplyTo>,
    ) -> MailId {
        let correlation = self.correlation.fetch_add(1, Ordering::AcqRel) + 1;
        let reply_to = reply_to_override.unwrap_or_else(|| {
            ReplyTo::with_correlation(ReplyTarget::Component(self.self_mailbox), correlation)
        });
        let mail_id = MailId::new(self.self_mailbox, correlation);
        let root = inherited_root.unwrap_or(mail_id);
        // iamacoffeepot/aether#1150: only the settlement increment is
        // eager here; the `Sent` trace event emits at flush against the
        // flush-begin anchor (see `flush_outbound_inner`). The recipient
        // id, kind, and lineage ride the `PendingMail` to flush, where the
        // deferred `Sent` is built from the routed `Mail`.
        self.mailer.record_sent_inflight(root);
        let mut buf = self
            .outbound
            .lock()
            .expect("outbound buffer poisoned; fail-fast per ADR-0063");
        // Write the payload into the ring in place. Open the blob lazily
        // on the first send of this flush window; on `RingFull` (full ring
        // or oversized payload) copy out to `Owned` — the never-block
        // valve. The open blob is left intact on `RingFull`, so a later
        // send (after a consumer frees space) can still extend it.
        let payload = {
            let OutboundBuffer {
                ring,
                blob_open,
                construct_start,
                ..
            } = &mut *buf;
            let ring =
                ring.get_or_insert_with(|| Arc::new(MailRing::with_capacity(ACTOR_RING_BYTES)));
            if !*blob_open {
                ring.open_blob();
                *blob_open = true;
                // iamacoffeepot/aether#1158: the blob just opened — stamp
                // the construct-start instant shared by every mail in this
                // flush window. `t_sent − t_construct_start` (flush-begin −
                // this) is the **construct** span (the producer building
                // the blob).
                *construct_start = Some(self.mailer.now_nanos());
            }
            match ring.append(recipient, kind, bytes) {
                Ok(loc) => PendingPayload::InRing(loc),
                Err(RingFull) => PendingPayload::Owned(bytes.to_vec()),
            }
        };
        buf.mails.push(PendingMail {
            recipient,
            kind,
            payload,
            count,
            reply_to,
            mail_id,
            root,
            parent_mail,
        });
        mail_id
    }

    /// ADR-0087 / 2c: seal the open ring blob and route the buffered
    /// mail. Called at handler end (via [`super::ctx::NativeCtx`]'s
    /// `Drop`). A no-op when nothing is buffered.
    ///
    /// The payloads are already in the ring (written by
    /// `push_envelope_buffered` as each send happened) or copied out to
    /// `Owned`; this just [`seal`](MailRing::seal)s the blob — publishing
    /// each in-ring mail's lock — and mints one [`MailRef`] per pending
    /// entry: [`MailRef::InRing`] for ring-resident payloads (the
    /// recipient reads them in place), [`MailRef::Owned`] for the
    /// copy-out fallback. The route metadata is identical for both, so
    /// the dispatch read path is unchanged.
    ///
    /// The buffer lock is released **before** routing: `Mailer::push` can
    /// run an inline handler synchronously, and holding the lock across
    /// arbitrary handler code would be a needless contention/re-entrancy
    /// hazard. A single drain suffices — the buffer is written only by
    /// this actor's per-handler send path, never re-entrantly during
    /// routing (inline handlers receive a `MailDispatch`, not a
    /// buffering `NativeCtx`).
    ///
    /// ADR-0087 Phase 3b: when a pool [`WakeSink`](crate::scheduler::WakeSink)
    /// is wired (every production binding — derived from the chassis
    /// `Spawner`), the whole blob is pushed as **one** `BlobWork` work
    /// item rather than routed per mail, so a fan-out of N costs one
    /// deque push + an inline demux instead of N pushes + up to N
    /// parked-worker wakeups.
    /// A binding with no `Spawner` (test transports built via
    /// [`Self::new_for_test`]) keeps the eager per-mail route.
    ///
    /// # Panics
    /// Panics if the outbound-buffer mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub fn flush_outbound(&self) {
        self.flush_outbound_inner();
    }

    /// Seal the open blob, mint a [`MailRef`] per buffered mail, and
    /// route. Folds the blob into this actor's cursor-shared
    /// [`BlobWork`] when a pool [`Spawner`](crate::Spawner) is wired,
    /// else routes per mail (test bindings without a spawner); a no-op
    /// when nothing is buffered.
    ///
    /// iamacoffeepot/aether#1150: this is the frame's flush-begin
    /// instant. Once the buffer is known non-empty, one `now_nanos` read
    /// stamps `flush_begin`, and every mail in the frame emits its
    /// deferred `Sent` trace event against that shared anchor (the
    /// per-send call site only bumped `in_flight`). Anchoring `Sent`
    /// here, not at the send call, drops the smear of "the rest of the
    /// handler that ran after the send" from the producer-side span. The
    /// clock read sits behind the emptiness check so a no-send handler
    /// return stays free.
    fn flush_outbound_inner(&self) {
        let flush_begin;
        // iamacoffeepot/aether#1158: the construct-start anchor stamped
        // when this window's blob opened (`push_envelope_buffered`). Read
        // it here and reset for the next window; fall back to `flush_begin`
        // (construct ≈ 0) on the impossible `None` so the field is never
        // a wire hole.
        let construct_start;
        let routed: Vec<Mail> = {
            let mut buf = self
                .outbound
                .lock()
                .expect("outbound buffer poisoned; fail-fast per ADR-0063");
            // Seal the open blob first (publishes the in-ring locks), so a
            // `MailRef::InRing` minted below reads a finalized header.
            if buf.blob_open {
                if let Some(ring) = buf.ring.as_ref() {
                    ring.seal();
                }
                buf.blob_open = false;
            }
            if buf.mails.is_empty() {
                // Reset the stale anchor so the next window re-stamps.
                buf.construct_start = None;
                return;
            }
            flush_begin = self.mailer.now_nanos();
            // Take the anchor and reset so the next blob re-stamps.
            construct_start = buf.construct_start.take().unwrap_or(flush_begin);
            let OutboundBuffer { ring, mails, .. } = &mut *buf;
            let ring = ring.as_ref();
            mails
                .drain(..)
                .map(|p| {
                    let payload = match p.payload {
                        PendingPayload::InRing(loc) => MailRef::in_ring(
                            Arc::clone(ring.expect("ring exists once an InRing mail was minted")),
                            loc,
                        ),
                        PendingPayload::Owned(bytes) => MailRef::from(bytes),
                    };
                    Mail::new(MailboxId(p.recipient), KindId(p.kind), payload, p.count)
                        .with_reply_to(p.reply_to)
                        .with_lineage(p.mail_id, p.root, p.parent_mail)
                })
                .collect()
        };

        // iamacoffeepot/aether#1150: emit each buffered mail's deferred
        // `Sent` trace event against the shared flush-begin anchor before
        // routing (the lock is already released — `push_trace_ring` runs
        // off the actor's own ring). `in_flight` was bumped eagerly at
        // the send call, so this is purely the trace-event half.
        for mail in &routed {
            self.mailer.record_sent_event_at(
                mail.mail_id,
                mail.root,
                mail.parent_mail,
                self.self_mailbox,
                mail.recipient,
                mail.kind,
                construct_start,
                flush_begin,
            );
        }

        // ADR-0087 / iamacoffeepot/aether#1137: fold the blob into this
        // actor's single active cursor-shared blob (recipient-grouped,
        // cooperatively drained, broadcast-recruited for wide fan-outs)
        // when a pool sink is wired. Otherwise route per mail (a test
        // binding with no `Spawner`).
        if self.spawner.is_some() {
            let mut guard = self
                .blob_producer
                .lock()
                .expect("blob_producer poisoned; fail-fast per ADR-0063");
            let producer = guard.get_or_insert_with(|| {
                let sink = self
                    .spawner
                    .as_ref()
                    .expect("spawner present in this branch")
                    .wake_sink()
                    .clone();
                super::blob_work::BlobProducer::new(Arc::clone(&self.mailer), sink)
            });
            producer.flush(routed);
        } else {
            for mail in routed {
                self.mailer.push(mail);
            }
        }
    }

    /// Correlation id the substrate minted for this actor's most
    /// recent `send_mail` (ADR-0042). `0` before any send. Universal
    /// — every send mints a correlation; a handler stashes it and
    /// matches it against the inbound reply's correlation to pair a
    /// reply with the request it sent.
    pub fn prev_correlation(&self) -> u64 {
        self.correlation.load(Ordering::Acquire)
    }
}

/// ADR-0093 hold-until-resolve dispatch: the `&self`-interior-mutability
/// bridge between [`super::ctx::NativeCtx`]'s dispatch primitive and the
/// per-actor [`super::dispatch_blocking::InflightTable`]. Each method
/// takes the table lock for one operation — mint+insert at dispatch,
/// fill-output from the worker, take at completion — matching the
/// `outbound` / `blob_producer` locking pattern (uncontended, single
/// logical writer).
impl NativeBinding {
    /// Insert a freshly-minted in-flight dispatch entry and return its
    /// [`DispatchId`](super::dispatch_blocking::DispatchId). Called on
    /// the actor thread at dispatch time, after the hold is acquired and
    /// before the worker spawns.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_insert(
        &self,
        hold: SettlementHold,
        reply_to: ReplyTo,
        context: Box<dyn Any + Send>,
    ) -> super::dispatch_blocking::DispatchId {
        self.inflight
            .lock()
            .expect("in-flight ledger poisoned; fail-fast per ADR-0063")
            .dispatch_insert(hold, reply_to, context)
    }

    /// Fill the worker's output into the named dispatch's completion
    /// slot. Called once, on the worker thread, before it pushes the
    /// [`TaskCompletionWake`](super::dispatch_blocking::TaskCompletionWake).
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_fill_output(
        &self,
        id: super::dispatch_blocking::DispatchId,
        output: Box<dyn Any + Send>,
    ) {
        self.inflight
            .lock()
            .expect("in-flight ledger poisoned; fail-fast per ADR-0063")
            .dispatch_fill_output(id, output);
    }

    /// Remove the named dispatch entry and rebuild its
    /// [`TaskDone`](super::dispatch_blocking::TaskDone). Called on the
    /// actor thread when the completion-wake mail lands.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_take<O: 'static, C: 'static>(
        &self,
        id: super::dispatch_blocking::DispatchId,
    ) -> Option<super::dispatch_blocking::TaskDone<O, C>> {
        self.inflight
            .lock()
            .expect("in-flight ledger poisoned; fail-fast per ADR-0063")
            .dispatch_take(id)
    }

    /// Non-consuming peek-then-take of the named dispatch entry: probe its
    /// boxed output + context against `O` / `C` and only remove + rebuild
    /// the [`TaskDone`](super::dispatch_blocking::TaskDone) on a match,
    /// leaving the entry intact on a mismatch. The `#[handler(task)]`
    /// dispatch chain calls this to route a completion to the right
    /// output-typed handler without a wrong-type probe consuming the entry.
    ///
    /// # Panics
    /// Panics if the in-flight ledger mutex is poisoned — fail-fast per
    /// ADR-0063.
    pub(crate) fn dispatch_try_take<O: 'static, C: 'static>(
        &self,
        id: super::dispatch_blocking::DispatchId,
    ) -> Option<super::dispatch_blocking::TaskDone<O, C>> {
        self.inflight
            .lock()
            .expect("in-flight ledger poisoned; fail-fast per ADR-0063")
            .dispatch_try_take(id)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction panic on failure is the assertion"
)]
#[allow(clippy::disallowed_methods)] // test scaffolding — threads here hold no settlement contract
mod tests {
    use super::*;
    use crate::mail::registry::{InboxHandler, OwnedDispatch};
    use crate::test_util::fresh_substrate;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Build a registry handler that forwards every [`MailDispatch`]
    /// it receives onto `tx` as an owned [`Envelope`]. Used by tests
    /// that need a registered recipient but only care about
    /// observing — or just not warn-dropping — the mail.
    fn forward_to_envelope_sender(tx: mpsc::Sender<Envelope>) -> Arc<dyn InboxHandler> {
        // iamacoffeepot/aether#848: the helper takes
        // [`OwnedDispatch`] directly so payload + kind_name move
        // into the forwarded [`Envelope`] without `to_vec()` /
        // `to_owned()` clones.
        Arc::new(move |dispatch: OwnedDispatch| {
            // ADR-0094: this test sink is the terminal consumer (there is
            // no real downstream dispatcher to discharge), so discharge
            // the obligation here before forwarding the value for the
            // test to observe — otherwise the observing `drop(env)` would
            // trip the debug guard.
            dispatch.discharge();
            // `Envelope` is now a type alias for `OwnedDispatch`, so
            // the inbox-handler value moves straight onto the actor
            // mpsc with no field-by-field translation.
            let _ = tx.send(dispatch);
        })
    }

    /// `prev_correlation` returns 0 before any send and tracks the
    /// monotonic counter as `send_mail` mints new ids.
    #[test]
    fn prev_correlation_tracks_send_mail_minting() {
        let (registry, mailer) = fresh_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        // Register a sink so push routes somewhere instead of
        // hitting the unknown-recipient warn.
        registry.register_inbox("test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();

        let transport = NativeBinding::new_for_test(mailer, MailboxId(99));

        assert_eq!(transport.prev_correlation(), 0);
        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        assert_eq!(transport.prev_correlation(), 1);
        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        assert_eq!(transport.prev_correlation(), 2);
    }

    /// ADR-0099 §5 own-child path through the real `ctx.actor` call site:
    /// a non-root singleton overrides [`Singleton::resolve`] to fold the
    /// caller's carry, and `ctx.actor::<R>()` feeds it `self.binding.carry()`.
    /// A parent at carry `C` addressing this child by bare type lands on
    /// `fold(C, ActorId::singleton(NAMESPACE))`, not the flat `hash(NAMESPACE)`
    /// — the miss the lineage fold closes (#1364).
    #[test]
    fn ctx_actor_folds_own_child_singleton_onto_caller_carry() {
        use crate::actor::native::ctx::NativeCtx;
        use aether_actor::{Actor, Singleton};
        use aether_data::{ActorId, Tag, fold_lineage, mailbox_id_from_name, with_tag};

        struct OwnChild;
        impl Actor for OwnChild {
            const NAMESPACE: &'static str = "test.actor_fold.child";
        }
        impl Singleton for OwnChild {
            fn resolve(caller_carry: u64) -> MailboxId {
                MailboxId(with_tag(
                    Tag::Mailbox,
                    fold_lineage(caller_carry, ActorId::singleton(<Self as Actor>::NAMESPACE)),
                ))
            }
        }

        let (_registry, mailer) = fresh_substrate();
        let parent_carry = 0x0BAD_F00D_u64;
        let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(parent_carry)));
        let ctx = NativeCtx::new(&transport, ReplyTo::NONE, MailId::NONE, MailId::NONE);

        let resolved = ctx.actor::<OwnChild>().mailbox_id();
        let expected = MailboxId(with_tag(
            Tag::Mailbox,
            fold_lineage(parent_carry, ActorId::singleton("test.actor_fold.child")),
        ));
        assert_eq!(
            resolved, expected,
            "ctx.actor feeds self.carry to Singleton::resolve, folding the own-child node"
        );
        assert_ne!(
            resolved,
            mailbox_id_from_name("test.actor_fold.child"),
            "the folded own-child id differs from the flat depth-1 hash"
        );
    }

    /// `install_inbox` is single-claim — a second install panics.
    #[test]
    #[should_panic(expected = "install_inbox called twice")]
    fn install_inbox_twice_panics() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (_tx1, rx1) = mpsc::channel::<Envelope>();
        let (_tx2, rx2) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx1);
        transport.install_inbox(rx2);
    }

    /// 2b: the buffered send path holds mail until flush, then forms one
    /// blob and routes each mail to its recipient with bytes + kind
    /// intact. Nothing reaches the sink before `flush_outbound`.
    #[test]
    fn buffered_sends_route_only_after_flush() {
        let (registry, mailer) = fresh_substrate();
        let (tx, rx) = mpsc::channel::<Envelope>();
        registry.register_inbox("test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(0x5151));

        transport.push_envelope_buffered(recipient.0, 7, &[1, 2, 3], 1, None, None);
        transport.push_envelope_buffered(recipient.0, 9, &[4, 5], 1, None, None);
        assert!(
            rx.try_recv().is_err(),
            "buffered sends must not route before flush"
        );

        transport.flush_outbound();
        let a = rx.try_recv().expect("first mail delivered after flush");
        let b = rx.try_recv().expect("second mail delivered after flush");
        assert_eq!(a.payload.bytes(), &[1, 2, 3]);
        assert_eq!(a.kind, KindId(7));
        assert_eq!(b.payload.bytes(), &[4, 5]);
        assert_eq!(b.kind, KindId(9));
        // Buffer drained — a second flush is a no-op.
        transport.flush_outbound();
        assert!(rx.try_recv().is_err());
    }

    /// 2b: a payload larger than the per-actor ring degrades to the
    /// `Owned` copy-out valve rather than panicking, still delivering the
    /// bytes intact (the large-payload zero-copy path is deferred).
    #[test]
    fn buffered_oversized_payload_flushes_via_copy_out() {
        let (registry, mailer) = fresh_substrate();
        let (tx, rx) = mpsc::channel::<Envelope>();
        registry.register_inbox("test.sink", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.sink").unwrap();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(0x6262));

        // Larger than the whole ring — never fits, so the valve copies out.
        let big = vec![0xABu8; ACTOR_RING_BYTES + 4096];
        transport.push_envelope_buffered(recipient.0, 3, &big, 1, None, None);
        transport.flush_outbound();

        let env = rx
            .try_recv()
            .expect("oversized mail still delivered via copy-out");
        assert_eq!(env.payload.len(), big.len());
        assert_eq!(env.payload.bytes(), &big[..]);
    }

    /// 2b: flushing an empty buffer is a no-op — the common idempotent
    /// case, since `NativeCtx::Drop` flushes every handler and most send
    /// nothing. Must not panic or allocate a ring.
    #[test]
    fn buffered_flush_empty_is_noop() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(0x7373));
        transport.flush_outbound();
        transport.flush_outbound();
    }

    /// Contention/backoff-sensitive tests live in `mod heavy`: this exercises
    /// the concurrent flush / consumer-release race, so it is serialized into
    /// the `serial-heavy` nextest group (`.config/nextest.toml`) to avoid
    /// oversubscribing cores, and selected by `scripts/flake-soak.sh` for
    /// fresh-process soak repetition.
    mod heavy {
        use super::*;

        /// 2b load-bearing race: the producer flushes tagged blobs into its
        /// ring while consumer threads read each `InRing` payload in place
        /// and drop the envelope (RAII-releasing the blob lock). A reused
        /// region — the producer overwriting bytes a consumer is mid-read on
        /// — would surface as a tag mismatch. This lifts the 2a ring stress
        /// test onto the full 2b path: buffer → flush → route → mpsc →
        /// consumer drop.
        #[test]
        fn buffered_concurrent_flush_and_consumer_release() {
            use std::thread;

            let (registry, mailer) = fresh_substrate();
            let (tx, rx) = mpsc::channel::<Envelope>();
            registry.register_inbox("test.sink", forward_to_envelope_sender(tx));
            let recipient = registry.lookup("test.sink").unwrap();
            let transport = NativeBinding::new_for_test(mailer, MailboxId(0x9191));

            let rx = Arc::new(Mutex::new(rx));
            let done = Arc::new(AtomicBool::new(false));
            let consumed = Arc::new(AtomicU64::new(0));
            let n_consumers = 4;

            let consumers: Vec<_> = (0..n_consumers)
                .map(|_| {
                    let rx = Arc::clone(&rx);
                    let done = Arc::clone(&done);
                    let consumed = Arc::clone(&consumed);
                    thread::spawn(move || {
                        loop {
                            let got = {
                                let guard = rx.lock().expect("rx mutex poisoned");
                                guard.recv_timeout(Duration::from_millis(20))
                            };
                            match got {
                                Ok(env) => {
                                    let bytes = env.payload.bytes();
                                    let tag = bytes[0];
                                    assert!(
                                        bytes.iter().all(|&b| b == tag),
                                        "decode-in-place saw a reused region: expected tag {tag}"
                                    );
                                    drop(env); // RAII release of the blob lock
                                    consumed.fetch_add(1, Ordering::AcqRel);
                                }
                                // Empty for the timeout: exit only once the
                                // producer is done (channel fully drained).
                                Err(_) if done.load(Ordering::Acquire) => break,
                                Err(_) => {}
                            }
                        }
                    })
                })
                .collect();

            let mut sent = 0u64;
            for i in 0..4_000u32 {
                let tag = (i & 0xff) as u8;
                let n = (i % 4 + 1) as usize;
                let payload = vec![tag; 8 + (i as usize % 24)];
                for _ in 0..n {
                    transport.push_envelope_buffered(recipient.0, 7, &payload, 1, None, None);
                    sent += 1;
                }
                transport.flush_outbound();
            }
            // All flushes returned synchronously, so every envelope is in the
            // channel before we signal done.
            done.store(true, Ordering::Release);
            for h in consumers {
                h.join().expect("consumer thread joins");
            }
            assert_eq!(
                consumed.load(Ordering::Acquire),
                sent,
                "every flushed mail must be consumed"
            );
        }
    }
}
