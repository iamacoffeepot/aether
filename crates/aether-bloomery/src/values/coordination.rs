//! Durable vocabulary for shared member verification and eager integration.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{BloomId, Nonce, WorkpieceId};
use crate::values::{
    AgentProfile, CandidateRef, ConfigRegistry, Evidence, ExecutionLimits, NetworkProfile, ResolvedModel,
    Transformation, VerifyFailureSet, VerifyProof,
};

#[derive(Serialize)]
struct HostClassName<'a>(&'a str);

impl ContentAddressed for HostClassName<'_> {
    const DOMAIN: &'static str = "aether.bloomery.host_class.v1";
}

#[derive(Serialize)]
struct VerificationEnvironment<'a> {
    image: &'a str,
    network: NetworkProfile,
    host_class: Digest,
}

#[derive(Serialize)]
struct ConstructionNonce<'a>(&'a str);

impl ContentAddressed for ConstructionNonce<'_> {
    const DOMAIN: &'static str = "aether.bloomery.construction_nonce.v1";
}

impl ContentAddressed for VerificationEnvironment<'_> {
    const DOMAIN: &'static str = "aether.bloomery.verification_environment.v1";
}

/// Canonical identity of an explicitly sealed executor class.
#[must_use]
pub fn host_class_digest(name: &str) -> Digest {
    digest_of(&HostClassName(name))
}

/// Canonical execution-environment identity used by both member and
/// composition verification contracts.
#[must_use]
pub fn verification_environment_digest(image: &str, network: NetworkProfile, host_class: Digest) -> Digest {
    digest_of(&VerificationEnvironment { image, network, host_class })
}

/// Canonical journal identity of the executor nonce assigned at construction
/// admission. Checkpoints carry this digest without exposing host nonce text.
#[must_use]
pub fn construction_nonce_digest(nonce: &Nonce) -> Digest {
    digest_of(&ConstructionNonce(&nonce.0))
}

/// How a policy-enabled bloom verifies member candidates.
#[derive(aether_data::Schema, Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum VerificationMode {
    /// Preserve one physical verification run per member.
    #[default]
    Standalone,
    /// Run standalone member requests serially while retaining one warm slot.
    WarmSerial,
    /// Verify several member contributions in one immutable composition.
    Contextual,
}

/// Optional sealed policy for eager integration and shared verification.
///
/// Absence preserves the legacy reducer. The initial policy never waits for an
/// arrival: bounds limit work already ready when the scheduler is asked.
#[aether_data::kind(name = "aether.bloomery.coordination_policy", default, eq)]
pub struct CoordinationPolicy {
    /// Member verification strategy.
    pub verification: VerificationMode,
    /// Whether verified contributions advance an immutable partial head.
    pub eager_integration: bool,
    /// Maximum logical requests admitted to one physical run or warm lease.
    pub max_run_members: u32,
    /// Maximum requests executed before a warm lane is released and admission
    /// is reassessed.
    pub max_serial_requests: u32,
    /// Maximum dependency-preserving attribution probes for one red node.
    pub max_attribution_probes: u32,
    /// Head movements tolerated in one repair episode before reservation.
    pub movement_budget: u32,
    /// Duration of a stable-head reservation.
    pub reservation_millis: u64,
    /// Explicit execution class every contextual proof must run on.
    pub host_class: String,
}

impl CoordinationPolicy {
    /// Whether every resource/liveness bound can make progress.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.max_run_members > 0
            && self.max_serial_requests > 0
            && (self.verification != VerificationMode::Contextual || self.max_attribution_probes > 0)
            && self.movement_budget > 0
            && self.reservation_millis > 0
            && !self.host_class.is_empty()
            && self.host_class.len() <= 128
            && self.host_class.bytes().all(|byte| byte.is_ascii_graphic())
    }
}

/// One immutable member version participating in a head or verification node.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberPin {
    pub workpiece: WorkpieceId,
    pub scope_revision: Digest,
    pub candidate: CandidateRef,
}

/// One immutable composition contribution.
///
/// `members` is the complete transitive coverage carried by `candidate`; it is
/// what prevents an ejected ancestor from surviving invisibly through a
/// dependent candidate's Git ancestry.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompositionInput {
    pub node: Digest,
    pub candidate: CandidateRef,
    pub members: Vec<MemberPin>,
}

