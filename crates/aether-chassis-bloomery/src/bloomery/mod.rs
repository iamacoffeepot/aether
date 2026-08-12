//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod approve;
mod chassis;
mod cli;
mod config;
mod construct;
mod driver;
mod executor;
mod findings;
mod intake;
mod mirror;
mod outbox;
mod poll_timer;
mod reactor;
mod source;
mod study;

pub use approve::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, PolicyError,
    StatementRejected, Tier, approval_from_statement, precheck_statement, verified_statement_approval,
};
pub use chassis::{BloomeryChassis, BloomeryEnv, DEFAULT_RPC_PORT, RpcPortConfig};
pub use cli::BloomeryCli;
pub use config::{CoordinatorConfig, CoordinatorOverlay, GithubConnectionConfig, GithubConnectionOverlay};
pub use construct::{CONSTRUCT_IMPLEMENT_COMMAND, dispatch_model};
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
pub use executor::{
    CaptureIdentity, DEFAULT_LANE_PROGRAM, ExecutorPortError, ExecutorShell, LaneProgram, LocalExecutor,
    LocalExecutorError, ProcessTransformRunner, RoutingExecutor, RunLifecycle, RunProcess, RunSpec, TransformRunner,
    UnconfiguredActionsBackend, mock_lane,
};
pub use intake::{
    Admission, AdmitDecision, AdmitSink, CycleError, CycleReport, DispatchError, DispatchRecord, EvidenceClaims,
    IntakeError, IntakeRefusal, NameEvidenceClaims, UploadedEvidence, admit_uploaded, attempt_artifact_name,
    dispatch_and_record, record_dispatch, run_intake_cycle,
};
pub use mirror::ProjectionShell;
pub use outbox::TopicOutbox;
pub use reactor::{
    DispatchTick, DrainTick, ExecutorReactorCapability, ExecutorReactorSetup, ExecutorReactorState,
    IntegrateReactorCapability, IntegrateReactorSetup, IntegrateReactorState, IntegrateTick, LandReactorCapability,
    LandReactorSetup, LandReactorState, LandTick, MirrorReactorCapability, MirrorReactorSetup, MirrorReactorState,
};
pub use source::SourceShell;
pub use study::{
    StudyAdmission, StudyAdmitDecision, StudyIntakeError, StudyRefusal, UploadedStudyRecord, admit_study,
    rebuild_study_index,
};
