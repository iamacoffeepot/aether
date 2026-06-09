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

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use aether_data::{Kind, Source};
use aether_kinds::trace::Nanos;
use aether_substrate::actor::native::TaskDone;
use aether_substrate::handle_store::HandleStore;
use aether_substrate::mail::registry::OwnedDispatch;
use aether_substrate::mail::{MailId, MailRef};
use aether_substrate::{
    Actor, BootError, Builder, BuiltChassis, Chassis, Mailer, NativeActor, NativeCtx,
    NativeInitCtx, NeverDriver, PassiveChassis, Registry, mail::MailboxId,
};
use std::thread;

/// Postcard-shape kind via the derive — exercises the
/// `decode_from_bytes` postcard path the macro's dispatch arm uses.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.greet")]
struct Greet {
    tag: u32,
}

/// Cast-shape kind so both arms (postcard + cast) get exercised
/// through one cap.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.ping")]
struct Ping {
    seq: u32,
}

struct MacroProbeCap {
    greet_total: Arc<AtomicU32>,
    ping_total: Arc<AtomicU32>,
}

impl aether_actor::Singleton for MacroProbeCap {}

/// Per-cap config — caps without a domain-specific config type
/// would write `()`, but here we thread shared atomic counters in
/// so the test can observe each handler's effect.
#[derive(Clone)]
struct ProbeConfig {
    greet_total: Arc<AtomicU32>,
    ping_total: Arc<AtomicU32>,
}

#[aether_data::actor]
impl NativeActor for MacroProbeCap {
    type Config = ProbeConfig;
    const NAMESPACE: &'static str = "test.macro_native_actor.probe";

    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            greet_total: config.greet_total,
            ping_total: config.ping_total,
        })
    }

    /// Handles postcard-shape `Greet` mail.
    #[aether_data::handler]
    fn on_greet(&self, _ctx: &mut NativeCtx<'_>, mail: Greet) {
        self.greet_total.fetch_add(mail.tag, AtomicOrdering::SeqCst);
    }

    /// Handles cast-shape `Ping` mail.
    #[aether_data::handler]
    fn on_ping(&self, _ctx: &mut NativeCtx<'_>, mail: Ping) {
        self.ping_total.fetch_add(mail.seq, AtomicOrdering::SeqCst);
    }
}

/// Test chassis fixture so `Builder::<C>::new` has a typed `C` to
/// parameterise on. `NeverDriver` makes the no-driver passive build
/// path the only valid one.
struct TestChassis;
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("TestChassis is built directly via Builder in tests");
    }
}

fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    {
        let registry = Arc::new(Registry::new());
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
        (registry, mailer)
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
        K::NAME.to_owned(),
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
fn macro_emitted_cap_routes_postcard_kind_through_dispatch() {
    let (registry, mailer) = fresh_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<MacroProbeCap>(ProbeConfig {
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

    let (registry, mailer) = fresh_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<MacroProbeCap>(ProbeConfig {
                greet_total: Arc::clone(&greet_total),
                ping_total: Arc::clone(&ping_total),
            })
            .build_passive()
            .expect("macro-emitted cap boots");

    let id = registry
        .lookup(MacroProbeCap::NAMESPACE)
        .expect("cap mailbox registered");

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
        assert!(
            Instant::now() < deadline,
            "Pooled slot should quiesce to Idle and become seizable"
        );
        thread::sleep(Duration::from_millis(5));
    };
    // The seize put the slot in `Running`.
    assert_eq!(seize.state().current(), SlotStateLabel::Running);

    // Build one seed envelope and dispatch it in place — no inbox bounce.
    let payload = Greet { tag: 11 }.encode_into_bytes();
    let seed = OwnedDispatch::disarmed(
        <Greet as Kind>::ID,
        Greet::NAME.to_owned(),
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
    assert_eq!(
        greet_total.load(AtomicOrdering::SeqCst),
        11,
        "the seed dispatched through on_greet in place"
    );
    assert_eq!(ping_total.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(
        seize.state().current(),
        SlotStateLabel::Idle,
        "the slot drained empty and returned to Idle"
    );

    drop(chassis);
}

#[test]
fn macro_emitted_cap_routes_cast_kind_through_dispatch() {
    let (registry, mailer) = fresh_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<MacroProbeCap>(ProbeConfig {
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
    Copy,
    Clone,
    Debug,
    PartialEq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.unknown")]
struct Unknown {
    payload: u32,
}

#[test]
fn macro_emitted_cap_drops_unknown_kind_via_dispatch() {
    let (registry, mailer) = fresh_substrate();
    let greet_total = Arc::new(AtomicU32::new(0));
    let ping_total = Arc::new(AtomicU32::new(0));

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<MacroProbeCap>(ProbeConfig {
                greet_total: Arc::clone(&greet_total),
                ping_total: Arc::clone(&ping_total),
            })
            .build_passive()
            .expect("macro-emitted cap boots");

    push_envelope(
        &registry,
        MacroProbeCap::NAMESPACE,
        &Unknown { payload: 99 },
    );

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
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.result_a")]
struct ResultA {
    value: u64,
}

/// Second completion output type — a structurally different shape so a
/// mis-route to the `ResultA` handler couldn't accidentally type-check.
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.result_b")]
struct ResultB {
    tag: u32,
}

/// Trigger that makes the cap dispatch a `ResultA`-producing worker.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.kick_a")]
struct KickA {
    seed: u64,
}

/// Trigger that makes the cap dispatch a `ResultB`-producing worker.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    ::aether_data::Kind,
    ::aether_data::Schema,
)]
#[kind(name = "test.macro_native_actor.kick_b")]
struct KickB {
    seed: u32,
}

/// Where each task handler records what it observed, so the test can
/// assert routing landed on the correct handler with the correct value.
#[derive(Clone)]
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

impl aether_actor::Singleton for TaskRouteCap {}

#[aether_data::actor]
impl NativeActor for TaskRouteCap {
    type Config = TaskObservations;
    const NAMESPACE: &'static str = "test.macro_native_actor.task_route";

    fn init(config: Self::Config, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { obs: config })
    }

    /// Dispatch a worker that produces a `ResultA`. The completion routes
    /// to `on_result_a` by output type.
    #[aether_data::handler]
    fn on_kick_a(&self, ctx: &mut NativeCtx<'_>, mail: KickA) {
        self.obs.dispatched.fetch_add(1, AtomicOrdering::SeqCst);
        let seed = mail.seed;
        ctx.dispatch_blocking(move || ResultA { value: seed });
    }

    /// Dispatch a worker that produces a `ResultB`.
    #[aether_data::handler]
    fn on_kick_b(&self, ctx: &mut NativeCtx<'_>, mail: KickB) {
        self.obs.dispatched.fetch_add(1, AtomicOrdering::SeqCst);
        let seed = mail.seed;
        ctx.dispatch_blocking(move || ResultB { tag: seed });
    }

    /// `ResultA` completion handler. Records the value + a call so the
    /// test can confirm only the `ResultA` dispatch reached it.
    #[aether_data::handler(task)]
    fn on_result_a(&self, ctx: &mut NativeCtx<'_>, done: TaskDone<ResultA>) {
        self.obs
            .a_value
            .store(done.output().value, AtomicOrdering::SeqCst);
        self.obs.a_calls.fetch_add(1, AtomicOrdering::SeqCst);
        // No caller behind these test dispatches (Source::NONE), so the
        // re-reply is a no-op; resolve still consumes the TaskDone and
        // releases the hold (avoiding the drop-without-resolve assert).
        done.resolve(ctx);
    }

    /// `ResultB` completion handler.
    #[aether_data::handler(task)]
    fn on_result_b(&self, ctx: &mut NativeCtx<'_>, done: TaskDone<ResultB>) {
        self.obs
            .b_tag
            .store(done.output().tag, AtomicOrdering::SeqCst);
        self.obs.b_calls.fetch_add(1, AtomicOrdering::SeqCst);
        done.resolve(ctx);
    }
}

#[test]
fn macro_routes_task_completions_by_output_type() {
    let (registry, mailer) = fresh_substrate();
    let obs = TaskObservations {
        dispatched: Arc::new(AtomicU32::new(0)),
        a_value: Arc::new(AtomicU64::new(0)),
        a_calls: Arc::new(AtomicU32::new(0)),
        b_tag: Arc::new(AtomicU32::new(0)),
        b_calls: Arc::new(AtomicU32::new(0)),
    };

    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
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

    assert!(
        wait_for(1, &obs.a_calls, Duration::from_secs(2)),
        "the ResultA completion routed to on_result_a"
    );
    assert!(
        wait_for(1, &obs.b_calls, Duration::from_secs(2)),
        "the ResultB completion routed to on_result_b"
    );

    // Each completion landed on the correct handler with the correct
    // payload — output-type routing, not a kind id.
    assert_eq!(
        obs.a_value.load(AtomicOrdering::SeqCst),
        7,
        "on_result_a saw its own dispatch's value"
    );
    assert_eq!(
        obs.b_tag.load(AtomicOrdering::SeqCst),
        9,
        "on_result_b saw its own dispatch's tag"
    );

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
    assert_eq!(
        obs.b_calls.load(AtomicOrdering::SeqCst),
        1,
        "on_result_b fired exactly once"
    );
    assert_eq!(
        obs.dispatched.load(AtomicOrdering::SeqCst),
        2,
        "both kick handlers dispatched a worker"
    );

    drop(chassis);
}
