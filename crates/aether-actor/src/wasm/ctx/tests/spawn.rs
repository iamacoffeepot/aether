//! Spawn and teardown: the typed and by-tag inline spawn verbs, their
//! up-front validation, the placement gate, and the `wire` / `unwire`
//! lifecycle calls the composition path makes.

use super::{
    __validate_inline_child_placement, ActorTypeTag, Addressable, ChildOf, FailingChild, LifecycleProbe, ModuleChild,
    NO_INBOUND_SOURCE, NestingParent, PROBE_UNWIRE_COUNT, PROBE_WIRE_COUNT, Registry, STUB_INIT_CONFIG, SpawnError,
    StubChild, StubConfig, SucceedingChild, WasmCtx, WasmPlacementFacts, install_inline_child, panicking_resolver,
    stub_resolver,
};
use crate::model::Subname;
use crate::model::ctx::Manual;
use crate::wasm::inline::compose::{InlineChildToReconstruct, reconstruct_one_child};
use aether_data::{Kind, MailboxId};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// Step 3: a synchronous `init` `Err` surfaces as
/// [`SpawnError::InitFailed`] (the inline child runs `init` in-process,
/// unlike the detached `spawn_child` whose init failure logs async).
/// Exercises [`install_inline_child`] directly so the host build runs
/// it without the panicking `spawn_inline_child` host-fn stub.
#[test]
fn install_inline_child_reports_init_failure() {
    let registry = Registry::new();
    let result = install_inline_child::<FailingChild>(
        &registry,
        MailboxId(0x5555),
        0,
        String::from("child"),
        false,
        0,
        Vec::new(),
        (),
    );
    assert!(
        matches!(result, Err(SpawnError::InitFailed(_))),
        "a failing init must return SpawnError::InitFailed, got {result:?}",
    );
}

/// Step 3: subname validation parity with `spawn_child` — a
/// separator-bearing `Named` subname is rejected up front with
/// [`SpawnError::SubnameInvalid`], before any host round-trip (so the
/// host build's panicking host-fn stub is never reached).
#[test]
fn spawn_inline_child_rejects_invalid_subname() {
    let registry = Registry::new();
    registry.set_self_id(0);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    let ctx = WasmCtx::__new(0, &registry, NO_INBOUND_SOURCE);
    let result = ctx.spawn_inline_child::<NestingParent, FailingChild>(Subname::Named("bad:name"), &());
    assert!(
        matches!(result, Err(SpawnError::SubnameInvalid(_))),
        "a separator-bearing subname must return SubnameInvalid, got {result:?}",
    );
}

#[test]
fn typed_spawn_rejects_unavailable_parent_identity_before_host_call() {
    let registry = Registry::new();
    registry.set_self_id(0x6010);
    let ctx = WasmCtx::__new(0x6010, &registry, NO_INBOUND_SOURCE);

    let result = ctx.spawn_inline_child::<NestingParent, SucceedingChild>(Subname::Named("bad:name"), &());
    assert!(
        matches!(result, Err(SpawnError::ParentIdentityUnavailable(MailboxId(0x6010)))),
        "a ctx with no registry actor identity is rejected before subname handling or allocation, got {result:?}",
    );
}

#[test]
fn typed_spawn_rejects_mismatched_parent_identity_before_host_call() {
    let registry = Registry::new();
    registry.set_self_id(0x6020);
    registry.set_entry_actor_tag(ActorTypeTag::of::<LifecycleProbe>());
    let ctx = WasmCtx::__new(0x6020, &registry, NO_INBOUND_SOURCE);

    let result = ctx.spawn_inline_child::<NestingParent, SucceedingChild>(Subname::Named("bad:name"), &());
    assert!(
        matches!(
            result,
            Err(SpawnError::ParentIdentityMismatch { expected, actual })
                if expected == ActorTypeTag::of::<NestingParent>()
                    && actual == ActorTypeTag::of::<LifecycleProbe>()
        ),
        "a ctx executing a different actor is rejected before subname handling or allocation, got {result:?}",
    );
}

/// Step 5(a): a known tag resolves to its exported type, and the passed
/// `config_bytes` are decoded and threaded into that type's `init`. Owned
/// logic: the tag → type selection and the config-decode-into-init path,
/// neither a derive nor another crate's machinery.
#[test]
fn spawn_inline_child_by_tag_spawns_matched_type_and_threads_config() {
    let registry = Registry::new();
    registry.set_self_id(0x10);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    registry.set_spawn_resolver(stub_resolver);
    STUB_INIT_CONFIG.set(None);

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let config_bytes = StubConfig { value: 0x1234_5678 }.encode_into_bytes();
    let alias = ctx
        .spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("tagged"), &config_bytes)
        .expect("a known tag spawns its exported type");

    assert!(registry.take(alias).is_some(), "the tagged child is resident under the resolver's alias");
    assert_eq!(
        STUB_INIT_CONFIG.get(),
        Some(0x1234_5678),
        "the config bytes were decoded and threaded into the child's init",
    );
}

