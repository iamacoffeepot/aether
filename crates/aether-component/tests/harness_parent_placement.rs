//! Public `SubstrateHarness` placement coverage for issue #4535.
//!
//! The scenario uses only `HarnessOp` plus ordinary `LoadComponent` values to
//! build two component peer scopes. The fixture caller's real
//! `PeerCtxExt::peer` send proves the runtime parent selected during explicit
//! placement is what embedded resolution consumes.

#![allow(
    clippy::disallowed_methods,
    reason = "the canonical-path assertion independently folds the returned address as its reference value"
)]

use std::fs;

use aether_actor::{Addressable, EMBEDDED_SCOPE};
use aether_component::ComponentHostCapability;
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_test_fixtures_kinds::{Bump, TickObserved};

const PROBE_EXPORT: &str = "test.probe";
const CALLER_EXPORT: &str = "test.parent_peer.caller";
const TARGET_EXPORT: &str = "test.parent_peer.target";

struct Loaded {
    mailbox_id: MailboxId,
    name: String,
}

fn load(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    label: &str,
    parent: Option<&str>,
    name: Option<&str>,
    export: &str,
) -> Loaded {
    let component = LoadComponent {
        wasm: wasm.to_vec(),
        name: name.map(str::to_owned),
        config: Vec::new(),
        export: Some(export.to_owned()),
    };
    let operation = match parent {
        Some(parent) => HarnessOp::load_component_under(parent, component),
        None => HarnessOp::send_and_await_reply(ComponentHostCapability::NAMESPACE, &component),
    };
    let result = harness.execute(vec![(label, operation)]).expect("component load operation");

    match result.reply::<LoadResult>(label).expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, name, .. } => Loaded { mailbox_id, name },
        LoadResult::Err { error } => panic!("load {export} beneath {parent:?} failed: {error}"),
    }
}

fn assert_child_identity(loaded: &Loaded, parent: &str, subname: &str) {
    let expected = format!("{parent}/{EMBEDDED_SCOPE}:{subname}");
    assert_eq!(loaded.name, expected, "LoadResult must return the registry-canonical child path");
    assert_eq!(
        loaded.mailbox_id,
        mailbox_id_from_path(&expected),
        "LoadResult mailbox id must be the lineage fold of its canonical path",
    );
}

#[test]
fn explicit_and_nested_parents_scope_live_peer_delivery() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let outer = load(&mut harness, &wasm, "outer", None, Some("outer"), PROBE_EXPORT);
    let outer_target = load(&mut harness, &wasm, "outer-target", Some(&outer.name), None, TARGET_EXPORT);
    let outer_caller = load(&mut harness, &wasm, "outer-caller", Some(&outer.name), None, CALLER_EXPORT);
    assert_child_identity(&outer_target, &outer.name, TARGET_EXPORT);
    assert_child_identity(&outer_caller, &outer.name, CALLER_EXPORT);

    let nested = load(&mut harness, &wasm, "nested", Some(&outer.name), Some("nested"), PROBE_EXPORT);
    assert_child_identity(&nested, &outer.name, "nested");
    let nested_target = load(&mut harness, &wasm, "nested-target", Some(&nested.name), None, TARGET_EXPORT);
    let nested_caller = load(&mut harness, &wasm, "nested-caller", Some(&nested.name), None, CALLER_EXPORT);
    assert_child_identity(&nested_target, &nested.name, TARGET_EXPORT);
    assert_child_identity(&nested_caller, &nested.name, CALLER_EXPORT);

    let baseline = harness.count_observed(TickObserved::NAME);
    harness
        .execute(vec![
            ("outer-peer", HarnessOp::send_and_settle(&outer_caller.name, &Bump)),
            ("nested-peer", HarnessOp::send_and_settle(&nested_caller.name, &Bump)),
        ])
        .expect("both parent-relative peer sends settle");
    assert_eq!(
        harness.count_observed(TickObserved::NAME) - baseline,
        2,
        "each caller must reach the target beneath its own runtime parent; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

#[test]
fn unresolved_explicit_parent_is_a_clean_load_error() {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let result = harness
        .execute(vec![(
            "missing-parent",
            HarnessOp::load_component_under(
                "aether.component/aether.embedded:missing",
                LoadComponent { wasm: Vec::new(), name: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("the component host replies to an unresolved parent");

    let LoadResult::Err { error } = result.reply::<LoadResult>("missing-parent").expect("decode LoadResult") else {
        panic!("an unresolved logical parent must not load a component");
    };
    assert!(error.contains("component parent"), "error identifies the parent boundary: {error}");
    assert!(error.contains("missing"), "error retains the unresolved address: {error}");
}
