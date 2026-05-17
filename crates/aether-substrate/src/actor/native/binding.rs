// Wire-encode: the FFI-mirror `wait_reply` ABI returns `i32` with
// `-1`/`-2`/`-3` reserved for timeout/buffer/cancelled and non-negative
// values returning the byte length; the `len → i32` cast (and matching
// `i32 → u64 → Duration` widening on `timeout_ms`) preserve the wire
// shape `aether-actor`'s sync wrapper expects.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

//! ADR-0074 §Decision (revisited by issue 665): native per-actor
//! binding state.
//!
//! [`NativeBinding`] is a regular struct each capability owns. It
//! holds the per-actor state — mailer + self mailbox + inbox +
//! correlation counter + wait-overflow queue — directly as fields,
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::actor::native::envelope::Envelope;
use crate::chassis::ctx::ChassisCtx;
use crate::mail::mailer::Mailer;
use crate::mail::{KindId, Mail, MailId, MailboxId, ReplyTarget, ReplyTo};
use crate::runtime::lifecycle::{FatalAborter, PanicAborter};

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
/// - [`Self::wait_reply`] — pulls from `self.inbox` with timeout,
///   filters by `(kind, correlation)`, parks non-matching envelopes
///   into `self.overflow` for a future `wait_reply` to find,
///   mirrors the wasm side's [`super::ctx::NativeCtx`] sync-wait
///   semantics.
/// - [`Self::prev_correlation`] — reads the atomic counter.
///
/// Reply (the typed `K` shape) goes through
/// [`Self::send_reply_for_handler`] below; persistence
/// (`save_state`) is wasm-component-only (ADR-0016) and never lands
/// here.
pub struct NativeBinding {
    mailer: Arc<Mailer>,
    self_mailbox: MailboxId,
    /// Owned by `wait_reply`; held in a `Mutex` so the `&self`
    /// receiver can take exclusive access. Wrapped in `OnceLock`
    /// so the inbox can be installed lazily after construction
    /// (capabilities sometimes have to thread the receiver through
    /// a builder before the transport sees it). `OnceLock::get()`
    /// returns `None` until [`NativeBinding::install_inbox`] runs;
    /// `wait_reply` returns the `ERR_NO_INBOX` sentinel in that
    /// case.
    inbox: OnceLock<Mutex<Receiver<Envelope>>>,
    /// Mismatched envelopes a previous `wait_reply` pulled but
    /// didn't return; consulted before the next `recv_timeout`.
    overflow: Mutex<VecDeque<Envelope>>,
    /// Monotonic correlation counter — atomic so `&self` can mint
    /// new ids without `&mut`.
    correlation: AtomicU64,
    /// ADR-0074 §Decision 5 cross-class `wait_reply` guard. `true`
    /// means this transport's owning capability declared
    /// `Capability::FRAME_BARRIER = true`; combined with the
    /// `frame_bound_set` lookup below, [`Self::wait_reply`] aborts
    /// when a frame-bound caller blocks on a free-running recipient
    /// (which would wedge the per-frame drain barrier waiting on a
    /// thread that doesn't synchronize with frames).
    caller_frame_bound: bool,
    /// Membership view of the chassis's frame-bound mailbox set.
    /// Read on each [`Self::wait_reply`] to classify the recipient
    /// of the prior `send_mail`. Cloned from
    /// [`ChassisCtx::frame_bound_set`] at boot; the chassis adds
    /// entries as additional frame-bound capabilities boot, so this
    /// view stays current across the chassis lifetime.
    frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
    /// Indirection over [`crate::runtime::lifecycle::fatal_abort`] — invoked
    /// by [`Self::wait_reply`] on cross-class violation. Cloned from
    /// [`ChassisCtx::fatal_aborter`] at boot.
    aborter: Arc<dyn FatalAborter>,
    /// Tracks `correlation_id → recipient_mailbox` for outbound
    /// requests so [`Self::wait_reply`] can resolve the recipient
    /// when checking the cross-class guard. Populated by
    /// [`Self::send_mail`] (every send, since fire-and-forget vs
    /// request/reply isn't distinguishable at this layer); pruned
    /// by [`Self::wait_reply`] when the matching reply arrives or
    /// the deadline expires. Bounded by the cleanup pass in
    /// `wait_reply`'s exit paths plus an opportunistic cap (see
    /// `MAX_PENDING_RECIPIENTS`); a runaway sender that never paired
    /// a wait would otherwise leak entries here.
    pending_recipients: Mutex<HashMap<u64, MailboxId>>,
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
}

