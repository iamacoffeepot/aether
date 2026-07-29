// Name registries. Two tables: mailboxes (MailboxId → name + entry,
// ids derived from name via ADR-0029's stable hash) and kinds (u64
// kind id → name + descriptor, ids derived from (name, schema) via
// ADR-0030 Phase 2's `kind_id_from_parts`). Both id spaces are a pure
// function of declaration-time data — no sequential allocation, no
// registration order dependence. The registry uses an `RwLock` only to
// serialize writers so mailboxes and kinds can be added at runtime —
// ADR-0010's runtime component loading mutates both tables after an
// `Arc<Registry>` has already been shared with the scheduler and hub
// client. Successful writes synchronously publish point-in-time route and
// kind views while holding that guard. Every table read loads those views
// without acquiring the writer lock (ADR-0165).

mod address;
mod dispatch;
mod errors;
mod handlers;
mod mailbox;
mod names;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "test-setup unwraps: fixture construction and decode panic on failure is the assertion"
)]
mod tests;

pub use address::{ActorAddressInventoryError, AddressResolutionError, ResolvedAddress};
pub use dispatch::{MailDispatch, OwnedDispatch};
#[cfg(test)]
pub(crate) use dispatch::{test_dispatch, test_owned_dispatch};
pub use errors::{DropError, KindConflict, NameConflict};
pub use handlers::{InboxHandler, InlineHandler, noop_handler};
pub use mailbox::{MailboxChangeHook, MailboxEntry, Registry};
