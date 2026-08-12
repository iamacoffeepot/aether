//! Issue 552 stage 1: integration test for the
//! `#[actor] impl NativeActor for X` macro arm.
//!
//! Lives as a standalone integration test (not in `#[cfg(test)] mod`)
//! because the macro emits absolute paths (`::aether_substrate::*`)
//! that the lib's own test mod can't resolve via implicit `extern
//! crate self`. An integration test depends on `aether_substrate`
//! externally, so the path resolves naturally.
//!
//! The chassis-side hand-rolled fixture in
//! `chassis_builder::tests::with_actor_boots_dispatches_and_tears_down`
//! covers the same end-to-end guarantee without the macro layer; this
//! test is the additional gate that the macro's codegen produces a
//! working `Actor + HandlesKind + NativeActor + NativeDispatch`
//! stack on a real cap shape.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]
// A deliberate embedder: the macro-codegen gate builds a bare `TestChassis` via
// `Builder::new` rather than the `composed` boot seam.
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aether_actor::OutboundReply;
use aether_data::{Kind, Source, SourceAddr};
use aether_kinds::trace::Nanos;
use aether_substrate::actor::native::{Pending, TaskDone};
use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch};
use aether_substrate::mail::{MailId, MailRef};
use aether_substrate::testing::{TestChassis, bare_substrate, boot_authority};
use aether_substrate::{
    Addressable, BootError, Builder, Dispatch, Manual, NativeActor, NativeBinding, NativeCtx, NativeInitCtx,
    PassiveChassis, Registry, mail::MailboxId,
};
use std::thread;

/// Structured-shape kind via the derive — exercises the
/// `decode_from_bytes` structured path the macro's dispatch arm uses.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.greet")]
struct Greet {
    tag: u32,
}

/// Cast-shape kind so both arms (structured + cast) get exercised
/// through one cap.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, ::aether_data::Kind, ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.ping")]
struct Ping {
    seq: u32,
}

struct MacroProbeCap {
    greet_total: Arc<AtomicU32>,
    ping_total: Arc<AtomicU32>,
}

/// Per-cap config — caps without a domain-specific config type
/// would write `()`, but here we thread shared atomic counters in
/// so the test can observe each handler's effect.
#[derive(Clone)]
struct ProbeParams {
    greet_total: Arc<AtomicU32>,
    ping_total: Arc<AtomicU32>,
}

#[aether_actor::actor(root)]
impl NativeActor for MacroProbeCap {
    // ADR-0156 §3: the shared probe counters are construction wiring, not
    // operator config, so they ride the `Params` channel; `Config` is `()`.
    type Config = ();
    type Params = ProbeParams;
    const NAMESPACE: &'static str = "test.macro_native_actor.probe";

    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { greet_total: params.greet_total, ping_total: params.ping_total })
    }

    /// Handles structured-shape `Greet` mail.
    #[aether_actor::handler::single]
    fn on_greet(&self, _ctx: &mut NativeCtx<'_>, mail: Greet) {
        self.greet_total.fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }

    /// Handles cast-shape `Ping` mail.
    #[aether_actor::handler::single]
    fn on_ping(&self, _ctx: &mut NativeCtx<'_>, mail: Ping) {
        self.ping_total.fetch_add(mail.seq, AtomicOrdering::SeqCst);
    }
}

