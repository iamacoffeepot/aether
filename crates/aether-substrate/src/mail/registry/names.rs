use aether_data::tagged_id::{Tag, with_tag};
use aether_data::{ActorId, MailboxCategory, fold_lineage};

use crate::mail::MailboxId;

/// The id a depth-1 registration takes from its canonical `name` — the fixed
/// point of the ADR-0099 lineage fold (§3).
///
/// The registry *assigns* this id, which is why it is the one name→id
/// derivation with no typed surface above it to resolve through:
/// `aether_actor::root_mailbox::<C>()` answers for a root cap and
/// `ctx.actor_ref::<C>()` for a declared sibling, but both return the value
/// this function defines. Every other name→id site in the crate, production and test alike,
/// calls here rather than re-deriving the hash beside it.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: the registry assigns the depth-1 id, so its own derivation has nothing typed to resolve through; single gated definition every other name-to-id site in the crate now routes through
pub fn canonical_mailbox_id(name: &str) -> MailboxId {
    MailboxId::from_name(name)
}

/// The position a `/`-rendered canonical lineage path names: the ADR-0099 §4
/// parse → fold, the inverse of the render. Each segment is one node, a bare
/// namespace a singleton and `namespace:discriminator` an instance, folded
/// root to leaf. A one-segment path is the depth-1 fixed point, equal to
/// [`canonical_mailbox_id`] for the same name.
///
/// Crate-private: the registry's name lookup is the one place a written
/// address becomes a position (ADR-0230 §3), and this is the fold it keys
/// with. It is the last step of [`lineage_prefixes`], so a path and its
/// ancestors are folded by one piece of code.
pub fn lineage_mailbox_id(path: &str) -> MailboxId {
    // `split` yields at least one segment, even for an empty path, so the
    // fallback is never taken.
    lineage_prefixes(path).last().map_or(MailboxId(with_tag(Tag::Mailbox, 0)), |(_, id)| id)
}

/// Every prefix of a `/`-rendered canonical lineage path that ends on a
/// segment boundary, root first, each with the position it names: the path's
/// ancestors in order, then the path itself (ADR-0099 §4).
///
/// The fold carries the untagged hash from one segment to the next and tags
/// a copy for each prefix, so the id of a prefix is the id that prefix has as
/// a path of its own, at any depth.
///
/// Consumers: [`lineage_mailbox_id`], the registry's birth check that a
/// nested name's parent holds a record, and `Registry::lineage_order`, which
/// reads one birth serial per prefix (ADR-0248 §5).
pub(super) fn lineage_prefixes(path: &str) -> impl Iterator<Item = (&str, MailboxId)> {
    path.split('/').scan((0, None), move |(end, carry), segment| {
        let node = match segment.split_once(':') {
            Some((namespace, discriminator)) => ActorId::instanced(namespace, discriminator),
            None => ActorId::singleton(segment),
        };
        let folded = carry.map_or(node.0, |parent| fold_lineage(parent, node));
        // A separator precedes every segment but the first.
        *end += segment.len() + usize::from(carry.is_some());
        *carry = Some(folded);

        Some((&path[..*end], MailboxId(with_tag(Tag::Mailbox, folded))))
    })
}

/// Categorise a native mailbox name for the inventory snapshot (issue 730).
/// Pure function of the name string. The hub uses this categorisation
/// (round-tripped through `MailboxDescriptor.category`) to render
/// type-prefixed labels in trace tool output. A guest's route is categorised
/// from the publication table before this runs (ADR-0241 §3), because a
/// guest is named by its own namespace and its name says nothing of its host.
pub(super) fn categorise_mailbox_name(name: &str) -> Option<MailboxCategory> {
    if name == "aether.chassis" {
        // Reachable via [`MailboxId::CHASSIS_MAILBOX_ID`] short-circuit;
        // never registered with a real handler. The synthetic entry in
        // [`Registry::list_mailbox_descriptors`] uses the same
        // categorisation so re-registration would be redundant.
        Some(MailboxCategory::ChassisSentinel)
    } else if name.starts_with("aether.") {
        // Chassis caps and substrate-owned actors live under the
        // `aether.` namespace (post-ADR-0074). Anything else is
        // user-space and falls through to `None`.
        Some(MailboxCategory::Actor)
    } else {
        None
    }
}
