//! The evidence-admission broker: the trust boundary that accepts an uploaded
//! attempt result only when its nonce names a live order and its bound digest is
//! the one the order displayed.

use std::error::Error;
use std::fmt;

use aether_bloomery::{
    Admit, BloomId, CandidateRef, Digest, Event, Evidence, EvidenceKind, Fact, InwardError, Nonce, ResolutionClaim,
    StageCatalog, StageId, StageResult, StageVerdict, StudyCall, StudyCost, VerifyFailure, VerifyFailureSet,
    WorkpieceId, classify_findings, normalize_stage_result,
};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};

use super::admission_key::AdmissionKey;
use super::dispatch::DispatchRecord;
use crate::bloomery::findings::{FindingsDecomposition, decompose_findings};
use crate::bloomery::triage::{TriageVerdict, triage_note, triage_repair};
use crate::store::{OutstandingOrder, StoreBackend};

/// What a worker uploaded as an attempt result: the nonce it claims to answer,
/// the digest its evidence is about (what the observation *claims*), the
/// verdict, and the supporting detail artifact. The broker checks the claimed
/// `subject` against the digest the matched order displayed.
///
/// Also the shape a *synthesised* result takes (ADR-0177): an order that
/// outlived its sealed execution limit produces no upload at all, so the
/// executor reactor builds one over the order's own facts — the displayed digest
/// as `subject`, a stored `TimeoutRecord`'s address as `detail` — and puts it
/// through this same broker. Deliberately the same door: a timeout must clear
/// the same nonce and displayed-digest checks a real upload does, spend the same
/// consume-once order, and reach the same retry and wedge accounting rather than
/// a parallel authority. Only *most* of the same door: an uploaded result for a
/// stage with no executor-dispatch lifecycle is refused here as
/// [`IntakeRefusal::OutOfLineStage`], but the synthesised side never gets that
/// far, because the reactor has no timeout verdict to build one from for such a
/// stage and leaves the order outstanding instead.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UploadedEvidence {
    /// The nonce the upload claims to answer.
    pub nonce: Nonce,
    /// The digest the upload claims its evidence is about.
    pub subject: Digest,
    /// The verdict the upload carries.
    pub verdict: StageVerdict,
    /// The supporting artifact (the check output, the review record).
    pub detail: Digest,
    /// The candidate the run captured (ADR-0152), authoritative from the port
    /// reference like the nonce — host-recorded state, never name-decoded.
    pub candidate: Option<CandidateRef>,
    /// The review critic's findings prose (#3656), authoritative from the port
    /// reference like the candidate. Persisted keyed by the order's member on a
    /// failing review so a Refine re-entry is directed by it.
    pub findings: Option<String>,
    /// The exact failed `verify.check` members (ADR-0178), decoded from the
    /// artifact name's mask — the same value the backend projected onto the
    /// port reference, which either composes that mask (local) or reads it
    /// (Actions). Nonempty only for a failed member Verify or `AggregateVerify` —
    /// the invariant `verifier_failure_refusal` below is what enforces that.
    pub failed_verifiers: VerifyFailureSet,
    /// What the attempt cost (#4679), authoritative from the port reference like
    /// the candidate. The study lane admits it against the same order — but
    /// *without* consuming, before the verdict admit below consumes — so an
    /// attempt's price is recorded whatever its verdict was. `None` is an
    /// unmeasured attempt, which writes no row rather than a zero one.
    pub cost: Option<StudyCost>,
    /// Per-call usage when the harness reported it. Rides with `cost` into the
    /// study admit so a banded price row can charge each call, not the sum.
    pub calls: Option<Vec<StudyCall>>,
    /// Session-reuse arm from `evidence.json`, when the backend read it.
    pub session_reuse_arm: Option<String>,
    /// Micro-USD saved by the reuse arm against its counterfactual.
    pub session_reuse_saved_micro_usd: Option<u64>,
    /// Peak resident bytes from `evidence.json`.
    pub peak_resident_bytes: Option<u64>,
    /// Paths the candidate changed that no declared-surface glob covers
    /// (ADR-0209). Host-recorded state copied off the port reference, like
    /// `findings`. Empty unless the containment overlay named a violation.
    pub violating_paths: Vec<String>,
}

