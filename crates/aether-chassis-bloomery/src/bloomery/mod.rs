//! The Bloomery coordinator chassis (ADR-0149 §Packaging).

pub use aether_substrate::Chassis;

mod approve;
mod chassis;
mod cli;
mod config;
mod construct;
mod doctor;
mod driver;
#[cfg(feature = "github")]
mod executor;
mod findings;
#[cfg(feature = "github")]
mod intake;
#[cfg(feature = "github")]
mod mirror;
#[cfg(feature = "github")]
mod notify;
mod outbox;
#[cfg(feature = "github")]
mod replica;
// Crate-visible: the control core runs the same sidecar for its mainline
// observer, and that cap lives outside this module.
#[cfg(feature = "github")]
mod local_landing;
pub(crate) mod poll_timer;
#[cfg(feature = "github")]
mod reactor;
mod repair;
#[cfg(feature = "runtime")]
mod scope_run;
#[cfg(feature = "github")]
mod source;
#[cfg(feature = "github")]
mod study;
#[cfg(all(feature = "github", any(test, feature = "testing")))]
mod testing;
#[cfg(feature = "github")]
mod triage;
mod verify;

pub use approve::{
    AdmissionRequest, AdrTouch, ApprovalPolicy, Completeness, Decision, Gate, Incompleteness, PolicyError,
    StatementRejected, Tier, approval_from_statement, check_signer_tier, load_policy, parse_policy, precheck_statement,
    projection_digest, verified_statement_approval,
};
pub use chassis::{BloomeryChassis, BloomeryEnv, DEFAULT_HTTP_PORT, DEFAULT_RPC_PORT, RpcPortConfig};
pub use cli::BloomeryCli;
pub use config::{CoordinatorConfig, CoordinatorOverlay, MissingWriterMarker};
#[cfg(feature = "github")]
pub use config::{GithubConnectionConfig, GithubConnectionOverlay};
pub use construct::{CONSTRUCT_IMPLEMENT_COMMAND, dispatch_model};
pub use doctor::{
    Ancestry, CheckResult, DETERMINISTIC_RETRY_BOUND, DoctorReport, Invariant, KitReport, KitTool, LiveState,
    OpenDispatch, REPLICA_AGE_BOUND, REQUIRED_KIT, ReplicaObservation, ResolvedTool, SURFACE_PARK_AGE_BOUND,
    ToolStatus, UNRESOLVED_HEAD_AGE_BOUND, evaluate,
};
#[cfg(feature = "github")]
pub use doctor::{DoctorBoard, DoctorReactorCapability, DoctorReactorSetup, DoctorReactorState, DoctorTick};
pub use driver::{BloomeryDriverCapability, BloomeryDriverRunning};
#[cfg(feature = "github")]
pub use executor::{
    CaptureIdentity, CapturedObjects, DEFAULT_LANE_PROGRAM, ExecutorPortError, ExecutorShell, LaneOccupancy,
    LaneProgram, LocalExecutor, LocalExecutorError, LocalLane, OrphanedRun, OutstandingDispatch,
    ProcessTransformRunner, ReconcileLanes, ReconcileReport, RoutingExecutor, RunLifecycle, RunProcess, RunSpec,
    TransformRunner, UnconfiguredActionsBackend, admits_lane_key, mock_lane,
};
#[cfg(feature = "github")]
pub use intake::{
    Admission, AdmitDecision, AdmitSink, CycleError, CycleReport, DispatchError, DispatchRecord, EvidenceClaims,
    IntakeError, IntakeRefusal, NameEvidenceClaims, UploadedEvidence, admit_uploaded, attempt_artifact_name,
    dispatch_and_record, record_dispatch, run_intake_cycle,
};
#[cfg(feature = "github")]
pub use mirror::ProjectionShell;
#[cfg(feature = "github")]
pub use notify::{
    Delivered, NotifyConfig, NotifyEvent, NotifyOverlay, NotifyReactorCapability, NotifyReactorSetup,
    NotifyReactorState, NotifyTick, Volume, deliver, notify_events, webhook_sink,
};
pub use outbox::TopicOutbox;
#[cfg(feature = "github")]
pub(crate) use reactor::candidate_push_at;
#[cfg(feature = "github")]
pub use reactor::{
    ArchiveFailure, ArchiveFailureView, ArchiveOutcome, ArchiveRecords, ArchiveRecordsResult, ArchiveRequest,
    ArchiveTier, ArchivedRecord, ArchivedRecordView, CandidatePush, ClaimReleaseReactorCapability,
    ClaimReleaseReactorSetup, ClaimReleaseReactorState, ClaimReleaseTick, DispatchTick, DrainTick,
    ExecutorReactorCapability, ExecutorReactorSetup, ExecutorReactorState, IntegrateReactorCapability,
    IntegrateReactorSetup, IntegrateReactorState, IntegrateTick, JanitorPolicy, JanitorReactorCapability,
    JanitorReactorSetup, JanitorReactorState, JanitorTick, LandReactorCapability, LandReactorSetup, LandReactorState,
    LandTick, ListArchive, ListArchiveResult, MirrorReactorCapability, MirrorReactorSetup, MirrorReactorState,
    SweepReport, SweepRequest, TargetScan, WorkingRefPruner, archive_pass, sweep,
};
#[cfg(feature = "github")]
pub use repair::{CandidateSource, PrepareError, prepare_candidate};
pub use repair::{candidate_tree_digest, capture_commit_digest};
#[cfg(feature = "github")]
pub use replica::{SourceReplicaShell, github_push_url, writer_marker_present};
#[cfg(feature = "runtime")]
pub use scope_run::{
    OpenedScopeRun, ScopeRunRefusal, ScopeRunState, open_scope_run, scope_dispatch_payload, scope_run_state,
    scope_run_subject,
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
#[cfg(feature = "runtime")]
pub use verify::{
    Accumulation, Attribution, AttributionError, AttributionRequest, BaseProbe, BaseRepairWorkpiece, BatchBisect,
    BatchComposer, BatchContext, BatchFailure, BatchFailureHooks, BatchGate, BatchMember, BatchReport, BatchRestart,
    BloomDisposition, CoverageEntry, CoverageMap, CoverageStatus, GateOutcome, Land, LandProbe, MemberFate,
    MissingCoverage, RepairBoard, RollDecision, RollHold, RunningGate, SurfaceOverlap, SweepContext, SweepDecision,
    SweepOutcome, TaintSet, TestClosure, UnknownFact, attribute_gate_failure, bisect_land_order, bloom_disposition,
    consult_proof_fact, coverage_map, decide_accumulation, decide_roll, decide_sweep, record_proof_facts,
    repair_landed, run_batch_gate, run_sweep, unknowns,
};
pub use verify::{
    ClosureKey, ClosureKeyError, DiscriminatedFact, DiscriminatedFacts, HostClass, ProofResult, ProofSource,
    RunnerReport, closure_key, discriminate,
};