impl ContentAddressed for CompositionInput {
    const DOMAIN: &'static str = "aether.bloomery.composition_input";
}

impl CompositionInput {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// One member's pinned revision inside an eager-integration generation.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct GenerationMember {
    pub workpiece: WorkpieceId,
    pub scope_revision: Digest,
}

/// Membership and base identity of one eager-integration generation.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct IntegrationGeneration {
    pub bloom: BloomId,
    pub base: CandidateRef,
    /// Active member revisions in sealed member order.
    pub members: Vec<GenerationMember>,
    /// Version epoch, advanced when an included contribution is replaced.
    pub epoch: u32,
}

impl ContentAddressed for IntegrationGeneration {
    const DOMAIN: &'static str = "aether.bloomery.integration_generation";
}

impl IntegrationGeneration {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// One immutable partial integration head.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct IntegrationHead {
    pub generation: Digest,
    pub node: Digest,
    pub candidate: CandidateRef,
    /// Digest of the append plan which produced this head.
    pub plan: Digest,
    /// Exact actual append order, flattened to current member versions.
    pub coverage: Vec<MemberPin>,
}

impl ContentAddressed for IntegrationHead {
    const DOMAIN: &'static str = "aether.bloomery.integration_head";
}

impl IntegrationHead {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }

    #[must_use]
    pub fn covers(&self, pin: &MemberPin) -> bool {
        self.coverage.contains(pin)
    }
}

/// Persisted source operation advancing one generation from one exact parent.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct IntegrationAppendPlan {
    pub bloom: BloomId,
    pub generation: Digest,
    pub expected_parent: IntegrationHead,
    /// Dependency-valid inputs in the actual order the source must apply.
    pub inputs: Vec<CompositionInput>,
}

impl ContentAddressed for IntegrationAppendPlan {
    const DOMAIN: &'static str = "aether.bloomery.integration_append_plan";
}

impl IntegrationAppendPlan {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// The immutable head a late member starts from.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ConstructContext {
    pub bloom_base: CandidateRef,
    pub starting_head: IntegrationHead,
}

impl ContentAddressed for ConstructContext {
    const DOMAIN: &'static str = "aether.bloomery.construct_context";
}

impl ConstructContext {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Source request that mechanically places an authored reconcile result onto
/// the head its order recorded before Verify can judge it.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CandidatePreparationPlan {
    pub bloom: BloomId,
    pub workpiece: WorkpieceId,
    pub scope_revision: Digest,
    pub authored: CandidateRef,
    pub context: ConstructContext,
}

impl ContentAddressed for CandidatePreparationPlan {
    const DOMAIN: &'static str = "aether.bloomery.candidate_preparation_plan";
}

impl CandidatePreparationPlan {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Candidate admitted after a successful source-side preparation.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PreparedCandidate {
    pub authored: CandidateRef,
    pub candidate: CandidateRef,
    pub context: ConstructContext,
    pub diff_base: CandidateRef,
}

/// Exact member attempt whose checkout inherits an eager integration head.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ContextualAttemptDispatch {
    pub bloom: BloomId,
    pub workpiece: WorkpieceId,
    pub stage: crate::StageId,
    /// Reducer-owned stage attempt, distinguishing a replay of one queued row
    /// from a later legitimate re-entry under otherwise identical inputs.
    pub attempt: u32,
    pub transformation: Transformation,
    pub scope_revision: Digest,
    pub candidate: Option<Digest>,
    pub profile: AgentProfile,
    pub configs: ConfigRegistry,
    pub context: ConstructContext,
}

impl ContentAddressed for ContextualAttemptDispatch {
    const DOMAIN: &'static str = "aether.bloomery.contextual_attempt_dispatch";
}

impl ContextualAttemptDispatch {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Host request to bind one queued construction intent to a physical nonce
/// immediately before backend submission.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ConstructionAdmission {
    pub nonce: Digest,
    pub dispatch: ContextualAttemptDispatch,
}

impl ContentAddressed for ConstructionAdmission {
    const DOMAIN: &'static str = "aether.bloomery.construction_admission";
}

