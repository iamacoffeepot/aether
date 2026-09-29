//! ADR-0243 §1 actor close, driven through a real chassis: an actor that
//! closes while the engine keeps running answers every held reply it still
//! owes with the reply kind's `unanswered` value, before it releases that
//! reply's hold, and releases its staged tasks with no reply. An engine
//! teardown settles them all silently, because every requester is closing
//! with it.
//!
//! Every request below reaches the actor through the production dispatcher,
//! and every answer reaches the recording sink through the binding reply
//! path.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::HeldReply;
use aether_data::{Kind, KindId, MailId};
use aether_substrate::actor::native::{Held, Pending, StagedTask};
use aether_substrate::mail::registry::OwnedDispatch;
use aether_substrate::runtime::lifecycle::{FatalAbortRecord, PanicAborter, RecordingAborter};
use aether_substrate::testing::{TestChassis, bare_substrate, boot_test_chassis_aborting_into, registered_ref};
use aether_substrate::{BootError, Mailer, NativeActor, NativeCtx, NativeInitCtx, PassiveChassis, ReplyTarget};

/// How long a wait that must succeed may take before the test fails.
const PATIENCE: Duration = Duration::from_secs(5);

/// How long a test watches for something that must not happen.
const QUIET: Duration = Duration::from_millis(200);

/// The reply every held request below owes.
#[aether_data::kind(name = "test.held_close.owed", copy, partial_eq)]
struct Owed {
    value: u32,
}

impl HeldReply for Owed {
    fn unanswered() -> Self {
        Self { value: u32::MAX }
    }
}

/// Hold the reply and keep the `Held` in actor state.
#[aether_data::kind(name = "test.held_close.keep", copy)]
struct Keep;

/// Hold the reply and park the `Held` in an unstarted task's context.
#[aether_data::kind(name = "test.held_close.stash", copy)]
struct Stash;

/// Stage a task with a plain context and keep it unstarted.
#[aether_data::kind(name = "test.held_close.stage", copy)]
struct Stage;

/// Close the actor.
#[aether_data::kind(name = "test.held_close.close", copy)]
struct Close;

/// A staged task's output; no task below ever starts.
#[aether_data::kind(name = "test.held_close.step", copy)]
struct Step;

/// A staged task's context that carries a held reply, which parks in the
/// ledger while the context is stored.
#[aether_data::kind(name = "test.held_close.stashed")]
struct Stashed {
    held: Held<Owed>,
}

/// A staged task's context with no debt.
#[aether_data::kind(name = "test.held_close.note", copy)]
struct Note {
    value: u32,
}

/// An actor that owes held replies in every shape a close must answer, and
/// keeps an unstarted task that owes nothing.
struct CloseProbe {
    kept: Vec<Held<Owed>>,
    unstarted: Vec<StagedTask<Step>>,
    /// Set by `on_close`: the scenario sends no request after it.
    closing: bool,
}

#[aether_actor::actor(root)]
impl NativeActor for CloseProbe {
    type Config = ();
    const NAMESPACE: &'static str = "test.held_close.probe";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { kept: Vec::new(), unstarted: Vec::new(), closing: false })
    }

    #[aether_actor::handler::single]
    fn on_keep(&mut self, ctx: &mut NativeCtx<'_>, _keep: Keep) -> Pending<Owed> {
        assert!(!self.closing, "the scenario holds every request before it closes the actor");
        let (pending, held) = ctx.hold::<Owed>();
        self.kept.push(held);
        pending
    }

    #[aether_actor::handler::single]
    fn on_stash(&mut self, ctx: &mut NativeCtx<'_>, _stash: Stash) -> Pending<Owed> {
        let (pending, held) = ctx.hold::<Owed>();
        self.unstarted.push(ctx.stage_blocking_with::<Step, Stashed>(Stashed { held }));
        pending
    }

    #[aether_actor::handler::single]
    fn on_stage(&mut self, ctx: &mut NativeCtx<'_>, _stage: Stage) {
        self.unstarted.push(ctx.stage_blocking_with::<Step, Note>(Note { value: 3 }));
    }

    #[aether_actor::handler::single]
    fn on_close(&mut self, ctx: &mut NativeCtx<'_>, _close: Close) {
        self.closing = true;
        ctx.shutdown();
    }
}

/// One reply the sink received: its kind, the correlation it echoes, its
/// payload, and how many holds its chain still had open when it arrived.
#[derive(Debug)]
struct Arrival {
    kind: KindId,
    correlation: u64,
    payload: Vec<u8>,
    held_open: u32,
}

/// A booted [`CloseProbe`] with its three requests sent: `Keep` under
/// correlation 1, `Stash` under 2, and `Stage` under 3, each replying to the
/// recording sink.
struct Scenario {
    chassis: PassiveChassis<TestChassis>,
    mailer: Arc<Mailer>,
    arrivals: Receiver<Arrival>,
    settled: Vec<crossbeam_channel::Receiver<()>>,
    roots: Vec<MailId>,
    record: Arc<FatalAbortRecord>,
}

