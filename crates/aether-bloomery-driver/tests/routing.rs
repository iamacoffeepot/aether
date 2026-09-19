//! Scenario tests: reactor routing from scripted sets, members, and replies.
//!
//! Each test names the bug it catches. Seqs are exact because the scripted
//! journal is fully deterministic: seeded moves and records take the first
//! seqs, and every routing batch lands where the test says it does.

mod reactor_world;
mod support;

use std::collections::BTreeMap;

use aether_bloomery_driver::{CallerId, Command, EvaluateTicket, InvokeTicket};
use aether_bloomery_kinds::{
    AppendRecords, AwaitProcessed, Call, CallProgram, ClosureArtifact, Detail, Digest, DriverRecord, EncodedArtifact,
    Evaluated, FaultReason, Invoked, NativeOrigin, OpaqueBytes, Processed, ProgramName, ProgramRef, ReactorIntent,
    ReactorName, ReactorSet, RecordedHead, RecordedHeadMove, Ref, RequestSource, RuleName, SetHead, Status, Utf8Text,
    artifact_digest,
};
use aether_data::{Kind, KindId, MailboxId};
use reactor_world::{activated_records, failed_records, head_moves, reactor_set, rejected_records, requested_records};
use support::{World, digest, program_head};

/// Mailbox the scripted program loads hand out.
const ROOT: MailboxId = MailboxId(7);

/// Store one program wasm bundle, answering its load as the program root.
fn store_program(world: &mut World, wasm: &[u8]) -> Digest {
    let digest = world.store(OpaqueBytes::ID, wasm);
    world.loads.insert(digest, Ok(ROOT));
    digest
}

fn reactor_name(name: &str) -> ReactorName {
    ReactorName::new(name).expect("valid reactor name")
}

fn rule_name(name: &str) -> RuleName {
    RuleName::new(name).expect("valid rule name")
}

fn call_intent(reactor: &str, rule: &str, program: &'static str, name: &str, input: Digest) -> ReactorIntent {
    let call = CallProgram {
        program: program_head(program),
        name: ProgramName::new(name).expect("valid program name"),
        input,
    };
    ReactorIntent::new(reactor_name(reactor), rule_name(rule), CallProgram::ID, call.encode_into_bytes())
}

/// Park one barrier waiter and drive its follow-ups, returning its caller.
fn await_processed(world: &mut World, through: u64) -> CallerId {
    let (caller, commands) = world.core.await_processed(AwaitProcessed { through });
    let manual = world.drive(commands);
    assert!(manual.is_empty(), "barrier follow-ups need no hand feeding");
    caller
}

/// The barrier reply collected for `caller`, if answered.
fn processed_by(world: &World, caller: CallerId) -> Option<Processed> {
    world.processed.iter().find(|(owed, _)| *owed == caller).map(|(_, reply)| reply.clone())
}

/// Parked watches awaiting a head that passes their boundary.
fn watch_count(world: &World) -> usize {
    world.parked.len()
}

/// Every `WatchHead` boundary the core emitted, in order.
fn watches_for(world: &World) -> Vec<u64> {
    world.watches_seen.clone()
}

/// Feed one held evaluation reply, sequencing the double behind it.
fn feed_evaluated(world: &mut World, ticket: EvaluateTicket, evaluated: Evaluated) -> Vec<Command> {
    if let Some(root) = world.eval_roots.remove(&ticket)
        && let Some(reactor) = world.reactors.get_mut(&root)
    {
        reactor.note_evaluated(&evaluated);
    }
    world.core.on_evaluated(ticket, evaluated)
}

#[test]
fn unbound_set_and_unbound_member_select_nothing() {
    // Catches an unbound set that errors instead of selecting nothing, or one
    // unbound member failing the whole set `select_reactors`-style.
    let (mut world, commands) = World::open();
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert!(world.appends.is_empty(), "an unbound set appends nothing");
    assert_eq!(watch_count(&world), 1);

    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let set_move = RecordedHeadMove::new(RecordedHead::from(&ReactorSet::ROOT), set_digest);
    let wake = world.append_external(None, &set_move);
    let manual = world.drive(wake);
    assert!(manual.is_empty());

    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: Vec::new() });
    let member = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), bundle_a);
    let wake = world.append_external(None, &member);
    let manual = world.drive(wake);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let activated = activated_records(&world);
    assert_eq!(activated.len(), 1);
    assert_eq!(activated[0].0, 2);
    assert_eq!(activated[0].1.head(), &program_head("a"));
    assert!(rejected_records(&world).is_empty());
    assert_eq!(world.warm_ranges_for(bundle_a), vec![(1, 2)]);
    assert_eq!(world.events_for(bundle_a), vec![3]);
    assert_eq!(watch_count(&world), 1);
}