fn push_envelope<K: Kind>(registry: &Registry, recipient: &str, payload: &K) {
    use aether_substrate::mail::registry::MailboxEntry;
    let id: MailboxId = registry.lookup(recipient).expect("mailbox registered");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry exists") else {
        panic!("expected mailbox entry under {recipient}");
    };
    let bytes = payload.encode_into_bytes();
    handler.enqueue(OwnedDispatch::disarmed(
        <K as Kind>::ID,
        None,
        Source::NONE,
        MailRef::from(bytes),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
}

fn wait_for(target: u32, counter: &AtomicU32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while counter.load(AtomicOrdering::SeqCst) < target && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    counter.load(AtomicOrdering::SeqCst) >= target
}

#[test]
fn macro_emitted_cap_routes_structured_kind_through_dispatch() {
    let (registry, mailer) = bare_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<MacroProbeCap>(ProbeParams {
            greet_total: Arc::clone(&greet_total),
            ping_total: Arc::clone(&ping_total),
        })
        .build_passive()
        .expect("macro-emitted cap boots");

    push_envelope(&registry, MacroProbeCap::NAMESPACE, &Greet { tag: 7 });
    assert!(
        wait_for(7, &greet_total, Duration::from_millis(500)),
        "macro dispatcher should route Greet → on_greet within budget"
    );
    assert_eq!(ping_total.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);
}

/// iamacoffeepot/aether#1135: a blob demuxer seeds a free `Pooled` actor
/// via `seize_and_run` — the seed dispatches in place (no inbox deposit /
/// `try_recv` repop) and the slot returns to `Idle`. Boots a real Pooled
/// actor through the chassis, lets it quiesce, then resolves the seize
/// handle off its registry `Inbox` entry, wins the `Idle → Running` seize,
/// and runs one seed envelope.
#[test]
fn seize_and_run_dispatches_seed_in_place() {
    use aether_substrate::mail::registry::MailboxEntry;
    use aether_substrate::scheduler::{BatchBudget, SlotStateLabel};

    let (registry, mailer) = bare_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<MacroProbeCap>(ProbeParams {
            greet_total: Arc::clone(&greet_total),
            ping_total: Arc::clone(&ping_total),
        })
        .build_passive()
        .expect("macro-emitted cap boots");

    let id = registry.lookup(MacroProbeCap::NAMESPACE).expect("cap mailbox registered");

    // The cap boots with no pre-load mail, so its slot quiesces to `Idle`.
    // Resolve the seize handle off the `Inbox` entry's deferred cell (the
    // #1135 surfacing) and wait for the slot to be seizable.
    let MailboxEntry::Inbox { seize, .. } = registry.entry(id).expect("entry exists") else {
        panic!("expected an Inbox entry for the Pooled cap");
    };
    let seize = seize.get().expect("a Pooled actor exposes a seize handle");

    let deadline = Instant::now() + Duration::from_millis(500);
    let slot = loop {
        if let Some(slot) = seize.try_seize() {
            break slot;
        }
        assert!(Instant::now() < deadline, "Pooled slot should quiesce to Idle and become seizable");
        thread::sleep(Duration::from_millis(5));
    };
    // The seize put the slot in `Running`.
    assert_eq!(seize.state().current(), SlotStateLabel::Running);

    // Build one seed envelope and dispatch it in place — no inbox bounce.
    let payload = Greet { tag: 11 }.encode_into_bytes();
    let seed = OwnedDispatch::disarmed(
        <Greet as Kind>::ID,
        None,
        Source::NONE,
        MailRef::from(payload),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        // The #1135 contract: a direct-dispatched seed has residence ≈ 0.
        Nanos(0),
        0,
        MailboxId(0),
    );
    slot.seize_and_run(seed, BatchBudget::standard());

    // Handler ran exactly once; the slot drained empty back to `Idle`.
    assert_eq!(greet_total.load(AtomicOrdering::SeqCst), 11, "the seed dispatched through on_greet in place");
    assert_eq!(ping_total.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(seize.state().current(), SlotStateLabel::Idle, "the slot drained empty and returned to Idle");

    drop(chassis);
}

#[test]
fn macro_emitted_cap_routes_cast_kind_through_dispatch() {
    let (registry, mailer) = bare_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<MacroProbeCap>(ProbeParams {
            greet_total: Arc::clone(&greet_total),
            ping_total: Arc::clone(&ping_total),
        })
        .build_passive()
        .expect("macro-emitted cap boots");

    push_envelope(&registry, MacroProbeCap::NAMESPACE, &Ping { seq: 42 });
    assert!(
        wait_for(42, &ping_total, Duration::from_millis(500)),
        "macro dispatcher should route Ping → on_ping within budget"
    );
    assert_eq!(greet_total.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);
}

#[test]
fn macro_routes_task_completions_by_output_type() {
    let (registry, mailer) = bare_substrate();
    let obs = TaskObservations {
        dispatched: Arc::new(AtomicU32::new(0)),
        a_value: Arc::new(AtomicU64::new(0)),
        a_calls: Arc::new(AtomicU32::new(0)),
        b_tag: Arc::new(AtomicU32::new(0)),
        b_calls: Arc::new(AtomicU32::new(0)),
    };

    let chassis: PassiveChassis<TestChassis> = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TaskRouteCap>(obs.clone())
        .build_passive()
        .expect("task-routing cap boots");

    // Kick off both dispatches. Each handler spawns a worker that fills
    // the ledger and pushes a `TaskCompletionWake` back to this actor's
    // own mailbox; the chassis redelivers those wakes through the macro's
    // single completion arm, which routes each to its output-typed
    // handler.
    push_envelope(&registry, TaskRouteCap::NAMESPACE, &KickA { seed: 7 });
    push_envelope(&registry, TaskRouteCap::NAMESPACE, &KickB { seed: 9 });

    assert!(wait_for(1, &obs.a_calls, Duration::from_secs(2)), "the ResultA completion routed to on_result_a");
    assert!(wait_for(1, &obs.b_calls, Duration::from_secs(2)), "the ResultB completion routed to on_result_b");

    // Each completion landed on the correct handler with the correct
    // payload — output-type routing, not a kind id.
    assert_eq!(obs.a_value.load(AtomicOrdering::SeqCst), 7, "on_result_a saw its own dispatch's value");
    assert_eq!(obs.b_tag.load(AtomicOrdering::SeqCst), 9, "on_result_b saw its own dispatch's tag");

    // Neither completion was mis-delivered to the other-typed handler:
    // each task handler fired exactly once. A wrong-type probe in the
    // completion arm must leave the ledger entry intact (non-consuming
    // `try_take_task_done`) — if it consumed, one handler would swallow
    // the other's completion and its call count would be 0.
    assert_eq!(
        obs.a_calls.load(AtomicOrdering::SeqCst),
        1,
        "on_result_a fired exactly once (no mis-routed extra completion)"
    );
    assert_eq!(obs.b_calls.load(AtomicOrdering::SeqCst), 1, "on_result_b fired exactly once");
    assert_eq!(obs.dispatched.load(AtomicOrdering::SeqCst), 2, "both kick handlers dispatched a worker");

    drop(chassis);
}

/// ADR-0109: a `-> Pending<R>` request handler plus a borrow-form
/// `&TaskDone -> R` completion settles with exactly one reply of `R`.
/// The macro arms no immediate reply on the request, and on completion
/// calls `resolve_value` with the handler's returned value, routing it
/// back to the original caller.
#[test]
fn macro_pending_request_borrow_completion_replies_once() {
    let (registry, mailer) = bare_substrate();
    let obs = DeferredObs::new();

    let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
    let caller =
        registry.register_inbox(&boot_authority(), "test.macro_native_actor.deferred_caller", forward_to(reply_tx));

    let chassis: PassiveChassis<TestChassis> = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<DeferredReplyCap>(obs.clone())
        .build_passive()
        .expect("deferred-reply cap boots");

    // The inbound names the caller as its reply target, so the deferred
    // reply routes back there.
    let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 55);
    push_envelope_replying_to(&registry, DeferredReplyCap::NAMESPACE, &KickP { seed: 21 }, caller_reply_to);

    let reply = reply_rx.recv_timeout(Duration::from_secs(2)).expect("the deferred reply lands on the caller");
    assert_eq!(reply.kind, <EchoReply as Kind>::ID, "the completion's `-> EchoReply` return routed back as the reply");
    let echoed = EchoReply::decode_from_bytes(reply.payload.bytes()).expect("the reply decodes");
    assert_eq!(echoed.value, 21, "resolve_value sent the value the completion handler returned");
    assert_eq!(reply.sender.correlation_id, 55, "the caller's correlation is echoed onto the reply");
    assert_eq!(obs.echo_calls.load(AtomicOrdering::SeqCst), 1, "the borrow-form completion handler fired once");

    // Exactly one reply settles — the macro must not also drop the
    // TaskDone (which would re-release) or double-send.
    assert!(
        reply_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "exactly one reply settles for the deferred request"
    );

    drop(chassis);
}

/// ADR-0109: a borrow-form `&TaskDone -> ()` completion releases the
/// hold without sending any reply. The macro emits `release_no_reply`,
/// so the completion runs cleanly (no lost-reply `debug_assert`) and
/// nothing routes back to the caller.
#[test]
fn macro_borrow_task_no_reply_releases_without_replying() {
    let (registry, mailer) = bare_substrate();
    let obs = DeferredObs::new();

    let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
    let caller = registry.register_inbox(
        &boot_authority(),
        "test.macro_native_actor.deferred_silent_caller",
        forward_to(reply_tx),
    );

    let chassis: PassiveChassis<TestChassis> = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<DeferredReplyCap>(obs.clone())
        .build_passive()
        .expect("deferred-reply cap boots");

    let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 9);
    push_envelope_replying_to(&registry, DeferredReplyCap::NAMESPACE, &KickS { seed: 88 }, caller_reply_to);

    assert!(
        wait_for(1, &obs.silent_calls, Duration::from_secs(2)),
        "the no-reply completion ran (release_no_reply, no lost-reply panic)"
    );
    assert_eq!(obs.silent_value.load(AtomicOrdering::SeqCst), 88, "the no-reply completion saw its own worker output");

    // No reply was sent — release_no_reply discharges without replying.
    assert!(
        reply_rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "a `&TaskDone -> ()` completion sends nothing back to the caller"
    );

    drop(chassis);
}

