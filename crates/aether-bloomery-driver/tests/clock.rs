//! Clock scenarios: `clock.until` requests armed on the driver's timer heap
//! and fired by explicit ticks (ADR-0245).
//!
//! The scripted world waits on settlement by construction: each test drives
//! the core's commands to quiescence and moves time only by calling
//! `tick(now)` with an explicit time, so no scenario sleeps or polls.

mod support;

use aether_bloomery_driver::Command;
use aether_bloomery_kinds::{
    AppendRecords, AwaitProcessed, CLOCK, CLOCK_BUNDLE, Call, CallOutcome, CallProgram, DriverRecord, EncodedArtifact,
    Evaluated, FaultReason, Fired, Head, MAX_DUE_AHEAD_MILLIS, NativeOrigin, Processed, ProgramName, ProgramRef,
    ReactorIntent, ReactorName, ReactorSet, RecordedHead, RecordedHeadMove, RequestSource, Requested, RuleName,
    Transition, Until,
};
use aether_bloomery_program::{ClockUntil, Program, Ran};
use aether_data::{Digest, Kind, OpaqueBytes, Storage, StorageData};
use support::{World, bundle_wasm, digest};

/// The journal time every scenario starts at.
const START_MILLIS: u64 = 1_700_000_000_000;

fn clock_name() -> ProgramName {
    ProgramName::new(ClockUntil::NAME).expect("the clock's name is a valid program name")
}

/// The identity every clock request is recorded under.
fn clock_program() -> ProgramRef {
    ProgramRef::new(CLOCK_BUNDLE, clock_name())
}

fn origin() -> NativeOrigin {
    NativeOrigin::new("test.clock").expect("valid origin")
}

/// Store one `Until` the core can read back.
fn store_until(world: &mut World, due_millis: u64) -> Digest {
    let bytes = Until::encode_storage(&StorageData::from_value(Until { due_millis })).expect("encode until");
    world.store(Until::ID, &bytes)
}

/// One native call of `clock.until` over a stored `Until`.
fn clock_call(input: Digest, key: u64) -> Call {
    Call { program: CLOCK, name: clock_name(), input, origin: origin(), key }
}

/// A world caught up at [`START_MILLIS`] over an empty journal.
fn open_world() -> World {
    let (mut world, commands) = World::open();
    world.now_millis = START_MILLIS;
    let manual = world.drive(commands);
    assert!(manual.is_empty(), "an empty journal catches up with no hand feeding");
    world
}

/// Place one call and drive its follow-ups, returning what the test must feed.
fn place(world: &mut World, call: Call) -> Vec<Command> {
    let (_, commands) = world.core.call(call);
    world.drive(commands)
}

/// Fire the timers due by `now_millis` and drive the follow-ups.
fn tick(world: &mut World, now_millis: u64) -> Vec<Command> {
    let commands = world.core.tick(now_millis);
    world.drive(commands)
}

fn asks_one_tick(manual: &[Command]) -> bool {
    matches!(manual, [Command::ArmTick])
}

/// Every committed record with the seq it landed at, in journal order.
fn recorded(world: &World) -> Vec<(u64, DriverRecord)> {
    world
        .committed
        .iter()
        .flat_map(|append| (append.expected_seq() + 1..).zip(append.records().iter().cloned()))
        .collect()
}

/// The `Transition` records the core appended: cause and record, in order.
fn transitions(world: &World) -> Vec<(u64, Transition)> {
    recorded(world)
        .into_iter()
        .filter_map(|(_, record)| match record {
            DriverRecord::Transition { cause, record } => Some((cause, record)),
            _ => None,
        })
        .collect()
}

fn fired(due_millis: u64) -> EncodedArtifact {
    EncodedArtifact::new(&Fired { due_millis }).expect("encode fired")
}

fn last_append(world: &World) -> &AppendRecords {
    world.committed.last().expect("an append committed")
}

