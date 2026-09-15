//! A construct lane's prompt is rendered from the scope revision's typed
//! sections, and a revision that leaves one of them empty never becomes a
//! commission's tip (#5995).
//!
//! Pre-fix, the prompt's whole `## Task` section was the revision's free-text
//! `description`. `problem`, `design`, `plan` and `declared_surface` were
//! signed, gated, and never delivered: a commission whose summary was a
//! one-line title dispatched a lane that had the title and nothing else. On
//! 2026-09-14 six members were built from titles that way before a lane
//! declined rather than guess, which is how the gap was found.
//!
//! Both halves are one scenario because either alone is a half-fix. Rendering
//! the fields with the doors still admitting empty ones renders empty headings;
//! refusing empty ones with the renderer still echoing `description` refuses
//! nothing anyone was reading.

#![allow(clippy::unwrap_used)]

use aether_bloomery::{
    CONSTRUCT_IMPLEMENT_COMMAND, ScopeRevision, StageId, WorkpieceId, digest_of, split_lane_identity,
};
use aether_chassis_bloomery::store::{CommissionBackend, CommissionError, RevisionEvidence};
use aether_harness_bloomery::{BloomeryHarness, LaneScript};

/// The sections a complete successor states. Each is distinct prose so the
/// rendered prompt can be read back section by section, and none of them is a
/// substring of another.
const PROBLEM: &str = "the lane is handed a title and no work order";
const DESIGN: &str = "render each heading from the field that carries it";
const PLAN: &str = "1. render from the fields\n2. refuse an empty section at the door";
const SUMMARY: &str = "thread the typed sections into the lane prompt";

#[test]
fn a_revision_with_an_empty_plan_is_refused_naming_the_field() {
    let harness = BloomeryHarness::start();
    let tip = harness.author_scope_revision("wp", &["crates/example-a/**"]);
    let authored = harness.scope_revision(tip).expect("the authored revision is in the commission store");

    let blank = ScopeRevision { predecessor: Some(tip), plan: String::new(), ..authored };
    let refused = harness
        .commission_store()
        .write_revision(&blank, &RevisionEvidence::default())
        .expect_err("a revision with no plan must not become a commission's tip");

    assert_eq!(refused, CommissionError::EmptySection { section: "plan".to_owned() });
    assert!(refused.to_string().contains("plan"), "the refusal names the empty field: {refused}");
    assert_eq!(
        harness.scope_revision(digest_of(&blank)),
        None,
        "a refused revision is not stored under its own digest either",
    );
}

#[test]
fn a_complete_revision_dispatches_a_prompt_carrying_every_section() {
    let mut harness = BloomeryHarness::start();
    harness.script_lane(&WorkpieceId("wp".into()), StageId::Construct, &[LaneScript::Decline]);

    let tip = harness.author_scope_revision("wp", &["crates/example-a/**"]);
    let authored = harness.scope_revision(tip).expect("the authored revision is in the commission store");
    let complete = ScopeRevision {
        predecessor: Some(tip),
        problem: PROBLEM.to_owned(),
        design: DESIGN.to_owned(),
        plan: PLAN.to_owned(),
        description: SUMMARY.to_owned(),
        ..authored
    };
    let sealed = harness
        .commission_store()
        .write_revision(&complete, &RevisionEvidence::default())
        .expect("the successor writes");

    let _bloom = harness.seal_member("wp", sealed);
    harness.run_until(|harness| construct_task(harness).is_some(), 40);
    let task = construct_task(&harness).expect("the construct lane was handed a work order");

    for section in [PROBLEM, DESIGN, PLAN] {
        assert!(task.contains(section), "the prompt carries the section verbatim: {task}");
    }
    assert!(task.contains("crates/example-a/**"), "the prompt states the declared surface: {task}");

    let at = |heading: &str| task.find(heading).unwrap_or_else(|| panic!("the prompt states {heading:?}:\n{task}"));
    let headings =
        [at("## Problem statement"), at("## Design notes"), at("## Implementation plan"), at("## Declared surface")];
    assert!(
        headings.windows(2).all(|pair| pair[0] < pair[1]),
        "problem, design, plan, then surface — the order a reader works through: {task}"
    );

    // `description` is the one-line summary the field was named for: it heads
    // the order and states none of the work. A renderer that put it back in the
    // body would be the pre-fix prompt wearing new headings.
    let (body, identity) = split_lane_identity(&task);
    assert!(identity.is_some(), "the fan-out pins the member onto the shared order: {task}");
    assert!(body.starts_with(&format!("# {SUMMARY}\n\n")), "the summary is the heading: {body}");
    assert_eq!(task.matches(SUMMARY).count(), 1, "and appears nowhere else: {task}");
}

/// The work order the first dispatched construct lane was handed, once one has
/// been dispatched.
fn construct_task(harness: &BloomeryHarness) -> Option<String> {
    harness.ledger().into_iter().find(|run| run.command == CONSTRUCT_IMPLEMENT_COMMAND).and_then(|run| run.task)
}
