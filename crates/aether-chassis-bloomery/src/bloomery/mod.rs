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
mod poll_timer;
#[cfg(feature = "github")]
mod reactor;
#[cfg(feature = "github")]
mod source;
#[cfg(feature = "github")]
mod study;

pub use approve::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, PolicyError,
    StatementRejected, Tier, approval_from_statement, precheck_statement, verified_statement_approval,
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
    CaptureIdentity, DEFAULT_LANE_PROGRAM, ExecutorPortError, ExecutorShell, LaneProgram, LocalExecutor,
    LocalExecutorError, ProcessTransformRunner, RoutingExecutor, RunLifecycle, RunProcess, RunSpec, TransformRunner,
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
pub use reactor::{
    ClaimReleaseReactorCapability, ClaimReleaseReactorSetup, ClaimReleaseReactorState, ClaimReleaseTick, DispatchTick,
    DrainTick, ExecutorReactorCapability, ExecutorReactorSetup, ExecutorReactorState, IntegrateReactorCapability,
    IntegrateReactorSetup, IntegrateReactorState, IntegrateTick, LandReactorCapability, LandReactorSetup,
    LandReactorState, LandTick, MirrorReactorCapability, MirrorReactorSetup, MirrorReactorState,
};
#[cfg(feature = "github")]
pub use source::SourceShell;
#[cfg(feature = "github")]
pub use study::{
    StudyAdmission, StudyAdmitDecision, StudyIntakeError, StudyRefusal, UploadedStudyRecord, admit_study,
    rebuild_study_index,
};
