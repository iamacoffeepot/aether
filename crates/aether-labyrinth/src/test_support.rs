//! Crate-internal test-support helpers shared across the unit-test and
//! baseline-replay modules. Test-only (`#[cfg(test)]` at the `mod` site in
//! `lib.rs`), so nothing here reaches production code; it only de-duplicates
//! fixtures the test modules would otherwise each re-declare.

use crate::reachability::test_fields::stencil_offsets;
use aether_kinds::{CorridorEdge, EdgeKind, StencilOffset};

/// The 4-way movement stencil as raw offsets: the zero "stay" offset plus the
/// four orthogonal one-cell moves. Delegates to the canonical
/// [`stencil_offsets`] so the offset literal lives in exactly one place; the
/// corridor / counterfactual / baseline harnesses share this name.
pub fn stencil_4way() -> Vec<StencilOffset> {
    stencil_offsets()
}

/// Count the `Flow` edges leaving node `n` (its flow out-degree).
pub fn flow_out(edges: &[CorridorEdge], n: u32) -> usize {
    edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Flow && e.from == n)
        .count()
}

/// Count the `Flow` edges entering node `n` (its flow in-degree).
pub fn flow_in(edges: &[CorridorEdge], n: u32) -> usize {
    edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Flow && e.to == n)
        .count()
}
