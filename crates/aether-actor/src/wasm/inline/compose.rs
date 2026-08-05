//! Dehydrate-compose / rehydrate-reconstruct for inline children
//! (ADR-0114 §5), shared by both `export!` arms (single-actor and
//! multi-actor) so the symmetric walk lives in one place rather than
//! being copy-pasted per arm.
//!
//! On dehydrate ([`dehydrate`]): run the parent's `on_dehydrate`
//! into a capture buffer, walk every resident inline child running its
//! `erased_on_dehydrate` into its own capture buffer, and pack the
//! parent's blob plus each child's into one composite (`bundle`).
//! The shim then calls the host `save_state` **once** with the result.
//!
//! On rehydrate ([`reconstruct_inline_children`]): decompose the
//! composite, run the parent's `on_rehydrate` with its slice, then per
//! child entry call the codegen-supplied reconstruct callback (which
//! resolves the type tag against the module's `export!` set and re-`init`s
//! the child) before restoring its `type State` and re-registering it.
//!
//! Both halves are plain `alloc`-crate code with no FFI imports, so the
//! logic is exercised on the host unit-test build; the wasm32-only
//! `save_state` call lives in the `export!` shim, not here.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use aether_data::{Kind, MailboxId};

use crate::mail::PriorState;
use crate::wasm::ctx::{CapturedState, NO_INBOUND_SOURCE, SpawnError, WasmDropCtx, WasmInitCtx, install_inline_child};
use crate::wasm::inline::Registry;
use crate::wasm::inline::bundle::{self, ChildEntry};
use crate::wasm::{ActorInitError, ErasedWasmActor, WasmActor, WasmCtx};

/// Run the parent's `on_dehydrate` and every inline child's, packing one
/// composite migration bundle (ADR-0114 §5).
///
/// `run_parent_dehydrate` runs the live parent instance's `on_dehydrate`
/// against the supplied capturing [`WasmDropCtx`] (so the parent's own
/// `save_state` is captured, not forwarded to the host). `registry` is the
/// component's inline-child registry (the `export!`-emitted
/// `static __AETHER_INLINE`); its resident children are walked here.
///
/// Returns `None` when the parent's `on_dehydrate` saved nothing **and**
/// no inline children are resident — the no-bundle case, so the shim
/// skips the host `save_state` exactly as a no-saving component does
/// today (the substrate then skips `on_rehydrate`, ADR-0016 §3). Otherwise
/// returns `Some((version, bytes))` for the single host `save_state`;
/// with no inline children that is byte-identical to the parent's own
/// blob.
#[must_use]
pub fn dehydrate(
    mailbox_id: u64,
    registry: &Registry,
    run_parent_dehydrate: impl FnOnce(&mut WasmDropCtx<'_>),
) -> Option<(u32, Vec<u8>)> {
    // Parent half: capture whatever the parent's `on_dehydrate` saves.
    let mut parent_capture = CapturedState::default();
    {
        let mut ctx = WasmDropCtx::__new_capturing(mailbox_id, &mut parent_capture);
        run_parent_dehydrate(&mut ctx);
    }
    let parent_saved = parent_capture.take();

    // Child half: walk the registry, driving each child's `on_dehydrate`
    // into its own capture buffer. The metadata snapshot is taken first so
    // the per-child borrow in `with_child_mut` never overlaps the walk.
    let metas = registry.child_metas();
    let mut children = Vec::with_capacity(metas.len());
    for meta in metas {
        let mut child_capture = CapturedState::default();
        registry.with_child_mut(meta.id, |child| {
            let mut ctx = WasmDropCtx::__new_capturing(meta.id.0, &mut child_capture);
            child.erased_on_dehydrate(&mut ctx);
        });
        let (version, state_bytes) = child_capture.take().unwrap_or((0, Vec::new()));
        children.push(ChildEntry {
            alias_id: meta.id.0,
            type_tag: meta.type_tag,
            is_counter: meta.is_counter,
            full_subname: meta.full_subname,
            version,
            state_bytes,
            config_bytes: meta.config_bytes,
            parent_id: Some(meta.parent.0),
        });
    }

    // No parent save and no children: there is no bundle to migrate, so
    // skip the host save entirely (the unchanged no-state path).
    if parent_saved.is_none() && children.is_empty() {
        return None;
    }

    let (parent_version, parent_bytes) = parent_saved.unwrap_or((0, Vec::new()));
    Some(bundle::compose(parent_version, &parent_bytes, &children))
}

/// One inline child to reconstruct, handed to the codegen-supplied
/// reconstruct callback. The callback resolves [`Self::type_tag`] against
/// the module's `export!` types, re-`init`s that type, restores its
/// `type State` from `(state_version, state_bytes)` via `on_rehydrate`,
/// and re-registers it in the component's inline-child registry under
/// `alias` — all of which it can do because it expands inside the
/// `export!` arm that knows the type set. An unknown tag is logged and
/// skipped (the callback returns `false`).
pub struct InlineChildToReconstruct<'a> {
    /// The alias [`MailboxId`] to re-register the reconstructed child
    /// under — the substrate route under this id survived the swap
    /// (ADR-0022; the parent mailbox / slot is stable across replace), so
    /// re-keying the guest registry by it restores addressing without a
    /// host round-trip.
    pub alias: MailboxId,
    /// The actor-type tag to resolve against the exported type set.
    pub type_tag: u64,
    /// Whether the original spawn used a counter discriminator (carried
    /// into the rebuilt slot metadata).
    pub is_counter: bool,
    /// The resolved subname (carried into the rebuilt slot metadata).
    pub full_subname: &'a str,
    /// The child's saved `on_dehydrate` bundle version.
    pub state_version: u32,
    /// The child's saved `on_dehydrate` bundle bytes.
    pub state_bytes: &'a [u8],
    /// The child's encoded `Config` bytes, retained from the spawning
    /// slot (issue 2690) — decoded to re-`init` the child from its real
    /// config instead of empty bytes.
    pub config_bytes: &'a [u8],
}