/// Type-level assertion that the macro emits the universal
/// `HandlesKind<K>` impls for each `#[handler]`. Runs at compile
/// time — the body never executes.
#[test]
fn macro_emits_handles_kind_per_handler() {
    fn assert_handles<R: aether_actor::HandlesKind<K>, K: Kind>() {}
    assert_handles::<MacroProbeCap, Greet>();
    assert_handles::<MacroProbeCap, Ping>();
}

/// Cast-shape kind the cap doesn't handle. Used to verify the
/// macro's `__aether_dispatch_envelope` returns `None` for unknown
/// kind ids — chassis-side dispatcher logs `dispatch missed` rather
/// than crashing. Issue 688 follow-up: parity with the
/// `dispatch_returns_none_for_unhandled_kind` test that retired
/// alongside the legacy facade.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, ::aether_data::Kind, ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.unknown")]
struct Unknown {
    payload: u32,
}

#[test]
fn macro_emitted_cap_drops_unknown_kind_via_dispatch() {
    let (registry, mailer) = bare_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<MacroProbeCap>(ProbeParams {
            greet_total: Arc::clone(&greet_total),
            ping_total: Arc::clone(&ping_total),
        })
        .build_passive()
        .expect("macro-emitted cap boots");

    push_envelope(&registry, MacroProbeCap::NAMESPACE, &Unknown { payload: 99 });

    // Settle: give the dispatcher time to observe + drop the envelope.
    // The macro-emitted dispatch returns None; the chassis-side
    // dispatcher logs a warn but doesn't increment any handler counter.
    thread::sleep(Duration::from_millis(50));
    assert_eq!(greet_total.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(ping_total.load(AtomicOrdering::SeqCst), 0);

    drop(chassis);
}

// ADR-0093 §3: `#[handler(task)]` completion handlers, routed by their
// `TaskDone<O>` output type rather than a kind id. The cap below has two
// task handlers of distinct `O` (`ResultA` / `ResultB`); a single
// `TaskCompletionWake` arm tries each via the non-consuming
// `try_take_task_done` probe, so each completion lands on exactly the
// handler whose output type matches.

/// First completion output type. Distinct from `ResultB` so the macro's
/// output-type routing has two arms to discriminate.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.result_a")]
struct ResultA {
    value: u64,
}

/// Second completion output type — a structurally different shape so a
/// mis-route to the `ResultA` handler couldn't accidentally type-check.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.result_b")]
struct ResultB {
    tag: u32,
}

/// Trigger that makes the cap dispatch a `ResultA`-producing worker.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, ::aether_data::Kind, ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.kick_a")]
struct KickA {
    seed: u64,
}

/// Trigger that makes the cap dispatch a `ResultB`-producing worker.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, ::aether_data::Kind, ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.kick_b")]
struct KickB {
    seed: u32,
}

/// Where each task handler records what it observed, so the test can
/// assert routing landed on the correct handler with the correct value.
#[derive(Clone)]
// ADR-0156 §4: a test-probe `Config` carrying shared counters (wiring, not
// operator data) — declares no aggregate member (see impl below the struct).
struct TaskObservations {
    /// How many times each kick handler dispatched a worker, so the cap's
    /// mail handlers touch `self` (and the test can sanity-check the
    /// dispatch side fired).
    dispatched: Arc<AtomicU32>,
    /// `value` the `ResultA` completion handler saw + how many times it
    /// fired.
    a_value: Arc<AtomicU64>,
    a_calls: Arc<AtomicU32>,
    /// `tag` the `ResultB` completion handler saw + its call count.
    b_tag: Arc<AtomicU32>,
    b_calls: Arc<AtomicU32>,
}

struct TaskRouteCap {
    obs: TaskObservations,
}

#[aether_actor::actor(root)]
impl NativeActor for TaskRouteCap {
    // ADR-0156 §3: the shared observation counters are construction wiring, not
    // operator config, so they ride the `Params` channel; `Config` is `()`.
    type Config = ();
    type Params = TaskObservations;
    const NAMESPACE: &'static str = "test.macro_native_actor.task_route";

    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { obs: params })
    }

    /// Dispatch a worker that produces a `ResultA`. The completion routes
    /// to `on_result_a` by output type.
    #[aether_actor::handler::single]
    fn on_kick_a(&self, ctx: &mut NativeCtx<'_>, mail: KickA) {
        self.obs.dispatched.fetch_add(1, AtomicOrdering::SeqCst);
        let seed = mail.seed;
        // This cap's completions self-resolve (by-value `TaskDone`), so
        // the request handler returns `()` and dispatches via
        // `dispatch_blocking_with` rather than the `Pending<R>`-returning
        // `dispatch_blocking` (ADR-0109).
        ctx.dispatch_blocking_with((), move || ResultA { value: seed });
    }

    /// Dispatch a worker that produces a `ResultB`.
    #[aether_actor::handler::single]
    fn on_kick_b(&self, ctx: &mut NativeCtx<'_>, mail: KickB) {
        self.obs.dispatched.fetch_add(1, AtomicOrdering::SeqCst);
        let seed = mail.seed;
        ctx.dispatch_blocking_with((), move || ResultB { tag: seed });
    }

    /// `ResultA` completion handler. Records the value + a call so the
    /// test can confirm only the `ResultA` dispatch reached it.
    #[aether_actor::handler(task)]
    fn on_result_a(&self, ctx: &mut NativeCtx<'_>, done: TaskDone<ResultA>) {
        self.obs.a_value.store(done.output().value, AtomicOrdering::SeqCst);
        self.obs.a_calls.fetch_add(1, AtomicOrdering::SeqCst);
        // No caller behind these test dispatches (Source::NONE), so the
        // re-reply is a no-op; resolve still consumes the TaskDone and
        // releases the hold (avoiding the drop-without-resolve assert).
        done.resolve(ctx);
    }

    /// `ResultB` completion handler.
    #[aether_actor::handler(task)]
    fn on_result_b(&self, ctx: &mut NativeCtx<'_>, done: TaskDone<ResultB>) {
        self.obs.b_tag.store(done.output().tag, AtomicOrdering::SeqCst);
        self.obs.b_calls.fetch_add(1, AtomicOrdering::SeqCst);
        done.resolve(ctx);
    }
}