/// Soft cap on [`NativeBinding::pending_recipients`]. A
/// fire-and-forget `send_mail` (one with no paired `wait_reply`)
/// leaves an entry in the map indefinitely; the cap stops a runaway
/// sender from growing the map without bound. Picked large enough
/// to comfortably exceed any realistic burst of in-flight requests
/// from a single capability — the cross-class guard is preventive,
/// not load-bearing for normal traffic. When the cap is hit the
/// oldest entry is dropped (insertion order; we accept that the
/// pruned correlation will silently skip the guard if its reply
/// ever lands, which is strictly less safe than aborting but no
/// worse than the pre-guard baseline).
const MAX_PENDING_RECIPIENTS: usize = 1024;

impl NativeBinding {
    /// Build a fresh transport. Pair `self_mailbox` with the id the
    /// `MailboxClaim` returned (the substrate routes replies back
    /// to it via the `ReplyTarget::Component(self_mailbox)` tag the
    /// transport stamps onto outbound mail). The inbox is installed
    /// separately via [`Self::install_inbox`] so capabilities that
    /// build the transport before pulling the receiver out of their
    /// claim aren't forced into a specific construction order.
    ///
    /// `caller_frame_bound`, `frame_bound_set`, and `aborter` wire
    /// the ADR-0074 §Decision 5 cross-class `wait_reply` guard.
    /// Capabilities authored under a [`crate::ChassisCtx`] should
    /// prefer [`Self::from_ctx`], which inherits the chassis's
    /// shared set + aborter automatically; the explicit constructor
    /// is for harnesses that don't go through a chassis (`TestBench`
    /// internals) or for tests that want to substitute a custom
    /// aborter.
    pub fn new(
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        caller_frame_bound: bool,
        frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
        aborter: Arc<dyn FatalAborter>,
        spawner: Option<Arc<crate::Spawner>>,
    ) -> Self {
        Self {
            mailer,
            self_mailbox,
            inbox: OnceLock::new(),
            overflow: Mutex::new(VecDeque::new()),
            correlation: AtomicU64::new(0),
            caller_frame_bound,
            frame_bound_set,
            aborter,
            pending_recipients: Mutex::new(HashMap::new()),
            spawner,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Convenience constructor that pulls the cross-class guard
    /// state (frame-bound set + aborter) from a [`ChassisCtx`]. The
    /// natural call site is inside a [`crate::Capability::boot`]:
    ///
    /// ```ignore
    /// let claim = ctx.claim_mailbox_drop_on_shutdown(NAME)?;
    /// let transport = NativeBinding::from_ctx(ctx, claim.id, Self::FRAME_BARRIER);
    /// ```
    ///
    /// Capabilities that don't migrate to this constructor in the
    /// same PR keep using [`Self::new_for_test`] (or call
    /// [`Self::new`] explicitly with their chosen guard state); the
    /// guard is a no-op for non-frame-bound callers and the
    /// `PanicAborter` default is harmless for production caps that
    /// never call `wait_reply`.
    #[must_use]
    pub fn from_ctx(ctx: &ChassisCtx<'_>, self_mailbox: MailboxId, frame_bound: bool) -> Self {
        Self::new(
            ctx.mail_send_handle(),
            self_mailbox,
            frame_bound,
            ctx.frame_bound_set(),
            ctx.fatal_aborter(),
            Some(Arc::clone(ctx.spawner_arc())),
        )
    }

    /// Test-only constructor that disables the cross-class guard
    /// (non-frame-bound caller, empty set, [`PanicAborter`]). Lets
    /// existing unit tests construct a transport without naming
    /// every guard parameter; not appropriate for production
    /// capabilities, which should go through [`Self::from_ctx`] so
    /// the guard wires to the chassis's shared state.
    pub fn new_for_test(mailer: Arc<Mailer>, self_mailbox: MailboxId) -> Self {
        Self::new(
            mailer,
            self_mailbox,
            false,
            Arc::new(RwLock::new(HashSet::new())),
            Arc::new(PanicAborter),
            None,
        )
    }

    /// Install the receiver half of the actor's inbox so
    /// `wait_reply` has somewhere to pull from. Called once per
    /// transport, before any `wait_reply` invocation. Subsequent
    /// calls panic — the slot is single-claim by construction.
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
    /// Distinct from [`Self::wait_reply`], which filters by
    /// `(kind, correlation)` and returns when a *specific* reply
    /// arrives — `recv_blocking` is for the dispatcher's "next
    /// thing, whatever it is" main loop.
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
        inbox.lock().unwrap().recv().ok()
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
        inbox.lock().unwrap().try_recv().ok()
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

/// Negative sentinel for `wait_reply` when no inbox is installed.
/// Picked outside the documented `-1`/`-2`/`-3` range so the SDK's
/// `decode_wait_reply` falls into the unknown-rc branch and surfaces
/// "no inbox installed" by name in the error.
const ERR_NO_INBOX_I32: i32 = 100;

/// Inherent send / `wait_reply` / `prev_correlation` entry points the
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
    /// per-handler [`super::ctx::NativeCtx`]'s [`Sender`] impl reads
    /// from its `in_flight_mail_id()` / `in_flight_root()` accessors
    /// and threads them in.
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
        // mail. No-op when the global trace queue isn't installed
        // (test fixtures bypassing the chassis); the drainer is the
        // only consumer.
        crate::runtime::trace::record_sent(
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
        // Record `correlation -> recipient` for the cross-class
        // `wait_reply` guard. We record on every send (the FFI here
        // can't tell fire-and-forget from request/reply) and let
        // `wait_reply` prune. The opportunistic cap below stops a
        // runaway sender that never paired a wait from leaking.
        {
            let mut pending = self.pending_recipients.lock().unwrap();
            if pending.len() >= MAX_PENDING_RECIPIENTS
                && let Some(&drop_key) = pending.keys().next()
            {
                pending.remove(&drop_key);
            }
            pending.insert(correlation, recipient_id);
        }
        self.mailer.push(mail);
        mail_id
    }

    /// Block this actor's thread until a mail of `expected_kind`
    /// (and, when `expected_correlation != 0`, also matching that
    /// correlation id) arrives, then copy up to `out.len()` bytes of
    /// its payload into `out` (ADR-0042). `timeout_ms` is clamped
    /// substrate-side to 30s.
    ///
    /// Returns `>= 0` = bytes written, `-1` = timeout, `-2` = payload
    /// larger than `out` (mail re-parked for retry), `-3` = the host
    /// tore the actor down mid-wait. Any other negative is a
    /// transport-specific sentinel (e.g. `-100` no-inbox).
    ///
    /// # Panics
    /// Panics if any of the internal mutexes (overflow, pending
    /// recipients, frame-bound set) are poisoned, or if the cross-class
    /// guard fires (ADR-0074 §Decision 5: a frame-bound caller blocking
    /// on a free-running recipient triggers `fatal_abort`) — both are
    /// fail-fast per ADR-0063.
    pub fn wait_reply(
        &self,
        expected_kind: u64,
        out: &mut [u8],
        timeout_ms: u32,
        expected_correlation: u64,
    ) -> i32 {
        let Some(inbox_mutex) = self.inbox.get() else {
            tracing::error!(
                target: "aether_substrate::native_transport",
                "wait_reply called without an installed inbox — install_inbox must run first"
            );
            return -ERR_NO_INBOX_I32;
        };

        // ADR-0074 §Decision 5 cross-class guard: a frame-bound
        // caller blocking on a free-running recipient would wedge
        // the per-frame drain barrier waiting on a thread that
        // doesn't synchronize with frames. Detect at the call site
        // and abort with a specific diagnostic instead of letting
        // `drain_frame_bound_or_abort` time out further downstream
        // with a less actionable "dispatcher wedged" reason. The
        // guard is preventive — today no in-tree capability calls
        // `wait_reply` cross-class — so the early return path is
        // for future-cap correctness, not current behavior.
        if self.caller_frame_bound
            && expected_correlation != ReplyTo::NO_CORRELATION
            && let Some(recipient) = self
                .pending_recipients
                .lock()
                .unwrap()
                .get(&expected_correlation)
                .copied()
            && !self.frame_bound_set.read().unwrap().contains(&recipient)
        {
            self.aborter.abort(format!(
                "frame-bound actor {} attempted wait_reply on free-running recipient {} \
                 (correlation {expected_correlation}, expected_kind {expected_kind}) — \
                 forbidden by ADR-0074 §Decision 5: blocking on a free-running actor would \
                 wedge the per-frame drain barrier",
                self.self_mailbox, recipient,
            ));
        }

        let timeout = Duration::from_millis(timeout_ms as u64);
        let deadline = Instant::now() + timeout;

        let rc = loop {
            // Drain overflow first — a previous `wait_reply` may
            // have parked envelopes that match this kind /
            // correlation.
            let from_overflow = {
                let mut overflow = self.overflow.lock().unwrap();
                let pos = overflow
                    .iter()
                    .position(|env| matches_filter(env, expected_kind, expected_correlation));
                pos.and_then(|i| overflow.remove(i))
            };
            if let Some(env) = from_overflow {
                let rc = write_payload(&env, out);
                if rc == -2 {
                    // Buffer too small: park back at the front so a
                    // retry with a larger buffer picks it up before
                    // anything newer.
                    self.overflow.lock().unwrap().push_front(env);
                }
                break rc;
            }

            // No overflow match — pull from the inbox with whatever
            // time is left on the deadline. The mutex guard stays
            // held across `recv_timeout`; the dispatcher thread is
            // single-tasked while parked here, so no other code on
            // this thread contends with the lock.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let recv_outcome = inbox_mutex.lock().unwrap().recv_timeout(remaining);

            match recv_outcome {
                Ok(env) => {
                    if matches_filter(&env, expected_kind, expected_correlation) {
                        let rc = write_payload(&env, out);
                        if rc == -2 {
                            // Same retry-friendly disposition as
                            // overflow-matched: park at the front.
                            self.overflow.lock().unwrap().push_front(env);
                        }
                        break rc;
                    }
                    self.overflow.lock().unwrap().push_back(env);
                    // Loop continues — try again with whatever time
                    // is left on the deadline.
                }
                Err(RecvTimeoutError::Timeout) => break -1,
                Err(RecvTimeoutError::Disconnected) => break -3,
            }
        };

        // Whatever the outcome, drop the recipient tracking entry
        // for this correlation — we won't need it again. (Keeps the
        // `pending_recipients` map bounded by the actual rate of
        // unpaired sends rather than total send volume.)
        if expected_correlation != ReplyTo::NO_CORRELATION {
            self.pending_recipients
                .lock()
                .unwrap()
                .remove(&expected_correlation);
        }
        rc
    }

    /// Correlation id the substrate minted for this actor's most
    /// recent `send_mail` (ADR-0042). `0` before any send. Universal
    /// — every send mints a correlation; sync wrappers filter
    /// `wait_reply` against it, async handlers stash it and match on
    /// the inbound's reply correlation.
    pub fn prev_correlation(&self) -> u64 {
        self.correlation.load(Ordering::Acquire)
    }
}

fn matches_filter(env: &Envelope, expected_kind: u64, expected_correlation: u64) -> bool {
    env.kind.0 == expected_kind
        && (expected_correlation == ReplyTo::NO_CORRELATION
            || env.sender.correlation_id == expected_correlation)
}

/// Copy `env.payload` into `out` and return the number of bytes
/// written, matching the wasm `wait_reply_p32` ABI:
/// `>= 0` = bytes written, `-2` = payload too large for the buffer.
/// Caller is responsible for parking the envelope back on overflow
/// when -2 is returned so a retry with a bigger buffer can pick it up
/// (the helper is byte-only so it can also be used for peek-style
/// callers that don't have an overflow to park on).
fn write_payload(env: &Envelope, out: &mut [u8]) -> i32 {
    if env.payload.len() > out.len() {
        return -2;
    }
    out[..env.payload.len()].copy_from_slice(&env.payload);
    env.payload.len() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::registry::Registry;
    use std::sync::mpsc;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(crate::handle_store::HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        (registry, mailer)
    }

    /// Build a registry handler that forwards every [`MailDispatch`]
    /// it receives onto `tx` as an owned [`Envelope`]. Used by tests
    /// that need a registered recipient but only care about
    /// observing — or just not warn-dropping — the mail.
    fn forward_to_envelope_sender(
        tx: mpsc::Sender<Envelope>,
    ) -> Arc<dyn crate::mail::registry::InboxHandler> {
        // iamacoffeepot/aether#848: the helper takes
        // [`OwnedDispatch`] directly so payload + kind_name move
        // into the forwarded [`Envelope`] without `to_vec()` /
        // `to_owned()` clones.
        Arc::new(move |dispatch: crate::mail::registry::OwnedDispatch| {
            let _ = tx.send(Envelope {
                kind: dispatch.kind,
                kind_name: dispatch.kind_name,
                origin: dispatch.origin,
                sender: dispatch.sender,
                payload: dispatch.payload,
                count: dispatch.count,
                mail_id: dispatch.mail_id,
                root: dispatch.root,
                parent_mail: dispatch.parent_mail,
            });
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

    /// `wait_reply` with no inbox installed returns the no-inbox
    /// negative sentinel.
    #[test]
    fn wait_reply_without_inbox_returns_no_inbox_sentinel() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(0, &mut buf, 1, 0);
        assert_eq!(rc, -ERR_NO_INBOX_I32);
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

    /// `wait_reply` returns the `-1` timeout sentinel when no
    /// envelope arrives within the deadline.
    #[test]
    fn wait_reply_times_out_when_inbox_quiet() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (_tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);
        let mut buf = [0u8; 16];
        // 1ms is enough — no sender ever pushes.
        let rc = transport.wait_reply(0, &mut buf, 1, 0);
        assert_eq!(rc, -1);
    }

    /// Build a transport with the cross-class guard wired: caller
    /// classification is configurable, and the chassis's
    /// frame-bound set is shared so tests can pre-populate it to
    /// classify recipients. Aborter is [`PanicAborter`] so a
    /// triggered abort surfaces as `should_panic` rather than
    /// `process::exit`-ing the test runner.
    fn transport_with_guard(
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
        caller_frame_bound: bool,
        frame_bound_set: Arc<RwLock<HashSet<MailboxId>>>,
    ) -> NativeBinding {
        NativeBinding::new(
            mailer,
            self_mailbox,
            caller_frame_bound,
            frame_bound_set,
            Arc::new(PanicAborter),
            None,
        )
    }

    /// ADR-0074 §Decision 5: a frame-bound caller blocking on a
    /// free-running recipient must abort. Verified via the
    /// `PanicAborter` — the panic message names both mailboxes plus
    /// the ADR.
    #[test]
    #[should_panic(expected = "forbidden by ADR-0074")]
    fn cross_class_wait_reply_aborts_when_caller_frame_bound_and_recipient_free_running() {
        let (registry, mailer) = fresh_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        registry.register_inbox("test.free.running", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.free.running").unwrap();

        // Empty frame-bound set => recipient classifies as free-running.
        let frame_bound_set = Arc::new(RwLock::new(HashSet::new()));
        let transport =
            transport_with_guard(Arc::clone(&mailer), MailboxId(99), true, frame_bound_set);
        let (_tx_inbox, rx_inbox) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx_inbox);

        // Send first to record the recipient against a correlation id.
        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        let correlation = transport.prev_correlation();

        // wait_reply should abort before timing out.
        let mut buf = [0u8; 16];
        transport.wait_reply(1, &mut buf, 1, correlation);
    }

    /// Same shape as the abort test, but the recipient is
    /// pre-registered as frame-bound. Guard sees same-class and
    /// lets `wait_reply` proceed to its normal `-1` timeout.
    #[test]
    fn cross_class_wait_reply_does_not_abort_when_recipient_also_frame_bound() {
        let (registry, mailer) = fresh_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        registry.register_inbox("test.frame.bound", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.frame.bound").unwrap();

        let frame_bound_set = Arc::new(RwLock::new(HashSet::new()));
        // Pre-populate recipient as frame-bound (mirroring what
        // `claim_frame_bound_mailbox` would do).
        frame_bound_set.write().unwrap().insert(recipient);

        let transport = transport_with_guard(
            Arc::clone(&mailer),
            MailboxId(99),
            true,
            Arc::clone(&frame_bound_set),
        );
        let (_tx_inbox, rx_inbox) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx_inbox);

        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        let correlation = transport.prev_correlation();

        let mut buf = [0u8; 16];
        // Same-class: no abort, just normal timeout (1ms).
        let rc = transport.wait_reply(1, &mut buf, 1, correlation);
        assert_eq!(rc, -1);
    }

    /// Free-running caller never trips the guard regardless of
    /// recipient class — only frame-bound callers care about being
    /// blocked across a class boundary.
    #[test]
    fn cross_class_wait_reply_does_not_abort_when_caller_free_running() {
        let (registry, mailer) = fresh_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        registry.register_inbox("test.any", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.any").unwrap();

        // Empty set — recipient is free-running. Caller is also
        // free-running. Guard must be inert.
        let frame_bound_set = Arc::new(RwLock::new(HashSet::new()));
        let transport =
            transport_with_guard(Arc::clone(&mailer), MailboxId(99), false, frame_bound_set);
        let (_tx_inbox, rx_inbox) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx_inbox);

        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        let correlation = transport.prev_correlation();

        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(1, &mut buf, 1, correlation);
        assert_eq!(rc, -1);
    }

    /// `pending_recipients` is cleaned up after `wait_reply` exits
    /// (success, timeout, disconnect) so fire-and-forget senders
    /// don't leak entries past their correlation horizon.
    #[test]
    fn wait_reply_prunes_pending_recipient_on_timeout() {
        let (registry, mailer) = fresh_substrate();
        let (tx, _rx) = mpsc::channel::<Envelope>();
        registry.register_inbox("test.prune", forward_to_envelope_sender(tx));
        let recipient = registry.lookup("test.prune").unwrap();

        let transport = NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(99));
        let (_tx_inbox, rx_inbox) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx_inbox);

        assert_eq!(transport.send_mail(recipient.0, 1, &[], 1), 0);
        let correlation = transport.prev_correlation();
        assert_eq!(transport.pending_recipients.lock().unwrap().len(), 1);

        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(1, &mut buf, 1, correlation);
        assert_eq!(rc, -1);
        assert_eq!(transport.pending_recipients.lock().unwrap().len(), 0);
    }

    fn make_envelope(kind: u64, payload: Vec<u8>, correlation: u64) -> Envelope {
        Envelope {
            kind: KindId(kind),
            kind_name: String::new(),
            origin: None,
            sender: ReplyTo::with_correlation(ReplyTarget::None, correlation),
            payload,
            count: 1,
            mail_id: crate::mail::MailId::NONE,
            root: crate::mail::MailId::NONE,
            parent_mail: None,
        }
    }

    /// `wait_reply` returns the matched envelope when it arrives via
    /// the inbox while the wait is parked.
    #[test]
    fn wait_reply_returns_payload_when_match_arrives() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);

        tx.send(make_envelope(0xABCD, vec![1, 2, 3, 4, 5], 0))
            .unwrap();

        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(0xABCD, &mut buf, 100, 0);
        assert_eq!(rc, 5);
        assert_eq!(&buf[..5], &[1, 2, 3, 4, 5]);
    }

