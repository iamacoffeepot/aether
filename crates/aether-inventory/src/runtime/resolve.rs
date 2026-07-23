//! Runtime reverse-lookup helper for `aether.inventory`.
//!
//! Projects a list of tagged-id strings onto [`ResolvedName`], dispatching
//! each id to the table that owns its family (ADR-0064 tags, ADR-0088 §2):
//! a thread id reverses through the process-global name registry
//! (`thread_name::resolve_runtime`), a mailbox or kind id through the
//! engine's own [`Registry`]. The `#[runtime] impl` in `runtime/mod.rs`
//! delegates here.

use crate::kinds::ResolvedName;
use aether_data::tagged_id::{self, Tag};
use aether_data::{KindId, MailboxId};
use aether_substrate::mail::registry::Registry;
use aether_substrate::runtime::thread_name::resolve_runtime;

/// Resolve each tagged-id string to its origin name. Returns one
/// [`ResolvedName`] per input, in request order with the `id` echoed for
/// correlation. `name` is `None` on a miss or a malformed id (a malformed
/// id does not abort its siblings).
///
/// The ADR-0064 tag picks the table, so each id is looked up in exactly
/// the one that can hold it:
///
/// - `thr-…` — the process-global runtime registry, populated when a
///   thread name is minted (`thread_name::current_thread_id`).
/// - `mbx-…` / `knd-…` — `registry`, the engine's own live tables. This
///   is what names a runtime-registered mailbox the link-time manifest
///   cannot carry: a component loaded at
///   `aether.component/aether.embedded:NAME` is registered here by the
///   component host, so it reverses to its lineage address instead of
///   falling back to a hex tag in a trace tree.
/// - anything else — `None`; the caller renders the tagged-id string.
pub fn resolve_ids(registry: &Registry, ids: Vec<String>) -> Vec<ResolvedName> {
    ids.into_iter()
        .map(|id| {
            // A malformed tagged-id string reports `None` rather than
            // aborting the batch — one bad id doesn't sink its siblings.
            let name = tagged_id::decode(&id).ok().and_then(|raw| match tagged_id::tag_of(raw) {
                Some(Tag::Thread) => resolve_runtime(raw),
                Some(Tag::Mailbox) => registry.mailbox_name(MailboxId(raw)),
                Some(Tag::Kind) => registry.kind_name(KindId(raw)),
                _ => None,
            });
            ResolvedName { id, name }
        })
        .collect()
}