/// Tripwire: an actor with a `#[handler(task)]` measures its completion arm
/// (iamacoffeepot/aether#4266).
///
/// Every ADR-0093 completion arrives as one framework kind,
/// `TaskCompletionWake`, dispatched through the typed table but absent from the
/// ADR-0033 advertised surface — a caller cannot address it, so `capabilities`
/// rightly omits it. Seeding the cost table from `capabilities` therefore left
/// task handlers with no `CostCell` at all: their execution time never folded,
/// `actor_cost` showed no row, and the #4261 undeclared-handler warning fired
/// on the one condition its author could not act on, because the macro owns
/// the declaration.
///
/// Asserts the split holds in both directions — the measured set carries the
/// completion kind, the advertised set does not.
#[test]
fn a_task_handler_is_measured_but_not_advertised() {
    use aether_data::Kind;
    use aether_substrate::actor::native::{Dispatch, TaskCompletionWake};

    let measured = <TaskRouteCap as Dispatch<TaskRouteCap>>::measured_kinds();
    let advertised: Vec<_> =
        <TaskRouteCap as Dispatch<TaskRouteCap>>::capabilities().handlers.iter().map(|h| h.id).collect();
    let wake = <TaskCompletionWake as Kind>::ID;

    assert!(measured.contains(&wake), "the completion arm must own a cost cell, got {measured:?}");
    assert!(
        !advertised.contains(&wake),
        "a caller cannot address a completion, so it stays off the advertised surface"
    );
    for kind in &advertised {
        assert!(measured.contains(kind), "every advertised handler is still measured, missing {kind:?}");
    }
}

/// iamacoffeepot/aether#4811: a handler's `#[cfg]` governs the artifacts the
/// macro derives from it.
///
/// `syn` does not evaluate `cfg`, so `#[actor]` sees all three handlers below
/// whichever configuration it expands in. An integration test compiles with
/// `cfg(test)` set, so `on_stripped` and its `CfgStripped` kind are absent here
/// while `on_present` and `on_kept` survive. The dispatch arm, the
/// `HandlerCapability` row, and the `measured_kinds` id derived from a stripped
/// handler each name a type and a method that do not exist in this build, so
/// leaking any one of them fails compilation before this test can run.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.cfg_kept")]
struct CfgKept {
    tag: u32,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.cfg_present")]
struct CfgPresent {
    tag: u32,
}

#[cfg(not(test))]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.cfg_stripped")]
struct CfgStripped {
    tag: u32,
}

struct CfgGatedCap {
    seen: AtomicU32,
}

#[aether_actor::actor]
impl NativeActor for CfgGatedCap {
    type Config = ();
    const NAMESPACE: &'static str = "test.macro_native_actor.cfg_gated";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { seen: AtomicU32::new(0) })
    }

    #[aether_actor::handler::single]
    fn on_kept(&self, _ctx: &mut NativeCtx<'_>, mail: CfgKept) {
        self.seen.fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }

    #[aether_actor::handler::single]
    #[cfg(test)]
    fn on_present(&self, _ctx: &mut NativeCtx<'_>, mail: CfgPresent) {
        self.seen.fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }

    #[aether_actor::handler::single]
    #[cfg(not(test))]
    fn on_stripped(&self, _ctx: &mut NativeCtx<'_>, mail: CfgStripped) {
        self.seen.fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }
}

