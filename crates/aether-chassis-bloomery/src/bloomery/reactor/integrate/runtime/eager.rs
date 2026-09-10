//! Immutable eager-head append and reconcile preparation source operations.

use serde::Serialize;

use aether_bloomery::{
    BloomId, CandidatePreparation, CandidatePreparationPlan, CandidateRef, Checkpoint, CompositionInput,
    ContentAddressed, Digest, Evidence, EvidenceKind, IntegrateOutcome, IntegrationAppendPlan, IntegrationHead,
    MemberPin, PreparedCandidate, digest_of,
};
use aether_bloomery_github::SourceError;

use crate::bloomery::SourceShell;
use crate::bloomery::coordination::{generation_namespace, preparation_namespace};

/// Result of executing one already-journaled append plan.
pub(super) enum AppendResult {
    Advanced(IntegrationHead),
    Conflicted { input: CompositionInput, at: CandidateRef, evidence: Evidence, diagnostic: String },
    Stale(Digest),
    Refused(String),
    Stopped(String),
}

#[derive(Serialize)]
struct IntegrationNodeAddress<'a> {
    generation: Digest,
    plan: Digest,
    candidate: CandidateRef,
    coverage: &'a [MemberPin],
}

impl ContentAddressed for IntegrationNodeAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.integration_node";
}

/// Scratch refs for one generation are isolated from every predecessor and
/// successor generation. An obsolete worker can finish only inside the name its
/// journaled plan selected.
/// Execute one immutable append plan in its generation namespace.
///
/// Each plan carries one atomic composition input. The outbox admits plans in
/// sequence, so a branch ahead of the expected parent can only be this same
/// request published before its receipt. Re-offering its input through
/// `integrate_pinned` answers `AlreadyUpToDate` and repairs a missing head
/// correspondence. Arrivals after this plan was journaled cannot extend it.
pub(super) fn append_plan(source: &SourceShell, plan: &IntegrationAppendPlan) -> AppendResult {
    if plan.inputs.len() != 1
        || plan.expected_parent.generation != plan.generation
        || plan.expected_parent.coverage.iter().any(|pin| !generation_holds(plan, pin))
    {
        return AppendResult::Refused("eager append plan has inconsistent generation inputs".to_owned());
    }

    let namespace = generation_namespace(plan.generation);
    let position = match source.integration_checkpoint(&namespace, &plan.expected_parent.candidate.checkout) {
        Ok(position) => position,
        Err(error) => return AppendResult::Stopped(format!("eager integration bootstrap failed: {error}")),
    };
    if position.checkpoint.tree != plan.expected_parent.candidate.tree {
        let Some(actual_head) = position.head else {
            return AppendResult::Stale(position.checkpoint.tree);
        };
        match source.is_fast_forward(&plan.expected_parent.candidate.checkout, &actual_head) {
            Ok(true) => {}
            Ok(false) => return AppendResult::Stale(position.checkpoint.tree),
            Err(error) => return AppendResult::Stopped(format!("eager integration ancestry check failed: {error}")),
        }
    }
    resume_append(source, plan, namespace, position.checkpoint.tree, position.head)
}

fn resume_append(
    source: &SourceShell,
    plan: &IntegrationAppendPlan,
    namespace: BloomId,
    mut tree: Digest,
    mut checkout: Option<Digest>,
) -> AppendResult {
    let mut coverage = plan.expected_parent.coverage.clone();
    let mut expected = Checkpoint { bloom: namespace, tree };
    for input in &plan.inputs {
        if let Err(reason) = extend_coverage(&mut coverage, &input.members) {
            return AppendResult::Refused(reason);
        }
        match source.integrate_pinned(&namespace, &input.candidate, &expected) {
            Ok(IntegrateOutcome::Integrated { tree: next_tree, head }) => {
                tree = next_tree;
                checkout = Some(head);
                expected.tree = next_tree;
            }
            Ok(IntegrateOutcome::Conflict { at, paths, diff, .. }) => {
                let at =
                    CandidateRef { tree: at, checkout: checkout.unwrap_or(plan.expected_parent.candidate.checkout) };
                let diagnostic = conflict_diagnostic(plan, input, &paths, &diff);
                let evidence = Evidence {
                    subject: at.tree,
                    kind: EvidenceKind::FoldConflict,
                    detail: Digest::of_wire_bytes(diagnostic.as_bytes()),
                };
                return AppendResult::Conflicted { input: input.clone(), at, evidence, diagnostic };
            }
            Ok(IntegrateOutcome::StaleCheckpoint { actual }) => return AppendResult::Stale(actual),
            Err(SourceError::Malformed(reason)) => return AppendResult::Refused(reason),
            Err(error) => return AppendResult::Stopped(format!("eager integration append failed: {error}")),
        }
    }

    let Some(checkout) = checkout else {
        return AppendResult::Refused("eager append produced no checkout".to_owned());
    };
    let candidate = CandidateRef { tree, checkout };
    let plan_digest = plan.digest();
    let node = digest_of(&IntegrationNodeAddress {
        generation: plan.generation,
        plan: plan_digest,
        candidate,
        coverage: &coverage,
    });
    AppendResult::Advanced(IntegrationHead {
        generation: plan.generation,
        node,
        candidate,
        plan: plan_digest,
        coverage,
    })
}

