//! Spawn and teardown: the typed and by-tag inline spawn verbs, their
//! up-front validation, the placement gate, and the `wire` / `unwire`
//! lifecycle calls the composition path makes.

use super::{
    __validate_inline_child_placement, ActorTypeTag, Addressable, ChildOf, FailingChild, LifecycleProbe,
    NO_INBOUND_SOURCE, NestingParent, PROBE_UNWIRE_COUNT, PROBE_WIRE_COUNT, PROBE_WIRE_FAILS, PROBE_WIRE_REFUSAL,
    Registry, STUB_INIT_CONFIG, STUB_INIT_COUNT, STUB_WIRE_COUNT, SpawnError, StubChild, StubConfig, SucceedingChild,
    WasmCtx, WasmPlacementFacts, install_inline_child, panicking_resolver, stub_resolver,
};
use crate::mail::Mail;
use crate::model::Anyone;
use crate::model::Subname;
use crate::model::ctx::{Erased, Unchecked};
use crate::reference::ErasedActorRef;
use crate::wasm::__validate_inline_child_alias;
use crate::wasm::inline::ChildRecord;
use crate::wasm::inline::compose::spawn_one_child;
use crate::wasm::inline::compose::{InlineChildToReconstruct, reconstruct_one_child};
use crate::wasm::inline::membrane_dispatch;
use aether_data::{Kind, MailboxId};
use alloc::boxed::Box;
use alloc::string::String;

/// Step 3: a synchronous `init` `Err` surfaces as
/// [`SpawnError::InitFailed`] (the inline child runs `init` in-process,
/// so its failure comes back synchronously).
/// Exercises [`install_inline_child`] directly so the host build runs
/// it without the panicking `spawn_inline_child` host-fn stub.
#[test]
fn install_inline_child_reports_init_failure() {
    let registry = Registry::new();
    let result = install_inline_child::<FailingChild>(
        &registry,
        MailboxId(0x5555),
        ChildRecord { full_subname: String::from("child"), ..ChildRecord::default() },
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

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let config_bytes = StubConfig { value: 0x1234_5678 }.encode_into_bytes();
    let alias = ctx
        .spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("tagged"), &config_bytes)
        .expect("a known tag spawns its exported type");

    assert!(registry.take(alias.id()).is_some(), "the tagged child is resident under the resolver's alias");
    assert_eq!(
        STUB_INIT_CONFIG.get(),
        Some(0x1234_5678),
        "the config bytes were decoded and threaded into the child's init",
    );
}

