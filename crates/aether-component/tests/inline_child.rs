//! ADR-0114 inline-child scenarios (rehomed per issue #3769): a wasm
//! parent's inline children carry state across `replace_component`
//! (typed reconstruct + by-tag spawn, issue 2692) and settle orphan mail
//! through the parent after a mid-life despawn (#1939). The children ride
//! aether-actor's inline-child machinery, but the component host is what
//! boots and replaces the hosting module, so the scenarios live here.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes
//! a hard panic there.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use aether_data::Kind;
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{ExecutionError, ExecutionResult, HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho, InlineProbe,
    TagSpawnQuery, TagSpawnReport,
};

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

/// Send `probe` to an inline child's alias once that alias resolves, and
/// hand back the sequence that reached it under the label `"probe"`.
///
/// An awaited `LoadResult::Ok` is not a barrier for the child becoming
/// addressable (iamacoffeepot/aether#4186). The load reply rides the
/// trampoline birth's own `SpawnOutcome`, while the child's alias is a
/// *second* registry-owner batch the trampoline stages from its `wire`
/// hook — and `wire` runs on a ctx rooted at `MailId::NONE`, so that batch
/// holds no chain and the load's settlement never covered it. ADR-0165's
/// activation suffix submits the batch and deliberately does not wait for
/// the owner to apply it, so nothing orders the alias against the reply.
/// There is no ordering to assert here, only an address to observe
/// becoming resolvable: poll to a bounded deadline so the test measures the
/// outcome rather than the runner. A child that never appears still fails,
/// just after 5s.
fn probe_child_alias<K: Kind>(harness: &mut SubstrateHarness, child_addr: &str, probe: &K) -> ExecutionResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match harness.execute(vec![("probe", HarnessOp::send_and_await_reply(child_addr, probe))]) {
            Ok(reached) => return reached,
            Err(ExecutionError::OpFailed { error: SubstrateHarnessError::UnknownMailbox(_), .. })
                if Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("inline child alias {child_addr} never answered a probe within 5s: {error}"),
        }
    }
}

