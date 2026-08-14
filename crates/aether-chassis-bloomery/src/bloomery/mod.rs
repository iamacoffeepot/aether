//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod approve;
mod chassis;
mod cli;
mod config;
mod construct;
mod driver;
#[cfg(feature = "github")]
mod executor;
mod findings;
#[cfg(feature = "github")]
mod intake;
#[cfg(feature = "github")]
mod mirror;
mod outbox;
// Crate-visible: the control core runs the same sidecar for its mainline
// observer, and that cap lives outside this module.
pub(crate) mod poll_timer;
#[cfg(feature = "github")]
mod reactor;
#[cfg(feature = "github")]
mod source;
#[cfg(feature = "github")]
mod study;
#[cfg(all(feature = "github", any(test, feature = "testing")))]
mod testing;
#[cfg(feature = "github")]
mod triage;

pub use approve::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, PolicyError,
    StatementRejected, Tier, approval_from_statement, load_policy, parse_policy, precheck_statement,
    verified_statement_approval,
};
pub use chassis::{BloomeryChassis, BloomeryEnv, DEFAULT_RPC_PORT, RpcPortConfig};
pub use cli::BloomeryCli;
pub use config::{CoordinatorConfig, CoordinatorOverlay};
#[cfg(feature = "github")]
pub use config::{GithubConnectionConfig, GithubConnectionOverlay};
pub use construct::{CONSTRUCT_IMPLEMENT_COMMAND, dispatch_model};
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
#[cfg(feature = "github")]
pub use executor::{
    CaptureIdentity, CapturedObjects, DEFAULT_LANE_PROGRAM, ExecutorPortError, ExecutorShell, LaneProgram,
    LocalExecutor, LocalExecutorError, LocalLane, OrphanedRun, OutstandingDispatch, ProcessTransformRunner,
    ReconcileLanes, ReconcileReport, RoutingExecutor, RunLifecycle, RunProcess, RunSpec, TransformRunner,
    UnconfiguredActionsBackend, mock_lane,
};
#[cfg(feature = "github")]
pub use intake::{
    Admission, AdmitDecision, AdmitSink, CycleError, CycleReport, DispatchError, DispatchRecord, EvidenceClaims,
    IntakeError, IntakeRefusal, NameEvidenceClaims, UploadedEvidence, admit_uploaded, attempt_artifact_name,
    dispatch_and_record, record_dispatch, run_intake_cycle,
};
#[cfg(feature = "github")]
pub use mirror::ProjectionShell;
pub use outbox::TopicOutbox;
#[cfg(feature = "github")]
pub(crate) use reactor::default_candidate_push;
#[cfg(feature = "github")]
pub use reactor::{
    CandidatePush, ClaimReleaseReactorCapability, ClaimReleaseReactorSetup, ClaimReleaseReactorState, ClaimReleaseTick,
    DispatchTick, DrainTick, ExecutorReactorCapability, ExecutorReactorSetup, ExecutorReactorState,
    IntegrateReactorCapability, IntegrateReactorSetup, IntegrateReactorState, IntegrateTick, JanitorPolicy,
    JanitorReactorCapability, JanitorReactorSetup, JanitorReactorState, JanitorTick, LandReactorCapability,
    LandReactorSetup, LandReactorState, LandTick, MirrorReactorCapability, MirrorReactorSetup, MirrorReactorState,
    SweepReport, SweepRequest, sweep,
};
#[cfg(feature = "github")]
pub use source::SourceShell;
#[cfg(feature = "github")]
pub use study::{
    StudyAdmission, StudyAdmitDecision, StudyIntakeError, StudyRefusal, UploadedStudyRecord, admit_study,
    rebuild_study_index,
};
#[cfg(all(feature = "github", any(test, feature = "testing")))]
pub use testing::{ScriptedEvidence, ScriptedEvidenceResult, ScriptedUpload, ScriptedVerdict};