impl ConstructionAdmission {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Version-bound immutable snapshot of a live construction checkout.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ConstructionCheckpoint {
    pub bloom: BloomId,
    pub workpiece: WorkpieceId,
    pub scope_revision: Digest,
    pub nonce: Digest,
    pub observation: u64,
    pub starting_checkout: Digest,
    pub candidate: CandidateRef,
}

impl ContentAddressed for ConstructionCheckpoint {
    const DOMAIN: &'static str = "aether.bloomery.construction_checkpoint";
}

impl ConstructionCheckpoint {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Bounded immutable merge preview over exact construction checkpoint versions.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompatibilityPreviewPlan {
    pub bloom: BloomId,
    pub generation: Digest,
    pub base: CandidateRef,
    pub checkpoints: Vec<ConstructionCheckpoint>,
}

impl ContentAddressed for CompatibilityPreviewPlan {
    const DOMAIN: &'static str = "aether.bloomery.compatibility_preview_plan";
}

impl CompatibilityPreviewPlan {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Provisional version-bound compatibility observation.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CompatibilityPreview {
    Clean { tree: Digest },
    Conflict { evidence: Digest },
    Refused { detail: Digest },
}

/// One retained compatibility preview under its immutable plan identity.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompatibilityPreviewRecord {
    pub plan: Digest,
    pub result: CompatibilityPreview,
}

/// Result of one immutable candidate preparation request.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // aether-suppression-request: the journal retains the exact prepared candidate as a derived wire value, including its inherited context
pub enum CandidatePreparation {
    Prepared(PreparedCandidate),
    Conflict { evidence: Evidence },
    Refused { detail: Digest },
}

/// Exact parent-bound repair of a red partial integration head.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PartialHeadRepairPlan {
    pub bloom: BloomId,
    pub generation: Digest,
    pub head: IntegrationHead,
    /// Atomic contributions already admitted into `head`, in actual order.
    pub inputs: Vec<CompositionInput>,
    pub evidence: Digest,
    pub attempt: u32,
}

impl ContentAddressed for PartialHeadRepairPlan {
    const DOMAIN: &'static str = "aether.bloomery.partial_head_repair_plan";
}

impl PartialHeadRepairPlan {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Model-lane dispatch repairing one exact red partial head.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PartialHeadRepairDispatch {
    pub plan: PartialHeadRepairPlan,
    pub transformation: Transformation,
    /// The bloom base scope used by ordinary composition repair provenance.
    pub scope_revision: Digest,
    pub profile: AgentProfile,
    pub configs: ConfigRegistry,
}

/// Terminal result of one exact partial-head repair attempt.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum PartialHeadRepairCompletion {
    Repaired { candidate: CandidateRef, evidence: Evidence },
    Refused { evidence: Evidence },
    HostFault { evidence: Evidence },
}

/// One sealed verification obligation.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum VerificationObligation {
    Gate { identity: String },
    MemberDelta { scope_revision: Digest, candidate: CandidateRef, diff_base: CandidateRef },
}

/// Complete inputs that determine what one member verification proves.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct VerificationContract {
    pub gate_set: Digest,
    pub obligations: Vec<VerificationObligation>,
    pub diff_base: CandidateRef,
    /// Exact executable invocation, including command, features, and targets.
    pub invocation: Digest,
    /// Toolchain, image, network posture, and other sealed environment inputs.
    pub environment: Digest,
    /// Runner class used by ADR-0200 discrimination.
    pub host_class: Digest,
}

/// One logical request and its exact member contract in a composition.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberContractPin {
    pub request: Digest,
    pub contract: Digest,
}

/// Sealed composition-wide fields which do not depend on the selected members
/// or the source node produced for their run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompositionContractTemplate {
    pub gate_set: Digest,
    pub gate_identities: Vec<String>,
    pub invocation: ContextualInvocationTemplate,
    pub environment: Digest,
    pub host_class: Digest,
}