/// Issue 2789: a by-tag inline spawn records the **spawner** as the
/// child's parent, not the cluster root — so a nested by-tag spawn (an
/// inline child spawning its own child) is reachable through the spawner's
/// `ctx.child` / `ctx.parent`. The spawner's own id (`0x5AFE`) is set
/// distinct from the cluster root (`0x1111`) so the assertion fails against
/// the old `registry.self_id()` behavior. Owned logic: the by-tag spawn's
/// parent recording, mirroring the typed `spawn_inline_child` path.
#[test]
fn spawn_inline_child_by_tag_parents_to_the_spawner_not_the_root() {
    let registry = Registry::new();
    registry.set_self_id(0x1111);
    registry.set_entry_actor_tag(ActorTypeTag::of::<LifecycleProbe>());
    registry.insert_child(
        MailboxId(0x5AFE),
        ChildRecord {
            type_tag: ActorTypeTag::of::<NestingParent>().0,
            full_subname: String::from("spawner"),
            parent: 0x1111,
            ..ChildRecord::default()
        },
        Box::new(NestingParent),
    );
    registry.set_spawn_resolver(stub_resolver);
    STUB_INIT_CONFIG.set(None);

    let spawner = 0x5AFE_u64;
    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(spawner, &registry, NO_INBOUND_SOURCE);
    let alias = ctx
        .spawn_inline_child_by_tag(
            ActorTypeTag::of::<StubChild>(),
            Subname::Named("nested"),
            &StubConfig { value: 1 }.encode_into_bytes(),
        )
        .expect("a known tag spawns its exported type");

    assert_eq!(
        registry.parent_of(alias.id()),
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

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let unknown = ActorTypeTag(0xFFFF_FFFF_FFFF_FFFF);
    let result = ctx.spawn_inline_child_by_tag(unknown, Subname::Named("tagged"), &[]);
    assert!(
        matches!(result, Err(SpawnError::UnknownActorTag(t)) if t == unknown),
        "an unresolvable tag returns UnknownActorTag(tag), got {result:?}",
    );
    assert!(registry.child_metas().is_empty(), "an unknown tag inserts no child");
}

#[test]
fn by_tag_spawn_rejects_zero_host_alias_before_init() {
    let registry = Registry::new();
    registry.set_self_id(0x10);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    registry.set_spawn_resolver(zero_alias_resolver);
    STUB_INIT_CONFIG.set(None);

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let result = ctx.spawn_inline_child_by_tag(
        ActorTypeTag::of::<StubChild>(),
        Subname::Named("tagged"),
        &StubConfig { value: 0x1234_5678 }.encode_into_bytes(),
    );

    assert!(matches!(result, Err(SpawnError::AliasAllocationFailed)));
    assert_eq!(STUB_INIT_CONFIG.get(), None, "a zero alias stops before child init");
    assert!(registry.child_metas().is_empty(), "a zero alias stops before registry insertion");
}

#[test]
fn inline_child_alias_validator_accepts_nonzero_alias() {
    assert_eq!(
        __validate_inline_child_alias(0xABCD_0001).expect("a nonzero host alias is valid"),
        MailboxId(0xABCD_0001),
    );
}

/// Step 5(c): subname validation runs before the resolver — a
/// separator-bearing `Named` is rejected with
/// [`SpawnError::SubnameInvalid`] and the (panicking) resolver never
/// runs.
#[test]
fn spawn_inline_child_by_tag_rejects_bad_subname_before_resolver() {
    let registry = Registry::new();
    registry.set_spawn_resolver(panicking_resolver);

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(0x10, &registry, NO_INBOUND_SOURCE);
    let result = ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<StubChild>(), Subname::Named("bad:name"), &[]);
    assert!(
        matches!(result, Err(SpawnError::SubnameInvalid(_))),
        "a separator-bearing subname is rejected before the resolver runs, got {result:?}",
    );
}