/// Why the broker refused an upload without touching the reducer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IntakeRefusal {
    /// No outstanding order names this nonce — the upload was fabricated, or its
    /// order was already consumed (a replay).
    UnknownNonce(Nonce),
    /// The order's stored row is corrupt (a digest column is not 32 bytes).
    CorruptOrder(Nonce),
    /// The upload's bound digest is not the one the order displayed — evidence
    /// never validates a digest it does not name.
    DigestMismatch {
        /// The digest the matched order displayed.
        displayed: Digest,
        /// The digest the upload actually claimed.
        claimed: Digest,
    },
    /// The order's stage is not a dispatched member stage (Construct / Verify /
    /// the repair-only Refine / the fold-conflict Reconcile) or a bloom-level
    /// aggregate gate — a well-formed dispatch only ever carries one of those,
    /// so this is a corrupt order. Refused rather than silently integrated.
    OutOfLineStage(StageId),
    /// The typed verifier set disagrees with the stored stage/verdict: only a
    /// failed member Verify or `AggregateVerify` may carry a set.
    InvalidVerifierFailures {
        /// The outstanding order's stored stage.
        stage: StageId,
        /// The upload's claimed verdict.
        verdict: StageVerdict,
        /// The set that violated the stage/verdict contract.
        failed_verifiers: VerifyFailureSet,
    },
    /// An executor-fault verdict arrived against a stage that has no
    /// environment-fault lifecycle. Dispatched member stages (Construct /
    /// Verify / Refine / Reconcile) and `AggregateReview` have one
    /// (ADR-0195 / ADR-0176); every other stage refuses rather than being
    /// given unratified semantics by admission.
    ExecutorFaultOutOfStage(StageId),
}

/// An accepted attempt result: the reducer [`Event`] the upload normalized to
/// (an integrating pass, typed Verify failure, ordinary member completion,
/// or parked question) and the [`Admit`] wire payload for #3497's
/// `aether.bloomery.admit` ingress.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Admission {
    /// The `aether.bloomery.admit` payload to send to the control core.
    pub admit: Admit,
    /// The decoded event the admit carries.
    pub event: Event,
}

/// The broker's verdict on one upload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdmitDecision {
    /// The upload's provenance and binding both hold — the order was consumed
    /// and the attempt result is ready to admit. Boxed: an [`Admission`] carries
    /// a whole reducer [`Event`], far larger than the refusal variant.
    Admitted(Box<Admission>),
    /// The upload was refused; the reducer is untouched and the order (on a
    /// digest mismatch) stays live for the honest worker.
    Refused(IntakeRefusal),
}

/// A failure that is neither a clean accept nor a clean refuse — the durable
/// store faulted, or an event that should always encode did not.
#[derive(Debug)]
pub enum IntakeError {
    /// The `SQLite` registry read/consume faulted.
    Store(rusqlite::Error),
    /// Encoding the admitted event to wire bytes failed.
    Encode(WireError),
}

impl fmt::Display for IntakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "evidence intake store error: {error}"),
            Self::Encode(error) => write!(f, "evidence intake event encode error: {error}"),
        }
    }
}

impl Error for IntakeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Encode(error) => Some(error),
        }
    }
}

impl From<rusqlite::Error> for IntakeError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl From<WireError> for IntakeError {
    fn from(error: WireError) -> Self {
        Self::Encode(error)
    }
}

/// Whether a non-parked stage verdict passes its completion gate (#3505). A
/// passing gate advances the member; a failing one re-dispatches within the retry
/// budget. `Parked` never reaches here — it routes to the `Question` hold path
/// (ADR-0151) before this is consulted.
fn verdict_passed(verdict: StageVerdict) -> bool {
    // Approved / VerificationPassed pass the gate; VerificationFailed and
    // ReviewFinding fail it. A parked verdict routes to the Question path before
    // the gate is consulted, so a stray one here reads as non-passing — never a
    // false advance.
    matches!(verdict, StageVerdict::Approved | StageVerdict::VerificationPassed)
}