/// Tripwire: a `#[cfg]`-stripped handler contributes no measured kind and no
/// advertised capability, while a `#[cfg]`-satisfied one contributes both, and
/// the survivors keep their declaration order.
///
/// The ids are content-derived — `Kind::ID` hashes the kind's declared name —
/// and the cost table keys rows on `(MailboxId, KindId)`, so a stripped handler
/// removes exactly its own row rather than shifting its siblings. Pinning the
/// exact vector is what would catch a future change that renumbered or reordered
/// them per configuration, which would silently mislabel cost data compared
/// across builds.
#[test]
fn a_cfg_gated_handler_leaves_no_dispatch_artifact() {
    use aether_data::Kind;
    use aether_substrate::actor::native::Dispatch;

    let measured = <CfgGatedCap as Dispatch<CfgGatedCap>>::measured_kinds();
    let advertised: Vec<_> =
        <CfgGatedCap as Dispatch<CfgGatedCap>>::capabilities().handlers.iter().map(|h| h.id).collect();

    let expected = vec![<CfgKept as Kind>::ID, <CfgPresent as Kind>::ID];
    assert_eq!(measured, expected, "a stripped handler contributes no measured kind and reorders no survivor");
    assert_eq!(advertised, expected, "a stripped handler contributes no advertised capability either");
}

/// ADR-0183: the same contract on the sibling macro. A `#[handler_set]`
/// handler's `#[cfg]` governs every artifact the set derives from it, and the
/// crate that *defines* the set is the one whose configuration decides.
///
/// A native set produces one family more than the `#[actor]` block above: past
/// the dispatch arm and the `HandlerCapability` row, it hands each adopter an
/// `impl HandlesKind<K>` through a `#[macro_export] macro_rules!` bridge that
/// expands in the adopter's crate. A `#[cfg]` replayed into that body would be
/// read against the adopter's features while the other two answered to the
/// definer's, so the bridge resolves its gate here at definition time instead.
/// The two halves have to agree: `HandlesKind<K>` is the permission that lets a
/// typed send compile, so a marker without a matching arm is mail that compiles
/// and is dropped.
///
/// This is where that pair is provable. A native set's expansion names
/// `::aether_substrate` types, so asserting it from `aether-actor-derive`'s
/// trybuild fixtures would mean dev-depending on the substrate from a
/// proc-macro crate and pulling the wasmtime and cranelift tree into its UI
/// tests. This file already builds the substrate and already hosts the
/// `#[actor]` half of the contract, so the set half is checked against the real
/// types beside it.
///
/// The configuration is the same one: an integration test compiles with
/// `cfg(test)` set, so `on_set_stripped` and its `SetCfgStripped` kind are
/// absent from this build while `on_set_present` and `on_set_kept` survive.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.set_cfg_kept")]
struct SetCfgKept {
    tag: u32,
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.set_cfg_present")]
struct SetCfgPresent {
    tag: u32,
}

#[cfg(not(test))]
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.set_cfg_stripped")]
struct SetCfgStripped {
    tag: u32,
}

#[aether_actor::handler_set]
trait CfgGatedSet {
    fn seen(&self) -> &AtomicU32;

    #[aether_actor::handler::single]
    fn on_set_kept(&self, _ctx: &mut NativeCtx<'_>, mail: SetCfgKept) {
        self.seen().fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }

    #[aether_actor::handler::single]
    #[cfg(test)]
    fn on_set_present(&self, _ctx: &mut NativeCtx<'_>, mail: SetCfgPresent) {
        self.seen().fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }

    #[aether_actor::handler::single]
    #[cfg(not(test))]
    fn on_set_stripped(&self, _ctx: &mut NativeCtx<'_>, mail: SetCfgStripped) {
        self.seen().fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }
}

struct CfgGatedSetAdopter {
    seen: AtomicU32,
}

impl CfgGatedSet for CfgGatedSetAdopter {
    fn seen(&self) -> &AtomicU32 {
        &self.seen
    }
}

#[aether_actor::actor(handler_set(CfgGatedSet))]
impl NativeActor for CfgGatedSetAdopter {
    type Config = ();
    const NAMESPACE: &'static str = "test.macro_native_actor.cfg_gated_set";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { seen: AtomicU32::new(0) })
    }
}

/// Reads the marker the set's bridge handed the adopter. The bound is
/// satisfiable only if the bridge emitted `impl HandlesKind<K> for A`, so
/// naming a pair here is an assertion that the marker exists, checked where a
/// typed send would check it.
fn adopter_holds_set_marker<K: Kind, A: aether_actor::HandlesKind<K>>() {}

/// Tripwire: a `#[cfg]`-stripped set handler leaves no dispatch artifact in an
/// adopter — no arm, no capability row, no marker — and a `#[cfg]`-satisfied
/// one still contributes all three.
///
/// The two directions are caught by different mechanisms, and both are needed.
/// *Leaking* is a compile error in every family: an arm, a capability row, or a
/// marker built from `on_set_stripped` names `SetCfgStripped` and a method,
/// neither of which exists in this configuration, so it fails the build before
/// this test runs.
///
/// *Over-stripping* compiles, so each family is checked on its own terms — a
/// gate whose predicate and negation did not partition the configurations, or
/// an attribute list applied to the wrong artifact, would quietly drop a
/// handler the set should have kept. The capability row is the two
/// `assert_eq!`s, read through both projections an adopter exposes it through:
/// `capabilities()` and `measured_kinds()`. The marker is
/// `adopter_holds_set_marker`, a compile-time check rather than an assertion —
/// the bound is satisfiable only if the bridge emitted `impl HandlesKind<K>`,
/// and a marker has no run-time value to compare. The arm is neither: nothing
/// the other two read changes when the arm alone goes missing, so it is checked
/// by behaviour, driving a `SetCfgPresent` through the set's dispatch and
/// requiring the handler's counter to move. Without that last check the hazard
/// this block opened with is the one that stays invisible — an adopter that
/// advertises the kind, measures it, and type-checks the send still drops the
/// mail.
#[test]
fn a_cfg_gated_set_handler_leaves_no_dispatch_artifact_in_an_adopter() {
    use aether_substrate::actor::native::Dispatch;

    let measured = <CfgGatedSetAdopter as Dispatch<CfgGatedSetAdopter>>::measured_kinds();
    let advertised: Vec<_> =
        <CfgGatedSetAdopter as Dispatch<CfgGatedSetAdopter>>::capabilities().handlers.iter().map(|h| h.id).collect();

    let expected = vec![<SetCfgKept as Kind>::ID, <SetCfgPresent as Kind>::ID];
    assert_eq!(measured, expected, "an adopter measures the set's surviving kinds and nothing the set stripped");
    assert_eq!(advertised, expected, "and advertises exactly those, in the set's declaration order");

    adopter_holds_set_marker::<SetCfgKept, CfgGatedSetAdopter>();
    adopter_holds_set_marker::<SetCfgPresent, CfgGatedSetAdopter>();

    let (_registry, mailer) = bare_substrate();
    let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0x1850_0002)));
    let mut adopter = CfgGatedSetAdopter { seen: AtomicU32::new(0) };
    let mut ctx: NativeCtx<'_, Manual> = NativeCtx::new_for_actor(&binding, Source::NONE, MailId::NONE, MailId::NONE);

    let handled = <CfgGatedSetAdopter as CfgGatedSet>::__aether_handler_set_dispatch(
        &mut adopter,
        &mut ctx,
        <SetCfgPresent as Kind>::ID,
        &SetCfgPresent { tag: 5 }.encode_into_bytes(),
    );
    assert_eq!(handled, aether_actor::DISPATCH_HANDLED, "the set's arm for a satisfied gate claims its own kind");
    assert_eq!(adopter.seen.load(AtomicOrdering::SeqCst), 5, "and runs the handler behind it on the adopter's state");
}