fn generation_holds(plan: &IntegrationAppendPlan, pin: &MemberPin) -> bool {
    plan.expected_parent.coverage.contains(pin)
        && plan
            .inputs
            .iter()
            .flat_map(|input| &input.members)
            .find(|other| other.workpiece == pin.workpiece)
            .is_none_or(|other| other == pin)
}

fn extend_coverage(coverage: &mut Vec<MemberPin>, additions: &[MemberPin]) -> Result<(), String> {
    if additions.is_empty() {
        return Err("an eager append input has empty member coverage".to_owned());
    }
    for pin in additions {
        match coverage.iter().find(|current| current.workpiece == pin.workpiece) {
            Some(current) if current != pin => {
                return Err(format!("eager append input replaces the pinned version of {}", pin.workpiece.0));
            }
            Some(_) => {}
            None => coverage.push(pin.clone()),
        }
    }
    Ok(())
}

fn conflict_diagnostic(plan: &IntegrationAppendPlan, input: &CompositionInput, paths: &[String], diff: &str) -> String {
    use core::fmt::Write;

    let mut diagnostic = format!(
        "Eager append {} could not place composition {} onto head {}.\n",
        plan.digest().to_hex(),
        input.node.to_hex(),
        plan.expected_parent.node.to_hex(),
    );
    if !paths.is_empty() {
        diagnostic.push_str("\nConflicting paths:\n");
        for path in paths {
            let _ = writeln!(diagnostic, "- {path}");
        }
    }
    if !diff.trim().is_empty() {
        diagnostic.push_str("\nConflicted contribution:\n\n");
        diagnostic.push_str(diff.trim());
        diagnostic.push('\n');
    }
    diagnostic
}

/// Mechanically prepare a reconcile result against the immutable head recorded
/// on its order. A conflict is evidence about that exact head and never reaches
/// member Verify. A clean result names P, the commit/tree pair Verify may judge.
pub(super) fn prepare_candidate(source: &SourceShell, plan: &CandidatePreparationPlan) -> CandidatePreparationResult {
    let namespace = preparation_namespace(plan);
    let base = &plan.context.starting_head.candidate;
    match source.prepare_pinned(&namespace, base, &plan.authored) {
        Ok(IntegrateOutcome::Integrated { tree, head }) => {
            CandidatePreparationResult::Completed(CandidatePreparation::Prepared(PreparedCandidate {
                authored: plan.authored,
                candidate: CandidateRef { tree, checkout: head },
                context: plan.context.clone(),
                diff_base: *base,
            }))
        }
        Ok(IntegrateOutcome::Conflict { at, paths, diff, .. }) => {
            let diagnostic = preparation_conflict_diagnostic(plan, &paths, &diff);
            let evidence = Evidence {
                subject: at,
                kind: EvidenceKind::FoldConflict,
                detail: Digest::of_wire_bytes(diagnostic.as_bytes()),
            };
            CandidatePreparationResult::Conflicted {
                preparation: CandidatePreparation::Conflict { evidence },
                diagnostic,
                parent: at,
            }
        }
        Ok(IntegrateOutcome::StaleCheckpoint { actual }) => {
            CandidatePreparationResult::Completed(CandidatePreparation::Refused { detail: actual })
        }
        Err(SourceError::Malformed(reason)) => CandidatePreparationResult::Refused(reason),
        Err(error) => CandidatePreparationResult::Stopped(format!("candidate preparation failed: {error}")),
    }
}

pub(super) enum CandidatePreparationResult {
    Completed(CandidatePreparation),
    Conflicted { preparation: CandidatePreparation, diagnostic: String, parent: Digest },
    Refused(String),
    Stopped(String),
}

