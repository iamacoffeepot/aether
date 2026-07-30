// Name registries. Two tables: mailboxes (MailboxId → name + entry,
// ids derived from name via ADR-0029's stable hash) and kinds (u64
// kind id → name + descriptor, ids derived from (name, schema) via
// ADR-0030 Phase 2's `kind_id_from_parts`). Both id spaces are a pure
// function of declaration-time data — no sequential allocation, no
// registration order dependence. The working tables sit behind a plain
// `Mutex` — there is no reader half, because every table read loads a
// published view instead (ADR-0165). Successful writes synchronously
// publish point-in-time route and kind views while holding that guard.
//
// Before the ADR-0165 runtime seal two writers share that guard: the
// registry owner draining its queue, and the boot path applying directly
// under a `BootAuthority`. The seal takes the last authority token out of
// circulation, after which the owner is the only caller that can name the
// guard at all.

mod address;
mod authority;
mod dispatch;
#[allow(dead_code, reason = "owner effects and inventory subscriptions are foundations consumed by arc follow-ups")]
pub(crate) mod effect;
mod errors;
mod handlers;
mod mailbox;
mod metrics;
mod names;
#[allow(dead_code, reason = "owner submission is installed now and consumed by staged-writer arc follow-ups")]
mod owner;
mod relay;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests;

pub use address::{ActorAddressInventoryError, AddressResolutionError, ResolvedAddress};
pub use authority::BootAuthority;
pub use dispatch::{MailDispatch, OwnedDispatch};
#[cfg(test)]
pub(crate) use dispatch::{test_dispatch, test_owned_dispatch};
#[doc(hidden)]
pub use effect::{PreparedAliasRoute, RegistryInventory, RegistrySubscription};
pub use errors::{DropError, KindConflict, NameConflict};
pub use handlers::{InboxHandler, InlineHandler, noop_handler};
pub(crate) use mailbox::{CapturedDisposition, RouteContinuation, RouteEndpoint, SeizeCell};
pub use mailbox::{MailboxEntry, Registry};
pub use metrics::RegistryQueueMetrics;
pub(crate) use owner::ParkAdmission;
pub(crate) use owner::RegistryOwnerLease;
pub(crate) use relay::RouteRelayHandle;
pub(crate) use relay::RouteRelayLease;
