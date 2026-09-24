//! Reactor restart scenarios: watermark recovery over one journal truth.

mod reactor_world;
mod support;

use aether_bloomery_driver::{Command, ProgramCore};
use aether_bloomery_kinds::{
    ActivationRejected, Detail, Digest, Evaluated, OpaqueBytes, ReactorIntent, ReactorName, RecordedHead,
    RecordedHeadMove, Ref, RuleName, SetHead,
};
use aether_data::Kind;
use reactor_world::{activated_records, failed_records, head_moves, reactor_set, rejected_records, requested_records};
use support::{World, digest, program_head};

/// Restart the core over the same journal truth and scripts.
///
/// Post-restart traffic is observed alone: recordings and injections reset
/// while the journal, artifacts, loads, and scripted replies carry over.
fn restart_world(world: &mut World) -> Vec<Command> {
    let (core, commands) = ProgramCore::start(world.limit);
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
fn restart_warms_through_the_watermark_and_redelivers_nothing_below_it() {
    // Catches the `DuplicateRequest` crash loop: a restart that replays at or
    // below the watermark re-records, and the fold aborts on the duplicate.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    let dest = world.store(OpaqueBytes::ID, b"dest-bytes");
    world.seed_move("target", digest(7));

    let head = program_head("target");
    let set_head = SetHead::new(&head, Some(Ref::from_digest(digest(7))), Ref::from_digest(dest));
    let intent = ReactorIntent::new(
        ReactorName::new("r").expect("valid reactor name"),
        RuleName::new("rule").expect("valid rule name"),
        SetHead::ID,
        set_head.encode_into_bytes(),
    );
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: vec![intent] });
    for seq in [4, 5, 6] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(activated_records(&world).len(), 1);
    assert_eq!(head_moves(&world).len(), 1);

    let commands = restart_world(&mut world);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.warm_ranges_for(bundle_a), vec![(1, 3)]);
    assert_eq!(world.events_for(bundle_a), vec![4, 5]);
    assert!(world.appends.is_empty(), "a restart records nothing twice");
    assert!(requested_records(&world).is_empty());
    assert!(failed_records(&world).is_empty());
    assert_eq!(world.loads_seen, vec![bundle_a]);
    assert_eq!(world.parked.len(), 1);

    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("target")), digest(9));
    let wake = world.append_external(None, &moved);
    let manual = world.drive(wake);
    assert!(manual.is_empty());
    assert_eq!(world.events_for(bundle_a), vec![4, 5, 6]);
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