/// Decompose a migration bundle, run the parent's `on_rehydrate` with its
/// slice, then reconstruct every inline child (ADR-0114 §5).
///
/// `run_parent_rehydrate` runs the freshly-`init`ed parent instance's
/// `on_rehydrate` with the parent's saved `(version, bytes)` rebuilt as a
/// [`PriorState`]. `registry` is the component's inline-child
/// registry (the `export!`-emitted `static __AETHER_INLINE`), forwarded to
/// each `reconstruct_child` call together with the child's effective
/// logical parent. `reconstruct_child` is the codegen callback that checks
/// the replacement module's current placement facts, re-`init`s one child
/// by type tag, restores its state, and re-registers it in that registry;
/// it returns `false` when the child cannot be restored.
///
/// Modern parent links reconstruct in iterative eligible passes so a parent
/// is resident before any descendant. A pass that makes no progress stops;
/// explicit orphans are never re-parented. Legacy, absent, and unusable
/// links fall back to the cluster root.
///
/// For a childless bundle the decompose yields the raw parent
/// `(version, bytes)` and no children, so the parent's `on_rehydrate`
/// sees the identical slice it would have today.
pub fn reconstruct_inline_children(
    version: u32,
    bytes: &[u8],
    registry: &Registry,
    run_parent_rehydrate: impl FnOnce(u32, &[u8]),
    mut reconstruct_child: impl FnMut(&Registry, MailboxId, &InlineChildToReconstruct<'_>) -> bool,
) {
    let decomposed = bundle::decompose(version, bytes);

    run_parent_rehydrate(decomposed.parent.version, &decomposed.parent.bytes);

    let cluster_root = MailboxId(registry.self_id());
    let mut pending = decomposed.children.iter().collect::<Vec<_>>();
    while !pending.is_empty() {
        let pending_count = pending.len();
        let mut deferred = Vec::with_capacity(pending_count);

        for entry in pending {
            let parent =
                entry.parent_id.filter(|parent_id| *parent_id != MailboxId::NONE.0).map_or(cluster_root, MailboxId);
            if parent != cluster_root && registry.actor_type_tag(parent).is_none() {
                deferred.push(entry);
                continue;
            }

            let to_reconstruct = InlineChildToReconstruct {
                alias: MailboxId(entry.alias_id),
                type_tag: entry.type_tag,
                is_counter: entry.is_counter,
                full_subname: &entry.full_subname,
                state_version: entry.version,
                state_bytes: &entry.state_bytes,
                config_bytes: &entry.config_bytes,
            };
            if !reconstruct_child(registry, parent, &to_reconstruct) {
                // An unknown type tag, placement rejected by the replacement
                // module's current facts, or a failed re-`init`: skip it.
                // Descendants remain deferred because this alias never
                // becomes resident.
                tracing::warn!(
                    target = "aether_actor::inline",
                    alias = to_reconstruct.alias.0,
                    parent = parent.0,
                    type_tag = to_reconstruct.type_tag,
                    "inline child not reconstructed across replace_component (unknown type tag, \
                     invalid current placement, or re-init failure); skipping",
                );
            }
        }

        if deferred.len() == pending_count {
            for entry in deferred {
                tracing::warn!(
                    target = "aether_actor::inline",
                    alias = entry.alias_id,
                    parent = entry.parent_id.unwrap_or(cluster_root.0),
                    type_tag = entry.type_tag,
                    "inline child not reconstructed across replace_component because its \
                     recorded parent is absent; leaving the orphan absent",
                );
            }
            break;
        }
        pending = deferred;
    }
}

/// Re-`init` one inline child of concrete type `A`, restore its `type State`,
/// and re-register it under `alias` at the cluster root. Retains the legacy
/// direct-call behavior; replacement uses [`reconstruct_one_child_at_parent`]
/// after resolving the persisted effective parent.
///
/// Decodes `A::Config` from `to_reconstruct.config_bytes` — the child's
/// real encoded config, retained in the slot since spawn (issue 2690) — so
/// a typed-config child re-`init`s with the config it was actually spawned
/// with, not empty bytes. A `()`-config child still round-trips (empty
/// bytes decode `Some(())`). Returns `false` (and does not register) when
/// the config bytes fail to decode (a genuinely undecodable blob) or
/// `A::init` returns `Err`; the caller logs and skips. The substrate alias
/// route under `alias` survived the swap (ADR-0022; the parent slot is
/// stable), so re-keying the guest registry by `alias` restores addressing
/// with no host round-trip.
///
/// This is the "decode real config → init → insert" core #2692's
/// `spawn_one_child` (tag-dispatched inline child spawn) is expected to
/// share as a sibling function, adding only the `on_rehydrate` restore
/// step below (per #2690's design notes, §Sequencing with #2692).
#[must_use]
pub fn reconstruct_one_child<A>(registry: &Registry, to_reconstruct: &InlineChildToReconstruct<'_>) -> bool
where
    A: WasmActor + ErasedWasmActor,
    <A as WasmActor>::State: ErasedWasmActor,
{
    reconstruct_one_child_at_parent::<A>(registry, MailboxId(registry.self_id()), to_reconstruct)
}

/// Re-`init` one inline child of concrete type `A`, restore its `type State`,
/// and re-register it under `alias` with the supplied logical `parent`.
/// Called by the `export!`-generated reconstruct callback after it matches
/// the type tag and validates the replacement module's current placement
/// facts.
#[must_use]
pub fn reconstruct_one_child_at_parent<A>(
    registry: &Registry,
    parent: MailboxId,
    to_reconstruct: &InlineChildToReconstruct<'_>,
) -> bool
where
    A: WasmActor + ErasedWasmActor,
    // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed as
    // the erased child. For an un-split component `State = Self`, so the
    // identity's `ErasedWasmActor` impl satisfies this.
    <A as WasmActor>::State: ErasedWasmActor,
{
    let Some(config) = <A::Config as Kind>::decode_from_bytes(to_reconstruct.config_bytes) else {
        return false;
    };
    let mut init_ctx = WasmInitCtx::__new(to_reconstruct.alias.0);
    // ADR-0156 §2: empty params for now — resolve `Params` to the compiled
    // default, mirroring the real-config decode above.
    let params = <A::Params as Default>::default();
    let Ok(mut child) = A::init(config, params, &mut init_ctx) else {
        return false;
    };

    // Restore the child's `type State` from its saved bundle before it is
    // registered, so the first inbound mail sees the rehydrated state.
    {
        // Rehydrate is not a mail dispatch — no inbound source on the ctx.
        let mut ctx = WasmCtx::__new(to_reconstruct.alias.0, registry, NO_INBOUND_SOURCE);
        // SAFETY: `state_bytes` lives for this call; `PriorState::__from_ptr`
        // forms a slice over it bounded by the borrow, never escaping.
        let prior = unsafe {
            PriorState::__from_ptr(
                to_reconstruct.state_version,
                to_reconstruct.state_bytes.as_ptr() as usize,
                to_reconstruct.state_bytes.len(),
            )
        };
        child.erased_on_rehydrate(&mut ctx, prior);
    }

    // The alias remains folded on the instance carry, but relative
    // addressing walks the logical parent link restored from the bundle.
    registry.insert_child(
        to_reconstruct.alias,
        to_reconstruct.type_tag,
        String::from(to_reconstruct.full_subname),
        to_reconstruct.is_counter,
        parent.0,
        to_reconstruct.config_bytes.to_vec(),
        Box::new(child),
    );
    true
}

/// Spawn one inline child of concrete type `A` selected by an
/// `ActorTypeTag` at runtime (issue 2692): decode `A::Config` from the real
/// `config_bytes`, then run the shared decode-free `install_inline_child`
/// core (`A::init` → `insert_child`). Called by the
/// `export!`-generated by-tag resolver once it has matched the tag to one of
/// the module's exported types; the resolver has already allocated `alias`
/// via the host `spawn_inline_child` host fn (so an unknown tag orphans no
/// alias — the resolver's fall-through never reaches this helper).
///
/// This is [`reconstruct_one_child`]'s twin — the *same* decode-real-config
/// `init` + insert core (issue 2690 made reconstruct decode from the slot's
/// retained bytes too), differing only in the absence of an `on_rehydrate`
/// state restore (a fresh spawn has no prior state). The child's logical
/// parent is recorded as `parent` — the id of the actor that issued the
/// spawn, threaded down from [`WasmCtx::spawn_inline_child_by_tag`] so a
/// *nested* by-tag spawn (an inline child spawning its own child, e.g. the
/// behavior host wrapping a widget) parents the new child to that spawning
/// actor rather than the cluster root, which is what lets the spawner's
/// `ctx.child` resolve it (issue 2688). A top-level spawn passes the root's
/// own id, so the flat-alias model is unchanged there.
///
/// Returns [`SpawnError::InitFailed`] when `A::Config` cannot decode from
/// `config_bytes` or `A::init` returns `Err`.
pub fn spawn_one_child<A>(
    registry: &Registry,
    parent: u64,
    alias: MailboxId,
    type_tag: u64,
    full_subname: String,
    is_counter: bool,
    config_bytes: &[u8],
) -> Result<MailboxId, SpawnError>
where
    A: WasmActor + ErasedWasmActor,
    // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed as
    // the erased child. For an un-split component `State = Self`. The same
    // erased bound set `reconstruct_one_child` uses — deliberately not
    // `Instanced`, which is an ergonomic guard on the *typed* call site a
    // tag-selected spawn neither can nor needs to enforce.
    <A as WasmActor>::State: ErasedWasmActor,
{
    let Some(config) = <A::Config as Kind>::decode_from_bytes(config_bytes) else {
        return Err(SpawnError::InitFailed(ActorInitError::new("spawn_inline_child_by_tag: Config decode failed")));
    };
    install_inline_child::<A>(
        registry,
        alias,
        type_tag,
        full_subname,
        is_counter,
        parent,
        config_bytes.to_vec(),
        config,
    )
}

#[cfg(test)]
mod tests {
    use super::{InlineChildToReconstruct, Registry, dehydrate, reconstruct_inline_children, reconstruct_one_child};
    use crate::mail::{Mail, PriorState};
    use crate::wasm::ctx::{NO_INBOUND_SOURCE, WasmDropCtx, WasmInitCtx};
    use crate::wasm::inline::bundle;
    use crate::wasm::{ActorInitError, ErasedWasmActor, WasmActor, WasmCtx};
    use crate::{Addressable, Lifecycle, Manual};
    use aether_data::{Kind, KindId, MailboxId};
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A child whose `on_dehydrate` saves a fixed 4-byte tag, so the
    /// compose can be asserted to carry the child's bytes. The reconstruct
    /// tests don't drive this type's dispatch.
    struct SavingChild {
        tag: u32,
    }

    impl ErasedWasmActor for SavingChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.saving_child"
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            0
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
            ctx.save_state(9, &self.tag.to_le_bytes());
        }
        fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
    }

    fn child_entry(alias_id: u64, type_tag: u64, parent_id: Option<u64>) -> bundle::ChildEntry {
        bundle::ChildEntry {
            alias_id,
            type_tag,
            is_counter: false,
            full_subname: String::from("child"),
            version: 0,
            state_bytes: Vec::new(),
            config_bytes: Vec::new(),
            parent_id,
        }
    }

    fn install_reconstructed(registry: &Registry, parent: MailboxId, child: &InlineChildToReconstruct<'_>) -> bool {
        registry.insert_child(
            child.alias,
            child.type_tag,
            String::from(child.full_subname),
            child.is_counter,
            parent.0,
            child.config_bytes.to_vec(),
            Box::new(SavingChild { tag: 0 }),
        );
        true
    }

    /// Step 3 coverage: a parent with two inline children yields a
    /// composite carrying both child entries plus the parent's own state,
    /// composed through one logical `save_state`.
    #[test]
    fn compose_dehydrate_packs_parent_and_children() {
        // Two children with distinct tags + type tags + aliases, in a
        // test-local registry (no shared-global aliasing across tests).
        let registry = Registry::new();
        let root = MailboxId(0x7000);
        let id_a = MailboxId(0xA1);
        let id_b = MailboxId(0xB2);
        registry.set_self_id(root.0);
        registry.insert_child(
            id_a,
            0xAAAA,
            String::from("a"),
            false,
            root.0,
            vec![0x11, 0x22],
            Box::new(SavingChild { tag: 0x1111_2222 }),
        );
        registry.insert_child(
            id_b,
            0xBBBB,
            String::from("b"),
            true,
            id_a.0,
            Vec::new(),
            Box::new(SavingChild { tag: 0x3333_4444 }),
        );

        // Parent saves a marker blob of its own.
        let (version, bytes) = dehydrate(0x7000, &registry, |ctx| {
            ctx.save_state(3, &[0xDE, 0xAD]);
        })
        .expect("a parent that saves plus two children yields a bundle");

        // Decompose and assert both children + the parent survived. The
        // local registry holds exactly the two inserted children.
        let decomposed = bundle::decompose(version, &bytes);
        assert_eq!(decomposed.parent.version, 3, "parent version is carried");
        assert_eq!(decomposed.parent.bytes, vec![0xDE, 0xAD]);
        assert_eq!(decomposed.children.len(), 2, "exactly the two inserted children are packed");
        let a = decomposed.children.iter().find(|c| c.alias_id == id_a.0).expect("child a present");
        assert_eq!(a.type_tag, 0xAAAA);
        assert_eq!(a.state_bytes, 0x1111_2222u32.to_le_bytes().to_vec());
        assert_eq!(a.config_bytes, vec![0x11, 0x22], "child a's config bytes ride the compose alongside its state");
        assert_eq!(a.parent_id, Some(root.0), "child a's root parent rides the appended metadata trailer");
        let b = decomposed.children.iter().find(|c| c.alias_id == id_b.0).expect("child b present");
        assert!(b.is_counter, "child b's counter flag is carried");
        assert_eq!(b.state_bytes, 0x3333_4444u32.to_le_bytes().to_vec());
        assert!(b.config_bytes.is_empty(), "child b was spawned with no retained config bytes");
        assert_eq!(b.parent_id, Some(id_a.0), "child b's nested parent rides the appended metadata trailer");
    }

    /// Step 4 coverage: each child entry is offered to the reconstruct
    /// callback with its type tag + alias + state, and an unknown tag is
    /// still offered (the callback decides to skip). The parent rehydrate
    /// runs once with the parent slice.
    #[test]
    fn reconstruct_offers_each_child_and_parent_slice() {
        // Build a composite with a parent blob + two children directly
        // through the bundle helpers. The callback only records, so the
        // registry threaded in is never inserted into here.
        use crate::wasm::inline::bundle::{ChildEntry, compose};

        const TAG_KNOWN: u64 = 0xBEEF;
        const TAG_UNKNOWN: u64 = 0xDEAD;

        let children = vec![
            ChildEntry {
                alias_id: 0xC1,
                type_tag: TAG_KNOWN,
                is_counter: false,
                full_subname: String::from("a"),
                version: 1,
                state_bytes: vec![1, 2, 3],
                config_bytes: Vec::new(),
                parent_id: None,
            },
            ChildEntry {
                alias_id: 0xC2,
                type_tag: TAG_UNKNOWN,
                is_counter: false,
                full_subname: String::from("b"),
                version: 2,
                state_bytes: vec![4, 5],
                config_bytes: Vec::new(),
                parent_id: None,
            },
        ];
        let (version, bytes) = compose(5, &[7, 7], &children);

        let registry = Registry::new();
        registry.set_self_id(0xC0);
        let mut parent_runs = 0u32;
        let mut offered: Vec<(u64, MailboxId, Vec<u8>)> = Vec::new();
        reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |pv, pb| {
                assert_eq!(pv, 5, "parent version slice is carried");
                assert_eq!(pb, &[7, 7], "parent bytes slice is carried");
                parent_runs += 1;
            },
            |_registry, parent, child| {
                offered.push((child.type_tag, parent, child.state_bytes.to_vec()));
                // An unknown tag is offered but the callback skips it.
                child.type_tag != TAG_UNKNOWN
            },
        );

        assert_eq!(parent_runs, 1, "the parent rehydrate runs exactly once");
        assert_eq!(offered.len(), 2, "both children are offered to the callback");
        assert_eq!(offered[0].1, MailboxId(0xC0), "legacy child a falls back to the cluster root");
        assert_eq!(offered[1].1, MailboxId(0xC0), "legacy child b falls back to the cluster root");
        assert_eq!(offered[0].2, vec![1, 2, 3], "child a state is carried");
        assert_eq!(offered[1].2, vec![4, 5], "child b state is carried");
    }

    #[test]
    fn legacy_bundle_reconstructs_children_under_cluster_root() {
        let root = MailboxId(0x1000);
        let child = MailboxId(0x1001);
        let (version, bytes) = bundle::compose(0, &[], &[child_entry(child.0, 0xA001, None)]);
        let registry = Registry::new();
        registry.set_self_id(root.0);

        reconstruct_inline_children(version, &bytes, &registry, |_, _| {}, install_reconstructed);

        assert_eq!(registry.parent_of(child), Some(root), "a trailer-free legacy child keeps the root fallback");
    }

    #[test]
    fn reconstruction_defers_descendant_recorded_before_parent() {
        let root = MailboxId(0x2000);
        let parent = MailboxId(0x2002);
        let descendant = MailboxId(0x2001);
        let children =
            vec![child_entry(descendant.0, 0xA002, Some(parent.0)), child_entry(parent.0, 0xA001, Some(root.0))];
        let (version, bytes) = bundle::compose(0, &[], &children);
        let registry = Registry::new();
        registry.set_self_id(root.0);
        let mut order = Vec::new();

        reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| {},
            |registry, parent, child| {
                order.push(child.alias);
                install_reconstructed(registry, parent, child)
            },
        );

        assert_eq!(order, vec![parent, descendant], "the parent reconstructs before its earlier-recorded descendant");
    }

    #[test]
    fn reconstruction_restores_each_exact_parent_link() {
        let root = MailboxId(0x3000);
        let branch = MailboxId(0x3001);
        let nested = MailboxId(0x3002);
        let leaf = MailboxId(0x3003);
        let children = vec![
            child_entry(branch.0, 0xA001, Some(root.0)),
            child_entry(nested.0, 0xA002, Some(branch.0)),
            child_entry(leaf.0, 0xA003, Some(nested.0)),
        ];
        let (version, bytes) = bundle::compose(0, &[], &children);
        let registry = Registry::new();
        registry.set_self_id(root.0);

        reconstruct_inline_children(version, &bytes, &registry, |_, _| {}, install_reconstructed);

        assert_eq!(registry.parent_of(branch), Some(root));
        assert_eq!(registry.parent_of(nested), Some(branch));
        assert_eq!(registry.parent_of(leaf), Some(nested));
    }

    #[test]
    fn rejected_and_missing_parents_leave_descendants_absent() {
        let root = MailboxId(0x4000);
        let rejected = MailboxId(0x4001);
        let descendant = MailboxId(0x4002);
        let missing_parent = MailboxId(0x4FFF);
        let orphan = MailboxId(0x4003);
        let children = vec![
            child_entry(rejected.0, 0xA001, Some(root.0)),
            child_entry(descendant.0, 0xA002, Some(rejected.0)),
            child_entry(orphan.0, 0xA003, Some(missing_parent.0)),
        ];
        let (version, bytes) = bundle::compose(0, &[], &children);
        let registry = Registry::new();
        registry.set_self_id(root.0);
        let mut offered = Vec::new();

        reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| {},
            |_registry, _parent, child| {
                offered.push(child.alias);
                false
            },
        );

        assert_eq!(offered, vec![rejected], "only the eligible but rejected parent is offered");
        assert!(registry.take(rejected).is_none());
        assert!(registry.take(descendant).is_none(), "a rejected parent's descendant stays absent");
        assert!(registry.take(orphan).is_none(), "an explicitly missing parent is never re-parented to the root");
    }

    #[test]
    fn reconstruction_terminates_when_no_parent_can_progress() {
        let root = MailboxId(0x5000);
        let left = MailboxId(0x5001);
        let right = MailboxId(0x5002);
        let children = vec![child_entry(left.0, 0xA001, Some(right.0)), child_entry(right.0, 0xA002, Some(left.0))];
        let (version, bytes) = bundle::compose(0, &[], &children);
        let registry = Registry::new();
        registry.set_self_id(root.0);
        let mut offered = 0;

        reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| {},
            |_registry, _parent, _child| {
                offered += 1;
                true
            },
        );

        assert_eq!(offered, 0, "a parent cycle reaches the no-progress exit without offering either child");
    }

    #[test]
    fn blocked_branch_does_not_prevent_independent_reconstruction() {
        let root = MailboxId(0x6000);
        let orphan = MailboxId(0x6001);
        let missing_parent = MailboxId(0x6FFF);
        let valid_parent = MailboxId(0x6003);
        let valid_child = MailboxId(0x6002);
        let children = vec![
            child_entry(orphan.0, 0xA001, Some(missing_parent.0)),
            child_entry(valid_child.0, 0xA003, Some(valid_parent.0)),
            child_entry(valid_parent.0, 0xA002, Some(root.0)),
        ];
        let (version, bytes) = bundle::compose(0, &[], &children);
        let registry = Registry::new();
        registry.set_self_id(root.0);

        reconstruct_inline_children(version, &bytes, &registry, |_, _| {}, install_reconstructed);

        assert!(registry.take(orphan).is_none(), "the blocked branch stays absent");
        assert!(registry.take(valid_parent).is_some(), "the independent parent reconstructs");
        assert!(registry.take(valid_child).is_some(), "the independent descendant reconstructs after its parent");
    }

    /// A typed (non-`()`) `Config` for step 5's reconstruct coverage: wraps
    /// a `u32`. Hand-rolls `Kind` rather than deriving it (a host-only test
    /// fixture needs no `Schema`/serde machinery) — `decode_from_bytes`
    /// only succeeds on exactly 4 bytes, so it decodes `None` from empty
    /// bytes just like a real typed config would, the branch
    /// `reconstruct_one_child` must still honor.
    #[derive(Clone, Copy, Default)]
    struct TypedConfig(u32);

    impl Kind for TypedConfig {
        const NAME: &'static str = "test.inline.typed_config";
        const ID: KindId = KindId(0x7A11_0000_0000_0001);

        fn decode_from_bytes(bytes: &[u8]) -> Option<Self> {
            let arr: [u8; 4] = bytes.try_into().ok()?;
            Some(Self(u32::from_le_bytes(arr)))
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            self.0.to_le_bytes().to_vec()
        }
    }

    /// A typed-config inline child for step 5's reconstruct coverage.
    /// `init` copies the decoded config into `observed`; `erased_dispatch`
    /// returns it as the dispatch code so the test can read back the value
    /// the child was actually `init`ed with — the erasure has no
    /// downcasting, so the dispatch return code is the observation channel
    /// (the same technique `RecordingChild` in `inline::mod`'s tests uses).
    struct TypedConfigChild {
        observed: u32,
    }

    impl Addressable for TypedConfigChild {
        const NAMESPACE: &'static str = "test.inline.typed_config_child";
        type Resolver = crate::Many;
    }

    impl Lifecycle<Self> for TypedConfigChild {
        type Config = TypedConfig;
        type Params = ();
        type InitError = ActorInitError;
        type InitCtx<'a> = WasmInitCtx<'a>;
        type Ctx<'a> = WasmCtx<'a>;

        fn init(config: TypedConfig, _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            Ok(Self { observed: config.0 })
        }
    }

    impl WasmActor for TypedConfigChild {
        type State = Self;
        type Persist = ();
    }

    impl crate::WasmDispatch<Self> for TypedConfigChild {
        fn dispatch(state: &mut Self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            state.observed
        }
    }

    impl ErasedWasmActor for TypedConfigChild {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _mail: Mail<'_>) -> u32 {
            self.observed
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Manual>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) {}
        fn erased_on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_, Manual>, _prior: PriorState<'_>) {}
    }

    /// Step 5 coverage (the branch this issue fixes): `reconstruct_one_child`
    /// over a `ChildEntry` carrying a typed-config child's real encoded
    /// config bytes re-`init`s it with that config — not the empty-bytes
    /// re-init that dropped every typed-config child before this fix. The
    /// reconstructed child is registered (returns `true`) and its
    /// `erased_dispatch` echoes back the decoded value, proving `init` saw
    /// the real config rather than a default.
    #[test]
    fn reconstruct_one_child_reinits_typed_config_from_real_bytes() {
        let registry = Registry::new();
        let alias = MailboxId(0x9001);
        let config = TypedConfig(0xDEAD_BEEF);
        let config_bytes = config.encode_into_bytes();
        let to_reconstruct = InlineChildToReconstruct {
            alias,
            type_tag: 0x1234,
            is_counter: false,
            full_subname: "typed",
            state_version: 0,
            state_bytes: &[],
            config_bytes: &config_bytes,
        };

        let reconstructed = reconstruct_one_child::<TypedConfigChild>(&registry, &to_reconstruct);
        assert!(reconstructed, "a typed-config child with real config bytes must reconstruct");

        let mut child = registry.take(alias).expect("the reconstructed child is registered under its alias");
        let code = child.erased_dispatch(
            &mut WasmCtx::__new(alias.0, &registry, NO_INBOUND_SOURCE),
            // SAFETY: a zero-length mail frame — ptr 0 with len 0 spans no
            // memory, and the probe child's dispatch reads no payload.
            unsafe { Mail::__from_ptr(0, 1, 0, 1, crate::NO_REPLY_HANDLE, alias.0) },
        );
        assert_eq!(code, 0xDEAD_BEEF, "the child's init decoded the real config value, not a default");
    }

    /// Step 5 coverage: an empty-bytes entry for a typed-config child still
    /// skips — the honest degradation for a genuinely undecodable blob
    /// (`TypedConfig::decode_from_bytes` requires exactly 4 bytes). Distinct
    /// from the `()`-config case, where empty bytes are the *correct*
    /// encoding and always decode `Some(())`.
    #[test]
    fn reconstruct_one_child_skips_typed_config_with_undecodable_bytes() {
        let registry = Registry::new();
        let alias = MailboxId(0x9002);
        let to_reconstruct = InlineChildToReconstruct {
            alias,
            type_tag: 0x1234,
            is_counter: false,
            full_subname: "typed",
            state_version: 0,
            state_bytes: &[],
            config_bytes: &[],
        };

        let reconstructed = reconstruct_one_child::<TypedConfigChild>(&registry, &to_reconstruct);
        assert!(!reconstructed, "empty bytes don't decode as a typed (non-unit) Config, so reconstruct skips");
        assert!(registry.take(alias).is_none(), "a skipped child is never registered");
    }
}
