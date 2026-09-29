//! ADR-0243 §9 staged work, driven through a real chassis: every mail below
//! reaches its actor through the production dispatcher, and every completion
//! through the wake its worker pushes.
//!
//! A staged task owes no reply, takes its chain in the turn that stages it,
//! and completes correlated to its request on that chain. The bounded queue
//! built on it holds each request's reply itself, so a request's chain
//! settles when that request is answered.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use aether_actor::{ErasedActorRef, HeldReply};
use aether_data::{Kind, RequestId, Source, SourceAddr};
use aether_substrate::actor::native::{Held, Pending, SpawnOutcome, StagedTask, TaskDone, TaskQueue};
use aether_substrate::mail::MailRef;
use aether_substrate::mail::registry::{DispatchParts, MailboxEntry, OwnedDispatch};
use aether_substrate::runtime::lifecycle::{FatalAbortRecord, FatalAborter, PanicAborter, RecordingAborter};
use aether_substrate::testing::{TestChassis, bare_substrate, boot_test_chassis_aborting_into, boot_test_chassis_with};
use aether_substrate::{BootError, NativeActor, NativeCtx, NativeInitCtx, PassiveChassis, Registry, Subname};

/// How long a wait that must succeed may take before the test fails.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long a test watches for something that must not happen.
const QUIET: Duration = Duration::from_millis(200);

/// Gates a test opens by number; a worker passing a closed gate blocks until
/// the test opens it, so the test decides when each task finishes.
#[derive(Clone, Default)]
struct Gates(Arc<(Mutex<HashSet<u32>>, Condvar)>);

impl Gates {
    fn open(&self, gate: u32) {
        let (open, opened) = &*self.0;
        open.lock().expect("gates lock").insert(gate);
        opened.notify_all();
    }

    fn pass(&self, gate: u32) {
        let (open, opened) = &*self.0;
        let open = open.lock().expect("gates lock");
        drop(opened.wait_while(open, |open| !open.contains(&gate)).expect("gates lock"));
    }
}

/// A unit of queued work; its worker passes `gate` before it finishes.
#[aether_data::kind(name = "test.staged_task.work", copy)]
struct Work {
    gate: u32,
}

/// The reply a queued unit of work is answered with.
#[aether_data::kind(name = "test.staged_task.worked", copy)]
struct Worked {
    gate: u32,
}

// A sentinel: no test here closes an actor while it still owes a `Worked`.
impl HeldReply for Worked {
    fn unanswered() -> Self {
        Self { gate: u32::MAX }
    }
}

/// The wiring a [`QueueProbe`] reports through: the gates its workers pass,
/// and the channel each worker announces its start on.
#[derive(Clone)]
struct QueueParams {
    gates: Gates,
    started: Sender<u32>,
}

/// An actor that runs every [`Work`] through a one-slot [`TaskQueue`].
struct QueueProbe {
    tasks: TaskQueue<Worked>,
    params: QueueParams,
}

#[aether_actor::actor(root)]
impl NativeActor for QueueProbe {
    type Config = ();
    type Params = QueueParams;
    const NAMESPACE: &'static str = "test.staged_task.queue";

    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { tasks: TaskQueue::new(1), params })
    }

    #[aether_actor::handler::single]
    fn on_work(&mut self, ctx: &mut NativeCtx<'_>, work: Work) -> Pending<Worked> {
        let QueueParams { gates, started } = self.params.clone();
        self.tasks.submit(ctx, move || {
            let _ = started.send(work.gate);
            gates.pass(work.gate);
            Worked { gate: work.gate }
        })
    }

    #[aether_actor::handler(task)]
    fn on_worked(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<Worked>) {
        self.tasks.complete(ctx, done);
    }
}

