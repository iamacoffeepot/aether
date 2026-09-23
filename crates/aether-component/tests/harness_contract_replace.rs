//! Contract monotonicity across replace (ADR-0231 §5, issue #6444).
//!
//! A replacement whose hosted type drops or changes a handler row of the
//! type the slot hosts is refused with `ReplaceResult::Err` before the old
//! instance is touched, so the old module keeps serving; a replacement that
//! only adds rows succeeds, and the added row then binds the next replace. A
//! refill after `DropComponent` is held to the dropped module's rows. The
//! fixtures are the bundle's `test.contract.*` exports.

use std::fs;

use aether_actor::ActorRef;
use aether_component::{ComponentHostCapability, WasmTrampoline};
use aether_data::{ActorPath, LoadName};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::Bump;

const BASE_EXPORT: &str = "test.contract.base";
const DROPPED_EXPORT: &str = "test.contract.dropped";
const CHANGED_EXPORT: &str = "test.contract.changed";
const EXTENDED_EXPORT: &str = "test.contract.extended";
const COUNT_QUERY: &str = "aether.test_fixtures.count_query";
const INLINE_PROBE: &str = "aether.test_fixtures.inline_probe";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";
const VICTIM: &str = "victim";

struct Fixture {
    harness: SubstrateHarness,
    wasm: Vec<u8>,
    victim: ActorPath,
}

impl Fixture {
    /// Boot a harness and load `test.contract.base` as `victim`.
    fn start() -> Option<Self> {
        let wasm = fs::read(require_wasm("aether_test_fixtures_bundle")?).expect("read fixture wasm");
        let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

        let load = LoadComponent {
            wasm: wasm.clone(),
            name: Some(VICTIM.to_owned()),
            config: Vec::new(),
            export: Some(BASE_EXPORT.to_owned()),
        };
        let operation = HarnessOp::send_and_await_reply(&harness.actor_ref::<ComponentHostCapability>(), &load);
        let result = harness.execute(vec![("load", operation)]).expect("component load operation");
        let victim = match result.reply::<LoadResult>("load").expect("decode LoadResult") {
            LoadResult::Ok { path, .. } => path,
            LoadResult::Err { error } => panic!("the base must load: {error}"),
        };

        Some(Self { harness, wasm, victim })
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

    fn trampoline(&self) -> ActorRef<WasmTrampoline> {
        let host = self.harness.actor_ref::<ComponentHostCapability>();
        self.harness
            .child::<ComponentHostCapability, WasmTrampoline>(&host, LoadName::new(VICTIM).expect("a valid load name"))
            .expect("the victim is live")
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
    fixture
        .harness
        .execute(vec![("bump", HarnessOp::send_and_settle(victim.erase(), &Bump))])
        .expect("bump the victim");
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

#[test]
fn a_refill_after_drop_is_held_to_the_dropped_contract() {
    let Some(mut fixture) = Fixture::start() else {
        return;
    };

    let drop = DropComponent { target: fixture.victim.clone() };
    let operation = HarnessOp::send_and_await_reply(&fixture.harness.actor_ref::<ComponentHostCapability>(), &drop);
    let dropped = fixture.harness.execute(vec![("drop", operation)]).expect("drop operation");
    if let DropResult::Err { error } = dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        panic!("the victim must drop: {error}");
    }

    let ReplaceResult::Err { error } = fixture.replace("refill-dropped", DROPPED_EXPORT) else {
        panic!("a refill that drops a row of the dropped module must be refused");
    };
    assert_eq!(error, fixture.refusal(COUNT_QUERY), "the refill is held to the dropped module's rows");

    if let ReplaceResult::Err { error } = fixture.replace("refill-base", BASE_EXPORT) {
        panic!("a refill that keeps the dropped module's rows must succeed: {error}");
    }
}