/// An actor with no task handler measures exactly what it advertises, so the
/// wider set is not a blanket widening.
#[test]
fn an_actor_without_task_handlers_measures_exactly_its_advertised_kinds() {
    use aether_substrate::actor::native::Dispatch;

    let measured = <MacroProbeCap as Dispatch<MacroProbeCap>>::measured_kinds();
    let advertised: Vec<_> =
        <MacroProbeCap as Dispatch<MacroProbeCap>>::capabilities().handlers.iter().map(|h| h.id).collect();
    assert_eq!(measured, advertised, "no task handler, no extra measured kind");
}

// ADR-0109 §5: the `#[actor]` macro submits a process-global link-time
// `HandlerEntry` for each native `#[handler]`, carrying the owning
// `NAMESPACE`, the input kind (id + name), and the reply kind read off
// the return type. This is the native analogue of the wasm
// `aether.kinds.inputs` custom section — the `aether.inventory.handlers`
// query projects it onto the wire.

/// Reply kind for the link-time native-handler-manifest macro test.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.pong")]
struct Pong {
    echoed: u32,
}

/// A cap with a synchronous `-> R` handler. Its `#[actor]` expansion
/// submits a link-time `HandlerEntry` declaring `Greet -> Pong`.
struct ReplyMacroCap;

struct ReplyParentA;

impl Addressable for ReplyParentA {
    const NAMESPACE: &'static str = "test.macro_native_actor.parent_a";
    type Resolver = aether_actor::One;
}

struct ReplyParentB;