/// Catches a queue that starts a waiting request's work in the turn that
/// frees its slot, on that turn's chain: the answered request would then
/// stay unsettled until the next request's work finished.
#[test]
fn a_queued_request_does_not_hold_the_chain_of_the_one_that_frees_its_slot() {
    let (registry, mailer) = bare_substrate();
    let gates = Gates::default();
    let (started_tx, started) = mpsc::channel();
    let chassis = boot_test_chassis_with::<QueueProbe>(
        &registry,
        &mailer,
        (),
        QueueParams { gates: gates.clone(), started: started_tx },
    );
    let queue = chassis.actor_ref::<QueueProbe>();

    // Both requests are in the inbox before the first worker can finish, so
    // the second is queued behind the first when the first's completion runs.
    let (_, first_settled) = chassis.send_tracked(queue, &Work { gate: 1 }, None);
    let (_, second_settled) = chassis.send_tracked(queue, &Work { gate: 2 }, None);
    assert_eq!(started.recv_timeout(PATIENCE), Ok(1), "the first request takes the one slot");
    gates.open(1);

    assert_eq!(started.recv_timeout(PATIENCE), Ok(2), "the first completion starts the queued request");
    first_settled.recv_timeout(PATIENCE).expect("the first request settles once it is answered");
    assert!(
        second_settled.recv_timeout(QUIET).is_err(),
        "the queued request's chain is held while its worker is blocked"
    );

    gates.open(2);
    second_settled.recv_timeout(PATIENCE).expect("the queued request settles once it is answered");
    drop(chassis);
}

/// A staged task's context: which step it is, and a value that rides it.
#[aether_data::kind(name = "test.staged_task.note", copy, partial_eq)]
struct Note {
    step: u32,
    value: u32,
}

/// A staged task's output.
#[aether_data::kind(name = "test.staged_task.step", copy)]
struct Step {
    step: u32,
}

/// A context holding a reply the handler owes, which the completion never
/// takes.
#[aether_data::kind(name = "test.staged_task.stranded")]
struct Stranded {
    held: Held<Worked>,
}

/// Stage a two-step chain: the first step's completion stages the second.
#[aether_data::kind(name = "test.staged_task.chain", copy)]
struct Chain {
    value: u32,
}

/// Stage a task whose context strands a held reply.
#[aether_data::kind(name = "test.staged_task.strand", copy)]
struct Strand;

/// Stage a task and keep it in state, unstarted.
#[aether_data::kind(name = "test.staged_task.stage", copy)]
struct Stage {
    value: u32,
}

/// Drop every unstarted task the actor keeps.
#[aether_data::kind(name = "test.staged_task.discard", copy)]
struct Discard;

/// Delivered as a reply to a staged request: report whether that request's
/// context is still stored.
#[aether_data::kind(name = "test.staged_task.probe", copy)]
struct Probe;

/// Close the actor.
#[aether_data::kind(name = "test.staged_task.close", copy)]
struct Close;

/// What a [`StageProbe`] saw.
#[derive(Debug, PartialEq)]
enum Seen {
    /// A task was staged under this request.
    Staged(RequestId),
    /// A completion ran.
    Completed { step: u32, in_reply_to: Option<RequestId>, note: Option<Note>, sender: bool },
    /// A probe found the context stored under its request, or not.
    Probed(bool),
    /// The actor began closing.
    Closing,
}

#[derive(Clone)]
struct StageParams {
    gates: Gates,
    seen: Sender<Seen>,
}

/// An actor that stages tasks directly, to show what a staged task's
/// completion sees and what an unstarted one leaves behind.
struct StageProbe {
    params: StageParams,
    unstarted: Vec<StagedTask<Step>>,
}

impl StageProbe {
    fn see(&self, seen: Seen) {
        let _ = self.params.seen.send(seen);
    }
}

#[aether_actor::actor(root)]
impl NativeActor for StageProbe {
    type Config = ();
    type Params = StageParams;
    const NAMESPACE: &'static str = "test.staged_task.stage_probe";

