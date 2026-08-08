//! The evidence-admission broker: the trust boundary that accepts an uploaded
//! attempt result only when its nonce names a live order and its bound digest is
//! the one the order displayed.

use std::error::Error;
use std::fmt;

use aether_bloomery::{
    Admit, BloomId, CandidateRef, Digest, Event, Fact, IdempotencyKey, InwardError, Nonce, ResolutionClaim,
    StageCatalog, StageId, StageResult, StageVerdict, WorkpieceId, normalize_stage_result,
};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};

use super::dispatch::DispatchRecord;
use crate::bloomery::findings::{FindingsDecomposition, decompose_findings};
use crate::store::{OutstandingOrder, StoreBackend};

/// What a worker uploaded as an attempt result: the nonce it claims to answer,
/// the digest its evidence is about (what the observation *claims*), the
/// verdict, and the supporting detail artifact. The broker checks the claimed
/// `subject` against the digest the matched order displayed.
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
    /// The order's stage is not in the dispatched member line (Construct / Verify /
    /// Refine / Review) — a well-formed dispatch only ever carries a member-line
    /// stage, so this is a corrupt order. Refused rather than silently integrated.
    OutOfLineStage(StageId),
}

/// An accepted attempt result: the reducer [`Event`] the upload normalized to
/// (a [`Fact::Integrate`] for a resolving verdict, or a [`Fact::AdmitEvidence`]
/// carrying a `Question` for a parked one) and the [`Admit`] wire payload for
/// #3497's `aether.bloomery.admit` ingress.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Admission {
    /// The `aether.bloomery.admit` payload to send to the control core.
    pub admit: Admit,
    /// The decoded event the admit carries — a [`Fact::Integrate`] or a
    /// [`Fact::AdmitEvidence`] (parked).
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
/// [`normalize_stage_result`] — into an [`Evidence`](aether_bloomery::Evidence)
/// bound to the displayed digest, wrapped in a [`ResolutionClaim`] built from the
/// matched row, and carried as a [`Fact::Integrate`]. A mismatch on either axis
/// refuses without touching the reducer.
///
/// # Errors
/// [`IntakeError::Store`] if the registry read/consume faulted, or
/// [`IntakeError::Encode`] if the admitted event failed to wire-encode.
pub fn admit_uploaded(store: &mut dyn StoreBackend, upload: &UploadedEvidence) -> Result<AdmitDecision, IntakeError> {
    let Some(stored) = store.lookup_order(&upload.nonce.0)? else {
        // Fabricated, or the order was already consumed (a replay).
        return Ok(AdmitDecision::Refused(IntakeRefusal::UnknownNonce(upload.nonce.clone())));
    };
    let Some(record) = DispatchRecord::from_stored(&stored) else {
        return Ok(AdmitDecision::Refused(IntakeRefusal::CorruptOrder(upload.nonce.clone())));
    };
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
    //   Fact::AttemptCompleted so the reducer routes the member into the Refine
    //   repair re-entry within the repair ceiling and wedges on exhaustion — the
    //   completion gate applies across the whole member line, so a failing
    //   verify is never silently integrated.
    // - Any other dispatched member stage (Construct — one with a successor in
    //   the member line — or the repair-only Refine) admits as
    //   Fact::AttemptCompleted: the reducer advances the member's cursor on a
    //   passing verdict, re-dispatches the stage within its retry budget on a
    //   failing one, and wedges once the budget is exhausted.
    // - An out-of-line stage (Review included — the model review is the
    //   bloom-level AggregateReview position, never a member dispatch) is a
    //   corrupt order and is refused, never routed to Integrate.
    let mut aggregate_findings: Option<FindingsDecomposition> = None;
    // The question digest a parked admission raises its hold under (the evidence
    // detail, per the reducer's `RecordEvidence` fold) — the key the held order is
    // filed under below, captured before the evidence moves into the fact.
    let mut parked_under = None;
    let event = if upload.verdict == StageVerdict::Parked {
        parked_under = Some(evidence.detail);
        Event {
            idempotency_key: IdempotencyKey(format!("aether.bloomery.park:{}", record.nonce.0)),
            fact: Fact::AdmitEvidence { bloom: record.bloom, evidence },
        }
    } else if record.stage == StageId::Verify {
        if verdict_passed(upload.verdict) {
            let claim = ResolutionClaim {
                workpiece: record.workpiece.clone(),
                scope_revision: record.scope_revision,
                candidate: record.candidate,
                evidence,
            };
            Event {
                idempotency_key: IdempotencyKey(format!("aether.bloomery.integrate:{}", record.nonce.0)),
                fact: Fact::Integrate { bloom: record.bloom, claim },
            }
        } else {
            Event {
                idempotency_key: IdempotencyKey(format!("aether.bloomery.attempt:{}", record.nonce.0)),
                fact: Fact::AttemptCompleted {
                    bloom: record.bloom,
                    workpiece: record.workpiece.clone(),
                    stage: record.stage,
                    passed: false,
                    evidence,
                    // Threaded for the journal's completeness; the reducer never
                    // adopts a failing attempt's capture (ADR-0152).
                    candidate: upload.candidate,
                },
            }
        }
    } else if StageCatalog::next_member_stage(record.stage).is_some() || record.stage == StageId::Refine {
        Event {
            idempotency_key: IdempotencyKey(format!("aether.bloomery.attempt:{}", record.nonce.0)),
            fact: Fact::AttemptCompleted {
                bloom: record.bloom,
                workpiece: record.workpiece.clone(),
                stage: record.stage,
                passed: verdict_passed(upload.verdict),
                evidence,
                candidate: upload.candidate,
            },
        }
    } else if record.stage == StageId::AggregateReview {
        // The whole-bloom aggregate verdict (ADR-0153) — a bloom-level order, no
        // member axis. A failing verdict's findings decompose against the ids
        // the critic's own prompt showed (the persisted work-order roster); a
        // *complete* attribution narrows the implication to the owning members,
        // anything less leaves it empty and the reducer expands the empty
        // implication to every member (fail-closed over-routing).
        let passed = verdict_passed(upload.verdict);
        let decomposition = aggregate_decomposition(store, &record, upload, passed)?;
        let implicated = match &decomposition {
            Some(decomposition) if decomposition.is_complete() => {
                decomposition.owners().into_iter().map(WorkpieceId).collect()
            }
            _ => Vec::new(),
        };
        aggregate_findings = decomposition;
        Event {
            idempotency_key: IdempotencyKey(format!("aether.bloomery.aggregate_review:{}", record.nonce.0)),
            fact: Fact::AggregateReviewCompleted { bloom: record.bloom, passed, evidence, implicated },
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
    // A failing Verify's findings — the mechanical failure output — persist
    // keyed by the member so the Refine repair re-entry is directed by them; a
    // passing Verify clears the stale row (#3656, ADR-0153). Only after the
    // consume — a refused upload writes nothing.
    if record.stage == StageId::Verify {
        if verdict_passed(upload.verdict) {
            store.clear_review_findings(record.bloom.0.as_bytes(), &record.workpiece.0)?;
        } else if let Some(findings) = &upload.findings {
            store.record_review_findings(record.bloom.0.as_bytes(), &record.workpiece.0, findings)?;
        }
    } else if record.stage == StageId::AggregateReview {
        persist_aggregate_findings(store, &record, upload, aggregate_findings.as_ref())?;
    }
    Ok(AdmitDecision::Admitted(Box::new(Admission { admit, event })))
}
