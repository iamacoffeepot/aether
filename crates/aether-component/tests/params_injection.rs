//! ADR-0170 integration coverage for wasm params injection: the manifest a
//! component's `Params` declares, the host's validation of it against the
//! provider registry, and the injected value the guest actually receives.
//!
//! Loads the `ProbeWithParams` / `ProbeWithUnprovidedParam` actors out of the
//! `probe` bundle through a [`SubstrateHarness`], which composes the same
//! `ParamProviderRegistry::with_substrate_facts()` a real chassis does — so
//! these exercise the production provider set rather than a stand-in.

use std::fs;
use std::path::Path;

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult, ReplicaIdentity};
use aether_test_fixtures_kinds::{ConfigEcho, ParamsEcho, ParamsQuery};

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor` entries
// are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

const PARAMS_EXPORT: &str = "test.probe_with_params";
const UNPROVIDED_EXPORT: &str = "test.probe_unprovided_param";

fn trampoline_address(name: &str) -> String {
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

fn bundle_wasm() -> Option<Vec<u8>> {
    let path = require_wasm("aether_test_fixtures_bundle")?;
    Some(fs::read::<&Path>(path.as_ref()).expect("read fixture wasm"))
}

fn load_op(wasm: Vec<u8>, export: &str, name: &str, replica: Option<ReplicaIdentity>) -> HarnessOp {
    HarnessOp::send_and_await_reply(
        ComponentHostCapability::NAMESPACE,
        &LoadComponent {
            wasm,
            name: Some(name.to_owned()),
            config: Vec::new(),
            export: Some(export.to_owned()),
            replica,
        },
    )
}

/// Load `export` under `name` with `replica`, then ask the instance what it
/// was injected with.
fn load_and_query(
    wasm: Vec<u8>,
    export: &str,
    name: &str,
    replica: Option<ReplicaIdentity>,
) -> (LoadResult, ParamsEcho) {
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let report = harness
        .execute(vec![
            ("load", load_op(wasm, export, name, replica)),
            ("echo", HarnessOp::send_and_await_reply(trampoline_address(name), &ParamsQuery)),
        ])
        .expect("load sequence");

    (
        report.reply::<LoadResult>("load").expect("decode LoadResult"),
        report.reply::<ParamsEcho>("echo").expect("decode ParamsEcho"),
    )
}

/// The bug this catches: a replica index that never reaches the guest — the
/// `LoadComponent::replica` field dropped between the fan-out site and the
/// provider, the provider encoding the wrong field, or the guest's generated
/// `from_entries` matching entries by position rather than by kind. Any of
/// those makes every replica believe it is instance 0, which is exactly the
/// silent misbehaviour params injection exists to prevent: replicated
/// instances share one config, so nothing else would disagree.
///
/// The index and count are deliberately both non-default and distinct from
/// each other, so a swapped pair or a defaulted field is visible.
#[test]
fn the_replica_identity_a_load_carries_reaches_the_guest_intact() {
    let Some(wasm) = bundle_wasm() else {
        return;
    };

    let (load, echo) = load_and_query(wasm, PARAMS_EXPORT, "sharded-3", Some(ReplicaIdentity { index: 3, count: 7 }));

    let LoadResult::Ok { capabilities, .. } = load else {
        panic!("params-requesting component failed to load: {load:?}");
    };
    let request = capabilities.params.first().expect("the component advertises its one param request");
    assert_eq!(request.id, <ReplicaIdentity as Kind>::ID);
    assert_eq!(request.name, <ReplicaIdentity as Kind>::NAME);
    assert_eq!(request.field, "replica", "the requesting Params field rides the manifest");

    assert_eq!(echo, ParamsEcho { index: 3, count: 7 }, "the guest's init received the identity the load carried");
}

/// The bug this catches: an unreplicated load resolving to the field-wise zero
/// instead of "replica 0 of 1" — a `count` of 0 that a component sharding work
/// by `index % count` divides by. The host substitutes
/// [`ReplicaIdentity::SOLE`] precisely so a component needs no
/// am-I-replicated branch, and that substitution is what this pins.
#[test]
fn an_unreplicated_load_injects_the_sole_identity_not_a_zero_count() {
    let Some(wasm) = bundle_wasm() else {
        return;
    };

    let (load, echo) = load_and_query(wasm, PARAMS_EXPORT, "solo", None);

    assert!(matches!(load, LoadResult::Ok { .. }), "an unreplicated load of a params-requesting component: {load:?}");
    assert_eq!(echo, ParamsEcho { index: 0, count: 1 }, "no replica on the load means replica 0 of 1");
}

/// The bug this catches: the load path not consulting the provider registry at
/// all — `validate` written but never called from `prepare_load` — so a
/// request nothing provides sails past into instantiation and surfaces (if at
/// all) as an opaque guest trap. The failure must be a clean `LoadResult::Err`
/// that names both the kind and the field, and no instance may exist
/// afterwards.
#[test]
fn a_request_no_provider_serves_fails_the_load_before_instantiation() {
    let Some(wasm) = bundle_wasm() else {
        return;
    };

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let report = harness
        .execute(vec![("load", load_op(wasm, UNPROVIDED_EXPORT, "unprovided", None))])
        .expect("the load itself settles — it is the load's verdict that must be Err");

    let LoadResult::Err { error } = report.reply::<LoadResult>("load").expect("decode LoadResult") else {
        panic!("a component requesting an unprovided kind must not load");
    };
    assert!(error.contains(<ConfigEcho as Kind>::NAME), "the error names the unprovided kind; was: {error}");
    assert!(error.contains("unprovided"), "the error names the requesting field; was: {error}");

    // And no instance was left behind: addressing the name the rejected load
    // would have registered finds no mailbox at all.
    let addressed = harness
        .execute(vec![("echo", HarnessOp::send_and_await_reply(trampoline_address("unprovided"), &ParamsQuery))]);
    assert!(addressed.is_err(), "a rejected load registers no mailbox, so the address does not resolve");
}

/// The bug this catches: the ADR-0170 FFI change regressing the no-request
/// path — the host shipping an encoded empty bag instead of zero bytes, or the
/// guest's `from_entries` rejecting an empty bag for a `Params` that asks for
/// nothing. Every component in the workspace but the two fixtures above is on
/// this path, so it has to keep loading and answering mail unchanged.
#[test]
fn a_component_declaring_no_params_still_loads_and_answers() {
    let Some(wasm) = bundle_wasm() else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");

    let report = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                ComponentHostCapability::NAMESPACE,
                &LoadComponent {
                    wasm,
                    name: Some("no_params".to_owned()),
                    config: Vec::new(),
                    export: Some("test.probe_with_config".to_owned()),
                    replica: None,
                },
            ),
        )])
        .expect("load sequence");

    let LoadResult::Ok { capabilities, .. } = report.reply::<LoadResult>("load").expect("decode LoadResult") else {
        panic!("a component that declares no Params must load unchanged");
    };
    assert!(capabilities.params.is_empty(), "a component with no `type Params` advertises no requests");
}
