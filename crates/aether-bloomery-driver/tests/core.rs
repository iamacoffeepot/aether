//! Scenario tests: the ADR-0226 program rules from scripted journal pages and replies.
//!
//! Each test names the bug it catches. Seqs are exact because the scripted
//! journal is fully deterministic: seeded moves and records take the first
//! seqs, and every core append lands where the test says it does.

mod program_world;
mod support;

use aether_bloomery_driver::{Command, InvokeTicket, LoadOutcome};
use aether_bloomery_kinds::{
    CallOutcome, CallRefusal, ClosureArtifact, Detail, Digest, DriverRecord, EncodedArtifact, ExecutorFault,
    FaultReason, Invoked, OpaqueBytes, ReadEventsResult, Ref, Utf8Text, artifact_digest,
};
use aether_data::Kind;
use program_world::{call, fault, requested, transition};
use support::{LIMIT_BYTES, World, bundle_wasm, digest};

const PROGRAM: &str = "test.program";
const HEAD: &str = "programs";
const ORIGIN: &str = "test.origin";

/// A test result kind citing one staged text, so a completed invocation's
/// staged set legitimately holds more than the result alone (ADR-0224 §3).
#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.program.cited.result")]
struct CitedResult {
    text: Ref<Utf8Text>,
}

/// The standard fixtures: a bundle declaring [`PROGRAM`] (`Utf8Text` in,
/// `OpaqueBytes` out) bound under [`HEAD`], with one text input's closure.
struct Fixtures {
    bundle: Digest,
    input: Digest,
    result: Digest,
    staged: EncodedArtifact,
}

fn fixtures(world: &mut World) -> Fixtures {
    let wasm = bundle_wasm(&[(PROGRAM, Utf8Text::ID, OpaqueBytes::ID, "run it")], &[], b"program");
    let bundle = world.store_bundle(&wasm);
    let input = world.store(Utf8Text::ID, b"input-text");
    world.script_closure(input, vec![ClosureArtifact::new(Utf8Text::ID, b"input-text".to_vec())]);
    let result = artifact_digest(OpaqueBytes::ID, b"result-bytes");
    let staged = EncodedArtifact::opaque_bytes(b"result-bytes");
    world.seed_move(HEAD, bundle);
    Fixtures { bundle, input, result, staged }
}

/// The one invoke ticket from a single manual command.
fn invoke_ticket(manual: &[Command]) -> InvokeTicket {
    let [Command::Invoke { ticket, .. }] = manual else {
        panic!("expected exactly one manual invoke, got {manual:?}");
    };
    *ticket
}

#[test]
fn startup_faults_prior_life_requests_once_and_invokes_nothing() {
    // Catches re-running prior-life requests (a crash loop), faulting
    // completed requests, and faulting twice.
    let (mut world, initial) = World::open();
    let bundle = digest(1);
    let input = digest(2);
    world.seed_move(HEAD, bundle);
    world.seed(None, &requested(bundle, PROGRAM, input, ORIGIN, 1));
    world.seed(None, &requested(bundle, PROGRAM, input, ORIGIN, 2));
    world.seed(None, &requested(bundle, PROGRAM, input, ORIGIN, 3));
    world.seed(Some(4), &fault(bundle, PROGRAM, input, FaultReason::Interrupted));
    let manual = world.drive(initial);
    assert!(manual.is_empty(), "startup needs no service replies: {manual:?}");
    assert!(world.abort.is_none());
    assert_eq!(world.appends.len(), 1, "one startup batch");
    let records = world.appends[0].records();
    assert_eq!(records.len(), 2, "exactly the two outstanding requests");
    assert!(matches!(
        &records[0],
        DriverRecord::Fault { cause: 2, record }
            if record.reason == FaultReason::Interrupted
    ));
    assert!(matches!(
        &records[1],
        DriverRecord::Fault { cause: 3, record }
            if record.reason == FaultReason::Interrupted
    ));
    assert_eq!(world.head(), 7, "no further append after the read-back");
    assert!(world.reads_seen.is_empty());
    assert!(world.loads_seen.is_empty());
    assert!(world.invokes_seen.is_empty());
    assert!(world.answers.is_empty());
}