// The ADR-0178 transport invariant, kept outside `admit_uploaded` so the trust
// boundary's main path stays readable: a failed member Verify or AggregateVerify
// is the result a set belongs to (they share the `verify.check` producer), and
// every other stage/verdict pair must carry none.
//
// A failed member Verify may carry the *empty* set, and that is a meaning rather
// than a hole: the reducer reads it as a gate that rendered no verdict and
// re-runs Verify over the untouched candidate. Refusing it here instead left the
// one producer that has to say it — the fail-closed evidence a lane that exited
// without writing any leaves behind — with nowhere to go but the order's
// execution limit, an hour later.
fn verifier_failure_refusal(stage: StageId, upload: &UploadedEvidence) -> Option<IntakeRefusal> {
    let valid = match (stage, upload.verdict) {
        (StageId::Verify | StageId::AggregateVerify, StageVerdict::VerificationFailed) => true,
        _ => upload.failed_verifiers.is_empty(),
    };
    (!valid).then_some(IntakeRefusal::InvalidVerifierFailures {
        stage,
        verdict: upload.verdict,
        failed_verifiers: upload.failed_verifiers,
    })
}

/// Decompose a failing aggregate verdict's findings against the bloom's
/// persisted work-order roster — exactly the workpiece ids the critic's
/// `## Task — {workpiece}` prompt sections named, so the tag vocabulary is
/// self-consistent with what the critic was shown. `None` for a passing
/// verdict or one carrying no findings.
fn aggregate_decomposition(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    upload: &UploadedEvidence,
    passed: bool,
) -> Result<Option<FindingsDecomposition>, IntakeError> {
    if passed {
        return Ok(None);
    }
    let Some(findings) = upload.findings.as_deref() else {
        return Ok(None);
    };
    let members: Vec<String> = store
        .list_dispatch_descriptions(record.bloom.0.as_bytes())?
        .into_iter()
        .map(|(workpiece, _)| workpiece)
        .filter(|workpiece| !workpiece.is_empty())
        .collect();
    Ok(Some(decompose_findings(findings, &members)))
}

/// Persist a consumed aggregate verdict's findings (ADR-0153). A pass clears
/// the bloom row. A failure freezes the full set on the bloom row under the
/// empty workpiece key — verbatim on the first failure, appended under its own
/// label on a later one (the delta-confirm at the ceiling), never clobbering
/// the set the members were re-opened against — and a complete decomposition
/// additionally slices each owner's blocks into its member row, which the
/// Refine re-entry prompt prefers over the bloom row.
fn persist_aggregate_findings(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    upload: &UploadedEvidence,
    decomposition: Option<&FindingsDecomposition>,
) -> rusqlite::Result<()> {
    if verdict_passed(upload.verdict) {
        return store.clear_review_findings(record.bloom.0.as_bytes(), "");
    }
    let Some(findings) = &upload.findings else {
        return Ok(());
    };
    let frozen = store
        .lookup_review_findings(record.bloom.0.as_bytes(), "")?
        .map_or_else(|| findings.clone(), |existing| format!("{existing}\n\n## Delta-confirm findings\n\n{findings}"));
    store.record_review_findings(record.bloom.0.as_bytes(), "", &frozen)?;
    if let Some(decomposition) = decomposition
        && decomposition.is_complete()
    {
        for (workpiece, slice) in &decomposition.slices {
            store.record_review_findings(record.bloom.0.as_bytes(), workpiece, slice)?;
        }
    }
    Ok(())
}

/// Persist a consumed aggregate-verify verdict's findings on the composition
/// workpiece (#5098). The reserved Refine reads that row the way a member Refine
/// reads its own: a compiler diagnostic the fold produced is the work order the
/// weave repair never sealed.
fn persist_aggregate_verify_findings(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    upload: &UploadedEvidence,
) -> rusqlite::Result<()> {
    if verdict_passed(upload.verdict) {
        return store.clear_review_findings(record.bloom.0.as_bytes(), WorkpieceId::COMPOSITION);
    }
    let Some(findings) = &upload.findings else {
        return Ok(());
    };
    store.record_review_findings(record.bloom.0.as_bytes(), WorkpieceId::COMPOSITION, findings)
}

