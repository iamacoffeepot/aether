//! ADR-0147 module-boot slot scenarios (`aether-test-fixtures-boot`).
//!
//! The fixture module exports `export!(boot = Boot, public = [WidgetA, WidgetB])`: `Boot`
//! is the unconditional boot actor, `WidgetA` / `WidgetB` are ordinary
//! selectable exports. `Boot` broadcasts `BOOT_OBSERVED` from `wire` (once per
//! instance) and `BOOT_TORN_DOWN` from `unwire` (once when it closes), so these
//! scenarios assert the host's per-`(engine, module content hash)` boot
//! singleton lifecycle end-to-end through mail: cardinality (N selector loads →
//! 1 boot), non-selectability (an `export = boot-namespace` load → `Err`),
//! survival (the boot outlives every widget), a drop at the boot closing it
//! for good, and a boot module refusing every republish.

use std::fs;

use aether_component::ComponentHostCapability;
use aether_data::ErasedActorPath;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{
    DescribeComponent, DescribeComponentResult, DropComponent, DropResult, ListComponents, ListComponentsResult,
    LoadComponent, LoadResult,
};
use aether_substrate::testing::successor_wasm;

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

/// ADR-0147 boot fixture markers (`aether-test-fixtures-boot`): the boot
/// actor broadcasts `BOOT_OBSERVED` from `wire` (once per instance) and
/// `BOOT_TORN_DOWN` from `unwire` (once when it closes); the scenarios
/// count them via `count_observed`.
const BOOT_OBSERVED: &str = "aether.test_fixture.boot_observed";
const BOOT_TORN_DOWN: &str = "aether.test_fixture.boot_torn_down";
/// The boot's published name: the root singleton at its namespace.
const BOOT_NAMESPACE: &str = "aether.test.boot.boot";
/// The republish subject with an added boot, and its bootless base
/// (issue 7109).
const SUBJECT_BOOT: &str = "republish_subject_boot";
const SUBJECT_BASE: &str = "republish_subject_base";

/// Load one named export of the boot fixture, blocking on `LoadResult::Ok`, and
/// return its trampoline's actor path.
fn load_boot_export(harness: &mut SubstrateHarness, wasm: &[u8], export: &str) -> ErasedActorPath {
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: Some(export.to_owned()) },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { path, .. } => path,
        LoadResult::Err { error } => panic!("boot fixture load({export}): {error}"),
    }
}

/// Drop one loaded actor, blocking on its `DropResult::Ok`.
fn drop_actor(harness: &mut SubstrateHarness, path: ErasedActorPath) {
    let dropped = harness
        .execute(vec![(
            "drop",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &DropComponent { target: path },
            ),
        )])
        .expect("drop sequence");
    match dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }
}

/// Drain the scheduler one cycle so any mail the preceding op set in flight is
/// processed before the next `count_observed` read. No actor in the fixture
/// subscribes `Tick`, so the advance only drains.
fn settle(harness: &mut SubstrateHarness) {
    harness.execute(vec![("settle", HarnessOp::advance(1))]).expect("settle advance");
}

/// Cardinality: two selector loads of the same module content instantiate the
/// boot actor exactly once — its `wire` marker is observed once, and it appears
/// exactly once in the loaded-component list, at its published name — not once
/// per load.
#[test]
fn module_boot_singleton_spawns_once_across_selector_loads() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_boot") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    load_boot_export(&mut harness, &wasm, "aether.test.boot.widget_a");
    load_boot_export(&mut harness, &wasm, "aether.test.boot.widget_b");
    settle(&mut harness);

    assert_eq!(
        harness.count_observed(BOOT_OBSERVED),
        1,
        "the module-boot singleton must be instantiated exactly once across two selector loads; \
         observed kinds: {:?}",
        harness.observed_kinds(),
    );

    let listed = harness
        .execute(vec![(
            "list",
            HarnessOp::send_and_await_reply(&harness.actor_ref::<ComponentHostCapability>(), &ListComponents {}),
        )])
        .expect("list sequence");
    let names = listed.reply::<ListComponentsResult>("list").expect("decode ListComponentsResult").names;
    // ADR-0241 §5: the boot is the root singleton at its published name, so
    // a boot spawned per load or beneath the host would list another name.
    let boot_listed = names.iter().filter(|n| n.contains("aether.test.boot.boot")).collect::<Vec<_>>();
    assert_eq!(
        boot_listed,
        ["aether.test.boot.boot"],
        "exactly one boot guest should be listed, at its published name, after two selector loads; got {names:?}"
    );
}

