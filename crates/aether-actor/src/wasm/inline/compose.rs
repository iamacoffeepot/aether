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
//! The walk stops at the first hook that returns an error and hands back
//! what the hooks that ran saved beside it (ADR-0249 §1).
//!
//! On rehydrate ([`reconstruct_inline_children`]): decompose the
//! composite, run the parent's `on_rehydrate` with its slice, then per
//! child entry call the codegen-supplied reconstruct callback (which
//! resolves the type tag against the module's `export!` set and re-`init`s
//! the child) before restoring its `type State` and re-registering it.
//! The first failure ends the rebuild and is returned: the republish it
//! belongs to is refused.
//!
//! Both halves are plain `alloc`-crate code with no FFI imports, so the
//! logic is exercised on the host unit-test build; the wasm32-only
//! `save_state` call lives in the `export!` shim, not here.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use aether_data::MailboxId;

use crate::mail::PriorState;
use crate::wasm::ctx::{CapturedState, NO_INBOUND_SOURCE, SpawnError, WasmDropCtx, WasmInitCtx, install_inline_child};
use crate::wasm::decode::decode_config;
use crate::wasm::inline::bundle::{self, ChildEntry};
use crate::wasm::inline::{ChildRecord, Registry};
use crate::wasm::{ActorInitError, ErasedWasmActor, WasmActor, WasmCtx};

/// A migration bundle as the host `save_state` takes it: `(version, bytes)`.
pub type SavedBundle = (u32, Vec<u8>);