/// Issue 2789: a by-tag inline spawn records the **spawner** as the
/// child's parent, not the cluster root — so a nested by-tag spawn (an
/// inline child spawning its own child, e.g. the behavior host wrapping
/// a widget) is reachable through the spawner's `ctx.child` /
/// `ctx.parent`. The spawner's own id (`0x5AFE`) is set distinct from
/// the cluster root (`0x1111`) so the assertion fails against the old
/// `registry.self_id()` behavior. Owned logic: the by-tag spawn's
/// parent recording, mirroring the typed `spawn_inline_child` path.
#[test]
fn spawn_inline_child_by_tag_parents_to_the_spawner_not_the_root() {
    let registry = Registry::new();
    registry.set_self_id(0x1111);
    registry.set_entry_actor_tag(ActorTypeTag::of::<LifecycleProbe>());
    registry.insert_child(
        MailboxId(0x5AFE),
        ActorTypeTag::of::<NestingParent>().0,
        String::from("spawner"),
        false,
        0x1111,
        Vec::new(),
        Box::new(NestingParent),
    );
    registry.set_spawn_resolver(stub_resolver);
    STUB_INIT_CONFIG.set(None);

    let spawner = 0x5AFE_u64;
    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(spawner, &registry, NO_INBOUND_SOURCE);
    let alias = ctx
        .spawn_inline_child_by_tag(
            ActorTypeTag::of::<StubChild>(),
            Subname::Named("nested"),
            &StubConfig { value: 1 }.encode_into_bytes(),
        )
        .expect("a known tag spawns its exported type");

    assert_eq!(
        registry.parent_of(alias),
        Some(MailboxId(spawner)),
        "the by-tag child's recorded parent is the spawner, not the cluster root",
    );
}

/// Step 5(b): a tag matching no exported type returns
/// [`SpawnError::UnknownActorTag`] and inserts no child — the untrusted
/// runtime-tag path the spawner recovers from.
#[test]
fn spawn_inline_child_by_tag_unknown_tag_errors_and_inserts_nothing() {
    let registry = Registry::new();
    registry.set_spawn_resolver(stub_resolver);

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let unknown = ActorTypeTag(0xFFFF_FFFF_FFFF_FFFF);
    let result = ctx.spawn_inline_child_by_tag(unknown, Subname::Named("tagged"), &[]);
    assert!(
        matches!(result, Err(SpawnError::UnknownActorTag(t)) if t == unknown),
        "an unresolvable tag returns UnknownActorTag(tag), got {result:?}",
    );
    assert!(registry.child_metas().is_empty(), "an unknown tag inserts no child");
}

/// Step 5(c): subname validation runs before the resolver — a
/// separator-bearing `Named` is rejected with
/// [`SpawnError::SubnameInvalid`] and the (panicking) resolver never
/// runs.
#[test]
fn spawn_inline_child_by_tag_rejects_bad_subname_before_resolver() {
    let registry = Registry::new();
    registry.set_spawn_resolver(panicking_resolver);

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let result = ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("bad:name"), &[]);
    assert!(
        matches!(result, Err(SpawnError::SubnameInvalid(_))),
        "a separator-bearing subname is rejected before the resolver runs, got {result:?}",
    );
}

#[test]
fn by_tag_placement_rejects_non_instanced_selection() {
    let registry = Registry::new();
    registry.set_self_id(0x20);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    let child = ActorTypeTag(0xDEAD);

    let result = __validate_inline_child_placement(
        &registry,
        0x20,
        child,
        WasmPlacementFacts { is_instanced: false, module_child: true, exact_parent_tags: &[] },
    );
    assert!(matches!(result, Err(SpawnError::ActorNotInstanced(tag)) if tag == child));
}

#[test]
fn by_tag_placement_rejects_disallowed_parent() {
    let registry = Registry::new();
    registry.set_self_id(0x30);
    let parent = ActorTypeTag::of::<LifecycleProbe>();
    registry.set_entry_actor_tag(parent);
    let child = ActorTypeTag::of::<StubChild>();

    let result = __validate_inline_child_placement(&registry, 0x30, child, StubChild::__AETHER_PLACEMENT);
    assert!(
        matches!(result, Err(SpawnError::PlacementDenied { parent: actual, child: selected }) if actual == parent && selected == child),
        "an exact child rejects a different runtime parent, got {result:?}",
    );
}

#[test]
fn by_tag_placement_accepts_module_child() {
    let registry = Registry::new();
    registry.set_self_id(0x40);
    registry.set_entry_actor_tag(ActorTypeTag::of::<LifecycleProbe>());

    let result = __validate_inline_child_placement(
        &registry,
        0x40,
        ActorTypeTag::of::<SucceedingChild>(),
        SucceedingChild::__AETHER_PLACEMENT,
    );
    assert!(result.is_ok(), "a composable instanced actor accepts the actual module parent: {result:?}");
}

