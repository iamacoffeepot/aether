//! Reading the reducer's per-member answers back out of the journal.
//!
//! Two questions share one source and have no other: which members of a bloom
//! actually resolved into it, and whether any bloom ever resolved a given
//! workpiece. Both are folded state rather than an event anyone can grep for —
//! a claim is inherited across a supersession as a *decision*, never re-admitted
//! as a fact — so replaying the journal is the only honest way to ask, and a
//! per-bloom scan for `Fact::Integrate` would silently answer "no" for every
//! member that arrived through a successor.
//!
//! The replay is a whole-journal read, affordable at both call sites: a bloom
//! lands once, and a commission is reopened by hand. Nothing here decides
//! anything — each caller states its own fail-closed direction for an answer it
//! could not get.

use std::collections::BTreeSet;

use aether_bloomery::{BloomId, BloomRecord, Snapshot, WorkpieceId, decode_recorded_decisions, decode_recorded_event};

use super::runtime::{StoreBackend, resolved_configs};

/// Replay the journal into a snapshot, folding each row's *recorded* decisions.
///
/// Recorded rather than recomputed: the decisions column holds what the
/// coordinator applied when the row was written, so a replay of it reconstructs
/// the board that exists rather than the board this binary's reducer would
/// decide today.
///
/// A row that does not decode is skipped with a warning rather than propagated.
/// One unreadable row costs the answer that row's contribution, never the whole
/// read — and every consumer here reads a missing contribution as "did not
/// resolve", which is the recoverable direction at both call sites.
pub fn replay_snapshot(store: &mut dyn StoreBackend) -> rusqlite::Result<Snapshot> {
    let configs = resolved_configs(store)?;
    let mut snapshot = Snapshot::default();
    for record in store.replay_journal()? {
        let Ok(event) = decode_recorded_event(&record.event, record.event_schema.as_deref()) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::store",
                sequence = record.sequence,
                "journal event did not decode; leaving it out of the membership replay",
            );
            continue;
        };
        let Ok(decisions) = decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref())
        else {
            tracing::warn!(
                target: "aether_chassis_bloomery::store",
                sequence = record.sequence,
                "journal decisions did not decode; leaving the row out of the membership replay",
            );
            continue;
        };
        snapshot = snapshot.apply(&event, &decisions, &configs);
    }
    Ok(snapshot)
}

/// The members of `bloom` whose work actually resolved into it.
///
/// `claims` is the reducer's own per-member resolution, inheritance from a
/// superseded predecessor included; `withdrawn` is the one-way exit a member
/// takes when an operator pulls it out of a walking bloom (#5327). A withdrawn
/// member produces no claim and contributes no candidate to the fold, so it is
/// not part of what landed even though it is still named in the sealed spec.
///
/// An unknown bloom answers with the empty set: this crate has no record that
/// anything resolved, and inventing one is the failure this exists to stop.
pub fn resolved_members(store: &mut dyn StoreBackend, bloom: &BloomId) -> rusqlite::Result<BTreeSet<WorkpieceId>> {
    let snapshot = replay_snapshot(store)?;
    let Some(record) = snapshot.blooms.get(bloom) else {
        return Ok(BTreeSet::new());
    };
    Ok(resolved_members_in(record))
}

fn resolved_members_in(record: &BloomRecord) -> BTreeSet<WorkpieceId> {
    let mut resolved: BTreeSet<_> =
        record.claims.keys().filter(|workpiece| !record.withdrawn.contains_key(*workpiece)).cloned().collect();
    let Some(state) = record.coordination.as_deref() else {
        return resolved;
    };
    let head = &state.integration.head;
    if record.resolved_tree != Some(head.candidate.tree) || record.resolved_head != Some(head.candidate.checkout) {
        return resolved;
    }
    resolved.extend(head.coverage.iter().filter_map(|pin| {
        let current = record
            .spec
            .members()
            .iter()
            .any(|member| member.workpiece == pin.workpiece && member.scope_revision == pin.scope_revision);
        let selected_claim = state.claims.get(&pin.workpiece.0).is_some_and(|claim| claim.member == *pin);
        (current && selected_claim && record.has_current_member_resolution(&pin.workpiece))
            .then(|| pin.workpiece.clone())
    }));
    resolved
}

/// The bloom that resolved `workpiece`, when one did.
///
/// The reopen door's guard reads this: a commission whose workpiece some bloom
/// resolved is landed for the ordinary reason, and putting it back in the line
/// would re-run work that is already in mainline. Any bloom counts, not the
/// newest one — a resolution is not undone by a later bloom naming the same
/// workpiece, and choosing between two would need an ordering the snapshot's
/// digest-keyed map does not carry.
pub fn resolving_bloom(store: &mut dyn StoreBackend, workpiece: &WorkpieceId) -> rusqlite::Result<Option<BloomId>> {
    let snapshot = replay_snapshot(store)?;
    Ok(snapshot
        .blooms
        .iter()
        .find_map(|(bloom, record)| resolved_members_in(record).contains(workpiece).then_some(*bloom)))
}

