//! Unit coverage for golden-task extraction and the seal-loop plan.
//!
//! The two things this module owns that can be wrong in a way nothing else
//! catches: which issue a landed pull request's prose names, and whether a plan
//! produces one distinct, cell-attributable bloom per `(task, cell, sample)`.

use std::collections::BTreeSet;

use aether_bloomery::{Digest, ModelOverride};
use aether_bloomery_git::{ChecksState, NewPullRequest, PullRequestApi, fixture::FakeGithub};
use aether_data::Kind;

use super::golden::{GoldenTask, GoldenTaskError, GoldenTaskSet, extract};
use super::run::{BenchmarkRefusal, MAX_BENCHMARK_BLOOMS, PlannedBloom, plan};

/// The tree a seeded landing's head carries — the reference answer extraction
/// reads back.
const LANDED_TREE: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";

fn cell(seed: u8) -> Digest {
    Digest::from_bytes([seed; 32])
}

/// A fixture repository holding one landed pull request: issue `issue` carries
/// `order`, the proposal's body closes it, and its head passed CI.
fn landed(issue: u64, order: &str, body: &str) -> (FakeGithub, u64) {
    let github = FakeGithub::new();
    github.seed_issue(issue, order);

    let head = github.seed_commit(LANDED_TREE);
    github.seed_ref("heads/landed", &head);
    github.seed_checks(&head, ChecksState::Passed);

    let proposal = github
        .create_pull_request(&NewPullRequest {
            title: "feat(x): the landed change".to_owned(),
            body: body.to_owned(),
            head: "landed".to_owned(),
            base: "main".to_owned(),
        })
        .expect("the fixture opens the proposal");
    github.merge_pull_request(proposal.number, "merge-sha");

    (github, proposal.number)
}

fn task_set(tasks: Vec<GoldenTask>) -> GoldenTaskSet {
    GoldenTaskSet { name: "wave".to_owned(), base: cell(0xB0), tasks }
}

fn task(pull_request: u64) -> GoldenTask {
    GoldenTask {
        pull_request,
        issue: pull_request,
        order: "an order".to_owned(),
        landed_head: "head".to_owned(),
        reference: LANDED_TREE.to_owned(),
    }
}

fn overrides(_: Digest) -> Option<String> {
    Some(ModelOverride::NAME.to_owned())
}

// A landed pull request becomes a work order, a landed head, and a reference
// answer. The bug: extraction that read the *pull request's* prose as the order
// would replay the implementer's summary instead of the task, so every cell
// would be measured on a description of the answer.
#[test]
fn extraction_takes_the_work_order_from_the_issue_the_landing_closed() {
    let (github, number) = landed(5820, "Build the benchmark run.", "Does the thing.\n\nCloses #5820\nRefs #4871");

    let task = extract(&github, number).expect("a landed, green, issue-closing proposal extracts");

    assert_eq!(task.issue, 5820);
    assert_eq!(task.order, "Build the benchmark run.", "the order is the issue text, not the proposal's");
    assert_eq!(task.reference, LANDED_TREE, "the reference answer is the landed head's tree");
    assert_eq!(task.pull_request, number);
}

// Tripwire: a closing keyword binds the number that immediately follows it, and
// binds all of it. `Closes #58201` names issue 58201; a prefix-matching or
// first-digits-win reader would silently replay issue 5820's order under a
// task drawn from a different landing, and every assertion above would still
// pass. `Refs #N` is not a closure at all.
#[test]
fn only_a_closing_keyword_names_the_work_order_and_it_names_the_whole_number() {
    let cases = [
        ("Closes #5820", Some(5820_u64)),
        ("closes #5820.", Some(5820)),
        ("Fixed (#5820)", Some(5820)),
        ("Closes #58201", Some(58201)),
        ("Refs #5820", None),
        ("Part of #5820", None),
        ("Closes 5820", None),
    ];

    for (body, want) in cases {
        let (github, number) = landed(5820, "an order", body);
        github.seed_issue(58201, "another order");

        let extracted = extract(&github, number);
        match want {
            Some(issue) => {
                assert_eq!(extracted.map(|task| task.issue), Ok(issue), "{body:?} names issue {issue}");
            }
            None => {
                assert_eq!(extracted, Err(GoldenTaskError::NoWorkOrder(number)), "{body:?} names no work order");
            }
        }
    }
}