fn zero_alias_resolver(
    registry: &Registry,
    parent: u64,
    tag: ActorTypeTag,
    is_counter: bool,
    full_subname: &str,
    config_bytes: &[u8],
) -> Result<MailboxId, SpawnError> {
    if tag == ActorTypeTag::of::<StubChild>() {
        __validate_inline_child_placement(registry, parent, tag, StubChild::__AETHER_PLACEMENT)?;
        let alias = __validate_inline_child_alias(0)?;
        spawn_one_child::<StubChild>(
            registry,
            parent,
            alias,
            tag.0,
            String::from(full_subname),
            is_counter,
            config_bytes,
        )
    } else {
        Err(SpawnError::UnknownActorTag(tag))
    }
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
        WasmPlacementFacts { is_instanced: false, exact_parent_tags: &[] },
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
fn by_tag_placement_accepts_any_listed_parent() {
    let registry = Registry::new();
    registry.set_self_id(0x40);
    registry.set_entry_actor_tag(ActorTypeTag::of::<LifecycleProbe>());

    let result = __validate_inline_child_placement(
        &registry,
        0x40,
        ActorTypeTag::of::<SucceedingChild>(),
        SucceedingChild::__AETHER_PLACEMENT,
    );
    assert!(result.is_ok(), "a child accepts a runtime parent later in its child_of list: {result:?}");
}

#[test]
fn placement_fixtures_cover_exact_lineage() {
    const EXACT_PARENT: ActorTypeTag = ActorTypeTag::of::<NestingParent>();
    const MISMATCH_PARENT: ActorTypeTag = ActorTypeTag::of::<LifecycleProbe>();

    fn assert_child_of<P: Addressable, C: ChildOf<P>>() {}

    assert_child_of::<NestingParent, FailingChild>();
    assert_child_of::<NestingParent, StubChild>();
    assert_child_of::<NestingParent, SucceedingChild>();
    assert_child_of::<LifecycleProbe, SucceedingChild>();

    assert_ne!(EXACT_PARENT, MISMATCH_PARENT, "the rejection candidate must have a distinct parent tag");
    assert_eq!(
        StubChild::__AETHER_PLACEMENT,
        WasmPlacementFacts { is_instanced: true, exact_parent_tags: &[EXACT_PARENT] },
        "the exact candidate must name only its declared parent",
    );
    assert!(
        !StubChild::__AETHER_PLACEMENT.exact_parent_tags.contains(&MISMATCH_PARENT),
        "the exact candidate must reject a different parent tag",
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
        ChildRecord {
            type_tag: ActorTypeTag::of::<NestingParent>().0,
            full_subname: String::from("nesting"),
            parent: 0x9000,
            ..ChildRecord::default()
        },
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
    install_inline_child::<LifecycleProbe>(
        &registry,
        probe,
        ChildRecord { full_subname: String::from("probe"), parent: 0x9200, ..ChildRecord::default() },
        (),
    )
    .expect("the probe installs");
    assert_eq!(PROBE_WIRE_COUNT.get(), 1, "a fresh inline spawn runs the child's wire exactly once");

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(0x9200, &registry, NO_INBOUND_SOURCE);
    let removed = ctx.despawn_inline_child(ErasedActorRef::new(probe));
    assert!(removed, "despawning a resident child returns true");
    assert_eq!(PROBE_UNWIRE_COUNT.get(), 1, "despawn runs the child's unwire exactly once");
    assert!(registry.take(probe).is_none(), "the despawned child's slot is gone");
}

/// Issue 7463 (ADR-0247 rule 3): an inline child whose `wire` returns an
/// error fails its spawn with that error, runs its `unwire`, and leaves no
/// slot. Catches the error dropped (the spawn would answer the alias of a
/// child that never wired), the failed child reinserted, and a child that
/// entered `wire` dropped without its `unwire`.
#[test]
fn an_inline_child_whose_wire_fails_fails_its_spawn_and_is_unwired() {
    let registry = Registry::new();
    registry.set_self_id(0x9400);
    PROBE_WIRE_COUNT.set(0);
    PROBE_UNWIRE_COUNT.set(0);
    PROBE_WIRE_FAILS.set(true);

    let probe = MailboxId(0x9401);
    let spawned = install_inline_child::<LifecycleProbe>(
        &registry,
        probe,
        ChildRecord { full_subname: String::from("probe"), parent: 0x9400, ..ChildRecord::default() },
        (),
    );
    PROBE_WIRE_FAILS.set(false);

    let Err(SpawnError::WireFailed(error)) = spawned else {
        panic!("a child whose wire returned an error must fail its spawn; got {spawned:?}");
    };
    assert_eq!(error.message(), PROBE_WIRE_REFUSAL, "the spawn error carries the child's own message");
    assert_eq!(PROBE_WIRE_COUNT.get(), 1, "the child's wire was entered once");
    assert_eq!(PROBE_UNWIRE_COUNT.get(), 1, "a child that entered wire runs its unwire");
    assert!(registry.take(probe).is_none(), "the failed child's slot is gone");
}

/// What a repeated spawn must leave behind: the one child the first spawn
/// built, initialised once and wired once, still holding the `value` it was
/// built with. The value is read from the child itself, through a dispatch
/// to `resident`.
fn assert_first_child_stands(registry: &Registry, parent: u64, resident: MailboxId, value: u32) {
    assert_eq!(STUB_INIT_COUNT.get(), 1, "the repeated spawn ran no second init");
    assert_eq!(STUB_WIRE_COUNT.get(), 1, "the repeated spawn ran no second wire");

    // SAFETY: a zero-length mail frame spans no memory, and the stub child's
    // dispatch reads no payload.
    let mail = unsafe { Mail::__from_ptr(0, 1, 0, 1, crate::NO_REPLY_HANDLE, resident.0) };
    let held = membrane_dispatch(parent, mail, registry, NO_INBOUND_SOURCE, |_mail| {
        panic!("the resident child handles its own mail")
    });
    assert_eq!(held, value, "the resident child kept the state it was first built with");
}

/// ADR-0249 §5: a typed spawn of a name at which a child of that type
/// already stands answers that child's alias. The repeat passes a different
/// config, and the host spawn fn is a panicking stub on this build, so the
/// test also proves no host call is made. Catches the second `init` whose
/// box replaced the resident child, which is what a `wire` run again after
/// a refused republish did.
#[test]
fn a_typed_spawn_of_a_resident_name_answers_the_resident_child() {
    let registry = Registry::new();
    let parent = 0x9500_u64;
    registry.set_self_id(parent);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    STUB_INIT_COUNT.set(0);
    STUB_WIRE_COUNT.set(0);

    let resident = install_inline_child::<StubChild>(
        &registry,
        MailboxId(0x9501),
        ChildRecord {
            type_tag: ActorTypeTag::of::<StubChild>().0,
            full_subname: String::from("kept"),
            parent,
            ..ChildRecord::default()
        },
        StubConfig { value: 7 },
    )
    .expect("the first spawn installs");

    let mut erased: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(parent, &registry, NO_INBOUND_SOURCE);
    let again = erased
        .__for_actor::<NestingParent>()
        .spawn_inline::<StubChild>(Subname::Named("kept"), &StubConfig { value: 9 })
        .expect("a spawn of a resident name answers the child that stands");

    assert_eq!(again.id(), resident, "the repeated spawn answered the resident child's alias");
    assert_first_child_stands(&registry, parent, resident, 7);
}

/// ADR-0249 §5: a by-tag spawn of a resident name answers the resident
/// child before the resolver runs, which is where the by-tag path allocates
/// its host alias; the repeat runs under a resolver that panics. Catches the
/// second `init` over a resident child on the tag-selected path.
#[test]
fn a_by_tag_spawn_of_a_resident_name_answers_the_resident_child() {
    let registry = Registry::new();
    let parent = 0x9600_u64;
    registry.set_self_id(parent);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    registry.set_spawn_resolver(stub_resolver);
    STUB_INIT_COUNT.set(0);
    STUB_WIRE_COUNT.set(0);

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(parent, &registry, NO_INBOUND_SOURCE);
    let tag = ActorTypeTag::of::<StubChild>();
    let resident = ctx
        .spawn_inline_child_by_tag(tag, Subname::Named("kept"), &StubConfig { value: 7 }.encode_into_bytes())
        .expect("the first spawn installs");

    registry.set_spawn_resolver(panicking_resolver);
    let again = ctx
        .spawn_inline_child_by_tag(tag, Subname::Named("kept"), &StubConfig { value: 9 }.encode_into_bytes())
        .expect("a spawn of a resident name answers the child that stands");

    assert_eq!(again.id(), resident.id(), "the repeated spawn answered the resident child's alias");
    assert_first_child_stands(&registry, parent, resident.id(), 7);
}

/// Issue 2746: a `replace_component` reconstruct runs `init` +
/// `on_rehydrate` and never `wire` in itself — the fresh-spawn-vs-reload
/// distinction. A rebuild leaves the child unwired; the `wire` export wires
/// rebuilt children after the entry actor's `wire` (ADR-0249 §6). Guards
/// against a future move of the `wire` call into the shared `insert_child`,
/// which would wrongly fire it on every reload.
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
    reconstruct_one_child::<LifecycleProbe>(&registry, &to_reconstruct)
        .expect("a ()-config probe reconstructs from empty bytes");
    assert_eq!(PROBE_WIRE_COUNT.get(), 0, "a reconstruct runs init + on_rehydrate, never wire");
    assert!(registry.take(alias).is_some(), "the reconstructed child is resident under its alias");
}

/// Issue 5722: `spawn_inline` keeps the parent-identity *read* — a ctx whose
/// mailbox identifies no actor is still rejected before the host alias call,
/// exactly as the two-type verb rejects it. Dropping the redundant `P` must
/// not mean dropping the guard, which an implementation that skipped the
/// registry read entirely would do (and then panic in the host-fn stub).
#[test]
fn spawn_inline_rejects_unavailable_parent_identity_before_host_call() {
    let registry = Registry::new();
    registry.set_self_id(0x7010);
    let mut erased: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(0x7010, &registry, NO_INBOUND_SOURCE);
    let ctx = erased.__for_actor::<NestingParent>();

    let result = ctx.spawn_inline::<SucceedingChild>(Subname::Named("bad:name"), &());
    assert!(
        matches!(result, Err(SpawnError::ParentIdentityUnavailable(MailboxId(0x7010)))),
        "a ctx with no registry actor identity is rejected before subname handling or allocation, got {result:?}",
    );
}

/// Issue 5722: `spawn_inline` reads its parent from the ctx rather than
/// naming it, and `A: Spawns<C>` already proved `C` lists that parent in its
/// `child_of(..)`. Under a ctx whose recorded identity is a `LifecycleProbe`,
/// the two-type verb naming `NestingParent` fails with
/// `ParentIdentityMismatch` (the sibling test above), while this one falls
/// through the parent gate to subname validation. The mismatch error is what
/// a wrong `P` copied from a sibling produces, and consumers `.ok()` or
/// warn-log it into a silently absent child.
#[test]
fn spawn_inline_accepts_any_recorded_parent_type() {
    let registry = Registry::new();
    registry.set_self_id(0x7020);
    registry.set_entry_actor_tag(ActorTypeTag::of::<LifecycleProbe>());
    let mut erased: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(0x7020, &registry, NO_INBOUND_SOURCE);
    let ctx = erased.__for_actor::<LifecycleProbe>();

    let result = ctx.spawn_inline::<SucceedingChild>(Subname::Named("bad:name"), &());
    assert!(
        matches!(result, Err(SpawnError::SubnameInvalid(_))),
        "the parent type is read, not named, so the spawn reaches subname validation, got {result:?}",
    );
}

/// ADR-0249 §6: a typed spawn of a resident name whose child never wired
/// wires it before answering. The child was rebuilt through `init` and
/// `on_rehydrate` without `wire`, so the spawn runs the step-1 helper and
/// answers its alias with no second `init` and no host call. Catches a
/// rebuilt child left permanently unwired when no `wire` body spawns it.
#[test]
fn a_typed_spawn_of_an_unwired_resident_wires_it_once() {
    let registry = Registry::new();
    let parent = 0x9700_u64;
    registry.set_self_id(parent);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    STUB_INIT_COUNT.set(0);
    STUB_WIRE_COUNT.set(0);
    let alias = MailboxId(0x9701);
    registry.insert_child(
        alias,
        ChildRecord {
            type_tag: ActorTypeTag::of::<StubChild>().0,
            full_subname: String::from("kept"),
            parent,
            ..ChildRecord::default()
        },
        Box::new(StubChild { value: 7 }),
    );

    let mut erased: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(parent, &registry, NO_INBOUND_SOURCE);
    let again = erased
        .__for_actor::<NestingParent>()
        .spawn_inline::<StubChild>(Subname::Named("kept"), &StubConfig { value: 9 })
        .expect("a spawn of an unwired resident wires it and answers");

    assert_eq!(again.id(), alias, "the spawn answered the resident child's alias");
    assert_eq!(STUB_INIT_COUNT.get(), 0, "the spawn ran no init");
    assert_eq!(STUB_WIRE_COUNT.get(), 1, "the spawn wired the unwired child once");
    // SAFETY: a zero-length mail frame spans no memory, and the stub child's
    // dispatch reads no payload.
    let mail = unsafe { Mail::__from_ptr(0, 1, 0, 1, crate::NO_REPLY_HANDLE, alias.0) };
    let held = membrane_dispatch(parent, mail, &registry, NO_INBOUND_SOURCE, |_mail| {
        panic!("the resident child handles its own mail")
    });
    assert_eq!(held, 7, "the resident child kept the state it was rebuilt with");
}

/// ADR-0249 §6: a by-tag spawn of a resident name whose child never wired
/// wires it before answering, without running the resolver. Catches the
/// tag-selected path leaving a rebuilt child unwired.
#[test]
fn a_by_tag_spawn_of_an_unwired_resident_wires_it_once() {
    let registry = Registry::new();
    let parent = 0x9800_u64;
    registry.set_self_id(parent);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    registry.set_spawn_resolver(panicking_resolver);
    STUB_INIT_COUNT.set(0);
    STUB_WIRE_COUNT.set(0);
    let alias = MailboxId(0x9801);
    registry.insert_child(
        alias,
        ChildRecord {
            type_tag: ActorTypeTag::of::<StubChild>().0,
            full_subname: String::from("kept"),
            parent,
            ..ChildRecord::default()
        },
        Box::new(StubChild { value: 7 }),
    );

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(parent, &registry, NO_INBOUND_SOURCE);
    let again = ctx
        .spawn_inline_child_by_tag(
            ActorTypeTag::of::<StubChild>(),
            Subname::Named("kept"),
            &StubConfig { value: 9 }.encode_into_bytes(),
        )
        .expect("a by-tag spawn of an unwired resident wires it and answers");

    assert_eq!(again.id(), alias, "the repeated spawn answered the resident child's alias");
    assert_eq!(STUB_INIT_COUNT.get(), 0, "the spawn ran no init");
    assert_eq!(STUB_WIRE_COUNT.get(), 1, "the spawn wired the unwired child once");
}

/// ADR-0249 §6: a spawn of a resident name whose `wire` refuses comes back
/// as [`SpawnError::WireFailed`]. Catches a refused `wire` installed as
/// live.
#[test]
fn a_spawn_of_an_unwired_resident_whose_wire_fails_reports_wire_failed() {
    let registry = Registry::new();
    let parent = 0x9900_u64;
    registry.set_self_id(parent);
    registry.set_entry_actor_tag(ActorTypeTag::of::<NestingParent>());
    registry.set_spawn_resolver(panicking_resolver);
    PROBE_WIRE_COUNT.set(0);
    PROBE_UNWIRE_COUNT.set(0);
    PROBE_WIRE_FAILS.set(true);
    let alias = MailboxId(0x9901);
    registry.insert_child(
        alias,
        ChildRecord {
            type_tag: ActorTypeTag::of::<LifecycleProbe>().0,
            full_subname: String::from("probe"),
            parent,
            ..ChildRecord::default()
        },
        Box::new(LifecycleProbe),
    );

    let ctx: WasmCtx<'_, Erased, Anyone, Unchecked> = WasmCtx::__new(parent, &registry, NO_INBOUND_SOURCE);
    let result = ctx.spawn_inline_child_by_tag(ActorTypeTag::of::<LifecycleProbe>(), Subname::Named("probe"), &[]);

    PROBE_WIRE_FAILS.set(false);
    assert!(
        matches!(result, Err(SpawnError::WireFailed(ref error)) if error.message() == PROBE_WIRE_REFUSAL),
        "a failing wire comes back as WireFailed, got {result:?}",
    );
    assert_eq!(PROBE_WIRE_COUNT.get(), 1, "the child's wire was entered once");
    assert_eq!(PROBE_UNWIRE_COUNT.get(), 1, "a child that entered wire runs its unwire");
}
