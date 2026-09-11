//! Exact-input contextual facts in the existing ADR-0200 ledger.

use aether_bloomery::digest::{ContentAddressed, digest_of};
use aether_bloomery::{CandidateRef, CompositionContract, Digest, MemberPin, SharedRunNode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use super::{
    BatchCheck, ClosureKey, HostClass, ProbeVerdict, ProofResult, ProofSource, RunnerReport, discriminate,
    record_proof_facts,
};
use crate::store::StoreBackend;

/// A real invocation identified by the trusted physical executor. Missing
/// tests are absent, rather than inferred green from another test's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextualRunnerReport {
    pub invocation: Digest,
    /// Exact tested candidate, ordered coverage, and complete contract.
    /// The physical plan's retry ordinal does not change those inputs.
    pub input: ClosureKey,
    pub contract: Digest,
    pub host_class: Digest,
    pub gate: String,
    pub report: RunnerReport,
}

#[derive(Deserialize)]
struct ObservationDocument {
    protocol: u32,
    nonce: Option<String>,
    gate: String,
    invocations: Vec<ObservedInvocation>,
}

#[derive(Deserialize)]
struct ObservationBundle {
    protocol: u32,
    documents: Vec<ObservationDocument>,
}

fn observation_documents(bytes: &[u8]) -> Result<Vec<ObservationDocument>, ContextualFactError> {
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(ContextualFactError::InvalidArtifact);
    }
    let bundle: ObservationBundle = serde_json::from_slice(bytes).map_err(|_| ContextualFactError::InvalidArtifact)?;
    let mut gates = BTreeSet::new();
    if bundle.protocol != 1
        || bundle.documents.len() > 256
        || bundle.documents.iter().any(|document| document.protocol != 1 || !gates.insert(document.gate.clone()))
    {
        return Err(ContextualFactError::InvalidArtifact);
    }
    Ok(bundle.documents)
}

#[derive(Deserialize)]
struct ObservedInvocation {
    invocation: Digest,
    at: Option<String>,
    outcomes: BTreeMap<String, ObservedResult>,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ObservedResult {
    Passed,
    Failed,
    Unknown,
    Infrastructure,
}

#[derive(Serialize)]
struct HostInvocation<'a> {
    nonce: &'a str,
}

impl ContentAddressed for HostInvocation<'_> {
    const DOMAIN: &'static str = "aether.bloomery.contextual_host_invocation.v1";
}

/// Decode one gate's document out of a verifier's additive observation
/// artifact, after the host has bound its file to an actual physical
/// invocation. Old candidate CLIs may omit this artifact; their ordinary gate
/// receipt remains usable, but supplies no new per-test ledger facts.
///
/// Baseline probes inside an umbrella are excluded because they ran another
/// tree. Their own source identity must be admitted separately.
fn reports_for_document(
    document: ObservationDocument,
    expected_nonce: &str,
    expected_gate: &str,
    node: &SharedRunNode,
    contract: &CompositionContract,
) -> Result<Vec<ContextualRunnerReport>, ContextualFactError> {
    if document.protocol != 1
        || document.nonce.as_deref() != Some(expected_nonce)
        || document.gate != expected_gate
        || !declares_gate(contract, expected_gate)
    {
        return Err(ContextualFactError::InputMismatch);
    }
    let mut seen = BTreeSet::new();
    let mut result = None;
    for observed in document.invocations {
        if !seen.insert(observed.invocation) {
            return Err(ContextualFactError::RepeatedInvocation);
        }
        if observed.at.is_some() {
            continue;
        }
        // Internal ordinals are diagnostic data. One host-issued physical
        // step contributes at most one gate report, regardless of how many
        // invocations its artifact claims. Unsealed test names supply no facts.
        if let Some(observed) = observed.outcomes.get(&format!("gate:{expected_gate}")) {
            result = Some(match result {
                Some(previous) if previous != *observed => ObservedResult::Unknown,
                _ => *observed,
            });
        }
    }
    let mut report = RunnerReport::new();
    match result {
        Some(ObservedResult::Passed) => report.insert(format!("gate:{expected_gate}"), ProofResult::Green),
        Some(ObservedResult::Failed) => report.insert(format!("gate:{expected_gate}"), ProofResult::Red),
        Some(ObservedResult::Unknown | ObservedResult::Infrastructure) | None => return Ok(Vec::new()),
    }
    Ok(vec![ContextualRunnerReport {
        invocation: digest_of(&HostInvocation { nonce: expected_nonce }),
        input: contextual_fact_key(node, contract),
        contract: contract.digest(),
        host_class: contract.host_class,
        gate: expected_gate.to_owned(),
        report,
    }])
}