/// Run the parent's `on_dehydrate` and every inline child's, packing one
/// composite migration bundle (ADR-0114 §5).
///
/// `run_parent_dehydrate` runs the live parent instance's `on_dehydrate`
/// against the supplied capturing [`WasmDropCtx`] (so the parent's own
/// `save_state` is captured, not forwarded to the host). `registry` is the
/// component's inline-child registry (the `export!`-emitted
/// `static __AETHER_INLINE`); its resident children are walked here.
///
/// Returns the bundle beside the hooks' outcome. The walk stops at the first
/// hook that returns an error, the parent's or a child's (ADR-0249 §1): a hook
/// that said no ends the step, and running the remaining children would move
/// more state out of a guest that is about to keep running. The bundle then
/// holds what the hooks that ran saved. A child the walk did not reach is
/// absent from it, and so is the refusing child when it saved nothing, so the
/// reinstated guest's rebuild leaves both resident and untouched.
///
/// The bundle is `None` when the parent's `on_dehydrate` saved nothing **and**
/// no child entry was packed — the no-bundle case, so the shim skips the host
/// `save_state` exactly as a no-saving component does (the substrate then
/// skips `on_rehydrate`, ADR-0016 §3). Otherwise it is `Some((version,
/// bytes))` for the single host `save_state`; with no inline children that is
/// byte-identical to the parent's own blob.
pub fn dehydrate(
    registry: &Registry,
    run_parent_dehydrate: impl FnOnce(&mut WasmDropCtx<'_>) -> Result<(), ActorInitError>,
) -> (Option<SavedBundle>, Result<(), ActorInitError>) {
    // Parent half: capture whatever the parent's `on_dehydrate` saves.
    let mut parent_capture = CapturedState::default();
    let mut outcome = run_parent_dehydrate(&mut WasmDropCtx::__new_capturing(&mut parent_capture, registry));
    let parent_saved = parent_capture.take();

    // Child half: walk the registry, driving each child's `on_dehydrate`
    // into its own capture buffer. The metadata snapshot is taken first so
    // the per-child borrow in `with_child_mut` never overlaps the walk. A
    // parent that refused leaves every child unreached.
    let metas = if outcome.is_ok() {
        registry.child_metas()
    } else {
        Vec::new()
    };
    let mut children = Vec::with_capacity(metas.len());
    for meta in metas {
        let mut child_capture = CapturedState::default();
        let hook = registry
            .with_child_mut(meta.id, |child| {
                child.erased_on_dehydrate(&mut WasmDropCtx::__new_capturing(&mut child_capture, registry))
            })
            .unwrap_or(Ok(()));
        let saved = child_capture.take();

        // A child that ran to the end is packed whether or not it saved, so
        // the successor rebuilds it. The refusing child is packed only when it
        // saved: an entry with no state would rebuild it fresh over the
        // resident one.
        let refused = hook.is_err();
        let packed = if refused {
            saved
        } else {
            Some(saved.unwrap_or((0, Vec::new())))
        };
        if let Err(error) = hook {
            outcome = Err(ActorInitError::from(format!("inline child `{}`: {error}", meta.full_subname)));
        }
        if let Some((version, state_bytes)) = packed {
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
        if refused {
            break;
        }
    }

    // No parent save and no children: there is no bundle to migrate, so
    // skip the host save entirely (the unchanged no-state path).
    if parent_saved.is_none() && children.is_empty() {
        return (None, outcome);
    }

    let (parent_version, parent_bytes) = parent_saved.unwrap_or((0, Vec::new()));
    (Some(bundle::compose(parent_version, &parent_bytes, &children)), outcome)
}

/// One inline child to reconstruct, handed to the codegen-supplied
/// reconstruct callback. The callback resolves [`Self::type_tag`] against
/// the module's `export!` types, re-`init`s that type, restores its
/// `type State` from `(state_version, state_bytes)` via `on_rehydrate`,
/// and re-registers it in the component's inline-child registry under
/// `alias` — all of which it can do because it expands inside the
/// `export!` arm that knows the type set. An unknown tag is an error the
/// callback returns.
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
/// by type tag, restores its state, and re-registers it in that registry.
///
/// Modern parent links reconstruct in iterative eligible passes so a parent
/// is resident before any descendant. Explicit orphans are never re-parented.
/// Legacy, absent, and unusable links fall back to the cluster root.
///
/// For a childless bundle the decompose yields the raw parent
/// `(version, bytes)` and no children, so the parent's `on_rehydrate`
/// sees the identical slice it would have today.
///
/// # Errors
/// The first failure, which ends the rebuild (ADR-0249 §1, §6): the parent's
/// `on_rehydrate` error, the error `reconstruct_child` returned for a child it
/// could not rebuild, or, when a pass makes no progress, an error naming a
/// child whose recorded parent never became resident.
pub fn reconstruct_inline_children(
    version: u32,
    bytes: &[u8],
    registry: &Registry,
    run_parent_rehydrate: impl FnOnce(u32, &[u8]) -> Result<(), ActorInitError>,
    mut reconstruct_child: impl FnMut(&Registry, MailboxId, &InlineChildToReconstruct<'_>) -> Result<(), ActorInitError>,
) -> Result<(), ActorInitError> {
    let decomposed = bundle::decompose(version, bytes);

    run_parent_rehydrate(decomposed.parent.version, &decomposed.parent.bytes)?;

    let cluster_root = MailboxId(registry.self_id());
    let mut pending = decomposed.children.iter().collect::<Vec<_>>();
    while !pending.is_empty() {
        let pending_count = pending.len();
        let mut deferred = Vec::with_capacity(pending_count);

        for entry in pending {
            let parent = entry.parent_id.map_or(cluster_root, MailboxId);
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
            reconstruct_child(registry, parent, &to_reconstruct)?;
        }

        let stalled = deferred.len() == pending_count;
        let orphan = deferred.first().filter(|_| stalled);
        if let Some(orphan) = orphan {
            return Err(ActorInitError::from(format!(
                "inline child `{}` was not rebuilt: its recorded parent is absent",
                orphan.full_subname
            )));
        }
        pending = deferred;
    }

    Ok(())
}

/// The error for a child whose persisted type tag matches no type the
/// module's `export!` lists. Called by the `export!`-generated reconstruct
/// callback.
#[doc(hidden)]
#[must_use]
pub fn unknown_child_type(child: &InlineChildToReconstruct<'_>) -> ActorInitError {
    ActorInitError::from(format!(
        "inline child `{}` was not rebuilt: this module lists no type with its tag {:#x}",
        child.full_subname, child.type_tag
    ))
}

/// The error for a child whose type the module still lists and whose
/// placement beneath its recorded parent the module now rejects. Called by
/// the `export!`-generated reconstruct callback.
#[doc(hidden)]
#[must_use]
pub fn placement_refused(child: &InlineChildToReconstruct<'_>, refusal: &SpawnError) -> ActorInitError {
    ActorInitError::from(format!(
        "inline child `{}` was not rebuilt: its placement is refused: {refusal:?}",
        child.full_subname
    ))
}

/// The held-reply guard both `on_dehydrate` exports run after the hooks
/// (ADR-0243 §6).
///
/// # Errors
/// When a held reply is still live: the dehydrate neither saved nor answered
/// it, so the republish is refused.
#[doc(hidden)]
pub fn held_unsaved(registry: &Registry) -> Result<(), ActorInitError> {
    if registry.__held_unsaved() {
        return Err(ActorInitError::from("a held reply is live and was not saved"));
    }
    Ok(())
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
/// bytes decode `Some(())`). The substrate alias
/// route under `alias` survived the swap (ADR-0022; the parent slot is
/// stable), so re-keying the guest registry by `alias` restores addressing
/// with no host round-trip.
///
/// This is the "decode real config → init → insert" core #2692's
/// `spawn_one_child` (tag-dispatched inline child spawn) is expected to
/// share as a sibling function, adding only the `on_rehydrate` restore
/// step below (per #2690's design notes, §Sequencing with #2692).
///
/// # Errors
/// As [`reconstruct_one_child_at_parent`].
pub fn reconstruct_one_child<A>(
    registry: &Registry,
    to_reconstruct: &InlineChildToReconstruct<'_>,
) -> Result<(), ActorInitError>
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
///
/// # Errors
/// Naming the child's subname and the cause, and registering nothing: the
/// config bytes no longer decode, `A::init` returned an error, or the child's
/// `on_rehydrate` did.
pub fn reconstruct_one_child_at_parent<A>(
    registry: &Registry,
    parent: MailboxId,
    to_reconstruct: &InlineChildToReconstruct<'_>,
) -> Result<(), ActorInitError>
where
    A: WasmActor + ErasedWasmActor,
    // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed as
    // the erased child. For an un-split component `State = Self`, so the
    // identity's `ErasedWasmActor` impl satisfies this.
    <A as WasmActor>::State: ErasedWasmActor,
{
    let subname = to_reconstruct.full_subname;
    let config = decode_config::<A::Config>(A::NAMESPACE, to_reconstruct.config_bytes).map_err(|error| {
        ActorInitError::from(format!("inline child `{subname}` was not rebuilt: its config no longer decodes: {error}"))
    })?;
    // ADR-0156 §2: empty params for now — resolve `Params` to the compiled
    // default, mirroring the real-config decode above.
    let params = <A::Params as Default>::default();
    let mut child = A::init(config, params, &mut WasmInitCtx::__new()).map_err(|error| {
        ActorInitError::from(format!("inline child `{subname}` was not rebuilt: init failed: {error}"))
    })?;

    // Restore the child's `type State` from its saved bundle before it is
    // registered, so the first inbound mail sees the rehydrated state.
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
    }
    .__with_registry(registry);
    child.erased_on_rehydrate(&mut ctx, prior).map_err(|error| {
        ActorInitError::from(format!("inline child `{subname}` was not rebuilt: on_rehydrate failed: {error}"))
    })?;

    // The persisted alias was originally folded under this same logical
    // parent. Restoring both values keeps routing and relative addressing on
    // one reconstructed lineage.
    registry.insert_child(
        to_reconstruct.alias,
        ChildRecord {
            type_tag: to_reconstruct.type_tag,
            full_subname: String::from(to_reconstruct.full_subname),
            is_counter: to_reconstruct.is_counter,
            parent: parent.0,
            config_bytes: to_reconstruct.config_bytes.to_vec(),
        },
        Box::new(child),
    );
    Ok(())
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
/// *nested* by-tag spawn (an inline child spawning its own child) parents the
/// new child to that spawning actor rather than the cluster root, which is
/// what lets the spawner's `ctx.child` resolve it (issue 2688) and gives the
/// host-scoped alias fold the same parent seed.
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
    let config = decode_config::<A::Config>(A::NAMESPACE, config_bytes).map_err(SpawnError::InitFailed)?;

    install_inline_child::<A>(
        registry,
        alias,
        ChildRecord { type_tag, full_subname, is_counter, parent, config_bytes: config_bytes.to_vec() },
        config,
    )
}

#[cfg(test)]
mod tests {
    use super::{InlineChildToReconstruct, Registry, dehydrate, reconstruct_inline_children, reconstruct_one_child};
    use crate::mail::{Mail, PriorState};
    use crate::wasm::ctx::{NO_INBOUND_SOURCE, WasmDropCtx, WasmInitCtx};
    use crate::wasm::inline::{ChildRecord, bundle};
    use crate::wasm::{ActorInitError, ErasedWasmActor, WasmActor, WasmCtx};
    use crate::{Addressable, Anyone, Erased, Lifecycle, Unchecked};
    use aether_data::{Kind, KindId, MailboxId, wire};
    use alloc::boxed::Box;
    use alloc::rc::Rc;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cell::Cell;

    /// A child whose `on_dehydrate` counts its run and then either saves a
    /// fixed 4-byte tag, so the compose can be asserted to carry the child's
    /// bytes, or returns `refusal` having saved nothing. The reconstruct
    /// tests don't drive this type's dispatch.
    struct SavingChild {
        tag: u32,
        refusal: Option<&'static str>,
        runs: Rc<Cell<u32>>,
    }

    fn saving(tag: u32) -> SavingChild {
        SavingChild { tag, refusal: None, runs: Rc::default() }
    }

    impl ErasedWasmActor for SavingChild {
        fn erased_namespace(&self) -> &'static str {
            "test.inline.saving_child"
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _mail: Mail<'_>) -> u32 {
            0
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) {}
        fn erased_on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
            self.runs.set(self.runs.get() + 1);
            match self.refusal {
                Some(refusal) => Err(ActorInitError::from(refusal)),
                None => ctx.save_state(9, &self.tag.to_le_bytes()),
            }
        }
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>,
            _prior: PriorState<'_>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
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

    fn install_reconstructed(registry: &Registry, parent: MailboxId, child: &InlineChildToReconstruct<'_>) {
        registry.insert_child(
            child.alias,
            ChildRecord {
                type_tag: child.type_tag,
                full_subname: String::from(child.full_subname),
                is_counter: child.is_counter,
                parent: parent.0,
                config_bytes: child.config_bytes.to_vec(),
            },
            Box::new(saving(0)),
        );
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
            ChildRecord {
                type_tag: 0xAAAA,
                full_subname: String::from("a"),
                parent: root.0,
                config_bytes: vec![0x11, 0x22],
                ..ChildRecord::default()
            },
            Box::new(saving(0x1111_2222)),
        );
        registry.insert_child(
            id_b,
            ChildRecord {
                type_tag: 0xBBBB,
                full_subname: String::from("b"),
                is_counter: true,
                parent: id_a.0,
                ..ChildRecord::default()
            },
            Box::new(saving(0x3333_4444)),
        );

        // Parent saves a marker blob of its own.
        let (bundle, hooks) = dehydrate(&registry, |ctx| ctx.save_state(3, &[0xDE, 0xAD]));
        hooks.expect("no hook refused");
        let (version, bytes) = bundle.expect("a parent that saves plus two children yields a bundle");

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
    /// callback with its type tag + alias + state. The parent rehydrate
    /// runs once with the parent slice.
    #[test]
    fn reconstruct_offers_each_child_and_parent_slice() {
        // Build a composite with a parent blob + two children directly
        // through the bundle helpers. The callback only records, so the
        // registry threaded in is never inserted into here.
        use crate::wasm::inline::bundle::{ChildEntry, compose};

        const TAG_A: u64 = 0xBEEF;
        const TAG_B: u64 = 0xDEAD;

        let children = vec![
            ChildEntry {
                alias_id: 0xC1,
                type_tag: TAG_A,
                is_counter: false,
                full_subname: String::from("a"),
                version: 1,
                state_bytes: vec![1, 2, 3],
                config_bytes: Vec::new(),
                parent_id: None,
            },
            ChildEntry {
                alias_id: 0xC2,
                type_tag: TAG_B,
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
                Ok(())
            },
            |_registry, parent, child| {
                offered.push((child.type_tag, parent, child.state_bytes.to_vec()));
                Ok(())
            },
        )
        .expect("every child is rebuilt");

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

        reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| Ok(()),
            |registry, parent, child| {
                install_reconstructed(registry, parent, child);
                Ok(())
            },
        )
        .expect("the legacy child is rebuilt");

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
            |_, _| Ok(()),
            |registry, parent, child| {
                order.push(child.alias);
                install_reconstructed(registry, parent, child);
                Ok(())
            },
        )
        .expect("both children are rebuilt");

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

        reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| Ok(()),
            |registry, parent, child| {
                install_reconstructed(registry, parent, child);
                Ok(())
            },
        )
        .expect("every link is rebuilt");

        assert_eq!(registry.parent_of(branch), Some(root));
        assert_eq!(registry.parent_of(nested), Some(branch));
        assert_eq!(registry.parent_of(leaf), Some(nested));
    }

    /// A child the callback cannot rebuild ends the rebuild with the
    /// callback's error. Catches the failure logged and skipped, which lets
    /// a republish answer `Ok` with a child missing.
    #[test]
    fn a_child_the_callback_cannot_rebuild_fails_the_rebuild() {
        let root = MailboxId(0x4000);
        let rejected = MailboxId(0x4001);
        let descendant = MailboxId(0x4002);
        let sibling = MailboxId(0x4003);
        let children = vec![
            child_entry(rejected.0, 0xA001, Some(root.0)),
            child_entry(descendant.0, 0xA002, Some(rejected.0)),
            child_entry(sibling.0, 0xA003, Some(root.0)),
        ];
        let (version, bytes) = bundle::compose(0, &[], &children);
        let registry = Registry::new();
        registry.set_self_id(root.0);
        let mut offered = Vec::new();

        let error = reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| Ok(()),
            |_registry, _parent, child| {
                offered.push(child.alias);
                Err(ActorInitError::from("placement denied"))
            },
        )
        .expect_err("a child that cannot be rebuilt fails the rebuild");

        assert_eq!(error.message(), "placement denied");
        assert_eq!(offered, vec![rejected], "the first failure ends the rebuild before the sibling is offered");
        assert!(registry.take(descendant).is_none(), "a rejected parent's descendant is not rebuilt");
    }

    /// A parent cycle makes no progress, and the rebuild returns an error
    /// naming a stranded child without offering either. Catches the loop
    /// that never ends and the cycle skipped with a warning.
    #[test]
    fn a_parent_cycle_fails_the_rebuild() {
        let root = MailboxId(0x5000);
        let left = MailboxId(0x5001);
        let right = MailboxId(0x5002);
        let children = vec![child_entry(left.0, 0xA001, Some(right.0)), child_entry(right.0, 0xA002, Some(left.0))];
        let (version, bytes) = bundle::compose(0, &[], &children);
        let registry = Registry::new();
        registry.set_self_id(root.0);
        let mut offered = 0;

        let error = reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| Ok(()),
            |_registry, _parent, _child| {
                offered += 1;
                Ok(())
            },
        )
        .expect_err("a parent cycle fails the rebuild");

        assert_eq!(error.message(), "inline child `child` was not rebuilt: its recorded parent is absent");
        assert_eq!(offered, 0, "neither child of the cycle is offered");
    }

    /// A child whose recorded parent is absent fails the rebuild after the
    /// branches that can be rebuilt are, and is never re-parented to the
    /// root. Catches the orphan left absent with a warning.
    #[test]
    fn an_orphan_fails_the_rebuild_after_the_independent_branch() {
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

        let error = reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| Ok(()),
            |registry, parent, child| {
                install_reconstructed(registry, parent, child);
                Ok(())
            },
        )
        .expect_err("an orphan fails the rebuild");

        assert_eq!(error.message(), "inline child `child` was not rebuilt: its recorded parent is absent");
        assert!(registry.take(orphan).is_none(), "the orphan is never re-parented to the root");
        assert!(registry.take(valid_parent).is_some(), "the independent parent is rebuilt");
        assert!(registry.take(valid_child).is_some(), "the independent descendant is rebuilt after its parent");
    }

    /// The parent's `on_rehydrate` error ends the rebuild before any child is
    /// offered. Catches the parent's refusal dropped while its children are
    /// rebuilt under it.
    #[test]
    fn a_parent_that_refuses_its_state_fails_the_rebuild() {
        let root = MailboxId(0x6100);
        let (version, bytes) = bundle::compose(0, &[], &[child_entry(0x6101, 0xA001, Some(root.0))]);
        let registry = Registry::new();
        registry.set_self_id(root.0);
        let mut offered = 0;

        let error = reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |_, _| Err(ActorInitError::from("state refused")),
            |_registry, _parent, _child| {
                offered += 1;
                Ok(())
            },
        )
        .expect_err("the parent's refusal fails the rebuild");

        assert_eq!(error.message(), "state refused");
        assert_eq!(offered, 0, "no child is offered after the parent refused");
    }

    /// A dehydrate whose second child refuses stops there: the first child's
    /// state is in the bundle, the refusing child and the third are absent
    /// from it, the third child's hook did not run, and the error is the
    /// second child's. Catches the walk running on past a refusal, which
    /// moves state out of a guest that keeps running, and the refusing child
    /// packed with no state, which rebuilds it fresh over the resident one.
    #[test]
    fn dehydrate_stops_at_the_child_that_refuses() {
        let registry = Registry::new();
        let root = MailboxId(0x7100);
        let ids = [MailboxId(0x7101), MailboxId(0x7102), MailboxId(0x7103)];
        let third_runs = Rc::new(Cell::new(0));
        registry.set_self_id(root.0);
        let record = |full_subname: &str| ChildRecord {
            type_tag: 0xAAAA,
            full_subname: String::from(full_subname),
            parent: root.0,
            ..ChildRecord::default()
        };
        registry.insert_child(ids[0], record("first"), Box::new(saving(0x1111_2222)));
        registry.insert_child(
            ids[1],
            record("second"),
            Box::new(SavingChild { tag: 0, refusal: Some("not now"), runs: Rc::default() }),
        );
        registry.insert_child(
            ids[2],
            record("third"),
            Box::new(SavingChild { tag: 0, refusal: None, runs: Rc::clone(&third_runs) }),
        );

        let (bundle, hooks) = dehydrate(&registry, |_ctx| Ok(()));

        let error = hooks.expect_err("the second child refused");
        assert_eq!(error.message(), "inline child `second`: not now");
        assert_eq!(third_runs.get(), 0, "the walk stops at the refusal");
        let (version, bytes) = bundle.expect("the first child's state is still handed back");
        let packed = bundle::decompose(version, &bytes).children;
        assert_eq!(packed.len(), 1, "only the child that ran to the end is packed");
        assert_eq!(packed[0].alias_id, ids[0].0);
        assert_eq!(packed[0].state_bytes, 0x1111_2222u32.to_le_bytes().to_vec());
    }

    /// A typed (non-`()`) `Config` for step 5's reconstruct coverage: wraps
    /// a `u32`. Hand-rolls `Kind` rather than deriving it (a host-only test
    /// fixture needs no `Schema`/serde machinery) — `decode_with` only
    /// succeeds on exactly 4 bytes, so it refuses empty bytes just like a
    /// real typed config would, the branch `reconstruct_one_child` must
    /// still honor.
    #[derive(Clone, Copy, Default)]
    struct TypedConfig(u32);

    impl Kind for TypedConfig {
        const NAME: &'static str = "test.inline.typed_config";
        const ID: KindId = KindId(0x7A11_0000_0000_0001);

        fn decode_with(bytes: &[u8], _ctx: &mut wire::DecodeCtx<'_>) -> Result<Self, wire::Error> {
            wire::decode_from_slice(bytes).map(Self)
        }

        fn encode_into_bytes(&self) -> Vec<u8> {
            self.0.to_le_bytes().to_vec()
        }
    }

    impl aether_data::CrossesActors for TypedConfig {}
    impl aether_data::CrossesWire for TypedConfig {}

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
        type Ctx<'a> = WasmCtx<'a, Self>;

        fn init(config: TypedConfig, _params: (), _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
            Ok(Self { observed: config.0 })
        }
    }

    impl crate::Declared for TypedConfigChild {
        type Depends = ();
        type Spawns = ();
        type Parents = ();
    }
    impl WasmActor for TypedConfigChild {
        type State = Self;
        type Persist = ();
    }

    impl crate::WasmDispatch<Self> for TypedConfigChild {
        fn dispatch(state: &mut Self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _mail: Mail<'_>) -> u32 {
            state.observed
        }
    }

    impl ErasedWasmActor for TypedConfigChild {
        fn erased_namespace(&self) -> &'static str {
            Self::NAMESPACE
        }
        fn erased_dispatch(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _mail: Mail<'_>) -> u32 {
            self.observed
        }
        fn erased_wire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_unwire(&mut self, _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>) {}
        fn erased_on_dehydrate(&mut self, _ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
            Ok(())
        }
        fn erased_on_rehydrate(
            &mut self,
            _ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>,
            _prior: PriorState<'_>,
        ) -> Result<(), ActorInitError> {
            Ok(())
        }
    }

    /// Step 5 coverage (the branch this issue fixes): `reconstruct_one_child`
    /// over a `ChildEntry` carrying a typed-config child's real encoded
    /// config bytes re-`init`s it with that config — not the empty-bytes
    /// re-init that dropped every typed-config child before this fix. The
    /// reconstructed child is registered and its
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

        reconstruct_one_child::<TypedConfigChild>(&registry, &to_reconstruct)
            .expect("a typed-config child with real config bytes must reconstruct");

        let mut child = registry.take(alias).expect("the reconstructed child is registered under its alias");
        let code = child.erased_dispatch(
            &mut WasmCtx::__new(alias.0, &registry, NO_INBOUND_SOURCE),
            // SAFETY: a zero-length mail frame — ptr 0 with len 0 spans no
            // memory, and the probe child's dispatch reads no payload.
            unsafe { Mail::__from_ptr(0, 1, 0, 1, crate::NO_REPLY_HANDLE, alias.0) },
        );
        assert_eq!(code, 0xDEAD_BEEF, "the child's init decoded the real config value, not a default");
    }

    /// An empty-bytes entry for a typed-config child is a genuinely
    /// undecodable blob (`TypedConfig::decode_from_bytes` requires exactly 4
    /// bytes), distinct from the `()`-config case, where empty bytes are the
    /// *correct* encoding. The rebuild returns an error naming the child and
    /// registers nothing. Catches the child skipped with a warning.
    #[test]
    fn reconstruct_one_child_refuses_typed_config_with_undecodable_bytes() {
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

        let error = reconstruct_one_child::<TypedConfigChild>(&registry, &to_reconstruct)
            .expect_err("empty bytes don't decode as a typed (non-unit) Config");

        let named = error.message().starts_with("inline child `typed` was not rebuilt: its config no longer decodes");
        assert!(named, "the error names the child and the cause: {error}");
        assert!(registry.take(alias).is_none(), "a child that was not rebuilt is never registered");
    }
}