#[test]
fn event_fan_out_reaches_every_live_digest_once() {
    // Catches a second Event per digest, or a warmup sent where live delivery belongs.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let bundle_b = world.store_reactor(b"reactor-b", MailboxId(102));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("b", bundle_b);

    for seq in [3, 4, 5] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.warm_ranges_for(bundle_a), vec![(1, 2)]);
    assert_eq!(world.warm_ranges_for(bundle_b), vec![(1, 3)]);
    assert_eq!(world.events_for(bundle_a), vec![3, 4, 5]);
    assert_eq!(world.events_for(bundle_b), vec![4, 5]);
    assert_eq!(watch_count(&world), 1);
}

#[test]
fn selective_delivery_skips_unlisted_and_unlive_heads() {
    // Catches an entry fanned out to every loaded digest, or delivered without checking liveness.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let bundle_missing = digest(3);
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("b", bundle_missing);

    for seq in [3, 4, 5] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(world.events_for(bundle_a), vec![3, 4, 5]);
    assert!(world.events_for(bundle_missing).is_empty());
    assert_eq!(rejected_records(&world).len(), 1);

    let bundle_a2 = world.store_reactor(b"reactor-a2", MailboxId(103));
    for seq in [6, 7] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("a")), bundle_a2);
    let follow = world.append_external(None, &moved);
    let manual = world.drive(follow);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.events_for(bundle_a), vec![3, 4, 5, 6]);
    assert_eq!(world.events_for(bundle_a2), vec![7]);
    assert!(world.events_for(bundle_missing).is_empty());
}

#[test]
fn unbound_program_head_becomes_a_single_reaction_failed() {
    // Catches a crash on a missing head, or a sibling intent dropped with the refusal.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let program_bundle = store_program(&mut world, b"program-wasm");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("prog", program_bundle);

    let missing = call_intent("r", "rule", "missing", "run", digest(9));
    let present = call_intent("r", "rule", "prog", "run", digest(9));
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: vec![missing, present] });
    for seq in [4, 5, 6, 7] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let failed = failed_records(&world);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, 3);
    assert!(failed[0].1.reactor.is_some());
    assert_eq!(requested_records(&world).len(), 1, "the sibling intent still stands");
}

#[test]
fn set_head_refusals_fail_only_that_intent() {
    // Catches a swap mismatch, a missing destination, or a wrong-kind
    // destination failing the batch instead of its own intent, and a swap
    // checked without the earlier move in the same batch.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    let wrong = world.store(Utf8Text::ID, b"text-bytes");
    let chain_mid = world.store(OpaqueBytes::ID, b"chain-mid");
    let chain_end = world.store(OpaqueBytes::ID, b"chain-end");
    world.seed_move("target", digest(7));

    let head = program_head("target");
    let set_intent = |from: Option<Digest>, to: Digest| {
        let set_head = SetHead::new(&head, from.map(Ref::from_digest), Ref::from_digest(to));
        ReactorIntent::new(reactor_name("r"), rule_name("rule"), SetHead::ID, set_head.encode_into_bytes())
    };
    let intents = vec![
        set_intent(Some(digest(7)), wrong),
        set_intent(Some(digest(7)), digest(77)),
        set_intent(Some(digest(9)), chain_mid),
        set_intent(Some(digest(7)), chain_mid),
        set_intent(Some(chain_mid), chain_end),
        call_intent("r", "other", "target", "run", digest(9)),
    ];
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents });
    for seq in [4, 5, 6, 7, 8, 9, 10, 11] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(failed_records(&world).len(), 3);
    assert_eq!(head_moves(&world).len(), 2, "the chained swap sees the earlier move");
    assert_eq!(requested_records(&world).len(), 1, "the sibling intent still stands");
}

