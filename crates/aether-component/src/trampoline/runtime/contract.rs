//! Contract refusal on replace (ADR-0231 §5): a replacement whose hosted type
//! drops or changes a handler row of the predecessor's hosted type is refused
//! before the old instance is touched. Added rows are allowed; config, docs,
//! assets and fallback presence are not compared.

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