#[cfg(test)]
mod tests {
    use std::iter::once;

    use aether_bloomery::testing::{claim, digest, draft, membership};
    use aether_bloomery::{
        AgentProfile, BloomId, BloomRecord, CandidateRef, CompositionContractTemplate, CompositionInput,
        CompositionPlan, ConfigRegistry, ContextualInvocationTemplate, CoordinationPolicy, CoordinationState, Evidence,
        EvidenceKind, ExecutionLimits, GenerationMember, Harness, MemberContractPin, MemberPin, MemberVerifyOutcome,
        MemberVerifyRequest, NetworkProfile, ReasoningEffort, ResolutionProof, SharedRunMode, SharedRunNode,
        SharedRunPhase, SharedRunPlan, StageId, ToolPolicy, Transformation, VerificationContract, VerificationMode,
        VerificationObligation, VerifyProof, Withdrawal, WithdrawalCause, WorkpieceId,
    };

    use super::resolved_members_in;

    fn profile() -> AgentProfile {
        AgentProfile {
            harness: Harness::Grok,
            model: "membership-test".to_owned(),
            effort: ReasoningEffort::Low,
            tools: ToolPolicy::None,
        }
    }

    fn member_request(
        bloom: BloomId,
        pin: &MemberPin,
        input: CompositionInput,
        base: CandidateRef,
    ) -> (MemberVerifyRequest, CompositionContractTemplate) {
        let profile = profile();
        let configs = ConfigRegistry::default();
        let transformation = Transformation {
            command: "verify.member".to_owned(),
            inputs: vec![pin.candidate.tree],
            checkout: pin.candidate.checkout,
            diff_base: Some(base.checkout),
            outputs: Vec::new(),
            image: "verify".to_owned(),
            limits: ExecutionLimits { wall_clock_secs: 60 },
            network: NetworkProfile::None,
            description: None,
            model: None,
        };
        let template = CompositionContractTemplate {
            gate_set: digest(8),
            gate_identities: vec!["verify.clippy".to_owned()],
            invocation: ContextualInvocationTemplate {
                command: "verify.check".to_owned(),
                extra_inputs: Vec::new(),
                diff_base: Some(base.checkout),
                outputs: Vec::new(),
                image: transformation.image.clone(),
                limits: transformation.limits,
                network: transformation.network,
                description: None,
                model: None,
                profile: profile.clone(),
                configs: configs.clone(),
            },
            environment: digest(9),
            host_class: digest(10),
        };
        (
            MemberVerifyRequest {
                bloom,
                member: pin.clone(),
                input,
                attempt: 0,
                context: None,
                contract: VerificationContract {
                    gate_set: digest(11),
                    obligations: vec![
                        VerificationObligation::Gate { identity: "verify.clippy".to_owned() },
                        VerificationObligation::MemberDelta {
                            scope_revision: pin.scope_revision,
                            candidate: pin.candidate,
                            diff_base: base,
                        },
                    ],
                    diff_base: base,
                    invocation: digest(12),
                    environment: template.environment,
                    host_class: template.host_class,
                },
                transformation,
                profile,
                configs,
            },
            template,
        )
    }

    fn contextual_record() -> (BloomRecord, MemberPin) {
        let spec = draft(1, vec![membership("wp", 2)]).seal();
        let bloom = spec.id();
        let base = CandidateRef { tree: digest(3), checkout: digest(4) };
        let candidate = CandidateRef { tree: digest(5), checkout: digest(6) };
        let pin = MemberPin { workpiece: WorkpieceId("wp".to_owned()), scope_revision: digest(2), candidate };
        let input = CompositionInput { node: digest(7), candidate, members: vec![pin.clone()] };
        let (request, template) = member_request(bloom, &pin, input.clone(), base);
        let mut state = CoordinationState::new(
            CoordinationPolicy {
                verification: VerificationMode::Contextual,
                eager_integration: true,
                max_run_members: 2,
                max_serial_requests: 2,
                max_attribution_probes: 2,
                movement_budget: 1,
                reservation_millis: 1_000,
                host_class: "membership-test".to_owned(),
            },
            template.clone(),
            bloom,
            base,
            vec![GenerationMember { workpiece: pin.workpiece.clone(), scope_revision: pin.scope_revision }],
        );
        let composition = CompositionPlan {
            bloom,
            base: state.integration.head.clone(),
            inputs: vec![input],
            requests: vec![request.clone()],
            contract: template
                .bind(vec![MemberContractPin { request: request.digest(), contract: request.contract.digest() }]),
        };
        let plan = SharedRunPlan {
            mode: SharedRunMode::Contextual,
            requests: vec![request.clone()],
            composition: Some(composition),
            probe_budget: 2,
            execution_attempt: 0,
        };
        let node = SharedRunNode { plan: plan.digest(), candidate, coverage: vec![pin.clone()] };
        let receipt = Evidence { subject: candidate.tree, kind: EvidenceKind::VerificationResult, detail: digest(13) };
        state.integration.head.node = node.digest();
        state.integration.head.candidate = candidate;
        state.integration.head.coverage = node.coverage.clone();
        state.runs.push(aether_bloomery::SharedRunRecord {
            plan: plan.clone(),
            node: Some(node.clone()),
            phase: SharedRunPhase::Terminal,
            stale: false,
            physical_run: Some(digest(14)),
            completed: vec![MemberVerifyOutcome::PassedIn {
                request: request.digest(),
                node: node.digest(),
                receipt: receipt.clone(),
            }],
            unfinished: Vec::new(),
            latencies: Vec::new(),
        });
        state.claims.insert(
            pin.workpiece.0.clone(),
            aether_bloomery::ContextualResolutionClaim {
                member: pin.clone(),
                proof: ResolutionProof::InComposition {
                    node: node.digest(),
                    receipt,
                    plan: plan.digest(),
                    request: request.digest(),
                    contract: plan.composition.as_ref().expect("a contextual plan has a contract").contract.digest(),
                },
            },
        );
        let mut record = BloomRecord::empty(spec);
        record.coordination = Some(Box::new(state));
        record.resolved_tree = Some(candidate.tree);
        record.resolved_head = Some(candidate.checkout);
        (record, pin)
    }

