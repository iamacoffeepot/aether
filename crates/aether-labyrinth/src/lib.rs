//! The space-time reachability subsystem — generative maze/map
//! reachability over scalar fields (issue 1908, extracted from
//! `aether-capabilities`).
//!
//! The crate reads as "the reachability primitive plus what's built on
//! it": a pure minimum-cost solver core, the analyses layered on its
//! cost-to-reach field, the certifier `#[transform]`s that wrap those
//! analyses as content-addressed DAG nodes (ADR-0047/0048/0049), and the
//! `aether.trajectory` recorder cap that captures the paths the
//! counterfactual / traffic passes consume.
//!
//! # Module map
//!
//! The data flows solver → corridor → derived passes → transforms →
//! recorder. The pure cores are crate-internal (`pub(crate)`); the
//! `transforms` module is the public surface, reached through the
//! link-time `#[transform]` inventory rather than by path.
//!
//! - `reachability` — the pure minimum-cost reachability solver core
//!   (#1857). Solves the cost-to-reach field `V` over a time-varying
//!   scalar cost field, and hosts the population sweep (#1863) and the
//!   self-realizing field simulator (#1867) that reuse it.
//! - `corridor` — the corridor-graph core (#1858): the connectivity
//!   skeleton of a solved `V` under a budget, plus the fork
//!   resolution-depth analysis (#1859).
//! - `counterfactual` — counterfactual reachability-from-state (#1864):
//!   classifies each budget-crossing in a recorded path as avoidable or
//!   unavoidable via a windowed re-solve seeded from the path's state.
//! - `traffic` — trajectory-density aggregation (#1865): snaps a set of
//!   paths onto the corridor graph and accumulates per-edge traffic.
//! - `escapability` — the O(1) local contribution-escapability bound
//!   (#1866): certifies a single snapshot's escapability and a
//!   conservative concurrency cap.
//! - `transforms` — the nine certifier `#[transform]`s wrapping the
//!   cores above as `Kind → Kind` DAG nodes. The link-time inventory
//!   submission populates both `aether-substrate-bundle`'s headless
//!   `TransformRegistry` and `aether-mcp`'s `describe_transforms`.
//! - `trajectory` (feature `native`) — the `aether.trajectory` recorder
//!   cap (#1862): accumulates per-tick samples into a seed-keyed
//!   `TrajectoryLog` handle (ADR-0049) replayable offline.
//!
//! # Transforms
//!
//! Each delegates to a pure core and clears the `#[transform]` purity
//! deny-list (no host fn, no `Ctx`, no `std::time` / `std::env`):
//!
//! - `solve` — minimum-cost reachability → the cost-to-reach field `V`.
//! - `reachability_margin` — threshold `V`'s final tick against a budget.
//! - `build_corridor_graph` — the time-sliced corridor graph of `V`.
//! - `corridor_resolution_depth` — the fork lookahead depth of a graph.
//! - `solve_population` — the survival-vs-reaction-delay curve of a
//!   seeded agent population.
//! - `solve_counterfactual` — per-crossing avoidable / unavoidable
//!   verdicts for a recorded path.
//! - `simulate_realization_sweep` — the closure-outcome distribution of a
//!   seeded population of self-realizing field runs.
//! - `realize_single_run` — one realized field, the inspection companion
//!   to the counts-only distribution.
//! - `aggregate_traffic` — per-edge traffic density of a path set on a
//!   corridor graph.
//!
//! # Capability
//!
//! - `TrajectoryRecorderCapability` (feature `native`) — the
//!   `aether.trajectory` mailbox cap. Default transforms-only builds (e.g.
//!   `aether-mcp`, which only reads the transform inventory) skip it; the
//!   chassis bins enable `native` to register it.

// The `aether.reach.*` / `aether.corridor.*` kind vocabulary (issue
// 1914), relocated from `aether-kinds` so this crate owns the kinds its
// solver, transforms, and passes operate over. Re-exported at the crate
// root (`pub use kinds::*`) so peers (e.g. `aether-mesh-viewer`) and this
// crate's own modules reach them by name.
pub mod kinds;
pub use kinds::*;

// The pure cores back the certifier `#[transform]`s and the test/baseline
// harness, all of which live in this crate; nothing reaches them across a
// crate boundary, so they stay `pub(crate)` (as they were in
// `aether-capabilities`). `escapability` keeps the `pub` it carried there
// — its `evaluate` / `EscapeParams` are the local-bound API the realization
// passes reference.
pub(crate) mod corridor;
pub(crate) mod counterfactual;
pub mod escapability;
pub(crate) mod reachability;
pub(crate) mod traffic;
// The certifier transforms. The `#[transform]` fns are private — they
// register via the link-time inventory, not by path — so the module is the
// only public handle.
pub mod transforms;

// The `aether.trajectory` recorder cap implements `NativeActor`, so it
// rides behind the `native` feature with the substrate runtime it needs;
// the transform-only consumer never compiles it.
#[cfg(feature = "native")]
pub mod trajectory;

#[cfg(feature = "native")]
pub use trajectory::TrajectoryRecorderCapability;

// Local `TestChassis` fixture for the recorder cap's tests. The cap-side
// copies in `aether-capabilities` / `aether-mcp` are `#[cfg(test)]
// pub(crate)` and not reachable cross-crate, so this crate keeps its own
// (issues 785 / 802 precedent).
#[cfg(all(test, feature = "native"))]
mod test_chassis;

// Baseline-replay validation harness for the reachability solver (#1860).
// Test-only; no production code.
#[cfg(test)]
mod reachability_baselines;

// Crate-internal test-support fixtures (`stencil_4way`, `flow_in` / `flow_out`)
// shared across the unit-test and baseline-replay modules. Test-only.
#[cfg(test)]
mod test_support;