/// ADR-0114 §5: an inline child carries its `type State` across a
/// `replace_component` swap. Loads `InlineStatefulParent` from the
/// `inline_child` bundle (issue 1994, ADR-0096) via
/// `export: Some("test.inline.stateful_parent")`, bumps the **child's**
/// counter to 2 through the child's first-class lineage address, replaces
/// the wasm at the same mailbox id with the same binary, then re-queries
/// the child's alias. The old instance's `on_dehydrate` packs the child's
/// state into the composite migration bundle; the new instance's
/// `on_rehydrate` reconstructs the child by type and restores its count —
/// so the post-replace query reads 2, not the fresh-`init` 0. Reload is
/// engine-internal correctness (dehydrate → composite → rehydrate
/// reconstruct), which is `SubstrateHarness`'s lane; #1916's `FleetHarness` already
/// proved the over-the-wire child addressing, so this doesn't re-prove it.
#[test]
fn replace_preserves_inline_child_state_via_reconstruct() {
    use aether_actor::Addressable;

    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_stateful";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let parent_addr = format!("aether.component/{}:{FIXTURE_NAME}", aether_component::WasmTrampoline::NAMESPACE);
    // The child's first-class lineage address: the parent's rendered name
    // plus the inline-child node (ADR-0114). The parent spawns it under
    // the `Named("widget")` subname in `wire`.
    let child_addr = format!("{parent_addr}/aether.embedded:widget");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineStatefulParent` from the `inline_child` bundle, capturing
    // its mailbox id for the replace. The name override keeps the registered
    // lineage address stable so the existing `parent_addr` / `child_addr`
    // strings remain valid.
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("test.inline.stateful_parent".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("inline_child_stateful load failed: {error}"),
    };

    // Bump the *child's* counter to 2 (mail demuxed to the child's alias),
    // then read it back. `send_and_settle` waits out each bump's whole chain,
    // so the bumps land before the query — but the alias itself only resolves once its own
    // owner batch applies, which the load reply does not order.
    probe_child_alias(&mut harness, &child_addr, &CountQuery);
    let pre = harness
        .execute(vec![
            ("bump_a", HarnessOp::send_and_settle::<Bump>(child_addr.as_str(), &Bump)),
            ("bump_b", HarnessOp::send_and_settle::<Bump>(child_addr.as_str(), &Bump)),
            ("query", HarnessOp::send_and_await_reply(child_addr.as_str(), &CountQuery)),
        ])
        .expect("bump + query sequence");
    assert_eq!(
        pre.reply::<CountReport>("query").expect("decode pre-replace CountReport"),
        CountReport { count: 2 },
        "two bumps should leave the inline child's counter at 2 before the replace",
    );

    // Replace the wasm at the parent's mailbox id with the same binary.
    // The old instance's `on_dehydrate` composites the child's state; the
    // new instance's `on_rehydrate` reconstructs the child and restores it.
    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = harness
        .execute(vec![(
            "swap",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &ReplaceComponent { mailbox_id, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    // Query the reconstructed child's alias: the count must still be 2.
    // A 0 here means the child vanished across the reload (its state lost,
    // or it booted fresh) — the regression ADR-0114 §5 closes.
    let post = harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(child_addr.as_str(), &CountQuery))])
        .expect("post-replace query sequence");
    let post_count = post.reply::<CountReport>("query").expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 2 },
        "the inline child's state must survive replace_component via the composite bundle + \
         rehydrate reconstruct; got {post_count:?} (0 means the child was not reconstructed)",
    );
}

/// Issue 2692: the real `export!`-generated by-tag resolver spawns an inline
/// child selected at runtime by `ActorTypeTag`, and the tag-spawned child
/// reconstructs across `replace_component`. Loads `InlineTagParent` from the
/// `inline_child` bundle via `export: Some("test.inline.tag_parent")`; the
/// parent's `wire` spawns `InlineStatefulChild` **by tag** (not the typed
/// verb) under `Named("tagged")` and also attempts a bogus tag. The scenario
/// asserts (1) a composable instanced child spawns, while a wrong exact
/// parent, an exported non-instanced actor, and a bogus tag are rejected,
/// (2) the tag-spawned child is live and stateful — its counter climbs to 2
/// through its own alias — and (3) after a
/// `replace_component` swap the child's count is still 2, i.e. the by-tag
/// child rides the same reconstruct arm its tag came from. Exercises the
/// generated resolver + host alias allocation end-to-end, which the
/// aether-actor host-unit tests (synthetic resolver, no wasm) cannot reach.
#[test]
#[allow(clippy::too_many_lines)]
fn spawn_inline_child_by_tag_spawns_and_reconstructs() {
    use aether_actor::Addressable;

    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_tag";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let parent_addr = format!("aether.component/{}:{FIXTURE_NAME}", aether_component::WasmTrampoline::NAMESPACE);
    // The tag-spawned child's first-class lineage address — the parent spawns
    // it under the `Named("tagged")` subname in `wire`.
    let child_addr = format!("{parent_addr}/aether.embedded:tagged");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineTagParent`, capturing its mailbox id for the replace. The
    // name override keeps the lineage address stable so `child_addr` is valid.
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("test.inline.tag_parent".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    let mailbox_id = match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("inline_child_tag load failed: {error}"),
    };

    // (1) Assert the generated resolver accepted only the composable child
    // and rejected wrong-parent, non-instanced, and unknown selections before
    // allocation. (2) The accepted child is live and stateful — bump it to 2
    // through its own alias and read it back, once that alias resolves.
    probe_child_alias(&mut harness, &child_addr, &CountQuery);
    let pre = harness
        .execute(vec![
            ("tag_report", HarnessOp::send_and_await_reply(parent_addr.as_str(), &TagSpawnQuery)),
            ("bump_a", HarnessOp::send_and_settle::<Bump>(child_addr.as_str(), &Bump)),
            ("bump_b", HarnessOp::send_and_settle::<Bump>(child_addr.as_str(), &Bump)),
            ("query", HarnessOp::send_and_await_reply(child_addr.as_str(), &CountQuery)),
        ])
        .expect("tag report + bump + query sequence");
    assert_eq!(
        pre.reply::<TagSpawnReport>("tag_report").expect("decode TagSpawnReport"),
        TagSpawnReport {
            composable_spawned: true,
            wrong_parent_rejected: true,
            non_instanced_rejected: true,
            unknown_tag_rejected: true,
        },
        "the generated resolver must enforce membership, instanced cardinality, and placement",
    );
    assert_eq!(
        pre.reply::<CountReport>("query").expect("decode pre-replace CountReport"),
        CountReport { count: 2 },
        "the tag-spawned InlineStatefulChild is live and its counter climbs to 2",
    );

    // (3) Replace the wasm at the parent's mailbox id with the same binary.
    // The tag-spawned child's state must reconstruct — its type tag is in the
    // same export! set the reconstruct arm walks.
    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = harness
        .execute(vec![(
            "swap",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &ReplaceComponent { mailbox_id, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post = harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(child_addr.as_str(), &CountQuery))])
        .expect("post-replace query sequence");
    let post_count = post.reply::<CountReport>("query").expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 2 },
        "the tag-spawned inline child's state must survive replace_component via reconstruct; \
         got {post_count:?} (0 means the by-tag child was not reconstructed)",
    );
}