impl CompositionContractTemplate {
    /// Bind the template to the exact logical member contracts selected for a
    /// run. Their order is the run's sealed request order.
    #[must_use]
    pub fn bind(&self, members: Vec<MemberContractPin>) -> CompositionContract {
        CompositionContract {
            gate_set: self.gate_set,
            gate_identities: self.gate_identities.clone(),
            members,
            invocation: self.invocation.clone(),
            environment: self.environment,
            host_class: self.host_class,
        }
    }

    /// Whether one member request can truthfully be discharged by this
    /// composition-wide invocation. A false result requires conservative
    /// standalone execution.
    #[must_use]
    pub fn covers(&self, request: &MemberVerifyRequest) -> bool {
        request.contract.environment == self.environment
            && request.profile == self.invocation.profile
            && request.configs == self.invocation.configs
            && request.transformation.image == self.invocation.image
            && request.transformation.limits == self.invocation.limits
            && request.transformation.network == self.invocation.network
            && request.contract.obligations.iter().all(|obligation| match obligation {
                VerificationObligation::Gate { identity } => self.gate_identities.contains(identity),
                VerificationObligation::MemberDelta { .. } => true,
            })
    }
}

/// Subject-independent invocation fields sealed before source preparation.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ContextualInvocationTemplate {
    pub command: String,
    pub extra_inputs: Vec<Digest>,
    pub diff_base: Option<Digest>,
    pub outputs: Vec<String>,
    pub image: String,
    pub limits: ExecutionLimits,
    pub network: NetworkProfile,
    pub description: Option<String>,
    pub model: Option<ResolvedModel>,
    pub profile: AgentProfile,
    pub configs: ConfigRegistry,
}

impl ContextualInvocationTemplate {
    /// Bind the template to the exact immutable source node.
    #[must_use]
    pub fn instantiate(&self, candidate: CandidateRef) -> Transformation {
        let mut inputs = Vec::with_capacity(self.extra_inputs.len() + 1);
        inputs.push(candidate.tree);
        inputs.extend_from_slice(&self.extra_inputs);
        Transformation {
            command: self.command.clone(),
            inputs,
            checkout: candidate.checkout,
            diff_base: self.diff_base,
            outputs: self.outputs.clone(),
            image: self.image.clone(),
            limits: self.limits,
            network: self.network,
            description: self.description.clone(),
            model: self.model.clone(),
        }
    }

    /// Whether an executable dispatch is this template bound to `candidate`.
    #[must_use]
    pub fn matches(&self, transformation: &Transformation, candidate: CandidateRef) -> bool {
        transformation == &self.instantiate(candidate)
    }
}

/// Complete group execution contract for one contextual node.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompositionContract {
    pub gate_set: Digest,
    pub gate_identities: Vec<String>,
    pub members: Vec<MemberContractPin>,
    pub invocation: ContextualInvocationTemplate,
    pub environment: Digest,
    pub host_class: Digest,
}

impl ContentAddressed for CompositionContract {
    const DOMAIN: &'static str = "aether.bloomery.composition_contract.v1";
}

impl CompositionContract {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

impl ContentAddressed for VerificationContract {
    const DOMAIN: &'static str = "aether.bloomery.verification_contract";
}

impl VerificationContract {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }

    #[must_use]
    pub fn member_delta(&self) -> Option<(&Digest, &CandidateRef, &CandidateRef)> {
        let mut deltas = self.obligations.iter().filter_map(|obligation| match obligation {
            VerificationObligation::MemberDelta { scope_revision, candidate, diff_base } => {
                Some((scope_revision, candidate, diff_base))
            }
            VerificationObligation::Gate { .. } => None,
        });
        let delta = deltas.next()?;
        deltas.next().is_none().then_some(delta)
    }
}

/// One logical member verification request, independent of physical grouping.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberVerifyRequest {
    pub bloom: BloomId,
    pub member: MemberPin,
    /// Atomic source contribution carrying this member. A repaired survivor
    /// group remains one input rather than being flattened by a host scheduler.
    pub input: CompositionInput,
    pub attempt: u32,
    pub context: Option<ConstructContext>,
    pub contract: VerificationContract,
    /// Exact standalone member transformation. Warm serial execution reuses it
    /// unchanged; contextual execution retains it for member-local checks.
    pub transformation: Transformation,
    pub profile: AgentProfile,
    pub configs: ConfigRegistry,
}