fn preparation_conflict_diagnostic(plan: &CandidatePreparationPlan, paths: &[String], diff: &str) -> String {
    use core::fmt::Write;

    let mut diagnostic = format!(
        "Candidate {} still conflicts with pinned head {}.\n",
        plan.authored.tree.to_hex(),
        plan.context.starting_head.node.to_hex(),
    );
    if !paths.is_empty() {
        diagnostic.push_str("\nConflicting paths:\n");
        for path in paths {
            let _ = writeln!(diagnostic, "- {path}");
        }
    }
    if !diff.trim().is_empty() {
        diagnostic.push_str("\nAuthored delta:\n\n");
        diagnostic.push_str(diff.trim());
        diagnostic.push('\n');
    }
    diagnostic
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use aether_bloomery::testing::digest;
    use aether_bloomery::{IntegrationAppendPlan, IntegrationHead, WorkpieceId};
    use aether_bloomery_github::{GitSource, MainlineRef, fixture::FakeGithub};

    use super::*;

    fn source(fake: &FakeGithub) -> SourceShell {
        SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), false, MainlineRef::default())))
    }

    fn pin(fake: &FakeGithub, seed: u8, workpiece: &str) -> MemberPin {
        let tree = digest(seed);
        MemberPin {
            workpiece: WorkpieceId(workpiece.to_owned()),
            scope_revision: digest(seed.saturating_add(20)),
            candidate: CandidateRef { tree, checkout: fake.seed_base_commit(&tree) },
        }
    }

    #[test]
    fn append_uses_the_recorded_order_and_replays_the_published_head() {
        let fake = FakeGithub::new();
        let base_tree = digest(1);
        let base = CandidateRef { tree: base_tree, checkout: fake.seed_base_commit(&base_tree) };
        let first = pin(&fake, 2, "dependent");
        let second = pin(&fake, 3, "root");
        let generation = digest(4);
        let first_plan = IntegrationAppendPlan {
            bloom: BloomId(digest(5)),
            generation,
            expected_parent: IntegrationHead {
                generation,
                node: digest(6),
                candidate: base,
                plan: digest(7),
                coverage: Vec::new(),
            },
            inputs: vec![CompositionInput {
                node: digest(8),
                candidate: first.candidate,
                members: vec![first.clone()],
            }],
        };
        let source = source(&fake);

        let AppendResult::Advanced(first_head) = append_plan(&source, &first_plan) else {
            panic!("the first pinned candidate should append")
        };
        let second_plan = IntegrationAppendPlan {
            bloom: first_plan.bloom,
            generation,
            expected_parent: first_head,
            inputs: vec![CompositionInput {
                node: digest(9),
                candidate: second.candidate,
                members: vec![second.clone()],
            }],
        };
        let AppendResult::Advanced(head) = append_plan(&source, &second_plan) else {
            panic!("the next pinned candidate should append from the admitted head")
        };
        let minted = fake.create_commit_count();
        let AppendResult::Advanced(replayed) = append_plan(&source, &second_plan) else {
            panic!("the exact append plan should replay")
        };

        assert_eq!(head.coverage, vec![first, second], "coverage preserves the plan's actual append order");
        assert_eq!(replayed, head, "the private generation branch recovers the exact published head");
        assert_eq!(fake.create_commit_count(), minted, "replay mints no replacement merge commits");
    }

    #[test]
    fn append_refuses_multiple_atomic_inputs_before_writing_source() {
        let fake = FakeGithub::new();
        let base_tree = digest(30);
        let base = CandidateRef { tree: base_tree, checkout: fake.seed_base_commit(&base_tree) };
        let first = pin(&fake, 31, "first");
        let second = pin(&fake, 32, "second");
        let generation = digest(33);
        let plan = IntegrationAppendPlan {
            bloom: BloomId(digest(34)),
            generation,
            expected_parent: IntegrationHead {
                generation,
                node: digest(35),
                candidate: base,
                plan: digest(36),
                coverage: Vec::new(),
            },
            inputs: vec![
                CompositionInput { node: digest(37), candidate: first.candidate, members: vec![first] },
                CompositionInput { node: digest(38), candidate: second.candidate, members: vec![second] },
            ],
        };
        let commits = fake.create_commit_count();

        assert!(matches!(append_plan(&source(&fake), &plan), AppendResult::Refused(_)));
        assert_eq!(fake.create_commit_count(), commits, "malformed append performs no source write");
    }

    #[test]
    fn append_refuses_to_replace_a_covered_member_version() {
        let fake = FakeGithub::new();
        let base_tree = digest(10);
        let base = CandidateRef { tree: base_tree, checkout: fake.seed_base_commit(&base_tree) };
        let original = pin(&fake, 11, "member");
        let replacement = pin(&fake, 12, "member");
        let generation = digest(13);
        let plan = IntegrationAppendPlan {
            bloom: BloomId(digest(14)),
            generation,
            expected_parent: IntegrationHead {
                generation,
                node: digest(15),
                candidate: base,
                plan: digest(16),
                coverage: vec![original],
            },
            inputs: vec![CompositionInput {
                node: digest(17),
                candidate: replacement.candidate,
                members: vec![replacement],
            }],
        };

        assert!(matches!(append_plan(&source(&fake), &plan), AppendResult::Refused(_)));
    }
}