#[test]
fn ordinals_count_within_one_rule() {
    // Catches a global or per-reply ordinal that collides or shifts dedup keys.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let program_bundle = store_program(&mut world, b"program-wasm");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("prog", program_bundle);

    let first = call_intent("r", "rule", "prog", "run", digest(11));
    let second = call_intent("r", "rule", "prog", "run", digest(12));
    let other = call_intent("r", "other", "prog", "run", digest(13));
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: vec![first, second, other] });
    for seq in [4, 5, 6, 7, 8, 9, 10] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let requested = requested_records(&world);
    assert_eq!(requested.len(), 3);
    let mut ordinals: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (_, record) in &requested {
        if let RequestSource::Reaction { rule, ordinal, .. } = &record.source {
            ordinals.entry(format!("{rule:?}")).or_default().push(*ordinal);
        }
    }
    let mut seen: Vec<Vec<u32>> = ordinals.values().cloned().collect();
    seen.sort();
    assert_eq!(seen, vec![vec![0], vec![0, 1]]);
}

#[test]
fn watch_wakes_routing_for_entries_others_append() {
    // Catches more than one outstanding watch.
    let (mut world, commands) = World::open();
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(watch_count(&world), 1);
    assert_eq!(watches_for(&world), vec![0]);

    let moved = RecordedHeadMove::new(RecordedHead::from(&program_head("other")), digest(9));
    let follow = world.append_external(None, &moved);
    let manual = world.drive(follow);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(watch_count(&world), 1);
    assert_eq!(watches_for(&world), vec![0, 1]);
}

/// One wasm bundle declaring one program: name, input kind, result kind.
fn program_bundle(name: &str, input: KindId, result: KindId) -> Vec<u8> {
    fn push_leb(mut value: u32, out: &mut Vec<u8>) {
        while value >= 0x80 {
            let byte = u8::try_from(value & 0x7f).expect("masked byte fits");
            out.push(byte | 0x80);
            value >>= 7;
        }
        out.push(u8::try_from(value).expect("final byte fits"));
    }
    let name = name.as_bytes();
    let intent = b"run it";
    let mut data = Vec::new();
    data.push(1u8);
    data.extend_from_slice(&u16::try_from(name.len()).expect("test name fits").to_le_bytes());
    data.extend_from_slice(name);
    data.extend_from_slice(&input.0.to_le_bytes());
    data.extend_from_slice(&result.0.to_le_bytes());
    data.push(0u8);
    data.extend_from_slice(&u16::try_from(intent.len()).expect("test intent fits").to_le_bytes());
    data.extend_from_slice(intent);
    let section_name = b"aether.bloomery.programs";
    let mut section = Vec::new();
    push_leb(u32::try_from(section_name.len()).expect("test section name fits"), &mut section);
    section.extend_from_slice(section_name);
    section.extend_from_slice(&data);
    let mut wasm = b"\0asm".to_vec();
    wasm.extend_from_slice(&1u32.to_le_bytes());
    wasm.push(0);
    push_leb(u32::try_from(section.len()).expect("test section fits"), &mut wasm);
    wasm.extend_from_slice(&section);
    wasm
}

/// Script one input's closure.
fn script_closure(world: &mut World, root: Digest, artifacts: Vec<ClosureArtifact>) {
    world.closures.insert(root, artifacts);
}

/// The one invoke ticket from a single manual command.
fn invoke_ticket(manual: &[Command]) -> InvokeTicket {
    let [Command::Invoke { ticket, .. }] = manual else {
        panic!("expected exactly one manual invoke, got {manual:?}");
    };
    *ticket
}

#[test]
fn two_heads_sharing_a_digest_get_one_delivery_per_seq() {
    // Catches per-head delivery, which duplicates `(cause, source)` and makes
    // the fold abort.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a", "b"]);
    let set_digest = world.store_set(&set);
    let shared = world.store_reactor(b"shared", MailboxId(101));
    let program_bundle = store_program(&mut world, b"program-wasm");
    world.seed_set_root(set_digest);
    world.seed_move("a", shared);
    world.seed_move("b", shared);
    world.seed_move("prog", program_bundle);

    let intent = call_intent("r", "rule", "prog", "run", digest(9));
    world.evaluates.insert(4, Evaluated::Completed { seq: 4, intents: vec![intent] });
    for seq in [3, 5, 6, 7, 8] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(requested_records(&world).len(), 1);
    assert_eq!(activated_records(&world).len(), 2);
    let events = world.events_for(shared);
    let mut ordered = events.clone();
    ordered.sort_unstable();
    ordered.dedup();
    assert_eq!(events, ordered, "one delivery per seq, never one per head");
}

