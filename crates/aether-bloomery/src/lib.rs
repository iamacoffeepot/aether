//! aether-bloomery: the canonical value vocabulary and pure control core
//! of Bloomery (ADR-0149).
//!
//! A pure rlib providing the two things the whole control plane rests on: a
//! canonical, immutable value vocabulary that everything durable is
//! expressed in, and the one pure function that owns every state
//! transition.
//!
//! # A `std` crate with an `alloc`-only vocabulary
//!
//! The crate compiles as `std`. Nothing here requires `#![no_std]`: the
//! control core is a native capability rather than a wasm component
//! (ADR-0149 §The boundary), so this crate emits no wasm and carries no
//! guest-SDK dependency, and the native host ([#3458]) and the GitHub
//! adapter link it directly.
//!
//! The vocabulary is nonetheless written against `alloc` paths — `alloc::vec`
//! and friends, never their `std` re-exports — so the types stay portable to
//! a `no_std` consumer without a fork. That is what the `extern crate alloc`
//! below buys; `std` beyond it appears only under `#[cfg(test)]`, since the
//! integration tests in `tests/` are ordinary std crates.
//!
//! # The shape
//!
//! - [`Digest`] / [`digest_of`] — content addressing: a sha256 over a
//!   value's canonical aether-wire bytes ([`aether_data::wire`], ADR-0118).
//! - [`values`] — the immutable value vocabulary forming a derivation DAG
//!   in which every artifact names its parents by digest: [`Artifact`],
//!   [`Statement`] + [`Provenance`], [`Workpiece`], the one-way bloom
//!   lifecycle ([`BloomDraft`] → [`BloomSpec`] → [`ResolvedBloom`] →
//!   [`LandingReceipt`]), [`Evidence`], [`StageBinding`] / [`StageCatalog`]
//!   and the [`AgentProfile`] a binding references, [`Attempt`],
//!   [`Transformation`].
//! - [`sign`] — the [`KeyProvider`] trait and its fake always-valid
//!   implementation. ADR-0149 ships the signature *shapes*; key custody is
//!   a later arc.
//! - [`manifest`] — [`PromptManifest`] assembly, fail-closed: an
//!   instruction-capable slot that does not trace to a signed statement or
//!   a versioned policy artifact rejects the attempt before dispatch.
//! - [`mod@reduce`] — the pure control core: [`reduce`](reduce::reduce) owns
//!   every state transition, with no I/O, no engine boot, no GitHub types.
//! - [`metrics`] — the metrics ledger: [`MetricsLedger`] folds the
//!   journal into dispatch / bloom / day rollups and seat rows. Cost stays a
//!   study-artifact digest; `cost == 0` is unpriced, never free.
//! - [`calibration`] — the capability ledger: [`CalibrationLedger`] folds the
//!   journal into per-`(harness, model, effort) × stage` counts — attempts,
//!   rolls, typed verifier failures, and study-measured cost — so which agent to
//!   run a stage under is an evidenced choice (ADR-0184).
//! - [`study_report`] — the pure forecast grade: [`grade`] folds a bloom's
//!   admitted study records into actual cost and time, reads its retries off the
//!   dispatch ledger, and grades all three against the sealed [`Forecast`]
//!   (ADR-0151, ADR-0180).
//! - [`mod@spend`] — the window spend projection: [`measure`] sums
//!   the priced column on each bloom's admitted study records so the seal-door
//!   governor and the ledger share one accounting path (ADR-0192).
//! - [`port`] — the [`SourceBackend`] / [`ProjectionBackend`] /
//!   [`ExecutorBackend`] trait shapes adapters implement and the host mounts.
//!   Kept here so adapters depend inward on this crate, cycle-free (ADR-0149
//!   §The boundary).
//!
//! [#3458]: https://github.com/iamacoffeepot/aether/issues/3458

// Declared even though the crate is `std`, so the vocabulary's imports name
// `alloc` directly and a `std`-only type cannot reach the value types unnoticed.
// See the crate docs above for why the crate is `std` at all (issue #3497).
extern crate alloc;

pub mod calibration;
pub mod control;
pub mod correspondence;
pub mod digest;
pub mod ids;
pub mod inward;
mod ledger;
pub mod manifest;
pub mod metrics;
pub mod persisted;
pub mod port;
pub mod reduce;
pub mod sign;
pub mod spend;
pub mod study_report;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod values;

