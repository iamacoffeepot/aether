//! ADR-0114 inline-child scenarios (rehomed per issue #3769): a wasm
//! parent's inline children carry state across `replace_component`
//! (typed reconstruct + by-tag spawn, issue 2692) and surrender their
//! address on a mid-life despawn (#1939, #4228), and match the host replies
//! to their own requests by request id (#6530). The children ride
//! aether-actor's inline-child machinery, but the component host is what
//! boots and replaces the hosting module, so the scenarios live here.
//!
//! Skipped when the fixture wasm hasn't been built (`require_wasm`); CI
//! pre-builds it and sets `AETHER_REQUIRE_RUNTIME=1` so the skip becomes
//! a hard panic there.

use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::{ActorRef, Addressable, ChildOf, Instanced};
use aether_component::{ComponentHostCapability, WasmTrampoline};
use aether_data::{Kind, LoadName};
use aether_harness_substrate::test_helpers::{init_save_sandbox, require_wasm, test_namespace_roots, write_fixture};
use aether_harness_substrate::{HarnessOp, SubstrateHarness, SubstrateHarnessError};
use aether_kinds::{LoadComponent, ReplaceComponent, ReplaceResult};
use aether_test_fixtures_bundle::{
    InlineChild, InlineDespawnChild, InlineDespawnParent, InlineFsDemuxChild, InlineFsDemuxParent, InlineParent,
    InlineStatefulChild, InlineStatefulParent, InlineTagParent, NestedDetachedLeaf, NestedLineageChild,
    NestedLineageLeaf, NestedLineageParent,
};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnChild, FsDemuxReport, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho,
    InlineProbe, RunFsDemux, SpawnNestedDetached, TagSpawnQuery, TagSpawnReport,
};

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

/// A child's instance key.
fn key(text: &str) -> LoadName {
    LoadName::new(text).expect("a valid instance key")
}

