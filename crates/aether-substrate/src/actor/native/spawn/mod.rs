//! How an instanced native actor comes into being (ADR-0079, ADR-0165).
//!
//! Three files, one per phase of the staged path:
//!
//! - [`builder`] — the caller-facing surface. The eager
//!   [`SpawnBuilder::finish`] bridge for chassis and embedder callers, and
//!   [`HandlerSpawnBuilder::stage`] for handler callers, which initializes on
//!   the handler thread and appends an ordered commit to that turn's outbound
//!   work instead of publishing global state.
//! - `reservation` — parent-local uniqueness between a parent's staged and
//!   live children, deliberately held outside the routing and actor registries.
//! - `activation` — the private adapter that carries a staged actor across
//!   the activation barrier to `Live` and delivers the ADR-0093 `TaskDone` back
//!   to the parent.
//!
//! Not to be confused with [`super::offload::thread`], which spawns OS threads
//! rather than actors.

pub(crate) mod activation;
pub mod builder;
pub(crate) mod reservation;

pub use builder::{HandlerSpawnBuilder, SpawnBuilder, SpawnError, SpawnOutcome, SpawnReceipt, Spawner, Subname};
