//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod approve;
mod chassis;
mod cli;
mod construct;
mod driver;
mod executor;
mod executor_driver;
mod findings;
mod intake;
mod integrate_driver;
mod land_driver;
mod local_executor;
mod mirror;
mod mirror_driver;
mod poll_timer;
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
pub use executor_driver::{DispatchTick, ExecutorDriverCapability, ExecutorDriverState};
pub use intake::{
    Admission, AdmitDecision, AdmitSink, CycleError, CycleReport, DispatchError, DispatchRecord, EvidenceClaims,
    IntakeError, IntakeRefusal, NameEvidenceClaims, UploadedEvidence, admit_uploaded, attempt_artifact_name,
    dispatch_and_record, record_dispatch, run_intake_cycle,
};
pub use integrate_driver::{IntegrateDriverCapability, IntegrateDriverState, IntegrateTick};
pub use land_driver::{LandDriverCapability, LandDriverState, LandTick};
pub use local_executor::{
    LocalExecutor, LocalExecutorError, ProcessTransformRunner, RunLifecycle, RunProcess, RunSpec, TransformRunner,
};
pub use mirror::{GithubMirrorConfig, GithubMirrorOverlay, ProjectionShell};
pub use mirror_driver::{DrainTick, MirrorDriverCapability, MirrorDriverState, TOPIC_VIEW_DOCUMENT};
pub use routing_executor::RoutingExecutor;
pub use source::SourceShell;
pub use study::{
    StudyAdmission, StudyAdmitDecision, StudyIntakeError, StudyRefusal, UploadedStudyRecord, admit_study,
    rebuild_study_index,
};