impl ContentAddressed for MemberVerifyRequest {
    const DOMAIN: &'static str = "aether.bloomery.member_verify_request";
}

impl MemberVerifyRequest {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Immutable contextual composition selected for a physical run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompositionPlan {
    pub bloom: BloomId,
    /// Exact selected eager head, including generation and coverage identity.
    pub base: IntegrationHead,
    pub inputs: Vec<CompositionInput>,
    pub requests: Vec<MemberVerifyRequest>,
    pub contract: CompositionContract,
}

impl ContentAddressed for CompositionPlan {
    const DOMAIN: &'static str = "aether.bloomery.composition_plan";
}

impl CompositionPlan {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Physical execution shape chosen for ready logical requests.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SharedRunMode {
    Standalone,
    WarmSerial,
    Contextual,
}

/// Reducer-approved immutable physical-run plan.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SharedRunPlan {
    pub mode: SharedRunMode,
    pub requests: Vec<MemberVerifyRequest>,
    pub composition: Option<CompositionPlan>,
    /// Sealed attribution bound; zero outside contextual mode.
    pub probe_budget: u32,
    /// Physical retry generation. Logical request identities and proof
    /// contracts remain unchanged across host-fault retries.
    pub execution_attempt: u32,
}

impl ContentAddressed for SharedRunPlan {
    const DOMAIN: &'static str = "aether.bloomery.shared_run_plan";
}

impl SharedRunPlan {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Immutable node prepared for a contextual run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SharedRunNode {
    pub plan: Digest,
    pub candidate: CandidateRef,
    pub coverage: Vec<MemberPin>,
}

impl ContentAddressed for SharedRunNode {
    const DOMAIN: &'static str = "aether.bloomery.shared_run_node";
}

impl SharedRunNode {
    #[must_use]
    pub fn digest(&self) -> Digest {
        digest_of(self)
    }
}

/// Preparation result for a reducer-approved shared run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SharedRunPreparation {
    /// Standalone/warm runs need no composition node.
    Standalone,
    Contextual(SharedRunNode),
    Refused {
        detail: Digest,
    },
}

/// Exact executor input of one reducer-approved physical run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // aether-suppression-request: the immutable journal dispatch retains plain node and invocation values within the supported Schema vocabulary
pub enum SharedRunExecution {
    /// Execute each request's own transformation serially in one retained slot.
    Serial,
    /// Execute the combined gate once over the immutable contextual node.
    Contextual { node: SharedRunNode, transformation: Transformation, profile: AgentProfile, configs: ConfigRegistry },
}

/// Snapshot-inert payload retained by the physical-run outbox row.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SharedRunDispatch {
    pub plan: SharedRunPlan,
    pub execution: SharedRunExecution,
}

/// Evidence-backed scope of one group failure.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FailureScope {
    Attributed { members: Vec<MemberPin>, evidence: Digest },
    Interaction { members: Vec<MemberPin>, evidence: Digest },
    Inherited { head: Digest, evidence: Digest },
    Unattributed { evidence: Digest },
}

/// One replayable coordination diagnostic bound to its immutable subject.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CoordinationDiagnostic {
    pub subject: Digest,
    pub scope: FailureScope,
}

/// What one logical request learned from a physical run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum MemberVerifyOutcome {
    PassedStandalone {
        request: Digest,
        proof: VerifyProof,
    },
    PassedIn {
        request: Digest,
        node: Digest,
        receipt: Evidence,
    },
    Failed {
        request: Digest,
        scope: FailureScope,
        failures: VerifyFailureSet,
        evidence: Evidence,
    },
    HostFault {
        request: Digest,
        evidence: Evidence,
    },
    /// The batch report proved this request is outside every blocked failure
    /// closure, but did not mint a member proof. It may enter a fresh survivor
    /// composition with the same immutable input.
    Survived {
        request: Digest,
        node: Digest,
        observation: Digest,
    },
    /// The run did not reach or discriminate this request. It must not be
    /// grouped as a proven survivor.
    Pending {
        request: Digest,
        observation: Digest,
    },
}