// The read-side reconstruction lives here beside its sole caller `admit_uploaded`;
// the write-side `to_stored` lives with `record_dispatch` in the dispatch module.
impl DispatchRecord {
    /// The typed record a stored row holds, or `None` when a column does not
    /// decode (a corrupt row, never a well-formed one). Also how the redispatch
    /// drain rebuilds a held order's lane for replay (#3664), which then
    /// overrides `nonce` so the replay is a distinct attempt.
    pub(crate) fn from_stored(order: &OutstandingOrder) -> Option<Self> {
        Some(Self {
            nonce: Nonce(order.nonce.clone()),
            bloom: BloomId(Digest::from_slice(&order.bloom)?),
            workpiece: WorkpieceId(order.workpiece.clone()),
            scope_revision: Digest::from_slice(&order.scope_revision)?,
            candidate: Digest::from_slice(&order.candidate)?,
            displayed_digest: Digest::from_slice(&order.displayed_digest)?,
            stage: from_bytes(&order.stage).ok()?,
            profile: from_bytes(&order.profile).ok()?,
            transformation: from_bytes(&order.transformation).ok()?,
            configs: from_bytes(&order.configs).ok()?,
        })
    }
}

/// The broker accept-gate + normalize → admit (#3502, the trust boundary).
///
/// Look the upload's nonce up in the outstanding-order registry; admit only when
/// the nonce names a live order **and** the evidence's bound digest is the one
/// the order displayed. On accept the order is consumed (so a replayed nonce
/// refuses) and the stage result normalizes — through the shape-only
/// [`normalize_stage_result`] — into an [`Evidence`]
/// bound to the displayed digest, wrapped in a [`ResolutionClaim`] built from the
/// matched row, and carried as a [`Fact::Integrate`]. A mismatch on either axis
/// refuses without touching the reducer.
///
/// # Errors
/// [`IntakeError::Store`] if the registry read/consume faulted, or
/// [`IntakeError::Encode`] if the admitted event failed to wire-encode.
/// The admission event for a whole-bloom aggregate-review verdict (ADR-0153) —
/// a bloom-level order, no member axis — paired with the findings decomposition
/// the caller persists after the consume.
///
/// A failing verdict's findings decompose against the ids the critic's own
/// prompt showed (the persisted work-order roster); a *complete* attribution
/// narrows the implication to the owning members, anything less leaves it empty
/// and the reducer expands the empty implication to every member (fail-closed
/// over-routing).
fn aggregate_review_event(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    upload: &UploadedEvidence,
    evidence: Evidence,
) -> Result<(Event, Option<FindingsDecomposition>), IntakeError> {
    let passed = verdict_passed(upload.verdict);
    let decomposition = aggregate_decomposition(store, record, upload, passed)?;
    let implicated = match &decomposition {
        Some(decomposition) if decomposition.is_complete() => {
            decomposition.owners().into_iter().map(WorkpieceId).collect()
        }
        _ => Vec::new(),
    };

    let event = Event {
        idempotency_key: AdmissionKey::AggregateReview.of(&record.nonce.0),
        fact: Fact::AggregateReviewCompleted {
            bloom: record.bloom,
            passed,
            evidence: advisory_evidence(upload, passed, evidence),
            implicated,
        },
    };
    Ok((event, decomposition))
}

/// Re-kind a *passing* composition review that still returned judgment findings
/// (#4961), so the reducer files them on the composition's findings channel on
/// its way to resolving the bloom.
///
/// The lane already decided the verdict from the classes the reviewer stated: a
/// review whose findings are all non-blocking advisories reports as a pass, and
/// what reaches here is that pass plus the prose it recorded. Read back through
/// the same domain-crate parser the lane derived the verdict with, so the format
/// has one spelling and this cannot disagree with the decision that produced it.
///
/// Only the kind moves — the same subject, the same detail artifact — exactly as
/// a bounced repair lap re-kinds its evidence. A pass with no advisories, and any
/// non-passing verdict, is untouched.
fn advisory_evidence(upload: &UploadedEvidence, passed: bool, evidence: Evidence) -> Evidence {
    let advisory = passed
        && upload.findings.as_deref().is_some_and(|prose| classify_findings(prose).advisories().next().is_some());

    if advisory {
        Evidence { kind: EvidenceKind::ReviewAdvisory, ..evidence }
    } else {
        evidence
    }
}

