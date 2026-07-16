//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod chassis;
mod cli;
mod construct;
mod driver;
mod executor;
mod intake;
mod mirror;
mod mirror_driver;
mod source;
mod study;

pub use chassis::{BloomeryChassis, BloomeryEnv, DEFAULT_RPC_PORT, RpcPortConfig};
pub use cli::BloomeryCli;
pub use construct::{CONSTRUCT_IMPLEMENT_COMMAND, build_construct_order};
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
pub use executor::ExecutorShell;
pub use intake::{
    Admission, AdmitDecision, AdmitSink, CycleError, CycleReport, DispatchError, DispatchRecord, EvidenceClaims,
    IntakeError, IntakeRefusal, UploadedEvidence, admit_uploaded, dispatch_and_record, record_dispatch,
    run_intake_cycle,
};
pub use mirror::{GithubMirrorConfig, GithubMirrorOverlay, ProjectionShell};
pub use mirror_driver::{
    DrainTick, MirrorDriverCapability, MirrorDriverState, TOPIC_LANDING_RECEIPT, TOPIC_VIEW_DOCUMENT,
};
pub use source::SourceShell;
pub use study::{
    StudyAdmission, StudyAdmitDecision, StudyIntakeError, StudyRefusal, UploadedStudyRecord, admit_study,
    rebuild_study_index,
};