#[test]
fn a_rule_waits_on_the_clock_and_fires_on_its_recorded_run() {
    // Catches a clock request that loads or invokes a bundle, fires before its
    // due time, is stamped before its due time when the wall clock lags the
    // tick, or never reaches the rule that waits on it as a typed run.
    let (mut world, commands) = World::open();
    world.now_millis = START_MILLIS;
    let set = ReactorSet::new(vec![Head::new("a")]).expect("a one-member set");
    let set_bytes = ReactorSet::encode_storage(&StorageData::from_value(set)).expect("encode set");
    let set_digest = world.store(ReactorSet::ID, &set_bytes);
    let reactor = world.store(OpaqueBytes::ID, &bundle_wasm(&[], &["test.reactor"], b"reactor"));
    world.loads.insert(reactor, Ok(()));
    world.seed(None, &RecordedHeadMove::new(RecordedHead::from(&ReactorSet::ROOT), set_digest));
    world.seed_move("a", reactor);
    world.seed_move("trigger", digest(9));

    let due = START_MILLIS + 30_000;
    let until = Until { due_millis: due };
    let wait = CallProgram::with_input(CLOCK, clock_name(), &until).expect("encode until");
    let reactor_name = ReactorName::new("r").expect("valid reactor name");
    let rule = RuleName::new("wait").expect("valid rule name");
    let intent = ReactorIntent::new(reactor_name, rule, CallProgram::ID, wait.encode_into_bytes());
    for seq in 1..=16 {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: vec![intent] });

    let manual = world.drive(commands);
    assert!(asks_one_tick(&manual), "the armed timer asks for one tick: {manual:?}");
    assert!(world.abort.is_none(), "{:?}", world.abort);
    assert_eq!(world.loads_seen, vec![reactor], "the clock loads nothing");
    assert!(world.invokes_seen.is_empty(), "the clock invokes nothing");
    let (request, requested) = recorded(&world)
        .into_iter()
        .find_map(|(seq, record)| match record {
            DriverRecord::Requested { record, .. } => Some((seq, record)),
            _ => None,
        })
        .expect("the rule's wait is requested");
    assert_eq!(requested.program, clock_program());
    assert!(
        matches!(requested.source, RequestSource::Reaction { bundle, .. } if bundle == reactor),
        "the request's source is the rule's own bundle: {:?}",
        requested.source
    );
    let input = EncodedArtifact::new(&until).expect("encode until").digest();
    assert_eq!(requested.input, input);

    let head = world.head();
    let manual = tick(&mut world, due - 1);
    assert!(asks_one_tick(&manual), "a timer not yet due asks for the next tick: {manual:?}");
    assert_eq!(world.head(), head, "nothing fires before its due time");

    world.now_millis = due - 1;
    let fired_at = head + 1;
    world.evaluates.remove(&fired_at);
    let manual = tick(&mut world, due);
    let [Command::Evaluate { bundle, request: event, .. }] = manual.as_slice() else {
        panic!("the recorded run reaches the reactor and nothing else is asked for: {manual:?}");
    };
    assert_eq!(*bundle, reactor);
    assert_eq!(event.entry().seq, fired_at);
    assert_eq!(event.entry().recorded_at_millis, due, "the run is stamped at its due time though the wall lags");
    let run: Ran<ClockUntil> = event.entry().to_entry().decode().expect("the rule's trigger types the run");
    assert_eq!(run.result().digest(), fired(due).digest());

    let transition = Transition { program: clock_program(), input, result: fired(due).digest() };
    assert_eq!(transitions(&world), vec![(request, transition)]);
    assert_eq!(last_append(&world).artifacts(), &[fired(due)]);
    assert_eq!(last_append(&world).not_before_millis(), due);
}

#[test]
fn a_due_time_past_seven_days_is_refused_and_seven_days_exactly_arms() {
    // Catches an unbounded timer, a refusal the caller cannot see, and an
    // off-by-one that refuses the last due time the bound allows.
    let mut world = open_world();
    let far = store_until(&mut world, START_MILLIS + MAX_DUE_AHEAD_MILLIS + 1);
    let manual = place(&mut world, clock_call(far, 1));
    assert!(manual.is_empty(), "a refused timer arms nothing: {manual:?}");
    let [(_, CallOutcome::Fault { key: 1, fault, .. })] = world.answers.as_slice() else {
        panic!("the refusal is recorded and answered: {:?}", world.answers);
    };
    assert!(matches!(fault.reason, FaultReason::Refused { .. }), "{fault:?}");

    let edge = store_until(&mut world, START_MILLIS + MAX_DUE_AHEAD_MILLIS);
    let manual = place(&mut world, clock_call(edge, 2));
    assert!(asks_one_tick(&manual), "seven days exactly arms: {manual:?}");
    assert_eq!(world.answers.len(), 1, "an armed timer is not answered until it fires");
}

