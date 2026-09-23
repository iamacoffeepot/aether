//! Contract refusal on replace (ADR-0231 §5): a replacement whose hosted type
//! drops or changes a handler row of the predecessor's hosted type is refused
//! before the old instance is touched. Added rows are allowed; config, docs,
//! assets and fallback presence are not compared.
//!
//! Carried-context refusal (ADR-0139 §4, #6429): a replacement whose module
//! does not declare the kind of a request context the old instance carries is
//! refused after the old instance's hooks ran, and the old instance is
//! reinstalled.

use std::collections::HashSet;
use std::fmt::Display;

use aether_kinds::ComponentCapabilities;
use aether_substrate::mail::KindId;

/// The first predecessor handler row the replacement drops or changes, or
/// `None` when the replacement keeps every row.
pub(super) fn contract_break(
    predecessor: &ComponentCapabilities,
    replacement: &ComponentCapabilities,
) -> Option<KindId> {
    aether_data::first_contract_break(
        predecessor.handlers.iter().map(|h| (h.id, h.reply)),
        replacement.handlers.iter().map(|h| (h.id, h.reply)),
    )
}

/// The refusal error naming the actor, by the replace request's own target,
/// and the first kind whose contract the replacement changes. One
/// constructor, so every refusal reads the same.
pub(super) fn contract_refusal(actor: &impl Display, kind: &str) -> String {
    format!("{actor} replacement changes its contract for {kind}")
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