/// The admission event for an aggregate review whose executor could not judge
/// the fold (ADR-0176) — the sibling of [`aggregate_review_event`] for the one
/// verdict that is not a verdict.
///
/// No findings are decomposed and none are persisted: there is nothing to
/// attribute, because no candidate was read. The idempotency key is its own, so
/// a replayed fault is a no-op against the journal rather than colliding with
/// the completion key a later real verdict on the same order would carry.
fn aggregate_review_executor_fault_event(record: &DispatchRecord, evidence: Evidence) -> Event {
    Event {
        idempotency_key: AdmissionKey::AggregateReviewExecutorFault.of(&record.nonce.0),
        fact: Fact::AggregateReviewExecutorFault { bloom: record.bloom, evidence },
    }
}

/// The admission event for a dispatched member stage whose executor could not
/// judge the subject (ADR-0195) — the sibling of
/// [`aggregate_review_executor_fault_event`] for per-member gates.
///
/// The idempotency key is its own, so a replayed fault is a no-op against the
/// journal rather than colliding with the completion key a later real verdict
/// on the same order would carry.
fn member_executor_fault_event(record: &DispatchRecord, evidence: Evidence) -> Event {
    Event {
        idempotency_key: AdmissionKey::MemberExecutorFault.of(&record.nonce.0),
        fact: Fact::MemberExecutorFault {
            bloom: record.bloom,
            workpiece: record.workpiece.clone(),
            stage: record.stage,
            evidence,
        },
    }
}

/// Whether `stage` is a dispatched member gate that ADR-0195 gives an
/// executor-fault lifecycle.
fn admits_member_executor_fault(stage: StageId) -> bool {
    stage == StageId::Verify || admits_as_attempt_completed(stage)
}

/// The finding a weave repair was dispatched to repair: the composition's own
/// row when a previous bounce wrote one, else the bloom-scoped frozen set.
///
/// Exactly the resolution the work-order overlay uses when it assembles the
/// lap's `## Findings` section, and that is the whole justification for the
/// triage's strictness — the lap is judged against the text it was actually
/// shown, not against something the host reconstructed differently.
fn repair_finding(store: &mut dyn StoreBackend, record: &DispatchRecord) -> rusqlite::Result<Option<String>> {
    let bloom = record.bloom.0.as_bytes();
    if let Some(findings) = store.lookup_review_findings(bloom, &record.workpiece.0)? {
        return Ok(Some(findings));
    }
    store.lookup_review_findings(bloom, "")
}

/// Triage a passing weave repair against the finding it was dispatched for
/// (#4959), or [`TriageVerdict::NotInspected`] when the bloom holds no finding
/// for it to have repaired.
fn triage_repair_lap(store: &mut dyn StoreBackend, record: &DispatchRecord) -> Result<TriageVerdict, IntakeError> {
    let Some(finding) = repair_finding(store, record)? else {
        return Ok(TriageVerdict::NotInspected);
    };

    Ok(triage_repair(&finding, store.lookup_capture_diff(&record.nonce.0)?.as_deref()))
}

/// Re-thread the finding for the lap that follows a bounce: the same text, plus
/// a section naming what the bounced lap failed to touch.
///
/// On the workpiece's own row, never the bloom-scoped one — the frozen aggregate
/// set is what a delta-confirm review is framed against, and a note about one
/// lane's lap has no business in it. The next dispatch's overlay prefers the
/// workpiece row, so the note is what that lap reads.
fn thread_triage_note(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    finding: &str,
    named: &[String],
) -> rusqlite::Result<()> {
    let threaded = format!("{finding}\n\n{}", triage_note(named));
    store.record_review_findings(record.bloom.0.as_bytes(), &record.workpiece.0, &threaded)
}

