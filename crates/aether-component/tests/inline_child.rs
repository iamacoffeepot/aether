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

use aether_kinds::{LoadComponent, LoadResult, ReplaceComponent, ReplaceResult};
use aether_substrate_bench::test_helpers::require_wasm;
use aether_substrate_bench::{BenchOp, SubstrateBench};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho, InlineProbe,
    TagSpawnQuery, TagSpawnReport,
};

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;

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
/// reconstruct), which is `SubstrateBench`'s lane; #1916's `FleetBench` already
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

    let mut bench = SubstrateBench::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineStatefulParent` from the `inline_child` bundle, capturing
    // its mailbox id for the replace. The name override keeps the registered
    // lineage address stable so the existing `parent_addr` / `child_addr`
    // strings remain valid.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
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
    // then read it back. `send_mail` is fire-and-settle, so the bumps land
    // before the query.
    let pre = bench
        .execute(vec![
            ("bump_a", BenchOp::send_mail::<Bump>(child_addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(child_addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(child_addr.as_str(), &CountQuery)),
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
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
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
    let post = bench
        .execute(vec![("query", BenchOp::send_and_await(child_addr.as_str(), &CountQuery))])
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
/// asserts (1) the bogus-tag spawn was rejected with `UnknownActorTag` (via a
/// `TagSpawnQuery`), (2) the tag-spawned child is live and stateful — its
/// counter climbs to 2 through its own alias — and (3) after a
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

    let mut bench = SubstrateBench::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineTagParent`, capturing its mailbox id for the replace. The
    // name override keeps the lineage address stable so `child_addr` is valid.
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
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

    // (1) The parent's `wire` attempted a bogus-tag spawn: assert the
    // generated resolver rejected it with `UnknownActorTag` (never spawned,
    // never panicked). (2) The known-tag child is live and stateful — bump
    // it to 2 through its own alias and read it back.
    let pre = bench
        .execute(vec![
            ("tag_report", BenchOp::send_and_await(parent_addr.as_str(), &TagSpawnQuery)),
            ("bump_a", BenchOp::send_mail::<Bump>(child_addr.as_str(), &Bump)),
            ("bump_b", BenchOp::send_mail::<Bump>(child_addr.as_str(), &Bump)),
            ("query", BenchOp::send_and_await(child_addr.as_str(), &CountQuery)),
        ])
        .expect("tag report + bump + query sequence");
    assert_eq!(
        pre.reply::<TagSpawnReport>("tag_report").expect("decode TagSpawnReport"),
        TagSpawnReport { unknown_tag_rejected: true },
        "a bogus ActorTypeTag must be rejected with UnknownActorTag by the generated resolver",
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
    let swapped = bench
        .execute(vec![(
            "swap",
            BenchOp::send_and_await(
                "aether.component",
                &ReplaceComponent { mailbox_id, wasm, drain_timeout_ms: None, config: Vec::new(), export: None },
            ),
        )])
        .expect("replace sequence");
    match swapped.reply::<ReplaceResult>("swap").expect("decode ReplaceResult") {
        ReplaceResult::Ok { .. } => {}
        ReplaceResult::Err { error } => panic!("replace_component: {error}"),
    }

    let post = bench
        .execute(vec![("query", BenchOp::send_and_await(child_addr.as_str(), &CountQuery))])
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
/// dispatch tail → `record_finished`), `SubstrateBench`'s lane; #1916's
/// `FleetBench` already proved over-the-wire inline addressing.
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

    let mut bench = SubstrateBench::builder().size(64, 48).with_component_host().build().expect("boot");
    let wasm = fs::read(&wasm_path).expect("read fixture wasm");

    // Load `InlineDespawnParent` from the `inline_child` bundle, then probe
    // the *live* child's alias: the membrane demuxes to the child, which
    // answers with the child marker, and the chain settles. The name override
    // keeps the registered lineage address stable so `parent_addr` / `child_addr`
    // remain valid.
    let live = bench
        .execute(vec![
            (
                "load",
                BenchOp::send_and_await(
                    "aether.component",
                    &LoadComponent {
                        wasm,
                        name: Some(FIXTURE_NAME.to_owned()),
                        config: Vec::new(),
                        export: Some("test.inline.despawn_parent".to_owned()),
                    },
                ),
            ),
            ("probe", BenchOp::send_and_await(child_addr.as_str(), &InlineProbe)),
        ])
        .expect("load + live-probe sequence");
    match live.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { .. } => {}
        LoadResult::Err { error } => panic!("inline_child_despawn load failed: {error}"),
    }
    assert_eq!(
        live.reply::<InlineEcho>("probe").expect("decode live-probe InlineEcho"),
        InlineEcho { who: INLINE_WHO_CHILD },
        "a probe to the live child's alias is demuxed to and answered by the child",
    );

    // Tear the child down via the parent (`ctx.despawn_inline_child(self.child)`),
    // then probe the *same* alias again. The kept alias routes the orphaned
    // probe to the parent's dispatch tail, so it settles (a SettlementTimeout
    // here would be the leak this verb prevents) and the *parent* answers.
    let post = bench
        .execute(vec![
            ("despawn", BenchOp::send_mail::<DespawnChild>(parent_addr.as_str(), &DespawnChild)),
            ("probe", BenchOp::send_and_await(child_addr.as_str(), &InlineProbe)),
        ])
        .expect("despawn + post-teardown probe must settle, not SettlementTimeout");
    assert_eq!(
        post.reply::<InlineEcho>("probe").expect("decode post-teardown InlineEcho"),
        InlineEcho { who: INLINE_WHO_PARENT },
        "after teardown, a probe to the same alias falls through to the parent \
         (kept alias → membrane no resident child → parent dispatch tail)",
    );
}