impl MemberVerifyOutcome {
    #[must_use]
    pub const fn request(&self) -> Digest {
        match self {
            Self::PassedStandalone { request, .. }
            | Self::PassedIn { request, .. }
            | Self::Failed { request, .. }
            | Self::HostFault { request, .. }
            | Self::Survived { request, .. }
            | Self::Pending { request, .. } => *request,
        }
    }
}

/// Terminal or partial report for one physical run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SharedRunCompletion {
    pub plan: Digest,
    pub run: Digest,
    pub outcomes: Vec<MemberVerifyOutcome>,
    /// Requests not reached remain pending and may be regrouped.
    pub unfinished: Vec<Digest>,
    pub latencies: Vec<MemberVerifyLatency>,
}

/// Replayable latency for one exact logical member request.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberVerifyLatency {
    pub request: Digest,
    pub member: MemberPin,
    pub latency_millis: u64,
}

/// Ordered unproved survivors that must enter their next immutable contextual
/// node together. Unknown or unreached requests are never members.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SurvivorGroup {
    pub source_plan: Digest,
    pub source_node: Digest,
    pub requests: Vec<Digest>,
}

/// Proof carried by a current member resolution.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ResolutionProof {
    Standalone(VerifyProof),
    InComposition { node: Digest, receipt: Evidence, plan: Digest, request: Digest, contract: Digest },
}

/// A contextual resolution claim. It deliberately does not impersonate the
/// legacy tree-bound [`crate::ResolutionClaim`].
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ContextualResolutionClaim {
    pub member: MemberPin,
    pub proof: ResolutionProof,
}

/// Durable lifecycle of one physical run.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SharedRunPhase {
    Preparing,
    Ready,
    Running,
    Terminal,
}

/// Reducer-owned state of one physical run.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SharedRunRecord {
    pub plan: SharedRunPlan,
    pub node: Option<SharedRunNode>,
    /// Source preparation and physical execution lifecycle.
    pub phase: SharedRunPhase,
    /// A later member/head generation displaced this plan. It remains retained
    /// only so an already-started physical execution can settle without
    /// affecting current claims.
    pub stale: bool,
    pub physical_run: Option<Digest>,
    pub completed: Vec<MemberVerifyOutcome>,
    pub unfinished: Vec<Digest>,
    pub latencies: Vec<MemberVerifyLatency>,
}

impl SharedRunRecord {
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self.phase, SharedRunPhase::Ready)
    }

    #[must_use]
    pub const fn is_running(&self) -> bool {
        matches!(self.phase, SharedRunPhase::Running)
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self.phase, SharedRunPhase::Terminal)
    }
}

/// Durable reservation after repeated repair displacement.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct StableHeadReservation {
    pub owner: WorkpieceId,
    pub generation: Digest,
    pub node: Digest,
    pub movement_count: u32,
    pub deadline_unix_millis: u64,
    pub hold: Digest,
}

/// Eager integration state for an opted-in bloom.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct EagerIntegrationState {
    pub generation: IntegrationGeneration,
    pub head: IntegrationHead,
    pub queued: Vec<CompositionInput>,
    /// Inputs admitted into the current head, retaining atomic provenance.
    pub admitted: Vec<CompositionInput>,
    pub in_flight: Option<IntegrationAppendPlan>,
    pub known_red: Option<Digest>,
    /// The exact node an accepted partial-head repair produced. It is red until
    /// its own aggregate proof arrives, and that proof is the only pre-check
    /// green that may clear `known_red`: a verdict established anywhere else
    /// stays red until repair and promotion clear it (ADR-0218).
    pub unproved_repair: Option<Digest>,
    pub reservation: Option<StableHeadReservation>,
    /// Head moves in the current repair episode.
    pub movement_count: u32,
}

