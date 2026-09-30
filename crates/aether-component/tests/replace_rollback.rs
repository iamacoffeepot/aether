//! Issue 6134: a replace whose candidate fails to start leaves the running
//! guest in place (ADR-0016 §4). A config that does not decode as the
//! successor's config kind refuses before any member prepares; a candidate
//! whose `on_rehydrate` traps is dropped and the old guest is reinstalled,
//! and the slot keeps its module and config, so a later replace rebuilds it.
//! The reinstated guest runs `wire` again, and nothing the failed candidate
//! sent leaves (ADR-0241 §7).
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes a
//! hard panic there.

use std::fs;

use aether_component::ComponentHostCapability;
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{InstanceConfig, LoadComponent, Publish, PublishResult};
use aether_substrate::testing::successor_wasm;
use aether_test_fixtures_bundle::ProbeWithConfig;
use aether_test_fixtures_kinds::{
    Bump, ConfigEcho, ConfigQuery, CountQuery, CountReport, PeerConfig, ProbeConfig, TickObserved, WireObserved,
};

const FIXTURE_CRATE: &str = "aether_test_fixtures_bundle";

/// The `test.republish.peer` rows this file sends: a silent `Bump` and
/// `CountQuery -> CountReport`. Both the group v1 and v2 peers ship only as
/// cdylib examples, so the test casts its `load_any` reference to this
/// instead of naming a type.
#[aether_actor::protocol]
trait GroupPeer {
    fn bump(mail: Bump);
    fn count(mail: CountQuery) -> CountReport;
}

/// A republish of `wasm` giving the instance at `path` the config `config`.
fn publish_configured(wasm: &[u8], path: &ErasedActorPath, config: Vec<u8>) -> Publish {
    Publish { code: wasm.to_vec().into(), configs: vec![InstanceConfig { path: path.clone(), config }] }
}

/// The group pair's two versions, or `None` when either is not built.
fn group_pair() -> Option<(Vec<u8>, Vec<u8>)> {
    let v1 = fs::read(require_wasm("republish_group_v1")?).expect("read republish_group_v1");
    let v2 = fs::read(require_wasm("republish_group_v2")?).expect("read republish_group_v2");
    Some((v1, v2))
}

/// Load the first version of `test.republish.peer` with `trap_on_rehydrate`
/// set, which v1 ignores and v2's `on_rehydrate` acts on.
fn load_trapping_peer(harness: &mut SubstrateHarness, v1: &[u8]) -> (aether_actor::ErasedActorRef, ErasedActorPath) {
    harness
        .load_any(&LoadComponent {
            wasm: v1.to_vec(),
            name: None,
            config: PeerConfig { trap_on_rehydrate: true }.encode_into_bytes(),
            export: Some("test.republish.peer".to_owned()),
        })
        .expect("load test.republish.peer v1")
}

fn expect_refused(result: &PublishResult, reason: &str) {
    match result {
        PublishResult::Err { error } => assert!(error.contains(reason), "the refusal must say {reason:?}: {error}"),
        PublishResult::Ok { .. } => panic!("a replacement that failed to start was accepted"),
    }
}

#[test]
fn a_config_that_does_not_decode_refuses_and_keeps_the_running_guest() {
    // Catches: a supplied config reaching a member's prepare undecoded, so a
    // candidate is built from bytes its typed `init` cannot read, or the old
    // guest is retired before the refusal and the query finds no guest.
    let Some(wasm_path) = require_wasm(FIXTURE_CRATE) else {
        return;
    };
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");

    let config = ProbeConfig { seed: 0x6134_0001, label: "before-replace".to_owned() };
    let (probe, path) = harness
        .load::<ProbeWithConfig>(LoadComponent {
            wasm: wasm.clone(),
            name: None,
            config: config.encode_into_bytes(),
            export: None,
        })
        .expect("load test.probe_with_config");

    // A `ProbeConfig` cut one byte short: its label's length prefix runs past
    // the end, so it does not decode as the successor's config kind.
    let mut undecodable = config.encode_into_bytes();
    undecodable.pop();

    let host = harness.actor_ref::<ComponentHostCapability>();
    let publish = publish_configured(&successor_wasm(&wasm, 1), &path, undecodable);
    let result = harness
        .execute(vec![
            ("publish", HarnessOp::send_and_await_reply(&host, &publish)),
            ("echo", HarnessOp::send_and_await_reply(&probe, &ConfigQuery)),
        ])
        .expect("replace + query sequence");

    expect_refused(&result.reply::<PublishResult>("publish").expect("decode PublishResult"), "does not decode");
    let echo = result.reply::<ConfigEcho>("echo").expect("decode ConfigEcho");
    assert_eq!(echo.seed, config.seed, "the running guest keeps the seed its own init saw");
    assert_eq!(echo.label, config.label, "the running guest keeps the label its own init saw");
}