// A landing whose CI did not pass sets no bar, and an unmerged proposal is not
// landed history. Both are refusals rather than tasks with a missing column: a
// benchmark cell drawn from either looks exactly like an honest one downstream,
// because the ledger records what a bloom did and never what it was drawn from.
#[test]
fn extraction_refuses_a_landing_that_is_not_one() {
    let (red, number) = landed(5820, "an order", "Closes #5820");
    let head = red.pull_request_head_sha(number).expect("the fixture holds the proposal");
    red.seed_checks(&head, ChecksState::Failed { failing: vec!["CI pass".to_owned()] });
    assert!(
        matches!(extract(&red, number), Err(GoldenTaskError::BarNotPassed { .. })),
        "a red landing sets no bar to clear"
    );

    let open = FakeGithub::new();
    open.seed_issue(5820, "an order");
    open.seed_ref("heads/open", &open.seed_commit(LANDED_TREE));
    let proposal = open
        .create_pull_request(&NewPullRequest {
            title: "t".to_owned(),
            body: "Closes #5820".to_owned(),
            head: "open".to_owned(),
            base: "main".to_owned(),
        })
        .expect("the fixture opens the proposal");
    assert_eq!(extract(&open, proposal.number), Err(GoldenTaskError::NotLanded(proposal.number)));
}

// The whole mechanism, in one assertion: two cells at sample size two over one
// golden task are four *distinct* blooms, each sealing its own cell address, all
// on the set's one base.
//
// Tripwire: a sealed spec is addressed by its own content, so a plan that reused
// one workpiece across samples would produce two identical ids — and the second
// admit would be deduplicated into the first, leaving a run of four that
// measured two while every per-cell assertion still held.
#[test]
fn two_cells_at_sample_size_two_are_four_distinct_blooms_attributable_to_their_cells() {
    let cells = [cell(0xC1), cell(0xC2)];

    let planned =
        plan(task_set(vec![task(5820)]), &cells, 2, overrides).expect("two resolvable cells at sample size two plan");

    assert_eq!(planned.blooms.len(), 4);
    let ids: BTreeSet<_> = planned.blooms.iter().map(PlannedBloom::id).collect();
    assert_eq!(ids.len(), 4, "every sample is its own bloom");

    for bloom in &planned.blooms {
        assert_eq!(bloom.spec.base(), planned.set.base, "every cell replays the same base");
        let member = &bloom.spec.members()[0];
        assert_eq!(
            member.configs.address::<ModelOverride>(),
            Some(bloom.cell),
            "the member seals its cell's override address, which is what the ledger attributes to"
        );
        assert!(member.approval.validates(&member.subject()), "the trial approval binds its own member subject");
        assert_eq!(bloom.order, "an order", "every cell replays the same work order");
    }

    assert_eq!(
        cells.map(|address| planned.blooms.iter().filter(|bloom| bloom.cell == address).count()),
        [2, 2],
        "each cell got its sample size"
    );
}

// A cell address the coordinator cannot resolve, or one filed under another
// kind, is a refusal. The bug it catches is silent rather than loud: an
// unresolved override leaves its dispatch on the compiled line (ADR-0184 §The
// agent is recomputed), so the run would produce N cells that all measure the
// same agent under N different names — a comparison table with no comparison in
// it.
#[test]
fn a_cell_that_is_not_a_resolvable_override_is_refused() {
    let set = task_set(vec![task(1)]);

    assert_eq!(plan(set.clone(), &[cell(0xC1)], 1, |_| None), Err(BenchmarkRefusal::UnresolvableCell(cell(0xC1))));
    assert_eq!(
        plan(set.clone(), &[cell(0xC1)], 1, |_| Some("aether.bloomery.stage_catalog".to_owned())),
        Err(BenchmarkRefusal::CellIsNotAnOverride {
            cell: cell(0xC1),
            kind: "aether.bloomery.stage_catalog".to_owned()
        })
    );
    assert_eq!(plan(set.clone(), &[], 1, overrides), Err(BenchmarkRefusal::NoCells));
    assert_eq!(plan(set, &[cell(0xC1)], 0, overrides), Err(BenchmarkRefusal::NoSamples));
}

// The multiplicative ceiling refuses the whole run rather than sealing a prefix:
// a partially-sealed benchmark is a comparison with a hole in it, and the cells
// that fit are indistinguishable from a complete table.
#[test]
fn a_run_over_the_bloom_ceiling_seals_nothing() {
    let over = vec![task(1); MAX_BENCHMARK_BLOOMS + 1];

    assert_eq!(
        plan(task_set(over), &[cell(0xC1)], 1, overrides),
        Err(BenchmarkRefusal::TooManyBlooms(MAX_BENCHMARK_BLOOMS + 1))
    );
}