impl Addressable for ReplyParentB {
    const NAMESPACE: &'static str = "test.macro_native_actor.parent_b";
    type Resolver = aether_actor::One;
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for ReplyMacroCap {
    type Config = ();
    const NAMESPACE: &'static str = "test.macro_native_actor.reply";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    /// A synchronous `-> Pong` handler — the reply contract ADR-0109
    /// captures from the return type. Stateless: the link-time
    /// `HandlerEntry` (not handler behaviour) is what this cap exists to
    /// exercise.
    #[allow(clippy::unused_self)]
    #[aether_actor::handler::single]
    fn on_greet_reply(&self, _ctx: &mut NativeCtx<'_>, mail: Greet) -> Pong {
        Pong { echoed: mail.tag }
    }
}

/// An instanced child with two legal native parents. It keeps child-placement
/// inventory independent from the singleton root that owns the reply handler.
struct InstancedChildCap;

#[aether_actor::actor(instanced, child_of(ReplyParentA), child_of(ReplyParentB))]
impl NativeActor for InstancedChildCap {
    type Config = ();
    const NAMESPACE: &'static str = "test.macro_native_actor.instanced_child";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[allow(clippy::unused_self)]
    #[aether_actor::handler::single]
    fn on_greet(&self, _ctx: &mut NativeCtx<'_>, _mail: Greet) {}
}

/// ADR-0109 §5: the macro emits a link-time `HandlerEntry` for each
/// native `#[handler]`, carrying the owning `NAMESPACE`, the input kind
/// (id + name), and the reply kind read off the return type. A `-> Pong`
/// handler surfaces `Greet -> Pong`; the round-trip reads the same entry
/// back out of the process-global inventory.
#[test]
fn macro_emits_native_handler_reply_manifest() {
    use aether_data::name_inventory::handler_entries;
    // Reference the cap so its `#[actor]` HandlerEntry submission links
    // into this test binary.
    assert_eq!(ReplyMacroCap::NAMESPACE, "test.macro_native_actor.reply");

    let entry = handler_entries()
        .find(|e| e.namespace == ReplyMacroCap::NAMESPACE && e.id == <Greet as Kind>::ID)
        .expect("the macro should submit a HandlerEntry for the Greet -> Pong handler");
    assert_eq!(entry.name, <Greet as Kind>::NAME, "input kind name round-trips");
    assert_eq!(
        entry.reply,
        Some(<Pong as Kind>::ID),
        "the `-> Pong` return type is captured as the reply contract (In -> Out)",
    );
}

#[test]
fn macro_emits_native_singleton_root_inventory_from_actor_type() {
    use aether_data::ActorId;
    use aether_data::name_inventory::root_entries;

    fn root<T: aether_actor::Root>() {}
    root::<ReplyMacroCap>();

    let root = root_entries()
        .find(|entry| entry.namespace == ReplyMacroCap::NAMESPACE)
        .expect("the macro should submit a RootEntry");
    assert_eq!(root.actor, ActorId::singleton(ReplyMacroCap::NAMESPACE));
}

#[test]
fn macro_emits_native_instanced_child_inventory_from_actor_types() {
    use aether_data::ActorId;
    use aether_data::name_inventory::child_entries;

    fn child_a<T: aether_actor::ChildOf<ReplyParentA>>() {}
    fn child_b<T: aether_actor::ChildOf<ReplyParentB>>() {}
    child_a::<InstancedChildCap>();
    child_b::<InstancedChildCap>();

    let mut parents = child_entries()
        .filter(|entry| entry.child_namespace == InstancedChildCap::NAMESPACE)
        .map(|entry| {
            assert_eq!(entry.child, ActorId::singleton(InstancedChildCap::NAMESPACE));
            assert_eq!(entry.parent, ActorId::singleton(entry.parent_namespace));
            entry.parent_namespace
        })
        .collect::<Vec<_>>();
    parents.sort_unstable();
    assert_eq!(parents, vec![ReplyParentA::NAMESPACE, ReplyParentB::NAMESPACE]);
}

/// An instanced cap that declares `root`. It is placeable at the chassis root
/// and, being instanced, anchors no abbreviated address — the two halves of
/// `root` that iamacoffeepot/aether#4121 separates.
struct InstancedRootCap;

#[aether_actor::actor(instanced, root)]
impl NativeActor for InstancedRootCap {
    type Config = ();
    const NAMESPACE: &'static str = "test.macro_native_actor.instanced_root";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    /// Present only so the identity carries a handler like any other cap; the
    /// link-time placement facts are what this fixture exists to exercise.
    #[allow(clippy::unused_self)]
    #[aether_actor::handler::single]
    fn on_greet(&self, _ctx: &mut NativeCtx<'_>, _mail: Greet) {}
}

/// ADR-0166 §1/§5, issue 4121: `root` grants placement permission and, for a
/// singleton, the right to anchor an abbreviated address. Those are two
/// artifacts — the `Root` impl and the `RootEntry` — and an instanced actor
/// gets only the first, because an instanced namespace identifies no single
/// actor and so anchors nothing.
///
/// Tripwire: the assertion is on link-time inventory the macro computes from
/// cardinality, so re-coupling the two artifacts fails here rather than as a
/// runtime warn-and-exclude inside `AddressIndex::build` — which is where the
/// mismatch used to surface, in a subsystem the declaration never mentions.
#[test]
fn instanced_root_takes_placement_without_claiming_an_address_anchor() {
    use aether_data::name_inventory::root_entries;

    // Placement: the bound the chassis spawn surfaces ask for still holds.
    fn root<T: aether_actor::Root>() {}
    root::<InstancedRootCap>();

    // Anchoring: no `RootEntry`, so the address index never sees a root it
    // would have to exclude.
    assert!(
        !root_entries().any(|entry| entry.namespace == InstancedRootCap::NAMESPACE),
        "an instanced root must submit no RootEntry — it cannot anchor an abbreviated address"
    );
}

// ADR-0109 deferred reply contract (#1805): a `-> Pending<R>` request
// handler plus a borrow-form `#[handler(task)]` completion. `&TaskDone ->
// R` has the macro send `R` via `resolve_value`; `&TaskDone -> ()` has it
// release the hold via `release_no_reply` with no reply. The fixtures
// below drive both through a real chassis and observe what routes back to
// a registered caller inbox.

/// Forward every dispatched envelope onto `tx` so a test can observe the
/// reply (or its absence) on a registered caller mailbox.
fn forward_to(tx: mpsc::Sender<OwnedDispatch>) -> Arc<dyn InboxHandler> {
    Arc::new(move |dispatch: OwnedDispatch| {
        // ADR-0094: terminal test consumer — discharge before the value is
        // forwarded for the test to observe.
        dispatch.discharge();
        let _ = tx.send(dispatch);
    })
}

/// Like [`push_envelope`] but stamps an explicit `reply_to` [`Source`] so
/// the handler's deferred reply has somewhere to route — a registered
/// caller inbox the test reads back.
fn push_envelope_replying_to<K: Kind>(registry: &Registry, recipient: &str, payload: &K, reply_to: Source) {
    use aether_substrate::mail::registry::MailboxEntry;
    let id: MailboxId = registry.lookup(recipient).expect("mailbox registered");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("entry exists") else {
        panic!("expected mailbox entry under {recipient}");
    };
    let bytes = payload.encode_into_bytes();
    handler.enqueue(OwnedDispatch::disarmed(
        <K as Kind>::ID,
        None,
        reply_to,
        MailRef::from(bytes),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId(0),
    ));
}

/// Trigger for the reply path: makes the cap dispatch an `EchoReply`
/// worker behind a `-> Pending<EchoReply>` request handler.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, ::aether_data::Kind, ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.kick_p")]
struct KickP {
    seed: u64,
}

/// Trigger for the no-reply path: makes the cap dispatch a `Silent`
/// worker whose completion releases the hold without replying.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable, ::aether_data::Kind, ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.kick_s")]
struct KickS {
    seed: u64,
}

/// The deferred reply kind — what the `&TaskDone -> EchoReply` completion
/// returns and the macro sends via `resolve_value`. Structured-shape so the
/// reply (wire-encoded by `Mailer::send_reply`) round-trips through
/// `EchoReply::decode_from_bytes`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.echo_reply")]
struct EchoReply {
    value: u64,
}

/// The no-reply path's worker output — a distinct output type from
/// `EchoReply` so the two task handlers route by output type. Never
/// replied; the completion records it and releases.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, ::aether_data::Kind, ::aether_data::Schema)]
#[kind(name = "test.macro_native_actor.silent")]
struct Silent {
    value: u64,
}

/// Where the deferred-reply cap records what its completion handlers
/// observed, so a test can assert each fired with the right value.
#[derive(Clone)]
struct DeferredObs {
    /// How many times the `&TaskDone -> EchoReply` completion fired.
    echo_calls: Arc<AtomicU32>,
    /// How many times the `&TaskDone -> ()` completion fired.
    silent_calls: Arc<AtomicU32>,
    /// The `value` the no-reply completion saw on its worker output.
    silent_value: Arc<AtomicU64>,
}

impl DeferredObs {
    fn new() -> Self {
        Self {
            echo_calls: Arc::new(AtomicU32::new(0)),
            silent_calls: Arc::new(AtomicU32::new(0)),
            silent_value: Arc::new(AtomicU64::new(0)),
        }
    }
}

struct DeferredReplyCap {
    obs: DeferredObs,
}

#[aether_actor::actor(root)]
impl NativeActor for DeferredReplyCap {
    // ADR-0156 §3: the shared deferred-reply counters are construction wiring,
    // not operator config, so they ride the `Params` channel; `Config` is `()`.
    type Config = ();
    type Params = DeferredObs;
    const NAMESPACE: &'static str = "test.macro_native_actor.deferred";

    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { obs: params })
    }

