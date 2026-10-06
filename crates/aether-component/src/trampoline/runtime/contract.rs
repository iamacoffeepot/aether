//! Carried-context refusal (ADR-0139 §4, #6429): a replacement whose module
//! does not declare the kind of a context the old instance carries, a
//! request's or a watch's (ADR-0079 §8), is refused after the old instance's
//! hooks ran, and the old instance is reinstalled.
//!
//! Inline-child types (ADR-0231 §4): the namespace and contract each actor
//! type a module can spawn inline publishes on its alias, and the
//! dependencies its spawn checks (ADR-0230), keyed by actor-type tag.

use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;

use aether_data::ActorId;
use aether_substrate::actor::wasm::component::InlineChildType;
use aether_substrate::actor::wasm::module::ModuleManifest;
use aether_substrate::mail::KindId;
use aether_substrate::mail::registry::RouteContract;

/// The type every actor a module can spawn inline publishes on its alias,
/// with the dependencies its spawn checks, keyed by its actor-type tag
/// (`ActorId::singleton(NAMESPACE)`): the exported groups, then the private
/// children of `aether.kinds.inputs.private`, each under the namespace the
/// manifest resolves for it.
pub(super) fn inline_children(manifest: &ModuleManifest) -> Vec<(u64, InlineChildType)> {
    manifest
        .exported_groups()
        .chain(manifest.private_groups())
        .map(|(namespace, group)| {
            let namespace: Arc<str> = namespace.into();
            let contract = RouteContract::from_capabilities(&group.capabilities);
            let dependencies = group.dependencies.as_slice().into();
            (ActorId::singleton(&namespace).0, InlineChildType { namespace, contract, dependencies })
        })
        .collect()
}

/// The first carried context kind the predecessor module declares and the
/// replacement module does not, or `None` when the replacement can take every
/// carried context. A kind neither module declares (one defined in a shared
/// kinds crate that no `#[actor]` retained) cannot be judged and passes.
pub(super) fn undeclared_context(
    carried: impl IntoIterator<Item = KindId>,
    predecessor: &HashSet<KindId>,
    replacement: &HashSet<KindId>,
) -> Option<KindId> {
    carried.into_iter().find(|kind| predecessor.contains(kind) && !replacement.contains(kind))
}

/// The refusal error naming the actor, by the replace request's own target,
/// and the carried context kind the replacement does not declare.
pub(super) fn context_refusal(actor: &impl Display, kind: &str) -> String {
    format!("{actor} replacement does not declare its carried context {kind}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use aether_substrate::mail::KindId;

    use super::undeclared_context;

    #[test]
    fn a_context_kind_only_the_replacement_lacks_is_refused() {
        let (kept, reshaped, shared) = (KindId(1), KindId(2), KindId(3));
        let predecessor = HashSet::from([kept, reshaped]);
        let replacement = HashSet::from([kept]);

        assert_eq!(undeclared_context([kept, shared], &predecessor, &replacement), None);
        assert_eq!(undeclared_context([kept, reshaped, shared], &predecessor, &replacement), Some(reshaped));
    }
}