#[test]
fn a_replace_whose_candidate_fails_rehydrate_keeps_the_running_guest() {
    // Catches: the candidate is installed despite its rehydrate error, so the
    // query reads the candidate's fresh count; and the slot's module or
    // stored config is promoted before rehydrate, so the replace that follows
    // with a non-trapping config fails again or loses the count.
    let Some((v1, v2)) = group_pair() else {
        return;
    };
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
    let (peer, path) = load_trapping_peer(&mut harness, &v1);
    let peer = harness.cast::<GroupPeer>(peer).expect("the peer publishes Bump and CountQuery");

    let host = harness.actor_ref::<ComponentHostCapability>();
    let calm = PeerConfig { trap_on_rehydrate: false }.encode_into_bytes();
    let result = harness
        .execute(vec![
            ("bump_1", HarnessOp::send_and_settle(&peer, &Bump)),
            ("bump_2", HarnessOp::send_and_settle(&peer, &Bump)),
            ("bump_3", HarnessOp::send_and_settle(&peer, &Bump)),
            ("trap", HarnessOp::send_and_await_reply(&host, &Publish { code: v2.clone().into(), configs: Vec::new() })),
            ("after_trap", HarnessOp::send_and_await_reply(&peer, &CountQuery)),
            ("calm", HarnessOp::send_and_await_reply(&host, &publish_configured(&v2, &path, calm))),
            ("after_calm", HarnessOp::send_and_await_reply(&peer, &CountQuery)),
        ])
        .expect("bump + replace + query sequence");

    expect_refused(&result.reply::<PublishResult>("trap").expect("decode PublishResult"), "on_rehydrate failed");
    assert_eq!(
        result.reply::<CountReport>("after_trap").expect("decode CountReport").count,
        3,
        "the reinstated peer keeps its count",
    );
    let calm = result.reply::<PublishResult>("calm").expect("decode PublishResult");
    assert!(matches!(calm, PublishResult::Ok { .. }), "a replace with a non-trapping config swaps the peer: {calm:?}");
    assert_eq!(
        result.reply::<CountReport>("after_calm").expect("decode CountReport").count,
        3,
        "the successor rehydrates the reinstated peer's count",
    );
}

/// Load the first version of `test.republish.peer` with `trap_on_rehydrate`
/// set, then republish the second, whose `on_rehydrate` reports
/// `TickObserved` and traps. Returns the harness and the `WireObserved`
/// count before the replace, or `None` when the fixture wasm is not built.
///
/// The observer is an inline sink, so a report reaches its count inside the
/// sender's call: anything the candidate or the reinstated guest mails it is
/// counted before the replace answers.
fn replace_with_rehydrate_trap() -> Option<(SubstrateHarness, usize)> {
    let (v1, v2) = group_pair()?;
    let mut harness = SubstrateHarness::builder().with_component_host().size(64, 48).build().expect("boot");
    let _ = load_trapping_peer(&mut harness, &v1);
    let wired = harness.count_observed(WireObserved::NAME);

    let Err(SubstrateHarnessError::Publish(error)) = harness.publish(v2) else {
        panic!("a replacement that failed to start was accepted");
    };
    assert!(error.contains("on_rehydrate failed"), "the refusal must say \"on_rehydrate failed\": {error}");

    Some((harness, wired))
}

#[test]
fn a_candidate_that_fails_rehydrate_sends_nothing() {
    // Catches: the candidate's `on_rehydrate` mail leaves before the swap is
    // known to succeed, so a candidate that then traps is still heard from.
    let Some((harness, _)) = replace_with_rehydrate_trap() else {
        return;
    };

    assert_eq!(harness.count_observed(TickObserved::NAME), 0, "the failed candidate's mail never leaves");
}

#[test]
fn a_reinstated_guest_is_wired_again() {
    // Catches: the old guest is reinstated after its `unwire` without running
    // `wire` again, so whatever `wire` set up stays torn down.
    let Some((harness, wired)) = replace_with_rehydrate_trap() else {
        return;
    };

    assert_eq!(harness.count_observed(WireObserved::NAME), wired + 1, "the reinstated peer runs `wire` again");
}
