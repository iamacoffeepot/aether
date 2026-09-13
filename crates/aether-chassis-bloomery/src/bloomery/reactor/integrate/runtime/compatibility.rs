//! Immutable construction-checkpoint compatibility previews.

use std::collections::BTreeSet;

use aether_bloomery::{
    CandidateRef, Checkpoint, CompatibilityPreview, CompatibilityPreviewPlan, CompositionPlan, Digest, Evidence,
    EvidenceKind, IntegrateOutcome, MemberPin, MemberVerifyRequest, SharedRunMode, SharedRunNode, SharedRunPlan,
    SharedRunPreparation, SurfacePattern, VerificationObligation,
};
use aether_bloomery_github::SourceError;

use crate::bloomery::SourceShell;
use crate::bloomery::coordination::compatibility_namespace;
use crate::bloomery::coordination::shared_run_namespace;
use crate::bloomery::verify::out_of_surface;
use crate::store::CommissionBackend;

pub(super) enum PreviewResult {
    Completed(CompatibilityPreview),
    Diagnosed { result: CompatibilityPreview, diagnostic: String, parent: Digest },
    Stopped(String),
}

pub(super) enum SharedPreparationResult {
    Completed(SharedRunPreparation),
    Diagnosed { preparation: SharedRunPreparation, diagnostic: String, parent: Digest },
    Stopped(String),
}

pub(super) fn prepare_shared_run<C: CommissionBackend>(
    source: &SourceShell,
    commissions: &mut C,
    plan: &SharedRunPlan,
) -> SharedPreparationResult {
    let composition = match (plan.mode, plan.composition.as_ref()) {
        (SharedRunMode::Standalone | SharedRunMode::WarmSerial, None) => {
            return SharedPreparationResult::Completed(SharedRunPreparation::Standalone);
        }
        (SharedRunMode::Contextual, Some(composition)) => composition,
        _ => {
            let parent =
                plan.composition.as_ref().map_or_else(Digest::default, |composition| composition.base.candidate.tree);
            return refused_shared(plan, parent, "shared-run mode and composition disagree");
        }
    };
    let (base, coverage) = match validate_shared_composition(source, commissions, plan, composition) {
        Ok(validated) => validated,
        Err((parent, reason)) => return refused_shared(plan, parent, &reason),
    };

    let namespace = shared_run_namespace(plan);
    let position = match source.integration_checkpoint(&namespace, &base.checkout) {
        Ok(position) => position,
        Err(error) => {
            return SharedPreparationResult::Stopped(format!("shared-run bootstrap failed: {error}"));
        }
    };
    if position.checkpoint.tree != base.tree {
        let Some(head) = position.head else {
            return refused_shared(plan, position.checkpoint.tree, "shared-run base checkout does not carry its tree");
        };
        match source.is_fast_forward(&base.checkout, &head) {
            Ok(true) => {}
            Ok(false) => {
                return refused_shared(plan, position.checkpoint.tree, "shared-run namespace is foreign");
            }
            Err(error) => {
                return SharedPreparationResult::Stopped(format!("shared-run ancestry check failed: {error}"));
            }
        }
    }
    let mut expected = Checkpoint { bloom: namespace, tree: position.checkpoint.tree };
    let mut head = position.head;
    for input in &composition.inputs {
        match source.integrate_pinned(&namespace, &input.candidate, &expected) {
            Ok(IntegrateOutcome::Integrated { tree, head: next }) => {
                expected.tree = tree;
                head = Some(next);
            }
            Ok(IntegrateOutcome::Conflict { at, paths, diff, .. }) => {
                let diagnostic = conflict_diagnostic_for("Shared run", plan.digest(), 0, &paths, &diff);
                return refused_shared_with(at, diagnostic);
            }
            Ok(IntegrateOutcome::StaleCheckpoint { actual }) => {
                return refused_shared(
                    plan,
                    actual,
                    &format!("shared-run namespace observed stale tree {}", actual.to_hex()),
                );
            }
            Err(SourceError::Malformed(reason)) => {
                return refused_shared(plan, expected.tree, &reason);
            }
            Err(error) => {
                return SharedPreparationResult::Stopped(format!("shared-run preparation failed: {error}"));
            }
        }
    }
    let Some(head) = head else {
        return refused_shared(plan, expected.tree, "contextual run produced no checkout head");
    };
    SharedPreparationResult::Completed(SharedRunPreparation::Contextual(SharedRunNode {
        plan: plan.digest(),
        candidate: CandidateRef { tree: expected.tree, checkout: head },
        coverage,
    }))
}

