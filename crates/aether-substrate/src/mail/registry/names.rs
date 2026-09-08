use aether_data::MailboxCategory;

use crate::mail::MailboxId;

/// The id a depth-1 registration takes from its canonical `name` — the fixed
/// point of the ADR-0099 lineage fold (§3).
///
/// The registry *assigns* this id, which is why it is the one name→id
/// derivation with no typed surface above it to resolve through:
/// `aether_actor::root_mailbox::<C>()` answers for a root cap and
/// `ctx.actor::<C>()` for a sibling, but both return the value this function
/// defines. Every other name→id site in the crate, production and test alike,
/// calls here rather than re-deriving the hash beside it.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: the registry assigns the depth-1 id, so its own derivation has nothing typed to resolve through; single gated definition every other name-to-id site in the crate now routes through
pub fn canonical_mailbox_id(name: &str) -> MailboxId {
    MailboxId::from_name(name)
}

/// Categorise a mailbox name for the inventory snapshot (issue 730).
/// Pure function of the name string. The hub uses this categorisation
/// (round-tripped through `MailboxDescriptor.category`) to render
/// type-prefixed labels in trace tool output.
pub(super) fn categorise_mailbox_name(name: &str) -> Option<MailboxCategory> {
    if name == "aether.chassis" {
        // Reachable via [`MailboxId::CHASSIS_MAILBOX_ID`] short-circuit;
        // never registered with a real handler. The synthetic entry in
        // [`Registry::list_mailbox_descriptors`] uses the same
        // categorisation so re-registration would be redundant.
        Some(MailboxCategory::ChassisSentinel)
    // Literal kept in sync with `aether_component::trampoline::WasmTrampoline::NAMESPACE`
    // (issue 654 made that the single source of truth). Substrate can't
    // import from capabilities (wrong dep direction), so this routing
    // categorisation duplicates the prefix; if it drifts, every
    // loaded-component test fails immediately because the mailbox
    // categorisation no longer matches.
    //
    // ADR-0099 §4: the name is now the `/`-rendered lineage
    // (`aether.component/aether.embedded:NAME`, and one more
    // `/...trampoline:CHILD` segment per nested sibling spawn), so the
    // trampoline node is the *leaf* segment rather than the whole-string
    // prefix — match on the last `/`-segment.
    } else if name.rsplit('/').next().is_some_and(|leaf| leaf.starts_with("aether.embedded:")) {
        Some(MailboxCategory::Trampoline)
    } else if name.starts_with("aether.") {
        // Chassis caps and substrate-owned actors live under the
        // `aether.` namespace (post-ADR-0074). Anything else is
        // user-space and falls through to `None`.
        Some(MailboxCategory::Actor)
    } else {
        None
    }
}
