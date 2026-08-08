//! The `control` native capability — the single-writer control core (ADR-0149
//! §The control core; native since the wasm-boundary retirement, formerly a
//! wasm component).
//!
//! [`ControlCore`] is the single owner of the live [`Snapshot`](aether_bloomery::Snapshot):
//! it drives [`reduce`](aether_bloomery::reduce()) on every admitted event, commits
//! the decision through the `aether.store` capability in one atomic transaction,
//! applies it to its in-memory snapshot on the commit reply, and serves reads off
//! that snapshot. At boot it replays the journal to rebuild the snapshot, so a
//! `kill -9` + restart converges through the reducer.
//!
//! # Why it is native
//!
//! The core was a wasm component (ADR-0149 §The boundary): the sandbox was meant
//! to keep the control logic from touching keys, the database, or a shell. But a
//! sandbox is only a boundary across a trust asymmetry, and the operator controls
//! the host binary *and* the control logic — one trust domain, no asymmetry — so
//! the wasm line guarded a door in a field while forcing every native peer to
//! address the core by mailbox rather than by type. It is now a native cap beside
//! the store / signing / artifacts caps it already drove: `reduce()` links
//! directly, and the api / reactors address it as [`ControlCore`]. wasm stays for
//! *extension* surfaces (user- or agent-authored logic on a fixed host), which the
//! control core is not (ADR-0149 §The boundary, amended).
//!
//! Identity/runtime split (ADR-0122): the [`ControlCore`] ZST is always-on; the
//! snapshot-owning runtime lives in `runtime.rs` behind the `runtime` feature.

use aether_actor::actor;

// The handled-kind types the `#[actor(singleton)]` dispatch table references —
// the `aether.bloomery.{admit,query}` ingress plus the store / source reply kinds
// each of the cap's handlers folds. Imported here (like the store / api caps) so
// the always-on identity markers resolve without the `runtime` runtime module.
use aether_bloomery::control::{
    Admit, ClaimResult, CommitResult, EnumerateClaimsResult, LoadConfigsResult, ObserveMainlineResult, Query,
    ReplayJournalResult,
};

/// Addressing identity for the `aether.bloomery.control` capability (ADR-0122).
#[actor(singleton, root)]
pub struct ControlCore;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::ControlCoreState;