/// Whether a completed attempt at `stage` is a member-line result the reducer
/// advances, retries, or wedges — Construct (a successor in the member line)
/// and the off-line Refine / Reconcile repairs. Reconcile has no successor in
/// `MEMBER_LINE`; naming it here is what stops a passing candidate being
/// refused as [`IntakeRefusal::OutOfLineStage`].
/// The terminal-Verify facts: a pass integrates, a preflight-only miss is
/// a host fault (#5020), a set carrying containment is
/// `ContainmentRefused` (ADR-0209), and every other failing set is a
/// candidate `VerifyFailed`.
fn verify_event(record: &DispatchRecord, upload: &UploadedEvidence, evidence: Evidence) -> Event {
    if verdict_passed(upload.verdict) {
        let claim = ResolutionClaim {
            workpiece: record.workpiece.clone(),
            scope_revision: record.scope_revision,
            candidate: record.candidate,
            evidence,
        };
        return Event {
            idempotency_key: AdmissionKey::Integrate.of(&record.nonce.0),
            fact: Fact::Integrate { bloom: record.bloom, claim },
        };
    }
    // Same admission key as a candidate failure so the dispatch is still
    // accounted for; the fact is what distinguishes the cause.
    Event {
        idempotency_key: AdmissionKey::VerifyFailed.of(&record.nonce.0),
        fact: if upload.failed_verifiers == VerifyFailureSet::one(VerifyFailure::Preflight) {
            Fact::VerifyHostFault {
                bloom: record.bloom,
                workpiece: record.workpiece.clone(),
                evidence,
                findings: upload.findings.clone().unwrap_or_default(),
            }
        } else if upload.failed_verifiers.contains(VerifyFailure::Containment) {
            Fact::ContainmentRefused {
                bloom: record.bloom,
                workpiece: record.workpiece.clone(),
                evidence,
                failed_verifiers: upload.failed_verifiers,
                violating_paths: upload.violating_paths.clone(),
            }
        } else {
            Fact::VerifyFailed {
                bloom: record.bloom,
                workpiece: record.workpiece.clone(),
                evidence,
                failed_verifiers: upload.failed_verifiers,
            }
        },
    }
}

fn admits_as_attempt_completed(stage: StageId) -> bool {
    StageCatalog::next_member_stage(stage).is_some() || matches!(stage, StageId::Refine | StageId::Reconcile)
}

