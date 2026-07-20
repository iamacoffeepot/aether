//! Phase 3 substrate-feature scenarios (issue 430). Each test boots
//! a `SubstrateBench` and exercises one substrate primitive — boot
//! lifecycle, component listing/exports, or inline-child state (via
//! `aether-test-fixtures`'s `probe` cdylib) — driving every step
//! through `SubstrateBench::execute` (issue 868). The render scenarios
//! moved to `aether-render`'s own `render_scenario` target (issue #3771)
//! and the text scenarios to `aether-text`'s `text_scenario` (issue #3772).
//!
//! Skipped when the fixture's wasm hasn't been built — fixture-loading
//! tests read `target/wasm32-unknown-unknown/{debug,release}/examples/probe.wasm`
//! and skip with an `eprintln!` when it's absent. CI builds the
//! fixture wasm before invoking `cargo test`; setting
//! `AETHER_REQUIRE_RUNTIME=1` (CI does) flips the skip point
//! into a hard panic so a missing pre-build is loud.
//!
//! The wasm locator + skip-or-panic gate live in
//! `aether_substrate_bench_capture::test_helpers` (issues 460 + 821).

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use std::path::Path;

use aether_data::MailboxId;
use aether_kinds::{
    DropComponent, DropResult, ListComponents, ListComponentsResult, LoadComponent, LoadResult, Ping, ReplaceComponent,
    ReplaceResult,
};
use aether_substrate_bench::{BenchOp, SubstrateBench};
use aether_substrate_bench_capture::test_helpers::require_runtime;
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho, InlineProbe,
    TagSpawnQuery, TagSpawnReport,
};

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;
use std::fs;

/// Caller-supplied component name passed to `LoadComponent`.
const PROBE_NAME: &str = "probe";
/// Full trampoline address the substrate registers under post-issue-634
/// Phase 4. Mail destined for the loaded probe goes here, not to the
/// bare `PROBE_NAME` (which isn't a registered mailbox). Built from
/// The `/`-rendered lineage a loaded component registers at (ADR-0099
/// §4): the component host `aether.component` `/`-joined to the
/// trampoline node — exactly what `LoadResult.name` reports.
fn probe_address() -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:{}", aether_component::WasmTrampoline::NAMESPACE, PROBE_NAME)
}
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";
/// ADR-0147 boot fixture markers (`crate::aether-test-fixtures-boot`): the boot
/// actor broadcasts `BOOT_OBSERVED` from `wire` (once per instance) and
/// `BOOT_TORN_DOWN` from `unwire` (once at teardown). The boot scenario counts
/// them via `count_observed`.
const BOOT_OBSERVED: &str = "aether.test_fixture.boot_observed";
const BOOT_TORN_DOWN: &str = "aether.test_fixture.boot_torn_down";

/// Load the probe into the bench via `execute`, blocking on the
/// `LoadResult` reply so subsequent `advance` ops see a
/// fully-instantiated and tick-subscribed component. Returns the
/// loaded component's `MailboxId` (the trampoline address), which
/// the drop / replace scenarios target. Pre-Phase-4 of issue 603 the
/// bench's `aether.control` mailbox (renamed to `aether.component` in
/// issue 638 phase 3) served as a single FIFO point for both load and
/// advance; Phase 4 split advance onto `aether.substrate_bench`, so load is
/// no longer naturally ordered ahead of advance — `SendAndAwait`
/// blocks on `LoadResult` before returning.
fn load_probe(bench: &mut SubstrateBench, wasm_path: &Path) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent { wasm, name: Some(PROBE_NAME.to_owned()), config: Vec::new(), export: None },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

#[path = "substrate_bench_scenario/boot.rs"]
mod boot;
#[path = "substrate_bench_scenario/component.rs"]
mod component;
#[path = "substrate_bench_scenario/inline_child.rs"]
mod inline_child;

// Pre-#775 the bench emitted `aether.observation.frame_stats` every
// 120 frames and a test verified one broadcast arrived after
// `advance(120)`. Issue 775 retired the broadcast cap, the frame_stats
// kind, and the helper that emitted it; this test went with them.