/// Decode the bounded per-gate artifacts from one umbrella execution. Reports
/// retain their gate identity; callers discriminate only matching gates from
/// separate host-issued steps over the same tested inputs. Internal
/// artifact invocation ids never establish that independence.
///
/// # Errors
/// A document is malformed, duplicated, or from another order. Additive gates
/// outside the sealed contract are ignored and cannot supply facts.
pub fn contextual_bundle_reports(
    bytes: &[u8],
    expected_nonce: &str,
    node: &SharedRunNode,
    contract: &CompositionContract,
) -> Result<Vec<ContextualRunnerReport>, ContextualFactError> {
    let mut reports = Vec::new();
    for document in observation_documents(bytes)? {
        if document.nonce.as_deref() != Some(expected_nonce) {
            return Err(ContextualFactError::InputMismatch);
        }
        if !declares_gate(contract, &document.gate) {
            continue;
        }
        let gate = document.gate.clone();
        reports.extend(reports_for_document(document, expected_nonce, &gate, node, contract)?);
    }
    Ok(reports)
}

/// Read only the typed check a real probe sought. Missing and disagreeing
/// observations stay unknown; a build exit code supplies no named test result.
/// This returns an observation, never a proof or a member failure.
///
/// # Errors
/// The bundle is malformed, duplicated, or bound to another physical step.
pub fn observed_probe_verdict(
    bytes: &[u8],
    expected_nonce: &str,
    check: &BatchCheck,
) -> Result<ProbeVerdict, ContextualFactError> {
    let documents = observation_documents(bytes)?;
    if documents.iter().any(|document| document.nonce.as_deref() != Some(expected_nonce)) {
        return Err(ContextualFactError::InputMismatch);
    }
    let Some(document) = documents.into_iter().find(|document| document.gate == check.gate()) else {
        return Ok(ProbeVerdict::Unknown);
    };
    let key = check.observation_key();
    let mut result = None;
    let mut seen = BTreeSet::new();
    for invocation in document.invocations {
        if !seen.insert(invocation.invocation) {
            return Err(ContextualFactError::RepeatedInvocation);
        }
        if invocation.at.is_some() {
            continue;
        }
        let observed = match invocation.outcomes.get(&key) {
            Some(ObservedResult::Passed) => ProbeVerdict::Passed,
            Some(ObservedResult::Failed) => ProbeVerdict::Failed,
            Some(ObservedResult::Unknown) => ProbeVerdict::Unknown,
            Some(ObservedResult::Infrastructure) => ProbeVerdict::Infrastructure,
            None => continue,
        };
        if result.is_some_and(|previous| previous != observed) {
            return Ok(ProbeVerdict::Unknown);
        }
        result = Some(observed);
    }
    Ok(result.unwrap_or(ProbeVerdict::Unknown))
}

fn declares_gate(contract: &CompositionContract, gate: &str) -> bool {
    contract.gate_identities.iter().any(|identity| identity == gate)
}

#[derive(Serialize)]
struct ContextualFactInput<'a> {
    candidate: CandidateRef,
    coverage: &'a [MemberPin],
    contract: Digest,
}

impl ContentAddressed for ContextualFactInput<'_> {
    const DOMAIN: &'static str = "aether.bloomery.contextual_fact_input.v1";
}

