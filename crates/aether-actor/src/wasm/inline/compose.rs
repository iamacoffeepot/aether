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
use crate::wasm::ctx::{CapturedState, NO_INBOUND_SOURCE, WasmDropCtx, WasmInitCtx};
use crate::wasm::inline::Registry;
use crate::wasm::inline::bundle::{self, ChildEntry};
use crate::wasm::{ErasedWasmActor, WasmActor, WasmCtx};

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
}

/// Decompose a migration bundle, run the parent's `on_rehydrate` with its
/// slice, then reconstruct every inline child (ADR-0114 §5).
///
/// `run_parent_rehydrate` runs the freshly-`init`ed parent instance's
/// `on_rehydrate` with the parent's saved `(version, bytes)` rebuilt as a
/// [`PriorState`]. `registry` is the component's inline-child
/// registry (the `export!`-emitted `static __AETHER_INLINE`), forwarded to
/// each `reconstruct_child` call. `reconstruct_child` is the codegen
/// callback that re-`init`s one child by type tag, restores its state, and
/// re-registers it in that registry; it returns `false` for an unknown tag
/// (logged + skipped by the callback).
///
/// For a childless bundle the decompose yields the raw parent
/// `(version, bytes)` and no children, so the parent's `on_rehydrate`
/// sees the identical slice it would have today.
pub fn reconstruct_inline_children(
    version: u32,
    bytes: &[u8],
    registry: &Registry,
    run_parent_rehydrate: impl FnOnce(u32, &[u8]),
    mut reconstruct_child: impl FnMut(&Registry, &InlineChildToReconstruct<'_>) -> bool,
) {
    let decomposed = bundle::decompose(version, bytes);

    run_parent_rehydrate(decomposed.parent.version, &decomposed.parent.bytes);

    for entry in &decomposed.children {
        let to_reconstruct = InlineChildToReconstruct {
            alias: MailboxId(entry.alias_id),
            type_tag: entry.type_tag,
            is_counter: entry.is_counter,
            full_subname: &entry.full_subname,
            state_version: entry.version,
            state_bytes: &entry.state_bytes,
        };
        if !reconstruct_child(registry, &to_reconstruct) {
            // An unknown type tag (a replace that dropped a child type) or
            // a failed re-`init`: skip it. The codegen callback has already
            // logged; nothing else to do for this entry.
            tracing::warn!(
                target = "aether_actor::inline",
                alias = to_reconstruct.alias.0,
                type_tag = to_reconstruct.type_tag,
                "inline child not reconstructed across replace_component (unknown type tag \
                 or re-init failure); skipping",
            );
        }
    }
}

/// Re-`init` one inline child of concrete type `A`, restore its
/// `type State`, and re-register it under `alias` in `registry` (ADR-0114
/// §5). Called by the `export!`-generated reconstruct callback once it has
/// matched the child's type tag to one of the module's exported types.
///
/// Returns `false` (and does not register) when `A::Config` cannot decode
/// from empty bytes (a typed-config inline child — its config isn't
/// persisted across replace) or `A::init` returns `Err`; the caller logs
/// and skips. The substrate alias route under `alias` survived the swap
/// (ADR-0022; the parent slot is stable), so re-keying the guest registry
/// by `alias` restores addressing with no host round-trip.
#[must_use]
pub fn reconstruct_one_child<A>(
    registry: &Registry,
    to_reconstruct: &InlineChildToReconstruct<'_>,
) -> bool
where
    A: WasmActor + ErasedWasmActor,
    // iamacoffeepot/aether#2311: `A::init` returns the runtime state, boxed as
    // the erased child. For an un-split component `State = Self`, so the
    // identity's `ErasedWasmActor` impl satisfies this.
    <A as WasmActor>::State: ErasedWasmActor,
{
    // The child's config isn't part of the migration bundle; re-`init`
    // from empty config bytes, the same shape the legacy zero-config
    // `init` shim uses. A typed-config child decodes `None` here and is
    // skipped.
    let Some(config) = <A::Config as Kind>::decode_from_bytes(&[]) else {
        return false;
    };
    let mut init_ctx = WasmInitCtx::__new(to_reconstruct.alias.0);
    let Ok(mut child) = A::init(config, &mut init_ctx) else {
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

    // The flat-alias model folds every inline child on the instance carry,
    // so a reconstructed child's logical parent is the cluster root (the
    // instance's real `self_id`). The dehydrate bundle does not persist the
    // parent link; it is re-derived here from the live registry. Per-parent
    // nesting (the address-tree = slot-tree fold) is a follow-up.
    registry.insert_child(
        to_reconstruct.alias,
        to_reconstruct.type_tag,
        String::from(to_reconstruct.full_subname),
        to_reconstruct.is_counter,
        registry.self_id(),
        Box::new(child),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::{Registry, dehydrate, reconstruct_inline_children};
    use crate::Manual;
    use crate::mail::{Mail, PriorState};
    use crate::wasm::ctx::WasmDropCtx;
    use crate::wasm::inline::bundle;
    use crate::wasm::{ErasedWasmActor, WasmCtx};
    use aether_data::MailboxId;
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

    /// Step 3 coverage: a parent with two inline children yields a
    /// composite carrying both child entries plus the parent's own state,
    /// composed through one logical `save_state`.
    #[test]
    fn compose_dehydrate_packs_parent_and_children() {
        // Two children with distinct tags + type tags + aliases, in a
        // test-local registry (no shared-global aliasing across tests).
        let registry = Registry::new();
        let id_a = MailboxId(0xA1);
        let id_b = MailboxId(0xB2);
        registry.insert_child(
            id_a,
            0xAAAA,
            String::from("a"),
            false,
            0,
            Box::new(SavingChild { tag: 0x1111_2222 }),
        );
        registry.insert_child(
            id_b,
            0xBBBB,
            String::from("b"),
            true,
            0,
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
        assert_eq!(
            decomposed.children.len(),
            2,
            "exactly the two inserted children are packed",
        );
        let a = decomposed
            .children
            .iter()
            .find(|c| c.alias_id == id_a.0)
            .expect("child a present");
        assert_eq!(a.type_tag, 0xAAAA);
        assert_eq!(a.state_bytes, 0x1111_2222u32.to_le_bytes().to_vec());
        let b = decomposed
            .children
            .iter()
            .find(|c| c.alias_id == id_b.0)
            .expect("child b present");
        assert!(b.is_counter, "child b's counter flag is carried");
        assert_eq!(b.state_bytes, 0x3333_4444u32.to_le_bytes().to_vec());
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
            },
            ChildEntry {
                alias_id: 0xC2,
                type_tag: TAG_UNKNOWN,
                is_counter: false,
                full_subname: String::from("b"),
                version: 2,
                state_bytes: vec![4, 5],
            },
        ];
        let (version, bytes) = compose(5, &[7, 7], &children);

        let registry = Registry::new();
        let mut parent_runs = 0u32;
        let mut offered: Vec<(u64, Vec<u8>)> = Vec::new();
        reconstruct_inline_children(
            version,
            &bytes,
            &registry,
            |pv, pb| {
                assert_eq!(pv, 5, "parent version slice is carried");
                assert_eq!(pb, &[7, 7], "parent bytes slice is carried");
                parent_runs += 1;
            },
            |_registry, child| {
                offered.push((child.type_tag, child.state_bytes.to_vec()));
                // An unknown tag is offered but the callback skips it.
                child.type_tag != TAG_UNKNOWN
            },
        );

        assert_eq!(parent_runs, 1, "the parent rehydrate runs exactly once");
        assert_eq!(
            offered.len(),
            2,
            "both children are offered to the callback"
        );
        assert_eq!(offered[0].1, vec![1, 2, 3], "child a state is carried");
        assert_eq!(offered[1].1, vec![4, 5], "child b state is carried");
    }
}
