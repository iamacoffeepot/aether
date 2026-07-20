//! ADR-0147 module-boot slot scenarios (`aether-test-fixtures-boot`).
//!
//! The fixture module exports `export!(boot = Boot, WidgetA, WidgetB)`: `Boot`
//! is the unconditional boot actor, `WidgetA` / `WidgetB` are ordinary
//! selectable exports. `Boot` broadcasts `BOOT_OBSERVED` from `wire` (once per
//! instance) and `BOOT_TORN_DOWN` from `unwire` (once at teardown), so these
//! scenarios assert the host's per-`(engine, module content hash)` boot
//! singleton lifecycle end-to-end through mail: cardinality (N selector loads →
//! 1 boot), non-selectability (an `export = boot-namespace` load → `Err`),
//! refcount survival (partial unload → boot lives), and teardown (last unload →
//! boot torn down).
//!
//! Teardown is observed via the boot's `BOOT_TORN_DOWN` marker rather than
//! `ListComponents`: `aether.component.drop` clears a trampoline's `Component`
//! but leaves its mailbox registered and addressable (an empty slot a
//! `replace` can refill), so a torn-down boot still appears in the loaded-
//! component list — the marker is the signal that its `unwire` actually ran.

use super::*;
use aether_substrate_bundle::FullBenchExt;

/// Load one named export of the boot fixture, blocking on `LoadResult::Ok`, and
/// return its trampoline `MailboxId`.
fn load_boot_export(bench: &mut SubstrateBench, wasm: &[u8], export: &str) -> MailboxId {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent { wasm: wasm.to_vec(), name: None, config: Vec::new(), export: Some(export.to_owned()) },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("boot fixture load({export}): {error}"),
    }
}

/// Drop one loaded actor, blocking on its `DropResult::Ok`.
fn drop_actor(bench: &mut SubstrateBench, mailbox_id: MailboxId) {
    let dropped = bench
        .execute(vec![("drop", BenchOp::send_and_await("aether.component", &DropComponent { mailbox_id }))])
        .expect("drop sequence");
    match dropped.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop_component: {error}"),
    }
}

/// Drain the scheduler one cycle so any fire-and-forget teardown mail the
/// preceding op set in flight (the host's self-directed `DropComponent` to a
/// zero-refcount boot) is fully processed before the next `count_observed`
/// read. No actor in the fixture subscribes `Tick`, so the advance only drains.
fn settle(bench: &mut SubstrateBench) {
    bench.execute(vec![("settle", BenchOp::advance(1))]).expect("settle advance");
}

/// Cardinality: two selector loads of the same module content instantiate the
/// boot actor exactly once — its `wire` marker is observed once, and it appears
/// exactly once in the loaded-component list — not once per load.
#[test]
fn module_boot_singleton_spawns_once_across_selector_loads() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_boot") else {
        return;
    };
    let mut bench = SubstrateBench::builder().size(64, 48).full().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    load_boot_export(&mut bench, &wasm, "aether.test.boot.widget_a");
    load_boot_export(&mut bench, &wasm, "aether.test.boot.widget_b");
    settle(&mut bench);

    assert_eq!(
        bench.count_observed(BOOT_OBSERVED),
        1,
        "the module-boot singleton must be instantiated exactly once across two selector loads; \
         observed kinds: {:?}",
        bench.observed_kinds(),
    );

    let listed = bench
        .execute(vec![("list", BenchOp::send_and_await("aether.component", &ListComponents {}))])
        .expect("list sequence");
    let names = listed.reply::<ListComponentsResult>("list").expect("decode ListComponentsResult").names;
    let boot_listed = names.iter().filter(|n| n.ends_with(":aether.test.boot.boot")).count();
    assert_eq!(boot_listed, 1, "exactly one boot trampoline should be listed after two selector loads; got {names:?}");
}

/// Non-selectability (ADR-0147 §1): a load whose export selector names the boot
/// actor's own namespace is a clean `LoadResult::Err` citing ADR-0147 — the
/// boot is unconditional, not caller-selectable — never a second boot-type
/// trampoline alongside the singleton.
#[test]
fn boot_actor_is_not_selectable_by_export() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_boot") else {
        return;
    };
    let mut bench = SubstrateBench::builder().size(64, 48).full().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
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
        LoadResult::Ok { name, .. } => panic!("the boot actor must not be selectable by export; loaded {name}"),
    }
}

/// Refcount + teardown: the boot survives a partial unload (one of two widgets
/// dropped) and is torn down only when the last non-boot actor from the module
/// unloads. Observed via the boot's `unwire` marker, which stays at zero across
/// the partial unload and reaches one after the final drop.
#[test]
fn module_boot_survives_partial_unload_and_tears_down_on_last() {
    let Some(wasm_path) = require_runtime("aether_test_fixtures_boot") else {
        return;
    };
    let mut bench = SubstrateBench::builder().size(64, 48).full().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let widget_a = load_boot_export(&mut bench, &wasm, "aether.test.boot.widget_a");
    let widget_b = load_boot_export(&mut bench, &wasm, "aether.test.boot.widget_b");
    settle(&mut bench);
    assert_eq!(
        bench.count_observed(BOOT_OBSERVED),
        1,
        "one boot instance should have spawned; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Partial unload: refcount 2 → 1, boot survives, no teardown marker.
    drop_actor(&mut bench, widget_a);
    settle(&mut bench);
    assert_eq!(
        bench.count_observed(BOOT_TORN_DOWN),
        0,
        "the boot must survive a partial unload while WidgetB is still loaded; observed kinds: {:?}",
        bench.observed_kinds(),
    );

    // Last unload: refcount 1 → 0, the host self-drops the boot and its
    // `unwire` broadcasts the teardown marker.
    drop_actor(&mut bench, widget_b);
    settle(&mut bench);
    assert_eq!(
        bench.count_observed(BOOT_TORN_DOWN),
        1,
        "the boot must be torn down when the last non-boot actor unloads; observed kinds: {:?}",
        bench.observed_kinds(),
    );
}