#[test]
fn calls_wait_for_the_startup_pass() {
    // Catches a new request being folded in as outstanding and faulted `Interrupted`.
    let (mut world, initial) = World::open();
    let prior =
        world.store_bundle(&bundle_wasm(&[(PROGRAM, Utf8Text::ID, OpaqueBytes::ID, "run it")], &[], b"program"));
    let input = world.store(Utf8Text::ID, b"input-text");
    world.script_closure(input, vec![ClosureArtifact::new(Utf8Text::ID, b"input-text".to_vec())]);
    world.seed_move(HEAD, prior);
    world.seed(None, &requested(digest(1), PROGRAM, digest(2), ORIGIN, 9));
    let (caller, held) = world.core.call(call(HEAD, PROGRAM, input, ORIGIN, 1));
    assert!(held.is_empty(), "a call before the pass commits emits nothing");
    world.script_invoke(
        4,
        Invoked::Completed {
            seq: 4,
            result: artifact_digest(OpaqueBytes::ID, b"result-bytes"),
            staged: vec![EncodedArtifact::opaque_bytes(b"result-bytes")],
        },
    );
    let manual = world.drive(initial);
    assert!(manual.is_empty(), "the full run needs no manual replies: {manual:?}");
    assert!(world.abort.is_none());
    assert_eq!(world.appends.len(), 3, "startup fault, requested, transition");
    assert!(matches!(world.appends[0].records(), [DriverRecord::Fault { cause: 2, .. }]));
    assert!(matches!(world.appends[1].records(), [DriverRecord::Requested { cause: None, .. }]));
    assert_eq!(world.answers.len(), 1);
    let (answered, outcome) = &world.answers[0];
    assert_eq!(*answered, caller);
    assert!(matches!(outcome, CallOutcome::Transition { seq: 5, .. }));
}