/// Two same-hash loads can both reach the component host before the first
/// module boot promotes `Live`. The second must join the actor-local pending
/// boot reservation instead of staging a duplicate deterministic boot name.
#[test]
fn concurrent_same_hash_loads_share_the_pending_boot() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_boot") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let widget_a = harness.send_deferred(
        harness.actor_ref::<ComponentHostCapability>(),
        &LoadComponent {
            wasm: wasm.clone(),
            name: None,
            config: Vec::new(),
            export: Some("aether.test.boot.widget_a".to_owned()),
        },
    );
    let widget_b = harness.send_deferred(
        harness.actor_ref::<ComponentHostCapability>(),
        &LoadComponent { wasm, name: None, config: Vec::new(), export: Some("aether.test.boot.widget_b".to_owned()) },
    );

    assert!(matches!(harness.await_deferred::<LoadResult>(widget_a).expect("first load reply"), LoadResult::Ok { .. }));
    assert!(matches!(
        harness.await_deferred::<LoadResult>(widget_b).expect("second load reply"),
        LoadResult::Ok { .. }
    ));
    settle(&mut harness);
    assert_eq!(harness.count_observed(BOOT_OBSERVED), 1, "both in-flight loads share one pending module boot");
}

/// Non-selectability (ADR-0147 §1): a load whose export selector names the boot
/// actor's own namespace is a clean `LoadResult::Err` citing ADR-0147 — the
/// boot is unconditional, not caller-selectable — never a second boot-type
/// trampoline alongside the singleton.
#[test]
fn boot_actor_is_not_selectable_by_export() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_boot") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &LoadComponent {
                    wasm,
                    name: None,
                    config: Vec::new(),
                    export: Some("aether.test.boot.boot".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Err { error } => {
            assert!(
                error.contains("ADR-0147") && error.contains("aether.test.boot.boot"),
                "selecting the boot actor must fail naming ADR-0147 and the boot namespace; got {error}",
            );
        }
        LoadResult::Ok { path: name, .. } => panic!("the boot actor must not be selectable by export; loaded {name}"),
    }
}

/// Drop the module boot, waiting for the drop's whole chain to settle: the
/// host forwards the drop on the caller's chain, so the boot's `unwire`
/// marker has been observed once this returns.
fn drop_boot(harness: &mut SubstrateHarness) {
    let boot = ErasedActorPath::new(BOOT_NAMESPACE).expect("the boot namespace is an actor path");
    harness
        .execute(vec![(
            "drop boot",
            HarnessOp::send_and_settle(
                &harness.actor_ref::<ComponentHostCapability>(),
                &DropComponent { target: boot },
            ),
        )])
        .expect("drop boot sequence");
}

/// Catches a boot torn down when the module's last widget unloads: after
/// every widget drops, the boot has not run `unwire`, the host still
/// describes its live guest, and a drop at it still finds it.
#[test]
fn module_boot_outlives_every_non_boot_instance() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_boot") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let widget_a = load_boot_export(&mut harness, &wasm, "aether.test.boot.widget_a");
    let widget_b = load_boot_export(&mut harness, &wasm, "aether.test.boot.widget_b");
    drop_actor(&mut harness, widget_a);
    drop_actor(&mut harness, widget_b);
    settle(&mut harness);

    assert_eq!(
        harness.count_observed(BOOT_TORN_DOWN),
        0,
        "the boot must outlive every widget of its module; observed kinds: {:?}",
        harness.observed_kinds(),
    );
    let described = harness
        .execute(vec![(
            "describe",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &DescribeComponent { name: BOOT_NAMESPACE.to_owned() },
            ),
        )])
        .expect("describe sequence");
    match described.reply::<DescribeComponentResult>("describe").expect("decode DescribeComponentResult") {
        DescribeComponentResult::Ok { capabilities } => {
            assert!(!capabilities.handlers.is_empty(), "the boot still hosts its guest");
        }
        DescribeComponentResult::Err { error } => panic!("the boot is still live after every widget dropped: {error}"),
    }

    drop_boot(&mut harness);
    assert_eq!(harness.count_observed(BOOT_TORN_DOWN), 1, "the boot ends only on its own drop");
}

