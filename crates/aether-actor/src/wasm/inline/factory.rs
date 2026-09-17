//! Linked constructors for actors that may be restored as inline children.
//! A child need not be a public module export to survive replacement.

use aether_data::MailboxId;
#[cfg(any(test, target_family = "wasm"))]
use aether_data::mailbox_id_from_name;

use super::Registry;
use super::compose::InlineChildToReconstruct;
use crate::wasm::WasmPlacementFacts;
#[cfg(target_family = "wasm")]
use crate::wasm::{__validate_inline_child_placement, ActorTypeTag};

/// The `#[actor]` macro submits one factory for each concrete WASM actor.
#[doc(hidden)]
pub struct ReconstructionFactory {
    pub namespace: &'static str,
    pub placement: WasmPlacementFacts,
    pub reconstruct: fn(&Registry, MailboxId, &InlineChildToReconstruct<'_>) -> bool,
}

#[cfg(target_family = "wasm")]
inventory::collect!(ReconstructionFactory);

#[cfg(any(test, target_family = "wasm"))]
fn unique_factory<'a>(
    type_tag: u64,
    factories: impl Iterator<Item = &'a ReconstructionFactory>,
) -> Option<&'a ReconstructionFactory> {
    let mut matching = factories.filter(|factory| mailbox_id_from_name(factory.namespace).0 == type_tag);
    let factory = matching.next()?;
    matching.next().is_none().then_some(factory)
}

/// Reconstruct an unexported child by its saved actor tag. Duplicate tags
/// are refused rather than choosing a link-order-dependent constructor.
#[must_use]
#[cfg(target_family = "wasm")]
pub fn reconstruct_registered_child(
    registry: &Registry,
    parent: MailboxId,
    child: &InlineChildToReconstruct<'_>,
) -> bool {
    let Some(factory) = unique_factory(child.type_tag, inventory::iter::<ReconstructionFactory>.into_iter()) else {
        return false;
    };
    __validate_inline_child_placement(registry, parent.0, ActorTypeTag(child.type_tag), factory.placement).is_ok()
        && (factory.reconstruct)(registry, parent, child)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reconstruct(_: &Registry, _: MailboxId, _: &InlineChildToReconstruct<'_>) -> bool {
        true
    }

    #[test]
    fn lookup_rejects_ambiguous_type_tags() {
        let placement = WasmPlacementFacts { is_instanced: true, module_child: false, exact_parent_tags: &[] };
        let first = ReconstructionFactory { namespace: "test.private.child", placement, reconstruct };
        let duplicate = ReconstructionFactory { namespace: "test.private.child", placement, reconstruct };
        let tag = mailbox_id_from_name(first.namespace).0;

        assert!(unique_factory(tag, [&first].into_iter()).is_some());
        assert!(unique_factory(tag, [&first, &duplicate].into_iter()).is_none());
        assert!(unique_factory(tag.wrapping_add(1), [&first].into_iter()).is_none());
    }
}