pub use calibration::CalibrationDocument;
pub use calibration::CalibrationLedger;
pub use calibration::CapabilityCell;
pub use calibration::CapabilityLedger;
pub use calibration::LEDGER_CAVEAT;
pub use calibration::VerifierFailures;
pub use control::Admit;
pub use control::AdmitResult;
pub use control::AggregateReviewPayload;
pub use control::AggregateVerifyPayload;
pub use control::BaseVerifyPayload;
pub use control::CONTROL_CORE_NAMESPACE;
pub use control::CancelDispatchPayload;
pub use control::ClaimResult;
pub use control::ClaimSeal;
pub use control::Commit;
pub use control::CommitResult;
pub use control::CompleteRelease;
pub use control::CompleteReleaseResult;
pub use control::CompleteTransfer;
pub use control::ConfigRecord;
pub use control::DispatchPayload;
pub use control::EnumerateClaims;
pub use control::EnumerateClaimsResult;
pub use control::IntegratePayload;
pub use control::JournalRecord;
pub use control::LandPayload;
pub use control::LoadConfigs;
pub use control::LoadConfigsResult;
pub use control::MemberClaimReleasePayload;
pub use control::MembershipMutation;
pub use control::MetricsQuery;
pub use control::MetricsQueryResult;
pub use control::MetricsView;
pub use control::ObserveMainline;
pub use control::ObserveMainlineResult;
pub use control::OrphanClaimReleasePayload;
pub use control::OutboxPayload;
pub use control::ProposalPayload;
pub use control::Query;
pub use control::QueryResult;
pub use control::QuerySelector;
pub use control::RedispatchPayload;
pub use control::ReleaseSeal;
pub use control::ReplayJournal;
pub use control::ReplayJournalResult;
pub use control::ReviewPass;
pub use control::SourceReplicaPayload;
pub use control::SpendQuery;
pub use control::SpendQueryResult;
pub use control::SplicePayload;
pub use control::Topic;
pub use control::TransferSeal;
pub use correspondence::BackendObjectId;
pub use correspondence::Correspondence;
pub use correspondence::CorrespondenceError;
pub use correspondence::SharedCorrespondence;
pub use digest::ContentAddressed;
pub use digest::Digest;
pub use digest::decode_hex;
pub use digest::digest_of;
pub use digest::encode_hex;
pub use digest::hex_nibble;
pub use digest::schema_digest;
pub use ids::BloomId;
pub use ids::IdempotencyKey;
pub use ids::KeyId;
pub use ids::Nonce;
pub use ids::SessionSlug;
pub use ids::StageId;
pub use ids::WorkpieceId;
pub use inward::InwardError;
pub use inward::StageResult;
pub use inward::StageVerdict;
pub use inward::StudyResult;
pub use inward::normalize_stage_result;
pub use inward::normalize_study_result;
pub use manifest::ClosureViolation;
pub use manifest::MANIFEST_CLOSURE_BUDGET;
pub use manifest::PromptManifest;
pub use manifest::ProvenanceIndex;
pub use manifest::Slot;
pub use manifest::SlotRole;
pub use manifest::assemble_manifest;
pub use metrics::DAYS_CAP;
pub use metrics::METRICS_DEFAULT_LIMIT;
pub use metrics::METRICS_MAX_LIMIT;
pub use metrics::MetricBloom;
pub use metrics::MetricDay;
pub use metrics::MetricDispatch;
pub use metrics::MetricsLedger;
pub use metrics::MetricsSeat;
pub use metrics::MetricsSummary;
pub use metrics::MetricsTimeline;
pub use metrics::RECONSTRUCTED_WINDOW;
pub use metrics::TIMELINE_SPAN_CAP;
pub use metrics::TimelineSpan;
pub use metrics::window_label;
pub use persisted::PERSISTED_KINDS;
pub use persisted::PersistedKind;
pub use persisted::PersistedSchemaError;
pub use persisted::PersistedUpcast;
pub use persisted::decode_persisted;
pub use persisted::decode_recorded_decisions;
pub use persisted::decode_recorded_event;
pub use persisted::kind_named;
pub use port::AwaitingSurfaceView;
pub use port::BackendId;
pub use port::BaseAlertView;
pub use port::BloomView;
pub use port::Checkpoint;
pub use port::ClaimHolder;
pub use port::ClaimOutcome;
pub use port::ClaimRefKind;
pub use port::ClaimRefState;
pub use port::ClaimReleaseOutcome;
pub use port::CommissionProjection;
pub use port::CompositionCursorView;
pub use port::CompositionView;
pub use port::Conclusion;
pub use port::EvidenceRef;
pub use port::ExecutionStatus;
pub use port::ExecutorBackend;
pub use port::ExecutorFaultView;
pub use port::HostFaultView;
pub use port::IntegrateOutcome;
pub use port::IntegrationPosition;
pub use port::LandOutcome;
pub use port::LandingBlock;
pub use port::LaneObservation;
pub use port::LeaseEvictionView;
pub use port::LeaseView;
pub use port::MAX_TITLE_CHARS;
pub use port::MemberView;
pub use port::MemberWhy;
pub use port::NarrowedCompositionView;
pub use port::ObservedLaneWrites;
pub use port::POSITIONAL_ROW_SCHEMA;
pub use port::PendingDecisionView;
pub use port::ProjectedReceipt;
pub use port::ProjectionBackend;
pub use port::ReviewParkView;
pub use port::RowSchemaError;
pub use port::SourceBackend;
pub use port::SourceSnapshot;
pub use port::TransitionWhy;
pub use port::ViewDocument;
pub use port::WedgeCause;
pub use port::WhyDocument;
pub use port::WhyState;
pub use port::WithdrawnView;
pub use port::WorkHandle;
pub use port::WorkOrder;
pub use port::decode_row;
pub use port::encode_row;
pub use port::intent_title;
pub use reduce::AGGREGATE_REVIEW_GATE;
pub use reduce::AGGREGATE_VERIFY_GATE;
pub use reduce::AdjudicationError;
pub use reduce::AdmitEvidenceError;
pub use reduce::AdoptAnswerError;
pub use reduce::AggregateReviewError;
pub use reduce::AggregateReviewFault;
pub use reduce::AggregateVerifyError;
pub use reduce::AttemptCompletedError;
pub use reduce::AwaitingSurface;
pub use reduce::BaseMismatch;
pub use reduce::BaseReverifyError;
pub use reduce::BloomRecord;
pub use reduce::BloomStatus;
pub use reduce::DISPATCH_MEMBER_GATE;
pub use reduce::DRAFT_ADMISSION_GATE;
pub use reduce::Decision;
pub use reduce::Decisions;
pub use reduce::Event;
pub use reduce::Excuse;
pub use reduce::FOLD_GATE;
pub use reduce::Fact;
pub use reduce::FileLease;
pub use reduce::FoldConflictError;
pub use reduce::FoldedIntegration;
pub use reduce::Gate;
pub use reduce::GrantAttemptsError;
pub use reduce::HostFaultError;
pub use reduce::HostFaultHold;
pub use reduce::IntegrateError;
pub use reduce::LAND_GATE;
pub use reduce::LandError;
pub use reduce::LandingRejectedError;
pub use reduce::LeaseEviction;
pub use reduce::LeaseObservationError;
pub use reduce::MemberExecutorFaultError;
pub use reduce::MemberMachineryFault;
pub use reduce::MemberPark;
pub use reduce::NarrowCompositionError;
pub use reduce::NarrowedComposition;
pub use reduce::OperatorHoldError;
pub use reduce::OperatorRepairError;
pub use reduce::OrphanClaimReleaseError;
pub use reduce::Outcome;
pub use reduce::ProposalError;
pub use reduce::Read;
pub use reduce::RecordedRead;
pub use reduce::RecordedRefusal;
pub use reduce::Refusal;
pub use reduce::ResolveError;
pub use reduce::SealConflict;
pub use reduce::SealError;
pub use reduce::Snapshot;
pub use reduce::SpliceError;
pub use reduce::StageProgress;
pub use reduce::SupersedeError;
pub use reduce::SuppressionDispositionError;
pub use reduce::SurfaceRequestedError;
pub use reduce::VerifyFailedError;
pub use reduce::WithdrawError;
pub use reduce::is_active_unlanded;
pub use reduce::reduce;
pub use reduce::view_of;
pub use reduce::why_of;
pub use sign::AuthorityDoor;
#[cfg(not(target_arch = "wasm32"))]
pub use sign::AuthorizedSigner;
#[cfg(not(target_arch = "wasm32"))]
pub use sign::Ed25519KeyProvider;
pub use sign::FakeKeyProvider;
pub use sign::KeyProvider;
#[cfg(not(target_arch = "wasm32"))]
pub use sign::OperatorKey;
#[cfg(not(target_arch = "wasm32"))]
pub use sign::OperatorKeyError;
pub use sign::SignatureEnvelope;
pub use sign::authorization_message;
#[cfg(not(target_arch = "wasm32"))]
pub use sign::sign_authorization;
pub use spend::measure;
pub use study_report::BloomGrade;
pub use study_report::StudyReport;
pub use study_report::grade;
pub use values::ADR_SCHEMA;
pub use values::ADR_TRANSITION_SCHEMA;
pub use values::AdjudicateRequest;
pub use values::Adjudication;
pub use values::Adr;
pub use values::AdrStatus;
pub use values::AdrTouch;
pub use values::AdrTransition;
pub use values::AdrValueError;
pub use values::AgentProfile;
pub use values::AgentSelection;
pub use values::ApprovalPolicy;
pub use values::ApprovalRule;
pub use values::ArchiveFailureView;
pub use values::ArchiveListView;
pub use values::ArchivePassView;
pub use values::ArchiveRecordView;
pub use values::Artifact;
pub use values::Attempt;
pub use values::BaseReceipt;
pub use values::BaseReverify;
pub use values::BaseVerdict;
pub use values::BloomDispatchView;
pub use values::BloomDispatchesView;
pub use values::BloomDraft;
pub use values::BloomSpec;
pub use values::CHECK_KEY;
pub use values::CONSTRUCT_IMPLEMENT_COMMAND;
pub use values::CRITICAL_KEY;
pub use values::CancelCommissionRequest;
pub use values::CandidateRef;
pub use values::CatalogError;
pub use values::ClaimRefView;
pub use values::ClaimsView;
pub use values::ClassifiedFinding;
pub use values::ClassifiedFindings;
pub use values::CommissionApprovalTier;
pub use values::CommissionApprovalView;
pub use values::CommissionCancelledView;
pub use values::CommissionCreatedView;
pub use values::CommissionHeadView;
pub use values::CommissionReopenedView;
pub use values::CommissionShowView;
pub use values::CommissionStatementRole;
pub use values::CommissionStatus;
pub use values::CommissionValueError;
pub use values::CommissionsView;
pub use values::Completeness;
pub use values::CompositionFinding;
pub use values::CompositionParents;
pub use values::ConfigKind;
pub use values::ConfigRegistry;
pub use values::ConfigResolveError;
pub use values::ConfigScopes;
pub use values::CoordinatorLogEntry;
pub use values::CoordinatorLogsView;
pub use values::CreateCommissionRequest;
pub use values::DEFAULT_HTTP_PORT;
pub use values::DependencyError;
pub use values::DispatchEvidenceView;
pub use values::DispatchFilePage;
pub use values::DispatchKey;
pub use values::DispatchProcessView;
pub use values::Disposition;
pub use values::DraftPatch;
pub use values::DraftView;
pub use values::DraftsView;
pub use values::ErrorView;
pub use values::EvictedHolder;
pub use values::Evidence;
pub use values::EvidenceKind;
pub use values::ExecutionLimits;
pub use values::FIELD_ENTRY_SCHEMA;
pub use values::FieldEntry;
pub use values::FieldKind;
pub use values::FindingClass;
pub use values::FoldContribution;
pub use values::Forecast;
pub use values::GrantRequest;
pub use values::HTTP_READ_TIMEOUT;
pub use values::Harness;
pub use values::HoldRequest;
pub use values::JUDGMENT_TAG;
pub use values::JournalEntry;
pub use values::JournalView;
pub use values::LANE_WORKPIECE_HEADER;
pub use values::LandingReceipt;
pub use values::LongContextBand;
pub use values::MAX_OBSERVED_WRITES;
pub use values::MECHANICAL_TAG;
pub use values::MemberCandidate;
pub use values::MemberDependency;
pub use values::MemberProjection;
pub use values::MemberSubject;
pub use values::Membership;
pub use values::ModelOverride;
pub use values::NamedPath;
pub use values::NamedSymbol;
pub use values::NarrowingRefusal;
pub use values::NetworkProfile;
pub use values::ORPHAN_CLAIM_RELEASE_WORDS;
pub use values::Observation;
pub use values::OperatorHold;
pub use values::OperatorProposal;
pub use values::OperatorRepair;
pub use values::OrphanClaimRelease;
pub use values::OrphanClaimReleaseCompletion;
pub use values::OrphanClaimReleaseRecord;
pub use values::OutcomeView;
pub use values::OverrideError;
pub use values::PathOrigin;
pub use values::PriceRates;
pub use values::PriceTable;
pub use values::ProposeRequest;
pub use values::Provenance;
pub use values::Question;
pub use values::REVIEW_CRITIC_COMMAND;
pub use values::ReasoningEffort;
pub use values::ReleaseAcceptedView;
pub use values::ReleaseRequest;
pub use values::ReopenCommissionRequest;
pub use values::RepairRequest;
pub use values::ResolutionClaim;
pub use values::ResolvedBloom;
pub use values::ResolvedConfigs;
pub use values::ResolvedDependencies;
pub use values::ResolvedModel;
pub use values::RetryRequest;
pub use values::ReverifyBaseRequest;
pub use values::RevisionEvidence;
pub use values::SCOPE_FILL_COMMAND;
pub use values::SCOPE_REVISION_SCHEMA;
pub use values::SCOPE_VERIFY_SCHEMA;
pub use values::ScopeRevision;
pub use values::ScopeRevisionWrittenView;
pub use values::ScopeRouting;
pub use values::ScopeRunOpenedView;
pub use values::ScopeRunRequest;
pub use values::ScopeVerifyInput;
pub use values::ScopeVerifyReport;
pub use values::SealRequest;
pub use values::SealedPriceTable;
pub use values::SpendCeiling;
pub use values::SpendQuiesce;
pub use values::SpendWindow;
pub use values::StageBinding;
pub use values::StageCatalog;
pub use values::StageOverride;
pub use values::StageReceipt;
pub use values::Statement;
pub use values::StudyCall;
pub use values::StudyCost;
pub use values::StudyRecord;
pub use values::SupersedeRequest;
pub use values::SuppressionAnswerRequest;
pub use values::SuppressionDisposition;
pub use values::SuppressionRequest;
pub use values::SuppressionVerdict;
pub use values::SurfacePathRequest;
pub use values::SurfacePattern;
pub use values::SurfaceRequest;
pub use values::Tier;
pub use values::TierVerdict;
pub use values::TimeoutRecord;
pub use values::ToolPolicy;
pub use values::Transformation;
pub use values::Unproducible;
pub use values::VERIFY_BASE_COMMAND;
pub use values::VERIFY_CHECK_COMMAND;
pub use values::VERIFY_LANE_IMAGE;
pub use values::VERIFY_LANE_NETWORK;
pub use values::VERIFY_MEMBER_COMMAND;
pub use values::VerifiedTree;
pub use values::VerifyFailure;
pub use values::VerifyFailureSet;
pub use values::VerifyGateSet;
pub use values::VerifyProof;
pub use values::VerifyReuse;
pub use values::Wedge;
pub use values::Widening;
pub use values::WithdrawRequest;
pub use values::Withdrawal;
pub use values::WithdrawalCause;
pub use values::Workpiece;
pub use values::WorkpieceBuilder;
pub use values::WorkpieceFact;
pub use values::WorkpieceFields;
pub use values::WorkpieceRefusal;
pub use values::WorkpiecesView;
pub use values::WriteRevisionRequest;
pub use values::classify_findings;
pub use values::coarsen;
pub use values::config_address;
pub use values::decode_config;
pub use values::gate_widening;
pub use values::http_success;
pub use values::is_model_lane;
pub use values::narrow_composition;
pub use values::normalize_write_paths;
pub use values::path_in_surface;
pub use values::pin_workpiece_description;
pub use values::resolve_member_dependencies;
pub use values::rewrites;
#[cfg(not(target_arch = "wasm32"))]
pub use values::signed_approval;
#[cfg(not(target_arch = "wasm32"))]
pub use values::signed_cancel;
#[cfg(not(target_arch = "wasm32"))]
pub use values::signed_proposal;
#[cfg(not(target_arch = "wasm32"))]
pub use values::signed_reopen;
pub use values::split_lane_identity;
pub use values::surface_additions;
pub use values::surface_intersection;
pub use values::surface_union;
pub use values::tier_verdict;
pub use values::verify_scope;
pub use values::widen;
