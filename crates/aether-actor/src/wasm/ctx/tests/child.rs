//! Typed cluster-child resolution: the recorded-[`ActorTypeTag`] check that
//! decides whether a positional lookup is admitted as an [`InlineChild<C>`].

use super::{
    ActorTypeTag, LifecycleProbe, NO_INBOUND_SOURCE, NestingParent, Registry, SucceedingChild, WasmCtx,
    install_inline_child,
};
use crate::model::ctx::Manual;
use aether_data::MailboxId;
use alloc::string::String;
use alloc::vec::Vec;

/// `child_as::<C>` admits the resident child only when the registry recorded
/// it as a `C`. The wrong-type arm is the one that matters: without the tag
/// check the same subname would hand back a live handle whose `send` bound is
/// checked against a type the child is not, which is the whole point of the
/// typed handle. Owned logic: the tag equality in `WasmCtx::typed_child`.
#[test]
fn child_as_admits_only_the_recorded_child_type() {
    let registry = Registry::new();
    registry.set_self_id(0xA000);
    let parent = MailboxId(0xA000);
    let child = MailboxId(0xA001);
    install_inline_child::<SucceedingChild>(
        &registry,
        child,
        ActorTypeTag::of::<SucceedingChild>().0,
        String::from("slot"),
        false,
        parent.0,
        Vec::new(),
        (),
    )
    .expect("the child installs");

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(parent.0, &registry, NO_INBOUND_SOURCE);

    let matched = ctx.child_as::<SucceedingChild>("slot").expect("the recorded type resolves");
    assert_eq!(matched.id(), child, "the typed handle carries the resolved alias");
    assert!(matched.matches(child), "the handle recognizes its own alias as the inbound source");
    assert!(ctx.child_as::<LifecycleProbe>("slot").is_none(), "a different actor type must not resolve");
    assert!(ctx.child_as::<SucceedingChild>("absent").is_none(), "an unresolved subname stays None");
}

/// `sibling_as::<C>` walks the asking child's recorded parent before applying
/// the same type check, so a sibling lookup resolves under the parent rather
/// than under the asker. Owned logic: the parent hop composed with the tag
/// check — a `child_as`-shaped implementation would resolve nothing here.
#[test]
fn sibling_as_resolves_through_the_recorded_parent() {
    let registry = Registry::new();
    registry.set_self_id(0xB000);
    let parent = MailboxId(0xB000);
    for (alias, subname) in [(MailboxId(0xB001), "a"), (MailboxId(0xB002), "b")] {
        install_inline_child::<SucceedingChild>(
            &registry,
            alias,
            ActorTypeTag::of::<SucceedingChild>().0,
            String::from(subname),
            false,
            parent.0,
            Vec::new(),
            (),
        )
        .expect("the child installs");
    }

    let ctx: WasmCtx<'_, Manual> = WasmCtx::__new(0xB001, &registry, NO_INBOUND_SOURCE);

    assert_eq!(
        ctx.sibling_as::<SucceedingChild>("b").map(|sibling| sibling.id()),
        Some(MailboxId(0xB002)),
        "the sibling resolves as a peer under the asker's recorded parent",
    );
    assert!(ctx.sibling_as::<NestingParent>("b").is_none(), "a different actor type must not resolve");
}