    /// Reply path: `-> Pending<EchoReply>` declares the deferred reply
    /// kind on the request signature and arms an `EchoReply` worker; the
    /// macro sends nothing now.
    #[allow(clippy::unused_self)]
    #[aether_actor::handler::single]
    fn on_kick_p(&self, ctx: &mut NativeCtx<'_>, mail: KickP) -> Pending<EchoReply> {
        let seed = mail.seed;
        ctx.dispatch_blocking(move || EchoReply { value: seed })
    }

    /// Borrow-form completion: returns the reply; the macro calls
    /// `resolve_value` with it and releases the hold.
    #[aether_actor::handler(task)]
    fn on_echo_done(&self, _ctx: &mut NativeCtx<'_>, done: &TaskDone<EchoReply>) -> EchoReply {
        self.obs.echo_calls.fetch_add(1, AtomicOrdering::SeqCst);
        EchoReply { value: done.output().value }
    }

    /// No-reply path: returns `()`, so it dispatches via
    /// `dispatch_blocking_with` (no `Pending<R>` contract to declare).
    #[allow(clippy::unused_self)]
    #[aether_actor::handler::single]
    fn on_kick_s(&self, ctx: &mut NativeCtx<'_>, mail: KickS) {
        let seed = mail.seed;
        ctx.dispatch_blocking_with((), move || Silent { value: seed });
    }

    /// Borrow-form no-reply completion: returns `()`, so the macro calls
    /// `release_no_reply` — the hold releases with no reply sent.
    #[aether_actor::handler(task)]
    fn on_silent_done(&self, _ctx: &mut NativeCtx<'_>, done: &TaskDone<Silent>) {
        self.obs.silent_value.store(done.output().value, AtomicOrdering::SeqCst);
        self.obs.silent_calls.fetch_add(1, AtomicOrdering::SeqCst);
    }
}

/// ADR-0112 manual reply class: input kind for the manual handler.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.manual_ping")]
struct ManualPing {
    seq: u32,
}

/// ADR-0112 manual reply class: the kind the manual handler replies with.
#[repr(C)]
#[derive(
    Copy, Clone, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.manual_ack")]
struct ManualAck {
    seq: u32,
}

/// ADR-0112: a manual-class cap — it receives the `Manual` ctx and issues
/// its own reply by hand via `OutboundReply::reply`, rather than via a
/// `-> R` return value.
struct ManualReplyCap;

#[aether_actor::actor]
impl NativeActor for ManualReplyCap {
    const NAMESPACE: &'static str = "test.macro_native_actor.manual_reply";
    type Config = ();

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[aether_actor::handler::manual]
    #[allow(clippy::unused_self)]
    fn on_ping(&mut self, ctx: &mut NativeCtx<'_, Manual>, ping: ManualPing) {
        ctx.reply(&ManualAck { seq: ping.seq });
    }
}

/// ADR-0112: a `#[handler::manual]` handler receives the `Manual` ctx and
/// replies through `ctx.reply` — drive it through the macro dispatch seam
/// (`new_dispatching` + `__aether_dispatch_envelope`) and assert the ack
/// lands at the caller carrying the declared correlation.
#[test]
fn manual_handler_replies_through_ctx() {
    let (registry, mailer) = bare_substrate();

    let (reply_tx, reply_rx) = mpsc::channel::<OwnedDispatch>();
    let caller =
        registry.register_inbox(&boot_authority(), "test.macro_native_actor.manual_caller", forward_to(reply_tx));

    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(0x1850_0001)));
    let caller_reply_to = Source::with_correlation(SourceAddr::Component(caller), 91);

    let mut cap = ManualReplyCap;
    {
        // ADR-0112: the dispatch seam carries the `Manual` ctx, and issue 4158
        // types it by the dispatching actor — `new_for_actor` builds both,
        // with the mode read off `dispatch`'s own signature.
        let mut ctx = NativeCtx::new_for_actor(&binding, caller_reply_to, MailId::NONE, MailId::NONE);
        let handled = <ManualReplyCap as Dispatch<ManualReplyCap>>::dispatch(
            &mut cap,
            &mut ctx,
            ManualPing::ID,
            &ManualPing { seq: 9 }.encode_into_bytes(),
        );
        assert_eq!(handled, Some(()), "the manual handler ran for its kind");
        // Drop the ctx (flushing buffered outbound) before reading the reply.
    }

    let reply = reply_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the manual handler replied to the inbound sender via ctx.reply");
    assert_eq!(reply.kind, ManualAck::ID, "the reply carries the manual handler's ack kind");
    assert_eq!(reply.sender.correlation_id, 91, "the caller's correlation is echoed onto the manual reply");
    let ack = ManualAck::decode_from_bytes(reply.payload.bytes()).expect("the reply decodes");
    assert_eq!(ack, ManualAck { seq: 9 }, "the manual reply carries the ping seq");
}
