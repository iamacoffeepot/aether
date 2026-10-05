//! Reactor restart scenarios: watermark recovery over one journal truth.

mod reactor_world;
mod support;

use aether_bloomery_driver::{Command, InvocationLimit, ProgramCore};
use aether_bloomery_kinds::{
    ActivationRejected, Detail, Evaluated, HeadChange, ReactorIntent, ReactorName, RecordedHead, RecordedHeadMove,
    RuleName, SetHeads, Status,
};
use aether_data::{Digest, Kind, OpaqueBytes, Ref};
use reactor_world::{activated_records, failed_records, head_moves, reactor_set, rejected_records, requested_records};
use support::{World, digest, program_head};

/// Restart the core over the same journal truth and scripts.
///
/// Post-restart traffic is observed alone: recordings and injections reset
/// while the journal, artifacts, loads, and scripted replies carry over.
fn restart_world(world: &mut World) -> Vec<Command> {
    let (core, commands) = ProgramCore::start(world.limit, InvocationLimit::DEFAULT);
    world.core = core;
    world.parked.clear();
    world.watches_seen.clear();
    world.events_seen.clear();
    world.warm_marks.clear();
    world.reactors.clear();
    world.eval_roots.clear();
    world.processed.clear();
    world.fetched.clear();
    world.answers.clear();
    world.abort = None;
    world.appends.clear();
    world.committed.clear();
    world.reads_seen.clear();
    world.closures_seen.clear();
    world.loads_seen.clear();
    world.invokes_seen.clear();
    world.conflict_next = None;
    world.fail_next = None;
    world.fail_reads = None;
    commands
}

/// Fail one bundle's load with `message`.
fn fail_load(world: &mut World, bundle: Digest, message: &str) {
    world.loads.insert(bundle, Err(message.to_string()));
}

#[test]
fn restart_preserves_committed_and_refused_atomic_groups() {
    // Catches restart recovering only part of a committed group, reapplying
    // its moves, or losing a refused group's reaction watermark.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    let first_destination = world.store(OpaqueBytes::ID, b"first-destination");
    let second_destination = world.store(OpaqueBytes::ID, b"second-destination");
    world.seed_move("target", digest(7));

    let set_head = SetHeads::new(vec![
        HeadChange::new(
            &program_head("target"),
            Some(Ref::from_digest(digest(7))),
            Ref::from_digest(first_destination),
        ),
        HeadChange::new(&program_head("other"), None, Ref::from_digest(second_destination)),
    ]);
    let intent = ReactorIntent::new(
        ReactorName::new("r").expect("valid reactor name"),
        RuleName::new("rule").expect("valid rule name"),
        SetHeads::ID,
        set_head.encode_into_bytes(),
    );
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: vec![intent] });
    let refused = SetHeads::new(vec![
        HeadChange::new(
            &program_head("target"),
            Some(Ref::from_digest(first_destination)),
            Ref::from_digest(second_destination),
        ),
        HeadChange::new(&program_head("other"), Some(Ref::from_digest(digest(9))), Ref::from_digest(first_destination)),
    ]);
    world.evaluates.insert(
        4,
        Evaluated::Completed {
            seq: 4,
            intents: vec![ReactorIntent::new(
                ReactorName::new("r").expect("valid reactor name"),
                RuleName::new("rule").expect("valid rule name"),
                SetHeads::ID,
                refused.encode_into_bytes(),
            )],
        },
    );
    for seq in [5, 6, 7, 8] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(activated_records(&world).len(), 1);
    assert_eq!(head_moves(&world).len(), 2, "both committed group moves are present");
    assert_eq!(failed_records(&world).len(), 1, "the late mismatch refuses its group once");

    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.warm_ranges_for(bundle_a), vec![(1, 4)]);
    assert_eq!(world.events_for(bundle_a), vec![5, 6, 7]);
    assert!(world.appends.is_empty(), "a restart records nothing twice");
    assert!(requested_records(&world).is_empty());
    assert!(failed_records(&world).is_empty());
    assert_eq!(world.loads_seen, vec![bundle_a]);
    assert_eq!(world.parked.len(), 1);

    let external_seq = world.head() + 1;
    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("target")), digest(9));
    let wake = world.append_external(None, &moved);
    let manual = world.drive(wake);
    assert!(manual.is_empty());
    assert_eq!(world.events_for(bundle_a), vec![5, 6, 7, external_seq]);
    assert_eq!(world.parked.len(), 1);
}

#[test]
fn restart_after_an_activation_only_batch_records_nothing_twice() {
    // Catches restarting at the reaction watermark alone: with no reaction
    // records, the activation watermark is the only fence against replay.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: Vec::new() });
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(activated_records(&world).len(), 1);

    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert!(world.appends.is_empty(), "a restart records nothing twice");
    assert_eq!(world.warm_ranges_for(bundle_a), vec![(1, 2)]);
    assert_eq!(world.events_for(bundle_a), vec![3]);
    assert!(requested_records(&world).is_empty());
    assert!(failed_records(&world).is_empty());
    assert_eq!(world.parked.len(), 1);
}