/// Exact tested inputs, independent of the physical plan's retry ordinal.
/// Candidate checkout, ordered member coverage, and the complete invocation
/// contract remain pinned; no fact migrates to a parent or another contract.
#[must_use]
pub fn contextual_fact_key(node: &SharedRunNode, contract: &CompositionContract) -> ClosureKey {
    ClosureKey::from_digest(digest_of(&ContextualFactInput {
        candidate: node.candidate,
        coverage: &node.coverage,
        contract: contract.digest(),
    }))
}

/// The exact latest green ledger row satisfying one declared contextual gate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextualProofFactReuse {
    /// The non-empty gate identity declared by the full contract.
    pub gate: String,
    /// The monotonic sequence of the latest exact fact for this gate.
    pub sequence: u64,
    /// The trusted dispatch nonce that produced the fact.
    pub producing_dispatch: String,
    /// The bloom charged for the producing physical work.
    pub producing_bloom: Vec<u8>,
}

/// A complete reusable proof witness for one node, contract, and host class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextualProofReuse {
    /// Digest of the exact composed node, including its candidate and coverage.
    pub node: Digest,
    /// Digest of the complete contextual verification contract.
    pub contract: Digest,
    /// Opaque host-class digest required by the contract.
    pub host_class: Digest,
    /// Latest green witness in contract declaration order, one per gate.
    pub facts: Vec<ContextualProofFactReuse>,
}

/// Find a complete contextual proof without inheriting facts from any parent.
/// Every non-empty declared gate must have a latest exact-input fact, and that
/// latest fact must be green. An empty or repeated gate declaration is not a
/// reusable contract.
///
/// # Errors
/// Reading the proof ledger failed.
pub fn reuse_contextual_proof(
    store: &mut dyn StoreBackend,
    node: &SharedRunNode,
    contract: &CompositionContract,
    host_class: &HostClass,
) -> rusqlite::Result<Option<ContextualProofReuse>> {
    if host_class.digest() != contract.host_class {
        return Ok(None);
    }
    let mut declared = BTreeSet::new();
    if contract.gate_identities.iter().any(|gate| gate.is_empty() || !declared.insert(gate.as_str()))
        || declared.is_empty()
    {
        return Ok(None);
    }

    let key = contextual_fact_key(node, contract);
    let expected =
        contract.gate_identities.iter().map(|gate| (format!("gate:{gate}"), gate.as_str())).collect::<BTreeMap<_, _>>();
    let mut latest: BTreeMap<&str, (u64, bool, ContextualProofFactReuse)> = BTreeMap::new();
    for row in store.list_proof_facts()? {
        if row.closure_key.as_slice() != key.as_bytes().as_slice() || row.host_class != host_class.as_str() {
            continue;
        }
        let Some(gate) = expected.get(&row.test_id) else {
            continue;
        };
        if latest.get(*gate).is_some_and(|(sequence, _, _)| *sequence >= row.sequence) {
            continue;
        }
        latest.insert(
            *gate,
            (
                row.sequence,
                row.result == ProofResult::Green.as_str(),
                ContextualProofFactReuse {
                    gate: (*gate).to_owned(),
                    sequence: row.sequence,
                    producing_dispatch: row.producing_dispatch,
                    producing_bloom: row.producing_bloom,
                },
            ),
        );
    }

    let facts = contract.gate_identities.iter().map(|gate| latest.get(gate.as_str())).collect::<Option<Vec<_>>>();
    let Some(facts) = facts.filter(|facts| facts.iter().all(|(_, green, _)| *green)) else {
        return Ok(None);
    };
    Ok(Some(ContextualProofReuse {
        node: node.digest(),
        contract: contract.digest(),
        host_class: contract.host_class,
        facts: facts.into_iter().map(|(_, _, fact)| fact.clone()).collect(),
    }))
}