pub fn admit_uploaded(store: &mut dyn StoreBackend, upload: &UploadedEvidence) -> Result<AdmitDecision, IntakeError> {
    let Some(stored) = store.lookup_order(&upload.nonce.0)? else {
        // Fabricated, or the order was already consumed (a replay).
        return Ok(AdmitDecision::Refused(IntakeRefusal::UnknownNonce(upload.nonce.clone())));
    };
    let Some(record) = DispatchRecord::from_stored(&stored) else {
        return Ok(AdmitDecision::Refused(IntakeRefusal::CorruptOrder(upload.nonce.clone())));
    };
    if let Some(refusal) = verifier_failure_refusal(record.stage, upload) {
        return Ok(AdmitDecision::Refused(refusal));
    }
    // ADR-0195 admits ExecutorFault for dispatched member stages and the
    // aggregate review. A fault claimed against any other stage is refused
    // here rather than routed — the refusal precedes the consume, so the
    // order stays live and an honest result on it can still land.
    if upload.verdict == StageVerdict::ExecutorFault
        && record.stage != StageId::AggregateReview
        && !admits_member_executor_fault(record.stage)
    {
        return Ok(AdmitDecision::Refused(IntakeRefusal::ExecutorFaultOutOfStage(record.stage)));
    }
    let observed = StageResult { subject: upload.subject, verdict: upload.verdict, detail: upload.detail };
    let evidence = match normalize_stage_result(&record.displayed_digest, &observed) {
        Ok(evidence) => evidence,
        // A mismatch is a lie about which digest the evidence names; refuse and
        // leave the order live so the honest worker can still deliver.
        Err(InwardError::DigestMismatch { displayed, claimed }) => {
            return Ok(AdmitDecision::Refused(IntakeRefusal::DigestMismatch { displayed, claimed }));
        }
    };
    // Provenance (a real outstanding order) and binding (the displayed digest)
    // both hold. Build the whole admission — including the fallible event encode
    // — *before* consuming the order: consuming first and then failing the encode
    // would lose the evidence with the nonce already spent and no retry.
    //
    // Route the attempt result by stage (#3505, ADR-0153):
    //
    // - A parked attempt normalizes to a Question evidence admitted through
    //   Fact::AdmitEvidence (ADR-0151) — never a Fact::Integrate, and never a
    //   failure: the order is consumed, but the parked outcome burns no stage
    //   retry, because a decision pending is not a defect.
    // - The terminal Verify admits by verdict: a *passing* one produces the
    //   member's ResolutionClaim through Fact::Integrate (the existing integrate
    //   path) — the verification evidence binds the exact candidate tree, which
    //   is what reduce_integrate re-checks; a *failing* one admits as
    //   Fact::VerifyFailed with the validated typed set so the reducer can
    //   distinguish a new identity from a repeated defect and account the
    //   member's repair roll deterministically.
    // - Any other dispatched member stage (Construct — one with a successor in
    //   the member line — or the off-line Refine / Reconcile repairs) admits as
    //   Fact::AttemptCompleted: the reducer advances the member's cursor on a
    //   passing verdict, re-dispatches the stage within its retry budget on a
    //   failing one, and wedges once the budget is exhausted.
    // - An out-of-line stage (Review included — the model review is the
    //   bloom-level AggregateReview position, never a member dispatch) is a
    //   corrupt order and is refused, never routed to Integrate.
    // The repair-lap triage (#4959), read before anything is consumed: a passing
    // weave repair whose diff changes nothing its finding names is admitted as a
    // *failing* lap instead, so the dodge spends the retry budget a refused lap
    // spends and never buys the judge round it was trying to reach. Everything
    // uncertain passes — see the `triage` module for the rules and the reason
    // they lean that way.
    let triage = if record.is_composition_refine() && verdict_passed(upload.verdict) {
        triage_repair_lap(store, &record)?
    } else {
        TriageVerdict::NotInspected
    };
    let mut aggregate_findings: Option<FindingsDecomposition> = None;
    // The question digest a parked admission raises its hold under (the evidence
    // detail, per the reducer's `RecordEvidence` fold) — the key the held order is
    // filed under below, captured before the evidence moves into the fact.
    let mut parked_under = None;
    let event = if upload.verdict == StageVerdict::Parked {
        parked_under = Some(evidence.detail);
        Event {
            idempotency_key: AdmissionKey::Park.of(&record.nonce.0),
            fact: Fact::AdmitEvidence { bloom: record.bloom, evidence },
        }
    } else if upload.verdict == StageVerdict::ExecutorFault && admits_member_executor_fault(record.stage) {
        member_executor_fault_event(&record, evidence)
    } else if record.stage == StageId::Verify {
        verify_event(&record, upload, evidence)
    } else if admits_as_attempt_completed(record.stage) {
        Event {
            idempotency_key: AdmissionKey::Attempt.of(&record.nonce.0),
            fact: Fact::AttemptCompleted {
                bloom: record.bloom,
                workpiece: record.workpiece.clone(),
                stage: record.stage,
                passed: verdict_passed(upload.verdict) && !triage.bounces(),
                // A bounced lap's evidence is about the *lap*, not about the
                // candidate: same subject and same supporting artifact, filed
                // under the kind that makes a dodge countable in the journal.
                evidence: if triage.bounces() {
                    Evidence { kind: EvidenceKind::RepairTriage, ..evidence }
                } else {
                    evidence
                },
                candidate: upload.candidate,
            },
        }
    } else if record.stage == StageId::AggregateReview {
        if upload.verdict == StageVerdict::ExecutorFault {
            aggregate_review_executor_fault_event(&record, evidence)
        } else {
            let (event, decomposition) = aggregate_review_event(store, &record, upload, evidence)?;
            aggregate_findings = decomposition;
            event
        }
    } else if record.stage == StageId::AggregateVerify {
        // The whole-bloom mechanical verdict — a bloom-level order, no member
        // axis and no implication: a compiler names no owners, so the reducer
        // re-opens every member on a failure.
        Event {
            idempotency_key: AdmissionKey::AggregateVerify.of(&record.nonce.0),
            fact: Fact::AggregateVerifyCompleted {
                bloom: record.bloom,
                passed: verdict_passed(upload.verdict),
                evidence,
            },
        }
    } else {
        // An out-of-line stage never comes from a well-formed dispatch; refuse it
        // rather than folding a non-line result into the member's resolution. The
        // order stays live (unconsumed) — the refusal precedes the consume below.
        return Ok(AdmitDecision::Refused(IntakeRefusal::OutOfLineStage(record.stage)));
    };
    let admit = Admit { event: to_vec(&event)? };
    // A parked attempt's order is re-filed under the question it raised, *before*
    // the consume below spends it (#3664). The admission is what raises the hold,
    // and the answer that releases it names only the question — so without this
    // row the redispatch has no lane to replay and the bloom wedges on a decision
    // that was made. Ordering matters in one direction only: a write fault here
    // leaves the order live and the upload retryable, whereas filing after the
    // consume would spend the order into a state no retry can reach. The
    // converse orphan is inert — a row under a question whose evidence never
    // admitted raises no hold, so nothing ever looks it up.
    if let Some(question) = parked_under {
        store.record_parked_question(question.as_bytes(), &stored)?;
    }
    // Consume-once, only now that the admission is fully constructed. A lost race
    // to consumption between the lookup and here reads as a replay.
    if !store.consume_order(&record.nonce.0)? {
        return Ok(AdmitDecision::Refused(IntakeRefusal::UnknownNonce(upload.nonce.clone())));
    }
    persist_consumed(store, &record, upload, &triage, aggregate_findings.as_ref())?;

    Ok(AdmitDecision::Admitted(Box::new(Admission { admit, event })))
}

