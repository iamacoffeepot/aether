//! Contract monotonicity across replace (ADR-0231 §5, issue #6444).
//!
//! A replacement whose hosted type drops or changes a handler row of the
//! type the slot hosts is refused with `ReplaceResult::Err` before the old
//! instance is touched, so the old module keeps serving; a replacement that
//! only adds rows succeeds, and the added row then binds the next replace. A
//! replace after `DropComponent` is refused, because the drop closed the
//! instance (ADR-0241 §8). A `#[fallback]` counts like a row: one may be added, and a replacement that
//! drops it is refused. The fixtures are the bundle's `test.contract.*`
//! exports.

use std::fs;

use aether_actor::ErasedActorRef;
use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::Bump;

const BASE_EXPORT: &str = "test.contract.base";
const DROPPED_EXPORT: &str = "test.contract.dropped";
const CHANGED_EXPORT: &str = "test.contract.changed";
const EXTENDED_EXPORT: &str = "test.contract.extended";
const FALLBACK_EXPORT: &str = "test.contract.fallback";
const COUNT_QUERY: &str = "aether.test_fixtures.count_query";
const INLINE_PROBE: &str = "aether.test_fixtures.inline_probe";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";

struct Fixture {
    harness: SubstrateHarness,
    wasm: Vec<u8>,
    victim: ErasedActorPath,
    /// The victim's reference, the load reply's stamped sender.
    victim_ref: ErasedActorRef,
}

impl Fixture {
    /// Boot a harness and load `test.contract.base`, the victim.
    fn start() -> Option<Self> {
        let wasm = fs::read(require_wasm("aether_test_fixtures_bundle")?).expect("read fixture wasm");
        let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

        let load =
            LoadComponent { wasm: wasm.clone(), name: None, config: Vec::new(), export: Some(BASE_EXPORT.to_owned()) };
        let (victim_ref, victim) =
            harness.load_any(&load).unwrap_or_else(|error| panic!("the base must load: {error}"));

        Some(Self { harness, wasm, victim, victim_ref })
    }

    fn replace(&mut self, label: &str, export: &str) -> ReplaceResult {
        let replace = ReplaceComponent {
            target: self.victim.clone(),
            wasm: self.wasm.clone(),
            drain_timeout_ms: None,
            config: Vec::new(),
            export: Some(export.to_owned()),
        };
        let operation = HarnessOp::send_and_await_reply(&self.harness.actor_ref::<ComponentHostCapability>(), &replace);
        let result = self.harness.execute(vec![(label, operation)]).expect("replace operation");
        result.reply::<ReplaceResult>(label).expect("decode ReplaceResult")
    }

    fn refusal(&self, kind: &str) -> String {
        format!("{} replacement changes its contract for {kind}", self.victim)
    }

    fn trampoline(&self) -> ErasedActorRef {
        self.victim_ref
    }
}

#[test]
fn a_replace_that_drops_a_row_is_refused_and_the_old_module_keeps_serving() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    let ReplaceResult::Err { error } = fixture.replace("replace-dropped", DROPPED_EXPORT) else {
        panic!("a replace that drops a row must be refused");
    };
    assert_eq!(error, fixture.refusal(COUNT_QUERY), "the refusal names the actor and the dropped kind");

    let victim = fixture.trampoline();
    let baseline = fixture.harness.count_observed(TICK_OBSERVED);
    fixture.harness.execute(vec![("bump", HarnessOp::send_and_settle(victim, &Bump))]).expect("bump the victim");
    assert_eq!(fixture.harness.count_observed(TICK_OBSERVED), baseline + 1, "the old module must still serve");
}

#[test]
fn a_replace_that_changes_a_reply_is_refused() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    let ReplaceResult::Err { error } = fixture.replace("replace-changed", CHANGED_EXPORT) else {
        panic!("a replace that changes a reply must be refused");
    };
    assert_eq!(error, fixture.refusal(COUNT_QUERY), "the refusal names the actor and the changed kind");
}

#[test]
fn an_added_row_is_accepted_and_then_binds() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    if let ReplaceResult::Err { error } = fixture.replace("replace-extended", EXTENDED_EXPORT) {
        panic!("a replace that only adds rows must succeed: {error}");
    }

    // The predecessor is now the extended type, so returning to the base
    // drops the row the extension added.
    let ReplaceResult::Err { error } = fixture.replace("replace-back", BASE_EXPORT) else {
        panic!("a replace that drops the added row must be refused");
    };
    assert_eq!(error, fixture.refusal(INLINE_PROBE), "the added row binds the next replace");
}

/// Catches a drop that leaves an empty slot a replace can refill: the
/// dropped instance is closed, so the host refuses the replace.
#[test]
fn a_replace_after_drop_is_refused() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    let drop = DropComponent { target: fixture.victim.clone() };
    let operation = HarnessOp::send_and_await_reply(&fixture.harness.actor_ref::<ComponentHostCapability>(), &drop);
    let dropped = fixture.harness.execute(vec![("drop", operation)]).expect("drop operation");
    if let DropResult::Err { error } = dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("the victim must drop: {error}");
    }

    let ReplaceResult::Err { error } = fixture.replace("replace-dropped", BASE_EXPORT) else {
        panic!("a replace after drop must be refused");
    };
    assert!(error.contains(fixture.victim.as_str()), "the refusal names the dropped path: {error}");
}

#[test]
fn an_added_fallback_is_accepted_and_a_replace_that_drops_it_is_refused() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    if let ReplaceResult::Err { error } = fixture.replace("replace-fallback", FALLBACK_EXPORT) {
        panic!("a replace that only adds a fallback must succeed: {error}");
    }

    // The extended type keeps every row and adds one, so only the dropped
    // fallback breaks the contract.
    let ReplaceResult::Err { error } = fixture.replace("replace-extended", EXTENDED_EXPORT) else {
        panic!("a replace that drops the fallback must be refused");
    };
    assert_eq!(error, format!("{} replacement drops its fallback", fixture.victim), "the refusal names the fallback");

    let victim = fixture.trampoline();
    let baseline = fixture.harness.count_observed(TICK_OBSERVED);
    fixture.harness.execute(vec![("bump", HarnessOp::send_and_settle(victim, &Bump))]).expect("bump the victim");
    assert_eq!(fixture.harness.count_observed(TICK_OBSERVED), baseline + 1, "the fallback guest must still serve");
}