#[test]
fn restart_rejects_a_live_head_whose_bundle_no_longer_loads() {
    // Catches a restart wedged on an unloadable instance: the head is
    // rejected once, the bundle is never retried, and routing proceeds.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: Vec::new() });
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(activated_records(&world).len(), 1);

    fail_load(&mut world, bundle_a, "bundle evicted");
    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let rejected = rejected_records(&world);
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].0, 2);
    assert_eq!(rejected[0].1.head, program_head("a"));
    assert_eq!(rejected[0].1.bundle, bundle_a);
    assert_eq!(world.loads_seen, vec![bundle_a]);
    assert!(world.events_for(bundle_a).is_empty());
    assert_eq!(world.parked.len(), 1);
}

#[test]
fn restart_reads_no_pages_to_rebuild_routing_heads() {
    // Catches restart replaying the journal prefix to rebuild routing `Heads`,
    // which the journal view's own catch-up has already folded.
    let (mut world, commands) = World::open();
    world.seed_move("x", digest(1));
    world.seed_move("y", digest(2));
    world.seed_move("x", digest(4));
    let rejected = ActivationRejected { head: program_head("a"), bundle: digest(3), reason: Detail::new("missing") };
    world.seed(Some(3), &rejected);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let from_start = world.events_seen.iter().filter(|after| **after == 0).count();
    assert_eq!(from_start, 1, "only the journal view reads from the start: {:?}", world.events_seen);
    assert!(world.appends.is_empty(), "a restart records nothing twice");
    assert_eq!(world.parked.len(), 1, "restart finished and live routing parked its watch");
}

/// Run a first incarnation whose restart point is 5: `a` activates at 2 on
/// `bundle_a`, the root evaluates 3 and 4 quietly, and a failed reaction at
/// 5 raises the reaction watermark there. Returns the bundle.
fn history_through_five(world: &mut World, commands: Vec<Command>) -> Digest {
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("x", digest(7));
    world.seed_move("y", digest(8));
    world.seed_move("z", digest(9));
    for seq in [3, 4, 6, 7, 8] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let failed = Evaluated::Failed {
        seq: 5,
        reactor: ReactorName::new("r").expect("valid reactor name"),
        reason: Detail::new("boom"),
    };
    world.evaluates.insert(5, failed);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(activated_records(world).len(), 1);
    assert_eq!(failed_records(world).iter().map(|(cause, _)| *cause).collect::<Vec<_>>(), vec![5]);
    bundle_a
}

#[test]
fn restart_over_an_adopted_root_warms_from_its_reported_cursor() {
    // Catches a restarted driver that treats a root the engine still holds
    // live as freshly stood up: warming it from 1 re-folds seqs it already
    // folded, and its double answers the batch out of sequence.
    let (mut world, commands) = World::open();
    let bundle_a = history_through_five(&mut world, commands);

    world.adopted.insert(bundle_a, Status::new(3, false));
    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.loads_seen, vec![bundle_a]);
    assert_eq!(world.warm_ranges_for(bundle_a), vec![(4, 5)], "the warm starts after the reported cursor");
    assert_eq!(world.events_for(bundle_a), vec![6, 7]);
    assert!(world.appends.is_empty(), "an adopted restart records nothing twice");
    assert_eq!(world.parked.len(), 1);
}

#[test]
fn restart_over_a_poisoned_adopted_root_rejects_its_heads() {
    // Catches an adopted root whose status reports poisoned being warmed or
    // routed to anyway instead of rejecting the heads it serves at the
    // restart point.
    let (mut world, commands) = World::open();
    let bundle_a = history_through_five(&mut world, commands);

    world.adopted.insert(bundle_a, Status::new(3, true));
    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let rejected = rejected_records(&world);
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].0, 5);
    assert_eq!(rejected[0].1.head, program_head("a"));
    assert_eq!(rejected[0].1.bundle, bundle_a);
    assert!(world.warm_ranges_for(bundle_a).is_empty());
    assert!(world.events_for(bundle_a).is_empty());
    assert_eq!(world.parked.len(), 1);
}

#[test]
fn restart_over_an_adopted_root_past_the_restart_point_rejects_its_heads() {
    // Catches an adopted root that evaluated seqs past the restart point,
    // whose reactions were never recorded, reaching live routing: the core
    // would find it ahead of the next seq and abort.
    let (mut world, commands) = World::open();
    let bundle_a = history_through_five(&mut world, commands);

    world.adopted.insert(bundle_a, Status::new(7, false));
    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none(), "unexpected abort: {:?}", world.abort);

    let rejected = rejected_records(&world);
    assert_eq!(rejected.len(), 1);
    assert_eq!(rejected[0].0, 5);
    assert_eq!(rejected[0].1.head, program_head("a"));
    assert!(rejected[0].1.reason.as_str().contains("past the restart point"), "{}", rejected[0].1.reason.as_str());
    assert!(world.warm_ranges_for(bundle_a).is_empty());
    assert!(world.events_for(bundle_a).is_empty());
    assert_eq!(world.parked.len(), 1);
}