/// The writes a *consumed* order leaves behind — the findings channel, and the
/// spent lap's capture diff.
///
/// After the consume on purpose, all of it: a refused upload writes nothing.
///
/// - A failing Verify's findings (the mechanical failure output) persist keyed by
///   the member so the Refine repair re-entry is directed by them; a passing
///   Verify clears the stale row (#3656, ADR-0153).
/// - A failing `AggregateVerify` persists the same mechanical findings on the
///   composition workpiece (#5098): that is the reserved Refine's findings
///   channel, and a subject-only weave repair is what happens when they never
///   land. A passing `AggregateVerify` clears that row so a later review-triggered
///   repair is not still directed by a compiler diagnostic the fold already
///   cleared.
/// - A failing aggregate verdict freezes its set bloom-scoped and slices it per
///   owner. An executor fault writes none and clears none: the frozen set belongs
///   to the last verdict that actually judged the fold, and a host outage is not
///   a reason to lose it or to add to it.
/// - A bounced repair lap (#4959) re-threads its finding with a section naming
///   what it failed to touch — otherwise the next lap reads the identical prose
///   and repeats itself until the budget is gone.
/// - The lap's capture diff is dropped either way: it has been read, and the
///   order it belongs to is spent.
fn persist_consumed(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    upload: &UploadedEvidence,
    triage: &TriageVerdict,
    aggregate_findings: Option<&FindingsDecomposition>,
) -> Result<(), IntakeError> {
    store.clear_capture_diff(&record.nonce.0)?;
    if let TriageVerdict::Dodged(named) = triage {
        tracing::warn!(
            target: "aether_chassis_bloomery::intake",
            nonce = %upload.nonce.0,
            workpiece = %record.workpiece.0,
            named = %named.join(", "),
            "repair lap changed nothing its finding names; bouncing it without a re-judge",
        );
        if let Some(finding) = repair_finding(store, record)? {
            thread_triage_note(store, record, &finding, named)?;
        }
    }
    if record.stage == StageId::Verify && upload.verdict != StageVerdict::ExecutorFault {
        if verdict_passed(upload.verdict) {
            store.clear_review_findings(record.bloom.0.as_bytes(), &record.workpiece.0)?;
        } else if let Some(findings) = &upload.findings {
            store.record_review_findings(record.bloom.0.as_bytes(), &record.workpiece.0, findings)?;
        }
    } else if record.stage == StageId::AggregateVerify {
        persist_aggregate_verify_findings(store, record, upload)?;
    } else if record.stage == StageId::AggregateReview && upload.verdict != StageVerdict::ExecutorFault {
        persist_aggregate_findings(store, record, upload, aggregate_findings)?;
    }
    Ok(())
}