    fn init((): (), params: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { params, unstarted: Vec::new() })
    }

    #[aether_actor::handler::single]
    fn on_chain(&mut self, ctx: &mut NativeCtx<'_>, chain: Chain) {
        let task = ctx.stage_blocking_with::<Step, Note>(Note { step: 1, value: chain.value });
        self.see(Seen::Staged(task.request()));
        task.start(ctx, || Step { step: 1 });
    }

    #[aether_actor::handler::single]
    fn on_strand(&mut self, ctx: &mut NativeCtx<'_>, _strand: Strand) -> Pending<Worked> {
        let (pending, held) = ctx.hold::<Worked>();
        let task = ctx.stage_blocking_with::<Step, Stranded>(Stranded { held });
        self.see(Seen::Staged(task.request()));
        task.start(ctx, || Step { step: 0 });
        pending
    }

    #[aether_actor::handler::single]
    fn on_stage(&mut self, ctx: &mut NativeCtx<'_>, stage: Stage) {
        let task = ctx.stage_blocking_with::<Step, Note>(Note { step: 0, value: stage.value });
        self.see(Seen::Staged(task.request()));
        self.unstarted.push(task);
    }

    #[aether_actor::handler::single]
    fn on_discard(&mut self, _ctx: &mut NativeCtx<'_>, _discard: Discard) {
        self.unstarted.clear();
    }

    #[aether_actor::handler::single]
    fn on_probe(&mut self, ctx: &mut NativeCtx<'_>, _probe: Probe) {
        let stored = ctx.take_context::<Note>().is_some();
        self.see(Seen::Probed(stored));
    }

    #[aether_actor::handler::single]
    fn on_close(&mut self, ctx: &mut NativeCtx<'_>, _close: Close) {
        self.see(Seen::Closing);
        ctx.shutdown();
    }

    /// Record what the completion sees; the first step of a chain stages
    /// the second, gated, from inside its completion.
    #[aether_actor::handler(task)]
    fn on_step(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<Step>) {
        let Step { step } = done.into_output();
        let note = ctx.take_context::<Note>();
        self.see(Seen::Completed { step, in_reply_to: ctx.in_reply_to(), note, sender: ctx.sender().is_some() });

        if step == 1 {
            let gates = self.params.gates.clone();
            let next = ctx.stage_blocking_with::<Step, Note>(Note { step: 2, value: 0 });
            self.see(Seen::Staged(next.request()));
            next.start(ctx, move || {
                gates.pass(2);
                Step { step: 2 }
            });
        }
    }
}

fn boot_stage_probe(
    aborter: Arc<dyn FatalAborter>,
) -> (Arc<Registry>, PassiveChassis<TestChassis>, Gates, Receiver<Seen>) {
    let (registry, mailer) = bare_substrate();
    let gates = Gates::default();
    let (seen_tx, seen) = mpsc::channel();
    let chassis = boot_test_chassis_aborting_into::<StageProbe>(
        &registry,
        &mailer,
        (),
        StageParams { gates: gates.clone(), seen: seen_tx },
        aborter,
    );
    (registry, chassis, gates, seen)
}

fn staged(seen: &Receiver<Seen>) -> RequestId {
    match seen.recv_timeout(PATIENCE).expect("the actor stages a task") {
        Seen::Staged(request) => request,
        other => panic!("expected a staged task, saw {other:?}"),
    }
}

/// Deliver `mail` to `actor` as the reply to `request`, the shape a reply to
/// an outbound request arrives in.
fn deliver_as_reply<K: Kind>(registry: &Registry, actor: ErasedActorRef, request: RequestId, mail: &K) {
    let MailboxEntry::Inbox { handler, .. } = registry.entry(actor).expect("the actor is registered") else {
        panic!("expected an inbox for {actor:?}");
    };
    let sender = Source::with_correlation(SourceAddr::None, request.0);
    let parts = DispatchParts { sender, ..DispatchParts::new(K::ID, MailRef::from(mail.encode_into_bytes())) };
    handler.enqueue(OwnedDispatch::disarmed(parts, actor));
}