fn scenario() -> Scenario {
    let (registry, mailer) = bare_substrate();
    let record = Arc::new(FatalAbortRecord::new());
    let aborter = Arc::new(RecordingAborter::new(Arc::new(PanicAborter), Arc::clone(&record)));

    let (tx, arrivals) = mpsc::channel();
    let tx = Mutex::new(tx);
    let arrival_mailer = Arc::clone(&mailer);
    let sink = registered_ref(
        &registry,
        "test.held_close.sink",
        Arc::new(move |dispatch: OwnedDispatch| {
            let held_open =
                dispatch.root.map_or(0, |root| arrival_mailer.trace_handle().settlement_counter().held_open(root));
            // The sink is the answer's terminal consumer: it finishes the
            // answer so the chain it joined can settle.
            arrival_mailer.record_finished(dispatch.mail_id, dispatch.root);
            dispatch.discharge();
            let arrival = Arrival {
                kind: dispatch.kind,
                correlation: dispatch.sender.correlation_id,
                payload: dispatch.payload.bytes().to_vec(),
                held_open,
            };
            let _ = tx.lock().expect("sink lock").send(arrival);
        }),
    );

    let chassis = boot_test_chassis_aborting_into::<CloseProbe>(&registry, &mailer, (), (), aborter);
    let probe = chassis.actor_ref::<CloseProbe>();
    let reply = |correlation| Some(ReplyTarget::Actor { to: sink, correlation });
    let (roots, settled) = [
        chassis.send_tracked(probe, &Keep, reply(1)),
        chassis.send_tracked(probe, &Stash, reply(2)),
        chassis.send_tracked(probe, &Stage, reply(3)),
    ]
    .into_iter()
    .unzip();

    let scenario = Scenario { chassis, mailer, arrivals, settled, roots, record };
    scenario.await_holds();
    scenario
}

impl Scenario {
    /// Wait until each request's hold is in place, so the close finds every
    /// entry armed, and check that nothing answered before the close.
    fn await_holds(&self) {
        let counter = self.mailer.trace_handle().settlement_counter();
        for &root in &self.roots {
            let deadline = Instant::now() + PATIENCE;
            while counter.held_open(root) == 0 {
                assert!(Instant::now() < deadline, "the request's hold never armed");
                thread::yield_now();
            }
        }
        assert!(self.arrivals.recv_timeout(QUIET).is_err(), "nothing answers a held reply before close");
    }
}

/// The two answers a close sends, in correlation order, after checking that
/// nothing arrives for the staged task.
fn answers(arrivals: &Receiver<Arrival>) -> Vec<Arrival> {
    let mut answers: Vec<Arrival> =
        (0..2).map(|_| arrivals.recv_timeout(PATIENCE).expect("the close answers a held reply")).collect();
    answers.sort_by_key(|arrival| arrival.correlation);
    assert!(arrivals.recv_timeout(QUIET).is_err(), "the staged task owes nothing, so nothing answers it");
    answers
}

/// Assert the close answered correlations 1 (kept in state) and 2 (parked in
/// a task context) with `Owed::unanswered()` before releasing each hold. The
/// stashed request's chain also carries its unstarted task's hold, which the
/// close releases only after the answers, so it arrives with two holds open.
fn assert_answered(answers: &[Arrival]) {
    let seen: Vec<(u64, u32)> = answers.iter().map(|arrival| (arrival.correlation, arrival.held_open)).collect();
    assert_eq!(seen, [(1, 1), (2, 2)], "each answer is sent before the holds on its chain release");
    for arrival in answers {
        assert_eq!(arrival.kind, Owed::ID, "the answer is the held reply kind");
        assert_eq!(
            Owed::decode_from_bytes(&arrival.payload).expect("the answer decodes"),
            Owed::unanswered(),
            "the answer is the kind's unanswered value",
        );
    }
}

/// Catches a silent close, an answer pointer lost when a `Held` parks in a
/// stored context, a hold released before its answer's `Sent`, and a staged
/// `Task` entry that is answered, treated as owed, or left holding its chain.
#[test]
fn closing_an_actor_answers_its_live_and_parked_held_replies() {
    let scenario = scenario();

    let _ = scenario.chassis.send_tracked(scenario.chassis.actor_ref::<CloseProbe>(), &Close, None);

    assert_answered(&answers(&scenario.arrivals));
    for settled in &scenario.settled {
        settled.recv_timeout(PATIENCE).expect("the close lets every request's chain settle");
    }
    assert_eq!(scenario.record.reason(), None, "a close that answers its debts fails nothing");
    drop(scenario.chassis);
}

/// Catches an engine teardown that answers held replies every requester is
/// closing too late to read, and a teardown that lets a `Held` kept in state
/// reach its fail-fast drop. The sink stays registered past the teardown, so
/// an answer would reach it. Chassis teardown drops a root's slot with the
/// actor still in it, so the ledger settles there silently, and no close
/// hook runs.
#[test]
fn dropping_the_chassis_settles_held_replies_silently() {
    let Scenario { chassis, arrivals, settled, record, .. } = scenario();

    drop(chassis);

    assert!(arrivals.recv_timeout(QUIET).is_err(), "an engine teardown answers no held reply");
    for settled in &settled {
        settled.recv_timeout(PATIENCE).expect("the teardown releases every request's chain");
    }
    assert_eq!(record.reason(), None, "a teardown that settles its debts fails nothing");
}
