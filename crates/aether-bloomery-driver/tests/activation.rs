//! Scenario tests: reactor activation from scripted moves, loads, and warm replies.
//!
//! Each test names the bug it catches.

mod reactor_world;
mod support;

use aether_bloomery_driver::{Command, EvaluateTicket};
use aether_bloomery_kinds::{Detail, Digest, Evaluated, OpaqueBytes, RecordedHead, RecordedHeadMove, Utf8Text, Warmed};
use aether_data::Kind;
use reactor_world::{activated_records, failed_records, head_moves, reactor_set, rejected_records, requested_records};
use support::{World, bundle_wasm, digest, program_head};

/// Feed one held evaluation reply, sequencing the double behind it.
fn feed_evaluated(world: &mut World, ticket: EvaluateTicket, evaluated: Evaluated) -> Vec<Command> {
    if let Some(root) = world.eval_roots.remove(&ticket)
        && let Some(reactor) = world.reactors.get_mut(&root)
    {
        reactor.note_evaluated(&evaluated);
    }
    world.core.on_evaluated(ticket, evaluated)
}

fn activated_live_from(world: &World, bundle: Digest) -> Vec<u64> {
    let mut out = Vec::new();
    for (_, record) in activated_records(world) {
        if record.bundle() == bundle {
            out.push(record.live_from().0);
        }
    }
    out
}

#[test]
fn a_move_at_n_lets_the_predecessor_evaluate_n_and_the_successor_start_at_n_plus_one() {
    // Catches an off-by-one boundary: the predecessor missing `N`, or the
    // successor going live at `N` instead of `N+1`.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: Vec::new() });
    let manual = world.drive(commands);
    assert!(manual.is_empty());

    let bundle_b = world.store_reactor(b"reactor-b");
    for seq in [4, 5] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), bundle_b);
    let wake = world.append_external(None, &moved);
    let manual = world.drive(wake);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.events_for(bundle_a), vec![3, 4]);
    assert_eq!(world.events_for(bundle_b), vec![5]);
    assert_eq!(world.warm_ranges_for(bundle_b), vec![(1, 4)]);
    assert_eq!(activated_live_from(&world, bundle_b), vec![5]);
    assert!(requested_records(&world).is_empty());
    assert!(failed_records(&world).is_empty());
    assert!(head_moves(&world).is_empty());
}

#[test]
fn shared_digest_sees_the_earlier_heads_warm() {
    // Catches a second activation for one digest that rewarms from 1, which the root would refuse.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let shared = world.store_reactor(b"shared");
    world.seed_set_root(set_digest);
    world.seed_move("a", shared);
    world.seed_move("b", shared);
    for seq in [3, 4, 5] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(world.warm_ranges_for(shared), vec![(1, 2)]);
}

#[test]
fn a_b_a_reuses_the_dormant_instance() {
    // Catches a second load for a digest that never unloaded, or re-warming
    // from seq 1 into `OutOfSequence`.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let first = world.store_reactor(b"first");
    let second = world.store_reactor(b"second");
    world.seed_set_root(set_digest);
    world.seed_move("a", first);
    for seq in [3, 4, 5, 6, 7] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());

    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), second);
    let wake = world.append_external(None, &moved);
    let follow = world.drive(wake);
    assert!(follow.is_empty());

    let back = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), first);
    let wake = world.append_external(None, &back);
    let follow = world.drive(wake);
    assert!(follow.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.loads_seen, vec![first, second]);
    assert_eq!(world.warm_ranges_for(first), vec![(1, 2), (5, 6)]);
    assert_eq!(activated_live_from(&world, first), vec![3, 7]);
    assert_eq!(world.events_for(first), vec![3, 4, 7]);
}

#[test]
fn rejected_interval_is_delivered_to_the_next_activation() {
    // Catches a fresh N+1 live-from that drops the owed prefix, or catch-up delivered out of order.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let missing = digest(3);
    world.seed_set_root(set_digest);
    world.seed_move("a", missing);
    let manual = world.drive(commands);
    assert!(manual.is_empty());

    let next = world.store_reactor(b"next");
    for seq in [3, 4, 5] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), next);
    let wake = world.append_external(None, &moved);
    let follow = world.drive(wake);
    assert!(follow.is_empty());
    assert!(world.abort.is_none());

    // Seqs 3-4 are the owed catch-up; 5 is the live follow-on once the activation commits.
    assert_eq!(world.events_for(next), vec![3, 4, 5]);
    assert_eq!(world.warm_ranges_for(next), vec![(1, 2)]);
    assert_eq!(activated_live_from(&world, next), vec![3]);

    // The catch-up reads from `live_from - 1`, never the journal prefix again.
    let last_warm = world.warm_marks.last().copied().expect("the activation warmed");
    let after_warm = &world.events_seen[last_warm..];
    assert!(after_warm.iter().all(|after| *after >= 2), "reads after the last warm: {after_warm:?}");
}