/// Catches a completion wake that is not correlated to the task's request
/// (its context would be unreachable), one sent off the staging chain, and a
/// staged hold released before the completion runs: the second step, staged
/// from inside the first step's completion, would then not hold the chain
/// the request started, and the request would settle before it finished.
#[test]
fn a_staged_completion_takes_its_context_and_continues_the_staging_chain() {
    let (_registry, chassis, gates, seen) = boot_stage_probe(Arc::new(PanicAborter));
    let actor = chassis.actor_ref::<StageProbe>();

    let (_, settled) = chassis.send_tracked(actor, &Chain { value: 7 }, None);
    let first = staged(&seen);
    assert_eq!(
        seen.recv_timeout(PATIENCE),
        Ok(Seen::Completed {
            step: 1,
            in_reply_to: Some(first),
            note: Some(Note { step: 1, value: 7 }),
            sender: false
        }),
        "the completion is correlated to its task's request, takes the context staged with it, and names no sender",
    );
    let second = staged(&seen);
    assert_ne!(first, second, "every staged task mints its own request id");
    assert!(settled.recv_timeout(QUIET).is_err(), "the second step holds the chain the request started");

    gates.open(2);
    assert_eq!(
        seen.recv_timeout(PATIENCE),
        Ok(Seen::Completed {
            step: 2,
            in_reply_to: Some(second),
            note: Some(Note { step: 2, value: 0 }),
            sender: false
        }),
    );
    settled.recv_timeout(PATIENCE).expect("the request settles once its last step completes");
    drop(chassis);
}

/// Catches a task completion the ADR-0243 §7 guard misses: a completion that
/// leaves a context holding a live `Held` untaken strands that debt, and must
/// fail fast naming the context kind.
#[test]
fn an_untaken_held_context_on_a_task_completion_fails_fast() {
    let record = Arc::new(FatalAbortRecord::new());
    let aborter = Arc::new(RecordingAborter::new(Arc::new(PanicAborter), Arc::clone(&record)));
    let (_registry, chassis, _gates, _seen) = boot_stage_probe(aborter);

    let _ = chassis.send_tracked(chassis.actor_ref::<StageProbe>(), &Strand, None);

    let _tripped = record.tripwire().recv_timeout(PATIENCE);
    let reason = record.reason().expect("the untaken context reached the chassis aborter");
    assert!(reason.contains(Stranded::NAME), "the failure names the untaken context kind: {reason}");
    drop(chassis);
}

/// Catches an unstarted task whose drop keeps the chain it staged on open,
/// or leaves its context stored: a queue that drops waiting work would then
/// wedge its requests' settlement and leak their contexts.
#[test]
fn dropping_an_unstarted_task_releases_its_chain_and_its_context() {
    let (registry, chassis, _gates, seen) = boot_stage_probe(Arc::new(PanicAborter));
    let actor = chassis.actor_ref::<StageProbe>();

    let (_, first_settled) = chassis.send_tracked(actor, &Stage { value: 1 }, None);
    let first = staged(&seen);
    let (_, second_settled) = chassis.send_tracked(actor, &Stage { value: 2 }, None);
    let second = staged(&seen);
    assert!(first_settled.recv_timeout(QUIET).is_err(), "an unstarted task holds the chain it was staged on");

    // A probe reaches a context that is stored, so a later miss means the
    // context is gone rather than that the probe never finds one.
    deliver_as_reply(&registry, actor.erase(), first, &Probe);
    assert_eq!(seen.recv_timeout(PATIENCE), Ok(Seen::Probed(true)), "the staged context is stored");

    let (_, discarded) = chassis.send_tracked(actor, &Discard, None);
    discarded.recv_timeout(PATIENCE).expect("the discard settles");
    first_settled.recv_timeout(PATIENCE).expect("dropping the task releases the chain it held");
    second_settled.recv_timeout(PATIENCE).expect("dropping the task releases the chain it held");

    deliver_as_reply(&registry, actor.erase(), second, &Probe);
    assert_eq!(seen.recv_timeout(PATIENCE), Ok(Seen::Probed(false)), "dropping the task removed its context");
    drop(chassis);
}