fn validate_shared_composition<C: CommissionBackend>(
    source: &SourceShell,
    commissions: &mut C,
    plan: &SharedRunPlan,
    composition: &CompositionPlan,
) -> Result<(CandidateRef, Vec<MemberPin>), (Digest, String)> {
    let base = composition.base.candidate;
    if composition.inputs.is_empty() || composition.requests != plan.requests {
        return Err((base.tree, "contextual run has inconsistent composition inputs".to_owned()));
    }
    let mut input_ids = BTreeSet::new();
    let expected_inputs = composition
        .requests
        .iter()
        .filter_map(|request| input_ids.insert(request.input.digest()).then_some(request.input.clone()))
        .collect::<Vec<_>>();
    if composition.inputs != expected_inputs {
        return Err((base.tree, "contextual run does not preserve its requests' atomic inputs".to_owned()));
    }
    let mut coverage = Vec::new();
    if !extend_coverage(&mut coverage, &composition.base.coverage) {
        return Err((base.tree, "contextual base contains conflicting member versions".to_owned()));
    }
    for input in &composition.inputs {
        if input.members.is_empty() || !extend_coverage(&mut coverage, &input.members) {
            return Err((base.tree, "contextual run replaces a pinned member version".to_owned()));
        }
    }
    for request in &composition.requests {
        if request.bloom != composition.bloom {
            return Err((
                request.member.candidate.tree,
                "contextual member request belongs to another bloom".to_owned(),
            ));
        }
        if !request.input.members.contains(&request.member) {
            return Err((
                request.member.candidate.tree,
                "contextual member request is not carried by its pinned input".to_owned(),
            ));
        }
        if let Err(reason) = validate_member_delta(source, commissions, request) {
            return Err((request.member.candidate.tree, reason));
        }
    }
    Ok((base, coverage))
}

