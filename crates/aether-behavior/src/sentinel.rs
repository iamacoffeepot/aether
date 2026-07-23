//! Reserved lifecycle sentinel kind ids (ADR-0137).
//!
//! The guest ABI is fixed at four exports; lifecycle hooks ride *inside*
//! `filter` rather than adding exports. The host invokes a hook by calling
//! `filter(sentinel, 0, 0)`, and the `#[behavior]` dispatch table routes
//! the sentinel to the script's `on_attach` / `on_detach` / `on_frame`.
//!
//! Real kind ids carry `Tag::Kind` in their high four bits (they are
//! `with_tag`-prefixed FNV hashes), so this tiny low-valued block cannot
//! collide with any authored kind. Keeping `#[on_frame]` on an SDK-owned
//! sentinel — rather than the widget `Collect` kind its natural spelling
//! implies — is what frees the SDK of any `aether-kit-widget` dependency.

use aether_data::KindId;

/// Dispatched post-restore with ctx available after mirror priming has been
/// requested; replay traffic fills mirrors asynchronously afterward.
pub const ATTACH: KindId = KindId(1);

/// Dispatched best-effort as the script leaves its position.
pub const DETACH: KindId = KindId(2);

/// Dispatched once per frame for per-frame work.
pub const FRAME: KindId = KindId(3);

/// Marker kind on a `report()` replay-request effect. Not a lifecycle
/// hook — it rides an [`crate::envelope::Effect`] whose empty body asks the
/// target to re-emit its observable kinds up-lane (the reply is that
/// traffic filling the mirror, not a return value).
pub const REPORT: KindId = KindId(4);

/// Whether `id` is one of the reserved sentinels — used to keep a sentinel
/// dispatch from seeding the inbound-kind mirror (a sentinel carries no
/// payload).
#[must_use]
pub fn is_sentinel(id: KindId) -> bool {
    id == ATTACH || id == DETACH || id == FRAME || id == REPORT
}
