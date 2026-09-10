//! Host materialization for one exact partial-head repair attempt.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use aether_bloomery::{
    Digest, MemberPin, PartialHeadRepairDispatch, ScopeRevision, SurfacePattern, WorkpieceId, surface_union,
};

use crate::store::{CommissionBackend, CommissionError};

const MAX_INPUTS: usize = 64;
const MAX_MEMBERS: usize = 256;
const MAX_SURFACE_GLOBS: usize = 512;
const MAX_SURFACE_BYTES: usize = 64 * 1024;
const MAX_FINDINGS_BYTES: usize = 64 * 1024;
const MAX_TASK_BYTES: usize = 128 * 1024;

/// Frozen instructions derived from the exact approved repair surface.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct PartialRepairTask {
    pub(super) description: String,
}

/// A permanent refusal to materialize or admit a paid repair attempt.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum PartialRepairTaskError {
    MissingEvidence,
    MissingFindings,
    FindingsTooLarge,
    InvalidTransformation,
    MissingInputs,
    TooManyInputs,
    MissingMembers { input: usize },
    TooManyMembers,
    ConflictingMemberVersion { workpiece: WorkpieceId },
    CoverageMismatch,
    MissingRevision { workpiece: WorkpieceId, scope_revision: Digest },
    RevisionOwnerMismatch { expected: WorkpieceId, actual: WorkpieceId },
    MissingApproval { workpiece: WorkpieceId, scope_revision: Digest },
    EmptySurface { workpiece: WorkpieceId },
    InvalidSurface { workpiece: WorkpieceId, glob: String },
    SurfaceTooLarge,
    TaskTooLarge,
    Store(String),
}

impl fmt::Display for PartialRepairTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for PartialRepairTaskError {}

impl From<CommissionError> for PartialRepairTaskError {
    fn from(error: CommissionError) -> Self {
        Self::Store(error.to_string())
    }
}

/// Read the exact approved member revisions, freeze a bounded repair task, and
/// return the derived union which must gate candidate retention.
pub(super) fn materialize_partial_repair_task(
    store: &mut impl CommissionBackend,
    dispatch: &PartialHeadRepairDispatch,
    overlaid_task: &str,
) -> Result<PartialRepairTask, PartialRepairTaskError> {
    let findings = extract_findings(overlaid_task)?;
    validate_dispatch(dispatch, findings)?;
    let surface = derive_partial_repair_surface(store, dispatch)?;
    let description = render_task(dispatch, findings, &surface)?;
    Ok(PartialRepairTask { description })
}

fn extract_findings(overlaid_task: &str) -> Result<&str, PartialRepairTaskError> {
    let Some((_, findings)) = overlaid_task.split_once("## Findings") else {
        return Err(PartialRepairTaskError::MissingFindings);
    };
    let findings = findings.trim();
    if findings.is_empty() {
        return Err(PartialRepairTaskError::MissingFindings);
    }
    Ok(findings)
}

/// Re-read the immutable scope pins when a completed candidate is about to be
/// retained. This keeps restart recovery from trusting an in-memory surface.
pub(super) fn derive_partial_repair_surface(
    store: &mut impl CommissionBackend,
    dispatch: &PartialHeadRepairDispatch,
) -> Result<Vec<String>, PartialRepairTaskError> {
    let members = flattened_members(dispatch)?;
    let revisions = load_approved_revisions(store, &members)?;
    bounded_surface(&revisions)
}

fn validate_dispatch(dispatch: &PartialHeadRepairDispatch, findings: &str) -> Result<(), PartialRepairTaskError> {
    if dispatch.plan.evidence == Digest::default() {
        return Err(PartialRepairTaskError::MissingEvidence);
    }
    if findings.trim().is_empty() {
        return Err(PartialRepairTaskError::MissingFindings);
    }
    if findings.len() > MAX_FINDINGS_BYTES {
        return Err(PartialRepairTaskError::FindingsTooLarge);
    }
    if dispatch.plan.head.generation != dispatch.plan.generation
        || dispatch.plan.head.candidate.tree == Digest::default()
        || dispatch.plan.head.candidate.checkout == Digest::default()
        || dispatch.transformation.inputs.first() != Some(&dispatch.plan.head.candidate.tree)
        || dispatch.transformation.checkout != dispatch.plan.head.candidate.checkout
        || dispatch.transformation.diff_base.is_none_or(|base| base == Digest::default())
    {
        return Err(PartialRepairTaskError::InvalidTransformation);
    }
    if dispatch.plan.inputs.is_empty() {
        return Err(PartialRepairTaskError::MissingInputs);
    }
    if dispatch.plan.inputs.len() > MAX_INPUTS {
        return Err(PartialRepairTaskError::TooManyInputs);
    }
    Ok(())
}