    #[test]
    fn selected_contextual_receipt_members_require_exact_final_root_and_current_claim() {
        let (record, pin) = contextual_record();
        assert_eq!(resolved_members_in(&record), once(pin.workpiece.clone()).collect());

        let mut stale = record.clone();
        stale.resolved_tree = Some(digest(99));
        assert!(resolved_members_in(&stale).is_empty(), "a stale final root does not land selected coverage");

        let mut stale_candidate = record.clone();
        stale_candidate.coordination.as_mut().expect("coordination state").integration.head.coverage[0].candidate =
            CandidateRef { tree: digest(97), checkout: digest(98) };
        assert!(
            resolved_members_in(&stale_candidate).is_empty(),
            "selected coverage cannot borrow a newer claim for the same scope"
        );

        let mut unproved = record.clone();
        unproved.coordination.as_mut().expect("coordination state").runs.clear();
        assert!(
            resolved_members_in(&unproved).is_empty(),
            "selected coverage without an exact retained member claim does not land"
        );

        let mut withdrawn = record;
        withdrawn.withdrawn.insert(
            pin.workpiece.clone(),
            Withdrawal {
                workpiece: pin.workpiece,
                cause: WithdrawalCause::Operator,
                reason: "withdrawn before landing".to_owned(),
                operator: "test".to_owned(),
            },
        );
        assert!(resolved_members_in(&withdrawn).is_empty(), "withdrawn contextual coverage does not land");
    }

    #[test]
    fn separately_verified_final_root_uses_an_exact_standalone_member_claim() {
        let (mut record, pin) = contextual_record();
        let state = record.coordination.as_mut().expect("coordination state");
        state.runs.clear();
        state.claims.insert(
            pin.workpiece.0.clone(),
            aether_bloomery::ContextualResolutionClaim {
                member: pin.clone(),
                proof: ResolutionProof::Standalone(VerifyProof {
                    gate_set: digest(20),
                    stage: StageId::Verify,
                    evidence: Evidence {
                        subject: pin.candidate.tree,
                        kind: EvidenceKind::VerificationResult,
                        detail: digest(21),
                    },
                }),
            },
        );

        assert_eq!(resolved_members_in(&record), once(pin.workpiece).collect());
    }

    #[test]
    fn separately_verified_aggregate_root_retains_its_exact_contextual_member_claim() {
        let (mut record, pin) = contextual_record();
        let state = record.coordination.as_mut().expect("coordination state");
        let final_root = CandidateRef { tree: digest(30), checkout: digest(31) };
        state.integration.head.candidate = final_root;
        assert!(
            state.contextual_aggregate_proof(&state.integration.head).is_none(),
            "the earlier contextual receipt does not prove a later aggregate root"
        );
        record.resolved_tree = Some(final_root.tree);
        record.resolved_head = Some(final_root.checkout);

        assert_eq!(resolved_members_in(&record), once(pin.workpiece).collect());
    }

    #[test]
    fn legacy_resolution_claims_remain_the_membership_source() {
        let spec = draft(1, vec![membership("legacy", 2)]).seal();
        let mut record = BloomRecord::empty(spec);
        record.claims.insert(WorkpieceId("legacy".to_owned()), claim("legacy", 2, 3));

        assert_eq!(resolved_members_in(&record), once(WorkpieceId("legacy".to_owned())).collect());
    }
}
