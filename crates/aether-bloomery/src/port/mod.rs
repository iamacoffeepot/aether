//! The typed port trait shapes (ADR-0149 §The boundary).
//!
//! Bloomery's boundary is a set of typed ports, each owned by a native
//! capability so the wasm application never touches keys, databases, tokens,
//! or shells. The trait *shapes* live here — not host-side — so adapters
//! depend inward on this crate, cycle-free: [#3459]'s GitHub adapter
//! implements [`SourceBackend`] / [`ProjectionBackend`] and the host
//! ([#3458]) mounts them behind `Arc<dyn …>`. If the traits lived host-side,
//! an adapter implementing a host trait while the host statically links the
//! adapter would be a package-level dependency cycle. Adapters depend inward
//! on this crate, never the reverse.
//!
//! Only the two ports with a first consumer in the crate DAG ship here. The
//! `store` / `executor` / `signing` port shapes arrive with their first
//! consumers, not speculatively (ADR-0149 §The boundary).
//!
//! These are pure trait definitions over the value vocabulary — the
//! implementations do the I/O; the contracts are protocol semantics. No I/O
//! occurs in this crate.
//!
//! [#3458]: https://github.com/iamacoffeepot/aether/issues/3458
//! [#3459]: https://github.com/iamacoffeepot/aether/issues/3459

mod projection;
mod source;

pub use projection::{ProjectionBackend, ProjectionState};
pub use source::{Checkpoint, IntegrateOutcome, LandOutcome, SourceBackend, SourceSnapshot};