/// Catches an actor close that treats an unstarted task as a lost reply, or
/// strands the chain it holds: a queue closed with work still waiting would
/// then abort the engine, or never let its callers settle.
#[test]
fn an_unstarted_task_is_released_at_actor_close() {
    let record = Arc::new(FatalAbortRecord::new());
    let aborter = Arc::new(RecordingAborter::new(Arc::new(PanicAborter), Arc::clone(&record)));
    let (_registry, chassis, _gates, seen) = boot_stage_probe(aborter);
    let actor = chassis.actor_ref::<StageProbe>();

    let (_, settled) = chassis.send_tracked(actor, &Stage { value: 1 }, None);
    staged(&seen);
    assert!(settled.recv_timeout(QUIET).is_err(), "an unstarted task holds the chain it was staged on");

    let _ = chassis.send_tracked(actor, &Close, None);
    assert_eq!(seen.recv_timeout(PATIENCE), Ok(Seen::Closing), "the close reached the actor");
    settled.recv_timeout(PATIENCE).expect("closing the actor releases the chain its unstarted task held");
    assert_eq!(record.reason(), None, "an unstarted task owes nothing, so its close fails nothing");
    drop(chassis);
}

/// A child birth's context: which birth it is.
#[aether_data::kind(name = "test.staged_task.birth_note", copy, partial_eq)]
struct BirthNote {
    value: u32,
}

/// Stage one child birth under `child-{value}`, with a [`BirthNote`].
#[aether_data::kind(name = "test.staged_task.hatch", copy)]
struct Hatch {
    value: u32,
}

/// Stage one child birth under a subname the grammar refuses, with a
/// [`BirthNote`].
#[aether_data::kind(name = "test.staged_task.hatch_refused", copy)]
struct HatchRefused {
    value: u32,
}

/// What a [`BirthProbe`] saw.
#[derive(Debug, PartialEq)]
enum Born {
    /// A birth was staged under this request.
    Staged(RequestId),
    /// Staging refused the birth and handed back this context.
    Refused(BirthNote),
    /// A birth's completion ran; it drops its `TaskDone` without reading it.
    Completed { in_reply_to: Option<RequestId>, note: Option<BirthNote> },
}

/// A root that stages [`BirthChild`] births with a context and reports what
/// staging and each completion see.
struct BirthProbe {
    seen: Sender<Born>,
}

#[aether_actor::actor(root)]
impl NativeActor for BirthProbe {
    type Config = ();
    type Params = Sender<Born>;
    const NAMESPACE: &'static str = "test.staged_task.birth_probe";

    fn init((): (), seen: Self::Params, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { seen })
    }

    #[aether_actor::handler::single]
    fn on_hatch(&mut self, ctx: &mut NativeCtx<'_>, hatch: Hatch) {
        let subname = format!("child-{}", hatch.value);
        let staged = ctx
            .spawn_child::<BirthChild>(Subname::Named(&subname), (), ())
            .stage_with(BirthNote { value: hatch.value });
        let _ = self
            .seen
            .send(staged.map_or_else(|(_, note)| Born::Refused(note), |receipt| Born::Staged(receipt.request)));
    }

    #[aether_actor::handler::single]
    fn on_hatch_refused(&mut self, ctx: &mut NativeCtx<'_>, hatch: HatchRefused) {
        let staged = ctx
            .spawn_child::<BirthChild>(Subname::Named("not a segment"), (), ())
            .stage_with(BirthNote { value: hatch.value });
        let _ = self
            .seen
            .send(staged.map_or_else(|(_, note)| Born::Refused(note), |receipt| Born::Staged(receipt.request)));
    }

    #[aether_actor::handler(task)]
    fn on_born(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome<BirthChild>>) {
        let note = ctx.take_context::<BirthNote>();
        let _ = self.seen.send(Born::Completed { in_reply_to: ctx.in_reply_to(), note });
        drop(done);
    }
}