#[test]
fn placement_fixtures_cover_exact_and_composable_lineage() {
    const EXACT_PARENT: ActorTypeTag = ActorTypeTag::of::<NestingParent>();
    const MISMATCH_PARENT: ActorTypeTag = ActorTypeTag::of::<LifecycleProbe>();

    fn assert_child_of<P: Addressable, C: ChildOf<P>>() {}
    fn assert_module_child<C: ModuleChild>() {}

    assert_child_of::<NestingParent, FailingChild>();
    assert_child_of::<NestingParent, StubChild>();
    assert_module_child::<SucceedingChild>();
    assert_child_of::<NestingParent, SucceedingChild>();
    assert_child_of::<LifecycleProbe, SucceedingChild>();

    assert_ne!(EXACT_PARENT, MISMATCH_PARENT, "the rejection candidate must have a distinct parent tag");
    assert_eq!(
        StubChild::__AETHER_PLACEMENT,
        WasmPlacementFacts { is_instanced: true, module_child: false, exact_parent_tags: &[EXACT_PARENT] },
        "the exact candidate must name only its declared parent",
    );
    assert!(
        !StubChild::__AETHER_PLACEMENT.exact_parent_tags.contains(&MISMATCH_PARENT),
        "the exact candidate must reject a different parent tag",
    );
    assert_eq!(
        SucceedingChild::__AETHER_PLACEMENT,
        WasmPlacementFacts { is_instanced: true, module_child: true, exact_parent_tags: &[] },
        "the composable candidate carries module permission without exact parents",
    );
    assert!(
        SucceedingChild::__AETHER_PLACEMENT.exact_parent_tags.is_empty(),
        "a composable candidate must not also carry exact parents",
    );
}

/// Issue 2746: a fresh inline spawn runs the child's `wire` after `init`,
/// and a `wire` that spawns a nested inline child works — the reentrant
/// take/reinsert path that would be silent UB under a borrow held across
/// the call. Owned logic: the composition path's lifecycle call and its
/// reentrancy, not a derive or another crate's machinery.
#[test]
fn install_inline_child_runs_wire_and_supports_nested_spawn() {
    let registry = Registry::new();
    registry.set_self_id(0x9000);
    registry.set_spawn_resolver(stub_resolver);
    STUB_INIT_CONFIG.set(None);

    let parent = MailboxId(0x9001);
    install_inline_child::<NestingParent>(
        &registry,
        parent,
        ActorTypeTag::of::<NestingParent>().0,
        String::from("nesting"),
        false,
        0x9000,
        Vec::new(),
        (),
    )
    .expect("the nesting parent installs");

    // The parent's `wire` ran and it was reinserted into its slot.
    assert!(registry.take(parent).is_some(), "the parent's wire ran and it was reinserted");
    // The `wire` spawned a nested inline child mid-wire (the reentrant
    // install path) — resolved to the stub resolver's fixed alias.
    assert!(
        registry.take(MailboxId(0xABCD_0001)).is_some(),
        "wire installed a nested inline child (reentrant registry access)",
    );
    assert_eq!(
        STUB_INIT_CONFIG.get(),
        Some(0x0BAD_CAFE),
        "the nested child ran init with the config threaded through wire's spawn",
    );
}

/// Issue 2746: `despawn_inline_child` runs a resident child's `unwire`
/// before dropping it (and spawn ran its `wire`). Owned logic: the
/// teardown mirror the composition path now makes.
#[test]
fn despawn_inline_child_runs_unwire() {
    let registry = Registry::new();
    registry.set_self_id(0x9200);
    PROBE_WIRE_COUNT.set(0);
    PROBE_UNWIRE_COUNT.set(0);

    let probe = MailboxId(0x9201);
    install_inline_child::<LifecycleProbe>(&registry, probe, 0, String::from("probe"), false, 0x9200, Vec::new(), ())
        .expect("the probe installs");
    assert_eq!(PROBE_WIRE_COUNT.get(), 1, "a fresh inline spawn runs the child's wire exactly once");

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0x9200, &registry, NO_INBOUND_SOURCE);
    let removed = ctx.despawn_inline_child(probe);
    assert!(removed, "despawning a resident child returns true");
    assert_eq!(PROBE_UNWIRE_COUNT.get(), 1, "despawn runs the child's unwire exactly once");
    assert!(registry.take(probe).is_none(), "the despawned child's slot is gone");
}

/// Issue 2746: a `replace_component` reconstruct runs `init` +
/// `on_rehydrate`, never `wire` — the fresh-spawn-vs-reload distinction
/// that keeps `wire` a genuine-first-attach signal. Guards against a
/// future move of the `wire` call into the shared `insert_child`, which
/// would wrongly fire it on every reload.
#[test]
fn reconstruct_does_not_run_wire() {
    let registry = Registry::new();
    PROBE_WIRE_COUNT.set(0);

    let alias = MailboxId(0x9301);
    let to_reconstruct = InlineChildToReconstruct {
        alias,
        type_tag: 0,
        is_counter: false,
        full_subname: "probe",
        state_version: 0,
        state_bytes: &[],
        config_bytes: &[],
    };
    let ok = reconstruct_one_child::<LifecycleProbe>(&registry, &to_reconstruct);
    assert!(ok, "a ()-config probe reconstructs from empty bytes");
    assert_eq!(PROBE_WIRE_COUNT.get(), 0, "a reconstruct runs init + on_rehydrate, never wire");
    assert!(registry.take(alias).is_some(), "the reconstructed child is resident under its alias");
}
