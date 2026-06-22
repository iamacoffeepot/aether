//! Runtime reverse-lookup helper for `aether.inventory`.
//!
//! Projects a list of tagged-id strings onto [`ResolvedName`] via the
//! runtime-registry arm of the ADR-0088 §2 chain
//! (`thread_name::resolve_runtime`). The `#[actor] impl` in `mod.rs`
//! delegates here.

use aether_data::tagged_id;
use aether_kinds::ResolvedName;
use aether_substrate::runtime::thread_name::resolve_runtime;

/// Resolve each tagged-id string to its origin name via the
/// runtime-registry arm of the reverse-lookup chain. Returns one
/// [`ResolvedName`] per input, in request order with the `id` echoed
/// for correlation. `name` is `Some` for a dynamically-minted instance
/// the substrate has registered; `None` on a miss or a malformed id
/// (a malformed id does not abort its siblings).
pub fn resolve_ids(ids: Vec<String>) -> Vec<ResolvedName> {
    ids.into_iter()
        .map(|id| {
            // A malformed tagged-id string reports `None` rather than
            // aborting the batch — one bad id doesn't sink its siblings.
            let name = tagged_id::decode(&id).ok().and_then(resolve_runtime);
            ResolvedName { id, name }
        })
        .collect()
}