/// The child a [`BirthProbe`] stages. Nothing mails it: its one handler is
/// there because an actor declares at least one.
struct BirthChild {
    gate: u32,
}

#[aether_actor::actor(instanced, child_of(BirthProbe))]
impl NativeActor for BirthChild {
    type Config = ();
    const NAMESPACE: &'static str = "test.staged_task.birth_child";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { gate: 0 })
    }

    #[aether_actor::handler::single]
    fn on_work(&self, _ctx: &mut NativeCtx<'_>, work: Work) -> Worked {
        Worked { gate: work.gate.max(self.gate) }
    }
}

fn boot_birth_probe(aborter: Arc<dyn FatalAborter>) -> (PassiveChassis<TestChassis>, Receiver<Born>) {
    let (registry, mailer) = bare_substrate();
    let (seen_tx, seen) = mpsc::channel();
    let chassis = boot_test_chassis_aborting_into::<BirthProbe>(&registry, &mailer, (), seen_tx, aborter);
    (chassis, seen)
}

/// Catches a birth whose completion is not correlated to the request its
/// context was stored under, or whose context is stored under a different
/// id: the completion's take would then find nothing.
#[test]
fn a_birth_staged_with_a_context_completes_with_it() {
    let (chassis, seen) = boot_birth_probe(Arc::new(PanicAborter));

    let _ = chassis.send_tracked(chassis.actor_ref::<BirthProbe>(), &Hatch { value: 7 }, None);
    let Ok(Born::Staged(request)) = seen.recv_timeout(PATIENCE) else {
        panic!("the birth stages");
    };
    assert_eq!(
        seen.recv_timeout(PATIENCE),
        Ok(Born::Completed { in_reply_to: Some(request), note: Some(BirthNote { value: 7 }) }),
        "the completion is correlated to the birth's request and takes the context staged with it",
    );
    drop(chassis);
}

/// Catches a stage that stores its context, or arms its completion, before
/// a synchronous refusal: the caller would lose the context it needs to
/// answer the refusal, and the request's chain would stay held.
#[test]
fn a_refused_birth_hands_its_context_back() {
    let (chassis, seen) = boot_birth_probe(Arc::new(PanicAborter));

    let (_, settled) = chassis.send_tracked(chassis.actor_ref::<BirthProbe>(), &HatchRefused { value: 3 }, None);
    assert_eq!(seen.recv_timeout(PATIENCE), Ok(Born::Refused(BirthNote { value: 3 })));
    settled.recv_timeout(PATIENCE).expect("a refused birth holds nothing, so its request settles");
    drop(chassis);
}

/// Catches a birth armed as a reply its completion owes: dropping the
/// completion's `TaskDone` unread would then fail fast as a lost reply, or
/// leave the staging chain held.
#[test]
fn dropping_a_birth_completion_fails_nothing() {
    let record = Arc::new(FatalAbortRecord::new());
    let aborter = Arc::new(RecordingAborter::new(Arc::new(PanicAborter), Arc::clone(&record)));
    let (chassis, seen) = boot_birth_probe(aborter);

    let (_, settled) = chassis.send_tracked(chassis.actor_ref::<BirthProbe>(), &Hatch { value: 1 }, None);
    assert!(matches!(seen.recv_timeout(PATIENCE), Ok(Born::Staged(_))), "the birth stages");
    assert!(matches!(seen.recv_timeout(PATIENCE), Ok(Born::Completed { .. })), "the completion runs");
    settled.recv_timeout(PATIENCE).expect("the staging chain settles once the completion ends");
    assert_eq!(record.reason(), None, "a birth owes nothing, so dropping its completion fails nothing");
    drop(chassis);
}