/// ADR-0114 teardown (#1939): an inline child torn down mid-life still
/// settles mail to its now-dead alias through the parent. Loads
/// `InlineDespawnParent` from the `inline_child` bundle (issue 1994,
/// ADR-0096) via `export: Some("test.inline.despawn_parent")`, probes
/// the child's first-class alias and asserts the *child* answers + the
/// chain settles, sends a `DespawnChild` trigger to the parent (which
/// calls `ctx.despawn_inline_child` on the stored alias), then probes the
/// **same** alias again. The substrate alias route is kept on teardown, so
/// the orphaned probe lands in the parent's inbox, the membrane finds no
/// resident child and falls through to the parent's dispatch tail — the
/// *parent* answers and the chain **settles**. A `SettlementTimeout` on
/// the post-teardown probe would be the leak this verb exists to prevent.
/// Teardown settlement is engine-internal (membrane fallthrough → parent
/// dispatch tail → `record_finished`), `SubstrateHarness`'s lane; #1916's
/// `FleetHarness` already proved over-the-wire inline addressing.
#[test]
fn despawn_inline_child_settles_orphan_mail_via_parent() {
    use aether_actor::Addressable;

    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_despawn";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let parent_addr = format!("aether.component/{}:{FIXTURE_NAME}", aether_component::WasmTrampoline::NAMESPACE);
    // The child's first-class lineage address: the parent's rendered name
    // plus the inline-child node (ADR-0114). The parent spawns it under the
    // `Named("widget")` subname in `wire`.
    let child_addr = format!("{parent_addr}/aether.embedded:widget");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineDespawnParent` from the `inline_child` bundle, then probe
    // the *live* child's alias: the membrane demuxes to the child, which
    // answers with the child marker, and the chain settles. The name override
    // keeps the registered lineage address stable so `parent_addr` / `child_addr`
    // remain valid.
    let loaded = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await_reply(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("test.inline.despawn_parent".to_owned()),
                },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("inline_child_despawn load failed: {error}"),
    }
    let live = probe_child_alias(&mut harness, &child_addr, &InlineProbe);
    assert_eq!(
        live.reply::<InlineEcho>("probe").expect("decode live-probe InlineEcho"),
        InlineEcho { who: INLINE_WHO_CHILD },
        "a probe to the live child's alias is demuxed to and answered by the child",
    );

    // Tear the child down via the parent (`ctx.despawn_inline_child(self.child)`),
    // then probe the *same* alias again. The kept alias routes the orphaned
    // probe to the parent's dispatch tail, so it settles (a SettlementTimeout
    // here would be the leak this verb prevents) and the *parent* answers.
    let post = harness
        .execute(vec![
            ("despawn", HarnessOp::send_and_settle::<DespawnChild>(parent_addr.as_str(), &DespawnChild)),
            ("probe", HarnessOp::send_and_await_reply(child_addr.as_str(), &InlineProbe)),
        ])
        .expect("despawn + post-teardown probe must settle, not SettlementTimeout");
    assert_eq!(
        post.reply::<InlineEcho>("probe").expect("decode post-teardown InlineEcho"),
        InlineEcho { who: INLINE_WHO_PARENT },
        "after teardown, a probe to the same alias falls through to the parent \
         (kept alias → membrane no resident child → parent dispatch tail)",
    );
}

/// ADR-0168 §1: a settlement-gated load covers the inline child's alias.
///
/// The alias route a `WasmTrampoline` publishes from its `wire` hook is a
/// birth-completing effect of the trampoline's own birth, so it holds the
/// chain that staged that birth — the `aether.component.load` chain. Settling
/// that chain therefore means the child is addressable, and this sequence
/// probes it in the very next op with no polling and no slack.
///
/// This is the sequence iamacoffeepot/aether#4186 measured failing: `wire` ran
/// on a rootless ctx, the alias batch held nothing, and `Settled` fired while
/// the route was still queued at the registry owner. The sibling scenarios
/// above still poll — deliberately, since they assert something else and
/// retiring their polls is separate work.
///
// Tripwire: the load chain's `Settled` covers the alias publication. Cutting
// the causing chain out of the `wire` ctx — or reverting the staged effect to
// the context's own root — puts the probe back in a race with the owner's
// apply, which is the defect class ADR-0168 was written for.
#[test]
fn settled_load_covers_the_inline_child_alias_publication() {
    use aether_actor::Addressable;

    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_settled_load";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let parent_addr = format!("aether.component/{}:{FIXTURE_NAME}", aether_component::WasmTrampoline::NAMESPACE);
    let child_addr = format!("{parent_addr}/aether.embedded:widget");

    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // `send_and_settle` is the settlement-gated op — it blocks on `Settled { root }`
    // for the whole load chain, where `send_and_await_reply` would resolve on the
    // `LoadResult` correlation alone. The probe follows in the same sequence,
    // so nothing but the hold orders it against the owner's alias apply.
    let reached = harness
        .execute(vec![
            (
                "load",
                HarnessOp::send_and_settle(
                    "aether.component",
                    &LoadComponent {
                        wasm,
                        name: Some(FIXTURE_NAME.to_owned()),
                        config: Vec::new(),
                        export: Some("test.inline.despawn_parent".to_owned()),
                    },
                ),
            ),
            ("probe", HarnessOp::send_and_await_reply(child_addr.as_str(), &InlineProbe)),
        ])
        .expect("a settled load must leave the inline child addressable");
    assert_eq!(
        reached.reply::<InlineEcho>("probe").expect("decode InlineEcho"),
        InlineEcho { who: INLINE_WHO_CHILD },
        "the probe reaches the live child, so the alias was published inside the load's settlement",
    );
}
