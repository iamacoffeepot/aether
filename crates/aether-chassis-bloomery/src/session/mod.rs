//! The `session` native capability — the executor session-reuse pool.
//!
//! An executor-lane optimization, **not** an ADR-0149 control-core store port:
//! the model-driven runner lane (#3511) launches a headless Claude per attempt
//! and, without a pool, re-sends the large shared prefix (system prompt,
//! `CLAUDE.md`, skill text) and pays a prompt-cache *write* for it every time.
//! This capability ports the session-reuse invariants the fleet already settled
//! for `scripts/agent-pool.mjs` — a pool keyed by `(model, effort, task)`,
//! age-bounded by the prompt-cache TTL (#3264), gated on a static-prefix
//! `head_hash` freshness check (#3422), chaining manifests across a resume — so
//! a resumed attempt reuses the cached prefix instead of re-writing it.
//!
//! It deliberately does **not** port the `workspace_tree_hash` (belief-truth
//! subtree) gate #3341 measured and removed: a resume re-derives every deciding
//! fact on the fresh checkout, so gating on an unchanged tree bought no
//! correctness and cost all reuse. This pool's sole consumer is the
//! construct/verify/refine retry loop, where the workpiece tree changing between
//! attempts is the whole point, so the tree hash rides the manifest for audit
//! only, never as an eligibility gate.
//!
//! It is a separate capability from [`StoreCapability`](crate::store), not a
//! fold into the journal store: session pooling has a distinct lease/expiry
//! lifecycle and is not one of ADR-0149's six control-core ports, so keeping it
//! separate holds the port boundary clean. It owns its own small `SQLite` pool
//! table (metadata + lease); the session transcript bytes are content-addressed
//! in [`ArtifactsCapability`](crate::artifacts), never in the pool.
//!
//! Identity/runtime split (ADR-0122): the [`SessionPoolCapability`] ZST + the
//! `aether.session.*` kind family are always-on; the `SQLite`-backed runtime
//! lives in `runtime.rs` behind the `runtime` feature.

pub mod kinds;
pub use kinds::*;

#[cfg(feature = "runtime")]
mod config;
#[cfg(feature = "runtime")]
pub use config::{SessionConfig, SessionOverlay};

use aether_actor::actor;

/// Addressing identity for the `aether.session` capability.
#[actor(singleton)]
pub struct SessionPoolCapability;

#[cfg(feature = "runtime")]
mod runtime;
#[cfg(feature = "runtime")]
pub use runtime::{LeasedSession, SessionBackend, SessionPoolState, SqliteSessionStore};

#[cfg(all(test, feature = "runtime"))]
mod tests;
