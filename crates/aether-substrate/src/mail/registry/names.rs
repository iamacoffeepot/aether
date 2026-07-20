use aether_data::MailboxCategory;

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
