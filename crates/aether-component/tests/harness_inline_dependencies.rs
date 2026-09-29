//! Inline-spawnable dependency refusal (ADR-0230 §3, issue #6455).
//!
//! The inline-dependency fixture's default export `Holder` spawns the
//! composable `Needy` inline, and `Needy` declares
//! `depends(ClipboardCapability)`. The host checks that declaration when the
//! module loads, before `Holder` runs: a load on a harness without clipboard
//! is a `LoadResult::Err` naming `Needy`, and the same load is `Ok` once the
//! in-memory clipboard is composed. A republish toward a successor whose
//! inline child declares an unmet dependency is refused the same way, and the
//! running instance keeps serving.
//!
//! A private inline child (issue 6590) is checked the same way. The fs-demux
//! fixture's `InlineFsDemuxParent` declares no dependency, but its private
//! child `InlineFsDemuxChild` declares `depends(FsCapability, …)`, which the
//! host reads from the module's `aether.kinds.inputs.private` section: a load
//! of the parent on a harness without fs roots is refused naming the child.

use std::fs;

use aether_clipboard::{ClipboardCapability, ClipboardParams};
use aether_component::ComponentHostCapability;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::Bump;

const HOLDER_EXPORT: &str = "test.inline_dependency.holder";
const REFUSAL: &str = "test.inline_dependency.needy depends on aether.clipboard, which is not live";
const SUBJECT_REFUSAL: &str = "test.republish.subject_helper depends on aether.clipboard, which is not live";
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";
const FS_DEMUX_PARENT_EXPORT: &str = "test.inline.fs_demux_parent";
const PRIVATE_REFUSAL: &str = "test.inline.fs_demux_child depends on aether.fs, which is not live";

fn load_result(harness: &mut SubstrateHarness, wasm: &[u8], label: &str, export: &str) -> LoadResult {
    let component =
        LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: Some(export.to_owned()) };
    let operation = HarnessOp::send_and_await_reply(&harness.actor_ref::<ComponentHostCapability>(), &component);
    let result = harness.execute(vec![(label, operation)]).expect("component load operation");
    result.reply::<LoadResult>(label).expect("decode LoadResult")
}

fn fixture_wasm() -> Option<Vec<u8>> {
    let wasm_path = require_wasm("aether_test_fixtures_inline_dependency")?;
    Some(fs::read(wasm_path).expect("read fixture wasm"))
}

#[test]
fn an_inline_child_dependency_refuses_its_module_load() {
    let Some(wasm) = fixture_wasm() else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let LoadResult::Err { error } = load_result(&mut harness, &wasm, "absent", HOLDER_EXPORT) else {
        panic!("a module whose inline child's dependency is not live must be refused");
    };
    assert_eq!(error, REFUSAL, "the refusal names the inline child and the missing namespace");

    // Refusal happens before creation: no guest stands at its namespace.
    let listed = harness.list_components().expect("list components");
    assert!(!listed.iter().any(|name| name == HOLDER_EXPORT), "the refused load created nothing: {listed:?}");

    let mut satisfied = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .with_actor::<ClipboardCapability>(ClipboardParams::InMemory)
        .build()
        .expect("boot with clipboard");
    match load_result(&mut satisfied, &wasm, "present", HOLDER_EXPORT) {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("a module whose inline child's dependency is live must load: {error}"),
    }
}

/// The satisfied path is `inline_child_matches_host_replies_to_its_own_requests`
/// in `inline_child.rs`, which loads the same export with fs roots composed.
#[test]
fn a_private_inline_child_dependency_refuses_its_module_load() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_fs_demux") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fs-demux fixture wasm");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let LoadResult::Err { error } = load_result(&mut harness, &wasm, "absent", FS_DEMUX_PARENT_EXPORT) else {
        panic!("a module whose private inline child's dependency is not live must be refused");
    };
    assert_eq!(error, PRIVATE_REFUSAL, "the refusal names the private child and the missing namespace");

    // Refusal happens before creation: no guest stands at its namespace.
    let listed = harness.list_components().expect("list components");
    assert!(!listed.iter().any(|name| name == FS_DEMUX_PARENT_EXPORT), "the refused load created nothing: {listed:?}");
}

/// Catches a republish checked only against its live members' own types: the
/// successor's private inline child declares a dependency that is not live,
/// which a rehydrating member would spawn before anything could refuse it.
#[test]
fn a_replace_toward_an_unmet_inline_dependency_keeps_the_running_module() {
    let Some(base_path) = require_wasm("republish_subject_base") else {
        return;
    };
    let Some(inline_path) = require_wasm("republish_subject_inline_depends") else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let (subject, _) = harness
        .load_any(&LoadComponent {
            wasm: fs::read(base_path).expect("read subject base wasm"),
            name: None,
            config: Vec::new(),
            export: None,
        })
        .unwrap_or_else(|error| panic!("the subject must load: {error}"));

    let operation = HarnessOp::send_and_await_reply(
        &harness.actor_ref::<ComponentHostCapability>(),
        &ReplaceComponent { wasm: fs::read(inline_path).expect("read successor wasm"), configs: Vec::new() },
    );
    let result = harness.execute(vec![("replace", operation)]).expect("replace sequence");
    let ReplaceResult::Err { error } = result.reply::<ReplaceResult>("replace").expect("decode ReplaceResult") else {
        panic!("a republish whose inline child's dependency is not live must be refused");
    };
    assert_eq!(
        error,
        format!("replace refused: {SUBJECT_REFUSAL}"),
        "the refusal names the inline child and the missing namespace"
    );

    // A refused republish keeps the running module: the subject still
    // answers `Bump` with exactly one `TickObserved`.
    let baseline = harness.count_observed(TICK_OBSERVED);
    harness.execute(vec![("bump", HarnessOp::send_and_settle(subject, &Bump))]).expect("bump the subject");
    assert_eq!(
        harness.count_observed(TICK_OBSERVED),
        baseline + 1,
        "the subject must still serve after a refused replace"
    );
}
