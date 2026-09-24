//! Issue 6134: a replace whose candidate fails to start leaves the running
//! guest in place (ADR-0016 §4). A candidate whose `init` fails is dropped
//! before the old guest runs any hook; a candidate whose `on_rehydrate` traps
//! is dropped and the old guest is reinstalled, and the slot keeps hosting
//! the old type, so a later bare replace rebuilds it.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_component::ComponentHostCapability;
use aether_data::{ActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::{Bump, ConfigEcho, ConfigQuery, CountQuery, CountReport, ProbeConfig};

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";

/// A replace of `target` with the same fixture wasm.
fn replace(target: &ActorPath, wasm: &[u8], config: Vec<u8>, export: Option<&str>) -> ReplaceComponent {
    ReplaceComponent {
        target: target.clone(),
        wasm: wasm.to_vec(),
        drain_timeout_ms: None,
        config,
        export: export.map(str::to_owned),
    }
}

fn expect_refused(result: &ReplaceResult, reason: &str) {
    match result {
        ReplaceResult::Err { error } => assert!(error.contains(reason), "the refusal must say {reason:?}: {error}"),
        ReplaceResult::Ok { .. } => panic!("a replacement that failed to start was accepted"),
    }
}

#[test]
fn a_replace_whose_candidate_fails_init_keeps_the_running_guest() {
    // Catches: the old guest is retired before the candidate's `init` runs,
    // so a failed `init` empties the slot and the query finds no guest.
    let Some(wasm_path) = require_wasm(FIXTURE_CRATE) else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");

    let config = ProbeConfig { seed: 0x6134_0001, label: "before-replace".to_owned() };
    let (probe, path) = harness
        .load_any(&LoadComponent {
            wasm: wasm.clone(),
            name: None,
            config: config.encode_into_bytes(),
            export: Some("test.probe_with_config".to_owned()),
        })
        .expect("load test.probe_with_config");

    // A `ProbeConfig` cut one byte short: its label's length prefix runs past
    // the end, so the candidate's typed `init` cannot decode it.
    let mut undecodable = config.encode_into_bytes();
    undecodable.pop();

    let host = harness.actor_ref::<ComponentHostCapability>();
    let result = harness
        .execute(vec![
            ("replace", HarnessOp::send_and_await_reply(&host, &replace(&path, &wasm, undecodable, None))),
            ("echo", HarnessOp::send_and_await_reply(probe, &ConfigQuery)),
        ])
        .expect("replace + query sequence");

    expect_refused(
        &result.reply::<ReplaceResult>("replace").expect("decode ReplaceResult"),
        "wasm instantiation failed",
    );
    let echo = result.reply::<ConfigEcho>("echo").expect("decode ConfigEcho");
    assert_eq!(echo.seed, config.seed, "the running guest keeps the seed its own init saw");
    assert_eq!(echo.label, config.label, "the running guest keeps the label its own init saw");
}

#[test]
fn a_replace_whose_candidate_fails_rehydrate_keeps_the_running_guest() {
    // Catches: the candidate is installed despite its rehydrate error, so the
    // query reads the candidate's fresh count; and the slot's hosted type or
    // module is promoted before rehydrate, so the bare replace that follows
    // rebuilds the trapping type and fails again.
    let Some(wasm_path) = require_wasm(FIXTURE_CRATE) else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");

    let (counter, path) = harness
        .load_any(&LoadComponent {
            wasm: wasm.clone(),
            name: None,
            config: Vec::new(),
            export: Some("test.stateful.counter".to_owned()),
        })
        .expect("load test.stateful.counter");

    let host = harness.actor_ref::<ComponentHostCapability>();
    let result = harness
        .execute(vec![
            ("bump_1", HarnessOp::send_and_settle(counter, &Bump)),
            ("bump_2", HarnessOp::send_and_settle(counter, &Bump)),
            ("bump_3", HarnessOp::send_and_settle(counter, &Bump)),
            (
                "trap",
                HarnessOp::send_and_await_reply(
                    &host,
                    &replace(&path, &wasm, Vec::new(), Some("test.stateful.rehydrate_trap")),
                ),
            ),
            ("after_trap", HarnessOp::send_and_await_reply(counter, &CountQuery)),
            ("bare", HarnessOp::send_and_await_reply(&host, &replace(&path, &wasm, Vec::new(), None))),
            ("after_bare", HarnessOp::send_and_await_reply(counter, &CountQuery)),
        ])
        .expect("bump + replace + query sequence");

    expect_refused(&result.reply::<ReplaceResult>("trap").expect("decode ReplaceResult"), "on_rehydrate failed");
    assert_eq!(
        result.reply::<CountReport>("after_trap").expect("decode CountReport").count,
        3,
        "the reinstated counter keeps its count",
    );
    let bare = result.reply::<ReplaceResult>("bare").expect("decode ReplaceResult");
    assert!(matches!(bare, ReplaceResult::Ok { .. }), "a bare replace rebuilds the counter: {bare:?}");
    assert_eq!(
        result.reply::<CountReport>("after_bare").expect("decode CountReport").count,
        3,
        "the rebuilt counter rehydrates the reinstated counter's count",
    );
}