/// Catches a boot respawned by a later load after it was dropped (its name
/// is spent, so the load would fail), and a drop the host refuses at a boot.
#[test]
fn a_drop_at_the_boot_closes_it_for_good() {
    let Some(wasm_path) = require_wasm("aether_test_fixtures_boot") else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    load_boot_export(&mut harness, &wasm, "aether.test.boot.widget_a");
    drop_boot(&mut harness);
    assert_eq!(
        harness.count_observed(BOOT_TORN_DOWN),
        1,
        "a drop at the boot closes it; observed kinds: {:?}",
        harness.observed_kinds(),
    );

    load_boot_export(&mut harness, &wasm, "aether.test.boot.widget_b");
    assert_eq!(
        harness.count_observed(BOOT_OBSERVED),
        1,
        "a later load of the module spawns no second boot; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}

/// Catches a republish that moves live instances onto or off a module that
/// declares a boot. A successor of the boot module declares the boot too, and
/// a successor that drops the boot republishes namespaces a boot module
/// published; both are refused before anything prepares or publishes, so no
/// second boot is spawned and the running boot is not torn down.
#[test]
fn a_module_that_declares_a_boot_is_not_replaceable() {
    let Some(boot_path) = require_wasm("aether_test_fixtures_boot") else {
        return;
    };
    let Some(subject_boot_path) = require_wasm(SUBJECT_BOOT) else {
        return;
    };
    let Some(subject_base_path) = require_wasm(SUBJECT_BASE) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let boot_wasm = fs::read(&boot_path).expect("read boot fixture wasm");

    load_boot_export(&mut harness, &boot_wasm, "aether.test.boot.widget_a");
    settle(&mut harness);
    let booted_once = harness.count_observed(BOOT_OBSERVED);
    let Err(SubstrateHarnessError::Publish(error)) = harness.publish(successor_wasm(&boot_wasm, 1)) else {
        panic!("a successor of a boot module must not be republished");
    };
    assert!(
        error.contains("declares the boot") && error.contains(BOOT_NAMESPACE),
        "the refusal names the successor's boot: {error}",
    );
    settle(&mut harness);
    assert_eq!(harness.count_observed(BOOT_OBSERVED), booted_once, "the refused successor spawns no boot");

    let subject_boot_wasm = fs::read(&subject_boot_path).expect("read subject boot fixture wasm");
    harness
        .load_any(&LoadComponent {
            wasm: subject_boot_wasm,
            name: None,
            config: Vec::new(),
            export: Some("test.republish.subject".to_owned()),
        })
        .expect("load the boot variant of the subject");
    settle(&mut harness);
    let booted = harness.count_observed(BOOT_OBSERVED);
    let subject_base_wasm = fs::read(&subject_base_path).expect("read subject base fixture wasm");
    let Err(SubstrateHarnessError::Publish(error)) = harness.publish(subject_base_wasm) else {
        panic!("a successor that drops a boot must not be republished");
    };
    assert!(
        error.contains("test.republish.subject") && error.contains("declares a boot"),
        "the refusal names the namespace a boot module published: {error}",
    );

    settle(&mut harness);
    assert_eq!(
        harness.count_observed(BOOT_OBSERVED),
        booted,
        "the refused successors spawn no boot of their own; observed kinds: {:?}",
        harness.observed_kinds(),
    );
    assert_eq!(harness.count_observed(BOOT_TORN_DOWN), 0, "the running boot is untouched by a refused republish");
}
