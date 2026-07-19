//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod approve;
mod chassis;
mod cli;
mod construct;
mod driver;
mod executor;
mod findings;
mod intake;
mod local_executor;
mod mirror;
mod outbox;
mod poll_timer;
mod reactor;
mod routing_executor;
mod source;
mod study;

pub use approve::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, PolicyError,
    StatementRejected, Tier, approval_from_statement, precheck_statement, verified_statement_approval,
};
pub use chassis::{BloomeryChassis, BloomeryEnv, DEFAULT_RPC_PORT, RpcPortConfig};
pub use cli::BloomeryCli;
pub use construct::{CONSTRUCT_IMPLEMENT_COMMAND, build_construct_order};
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
pub use executor::{ExecutorPortError, ExecutorShell};
pub use intake::{
    Admission, AdmitDecision, AdmitSink, CycleError, CycleReport, DispatchError, DispatchRecord, EvidenceClaims,
    IntakeError, IntakeRefusal, NameEvidenceClaims, UploadedEvidence, admit_uploaded, attempt_artifact_name,
    dispatch_and_record, record_dispatch, run_intake_cycle,
};
pub use local_executor::{
    LocalExecutor, LocalExecutorError, ProcessTransformRunner, RunLifecycle, RunProcess, RunSpec, TransformRunner,
};
pub use mirror::{GithubMirrorConfig, GithubMirrorOverlay, ProjectionShell};
pub use outbox::TopicOutbox;
pub use reactor::{
    DispatchTick, DrainTick, ExecutorReactorCapability, ExecutorReactorState, IntegrateReactorCapability,
    IntegrateReactorState, IntegrateTick, LandReactorCapability, LandReactorState, LandTick, MirrorReactorCapability,
    MirrorReactorState,
};
pub use routing_executor::RoutingExecutor;
pub use source::SourceShell;
pub use study::{
    StudyAdmission, StudyAdmitDecision, StudyIntakeError, StudyRefusal, UploadedStudyRecord, admit_study,
    rebuild_study_index,
};
