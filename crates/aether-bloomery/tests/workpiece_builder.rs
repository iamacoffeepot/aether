//! Replacing an authored list with an empty list must clear it (issue 5580).
//!
//! Last-write-wins is per generation. [`WorkpieceBuilder::finish`] is the
//! production boundary that projects the winning generation; these cases fail
//! if an empty replacement leaves the previous edges, evidence, plan steps, or
//! declared-surface globs in the scope revision.

use aether_bloomery::{ScopeRouting, WorkpieceBuilder, WorkpieceId, WorkpieceRefusal};

fn workpiece() -> WorkpieceId {
    WorkpieceId(String::from("issue-5580"))
}

fn routing() -> ScopeRouting {
    ScopeRouting { size: String::from("l"), model: String::from("grok-4.6") }
}

fn empty() -> core::iter::Empty<&'static str> {
    core::iter::empty()
}

fn required(builder: &mut WorkpieceBuilder) -> &mut WorkpieceBuilder {
    builder
        .problem("the problem")
        .plan_step(["do the work"])
        .declared_surface(["crates/aether-bloomery/src/lib.rs"])
}

#[test]
fn finish_projects_no_dependencies_after_an_empty_edge_replacement() {
    let mut builder = WorkpieceBuilder::new(workpiece());
    let revision = required(&mut builder)
        .edge(["issue-1", "issue-2"])
        .edge(empty())
        .finish(None, routing())
        .expect("clearing edges is coherent");
    assert!(revision.dependencies.is_empty(), "empty edge replacement resurrected {revision:?}");
}

#[test]
fn finish_refuses_a_declared_surface_cleared_to_empty() {
    let mut builder = WorkpieceBuilder::new(workpiece());
    required(&mut builder).declared_surface(empty());
    assert!(matches!(builder.finish(None, routing()), Err(WorkpieceRefusal::EmptyDeclaredSurface { .. })));
}

#[test]
fn finish_refuses_plan_steps_cleared_to_empty() {
    let mut builder = WorkpieceBuilder::new(workpiece());
    required(&mut builder).plan_step(empty());
    assert!(matches!(builder.finish(None, routing()), Err(WorkpieceRefusal::NoPlanStep { .. })));
}

#[test]
fn finish_drops_cleared_evidence_from_the_problem() {
    let mut builder = WorkpieceBuilder::new(workpiece());
    let revision = required(&mut builder)
        .evidence(["grounding that must not survive"])
        .evidence(empty())
        .finish(None, routing())
        .expect("clearing evidence is coherent");
    assert_eq!(revision.problem, "the problem");
}
