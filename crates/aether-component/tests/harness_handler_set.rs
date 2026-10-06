//! ADR-0169 handler sets through a loaded wasm module.
//!
//! `HandlerSetAdopter` adopts `AnswerSet`, overrides one of its two request
//! handlers and counts the set requests it answers. The macro's accept cases
//! prove the adoption compiles and `handlers_manifest` proves its rows; this
//! is where a loaded adopter is mailed. It catches a miss that does not reach the set's
//! default body, an override the default shadows, a default body that does not
//! reach the adopter's state through the set's accessor, and a manifest that
//! lists a set kind twice and fails the load (issue 5671).
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes
//! a hard panic there.

use std::fs;

use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_test_fixtures_bundle::HandlerSetAdopter;
use aether_test_fixtures_kinds::{
    AskKept, AskKeptResult, AskReplaced, AskReplacedResult, CountQuery, CountReport, HANDLER_SET_DEFAULT_BODY,
    HANDLER_SET_OVERRIDE_BODY,
};

const BUNDLE: &str = "aether_test_fixtures_bundle";

#[test]
fn a_loaded_adopter_answers_from_the_set_default_and_from_its_override() {
    let Some(wasm_path) = require_wasm(BUNDLE) else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let adopter = harness
        .load::<HandlerSetAdopter>(LoadComponent { wasm, name: None, config: Vec::new(), export: None })
        .unwrap_or_else(|error| panic!("the adopter loads: {error}"));

    let answers = harness
        .execute(vec![
            ("kept", HarnessOp::send_and_await_reply(&adopter, &AskKept)),
            ("replaced", HarnessOp::send_and_await_reply(&adopter, &AskReplaced)),
            ("count", HarnessOp::send_and_await_reply(&adopter, &CountQuery)),
        ])
        .expect("every request is answered");

    assert_eq!(
        answers.reply::<AskKeptResult>("kept").expect("decode AskKeptResult"),
        AskKeptResult { answered_by: HANDLER_SET_DEFAULT_BODY },
        "a request the adopter has no arm for reaches the set's default body",
    );
    assert_eq!(
        answers.reply::<AskReplacedResult>("replaced").expect("decode AskReplacedResult"),
        AskReplacedResult { answered_by: HANDLER_SET_OVERRIDE_BODY },
        "the adopter's override runs in place of the set's default body",
    );
    assert_eq!(
        answers.reply::<CountReport>("count").expect("decode CountReport"),
        CountReport { count: 2 },
        "the set's default body and the override both counted on the adopter's own state",
    );
}