/// Record independently observed facts under the exact candidate, ordered
/// coverage, and full contract. The host supplies `class_name` after checking its sealed class
/// digest; a worker's label cannot select a different ledger namespace.
///
/// # Errors
/// The reports do not name these inputs or independent physical invocations,
/// or the existing proof ledger could not persist the accepted facts.
pub fn record_contextual_facts(
    store: &mut dyn StoreBackend,
    node: &SharedRunNode,
    contract: &CompositionContract,
    reports: [&ContextualRunnerReport; 2],
    class_name: &HostClass,
    dispatch: &str,
    bloom: &[u8],
) -> Result<usize, ContextualFactError> {
    if reports[0].invocation == reports[1].invocation {
        return Err(ContextualFactError::RepeatedInvocation);
    }
    if reports[0].gate != reports[1].gate
        || class_name.digest() != contract.host_class
        || reports.iter().any(|report| {
            report.input != contextual_fact_key(node, contract)
                || report.contract != contract.digest()
                || report.host_class != contract.host_class
                || !declares_gate(contract, &report.gate)
                || report.report.outcomes().any(|(check, _)| check != format!("gate:{}", report.gate))
        })
    {
        return Err(ContextualFactError::InputMismatch);
    }
    Ok(record_proof_facts(
        store,
        &ProofSource::Contextual { input: contextual_fact_key(node, contract) },
        &discriminate(&reports[0].report, &reports[1].report),
        class_name,
        dispatch,
        bloom,
    )?)
}

/// A refused fact is not a member verification failure.
#[derive(Debug)]
pub enum ContextualFactError {
    RepeatedInvocation,
    InputMismatch,
    InvalidArtifact,
    Store(rusqlite::Error),
}

impl fmt::Display for ContextualFactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepeatedInvocation => formatter.write_str("one invocation cannot discriminate itself"),
            Self::InputMismatch => formatter.write_str("contextual observations name different tested inputs"),
            Self::InvalidArtifact => {
                formatter.write_str("contextual observations are not a supported bounded artifact")
            }
            Self::Store(error) => write!(formatter, "contextual fact storage: {error}"),
        }
    }
}

impl Error for ContextualFactError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::RepeatedInvocation | Self::InputMismatch | Self::InvalidArtifact => None,
        }
    }
}

