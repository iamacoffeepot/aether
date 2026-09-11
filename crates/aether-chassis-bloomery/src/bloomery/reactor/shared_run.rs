//! Host-only durable descriptions shared by probe preparation and execution.

use aether_bloomery::{
    AgentProfile, CandidateRef, CompositionInput, ConfigRegistry, Digest, Evidence, StageVerdict, StudyCall, StudyCost,
    Transformation, VerifyFailureSet,
};
use serde::{Deserialize, Serialize};

use crate::bloomery::{BatchProbeRequest, ProbeVerdict};

/// One immutable source request for a bounded attribution probe.
///
/// `inputs` is already dependency closed. An empty input set tests `base`;
/// for an inherited-context probe that base is the member's exact starting
/// integration head rather than the bloom's original base.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedProbePreparationRequest {
    pub run: Digest,
    pub ordinal: u32,
    pub plan: Digest,
    pub probe: BatchProbeRequest,
    pub base: CandidateRef,
    pub inputs: Vec<CompositionInput>,
    pub transformation: Transformation,
    pub profile: AgentProfile,
    pub configs: ConfigRegistry,
}

/// Exact prepared executor input stored before a probe can be submitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedSharedProbe {
    pub request: Digest,
    pub candidate: CandidateRef,
    pub transformation: Transformation,
    pub profile: AgentProfile,
    pub configs: ConfigRegistry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharedProbePreparation {
    Prepared(Box<PreparedSharedProbe>),
    Refused { request: Digest, detail: Digest },
}

/// Persisted description of any physical executor step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharedStepDescriptor {
    Member { request: Digest },
    ContextualFull { node: Digest },
    Probe(Box<SharedProbePreparationRequest>),
}

/// Trusted, restartable result of one actual executor invocation.
///
/// Every optional field defaults on decode: a receipt is a durable row a later
/// build has to read back, and a field this build added must not make every
/// row written before it undecodable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedStepReceipt {
    pub invocation: Digest,
    pub evidence: Evidence,
    #[serde(with = "StageVerdictDef")]
    pub verdict: StageVerdict,
    pub failed_verifiers: VerifyFailureSet,
    #[serde(default)]
    pub failed_verifier_names: Vec<String>,
    #[serde(default)]
    pub findings: Option<String>,
    #[serde(default)]
    pub cost: Option<StudyCost>,
    #[serde(default)]
    pub calls: Option<Vec<StudyCall>>,
    #[serde(default)]
    pub contextual_observations: Option<Vec<u8>>,
    #[serde(default)]
    pub probe_verdict: Option<ProbeVerdict>,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "StageVerdict", rename_all = "snake_case")]
enum StageVerdictDef {
    Approved,
    VerificationPassed,
    VerificationFailed,
    ReviewFinding,
    Parked,
    ExecutorFault,
    Declined,
    SurfaceRequested,
}