#[test]
fn call_program_resolves_its_head_through_the_trigger_and_enters_the_pipeline() {
    // Catches `N-1` or write-time resolution, a missing cause, a request
    // never invoked.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let first = store_program(&mut world, b"program-one");
    let second = store_program(&mut world, b"program-two");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("prog", first);
    world.seed_move("prog", second);

    let intent = call_intent("r", "rule", "prog", "run", digest(9));
    world.evaluates.insert(4, Evaluated::Completed { seq: 4, intents: vec![intent] });
    for seq in [3, 5, 6, 7] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let requested = requested_records(&world);
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0].0, Some(4));
    let run = ProgramName::new("run").expect("valid program name");
    assert_eq!(requested[0].1.program, ProgramRef::new(second, run));
    let faults = world
        .appends
        .iter()
        .flat_map(AppendRecords::records)
        .filter(|record| matches!(record, DriverRecord::Fault { cause: 6, .. }))
        .count();
    assert_eq!(faults, 1, "the reaction request enters the program pipeline");
}

#[test]
fn unsupported_intent_records_reaction_failed_and_keeps_siblings() {
    // Catches failing or dropping the whole reply for one bad intent.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let program_bundle = store_program(&mut world, b"program-wasm");
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("prog", program_bundle);

    let bad = ReactorIntent::new(reactor_name("r"), rule_name("rule"), OpaqueBytes::ID, b"not-an-intent".to_vec());
    let sibling = call_intent("r", "rule", "prog", "run", digest(9));
    world.evaluates.insert(3, Evaluated::Completed { seq: 3, intents: vec![bad, sibling] });
    for seq in [4, 5, 6, 7] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let failed = failed_records(&world);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, 3);
    assert!(failed[0].1.reactor.is_some());
    assert!(failed[0].1.reason.as_str().contains("unsupported"));
    assert_eq!(requested_records(&world).len(), 1, "the sibling intent still stands");
}

#[test]
fn set_head_conflict_rechecks_the_swap() {
    // Catches appending a swap decided against a stale view: the target moves
    // after the first derivation, so the re-derived batch must refuse it.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    let dest = world.store(OpaqueBytes::ID, b"dest-bytes");
    world.seed_move("target", digest(7));
    for seq in [4, 5, 6] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    let [Command::Evaluate { ticket, .. }] = manual.as_slice() else {
        panic!("expected one held evaluate, got {manual:?}");
    };
    let ticket = *ticket;

    let raced = RecordedHeadMove::new(RecordedHead::from(&program_head("target")), digest(8));
    let wake = world.append_external(None, &raced);
    assert!(wake.is_empty(), "no watch is parked while routing is busy");

    let head = program_head("target");
    let from = Ref::from_digest(digest(7));
    let to = Ref::from_digest(dest);
    let set_head = SetHead::new(&head, Some(from), to);
    let intent = ReactorIntent::new(reactor_name("r"), rule_name("rule"), SetHead::ID, set_head.encode_into_bytes());
    let reply = Evaluated::Completed { seq: 3, intents: vec![intent] };
    let follow = feed_evaluated(&mut world, ticket, reply);
    let manual = world.drive(follow);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let committed: Vec<_> = world.committed.iter().flat_map(AppendRecords::records).collect();
    assert!(
        !committed.iter().any(|record| matches!(record, DriverRecord::HeadMoved { .. })),
        "the stale swap must not commit"
    );
    let failed = committed.iter().filter(|record| matches!(record, DriverRecord::ReactionFailed { .. })).count();
    assert_eq!(failed, 1, "the re-derived batch refuses the raced swap");
}