fn validate_member_delta<C: CommissionBackend>(
    source: &SourceShell,
    commissions: &mut C,
    request: &MemberVerifyRequest,
) -> Result<(), String> {
    let member = &request.member;
    let subject_matches = request.transformation.inputs.first() == Some(&member.candidate.tree)
        && request.transformation.checkout == member.candidate.checkout
        && request.transformation.diff_base == Some(request.contract.diff_base.checkout);
    if !subject_matches {
        return Err(format!("member {} transformation does not carry its pinned delta", member.workpiece.0));
    }

    let mut deltas = request.contract.obligations.iter().filter_map(|obligation| match obligation {
        VerificationObligation::MemberDelta { scope_revision, candidate, diff_base } => {
            Some((scope_revision, candidate, diff_base))
        }
        VerificationObligation::Gate { .. } => None,
    });
    let expected = deltas.next();
    if deltas.next().is_some()
        || !matches!(
            expected,
            Some((scope_revision, candidate, diff_base))
                if *scope_revision == member.scope_revision
                    && candidate == &member.candidate
                    && diff_base == &request.contract.diff_base
        )
    {
        return Err(format!("member {} has no unique matching delta obligation", member.workpiece.0));
    }

    let revision = commissions
        .load_revision(member.scope_revision)
        .map_err(|error| format!("member {} scope revision is unreadable: {error}", member.workpiece.0))?
        .ok_or_else(|| format!("member {} scope revision is absent", member.workpiece.0))?;
    if revision.workpiece != member.workpiece {
        return Err(format!("member {} scope revision belongs to another workpiece", member.workpiece.0));
    }
    if revision.declared_surface.is_empty()
        || revision.declared_surface.iter().any(|glob| SurfacePattern::parse(glob).is_none())
    {
        return Err(format!("member {} approved surface is absent or malformed", member.workpiece.0));
    }
    if commissions
        .load_approvals(member.scope_revision)
        .map_err(|error| format!("member {} approval is unreadable: {error}", member.workpiece.0))?
        .is_empty()
    {
        return Err(format!("member {} scope revision has no retained approval", member.workpiece.0));
    }

    let changed = source
        .changed_paths(&request.contract.diff_base, &member.candidate)
        .map_err(|error| format!("member {} exact delta is unavailable: {error}", member.workpiece.0))?;
    let violations = out_of_surface(changed.iter().map(String::as_str), &revision.declared_surface);
    if !violations.is_empty() {
        return Err(format!(
            "member {} changed {} path(s) outside its approved surface: {}",
            member.workpiece.0,
            violations.len(),
            violations.iter().take(32).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(())
}

fn extend_coverage(coverage: &mut Vec<MemberPin>, additions: &[MemberPin]) -> bool {
    for pin in additions {
        match coverage.iter().find(|current| current.workpiece == pin.workpiece) {
            Some(current) if current != pin => return false,
            Some(_) => {}
            None => coverage.push(pin.clone()),
        }
    }
    true
}

fn refused_shared(plan: &SharedRunPlan, parent: Digest, reason: &str) -> SharedPreparationResult {
    refused_shared_with(parent, format!("Shared run {} was refused: {reason}\n", plan.digest().to_hex()))
}

fn refused_shared_with(parent: Digest, diagnostic: String) -> SharedPreparationResult {
    SharedPreparationResult::Diagnosed {
        preparation: SharedRunPreparation::Refused { detail: Digest::of_wire_bytes(diagnostic.as_bytes()) },
        diagnostic,
        parent,
    }
}

/// Merge the plan's exact checkpoint versions in their recorded order. The
/// namespace is private to the plan, so an obsolete preview cannot move a live
/// integration branch and replay can recover a prefix published before its
/// result receipt.
pub(super) fn preview(source: &SourceShell, plan: &CompatibilityPreviewPlan) -> PreviewResult {
    if plan.checkpoints.is_empty()
        || plan
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.bloom != plan.bloom || checkpoint.starting_checkout != plan.base.checkout)
    {
        return refused(plan, "compatibility preview has inconsistent checkpoint inputs");
    }

    let namespace = compatibility_namespace(plan);
    let position = match source.integration_checkpoint(&namespace, &plan.base.checkout) {
        Ok(position) => position,
        Err(error) => {
            return PreviewResult::Stopped(format!("compatibility preview bootstrap failed: {error}"));
        }
    };
    if position.checkpoint.tree != plan.base.tree {
        let Some(head) = position.head else {
            return refused(plan, "compatibility base checkout does not carry its tree");
        };
        match source.is_fast_forward(&plan.base.checkout, &head) {
            Ok(true) => {}
            Ok(false) => {
                return refused(plan, "compatibility preview namespace advanced outside its plan");
            }
            Err(error) => {
                return PreviewResult::Stopped(format!("compatibility preview ancestry check failed: {error}"));
            }
        }
    }

    let mut expected = Checkpoint { bloom: namespace, tree: position.checkpoint.tree };
    for checkpoint in &plan.checkpoints {
        match source.integrate_pinned(&namespace, &checkpoint.candidate, &expected) {
            Ok(IntegrateOutcome::Integrated { tree, .. }) => expected.tree = tree,
            Ok(IntegrateOutcome::Conflict { at, paths, diff, .. }) => {
                let diagnostic = conflict_diagnostic(plan, checkpoint.observation, &paths, &diff);
                let evidence = Evidence {
                    subject: at,
                    kind: EvidenceKind::FoldConflict,
                    detail: Digest::of_wire_bytes(diagnostic.as_bytes()),
                };
                return PreviewResult::Diagnosed {
                    result: CompatibilityPreview::Conflict { evidence: evidence.detail },
                    diagnostic,
                    parent: evidence.subject,
                };
            }
            Ok(IntegrateOutcome::StaleCheckpoint { actual }) => {
                return refused(plan, &format!("compatibility preview observed stale tree {}", actual.to_hex()));
            }
            Err(SourceError::Malformed(reason)) => return refused(plan, &reason),
            Err(error) => {
                return PreviewResult::Stopped(format!("compatibility preview failed: {error}"));
            }
        }
    }
    PreviewResult::Completed(CompatibilityPreview::Clean { tree: expected.tree })
}

fn refused(plan: &CompatibilityPreviewPlan, reason: &str) -> PreviewResult {
    let diagnostic = format!("Compatibility preview {} was refused: {reason}\n", plan.digest().to_hex());
    PreviewResult::Diagnosed {
        result: CompatibilityPreview::Refused { detail: Digest::of_wire_bytes(diagnostic.as_bytes()) },
        diagnostic,
        parent: plan.base.tree,
    }
}

fn conflict_diagnostic(plan: &CompatibilityPreviewPlan, observation: u64, paths: &[String], diff: &str) -> String {
    conflict_diagnostic_for("Compatibility preview", plan.digest(), observation, paths, diff)
}

fn conflict_diagnostic_for(kind: &str, plan: Digest, observation: u64, paths: &[String], diff: &str) -> String {
    use core::fmt::Write;

    let mut diagnostic = format!("{kind} {} found a conflict at observation {observation}.\n", plan.to_hex());
    if !paths.is_empty() {
        diagnostic.push_str("\nConflicting paths:\n");
        for path in paths {
            let _ = writeln!(diagnostic, "- {path}");
        }
    }
    if !diff.trim().is_empty() {
        diagnostic.push_str("\nCheckpoint delta:\n\n");
        diagnostic.push_str(diff.trim());
        diagnostic.push('\n');
    }
    diagnostic
}

#[cfg(test)]
mod tests {
    use core::fmt::Write as _;
    use std::path::Path;
    use std::process::Command;
    use std::slice::from_ref;
    use std::sync::Arc;

    use aether_bloomery::testing::digest;
    use aether_bloomery::{
        AgentProfile, BackendObjectId, BloomId, CompositionContract, CompositionInput, CompositionPlan, ConfigRegistry,
        ContextualInvocationTemplate, Correspondence, ExecutionLimits, FakeKeyProvider, Harness, IntegrationHead,
        MemberContractPin, MemberVerifyRequest, NetworkProfile, Observation, Provenance, ReasoningEffort,
        SCOPE_REVISION_SCHEMA, ScopeRevision, ScopeRouting, Statement, ToolPolicy, Transformation,
        VERIFY_CHECK_COMMAND, VerificationContract, VerificationObligation, WorkpieceId,
    };
    use aether_bloomery_git::command::run_stdin;
    use aether_bloomery_github::fixture::FakeGithub;
    use aether_bloomery_github::{GitDataApi, GitObjectId, GitSource, MainlineRef};

    use super::*;
    use crate::store::{CommissionBackend, RevisionEvidence, SqliteStore};

    struct SourceFixture {
        _root: tempfile::TempDir,
        fake: FakeGithub,
        source: SourceShell,
        base: CandidateRef,
        first: CandidateRef,
        second: CandidateRef,
        prepared: CandidateRef,
    }

    fn git(repo: &Path, args: &[&str], input: &str) -> String {
        let output = run_stdin(repo, args, input).expect("run git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("utf-8 git output").trim().to_owned()
    }

    fn tree(repo: &Path, files: &[(&str, &str)]) -> String {
        let mut entries = String::new();
        for (path, contents) in files {
            let blob = git(repo, &["hash-object", "-w", "--stdin"], contents);
            let _ = writeln!(entries, "100644 blob {blob}\t{path}");
        }
        git(repo, &["mktree"], &entries)
    }

    fn record(fake: &FakeGithub, digest: Digest, object: &str) {
        let object = BackendObjectId::from(GitObjectId::from_hex(object).expect("git object id"));
        Correspondence::record(fake, &digest, &object).expect("record correspondence");
    }

    fn candidate(
        fake: &FakeGithub,
        tree_digest: Digest,
        tree: &str,
        parents: &[String],
        message: &str,
    ) -> CandidateRef {
        let commit = fake.create_commit(message, tree, parents).expect("create commit");
        let checkout = Digest::of_wire_bytes(commit.sha.as_bytes());
        record(fake, tree_digest, tree);
        record(fake, checkout, &commit.sha);
        CandidateRef { tree: tree_digest, checkout }
    }

    fn source_fixture() -> SourceFixture {
        let root = tempfile::tempdir().expect("tempdir");
        let repo = root.path().join("objects.git");
        let status = Command::new("git").args(["init", "--bare", "-b", "main"]).arg(&repo).status().expect("git init");
        assert!(status.success());
        let repo = repo.canonicalize().expect("absolute repository");
        let fake = FakeGithub::new().with_object_repo(repo.clone());
        let base_tree = tree(&repo, &[]);
        let base = candidate(&fake, digest(10), &base_tree, &[], "base");
        let base_sha = fake.resolve_backend_object(&base.checkout).expect("lookup").expect("base object");
        let base_sha = GitObjectId::try_from(base_sha).expect("base git id").to_hex();
        let first_tree = tree(&repo, &[("a.txt", "a")]);
        let first = candidate(&fake, digest(11), &first_tree, from_ref(&base_sha), "first");
        let second_tree = tree(&repo, &[("b.txt", "b")]);
        let second = candidate(&fake, digest(12), &second_tree, from_ref(&base_sha), "second");
        let first_sha = fake.resolve_backend_object(&first.checkout).expect("lookup").expect("first object");
        let first_sha = GitObjectId::try_from(first_sha).expect("first git id").to_hex();
        let prepared_tree = tree(&repo, &[("a.txt", "a"), ("repair.txt", "repair")]);
        let prepared = candidate(&fake, digest(13), &prepared_tree, &[base_sha, first_sha], "prepared reconcile merge");
        let source = SourceShell::new(Arc::new(GitSource::new(
            fake.clone(),
            Arc::new(fake.clone()),
            false,
            MainlineRef::default(),
        )));
        SourceFixture { _root: root, fake, source, base, first, second, prepared }
    }

    fn profile() -> AgentProfile {
        AgentProfile {
            harness: Harness::Codex,
            model: "host".to_owned(),
            effort: ReasoningEffort::Max,
            tools: ToolPolicy::Allow(vec!["read".to_owned()]),
        }
    }

    fn stored_scope(store: &mut SqliteStore, workpiece: &str, surface: &[&str], approved: bool) -> Digest {
        let id = WorkpieceId(workpiece.to_owned());
        let intent = Statement {
            words: format!("test intent for {workpiece}").into_bytes(),
            provenance: Provenance::ObservationAttestation(Observation { source: "test".to_owned() }),
            parents: Vec::new(),
        };
        store.create(&id, &intent).expect("create commission");
        let revision = ScopeRevision {
            schema: SCOPE_REVISION_SCHEMA,
            workpiece: id,
            predecessor: None,
            problem: "problem".to_owned(),
            design: "design".to_owned(),
            plan: "plan".to_owned(),
            declared_surface: surface.iter().map(|path| (*path).to_owned()).collect(),
            dogfood_brief: String::new(),
            routing: ScopeRouting { size: "M".to_owned(), model: "construct: test".to_owned() },
            dependencies: Vec::new(),
            description: String::new(),
            implements: Vec::new(),
            declared_crates: Vec::new(),
            declared_reads: Vec::new(),
        };
        let scope = store.write_revision(&revision, &RevisionEvidence::default()).expect("write revision");
        if approved {
            let approval = Statement {
                words: scope.as_bytes().to_vec(),
                provenance: Provenance::ObservationAttestation(Observation {
                    source: "aether.bloomery.approve_gate:auto-tier".to_owned(),
                }),
                parents: vec![scope],
            };
            store.insert_approval(&approval, &FakeKeyProvider).expect("approve revision");
        }
        scope
    }

    fn approved_scope(store: &mut SqliteStore, workpiece: &str, surface: &[&str]) -> Digest {
        stored_scope(store, workpiece, surface, true)
    }

    fn request(
        bloom: BloomId,
        workpiece: &str,
        scope: Digest,
        base: &CandidateRef,
        candidate: CandidateRef,
    ) -> MemberVerifyRequest {
        let member = MemberPin { workpiece: WorkpieceId(workpiece.to_owned()), scope_revision: scope, candidate };
        let contract = VerificationContract {
            gate_set: digest(20),
            obligations: vec![
                VerificationObligation::Gate { identity: "verify.clippy".to_owned() },
                VerificationObligation::MemberDelta { scope_revision: scope, candidate, diff_base: *base },
            ],
            diff_base: *base,
            invocation: digest(21),
            environment: digest(22),
            host_class: digest(23),
        };
        MemberVerifyRequest {
            bloom,
            member: member.clone(),
            input: CompositionInput { node: candidate.tree, candidate, members: vec![member] },
            attempt: 1,
            context: None,
            contract,
            transformation: Transformation {
                command: "verify.member".to_owned(),
                inputs: vec![candidate.tree],
                checkout: candidate.checkout,
                diff_base: Some(base.checkout),
                outputs: vec!["result-record".to_owned()],
                image: "verify-image".to_owned(),
                limits: ExecutionLimits { wall_clock_secs: 900 },
                network: NetworkProfile::Restricted,
                description: None,
                model: None,
            },
            profile: profile(),
            configs: ConfigRegistry::default(),
        }
    }

    fn plan(bloom: BloomId, base: CandidateRef, requests: Vec<MemberVerifyRequest>) -> SharedRunPlan {
        plan_on(
            IntegrationHead {
                generation: bloom.0,
                node: digest(25),
                candidate: base,
                plan: digest(26),
                coverage: Vec::new(),
            },
            requests,
        )
    }

    fn plan_on(base: IntegrationHead, requests: Vec<MemberVerifyRequest>) -> SharedRunPlan {
        let inputs = requests.iter().map(|request| request.input.clone()).collect();
        let composition = CompositionPlan {
            bloom: requests.first().expect("nonempty contextual plan").bloom,
            inputs,
            requests: requests.clone(),
            contract: CompositionContract {
                gate_set: digest(20),
                gate_identities: vec!["verify.clippy".to_owned()],
                members: requests
                    .iter()
                    .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
                    .collect(),
                invocation: ContextualInvocationTemplate {
                    command: VERIFY_CHECK_COMMAND.to_owned(),
                    extra_inputs: Vec::new(),
                    diff_base: Some(base.candidate.checkout),
                    outputs: vec!["result-record".to_owned()],
                    image: "verify-image".to_owned(),
                    limits: ExecutionLimits { wall_clock_secs: 900 },
                    network: NetworkProfile::Restricted,
                    description: None,
                    model: None,
                    profile: profile(),
                    configs: ConfigRegistry::default(),
                },
                environment: digest(22),
                host_class: digest(23),
            },
            base,
        };
        SharedRunPlan {
            mode: SharedRunMode::Contextual,
            requests,
            composition: Some(composition),
            probe_budget: 2,
            execution_attempt: 0,
        }
    }

    #[test]
    fn each_original_surface_is_checked_before_the_composition_namespace_is_written() {
        let fixture = source_fixture();
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(digest(1));
        let first_scope = approved_scope(&mut store, "first", &["b.txt"]);
        let second_scope = approved_scope(&mut store, "second", &["a.txt"]);
        let requests = vec![
            request(bloom, "first", first_scope, &fixture.base, fixture.first),
            request(bloom, "second", second_scope, &fixture.base, fixture.second),
        ];

        let result = prepare_shared_run(&fixture.source, &mut store, &plan(bloom, fixture.base, requests));

        let SharedPreparationResult::Diagnosed { diagnostic, .. } = result else {
            panic!("the first member's own surface must refuse");
        };
        assert!(diagnostic.contains("a.txt"), "the refusal names the path its own surface omitted: {diagnostic}");
        assert!(
            fixture.fake.list_matching_refs("heads/bloom/").expect("list refs").is_empty(),
            "a union surface must not reach composition source mutation"
        );
    }

    #[test]
    fn shared_preparation_preserves_inherited_head_coverage_before_new_inputs() {
        let fixture = source_fixture();
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(digest(5));
        let scope = approved_scope(&mut store, "second", &["b.txt"]);
        let request = request(bloom, "second", scope, &fixture.base, fixture.second);
        let inherited = MemberPin {
            workpiece: WorkpieceId("inherited".to_owned()),
            scope_revision: digest(80),
            candidate: fixture.first,
        };
        let base = IntegrationHead {
            generation: bloom.0,
            node: digest(81),
            candidate: fixture.base,
            plan: digest(82),
            coverage: vec![inherited.clone()],
        };

        let result = prepare_shared_run(&fixture.source, &mut store, &plan_on(base, vec![request.clone()]));

        let SharedPreparationResult::Completed(SharedRunPreparation::Contextual(node)) = result else {
            panic!("the current pinned head and contribution prepare");
        };
        assert_eq!(node.coverage, vec![inherited, request.member]);
    }

    #[test]
    fn shared_preparation_refuses_a_rewritten_atomic_input_before_source_mutation() {
        let fixture = source_fixture();
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(digest(6));
        let scope = approved_scope(&mut store, "first", &["a.txt"]);
        let request = request(bloom, "first", scope, &fixture.base, fixture.first);
        let mut plan = plan(bloom, fixture.base, vec![request]);
        plan.composition.as_mut().expect("the contextual fixture carries its composition plan").inputs[0].node =
            digest(99);

        let result = prepare_shared_run(&fixture.source, &mut store, &plan);

        let SharedPreparationResult::Diagnosed { diagnostic, .. } = result else {
            panic!("the rewritten request input must refuse");
        };
        assert!(diagnostic.contains("atomic inputs"));
        assert!(
            fixture.fake.list_matching_refs("heads/bloom/").expect("list refs").is_empty(),
            "malformed input identity must not reach source mutation"
        );
    }

    #[test]
    fn a_prepared_merge_candidate_is_checked_against_its_exact_pinned_base() {
        let fixture = source_fixture();
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(digest(2));
        let scope = approved_scope(&mut store, "prepared", &["a.txt", "repair.txt"]);
        let request = request(bloom, "prepared", scope, &fixture.base, fixture.prepared);

        assert_eq!(validate_member_delta(&fixture.source, &mut store, &request), Ok(()));
    }

    #[test]
    fn member_delta_and_transformation_pins_must_match_exactly_once() {
        let fixture = source_fixture();
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(digest(3));
        let scope = approved_scope(&mut store, "pinned", &["a.txt"]);
        let mut request = request(bloom, "pinned", scope, &fixture.base, fixture.first);

        request.contract.obligations.push(VerificationObligation::MemberDelta {
            scope_revision: scope,
            candidate: request.member.candidate,
            diff_base: request.contract.diff_base,
        });
        assert!(
            validate_member_delta(&fixture.source, &mut store, &request)
                .expect_err("duplicate delta")
                .contains("unique matching delta obligation")
        );

        request.contract.obligations.pop();
        request.transformation.checkout = digest(99);
        assert!(
            validate_member_delta(&fixture.source, &mut store, &request)
                .expect_err("mismatched transformation")
                .contains("pinned delta")
        );
    }

    #[test]
    fn containment_refuses_missing_unapproved_malformed_and_unavailable_inputs() {
        let fixture = source_fixture();
        let mut store = SqliteStore::open(":memory:").expect("store");
        let bloom = BloomId(digest(4));

        let missing = request(bloom, "missing", digest(90), &fixture.base, fixture.first);
        assert!(
            validate_member_delta(&fixture.source, &mut store, &missing)
                .expect_err("missing revision")
                .contains("scope revision is absent")
        );

        let unapproved_scope = stored_scope(&mut store, "unapproved", &["a.txt"], false);
        let unapproved = request(bloom, "unapproved", unapproved_scope, &fixture.base, fixture.first);
        assert!(
            validate_member_delta(&fixture.source, &mut store, &unapproved)
                .expect_err("unapproved revision")
                .contains("no retained approval")
        );

        let malformed_scope = approved_scope(&mut store, "malformed", &["src/**/nested"]);
        let malformed = request(bloom, "malformed", malformed_scope, &fixture.base, fixture.first);
        assert!(
            validate_member_delta(&fixture.source, &mut store, &malformed)
                .expect_err("malformed surface")
                .contains("surface is absent or malformed")
        );

        let owner_scope = approved_scope(&mut store, "owner", &["a.txt"]);
        let wrong_workpiece = request(bloom, "borrower", owner_scope, &fixture.base, fixture.first);
        assert!(
            validate_member_delta(&fixture.source, &mut store, &wrong_workpiece)
                .expect_err("wrong workpiece")
                .contains("belongs to another workpiece")
        );

        let unavailable_scope = approved_scope(&mut store, "unavailable", &["a.txt"]);
        let mut unavailable = request(bloom, "unavailable", unavailable_scope, &fixture.base, fixture.first);
        let unknown = CandidateRef { tree: digest(91), checkout: digest(92) };
        unavailable.member.candidate = unknown;
        unavailable.transformation.inputs[0] = unknown.tree;
        unavailable.transformation.checkout = unknown.checkout;
        unavailable.contract.obligations[1] = VerificationObligation::MemberDelta {
            scope_revision: unavailable_scope,
            candidate: unknown,
            diff_base: fixture.base,
        };
        assert!(
            validate_member_delta(&fixture.source, &mut store, &unavailable)
                .expect_err("unavailable exact range")
                .contains("exact delta is unavailable")
        );
    }
}