/// Complete journal-replayable state for an opted-in bloom.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CoordinationState {
    pub policy: CoordinationPolicy,
    /// Reducer-authored composition-wide execution contract. Hosts bind only
    /// the exact selected request contracts; they do not reconstruct policy or
    /// aggregate invocation details from mutable configuration.
    pub composition_contract: CompositionContractTemplate,
    pub integration: EagerIntegrationState,
    pub requests: Vec<MemberVerifyRequest>,
    pub runs: Vec<SharedRunRecord>,
    pub claims: BTreeMap<String, ContextualResolutionClaim>,
    pub prepared: BTreeMap<String, PreparedCandidate>,
    pub preparations: Vec<CandidatePreparationPlan>,
    pub contexts: BTreeMap<String, ConstructContext>,
    pub checkpoints: BTreeMap<String, ConstructionCheckpoint>,
    pub queued_construction: BTreeMap<String, ContextualAttemptDispatch>,
    pub admitted_construction: BTreeMap<String, ConstructionAdmission>,
    pub preview_plans: Vec<CompatibilityPreviewPlan>,
    pub previews: Vec<CompatibilityPreviewRecord>,
    pub diagnostics: Vec<CoordinationDiagnostic>,
    pub survivor_groups: Vec<SurvivorGroup>,
    pub partial_head_repair: Option<PartialHeadRepairPlan>,
    pub partial_head_repair_attempts: u32,
    /// A non-eager policy's ordinary final fold has been emitted and is
    /// awaiting its version-bound Resolve fact.
    pub final_in_flight: bool,
    /// Aggregate gates for the exact selected root have been emitted. This
    /// prevents a later scheduling fact from dispatching the final root twice.
    pub final_dispatched: bool,
}

impl CoordinationState {
    #[must_use]
    pub fn new(
        policy: CoordinationPolicy,
        composition_contract: CompositionContractTemplate,
        bloom: BloomId,
        base: CandidateRef,
        members: Vec<GenerationMember>,
    ) -> Self {
        let generation = IntegrationGeneration { bloom, base, members, epoch: 0 };
        let generation_id = generation.digest();
        Self {
            policy,
            composition_contract,
            integration: EagerIntegrationState {
                generation,
                head: IntegrationHead {
                    generation: generation_id,
                    node: generation_id,
                    candidate: base,
                    plan: Digest::default(),
                    coverage: Vec::new(),
                },
                queued: Vec::new(),
                admitted: Vec::new(),
                in_flight: None,
                known_red: None,
                unproved_repair: None,
                reservation: None,
                movement_count: 0,
            },
            requests: Vec::new(),
            runs: Vec::new(),
            claims: BTreeMap::new(),
            prepared: BTreeMap::new(),
            preparations: Vec::new(),
            contexts: BTreeMap::new(),
            checkpoints: BTreeMap::new(),
            queued_construction: BTreeMap::new(),
            admitted_construction: BTreeMap::new(),
            preview_plans: Vec::new(),
            previews: Vec::new(),
            diagnostics: Vec::new(),
            survivor_groups: Vec::new(),
            partial_head_repair: None,
            partial_head_repair_attempts: 0,
            final_in_flight: false,
            final_dispatched: false,
        }
    }

    #[must_use]
    pub fn request(&self, id: Digest) -> Option<&MemberVerifyRequest> {
        self.requests.iter().find(|request| request.digest() == id)
    }

    #[must_use]
    pub fn run(&self, id: Digest) -> Option<&SharedRunRecord> {
        self.runs.iter().find(|run| run.plan.digest() == id)
    }

    #[must_use]
    pub fn preparation(&self, id: Digest) -> Option<&CandidatePreparationPlan> {
        self.preparations.iter().find(|plan| plan.digest() == id)
    }