    /// `wait_reply` parks non-matching envelopes onto overflow so the
    /// dispatcher's next `recv` (or a follow-up wait) sees them.
    #[test]
    fn wait_reply_parks_non_matching_into_overflow() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);

        tx.send(make_envelope(0x1111, vec![9], 0)).unwrap();
        tx.send(make_envelope(0xABCD, vec![1], 0)).unwrap();

        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(0xABCD, &mut buf, 100, 0);
        assert_eq!(rc, 1);
        assert_eq!(transport.overflow.lock().unwrap().len(), 1);
    }

    /// `wait_reply` filters by correlation when one is supplied — a
    /// matching kind with a different correlation parks; only the
    /// correlation-matched envelope returns.
    #[test]
    fn wait_reply_filters_by_correlation_not_just_kind() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);

        tx.send(make_envelope(0xABCD, vec![0xFF], 11)).unwrap();
        tx.send(make_envelope(0xABCD, vec![0x42], 22)).unwrap();

        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(0xABCD, &mut buf, 100, 22);
        assert_eq!(rc, 1);
        assert_eq!(buf[0], 0x42);
        assert_eq!(transport.overflow.lock().unwrap().len(), 1);
    }

    /// `wait_reply` checks overflow before recv — a matching envelope
    /// already on overflow returns without touching the inbox.
    #[test]
    fn wait_reply_pulls_match_from_overflow_before_recv() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);

        transport
            .overflow
            .lock()
            .unwrap()
            .push_back(make_envelope(0xABCD, vec![7], 0));
        drop(tx);

        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(0xABCD, &mut buf, 100, 0);
        assert_eq!(rc, 1);
        assert_eq!(buf[0], 7);
    }

    /// `wait_reply` returns -2 when payload exceeds the buffer and
    /// parks the envelope back on overflow with `push_front` so a
    /// retry with a larger buffer rediscovers it.
    #[test]
    fn wait_reply_parks_on_buffer_too_small_for_retry() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);

        let big_payload = vec![0xAA; 10];
        tx.send(make_envelope(0xABCD, big_payload.clone(), 0))
            .unwrap();

        let mut small = [0u8; 4];
        let rc = transport.wait_reply(0xABCD, &mut small, 100, 0);
        assert_eq!(rc, -2);
        assert_eq!(transport.overflow.lock().unwrap().len(), 1);

        let mut big = [0u8; 16];
        let rc = transport.wait_reply(0xABCD, &mut big, 100, 0);
        assert_eq!(rc, big_payload.len() as i32);
        assert_eq!(&big[..big_payload.len()], &big_payload[..]);
    }

    /// `wait_reply` returns -3 cancelled when the inbox sender drops
    /// (the receiver disconnects) before any matching mail arrives.
    #[test]
    fn wait_reply_returns_cancelled_when_sender_drops() {
        let (_registry, mailer) = fresh_substrate();
        let transport = NativeBinding::new_for_test(mailer, MailboxId(1));
        let (tx, rx) = mpsc::channel::<Envelope>();
        transport.install_inbox(rx);
        drop(tx);

        let mut buf = [0u8; 16];
        let rc = transport.wait_reply(0xABCD, &mut buf, 100, 0);
        assert_eq!(rc, -3);
    }
}
