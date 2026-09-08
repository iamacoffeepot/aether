//! The by-name address supplier for loaded components — the "routing" seam
//! of the `aether.component` capability.
//!
//! Senders do not route through this module. A co-hosted actor addresses a
//! loaded component the way it addresses anything else, by type:
//! `ctx.actor::<CameraComponent>()` for the default-named instance and
//! `ctx.resolve_embedded::<CameraComponent>(load_name)` for one loaded under
//! an explicit name. Both select `Embedded`'s parent scope from the caller's
//! runtime context, so neither needs this cap's carry — which is why the
//! sender-side `peer` / `peer_named` / `loaded` facades that once lived here
//! retired (iamacoffeepot/aether#5790). What remains is the one thing a
//! sender-side verb cannot supply: the host carry itself, for a caller that
//! holds no co-hosted ctx.

use aether_actor::{Addressable, Embedded};

use super::ComponentHostCapability;

/// Resolve the [`MailboxId`](aether_data::MailboxId) of the embeddable
/// component loaded under `name`, by folding the instance node
/// `aether.embedded:<name>` (the [`Embedded`]
/// resolver) onto the `aether.component` host cap's carry (ADR-0099 §5/§6,
/// ADR-0119).
///
/// This is the by-name carry-supplier. `aether-actor`'s `Embedded` resolver
/// owns the fold and the reserved scope
/// ([`EMBEDDED_SCOPE`](aether_actor::EMBEDDED_SCOPE)); this fn supplies the
/// `aether.component` carry, read only from its owner
/// [`ComponentHostCapability`]. Equal by construction to a component's own
/// `type Resolver = Embedded`, so this and the typed ctx routes
/// (`ctx.actor::<R>()`, `ctx.resolve_embedded::<R>(name)`) agree whenever the
/// caller's runtime parent *is* the root component host. Reach for it where no
/// such ctx exists — a native driver, or a test standing up the address
/// independently — and pair it with `ctx.actor_at::<R>(id)` / `ctx.send_to` to
/// send. Available on every target: a wasm peer resolves an embeddable the
/// same way a native one does, no transport branch (ADR-0029 client-side
/// no-lookup).
#[must_use]
pub fn resolve_embedded(name: &str) -> aether_data::MailboxId {
    use aether_actor::Resolve;
    Embedded::resolve(<ComponentHostCapability as Addressable>::resolve(0, ()).0, name, ())
}