    /// Whether `pin` carries an exact admitted proof in this coordination
    /// state. Contextual receipts remain bound to their physical node and
    /// complete contract; they never impersonate standalone tree proofs.
    #[must_use]
    pub fn has_exact_claim(&self, pin: &MemberPin) -> bool {
        let Some(claim) = self.claims.get(&pin.workpiece.0).filter(|claim| claim.member == *pin) else {
            return false;
        };
        match &claim.proof {
            ResolutionProof::Standalone(proof) => {
                proof.stage == crate::StageId::Verify
                    && proof.verified().tree == pin.candidate.tree
                    && proof.evidence.kind == crate::EvidenceKind::VerificationResult
                    && proof.evidence.validates(&pin.candidate.tree)
            }
            ResolutionProof::InComposition { node, receipt, plan, request, contract } => {
                let Some(run) = self.run(*plan) else {
                    return false;
                };
                let (Some(recorded_node), Some(composition)) = (run.node.as_ref(), run.plan.composition.as_ref())
                else {
                    return false;
                };
                run.plan.digest() == *plan
                    && recorded_node.digest() == *node
                    && composition.contract.digest() == *contract
                    && composition.contract.members.iter().any(|member| member.request == *request)
                    && run
                        .plan
                        .requests
                        .iter()
                        .any(|candidate| candidate.digest() == *request && candidate.member == *pin)
                    && run.completed.iter().any(|outcome| {
                        matches!(outcome, MemberVerifyOutcome::PassedIn {
                            request: recorded_request,
                            node: recorded_node,
                            receipt: recorded_receipt,
                        } if recorded_request == request && recorded_node == node && recorded_receipt == receipt)
                    })
                    && receipt.kind == crate::EvidenceKind::VerificationResult
                    && receipt.validates(&recorded_node.candidate.tree)
            }
        }
    }

    /// Whether integration/pre-check selection is owned by the exact
    /// journaled head. Contextual policies use that root during their deferred
    /// final fold even when continuous eager advancement is disabled.
    #[must_use]
    pub fn uses_selected_root(&self) -> bool {
        self.policy.eager_integration
            || (self.policy.verification == VerificationMode::Contextual
                && (self.final_in_flight || !self.integration.head.coverage.is_empty()))
    }

    /// Whether speculative aggregate pre-checks should use the selected root.
    /// Deferred contextual policies expose dependency-enabling heads to late
    /// constructors but wait for full logical readiness before speculation.
    #[must_use]
    pub fn uses_selected_root_for_precheck(&self) -> bool {
        self.policy.eager_integration
            || (self.policy.verification == VerificationMode::Contextual && self.final_in_flight)
    }

    /// Next physical execution generation for the same immutable logical run.
    /// Hosts use this when terminal infrastructure failure leaves requests
    /// eligible without changing their proof contracts.
    #[must_use]
    pub fn next_execution_attempt(&self, plan: &SharedRunPlan) -> u32 {
        self.runs
            .iter()
            .filter(|run| {
                run.plan.mode == plan.mode
                    && run.plan.requests == plan.requests
                    && run.plan.composition == plan.composition
                    && run.plan.probe_budget == plan.probe_budget
            })
            .map(|run| run.plan.execution_attempt)
            .max()
            .map_or(0, |attempt| attempt.saturating_add(1))
    }

    /// The retained receipt when a settled contextual run proved this exact
    /// selected root under the complete currently sealed composition contract.
    /// This is the aggregate-position reuse seam; it never mints tree-bound
    /// member proofs.
    #[must_use]
    pub fn contextual_aggregate_proof(&self, head: &IntegrationHead) -> Option<&Evidence> {
        self.runs.iter().find_map(|run| {
            let (Some(node), Some(composition)) = (run.node.as_ref(), run.plan.composition.as_ref()) else {
                return None;
            };
            let members = run
                .plan
                .requests
                .iter()
                .map(|request| MemberContractPin { request: request.digest(), contract: request.contract.digest() })
                .collect();
            let valid = run.plan.mode == SharedRunMode::Contextual
                && !run.plan.requests.is_empty()
                && run.is_terminal()
                && !run.stale
                && run.unfinished.is_empty()
                && node.candidate == head.candidate
                && node.coverage == head.coverage
                && composition.contract == self.composition_contract.bind(members)
                && run.plan.requests.iter().all(|request| self.composition_contract.covers(request))
                && run.completed.len() == run.plan.requests.len()
                && run.plan.requests.iter().zip(&run.completed).all(|(request, outcome)| {
                    matches!(outcome, MemberVerifyOutcome::PassedIn { request: proven_request, node: proven, receipt }
                        if *proven_request == request.digest()
                            && *proven == node.digest()
                            && receipt.kind == crate::EvidenceKind::VerificationResult
                            && receipt.validates(&node.candidate.tree))
                });
            valid.then(|| match &run.completed[0] {
                MemberVerifyOutcome::PassedIn { receipt, .. } => receipt,
                _ => unreachable!("validated contextual completion"),
            })
        })
    }
}