#[test]
fn unbound_head_is_refused_without_a_write() {
    // Catches recording a `Requested` that pins no bundle.
    let (mut world, initial) = World::open();
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (caller, commands) = world.core.call(call("missing", PROGRAM, digest(2), ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert!(world.appends.is_empty(), "a refusal records nothing");
    assert_eq!(world.answers.as_slice(), [(caller, CallOutcome::Refused { key: 1, reason: CallRefusal::HeadUnbound })]);
}

#[test]
fn repeated_key_answers_from_the_recorded_outcome() {
    // Catches re-running on retry.
    let (mut world, initial) = World::open();
    let bundle = digest(1);
    let input = digest(2);
    let result = digest(3);
    world.seed_move(HEAD, bundle);
    world.seed(None, &requested(bundle, PROGRAM, input, ORIGIN, 1));
    world.seed(Some(2), &transition(bundle, PROGRAM, input, result));
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    assert!(world.appends.is_empty(), "a clean history recovers with no writes");
    let (caller, commands) = world.core.call(call(HEAD, PROGRAM, input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.appends.is_empty(), "a repeat records nothing");
    assert_eq!(
        world.answers.as_slice(),
        [(caller, CallOutcome::Transition { key: 1, seq: 3, transition: transition(bundle, PROGRAM, input, result) })]
    );
}

#[test]
fn repeated_key_while_in_flight_shares_one_request() {
    // Catches a duplicate `Requested`, which the fold would reject.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    world.script_invoke(2, Invoked::Completed { seq: 2, result: fixed.result, staged: vec![fixed.staged.clone()] });
    let (first, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let (second, shared) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    assert!(shared.is_empty(), "the repeat joins the unrecorded request");
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(world.appends.len(), 2, "one requested and its transition");
    assert!(matches!(world.appends[0].records(), [DriverRecord::Requested { cause: None, .. }]));
    assert_eq!(world.answers.len(), 2);
    assert_eq!(world.answers[0].0, first);
    assert_eq!(world.answers[1].0, second);
    assert!(matches!(&world.answers[0].1, CallOutcome::Transition { key: 1, seq: 3, .. }));
    assert_eq!(world.answers[0].1, world.answers[1].1, "both callers get the same outcome");
}

#[test]
fn reused_key_for_a_different_request_is_refused_without_a_write() {
    // Catches recording a second request under one idempotency key.
    let (mut world, initial) = World::open();
    let bundle = digest(1);
    world.seed_move(HEAD, bundle);
    world.seed(None, &requested(bundle, PROGRAM, digest(2), ORIGIN, 1));
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    assert_eq!(world.appends.len(), 1, "only the startup fault");
    let (caller, commands) = world.core.call(call(HEAD, "test.other", digest(2), ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(world.appends.len(), 1, "a reused key records nothing");
    assert_eq!(world.answers.as_slice(), [(caller, CallOutcome::Refused { key: 1, reason: CallRefusal::KeyReused })]);
}

#[test]
fn fence_conflict_refolds_and_resolves_the_head_again() {
    // Catches pinning a stale head and writing under a stale fence.
    let (mut world, initial) = World::open();
    let stale = digest(1);
    let fresh = digest(2);
    world.seed_move(HEAD, stale);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (caller, commands) = world.core.call(call(HEAD, PROGRAM, digest(3), ORIGIN, 1));
    world.seed_move(HEAD, fresh);
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(world.appends.len(), 3, "stale attempt, retried requested, and its fault");
    let pinned = |index: usize| {
        let [DriverRecord::Requested { record, .. }] = world.appends[index].records() else {
            panic!("expected one requested at append {index}, got {:?}", world.appends[index].records());
        };
        record.program.bundle()
    };
    assert_eq!(pinned(0), stale, "the first attempt pins the stale head");
    assert_eq!(pinned(1), fresh, "the retry pins the head after refolding");
    assert_eq!(world.answers.len(), 1);
    assert_eq!(world.answers[0].0, caller);
    assert!(
        matches!(&world.answers[0].1, CallOutcome::Fault { fault, .. } if matches!(fault.reason, FaultReason::BundleUnavailable { .. }))
    );
}

#[test]
fn one_invoke_in_flight_per_root() {
    // Catches concurrent invocations and double loads of one digest.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let other = world.store(Utf8Text::ID, b"other-text");
    world.script_closure(other, vec![ClosureArtifact::new(Utf8Text::ID, b"other-text".to_vec())]);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (_, first) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(first);
    let ticket = invoke_ticket(&manual);
    assert_eq!(world.reads_seen, [fixed.bundle], "one bundle read");
    assert_eq!(world.loads_seen, [fixed.bundle], "one load");
    let (_, second) = world.core.call(call(HEAD, PROGRAM, other, ORIGIN, 2));
    let manual = world.drive(second);
    assert!(manual.is_empty(), "the second request waits its turn");
    assert_eq!(world.invokes_seen.len(), 1, "no second invoke while one is in flight");
    let reply =
        world.core.on_invoked(ticket, Invoked::Completed { seq: 2, result: fixed.result, staged: vec![fixed.staged] });
    let manual = world.drive(reply);
    let second_ticket = invoke_ticket(&manual);
    assert_eq!(world.reads_seen.len(), 1, "the bundle is never re-read");
    assert_eq!(world.loads_seen.len(), 1, "the bundle is never reloaded");
    assert_eq!(world.invokes_seen.len(), 2);
    assert_eq!(world.invokes_seen[0].0, fixed.bundle);
    assert_eq!(world.invokes_seen[1].0, fixed.bundle);
    let other_result = artifact_digest(OpaqueBytes::ID, b"other-result");
    let reply = world.core.on_invoked(
        second_ticket,
        Invoked::Completed {
            seq: 3,
            result: other_result,
            staged: vec![EncodedArtifact::opaque_bytes(b"other-result")],
        },
    );
    let manual = world.drive(reply);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(world.answers.len(), 2);
}

#[test]
fn unknown_program_faults_before_closure_or_load() {
    // Catches a closure read or load for a program the bundle never declared.
    let (mut world, initial) = World::open();
    let bundle =
        world.store_bundle(&bundle_wasm(&[("test.other", Utf8Text::ID, OpaqueBytes::ID, "other")], &[], b"program"));
    world.seed_move(HEAD, bundle);
    let input = world.store(Utf8Text::ID, b"input-text");
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (caller, commands) = world.core.call(call(HEAD, PROGRAM, input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.closures_seen.is_empty(), "no closure read for an unknown program");
    assert!(world.loads_seen.is_empty(), "no load for an unknown program");
    assert_eq!(world.answers.len(), 1);
    assert_eq!(world.answers[0].0, caller);
    let CallOutcome::Fault { fault, .. } = &world.answers[0].1 else {
        panic!("expected a fault, got {:?}", world.answers[0].1);
    };
    let FaultReason::BundleUnavailable { reason } = &fault.reason else {
        panic!("expected an unavailable bundle, got {:?}", fault.reason);
    };
    assert!(reason.as_str().contains(PROGRAM), "names the unknown program: {reason:?}");
}

#[test]
fn wrong_input_kind_faults_before_load() {
    // Catches loading a bundle for an input that fails its declaration check.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let input = artifact_digest(OpaqueBytes::ID, b"opaque-input");
    world.script_closure(input, vec![ClosureArtifact::new(OpaqueBytes::ID, b"opaque-input".to_vec())]);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(world.reads_seen, [fixed.bundle], "the bundle is read");
    assert_eq!(world.closures_seen, [input], "the closure is read");
    assert!(world.loads_seen.is_empty(), "a kind mismatch loads nothing");
    assert_eq!(world.answers.len(), 1);
    assert!(
        matches!(&world.answers[0].1, CallOutcome::Fault { fault, .. } if matches!(fault.reason, FaultReason::BundleUnavailable { .. }))
    );
}

#[test]
fn oversized_closure_faults_with_the_limit_and_loads_nothing() {
    // Catches loading a bundle for an input whose closure exceeds the cap,
    // and misreporting the limit the closure exceeded.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    world.script_oversized(fixed.input);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.loads_seen.is_empty(), "an oversized closure loads nothing");
    assert_eq!(world.answers.len(), 1);
    assert!(matches!(
        &world.answers[0].1,
        CallOutcome::Fault { fault, .. }
            if fault.reason == FaultReason::ClosureTooLarge { limit_bytes: LIMIT_BYTES }
    ));
}

#[test]
fn missing_closure_member_faults_input_missing() {
    // Catches treating a dangling input digest as loadable.
    let (mut world, initial) = World::open();
    let bundle =
        world.store_bundle(&bundle_wasm(&[(PROGRAM, Utf8Text::ID, OpaqueBytes::ID, "run it")], &[], b"program"));
    world.seed_move(HEAD, bundle);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, digest(9), ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.loads_seen.is_empty(), "a missing input loads nothing");
    assert_eq!(world.answers.len(), 1);
    assert!(
        matches!(&world.answers[0].1, CallOutcome::Fault { fault, .. } if fault.reason == FaultReason::InputMissing)
    );
}

#[test]
fn failed_load_faults_and_is_never_retried() {
    // Catches a reload loop over a deterministically failing digest, and a
    // request stranded in the queue behind the failed load.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    world.hold_load(fixed.bundle);
    let manual = world.drive(initial);
    assert!(manual.is_empty());

    let (_, first) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(first);
    let [Command::Load { ticket, .. }] = manual.as_slice() else {
        panic!("expected exactly one manual load, got {manual:?}");
    };
    let ticket = *ticket;
    let (_, queued) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 2));
    let manual = world.drive(queued);
    assert!(manual.is_empty(), "the queued request waits behind the load");

    let failed = world.core.on_loaded(ticket, LoadOutcome::Failed { error: "boom".to_string() });
    let manual = world.drive(failed);
    assert!(manual.is_empty());
    let (_, later) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 3));
    let manual = world.drive(later);
    assert!(manual.is_empty());

    assert!(world.abort.is_none());
    assert_eq!(world.reads_seen, [fixed.bundle], "the bundle is read once");
    assert_eq!(world.loads_seen, [fixed.bundle], "the bundle is loaded once");
    assert_eq!(world.answers.len(), 3);
    for (_, outcome) in &world.answers {
        let CallOutcome::Fault { fault, .. } = outcome else {
            panic!("expected a fault, got {outcome:?}");
        };
        let FaultReason::BundleUnavailable { reason } = &fault.reason else {
            panic!("expected an unavailable bundle, got {:?}", fault.reason);
        };
        assert_eq!(reason.as_str(), "boom", "the recorded reason is reused");
    }
}

#[test]
fn repeat_while_the_outcome_is_written_waits_instead_of_reinvoking() {
    // Catches a retry that arrives between `Invoked` and the outcome's
    // read-back starting the request's pipeline again (a second invocation).
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (first, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    let ticket = invoke_ticket(&manual);

    let mut commands =
        world.core.on_invoked(ticket, Invoked::Completed { seq: 2, result: fixed.result, staged: vec![fixed.staged] });
    let (repeat, repeated) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    commands.extend(repeated);
    let manual = world.drive(commands);

    assert!(manual.is_empty(), "no second invoke: {manual:?}");
    assert!(world.abort.is_none());
    assert_eq!(world.invokes_seen.len(), 1);
    assert_eq!(world.closures_seen.len(), 1);
    assert_eq!(world.answers.len(), 2);
    assert_eq!(world.answers[0].0, first);
    assert_eq!(world.answers[1].0, repeat);
    assert!(matches!(&world.answers[0].1, CallOutcome::Transition { key: 1, seq: 3, .. }));
    assert_eq!(world.answers[0].1, world.answers[1].1, "both callers get the recorded outcome");
}

#[test]
fn mismatched_result_kind_is_a_protocol_violation_that_stages_nothing() {
    // Catches recording a `Transition` that contradicts its declaration.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let staged = EncodedArtifact::text("wrong-kind");
    let result = artifact_digest(Utf8Text::ID, b"wrong-kind");
    world.script_invoke(2, Invoked::Completed { seq: 2, result, staged: vec![staged] });
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(world.appends.len(), 2);
    assert!(world.appends[1].artifacts().is_empty(), "a violation stages nothing");
    assert!(matches!(
        world.appends[1].records(),
        [DriverRecord::Fault { cause: 2, record }]
            if matches!(record.reason, FaultReason::ProtocolViolation { .. })
    ));
    assert_eq!(world.answers.len(), 1);
    assert!(
        matches!(&world.answers[0].1, CallOutcome::Fault { fault, .. } if matches!(fault.reason, FaultReason::ProtocolViolation { .. }))
    );
}

#[test]
fn result_citing_a_staged_text_records_every_staged_artifact() {
    // Catches faulting a completed invocation for staging more than the
    // result alone, when ADR-0224 §3 allows any staged set reachable from it.
    let (mut world, initial) = World::open();
    let bundle =
        world.store_bundle(&bundle_wasm(&[(PROGRAM, Utf8Text::ID, CitedResult::ID, "run it")], &[], b"program"));
    let input = world.store(Utf8Text::ID, b"input-text");
    world.script_closure(input, vec![ClosureArtifact::new(Utf8Text::ID, b"input-text".to_vec())]);
    world.seed_move(HEAD, bundle);
    let manual = world.drive(initial);
    assert!(manual.is_empty());

    let text = EncodedArtifact::text("derived");
    let result_value = CitedResult { text: Ref::from_digest(text.digest()) };
    let result = EncodedArtifact::new(&result_value).expect("encode cited result");
    world.script_invoke(
        2,
        Invoked::Completed { seq: 2, result: result.digest(), staged: vec![text.clone(), result.clone()] },
    );
    let (caller, commands) = world.core.call(call(HEAD, PROGRAM, input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());

    assert_eq!(world.appends.len(), 2);
    assert_eq!(world.appends[1].artifacts(), [text, result.clone()]);
    let expected = transition(bundle, PROGRAM, input, result.digest());
    assert_eq!(world.appends[1].records(), [DriverRecord::Transition { cause: 2, record: expected.clone() }]);
    assert_eq!(world.answers.as_slice(), [(caller, CallOutcome::Transition { key: 1, seq: 3, transition: expected })]);
}

#[test]
fn staged_artifact_unreachable_from_the_result_is_a_protocol_violation_that_stages_nothing() {
    // Catches recording an orphan from a bundle whose own `refuse_orphans`
    // check was skipped or lied about — the driver re-checks it natively.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let orphan = EncodedArtifact::text("orphan");
    world.script_invoke(
        2,
        Invoked::Completed { seq: 2, result: fixed.result, staged: vec![fixed.staged.clone(), orphan.clone()] },
    );
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(world.appends.len(), 2);
    assert!(world.appends[1].artifacts().is_empty(), "a violation stages nothing");
    let CallOutcome::Fault { fault, .. } = &world.answers[0].1 else {
        panic!("expected a fault, got {:?}", world.answers[0].1);
    };
    let FaultReason::ProtocolViolation { reason } = &fault.reason else {
        panic!("expected a protocol violation, got {:?}", fault.reason);
    };
    assert!(reason.as_str().contains(&orphan.digest().to_string()), "names the orphan digest: {reason:?}");
}

#[test]
fn result_absent_from_staged_is_a_protocol_violation() {
    // Catches recording a `Transition` whose result blob was never written.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let missing_result = digest(200);
    world.script_invoke(2, Invoked::Completed { seq: 2, result: missing_result, staged: vec![fixed.staged.clone()] });
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(world.appends.len(), 2);
    assert!(world.appends[1].artifacts().is_empty(), "a violation stages nothing");
    let CallOutcome::Fault { fault, .. } = &world.answers[0].1 else {
        panic!("expected a fault, got {:?}", world.answers[0].1);
    };
    let FaultReason::ProtocolViolation { reason } = &fault.reason else {
        panic!("expected a protocol violation, got {:?}", fault.reason);
    };
    assert!(reason.as_str().contains(&missing_result.to_string()), "names the missing result: {reason:?}");
}

#[test]
fn duplicate_staged_digest_is_a_protocol_violation() {
    // Catches a hostile staged list that repeats a blob rather than citing it.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    world.script_invoke(
        2,
        Invoked::Completed { seq: 2, result: fixed.result, staged: vec![fixed.staged.clone(), fixed.staged.clone()] },
    );
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert_eq!(world.appends.len(), 2);
    assert!(world.appends[1].artifacts().is_empty(), "a violation stages nothing");
    let CallOutcome::Fault { fault, .. } = &world.answers[0].1 else {
        panic!("expected a fault, got {:?}", world.answers[0].1);
    };
    let FaultReason::ProtocolViolation { reason } = &fault.reason else {
        panic!("expected a protocol violation, got {:?}", fault.reason);
    };
    assert!(reason.as_str().contains(&fixed.staged.digest().to_string()), "names the duplicated digest: {reason:?}");
}

#[test]
fn invoked_seq_mismatch_and_rejection_are_protocol_violations() {
    // Catches attributing another request's reply, and swallowing a rejection.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    world.script_invoke(2, Invoked::Completed { seq: 999, result: fixed.result, staged: vec![fixed.staged.clone()] });
    let (_, first) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(first);
    assert!(manual.is_empty());
    world.script_invoke(4, Invoked::Rejected { seq: 4, reason: Detail::new("nope") });
    let (_, second) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 2));
    let manual = world.drive(second);
    assert!(manual.is_empty());
    assert_eq!(world.answers.len(), 2);
    let CallOutcome::Fault { fault: first, .. } = &world.answers[0].1 else {
        panic!("expected a fault, got {:?}", world.answers[0].1);
    };
    let FaultReason::ProtocolViolation { reason } = &first.reason else {
        panic!("expected a protocol violation, got {:?}", first.reason);
    };
    assert!(reason.as_str().contains("999"), "names the stray seq: {reason:?}");
    let CallOutcome::Fault { fault: second, .. } = &world.answers[1].1 else {
        panic!("expected a fault, got {:?}", world.answers[1].1);
    };
    let FaultReason::ProtocolViolation { reason } = &second.reason else {
        panic!("expected a protocol violation, got {:?}", second.reason);
    };
    assert_eq!(reason.as_str(), "nope", "a rejection keeps its reason");
}

#[test]
fn an_executor_fault_records_its_fault_reason_caused_by_the_request_and_no_transition() {
    // Catches a driver that treats `Faulted` as a protocol violation, drops
    // it, maps it to the wrong reason, or loses the failure's reason text.
    let reason =
        Detail::new("reading the daemon's platform: connecting to the Docker daemon failed (entity not found)");
    let cases = [
        (ExecutorFault::TimedOut, FaultReason::TimedOut),
        (ExecutorFault::ResourceExhausted, FaultReason::ResourceExhausted),
        (ExecutorFault::Failed { reason: reason.clone() }, FaultReason::ExecutorFailed { reason }),
    ];
    for (executor, expected) in cases {
        let (mut world, initial) = World::open();
        let fixed = fixtures(&mut world);
        assert!(world.drive(initial).is_empty());
        world.script_invoke(2, Invoked::Faulted { seq: 2, fault: executor });
        let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
        assert!(world.drive(commands).is_empty());
        assert!(world.abort.is_none());
        let record = fault(fixed.bundle, PROGRAM, fixed.input, expected);
        assert_eq!(world.appends.len(), 2, "requested, then the fault alone");
        assert!(world.appends[1].artifacts().is_empty());
        assert_eq!(world.appends[1].records(), [DriverRecord::Fault { cause: 2, record: record.clone() }]);
        assert!(matches!(&world.answers[..], [(_, CallOutcome::Fault { seq: 3, fault, .. })] if *fault == record));
    }

    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    assert!(world.drive(initial).is_empty());
    world.script_invoke(2, Invoked::Faulted { seq: 999, fault: ExecutorFault::TimedOut });
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    assert!(world.drive(commands).is_empty());
    assert!(matches!(
        &world.answers[..],
        [(_, CallOutcome::Fault { fault, .. })] if matches!(fault.reason, FaultReason::ProtocolViolation { .. })
    ));
}

#[test]
fn completed_invocation_appends_staged_artifacts_with_a_caused_transition() {
    // Catches a broken cause link, a wrong outcome seq, and artifacts left unstaged.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    world.script_invoke(2, Invoked::Completed { seq: 2, result: fixed.result, staged: vec![fixed.staged.clone()] });
    let (caller, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(world.appends.len(), 2);
    assert_eq!(world.appends[1].artifacts().len(), 1);
    assert_eq!(world.appends[1].artifacts()[0], fixed.staged);
    let expected = transition(fixed.bundle, PROGRAM, fixed.input, fixed.result);
    assert_eq!(world.appends[1].records(), [DriverRecord::Transition { cause: 2, record: expected.clone() }]);
    assert_eq!(world.answers.as_slice(), [(caller, CallOutcome::Transition { key: 1, seq: 3, transition: expected })]);
    assert_eq!(world.invokes_seen.len(), 1);
    let (bundle, invoke) = &world.invokes_seen[0];
    assert_eq!(*bundle, fixed.bundle);
    assert_eq!(invoke.seq(), 2);
    assert_eq!(invoke.program().as_str(), PROGRAM);
    assert_eq!(invoke.input(), fixed.input);
    assert_eq!(invoke.closure().len(), 1);
}

#[test]
fn refused_transition_append_records_a_protocol_violation() {
    // Catches dropping a completed invocation whose artifacts the journal refused.
    let (mut world, initial) = World::open();
    let fixed = fixtures(&mut world);
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let (_, commands) = world.core.call(call(HEAD, PROGRAM, fixed.input, ORIGIN, 1));
    let manual = world.drive(commands);
    let ticket = invoke_ticket(&manual);
    world.fail_next = Some("citation unverified".to_string());
    let reply = world
        .core
        .on_invoked(ticket, Invoked::Completed { seq: 2, result: fixed.result, staged: vec![fixed.staged.clone()] });
    let manual = world.drive(reply);
    assert!(manual.is_empty());
    assert!(world.abort.is_none());
    assert_eq!(world.appends.len(), 3, "requested, refused transition, violation fault");
    assert!(matches!(world.appends[1].records(), [DriverRecord::Transition { cause: 2, .. }]));
    let expected = fault(
        fixed.bundle,
        PROGRAM,
        fixed.input,
        FaultReason::ProtocolViolation { reason: Detail::new("citation unverified") },
    );
    assert_eq!(world.appends[2].records(), [DriverRecord::Fault { cause: 2, record: expected }]);
    assert_eq!(world.answers.len(), 1);
    assert!(matches!(
        &world.answers[0].1,
        CallOutcome::Fault { seq: 3, fault, .. }
            if matches!(fault.reason, FaultReason::ProtocolViolation { .. })
    ));
}

#[test]
fn unreadable_journal_aborts() {
    // Catches running on an untrusted view.
    let (mut world, initial) = World::open();
    world.fail_reads = Some("disk gone".to_string());
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    assert_eq!(world.abort.as_deref(), Some("journal read failed: disk gone"));
    assert!(world.answers.is_empty());
    assert!(world.appends.is_empty());
}

#[test]
fn replayed_ticket_returns_no_commands() {
    // Catches answering a reply the core already consumed.
    let (mut world, initial) = World::open();
    let [Command::ReadEvents { ticket, .. }] = initial.as_slice() else {
        panic!("expected one initial read, got {initial:?}");
    };
    let ticket = *ticket;
    let manual = world.drive(initial);
    assert!(manual.is_empty());
    let replay = world.core.on_events(ticket, ReadEventsResult::Ok { after: 0, head: 0, entries: vec![] });
    assert!(replay.is_empty(), "a consumed ticket answers nothing");
    assert!(world.abort.is_none());
    assert!(world.appends.is_empty());
}
