//! Contract monotonicity across a republish (ADR-0231 §5, ADR-0241 §4).
//!
//! A successor module that drops or changes a handler row of a namespace its
//! predecessor publishes is refused by the admission preview before any live
//! instance prepares, so the old module keeps serving; a successor that only
//! adds rows is republished, and the added row then binds the next
//! republish. A `#[fallback]` counts like a row: one may be added, and a
//! successor that drops it is refused. A republish after the only instance
//! was dropped still publishes, and the dropped name stays spent (ADR-0241
//! §8). The fixtures are the `test.republish.subject` successors of issue
//! 7109.

use std::fs;

use aether_actor::ProtocolRef;
use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::Bump;

const SUBJECT: &str = "test.republish.subject";
const COUNT_QUERY: &str = "aether.test_fixtures.count_query";
const INLINE_PROBE: &str = "aether.test_fixtures.inline_probe";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";

/// The subject's row every successor here keeps: a silent `Bump`. Every
/// successor is a cdylib example, so the fixture casts its `load_any`
/// reference to this instead of naming a type.
#[aether_actor::protocol]
trait SubjectBump {
    fn bump(mail: Bump);
}

struct Fixture {
    harness: SubstrateHarness,
    subject: ErasedActorPath,
    /// The subject's reference, cast from the load reply's stamped sender.
    subject_ref: ProtocolRef<SubjectBump>,
}

/// The `republish_subject_<variant>` module's bytes, or `None` to skip.
fn subject_wasm(variant: &str) -> Option<Vec<u8>> {
    Some(fs::read(require_wasm(&format!("republish_subject_{variant}"))?).expect("read fixture wasm"))
}

impl Fixture {
    /// Boot a harness and load the base subject.
    fn start() -> Option<Self> {
        let wasm = subject_wasm("base")?;
        let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

        let load = LoadComponent { wasm, name: None, config: Vec::new(), export: None };
        let (subject_ref, subject) =
            harness.load_any(&load).unwrap_or_else(|error| panic!("the base must load: {error}"));
        let subject_ref = harness.cast::<SubjectBump>(subject_ref).expect("the base subject publishes Bump");

        Some(Self { harness, subject, subject_ref })
    }

    /// Republish the `variant` successor, returning the host's verdict.
    fn replace(&mut self, label: &str, variant: &str) -> ReplaceResult {
        let wasm = subject_wasm(variant).expect("the successor fixture is built with the base");
        let replace = ReplaceComponent { wasm, configs: Vec::new() };
        let operation = HarnessOp::send_and_await_reply(&self.harness.actor_ref::<ComponentHostCapability>(), &replace);
        let result = self.harness.execute(vec![(label, operation)]).expect("replace operation");
        result.reply::<ReplaceResult>(label).expect("decode ReplaceResult")
    }

    /// Bump the subject and assert it still serves.
    fn assert_serves(&mut self, why: &str) {
        let baseline = self.harness.count_observed(TICK_OBSERVED);
        let bump = HarnessOp::send_and_settle(&self.subject_ref, &Bump);
        self.harness.execute(vec![("bump", bump)]).expect("bump the subject");
        assert_eq!(self.harness.count_observed(TICK_OBSERVED), baseline + 1, "{why}");
    }
}

/// The admission refusal for a successor that drops or changes `kind`'s row.
fn narrowing(kind: &str) -> String {
    format!("replace refused: module publish refused: {SUBJECT} drops or changes its row for {kind}")
}

// Catches: the admission preview skipped, so a narrowing successor reaches
// the members and the old module stops serving a row its callers rely on.
#[test]
fn a_replace_that_drops_a_row_is_refused_and_the_old_module_keeps_serving() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    let ReplaceResult::Err { error } = fixture.replace("replace-dropped", "dropped") else {
        panic!("a replace that drops a row must be refused");
    };
    assert!(
        error.starts_with(&narrowing(COUNT_QUERY)),
        "the refusal names the namespace and the dropped kind: {error}"
    );

    fixture.assert_serves("the old module must still serve");
}

#[test]
fn a_replace_that_changes_a_reply_is_refused() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    let ReplaceResult::Err { error } = fixture.replace("replace-changed", "changed") else {
        panic!("a replace that changes a reply must be refused");
    };
    assert!(
        error.starts_with(&narrowing(COUNT_QUERY)),
        "the refusal names the namespace and the changed kind: {error}"
    );
}

// Catches: the successor's added rows not becoming the published contract,
// so a later successor could drop them unrefused.
#[test]
fn an_added_row_is_accepted_and_then_binds() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    if let ReplaceResult::Err { error } = fixture.replace("replace-extended", "extended") {
        panic!("a replace that only adds rows must succeed: {error}");
    }

    // The predecessor is now the extended module, so returning to the base
    // drops the row the extension added.
    let ReplaceResult::Err { error } = fixture.replace("replace-back", "base") else {
        panic!("a replace that drops the added row must be refused");
    };
    assert!(error.starts_with(&narrowing(INLINE_PROBE)), "the added row binds the next replace: {error}");
}

/// Catches a drop that leaves an empty slot a republish can refill: the
/// republish publishes with no live instance to move, and the dropped name
/// stays spent.
#[test]
fn a_replace_after_drop_republishes_and_the_dropped_name_stays_spent() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    let drop = DropComponent { target: fixture.subject.clone() };
    let operation = HarnessOp::send_and_await_reply(&fixture.harness.actor_ref::<ComponentHostCapability>(), &drop);
    let dropped = fixture.harness.execute(vec![("drop", operation)]).expect("drop operation");
    if let DropResult::Err { error } = dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("the subject must drop: {error}");
    }

    let ReplaceResult::Ok { types } = fixture.replace("replace-extended", "extended") else {
        panic!("a republish with no live instance must still publish");
    };
    assert!(types.iter().any(|replaced| replaced.namespace == SUBJECT), "the republish reports {SUBJECT}: {types:?}");

    let listed = fixture.harness.list_components().expect("list components");
    assert!(
        !listed.iter().any(|name| name == fixture.subject.as_str()),
        "the republish refills no dropped name: {listed:?}",
    );
}

#[test]
fn an_added_fallback_is_accepted_and_a_replace_that_drops_it_is_refused() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    if let ReplaceResult::Err { error } = fixture.replace("replace-fallback", "fallback") {
        panic!("a replace that only adds a fallback must succeed: {error}");
    }

    // The extended module keeps every row and adds one, so only the dropped
    // fallback breaks the contract.
    let ReplaceResult::Err { error } = fixture.replace("replace-extended", "extended") else {
        panic!("a replace that drops the fallback must be refused");
    };
    assert!(
        error.starts_with(&format!("replace refused: module publish refused: {SUBJECT}")) && error.contains("fallback"),
        "the refusal names the namespace and the fallback: {error}",
    );

    fixture.assert_serves("the fallback guest must still serve");
}