#[test]
fn poisoned_warm_records_rejection_and_marks_the_instance() {
    // Catches a poisoned mid-activation instance that is reloaded.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let shared = world.store_reactor(b"shared");
    world.seed_set_root(set_digest);
    world.seed_move("a", shared);
    world.seed_move("b", shared);
    world.warm_pages.insert(1, Warmed::Poisoned { last_trusted: 0, reason: Detail::new("poisoned") });
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(rejected_records(&world).len(), 2);
    assert!(failed_records(&world).is_empty(), "a warm poison records only rejections");
    assert_eq!(world.loads_seen, vec![shared]);
}

#[test]
fn shared_instance_past_the_owed_start_is_rejected() {
    // Catches a past-start activation that warms anyway.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let shared = world.store_reactor(b"shared");
    let missing = digest(3);
    world.seed_set_root(set_digest);
    world.seed_move("a", shared);
    world.seed_move("b", missing);
    for seq in [3, 4, 5, 6, 7] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());

    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("b")), shared);
    let wake = world.append_external(None, &moved);
    let follow = world.drive(wake);
    assert!(follow.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.warm_ranges_for(shared), vec![(1, 2)]);
    assert_eq!(world.events_for(shared), vec![3, 4, 5, 6, 7]);
    let rejected = rejected_records(&world).iter().filter(|(cause, _)| *cause == 6).count();
    assert_eq!(rejected, 1);
}

#[test]
fn activation_on_a_program_only_digest_is_rejected_before_any_load() {
    // Catches an undeclared reactor role loaded instead of refused from the missing section.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let reactor = world.store_reactor(b"reactor");
    let program = world
        .store(OpaqueBytes::ID, &bundle_wasm(&[("run", Utf8Text::ID, OpaqueBytes::ID, "run it")], &[], b"program"));
    world.seed_set_root(set_digest);
    world.seed_move("a", reactor);
    world.seed_move("prog", program);
    for seq in [3, 4, 5] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());

    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), program);
    let wake = world.append_external(None, &moved);
    let follow = world.drive(wake);
    assert!(follow.is_empty());
    assert!(world.abort.is_none());

    assert!(!world.loads_seen.contains(&program));
    let rejected = rejected_records(&world);
    let (_, record) =
        rejected.iter().find(|(_, record)| record.bundle == program).expect("a is rejected for the program digest");
    assert!(record.reason.as_str().contains("declares no reactors"), "unexpected reason: {:?}", record.reason);
}

#[test]
fn poisoned_digest_rejects_every_head_it_served() {
    // Catches routing on to a poisoned root, a missing per-head rejection, or
    // reloading a poisoned digest when a head moves back to it.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let shared = world.store_reactor(b"shared");
    world.seed_set_root(set_digest);
    world.seed_move("a", shared);
    world.seed_move("b", shared);
    for seq in [3, 5, 6, 7, 8, 9, 10, 11] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    let [Command::Evaluate { ticket, .. }] = manual.as_slice() else {
        panic!("expected one held evaluate, got {manual:?}");
    };
    let ticket = *ticket;

    let poisoned = Evaluated::Poisoned { seq: 4, last_trusted: 3, reason: Detail::new("boom") };
    let follow = feed_evaluated(&mut world, ticket, poisoned);
    let manual = world.drive(follow);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let failed = failed_records(&world);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, 4);
    assert!(failed[0].1.reactor.is_none());
    let rejected = rejected_records(&world);
    assert_eq!(rejected.len(), 2);
    assert!(rejected.iter().all(|(cause, _)| *cause == 4));
    assert_eq!(world.events_for(shared), vec![3, 4]);

    let next = world.store_reactor(b"next");
    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), next);
    let wake = world.append_external(None, &moved);
    let manual = world.drive(wake);
    assert!(manual.is_empty());

    let back = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), shared);
    let wake = world.append_external(None, &back);
    let manual = world.drive(wake);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.loads_seen, vec![shared, next]);
    assert_eq!(world.events_for(shared), vec![3, 4], "nothing routes to a poisoned root");
    // Seqs 3-4 went out while live, so the successor owes only 5-9, then goes live at 10.
    assert_eq!(world.events_for(next), vec![5, 6, 7, 8, 9, 10, 11]);
    assert_eq!(world.warm_ranges_for(next), vec![(1, 4)]);
    assert_eq!(activated_live_from(&world, next), vec![5]);
    let rejected = rejected_records(&world);
    assert!(
        rejected
            .iter()
            .any(|(cause, record)| *cause == 11 && record.head == program_head("a") && record.bundle == shared)
    );
}
