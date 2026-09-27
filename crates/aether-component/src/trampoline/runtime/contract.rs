//! Contract refusal on replace (ADR-0231 §5): a replacement whose hosted type
//! drops or changes a handler row of the predecessor's hosted type, or drops
//! its `#[fallback]`, is refused before the old instance is touched. Added
//! rows and an added fallback are allowed; config, docs and assets are not
//! compared. The rule is the one the registry holds a republished route
//! contract to, [`RouteContract::first_break`], so a replace the trampoline
//! lets through is one whose republish the registry accepts.
//!
//! Carried-context refusal (ADR-0139 §4, #6429): a replacement whose module
//! does not declare the kind of a request context the old instance carries is
//! refused after the old instance's hooks ran, and the old instance is
//! reinstalled.
//!
//! Inline-child contracts (ADR-0231 §4): the contract each actor type a module
//! can spawn inline publishes on its alias, keyed by actor-type tag.

use std::collections::HashSet;
use std::fmt::Display;

use aether_data::ActorId;
use aether_kinds::ComponentCapabilities;
use aether_substrate::actor::wasm::module::ModuleManifest;
use aether_substrate::mail::KindId;
use aether_substrate::mail::registry::{ContractBreak, RouteContract};

/// The first break the replacement makes in the predecessor's contract, or
/// `None` when it keeps every row and, if the predecessor has one, the
/// fallback.
pub(super) fn contract_break(
    predecessor: &ComponentCapabilities,
    replacement: &ComponentCapabilities,
) -> Option<ContractBreak> {
    RouteContract::from_capabilities(predecessor).first_break(&RouteContract::from_capabilities(replacement))
}

/// The refusal error naming the actor, by the replace request's own target,
/// and the break: the first kind whose contract the replacement changes, by
/// `kind_label`, or its dropped fallback. One constructor, so every refusal
/// reads the same.
pub(super) fn contract_refusal(
    actor: &impl Display,
    contract_break: ContractBreak,
    kind_label: impl FnOnce(KindId) -> String,
) -> String {
    match contract_break {
        ContractBreak::Row(kind) => format!("{actor} replacement changes its contract for {}", kind_label(kind)),
        ContractBreak::Fallback => format!("{actor} replacement drops its fallback"),
    }
}

/// The contract every actor type a module can spawn inline publishes, keyed
/// by its actor-type tag (`ActorId::singleton(NAMESPACE)`): the exported
/// groups, then the private children of `aether.kinds.inputs.private`. The
/// implicit group of a single-actor module takes the module's namespace.
pub(super) fn inline_contracts(manifest: &ModuleManifest) -> Vec<(u64, RouteContract)> {
    manifest
        .actors()
        .iter()
        .chain(manifest.private_actors())
        .filter_map(|group| {
            group.namespace.as_deref().or_else(|| manifest.namespace()).map(|namespace| {
                (ActorId::singleton(namespace).0, RouteContract::from_capabilities(&group.capabilities))
            })
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
    format!("{actor} replacement does not declare its carried request context {kind}")
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
