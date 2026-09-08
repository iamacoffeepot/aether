//! How an instanced native actor comes into being (ADR-0079, ADR-0165).
//!
//! The vocabulary a caller answers with sits flat here — `error` for the
//! failure modes, `outcome` for the receipt a staging site gets back and
//! the authoritative fate that follows it. Beside them are the two
//! caller-facing builders, one per authority:
//!
//! - `eager` — the [`SpawnBuilder::finish`] bridge for chassis and
//!   embedder callers, whose terminals write registry, liveness, cost, and
//!   slot state synchronously.
//! - `staged` — [`HandlerSpawnBuilder::stage`] for handler callers, which
//!   initializes on the handler thread and appends an ordered commit to that
//!   turn's outbound work instead of publishing global state.
//!
//! Both flow through the shared `spawner` engine, which is split by phase:
//! `prepare` resolves identity and constructs the actor with no shared
//! write, `commit` takes whichever of the two routes the ADR-0165 seal
//! leaves open, and `teardown` walks every spawned slot at chassis
//! shutdown.
//!
//! - `reservation` — parent-local uniqueness between a parent's staged and
//!   live children, deliberately held outside the routing and actor registries.
//! - `activation` — the private adapter that carries a staged actor across
//!   the activation barrier to `Live` and delivers the ADR-0093 `TaskDone`
//!   back to the parent.
//!
//! Not to be confused with [`super::offload::thread`], which spawns OS threads
//! rather than actors.

pub(crate) mod activation;
mod eager;
mod error;
mod outcome;
pub(crate) mod reservation;
mod spawner;
mod staged;

#[cfg(test)]
mod tests;

/// The spawn-subname vocabulary, re-exported from `aether-actor`
/// (ADR-0097). It's shared between native `spawn_child` and the FFI
/// guest's `WasmCtx::spawn_child`, so it lives in the actor SDK both
/// transports depend on; native call sites import it from this path
/// unchanged. The full mailbox name is `"{A::NAMESPACE}:{subname}"`,
/// hashed deterministically (ADR-0029) to the returned `MailboxId`.
pub use aether_actor::Subname;

pub use eager::SpawnBuilder;
pub use error::SpawnError;
pub use outcome::{SpawnOutcome, SpawnReceipt};
pub use spawner::Spawner;
pub use staged::HandlerSpawnBuilder;