fn flattened_members(dispatch: &PartialHeadRepairDispatch) -> Result<Vec<MemberPin>, PartialRepairTaskError> {
    let mut members = Vec::new();
    let mut positions = BTreeMap::<WorkpieceId, usize>::new();
    for (input_index, input) in dispatch.plan.inputs.iter().enumerate() {
        if input.node == Digest::default()
            || input.candidate.tree == Digest::default()
            || input.candidate.checkout == Digest::default()
            || input.members.is_empty()
        {
            return Err(PartialRepairTaskError::MissingMembers { input: input_index });
        }
        for member in &input.members {
            if let Some(index) = positions.get(&member.workpiece).copied() {
                if members[index] != *member {
                    return Err(PartialRepairTaskError::ConflictingMemberVersion {
                        workpiece: member.workpiece.clone(),
                    });
                }
                continue;
            }
            if members.len() == MAX_MEMBERS {
                return Err(PartialRepairTaskError::TooManyMembers);
            }
            positions.insert(member.workpiece.clone(), members.len());
            members.push(member.clone());
        }
    }
    if members != dispatch.plan.head.coverage {
        return Err(PartialRepairTaskError::CoverageMismatch);
    }
    Ok(members)
}

fn load_approved_revisions(
    store: &mut impl CommissionBackend,
    members: &[MemberPin],
) -> Result<Vec<ScopeRevision>, PartialRepairTaskError> {
    let mut revisions = Vec::with_capacity(members.len());
    for member in members {
        let Some(revision) = store.load_revision(member.scope_revision)? else {
            return Err(PartialRepairTaskError::MissingRevision {
                workpiece: member.workpiece.clone(),
                scope_revision: member.scope_revision,
            });
        };
        if revision.workpiece != member.workpiece {
            return Err(PartialRepairTaskError::RevisionOwnerMismatch {
                expected: member.workpiece.clone(),
                actual: revision.workpiece,
            });
        }
        if store.load_approvals(member.scope_revision)?.is_empty() {
            return Err(PartialRepairTaskError::MissingApproval {
                workpiece: member.workpiece.clone(),
                scope_revision: member.scope_revision,
            });
        }
        revisions.push(revision);
    }
    Ok(revisions)
}

fn bounded_surface(revisions: &[ScopeRevision]) -> Result<Vec<String>, PartialRepairTaskError> {
    for revision in revisions {
        if revision.declared_surface.is_empty() {
            return Err(PartialRepairTaskError::EmptySurface { workpiece: revision.workpiece.clone() });
        }
        for glob in &revision.declared_surface {
            if SurfacePattern::parse(glob).is_none() {
                return Err(PartialRepairTaskError::InvalidSurface {
                    workpiece: revision.workpiece.clone(),
                    glob: glob.clone(),
                });
            }
        }
    }
    let surfaces = revisions.iter().map(|revision| revision.declared_surface.as_slice()).collect::<Vec<_>>();
    let surface = surface_union(&surfaces);
    if surface.is_empty() {
        return Err(PartialRepairTaskError::EmptySurface { workpiece: WorkpieceId(String::new()) });
    }
    if surface.len() > MAX_SURFACE_GLOBS
        || surface
            .iter()
            .try_fold(0_usize, |total, glob| total.checked_add(glob.len()))
            .is_none_or(|total| total > MAX_SURFACE_BYTES)
    {
        return Err(PartialRepairTaskError::SurfaceTooLarge);
    }
    Ok(surface)
}