/// The `C` inline child keyed `key` beneath `parent`, once its alias is live.
///
/// An awaited `LoadResult::Ok` is not a barrier for the child becoming
/// addressable (iamacoffeepot/aether#4186). The load reply rides the
/// trampoline birth's own `SpawnOutcome`, while the child's alias is a
/// *second* registry-owner batch the trampoline stages from its `wire`
/// hook — and `wire` runs on a ctx rooted at `MailId::NONE`, so that batch
/// holds no chain and the load's settlement never covered it. ADR-0165's
/// activation suffix submits the batch and deliberately does not wait for
/// the owner to apply it, so nothing orders the alias against the reply.
/// There is no ordering to assert here, only a child to observe going live:
/// poll to a bounded deadline so the test measures the outcome rather than
/// the runner. A child that never appears still fails, just after 5s.
fn await_child<P, C>(harness: &SubstrateHarness, parent: ActorRef<P>, name: &str) -> ActorRef<C>
where
    P: Addressable,
    C: ChildOf<P> + Instanced,
{
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match harness.child::<P, C>(&parent, key(name)) {
            Ok(child) => return child,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!("inline child {name} never went live within 5s: {error}"),
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
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_stateful";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineStatefulParent` from the `inline_child` bundle, capturing
    // its path for the replace.
    let (parent, path) = harness
        .load::<InlineStatefulParent>(LoadComponent {
            wasm,
            name: Some(FIXTURE_NAME.to_owned()),
            config: Vec::new(),
            export: Some("test.inline.stateful_parent".to_owned()),
        })
        .unwrap_or_else(|error| panic!("inline_child_stateful load failed: {error}"));

    // Bump the *child's* counter to 2 (mail demuxed to the child's alias),
    // then read it back. `send_and_settle` waits out each bump's whole chain,
    // so the bumps land before the query — but the alias itself only resolves
    // once its own owner batch applies, which the load reply does not order.
    // The parent spawns the child under the `Named("widget")` subname in `wire`.
    let child = await_child::<InlineStatefulParent, InlineStatefulChild>(&harness, parent, "widget");
    let pre = harness
        .execute(vec![
            ("bump_a", HarnessOp::send_and_settle::<Bump>(&child, &Bump)),
            ("bump_b", HarnessOp::send_and_settle::<Bump>(&child, &Bump)),
            ("query", HarnessOp::send_and_await_reply(&child, &CountQuery)),
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
                &harness.actor_ref::<ComponentHostCapability>(),
                &ReplaceComponent { target: path, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
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
        .execute(vec![("query", HarnessOp::send_and_await_reply(&child, &CountQuery))])
        .expect("post-replace query sequence");
    let post_count = post.reply::<CountReport>("query").expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 2 },
        "the inline child's state must survive replace_component via the composite bundle + \
         rehydrate reconstruct; got {post_count:?} (0 means the child was not reconstructed)",
    );
}

/// Issue 6136: a private inline child — one its module lists under
/// `export!(…, private = [..])` rather than exporting — is rebuilt by a
/// `replace_component` swap and keeps answering its own mail. `InlineChild`
/// answers `InlineProbe` with the child marker and its parent with the parent
/// marker, so a rebuild that consulted only the exported types would drop the
/// child while its alias survived, and the parent would answer in its place.
#[test]
fn replace_rebuilds_a_private_inline_child() {
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_private";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let (parent, path) = harness
        .load::<InlineParent>(LoadComponent {
            wasm,
            name: Some(FIXTURE_NAME.to_owned()),
            config: Vec::new(),
            export: Some("test.inline.parent".to_owned()),
        })
        .unwrap_or_else(|error| panic!("inline_child_private load failed: {error}"));
    let child = await_child::<InlineParent, InlineChild>(&harness, parent, "widget");
    let pre = harness
        .execute(vec![("probe", HarnessOp::send_and_await_reply(&child, &InlineProbe))])
        .expect("pre-replace probe");
    assert_eq!(
        pre.reply::<InlineEcho>("probe").expect("decode pre-replace InlineEcho"),
        InlineEcho { who: INLINE_WHO_CHILD },
        "the private inline child answers its own probe before the replace",
    );

    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = harness
        .execute(vec![(
            "swap",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &ReplaceComponent { target: path, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post = harness
        .execute(vec![("probe", HarnessOp::send_and_await_reply(&child, &InlineProbe))])
        .expect("post-replace probe");
    assert_eq!(
        post.reply::<InlineEcho>("probe").expect("decode post-replace InlineEcho"),
        InlineEcho { who: INLINE_WHO_CHILD },
        "the private inline child must be rebuilt under its old alias; the parent answering means the replace \
         dropped it",
    );
}

/// Issue 6136: listing a type under `export!(…, private = [..])` makes it
/// rebuildable, not loadable. The host must refuse an export selector that
/// names the private `InlineChild`, so the private list stays out of the
/// constructor table and the manifest sections the host reads.
#[test]
fn a_private_inline_child_is_not_loadable_by_selector() {
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let loaded = harness.load_any(&LoadComponent {
        wasm,
        name: Some("inline_child_private_selector".to_owned()),
        config: Vec::new(),
        export: Some(InlineChild::NAMESPACE.to_owned()),
    });
    assert!(
        matches!(loaded, Err(SubstrateHarnessError::Load(_))),
        "a private inline child must not load by export selector; got {loaded:?}",
    );
}

/// Issue 4490 end-to-end lineage packet. A root wasm actor spawns an inline
/// `branch`, that actor immediately spawns an inline `leaf`, and later the
/// branch spawns a detached `worker`. Both grandchildren must live at the
/// rendered branch lineage rather than restarting from the component root.
/// The inline leaf accepts delivery, persists across replacement under the
/// same logical parent, and can still be found and torn down by that parent
/// after reconstruction; the detached worker remains independently live.
#[test]
#[allow(clippy::too_many_lines)]
fn nested_wasm_spawns_preserve_lineage_through_delivery_replace_and_teardown() {
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_nested_lineage";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");
    let (root, path) = harness
        .load::<NestedLineageParent>(LoadComponent {
            wasm,
            name: Some(FIXTURE_NAME.to_owned()),
            config: Vec::new(),
            export: Some("test.inline.nested_parent".to_owned()),
        })
        .unwrap_or_else(|error| panic!("nested lineage fixture load failed: {error}"));

    // Grandchild-depth delivery proves the inline alias extends `branch`.
    let branch = await_child::<NestedLineageParent, NestedLineageChild>(&harness, root, "branch");
    let leaf = await_child::<NestedLineageChild, NestedLineageLeaf>(&harness, branch, "leaf");
    let before = harness
        .execute(vec![
            ("bump_a", HarnessOp::send_and_settle::<Bump>(&leaf, &Bump)),
            ("bump_b", HarnessOp::send_and_settle::<Bump>(&leaf, &Bump)),
            ("query", HarnessOp::send_and_await_reply(&leaf, &CountQuery)),
        ])
        .expect("deliver to nested inline leaf");
    assert_eq!(before.reply::<CountReport>("query").expect("decode nested leaf count"), CountReport { count: 2 },);

    // Spawn a detached wasm actor while dispatching the inline branch. Its
    // predicted and registered identity must use the same branch seed.
    harness
        .execute(vec![(
            "spawn_worker",
            HarnessOp::send_and_settle::<SpawnNestedDetached>(&branch, &SpawnNestedDetached),
        )])
        .expect("nested detached spawn settles");
    let worker = await_child::<NestedLineageChild, NestedDetachedLeaf>(&harness, branch, "worker");
    let reached = harness
        .execute(vec![("probe", HarnessOp::send_and_await_reply(&worker, &CountQuery))])
        .expect("the detached worker answers");
    assert_eq!(
        reached.reply::<CountReport>("probe").expect("decode nested worker reply"),
        CountReport { count: 77 },
        "the detached worker is delivered at the executing inline actor's lineage",
    );

    let wasm = fs::read(&wasm_path).expect("re-read fixture wasm");
    let swapped = harness
        .execute(vec![(
            "swap",
            HarnessOp::send_and_await_reply(
                &harness.actor_ref::<ComponentHostCapability>(),
                &ReplaceComponent { target: path, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace nested lineage fixture");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace nested lineage fixture: {error}"),
    }

    let after = harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(&leaf, &CountQuery))])
        .expect("query reconstructed nested leaf");
    assert_eq!(
        after.reply::<CountReport>("query").expect("decode reconstructed nested leaf count"),
        CountReport { count: 2 },
        "rehydration restores the leaf under its persisted branch parent",
    );
    let worker_after = harness
        .execute(vec![("worker", HarnessOp::send_and_await_reply(&worker, &CountQuery))])
        .expect("detached worker outlives root replacement");
    assert_eq!(
        worker_after.reply::<CountReport>("worker").expect("decode post-replace worker reply"),
        CountReport { count: 77 },
    );

    // The reconstructed branch resolves its reconstructed child by logical
    // parent and retires that exact grandchild alias.
    harness
        .execute(vec![("despawn_leaf", HarnessOp::send_and_settle::<DespawnChild>(&branch, &DespawnChild))])
        .expect("despawn reconstructed nested leaf");
    let retired = harness.child::<NestedLineageChild, NestedLineageLeaf>(&branch, key("leaf"));
    assert!(retired.is_err(), "the reconstructed grandchild route retires at its nested position; got {retired:?}");
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
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_tag";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineTagParent`, capturing its path for the replace.
    let (parent, path) = harness
        .load::<InlineTagParent>(LoadComponent {
            wasm,
            name: Some(FIXTURE_NAME.to_owned()),
            config: Vec::new(),
            export: Some("test.inline.tag_parent".to_owned()),
        })
        .unwrap_or_else(|error| panic!("inline_child_tag load failed: {error}"));

    // (1) Assert the generated resolver accepted only the composable child
    // and rejected wrong-parent, non-instanced, and unknown selections before
    // allocation. (2) The accepted child is live and stateful — bump it to 2
    // through its own alias and read it back, once that alias resolves. The
    // parent spawns it under the `Named("tagged")` subname in `wire`.
    let child = await_child::<InlineTagParent, InlineStatefulChild>(&harness, parent, "tagged");
    let pre = harness
        .execute(vec![
            ("tag_report", HarnessOp::send_and_await_reply(&parent, &TagSpawnQuery)),
            ("bump_a", HarnessOp::send_and_settle::<Bump>(&child, &Bump)),
            ("bump_b", HarnessOp::send_and_settle::<Bump>(&child, &Bump)),
            ("query", HarnessOp::send_and_await_reply(&child, &CountQuery)),
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
                &harness.actor_ref::<ComponentHostCapability>(),
                &ReplaceComponent { target: path, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post = harness
        .execute(vec![("query", HarnessOp::send_and_await_reply(&child, &CountQuery))])
        .expect("post-replace query sequence");
    let post_count = post.reply::<CountReport>("query").expect("decode post-replace CountReport");
    assert_eq!(
        post_count,
        CountReport { count: 2 },
        "the tag-spawned inline child's state must survive replace_component via reconstruct; \
         got {post_count:?} (0 means the by-tag child was not reconstructed)",
    );
}

/// ADR-0114 teardown (#1939, #4228): tearing an inline child down takes its
/// address with it. Loads `InlineDespawnParent` from the `inline_child`
/// bundle (issue 1994, ADR-0096) via
/// `export: Some("test.inline.despawn_parent")`, probes the child's
/// first-class alias and asserts the *child* answers, sends a
/// `DespawnChild` trigger to the parent (which calls
/// `ctx.despawn_inline_child` on the stored alias), then looks the **same**
/// child up again.
///
/// Teardown retires the alias route with the child (#4228), and a child
/// lookup proves only a live route — so the second lookup does not land on
/// the parent's dispatch tail as a probe once did, it is refused outright.
/// That is the correcting signal a peer holding a stale address was
/// previously denied: before this, the alias resolved, the membrane found no
/// resident child, and the parent silently answered for an actor that no
/// longer existed.
///
/// The parent is probed through its own reference afterwards as the
/// positive control: it still answers, so the alias's disappearance is the
/// retirement rather than a host that went away with its child. Mail sent to
/// the retired alias's position takes the retired-route path instead, which
/// settles the chain (ADR-0080 §2) — asserted at the route level in
/// `aether-substrate`'s
/// `despawning_an_inline_child_retires_its_alias_and_notifies_watchers`.
/// #1916's `FleetHarness` already proved over-the-wire inline addressing.
#[test]
fn despawn_inline_child_retires_the_alias_address() {
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_despawn";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineDespawnParent` from the `inline_child` bundle, then probe
    // the *live* child's alias: the membrane demuxes to the child, which
    // answers with the child marker, and the chain settles. The parent spawns
    // the child under the `Named("widget")` subname in `wire`.
    let (parent, _) = harness
        .load::<InlineDespawnParent>(LoadComponent {
            wasm,
            name: Some(FIXTURE_NAME.to_owned()),
            config: Vec::new(),
            export: Some("test.inline.despawn_parent".to_owned()),
        })
        .unwrap_or_else(|error| panic!("inline_child_despawn load failed: {error}"));
    let child = await_child::<InlineDespawnParent, InlineDespawnChild>(&harness, parent, "widget");
    let live = harness
        .execute(vec![("probe", HarnessOp::send_and_await_reply(&child, &InlineProbe))])
        .expect("the live child answers");
    assert_eq!(
        live.reply::<InlineEcho>("probe").expect("decode live-probe InlineEcho"),
        InlineEcho { who: INLINE_WHO_CHILD },
        "a probe to the live child's alias is demuxed to and answered by the child",
    );

    // Tear the child down via the parent (`ctx.despawn_inline_child(self.child)`),
    // then look the *same* child up again. Its route is retired with the child,
    // and only a live route proves, so the lookup never lands on the parent.
    harness
        .execute(vec![("despawn", HarnessOp::send_and_settle::<DespawnChild>(&parent, &DespawnChild))])
        .expect("despawn must settle");

    let orphan = harness.child::<InlineDespawnParent, InlineDespawnChild>(&parent, key("widget"));
    assert!(
        orphan.is_err(),
        "a despawned alias must stop resolving, so a peer looking the child up is refused instead of \
         being silently answered by the host; got {orphan:?}",
    );

    // Positive control: the parent is still live and still answers, so the
    // alias's disappearance is the retirement and not a departed host.
    let parent = harness
        .execute(vec![("parent", HarnessOp::send_and_await_reply(&parent, &InlineProbe))])
        .expect("the parent must still be addressable after tearing its child down");
    assert_eq!(
        parent.reply::<InlineEcho>("parent").expect("decode post-teardown InlineEcho"),
        InlineEcho { who: INLINE_WHO_PARENT },
        "the host component survives its inline child's teardown",
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
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_settled_load";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder().size(64, 48).with_component_host().build().expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // `send_and_settle` is the settlement-gated op — it blocks on `Settled { root }`
    // for the whole load chain, where a reply wait would resolve on the
    // `LoadResult` correlation alone. The lookups follow with no poll, so
    // nothing but the hold orders them against the owner's alias apply.
    harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_settle(
                &host,
                &LoadComponent {
                    wasm,
                    name: Some(FIXTURE_NAME.to_owned()),
                    config: Vec::new(),
                    export: Some("test.inline.despawn_parent".to_owned()),
                },
            ),
        )])
        .expect("the load chain settles");

    // A settle carries no reply, so the loaded parent is reached as what it
    // is to the host: the trampoline child keyed by its load name. The inline
    // child's alias resolves to that trampoline's endpoint, keyed `widget`
    // beneath it, so it is looked up as a trampoline too and probed erased.
    let parent = harness
        .child::<ComponentHostCapability, WasmTrampoline>(&host, key(FIXTURE_NAME))
        .expect("the settled load's trampoline is live");
    let child = harness
        .child::<WasmTrampoline, WasmTrampoline>(&parent, key("widget"))
        .expect("a settled load must leave the inline child addressable");
    let reached = harness
        .execute(vec![("probe", HarnessOp::send_and_await_reply(child.erase(), &InlineProbe))])
        .expect("the inline child answers");
    assert_eq!(
        reached.reply::<InlineEcho>("probe").expect("decode InlineEcho"),
        InlineEcho { who: INLINE_WHO_CHILD },
        "the probe reaches the live child, so the alias was published inside the load's settlement",
    );
}

/// Issue 6530: an inline child matches the host replies to its own requests.
/// The child sends two identical `aether.fs.read` requests with `send_tracked`;
/// the replies carry indistinguishable payloads and arrive as host dispatches
/// to the child's alias, so the child reports only when `in_reply_to()` hands
/// it each reply's request id. A membrane that built the child a cluster ctx
/// for that dispatch left the child reading `None`, and it never reported.
#[test]
fn inline_child_matches_host_replies_to_its_own_requests() {
    const BUNDLE_STEM: &str = "aether_test_fixtures_bundle";
    const FIXTURE_NAME: &str = "inline_child_reply";

    let Some(wasm_path) = require_wasm(BUNDLE_STEM) else {
        return;
    };
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .namespace_roots(test_namespace_roots(init_save_sandbox("inline-child-reply")))
        .build()
        .expect("boot");
    let path = write_fixture("inline-child-reply.txt", b"same path, same reply payload");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    let (parent, _path) = harness
        .load::<InlineFsDemuxParent>(LoadComponent {
            wasm,
            name: Some(FIXTURE_NAME.to_owned()),
            config: Vec::new(),
            export: Some("test.inline.fs_demux_parent".to_owned()),
        })
        .unwrap_or_else(|error| panic!("inline_child_reply load failed: {error}"));
    let child = await_child::<InlineFsDemuxParent, InlineFsDemuxChild>(&harness, parent, "demux");

    let baseline = harness.count_observed(FsDemuxReport::NAME);
    harness
        .execute(vec![(
            "trigger",
            HarnessOp::send_and_settle(&child, &RunFsDemux { namespace: "save".to_owned(), path }),
        )])
        .expect("RunFsDemux to the inline child");
    assert_eq!(
        harness.count_observed(FsDemuxReport::NAME) - baseline,
        1,
        "the inline child did not match both fs replies by request id; observed kinds: {:?}",
        harness.observed_kinds(),
    );
}