impl From<rusqlite::Error> for ContextualFactError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{
        CandidateRef, ConfigRegistry, ContextualInvocationTemplate, ExecutionLimits, MemberContractPin, NetworkProfile,
        StageCatalog, StageId, WorkpieceId,
    };

    use super::*;
    use crate::bloomery::verify::ProofResult;
    use crate::store::{SqliteStore, StoreBackend};

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    fn inputs() -> (SharedRunNode, CompositionContract) {
        (
            SharedRunNode {
                plan: digest(3),
                candidate: CandidateRef { tree: digest(1), checkout: digest(2) },
                coverage: Vec::new(),
            },
            CompositionContract {
                gate_set: digest(4),
                gate_identities: vec!["verify.test".to_owned()],
                members: vec![MemberContractPin { request: digest(5), contract: digest(6) }],
                invocation: ContextualInvocationTemplate {
                    command: "verify.check".to_owned(),
                    extra_inputs: vec![digest(8)],
                    diff_base: Some(digest(9)),
                    outputs: vec!["evidence".to_owned()],
                    image: "native".to_owned(),
                    limits: ExecutionLimits { wall_clock_secs: 60 },
                    network: NetworkProfile::None,
                    description: None,
                    model: None,
                    profile: StageCatalog::binding_of(StageId::AggregateVerify).profile,
                    configs: ConfigRegistry::default(),
                },
                environment: digest(7),
                host_class: HostClass::new("fleet").digest(),
            },
        )
    }

    fn report(node: &SharedRunNode, contract: &CompositionContract, invocation: u8) -> ContextualRunnerReport {
        gate_report(node, contract, "verify.test", invocation, ProofResult::Green)
    }

    fn gate_report(
        node: &SharedRunNode,
        contract: &CompositionContract,
        gate: &str,
        invocation: u8,
        result: ProofResult,
    ) -> ContextualRunnerReport {
        let mut report = RunnerReport::new();
        report.insert(format!("gate:{gate}"), result);
        ContextualRunnerReport {
            invocation: digest(invocation),
            input: contextual_fact_key(node, contract),
            contract: contract.digest(),
            host_class: contract.host_class,
            gate: gate.to_owned(),
            report,
        }
    }

    fn record_gate(
        store: &mut SqliteStore,
        node: &SharedRunNode,
        contract: &CompositionContract,
        gate: &str,
        invocation: u8,
        result: ProofResult,
        dispatch: &str,
    ) {
        let first = gate_report(node, contract, gate, invocation, result);
        let second = gate_report(node, contract, gate, invocation + 1, result);
        assert_eq!(
            record_contextual_facts(
                store,
                node,
                contract,
                [&first, &second],
                &HostClass::new("fleet"),
                dispatch,
                &[invocation; 32],
            )
            .expect("valid contextual fact"),
            1,
        );
    }

    #[test]
    fn complete_latest_green_facts_reuse_the_exact_context_without_writing() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, mut contract) = inputs();
        contract.gate_identities.push("verify.clippy".to_owned());
        record_gate(&mut store, &node, &contract, "verify.test", 10, ProofResult::Green, "test-dispatch");
        record_gate(&mut store, &node, &contract, "verify.clippy", 20, ProofResult::Green, "clippy-dispatch");
        let before = store.list_proof_facts().expect("proof facts read");

        let reuse = reuse_contextual_proof(&mut store, &node, &contract, &HostClass::new("fleet"))
            .expect("proof lookup")
            .expect("all declared gates are green");

        assert_eq!(reuse.node, node.digest());
        assert_eq!(reuse.contract, contract.digest());
        assert_eq!(reuse.host_class, contract.host_class);
        assert_eq!(
            reuse.facts.iter().map(|fact| fact.gate.as_str()).collect::<Vec<_>>(),
            ["verify.test", "verify.clippy"]
        );
        assert_eq!(reuse.facts[0].producing_dispatch, "test-dispatch");
        assert_eq!(reuse.facts[1].producing_dispatch, "clippy-dispatch");
        assert_eq!(store.list_proof_facts().expect("proof facts read"), before);
    }

    #[test]
    fn missing_empty_or_red_gate_sets_are_not_reused() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, mut contract) = inputs();
        contract.gate_identities.push("verify.clippy".to_owned());
        record_gate(&mut store, &node, &contract, "verify.test", 10, ProofResult::Green, "green");
        assert!(
            reuse_contextual_proof(&mut store, &node, &contract, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_none()
        );

        record_gate(&mut store, &node, &contract, "verify.clippy", 20, ProofResult::Red, "red");
        assert!(
            reuse_contextual_proof(&mut store, &node, &contract, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_none()
        );

        let mut empty = contract.clone();
        empty.gate_identities.clear();
        assert!(
            reuse_contextual_proof(&mut store, &node, &empty, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_none()
        );
    }

    #[test]
    fn a_newer_red_fact_supersedes_an_older_green_fact() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, contract) = inputs();
        record_gate(&mut store, &node, &contract, "verify.test", 10, ProofResult::Green, "older-green");
        assert!(
            reuse_contextual_proof(&mut store, &node, &contract, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_some()
        );
        record_gate(&mut store, &node, &contract, "verify.test", 20, ProofResult::Red, "newer-red");
        assert!(
            reuse_contextual_proof(&mut store, &node, &contract, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_none()
        );
    }

    #[test]
    fn a_replayed_recording_cannot_re_date_the_green_a_red_already_superseded() {
        // The restart path re-derives facts from retained receipts, so the same
        // pair is offered to the ledger more than once. Reuse consults the
        // newest sequence per gate: appending a second copy of the older green
        // would put it past the red that replaced it and hand out a proof for a
        // context that is currently failing.
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, contract) = inputs();
        record_gate(&mut store, &node, &contract, "verify.test", 10, ProofResult::Green, "first-run");
        record_gate(&mut store, &node, &contract, "verify.test", 20, ProofResult::Red, "second-run");
        let after_red = store.list_proof_facts().expect("proof facts read");

        let replayed = gate_report(&node, &contract, "verify.test", 10, ProofResult::Green);
        let replayed_peer = gate_report(&node, &contract, "verify.test", 11, ProofResult::Green);
        assert_eq!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&replayed, &replayed_peer],
                &HostClass::new("fleet"),
                "first-run",
                &[10; 32],
            )
            .expect("the replay is accepted, not refused"),
            0,
            "a fact the ledger already holds is not appended again",
        );

        assert_eq!(store.list_proof_facts().expect("proof facts read"), after_red);
        assert!(
            reuse_contextual_proof(&mut store, &node, &contract, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_none(),
            "the newer red still stands after the replay",
        );
    }

    #[test]
    fn proof_reuse_does_not_cross_node_contract_or_host_class() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, contract) = inputs();
        record_gate(&mut store, &node, &contract, "verify.test", 10, ProofResult::Green, "exact");

        let mut other_node = node.clone();
        other_node.candidate.checkout = digest(30);
        assert!(
            reuse_contextual_proof(&mut store, &other_node, &contract, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_none()
        );
        let mut other_contract = contract.clone();
        other_contract.invocation.diff_base = Some(digest(31));
        assert!(
            reuse_contextual_proof(&mut store, &node, &other_contract, &HostClass::new("fleet"))
                .expect("proof lookup")
                .is_none()
        );
        assert!(
            reuse_contextual_proof(&mut store, &node, &contract, &HostClass::new("different"))
                .expect("proof lookup")
                .is_none()
        );
    }

    #[test]
    fn composed_facts_have_one_exact_input_address_without_parent_fanout() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, contract) = inputs();
        let first = report(&node, &contract, 10);
        let second = report(&node, &contract, 11);
        assert_eq!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&first, &second],
                &HostClass::new("fleet"),
                "run",
                &[12; 32],
            )
            .expect("valid contextual fixture"),
            1
        );
        let facts = store.list_proof_facts().expect("valid contextual fixture");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].closure_key, contextual_fact_key(&node, &contract).as_bytes());
        let mut changed = contract.clone();
        changed.invocation.diff_base = Some(digest(42));
        assert_ne!(contextual_fact_key(&node, &contract), contextual_fact_key(&node, &changed));
        let mut replaced_member = contract.clone();
        replaced_member.members[0].request = digest(43);
        assert_ne!(contextual_fact_key(&node, &contract), contextual_fact_key(&node, &replaced_member));
        let mut different_checkout = node.clone();
        different_checkout.candidate.checkout = digest(44);
        assert_ne!(contextual_fact_key(&node, &contract), contextual_fact_key(&different_checkout, &contract));
    }

    #[test]
    fn independent_physical_attempts_discriminate_the_same_tested_input() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, contract) = inputs();
        let mut retry = node.clone();
        retry.plan = digest(70);
        assert_ne!(node.digest(), retry.digest());
        let first = report(&node, &contract, 10);
        let second = report(&retry, &contract, 11);
        assert_eq!(
            record_contextual_facts(
                &mut store,
                &retry,
                &contract,
                [&first, &second],
                &HostClass::new("fleet"),
                "retry",
                &[12; 32],
            )
            .expect("independent exact-input reports"),
            1
        );
        let proof = reuse_contextual_proof(&mut store, &retry, &contract, &HostClass::new("fleet"))
            .expect("proof lookup")
            .expect("the exact input was independently green");
        assert_eq!(proof.node, retry.digest());
    }

    #[test]
    fn fact_inputs_retain_candidate_tree_and_ordered_member_versions() {
        let (mut node, contract) = inputs();
        node.coverage = ["a", "b"]
            .map(|name| MemberPin {
                workpiece: WorkpieceId(name.to_owned()),
                scope_revision: digest(71),
                candidate: node.candidate,
            })
            .to_vec();
        let key = contextual_fact_key(&node, &contract);
        let mut changed = node.clone();
        changed.candidate.tree = digest(72);
        assert_ne!(key, contextual_fact_key(&changed, &contract));
        changed.clone_from(&node);
        changed.coverage.reverse();
        assert_ne!(key, contextual_fact_key(&changed, &contract));
        changed = node;
        changed.coverage[0].scope_revision = digest(73);
        assert_ne!(key, contextual_fact_key(&changed, &contract));
    }

    #[test]
    fn duplicated_or_rebound_reports_cannot_mint_facts() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, contract) = inputs();
        let first = report(&node, &contract, 10);
        let mut second = first.clone();
        assert!(matches!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&first, &second],
                &HostClass::new("fleet"),
                "run",
                &[12; 32],
            ),
            Err(ContextualFactError::RepeatedInvocation)
        ));
        second.invocation = digest(11);
        second.input = ClosureKey::from_digest(digest(99));
        assert!(matches!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&first, &second],
                &HostClass::new("fleet"),
                "run",
                &[12; 32],
            ),
            Err(ContextualFactError::InputMismatch)
        ));
        assert!(store.list_proof_facts().expect("valid contextual fixture").is_empty());
    }

    fn bundle(document: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "protocol": 1, "documents": [document] }))
            .expect("valid contextual fixture")
    }

    #[test]
    fn baseline_and_unknown_observations_do_not_become_candidate_facts() {
        let (node, contract) = inputs();
        let artifact = serde_json::json!({
            "protocol": 1, "nonce": "physical-step", "gate": "verify.test",
            "invocations": [
                {"invocation": digest(11), "at": null, "outcomes": {"gate:verify.test": "passed", "unsealed": "passed", "not_run": "unknown"}},
                {"invocation": digest(12), "at": "inherited-head", "outcomes": {"base_only": "passed"}}
            ]
        });
        let reports =
            contextual_bundle_reports(&bundle(&artifact), "physical-step", &node, &contract).expect("bound artifact");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].report.outcomes().collect::<Vec<_>>(), [("gate:verify.test", ProofResult::Green)]);
        assert!(contextual_bundle_reports(&bundle(&artifact), "another-step", &node, &contract).is_err());
    }

    #[test]
    fn artifact_ordinals_cannot_discriminate_one_physical_step() {
        let (node, contract) = inputs();
        let mut artifact = serde_json::json!({
            "protocol": 1, "nonce": "first-step", "gate": "verify.test",
            "invocations": [
                {"invocation": digest(11), "at": null, "outcomes": {"gate:verify.test": "passed"}},
                {"invocation": digest(12), "at": null, "outcomes": {"gate:verify.test": "passed"}}
            ]
        });
        let first =
            contextual_bundle_reports(&bundle(&artifact), "first-step", &node, &contract).expect("bound artifact");
        assert_eq!(first.len(), 1);
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        assert!(matches!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&first[0], &first[0]],
                &HostClass::new("fleet"),
                "first-step",
                &[12; 32],
            ),
            Err(ContextualFactError::RepeatedInvocation)
        ));
        assert!(store.list_proof_facts().expect("valid contextual fixture").is_empty());

        // Only a separately issued physical nonce can supply the second
        // report. The candidate's internal ids can even be identical.
        artifact["nonce"] = "second-step".into();
        let second =
            contextual_bundle_reports(&bundle(&artifact), "second-step", &node, &contract).expect("bound artifact");
        assert_ne!(first[0].invocation, second[0].invocation);
        assert_eq!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&first[0], &second[0]],
                &HostClass::new("fleet"),
                "second-step",
                &[12; 32],
            )
            .expect("valid contextual fixture"),
            1,
        );
    }

    #[test]
    fn disagreeing_internal_observations_supply_no_gate_fact() {
        let (node, contract) = inputs();
        let artifact = serde_json::json!({
            "protocol": 1, "nonce": "step", "gate": "verify.test",
            "invocations": [
                {"invocation": digest(11), "at": null, "outcomes": {"gate:verify.test": "passed"}},
                {"invocation": digest(12), "at": null, "outcomes": {"gate:verify.test": "failed"}},
                {"invocation": digest(13), "at": null, "outcomes": {"gate:verify.test": "passed"}}
            ]
        });
        assert!(
            contextual_bundle_reports(&bundle(&artifact), "step", &node, &contract).expect("bound artifact").is_empty()
        );
    }

    #[test]
    fn unsealed_checks_and_mismatched_host_names_cannot_mint_facts() {
        let mut store = SqliteStore::open(":memory:").expect("valid contextual fixture");
        let (node, contract) = inputs();
        let first = report(&node, &contract, 10);
        let mut second = report(&node, &contract, 11);
        assert!(matches!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&first, &second],
                &HostClass::new("different"),
                "run",
                &[12; 32]
            ),
            Err(ContextualFactError::InputMismatch)
        ));
        second.report.insert("gate:unsealed", ProofResult::Green);
        assert!(matches!(
            record_contextual_facts(
                &mut store,
                &node,
                &contract,
                [&first, &second],
                &HostClass::new("fleet"),
                "run",
                &[12; 32]
            ),
            Err(ContextualFactError::InputMismatch)
        ));
        assert!(store.list_proof_facts().expect("valid contextual fixture").is_empty());
    }

    #[test]
    fn a_gate_failure_does_not_claim_an_unreached_test_failed() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "protocol": 1,
            "documents": [{
                "protocol": 1, "nonce": "step", "gate": "verify.test",
                "invocations": [{
                    "invocation": digest(11), "at": null,
                    "outcomes": {"gate:verify.test": "failed"}
                }]
            }]
        }))
        .expect("valid contextual fixture");
        assert_eq!(
            observed_probe_verdict(&bytes, "step", &BatchCheck::Gate { id: "verify.test".to_owned() })
                .expect("valid contextual fixture"),
            ProbeVerdict::Failed
        );
        assert_eq!(
            observed_probe_verdict(
                &bytes,
                "step",
                &BatchCheck::Test { gate: "verify.test".to_owned(), id: "not-reached".to_owned() }
            )
            .expect("valid contextual fixture"),
            ProbeVerdict::Unknown
        );
        assert!(
            observed_probe_verdict(&bytes, "another-step", &BatchCheck::Gate { id: "verify.test".to_owned() }).is_err()
        );
    }

    #[test]
    fn bundles_keep_gates_separate_and_ignore_internal_baselines() {
        let (node, mut contract) = inputs();
        contract.gate_identities.push("verify.clippy".to_owned());
        let bytes = serde_json::to_vec(&serde_json::json!({
            "protocol": 1,
            "documents": [
                {"protocol": 1, "nonce": "step", "gate": "verify.test", "invocations": [
                    {"invocation": digest(11), "at": null, "outcomes": {"gate:verify.test": "passed"}},
                    {"invocation": digest(12), "at": "base", "outcomes": {"gate:verify.test": "failed"}}
                ]},
                {"protocol": 1, "nonce": "step", "gate": "verify.clippy", "invocations": [
                    {"invocation": digest(13), "at": null, "outcomes": {"gate:verify.clippy": "failed"}}
                ]}
            ]
        }))
        .expect("valid contextual fixture");
        let reports = contextual_bundle_reports(&bytes, "step", &node, &contract).expect("valid contextual fixture");
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].gate, "verify.test");
        assert_eq!(reports[1].gate, "verify.clippy");
        assert_eq!(
            observed_probe_verdict(&bytes, "step", &BatchCheck::Gate { id: "verify.test".to_owned() })
                .expect("valid contextual fixture"),
            ProbeVerdict::Passed
        );
    }

    #[test]
    fn an_additive_gate_cannot_mint_or_suppress_sealed_facts() {
        let (node, contract) = inputs();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "protocol": 1,
            "documents": [
                {"protocol": 1, "nonce": "step", "gate": "verify.test", "invocations": [
                    {"invocation": digest(11), "at": null, "outcomes": {"gate:verify.test": "passed"}}
                ]},
                {"protocol": 1, "nonce": "step", "gate": "verify.future", "invocations": [
                    {"invocation": digest(12), "at": null, "outcomes": {"gate:verify.future": "passed"}}
                ]}
            ]
        }))
        .expect("valid contextual fixture");
        let reports = contextual_bundle_reports(&bytes, "step", &node, &contract).expect("valid contextual fixture");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].gate, "verify.test");
    }
}