fn render_task(
    dispatch: &PartialHeadRepairDispatch,
    findings: &str,
    surface: &[String],
) -> Result<String, PartialRepairTaskError> {
    let mut parents = String::new();
    for (index, input) in dispatch.plan.inputs.iter().enumerate() {
        use std::fmt::Write as _;
        writeln!(
            parents,
            "{}. node `{}`; tree `{}`; checkout `{}`",
            index + 1,
            input.node.to_hex(),
            input.candidate.tree.to_hex(),
            input.candidate.checkout.to_hex()
        )
        .expect("writing to a String is infallible");
        for member in &input.members {
            writeln!(
                parents,
                "   - `{}` revision `{}` candidate `{}` checkout `{}`",
                member.workpiece,
                member.scope_revision.to_hex(),
                member.candidate.tree.to_hex(),
                member.candidate.checkout.to_hex()
            )
            .expect("writing to a String is infallible");
        }
    }
    let surface = surface.iter().map(|glob| format!("- `{glob}`")).collect::<Vec<_>>().join("\n");
    let description = format!(
        "Repair the exact frozen partial integration head. Preserve already completed member work and make only the smallest changes needed to resolve the retained failure.\n\n\
         ## Frozen partial head\n\n\
         Generation: `{}`\n\
         Node: `{}`\n\
         Tree: `{}`\n\
         Checkout: `{}`\n\
         Producing plan: `{}`\n\n\
         ## Failed evidence\n\n\
         Evidence: `{}`\n\n\
         {findings}\n\n\
         ## Frozen parent inputs\n\n\
         {parents}\
         ## Approved union surface\n\n\
         {surface}\n\n\
         ## Task\n\n\
         Repair the failed interaction while preserving the frozen parent contributions. Do not edit any path outside the approved union surface.",
        dispatch.plan.generation.to_hex(),
        dispatch.plan.head.node.to_hex(),
        dispatch.plan.head.candidate.tree.to_hex(),
        dispatch.plan.head.candidate.checkout.to_hex(),
        dispatch.plan.head.plan.to_hex(),
        dispatch.plan.evidence.to_hex(),
    );
    if description.len() > MAX_TASK_BYTES {
        return Err(PartialRepairTaskError::TaskTooLarge);
    }
    Ok(description)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_bloomery::{
        BloomId, CandidateRef, CompositionInput, ConfigRegistry, IntegrationHead, PartialHeadRepairPlan,
        SCOPE_REVISION_SCHEMA, ScopeRouting, StageCatalog, StageId, Transformation,
    };

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn candidate(tree: u8, checkout: u8) -> CandidateRef {
        CandidateRef { tree: digest(tree), checkout: digest(checkout) }
    }

    fn pin(workpiece: &str, scope: u8, tree: u8, checkout: u8) -> MemberPin {
        MemberPin {
            workpiece: WorkpieceId(workpiece.to_owned()),
            scope_revision: digest(scope),
            candidate: candidate(tree, checkout),
        }
    }

    fn dispatch() -> PartialHeadRepairDispatch {
        let first = pin("first", 11, 12, 13);
        let second = pin("second", 21, 22, 23);
        let parent = candidate(31, 32);
        let generation = digest(33);
        let binding = StageCatalog::binding_of(StageId::Refine);
        PartialHeadRepairDispatch {
            plan: PartialHeadRepairPlan {
                bloom: BloomId(digest(34)),
                generation,
                head: IntegrationHead {
                    generation,
                    node: digest(35),
                    candidate: parent,
                    plan: digest(36),
                    coverage: vec![first.clone(), second.clone()],
                },
                inputs: vec![CompositionInput {
                    node: digest(37),
                    candidate: candidate(38, 39),
                    members: vec![first, second],
                }],
                evidence: digest(40),
                attempt: 1,
            },
            transformation: Transformation::for_member_stage(&binding, parent.tree, parent.checkout, digest(41)),
            scope_revision: digest(42),
            profile: binding.profile,
            configs: ConfigRegistry::default(),
        }
    }

    fn revision(workpiece: &str, declared_surface: &[&str]) -> ScopeRevision {
        ScopeRevision {
            schema: SCOPE_REVISION_SCHEMA,
            workpiece: WorkpieceId(workpiece.to_owned()),
            predecessor: None,
            problem: "problem".to_owned(),
            design: "design".to_owned(),
            plan: "plan".to_owned(),
            declared_surface: declared_surface.iter().map(|glob| (*glob).to_owned()).collect(),
            dogfood_brief: String::new(),
            routing: ScopeRouting { size: "s".to_owned(), model: "construct: test".to_owned() },
            dependencies: Vec::new(),
            description: String::new(),
            implements: Vec::new(),
            declared_crates: Vec::new(),
            declared_reads: Vec::new(),
        }
    }

    #[test]
    fn task_freezes_evidence_actual_parent_and_original_member_pins() {
        let dispatch = dispatch();
        let surface = vec!["crates/a/**".to_owned(), "crates/b/**".to_owned()];
        let description = render_task(&dispatch, "gate failed on the composed tree", &surface)
            .expect("the bounded exact repair task renders");

        assert!(description.contains(&dispatch.plan.evidence.to_hex()));
        assert!(description.contains(&dispatch.plan.inputs[0].node.to_hex()));
        assert!(description.contains(&dispatch.plan.inputs[0].candidate.checkout.to_hex()));
        for member in &dispatch.plan.inputs[0].members {
            assert!(description.contains(&member.workpiece.0));
            assert!(description.contains(&member.scope_revision.to_hex()));
        }
        assert!(description.contains("gate failed on the composed tree"));
        assert!(description.contains("crates/a/**"));
        assert!(description.contains("crates/b/**"));
    }

    #[test]
    fn task_refuses_a_union_surface_or_findings_substitution() {
        let mut mismatched = dispatch();
        mismatched.plan.head.coverage.swap(0, 1);
        assert_eq!(flattened_members(&mismatched), Err(PartialRepairTaskError::CoverageMismatch));

        let dispatch = dispatch();
        assert_eq!(validate_dispatch(&dispatch, "  "), Err(PartialRepairTaskError::MissingFindings));
        assert_eq!(extract_findings("standing task"), Err(PartialRepairTaskError::MissingFindings));
        assert_eq!(extract_findings("standing task\n\n## Findings\n\n gate failed "), Ok("gate failed"));
    }

    #[test]
    fn surface_is_the_bounded_union_of_each_original_revision() {
        let first = revision("first", &["crates/a/**", "Cargo.lock"]);
        let second = revision("second", &["crates/a/src/**", "crates/b/**"]);

        assert_eq!(
            bounded_surface(&[first, second]).expect("both original surfaces are valid"),
            vec!["Cargo.lock", "crates/a/**", "crates/b/**"]
        );
        assert!(matches!(
            bounded_surface(&[revision("invalid", &["../outside/**"])]),
            Err(PartialRepairTaskError::InvalidSurface { .. })
        ));
    }
}
