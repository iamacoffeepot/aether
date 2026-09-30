//! Inline-child dependency refusal at stand-up (ADR-0230, ADR-0241 §4).
//!
//! Dependencies are checked where an actor stands up, never at module load.
//! The inline-dependency fixture's `Holder` spawns its private child `Needy`
//! from `wire`, and `Needy` declares `depends(ClipboardCapability)`: without
//! clipboard the module loads, the spawn answers the guest
//! `SpawnError::DependencyNotLive`, and no `Needy` stands; with the in-memory
//! clipboard composed, `Needy` is live. The fs-demux fixture's private child
//! declares `depends(FsCapability, …)` and is refused the same way.
//!
//! A republish rebuilds each live inline child, so a dependency its
//! successor type adds is checked then: `republish_subject_helper`'s live
//! helper declares none, and republishing it to
//! `republish_subject_inline_depends`, whose helper adds clipboard, is
//! refused naming that helper. The same successor republishes the helperless
//! base, which has no helper to rebuild, and proceeds.

use std::fs;

use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_data::LoadName;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::LoadComponent;
use aether_test_fixtures_fs_demux::{InlineFsDemuxChild, InlineFsDemuxParent};
use aether_test_fixtures_inline_dependency::{Holder, Needy};
use aether_test_fixtures_kinds::{CountQuery, CountReport, SpawnOutcome, SpawnOutcomeQuery};

/// The republish subject's row this file sends: `CountQuery -> CountReport`.
/// The subject ships only as a cdylib example, so a test casts its
/// `load_any` reference to this instead of naming a type.
#[aether_actor::protocol]
trait SubjectCount {
    fn count(mail: CountQuery) -> CountReport;
}

fn read_wasm(stem: &str) -> Option<Vec<u8>> {
    require_wasm(stem).map(|path| fs::read(path).expect("read fixture wasm"))
}

fn component(wasm: &[u8]) -> LoadComponent {
    LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: None }
}

fn key(text: &str) -> LoadName {
    LoadName::new(text).expect("a valid instance key")
}

/// Load `Holder` on `harness`, answer how its spawn of `Needy` ended, and
/// whether `Needy` stands once the registry owner has applied every batch
/// the load staged.
fn holder_spawn(harness: &mut SubstrateHarness, wasm: &[u8]) -> (SpawnOutcome, bool) {
    let holder =
        harness.load::<Holder>(component(wasm)).unwrap_or_else(|error| panic!("the holder must load: {error}"));

    let result = harness
        .execute(vec![("outcome", HarnessOp::send_and_await_reply(&holder, &SpawnOutcomeQuery))])
        .expect("query the holder's spawn outcome");
    let outcome = result.reply::<SpawnOutcome>("outcome").expect("decode SpawnOutcome");

    harness.await_registry_applied();
    let live = match harness.child::<Holder, Needy>(&holder, key("needy")) {
        Ok(_) => true,
        Err(SubstrateHarnessError::ChildRefused(_)) => false,
        Err(error) => panic!("the child lookup must answer: {error}"),
    };
    (outcome, live)
}

#[test]
fn an_inline_child_with_an_unmet_dependency_is_refused_at_spawn() {
    let Some(wasm) = read_wasm("aether_test_fixtures_inline_dependency") else {
        return;
    };

    let mut absent = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    assert_eq!(
        holder_spawn(&mut absent, &wasm),
        (SpawnOutcome { spawned: false, dependency_not_live: true }, false),
        "without clipboard the module loads, its spawn is refused as DependencyNotLive, and no child stands",
    );

    let mut present = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<ClipboardCapability>(ClipboardParams::InMemory)
        .build()
        .expect("boot with clipboard");
    assert_eq!(
        holder_spawn(&mut present, &wasm),
        (SpawnOutcome { spawned: true, dependency_not_live: false }, true),
        "with clipboard composed the child is spawned and live",
    );
}

/// The satisfied path is `inline_child_matches_host_replies_to_its_own_requests`
/// in `inline_child.rs`, which loads the same export with fs roots composed.
#[test]
fn a_private_inline_child_with_an_unmet_dependency_is_refused_at_spawn() {
    let Some(wasm) = read_wasm("aether_test_fixtures_fs_demux") else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let parent = harness
        .load::<InlineFsDemuxParent>(component(&wasm))
        .unwrap_or_else(|error| panic!("the parent loads without fs: {error}"));

    harness.await_registry_applied();
    let child = harness.child::<InlineFsDemuxParent, InlineFsDemuxChild>(&parent, key("demux"));
    assert!(
        matches!(child, Err(SubstrateHarnessError::ChildRefused(_))),
        "the private child whose fs dependency is not live must not stand",
    );
}

#[test]
fn a_republish_adding_an_unmet_dependency_to_a_live_inline_instance_is_refused() {
    let Some(helper) = read_wasm("republish_subject_helper") else {
        return;
    };
    let Some(successor) = read_wasm("republish_subject_inline_depends") else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (subject, path) =
        harness.load_any(&component(&helper)).unwrap_or_else(|error| panic!("the subject must load: {error}"));
    let subject = harness.cast::<SubjectCount>(subject).expect("the subject publishes CountQuery");
    // The helper's alias batch is queued ahead of the load's answer; the
    // barrier makes it live before the republish reads the inventory.
    harness.await_registry_applied();

    let Err(SubstrateHarnessError::Publish(error)) = harness.publish(successor) else {
        panic!("a republish adding an unmet dependency to a live helper must be refused");
    };
    assert_eq!(
        error,
        format!("{path}/test.republish.subject_helper:helper depends on aether.clipboard, which is not live"),
        "the refusal names the live helper and the missing namespace",
    );

    let result = harness
        .execute(vec![("count", HarnessOp::send_and_await_reply(&subject, &CountQuery))])
        .expect("query the subject");
    assert_eq!(
        result.reply::<CountReport>("count").expect("decode CountReport"),
        CountReport { count: 0 },
        "the subject keeps serving after the refused republish",
    );
}

#[test]
fn a_republish_adding_a_dependency_to_a_type_with_no_live_instance_proceeds() {
    let Some(base) = read_wasm("republish_subject_base") else {
        return;
    };
    let Some(successor) = read_wasm("republish_subject_inline_depends") else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    harness.load_any(&component(&base)).unwrap_or_else(|error| panic!("the subject must load: {error}"));

    if let Err(error) = harness.publish(successor) {
        panic!("a type with no live instance is checked at its next spawn: {error}");
    }
}