#[test]
fn timers_armed_out_of_order_fire_together_in_due_order() {
    // Catches firing in request order rather than due order, one append per
    // timer, a staged result repeated for a shared due time, a tick asked for
    // per timer, and a batch floored below its latest due time.
    let mut world = open_world();
    let dues = [300, 100, 200, 100].map(|offset| START_MILLIS + offset);
    let mut ticks = 0;
    for (key, due) in (1..).zip(dues) {
        let input = store_until(&mut world, due);
        let manual = place(&mut world, clock_call(input, key));
        ticks += manual.iter().filter(|command| matches!(command, Command::ArmTick)).count();
    }
    assert_eq!(ticks, 1, "one tick serves every armed timer");

    let appended = world.committed.len();
    let manual = tick(&mut world, START_MILLIS + 300);
    assert!(manual.is_empty(), "nothing stays armed: {manual:?}");
    assert_eq!(world.committed.len(), appended + 1, "every due timer fires in one append");
    let causes: Vec<u64> = transitions(&world).into_iter().map(|(cause, _)| cause).collect();
    assert_eq!(causes, vec![2, 4, 3, 1], "due order, then request order");
    assert_eq!(last_append(&world).artifacts().len(), 3, "one fired result per distinct due time");
    assert_eq!(last_append(&world).not_before_millis(), START_MILLIS + 300);
    assert!(world.answers.iter().all(|(_, outcome)| matches!(outcome, CallOutcome::Transition { .. })));
    assert_eq!(world.answers.len(), 4);
}

#[test]
fn a_restart_re_arms_an_outstanding_timer_instead_of_interrupting_it() {
    // Catches a restart that faults a timer `Interrupted`, leaves it unarmed,
    // or stops interrupting the other programs outstanding beside it.
    let (mut world, commands) = World::open();
    world.now_millis = START_MILLIS;
    let due = START_MILLIS + 1_000;
    let input = store_until(&mut world, due);
    let clock_source = RequestSource::Native { origin: origin(), key: 1 };
    world.seed(None, &Requested { program: clock_program(), input, source: clock_source });
    let other = ProgramRef::new(digest(7), ProgramName::new("run").expect("valid program name"));
    let other_source = RequestSource::Native { origin: origin(), key: 2 };
    world.seed(None, &Requested { program: other, input: digest(8), source: other_source });

    let restarted_at = due + 60_000;
    world.now_millis = restarted_at;
    let manual = world.drive(commands);
    assert!(asks_one_tick(&manual), "recovery re-arms the timer: {manual:?}");
    let interrupted: Vec<u64> = recorded(&world)
        .into_iter()
        .filter_map(|(_, record)| match record {
            DriverRecord::Fault { cause, record } if record.reason == FaultReason::Interrupted => Some(cause),
            _ => None,
        })
        .collect();
    assert_eq!(interrupted, vec![2], "only the program is interrupted");

    let manual = tick(&mut world, restarted_at);
    assert!(manual.is_empty(), "{manual:?}");
    let causes: Vec<u64> = transitions(&world).into_iter().map(|(cause, _)| cause).collect();
    assert_eq!(causes, vec![1], "the overdue timer fires on the first tick");
}

#[test]
fn a_barrier_does_not_wait_out_an_armed_timer() {
    // Catches counting an armed timer as outstanding work, which would hold
    // every barrier until the timer fires, or forever behind a cron chain.
    let mut world = open_world();
    let input = store_until(&mut world, START_MILLIS + 60_000);
    let manual = place(&mut world, clock_call(input, 1));
    assert!(asks_one_tick(&manual), "{manual:?}");

    let through = world.head();
    let (_, commands) = world.core.await_processed(AwaitProcessed { through });
    let manual = world.drive(commands);
    assert!(manual.is_empty(), "{manual:?}");
    assert!(
        matches!(world.processed.as_slice(), [(_, Processed::Head { head })] if *head == through),
        "the barrier answers while the timer is armed: {:?}",
        world.processed
    );
}