#[test]
fn failed_reactor_records_reaction_failed_and_stays_live() {
    // Catches treating `Failed` like poison: the instance must stay live and
    // keep evaluating.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);

    let failed = Evaluated::Failed { seq: 3, reactor: reactor_name("r"), reason: Detail::new("boom") };
    world.evaluates.insert(3, failed);
    world.evaluates.insert(4, Evaluated::Completed { seq: 4, intents: Vec::new() });
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let failed = failed_records(&world);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, 3);
    assert!(failed[0].1.reactor.is_some());
    assert!(rejected_records(&world).is_empty());
    assert_eq!(world.warm_ranges_for(bundle_a), vec![(1, 2)]);
    assert_eq!(world.events_for(bundle_a), vec![3, 4]);
}

#[test]
fn refused_routing_append_aborts() {
    // Catches silently dropping a seq's records when the journal refuses them.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);

    world.fail_next = Some("journal exploded".to_string());
    let _manual = world.drive(commands);
    let abort = world.abort.clone().expect("a refused routing append aborts");
    assert!(abort.contains("routing batch"), "unexpected abort: {abort}");
}

#[test]
fn a_digest_serves_one_role() {
    // Catches a second load (`SubnameInUse`) or a cross-role invoke: a call
    // naming a reactor digest faults without loading it as a program.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    for seq in [3, 4, 5] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    assert!(manual.is_empty());

    let name = ProgramName::new("run").expect("valid program name");
    let origin = NativeOrigin::new("test.origin").expect("valid origin");
    let call = Call { program: program_head("a"), name, input: digest(9), origin, key: 1 };
    let (_caller, commands) = world.core.call(call);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.loads_seen, vec![bundle_a], "no second load as a program");
    let faults: Vec<_> = world
        .appends
        .iter()
        .flat_map(AppendRecords::records)
        .filter_map(|record| match record {
            DriverRecord::Fault { record, .. } => Some(record.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(faults.len(), 1);
    let FaultReason::BundleUnavailable { reason } = &faults[0].reason else {
        panic!("expected an unavailable fault, got {:?}", faults[0].reason);
    };
    assert!(reason.as_str().contains("reactor bundle"), "unexpected reason: {}", reason.as_str());
    assert_eq!(world.events_for(bundle_a), vec![3, 4, 5]);
}

#[test]
fn await_processed_waits_for_routing_and_its_appends() {
    // Catches answering before the batch lands or between a reaction's
    // `Requested` and its outcome: the barrier stays parked while the
    // reaction's invoke is still in flight.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    let wasm = program_bundle("test.program", Utf8Text::ID, OpaqueBytes::ID);
    let program_bundle = store_program(&mut world, &wasm);
    let input = world.store(Utf8Text::ID, b"input-text");
    script_closure(&mut world, input, vec![ClosureArtifact::new(Utf8Text::ID, b"input-text".to_vec())]);
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    world.seed_move("prog", program_bundle);
    for seq in [4, 5, 6] {
        world.evaluates.insert(seq, Evaluated::Completed { seq, intents: Vec::new() });
    }
    let manual = world.drive(commands);
    let [Command::Evaluate { ticket, .. }] = manual.as_slice() else {
        panic!("expected one held evaluate, got {manual:?}");
    };
    let ticket = *ticket;

    let caller = await_processed(&mut world, 5);
    assert!(processed_by(&world, caller).is_none(), "the barrier waits for routing");

    let intent = call_intent("r", "rule", "prog", "test.program", input);
    let reply = Evaluated::Completed { seq: 3, intents: vec![intent] };
    let follow = feed_evaluated(&mut world, ticket, reply);
    let manual = world.drive(follow);
    let ticket = invoke_ticket(&manual);
    assert!(processed_by(&world, caller).is_none(), "the barrier waits for the outcome");

    let result = artifact_digest(OpaqueBytes::ID, b"result-bytes");
    let staged = EncodedArtifact::opaque_bytes(b"result-bytes");
    let invoked = Invoked::Completed { seq: 5, result, staged: vec![staged] };
    let follow = world.core.on_invoked(ticket, invoked);
    let manual = world.drive(follow);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let replied = processed_by(&world, caller).expect("the barrier answers after the outcome");
    assert_eq!(replied.head, world.head());
}

#[test]
fn barrier_waits_for_the_seq_being_routed() {
    // Catches answering a barrier once routing `Heads` fold its seq, before
    // that seq's replies are collected and its batch appended.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    let manual = world.drive(commands);
    let [Command::Evaluate { ticket, .. }] = manual.as_slice() else {
        panic!("expected one held evaluate, got {manual:?}");
    };
    let ticket = *ticket;

    let caller = await_processed(&mut world, 3);
    assert!(processed_by(&world, caller).is_none(), "seq 3 is still being evaluated");

    let follow = feed_evaluated(&mut world, ticket, Evaluated::Completed { seq: 3, intents: Vec::new() });
    let manual = world.drive(follow);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(processed_by(&world, caller).map(|reply| reply.head), Some(world.head()));
}

#[test]
fn quiet_set_swap_activates_only_newly_selected_heads() {
    // Catches a set swap at a seq with no live digest activating every
    // selected head instead of only the newly selected ones.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let shared = world.store_reactor(b"shared", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", shared);
    world.evaluates.insert(8, Evaluated::Completed { seq: 8, intents: Vec::new() });
    let manual = world.drive(commands);
    let [Command::Evaluate { ticket, .. }] = manual.as_slice() else {
        panic!("expected one held evaluate, got {manual:?}");
    };
    let ticket = *ticket;

    let poisoned = Evaluated::Poisoned { seq: 3, last_trusted: 2, reason: Detail::new("boom") };
    let follow = feed_evaluated(&mut world, ticket, poisoned);
    let manual = world.drive(follow);
    assert!(manual.is_empty());

    let grown = reactor_set(&["a", "b"]);
    let grown_digest = world.store_set(&grown);
    let swap = RecordedHeadMove::new(RecordedHead::from(&ReactorSet::ROOT), grown_digest);
    let wake = world.append_external(None, &swap);
    let manual = world.drive(wake);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(activated_records(&world).len(), 1, "no head changed, none activates");
    assert_eq!(world.events_for(shared), vec![3]);

    let second = world.store_reactor(b"second", MailboxId(102));
    let member = RecordedHeadMove::new(RecordedHead::from(&program_head("b")), second);
    let wake = world.append_external(None, &member);
    let manual = world.drive(wake);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    let activated = activated_records(&world);
    assert_eq!(activated.len(), 2);
    assert_eq!(activated[1].1.head(), &program_head("b"));
    assert_eq!(world.warm_ranges_for(second), vec![(1, 7)]);
    assert_eq!(world.events_for(second), vec![8]);
}

#[test]
fn live_out_of_sequence_resyncs_by_status() {
    // Catches recording or skipping on a resync: with the cursor still at
    // `N-1`, the event is re-sent and nothing is recorded for the first try.
    let (mut world, commands) = World::open();
    let set = reactor_set(&["a"]);
    let set_digest = world.store_set(&set);
    let bundle_a = world.store_reactor(b"reactor-a", MailboxId(101));
    world.seed_set_root(set_digest);
    world.seed_move("a", bundle_a);
    let manual = world.drive(commands);
    let [Command::Evaluate { ticket, .. }] = manual.as_slice() else {
        panic!("expected one held evaluate, got {manual:?}");
    };
    let ticket = *ticket;

    let stale = Evaluated::OutOfSequence { seq: 3, expected: 3 };
    let follow = feed_evaluated(&mut world, ticket, stale);
    let manual = world.drive(follow);
    let [Command::QueryStatus { ticket, .. }] = manual.as_slice() else {
        panic!("expected one status query, got {manual:?}");
    };
    let ticket = *ticket;
    assert!(failed_records(&world).is_empty(), "a resync records nothing");

    let status = Status::new(2, false);
    let follow = world.core.on_status(ticket, &status);
    let manual = world.drive(follow);
    let [Command::Evaluate { ticket, request, .. }] = manual.as_slice() else {
        panic!("expected one re-sent evaluate, got {manual:?}");
    };
    let ticket = *ticket;
    assert_eq!(request.entry().seq, 3);

    let reply = Evaluated::Completed { seq: 3, intents: Vec::new() };
    let follow = feed_evaluated(&mut world, ticket, reply);
    let manual = world.drive(follow);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.events_for(bundle_a), vec![3, 3]);
    assert!(failed_records(&world).is_empty());
    assert!(rejected_records(&world).is_empty());
    assert_eq!(watch_count(&world), 1);
}
