//! Study-evidence intake: normalizing a runner **result record** into durable,
//! digest-addressed study evidence and a rebuildable per-bloom study index
//! (ADR-0149 §The bloom, issue #3523).
//!
//! This is the study back half of #3511's runner lane. A `construct.implement`
//! attempt uploads one nonce-tagged result record (the cost / tokens / turns /
//! duration `scripts/agent-usage-record.mjs` derives); this module *accepts* it
//! and turns it into gradeable evidence, under the same trust boundary #3502's
//! evidence intake defends — with one deliberate difference.
//!
//! # Reuse #3502's accept-gate, but match without consuming
//!
//! Admission reuses #3502's broker predicate: a study upload is admitted only
//! when its idempotency nonce names a live
//! [`OutstandingOrder`](crate::store::OutstandingOrder) **and** its
//! bound digest equals the digest that order displayed. A fabricated or
//! wrong-digest upload is refused and the store is untouched — the value
//! vocabulary's "no evidence grades a digest it does not name" invariant,
//! enforced by the same [`normalize_study_result`] path the stage normalizer
//! uses for verdicts.
//!
//! The difference is consume-once. #3502's [`admit_uploaded`](super::admit_uploaded)
//! *consumes* the outstanding order on accept (a replayed nonce refuses). A
//! study record and the terminal resolution claim share one attempt/nonce, so
//! the study lane reuses the *match* predicate ([`StoreBackend::lookup_order`])
//! but never the *consume* arm: the resolution claim stays the consuming
//! terminal, and a study admit leaves the order live.
//!
//! # The bytes are the truth; the index is a projection
//!
//! An accepted study upload does **not** enter the reducer (no new `Fact`, no
//! new `EvidenceKind` — that reducer-side admission is deferred to #3525). It
//! normalizes to a [`StudyRecord`] whose canonical bytes are `put` into
//! `aether.artifacts` with the graded attempt digest as a derivation parent, and
//! a `(bloom, attempt) → study-artifact` row is written to the `study_index`
//! projection ([`StoreBackend::record_study`]). That projection is always
//! rebuildable from the artifact bytes alone ([`rebuild_study_index`]) — it can
//! be dropped and reconstructed, and it survives the outstanding order being
//! consumed by the later resolution claim, because the bloom and attempt it
//! keys on live in the `StudyRecord`'s own bytes.

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

use aether_bloomery::{BloomId, Digest, Nonce, StudyCost, StudyRecord};
use aether_bloomery_github::{InwardError, StudyResult, normalize_study_result};
use aether_data::wire::{Error as WireError, from_bytes, to_vec};

use crate::artifacts::{ArtifactsCapabilityState, ArtifactsError, PutResult};
use crate::store::StoreBackend;

/// A runner result record a worker uploaded for a `construct.implement` attempt:
/// the nonce it answers, the attempt digest its cost is about, and the parsed
/// cost columns. The study sibling of #3502's
/// [`UploadedEvidence`](super::UploadedEvidence).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UploadedStudyRecord {
    /// The nonce the upload claims to answer.
    pub nonce: Nonce,
    /// The attempt digest the upload claims its cost grades.
    pub subject: Digest,
    /// The gradeable cost columns the upload carries.
    pub cost: StudyCost,
}

/// Why the study broker refused an upload without writing anything. Mirrors
/// #3502's [`IntakeRefusal`](super::IntakeRefusal), for the study lane.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StudyRefusal {
    /// No outstanding order names this nonce — the upload was fabricated, or its
    /// order was already consumed by the terminal resolution claim.
    UnknownNonce(Nonce),
    /// The matched order's stored row is corrupt (a digest column is not 32
    /// bytes).
    CorruptOrder(Nonce),
    /// The upload's bound digest is not the one the order displayed — a study
    /// record never grades a digest it does not name.
    DigestMismatch {
        /// The digest the matched order displayed.
        displayed: Digest,
        /// The digest the upload actually claimed.
        claimed: Digest,
    },
}

/// An accepted study upload: the study artifact's content-store digest and the
/// (bloom, attempt) it was indexed under.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StudyAdmission {
    /// The sealed bloom the graded attempt belongs to.
    pub bloom: BloomId,
    /// The exact attempt digest the study record grades.
    pub subject: Digest,
    /// The content-store digest of the `StudyRecord` artifact just `put`.
    pub study_artifact: String,
}

/// The study broker's verdict on one upload.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StudyAdmitDecision {
    /// The upload matched a live order and its digest bound: the study artifact
    /// was put and indexed. The order was **not** consumed.
    Admitted(StudyAdmission),
    /// The upload was refused; nothing was written and the order (on a digest
    /// mismatch) stays live.
    Refused(StudyRefusal),
}

/// A failure that is neither a clean accept nor a clean refuse — a durable
/// store or artifact operation faulted, or a record that should always encode
/// did not.
#[derive(Debug)]
pub enum StudyIntakeError {
    /// The `SQLite` registry / index read or write faulted.
    Store(rusqlite::Error),
    /// Encoding the study record to wire bytes failed.
    Encode(WireError),
    /// The artifacts content store could not persist the study record's bytes.
    Artifacts(ArtifactsError),
}

impl fmt::Display for StudyIntakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "study intake store error: {error}"),
            Self::Encode(error) => write!(f, "study record encode error: {error}"),
            Self::Artifacts(error) => write!(f, "study artifact store error: {error:?}"),
        }
    }
}

impl Error for StudyIntakeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Artifacts(_) => None,
        }
    }
}

impl From<rusqlite::Error> for StudyIntakeError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl From<WireError> for StudyIntakeError {
    fn from(error: WireError) -> Self {
        Self::Encode(error)
    }
}

/// The study broker accept-gate + normalize → put → index (issue #3523).
///
/// Reuses #3502's nonce+digest match against the outstanding-order registry, but
/// **matches without consuming** — the resolution claim stays the consuming
/// terminal. On accept the upload normalizes to a [`StudyRecord`] whose canonical
/// bytes are `put` into `aether.artifacts` (the graded attempt digest as a
/// derivation parent), and a `(bloom, attempt) → study-artifact` row is written
/// to the projection index. A mismatch on either axis refuses without writing.
///
/// # Errors
/// [`StudyIntakeError::Store`] if the registry/index faulted,
/// [`StudyIntakeError::Encode`] if the study record failed to wire-encode, or
/// [`StudyIntakeError::Artifacts`] if the content store could not persist it.
pub fn admit_study(
    store: &mut dyn StoreBackend,
    artifacts: &mut ArtifactsCapabilityState,
    upload: &UploadedStudyRecord,
) -> Result<StudyAdmitDecision, StudyIntakeError> {
    // Match WITHOUT consuming: a study upload and the terminal resolution claim
    // share an attempt/nonce, so the study lane reuses #3502's lookup predicate
    // but never its `consume_order` arm.
    let Some(stored) = store.lookup_order(&upload.nonce.0)? else {
        return Ok(StudyAdmitDecision::Refused(StudyRefusal::UnknownNonce(upload.nonce.clone())));
    };
    let (Some(bloom), Some(displayed)) =
        (Digest::from_slice(&stored.bloom).map(BloomId), Digest::from_slice(&stored.displayed_digest))
    else {
        return Ok(StudyAdmitDecision::Refused(StudyRefusal::CorruptOrder(upload.nonce.clone())));
    };
    // The binding check — the value-vocabulary invariant reused for study
    // records: a record grades only the digest the order displayed.
    let cost = match normalize_study_result(&displayed, &StudyResult { subject: upload.subject, cost: upload.cost }) {
        Ok(cost) => cost,
        Err(InwardError::DigestMismatch { displayed, claimed }) => {
            return Ok(StudyAdmitDecision::Refused(StudyRefusal::DigestMismatch { displayed, claimed }));
        }
    };
    // Normalize to the durable, content-addressed study artifact. Its bytes are
    // the truth; the index row below is a projection over them.
    let record = StudyRecord { bloom, subject: displayed, cost };
    let bytes = to_vec(&record)?;
    let study_artifact = match artifacts.put(&bytes, &[digest_to_parent(&displayed)]) {
        PutResult::Ok { digest } => digest,
        PutResult::Err { error } => return Err(StudyIntakeError::Artifacts(error)),
    };
    store.record_study(bloom.0.as_bytes(), displayed.as_bytes(), &study_artifact)?;
    Ok(StudyAdmitDecision::Admitted(StudyAdmission { bloom, subject: displayed, study_artifact }))
}

/// Rebuild the per-bloom study index from the artifact store alone (issue
/// #3523): drop every index row, then re-record one per study artifact found in
/// `aether.artifacts`. The index is a projection, never a second truth — its
/// rows are fully derivable from the `StudyRecord` bytes (each names the bloom
/// and attempt it keys on), so a rebuild reconstructs the identical set and
/// survives the outstanding order being consumed by the later resolution claim.
///
/// A stored artifact is a study record when its bytes canonically decode to a
/// [`StudyRecord`] *and* round-trip to exactly those bytes (the untagged wire
/// format needs the round-trip to reject a coincidental decode of some other
/// artifact) *and* its single recorded parent is the attempt digest that record
/// grades. Returns how many rows were rebuilt.
///
/// # Errors
/// [`StudyIntakeError::Store`] if clearing or re-recording the index faulted.
pub fn rebuild_study_index(
    store: &mut dyn StoreBackend,
    artifacts: &mut ArtifactsCapabilityState,
) -> Result<usize, StudyIntakeError> {
    store.clear_study_index()?;
    let mut rebuilt = 0;
    for entry in artifacts.scan() {
        let Ok(record) = from_bytes::<StudyRecord>(&entry.bytes) else {
            continue;
        };
        if to_vec(&record).ok().as_deref() != Some(entry.bytes.as_slice()) {
            continue;
        }
        if entry.parents != [digest_to_parent(&record.subject)] {
            continue;
        }
        store.record_study(record.bloom.0.as_bytes(), record.subject.as_bytes(), &entry.digest)?;
        rebuilt += 1;
    }
    Ok(rebuilt)
}

/// Render a digest as the lowercase-hex parent string the artifact store records
/// — the derivation edge from a study record to the attempt it grades.
fn digest_to_parent(digest: &Digest) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest.as_bytes() {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{BloomId, Digest, Nonce, StudyCost, StudyRecord, WorkpieceId};
    use aether_bloomery_github::parse_study_cost;
    use aether_data::wire::from_bytes;

    use super::{
        StudyAdmission, StudyAdmitDecision, StudyRefusal, UploadedStudyRecord, admit_study, rebuild_study_index,
    };
    use crate::artifacts::{ArtifactsCapabilityState, GetResult};
    use crate::bloomery::{DispatchRecord, record_dispatch};
    use crate::store::{SqliteStore, StoreBackend};

    fn store() -> SqliteStore {
        SqliteStore::open(":memory:").unwrap()
    }

    fn artifacts() -> (tempfile::TempDir, ArtifactsCapabilityState) {
        let dir = tempfile::tempdir().unwrap();
        let state = ArtifactsCapabilityState::open(dir.path()).unwrap();
        (dir, state)
    }

    fn cost() -> StudyCost {
        StudyCost {
            cost_micro_usd: 420_000,
            turns: 7,
            duration_millis: 123_456,
            input_tokens: 1_000,
            cache_write_tokens: 200,
            cache_write_1h_tokens: 150,
            cache_write_5m_tokens: 50,
            cache_read_tokens: 8_000,
            output_tokens: 900,
        }
    }

    // Record an outstanding order whose displayed digest is `attempt` — the
    // #3502 dispatch-record write side, driven directly (as #3502's own tests do).
    fn seed_order(store: &mut dyn StoreBackend, nonce: &str, bloom: BloomId, attempt: Digest) {
        let record = DispatchRecord {
            nonce: Nonce(nonce.to_owned()),
            bloom,
            workpiece: WorkpieceId("wp-study".to_owned()),
            scope_revision: Digest::from_bytes([2; 32]),
            candidate: attempt,
            displayed_digest: attempt,
            stage: aether_bloomery::StageId::Verify,
            transformation: aether_bloomery::Transformation::for_member_stage(
                aether_bloomery::StageId::Verify,
                attempt,
                Digest::from_bytes([0xC0; 32]),
            ),
            configs: aether_bloomery::ConfigRegistry::default(),
        };
        record_dispatch(store, &record).unwrap();
    }

    fn admit(
        store: &mut dyn StoreBackend,
        artifacts: &mut ArtifactsCapabilityState,
        nonce: &str,
        attempt: Digest,
    ) -> StudyAdmission {
        let upload = UploadedStudyRecord { nonce: Nonce(nonce.to_owned()), subject: attempt, cost: cost() };
        match admit_study(store, artifacts, &upload).unwrap() {
            StudyAdmitDecision::Admitted(admission) => admission,
            StudyAdmitDecision::Refused(refusal) => panic!("expected an admission, got a refusal: {refusal:?}"),
        }
    }

    fn artifact_bytes(artifacts: &mut ArtifactsCapabilityState, digest: &str) -> Vec<u8> {
        match artifacts.get(digest.to_owned()) {
            GetResult::Ok { bytes, .. } => bytes,
            GetResult::Err { error, .. } => panic!("study artifact absent: {error:?}"),
        }
    }

    #[test]
    fn a_matching_upload_admits_a_study_artifact_and_leaves_the_order_live() {
        // The accept path: a matched nonce + displayed digest normalizes to a
        // durable study artifact grading the attempt, indexed under the bloom —
        // and, unlike #3502's evidence intake, the order is NOT consumed.
        let mut store = store();
        let (_dir, mut artifacts) = artifacts();
        let bloom = BloomId(Digest::from_bytes([1; 32]));
        let attempt = Digest::from_bytes([5; 32]);
        seed_order(&mut store, "n-1", bloom, attempt);

        let admission = admit(&mut store, &mut artifacts, "n-1", attempt);
        assert_eq!(admission.bloom, bloom);
        assert_eq!(admission.subject, attempt);

        // The index resolves (bloom, attempt) → the study artifact digest.
        let looked = store.lookup_study(bloom.0.as_bytes(), attempt.as_bytes()).unwrap();
        assert_eq!(looked.as_deref(), Some(admission.study_artifact.as_str()));

        // The artifact's bytes are a StudyRecord grading exactly this attempt.
        let decoded: StudyRecord = from_bytes(&artifact_bytes(&mut artifacts, &admission.study_artifact)).unwrap();
        assert!(decoded.grades(&attempt), "the study artifact grades the attempt it names");
        assert_eq!(decoded.bloom, bloom);
        assert_eq!(decoded.cost, cost());

        // Match-without-consume: the order stays live for the terminal resolution
        // claim, and a second study admit is idempotent and still leaves it.
        assert!(store.lookup_order("n-1").unwrap().is_some());
        let again = admit(&mut store, &mut artifacts, "n-1", attempt);
        assert_eq!(again.study_artifact, admission.study_artifact, "identical bytes dedup to one artifact");
        assert!(store.lookup_order("n-1").unwrap().is_some());
    }

    #[test]
    fn an_unknown_nonce_is_refused_and_nothing_is_written() {
        let mut store = store();
        let (_dir, mut artifacts) = artifacts();
        let upload = UploadedStudyRecord {
            nonce: Nonce("fabricated".to_owned()),
            subject: Digest::from_bytes([5; 32]),
            cost: cost(),
        };
        assert!(matches!(
            admit_study(&mut store, &mut artifacts, &upload).unwrap(),
            StudyAdmitDecision::Refused(StudyRefusal::UnknownNonce(_))
        ));
        assert!(store.study_rows().unwrap().is_empty(), "a refused upload writes no index row");
    }

    #[test]
    fn a_right_nonce_with_the_wrong_digest_is_refused_and_the_store_is_untouched() {
        // The trust boundary: a live nonce but a digest other than the one
        // displayed is a lie — refused, no row written, and the order left live.
        let mut store = store();
        let (_dir, mut artifacts) = artifacts();
        let bloom = BloomId(Digest::from_bytes([1; 32]));
        let attempt = Digest::from_bytes([5; 32]);
        seed_order(&mut store, "n-2", bloom, attempt);

        let lying =
            UploadedStudyRecord { nonce: Nonce("n-2".to_owned()), subject: Digest::from_bytes([9; 32]), cost: cost() };
        match admit_study(&mut store, &mut artifacts, &lying).unwrap() {
            StudyAdmitDecision::Refused(StudyRefusal::DigestMismatch { displayed, claimed }) => {
                assert_eq!(displayed, attempt);
                assert_eq!(claimed, Digest::from_bytes([9; 32]));
            }
            other => panic!("expected DigestMismatch, got {other:?}"),
        }
        assert!(store.study_rows().unwrap().is_empty());
        assert!(store.lookup_order("n-2").unwrap().is_some());

        // The honest matching upload still lands.
        let honest = UploadedStudyRecord { subject: attempt, ..lying };
        assert!(matches!(admit_study(&mut store, &mut artifacts, &honest).unwrap(), StudyAdmitDecision::Admitted(_)));
    }

    #[test]
    fn dropping_the_study_index_rebuilds_the_same_rows_from_the_artifact_bytes() {
        // Tripwire: the study index is a projection over the artifact bytes,
        // never a second truth — dropping it and rebuilding from the store alone
        // reconstructs the identical rows, even after the outstanding orders are
        // consumed by the terminal resolution claim.
        let mut store = store();
        let (_dir, mut artifacts) = artifacts();
        let bloom_a = BloomId(Digest::from_bytes([1; 32]));
        let bloom_b = BloomId(Digest::from_bytes([4; 32]));
        let att_a = Digest::from_bytes([5; 32]);
        let att_b = Digest::from_bytes([6; 32]);
        seed_order(&mut store, "n-a", bloom_a, att_a);
        seed_order(&mut store, "n-b", bloom_b, att_b);
        admit(&mut store, &mut artifacts, "n-a", att_a);
        admit(&mut store, &mut artifacts, "n-b", att_b);

        let before = store.study_rows().unwrap();
        assert_eq!(before.len(), 2);

        // The resolution claim later consumes both orders — the study index must
        // not depend on them staying live.
        assert!(store.consume_order("n-a").unwrap());
        assert!(store.consume_order("n-b").unwrap());

        let rebuilt = rebuild_study_index(&mut store, &mut artifacts).unwrap();
        assert_eq!(rebuilt, 2);
        assert_eq!(store.study_rows().unwrap(), before, "rebuild reconstructs the identical rows");
    }

    #[test]
    fn end_to_end_study_intake_over_a_recorded_order_and_a_fake_result_record() {
        // Steps 5–6, against fakes: a recorded outstanding order (the #3502
        // dispatch-record write) + a fake nonce-tagged result record (the
        // agent-usage-record.mjs JSON) → normalized → study artifact put →
        // indexed under the bloom, naming the exact attempt digest; and a
        // mismatched upload → refused, store untouched.
        let mut store = store();
        let (_dir, mut artifacts) = artifacts();
        let bloom = BloomId(Digest::from_bytes([1; 32]));
        let attempt = Digest::from_bytes([5; 32]);
        seed_order(&mut store, "n-e2e", bloom, attempt);

        let json = br#"{"num_turns": 7, "cost_usd": 0.42, "duration_ms": 123456, "input": 1000,
            "cache_write": 200, "cache_write_1h": 150, "cache_write_5m": 50, "cache_read": 8000, "output": 900}"#;
        let cost = parse_study_cost(json).unwrap();
        let upload = UploadedStudyRecord { nonce: Nonce("n-e2e".to_owned()), subject: attempt, cost };
        let StudyAdmitDecision::Admitted(admission) = admit_study(&mut store, &mut artifacts, &upload).unwrap() else {
            panic!("the matching fake record admits");
        };

        // Indexed under the bloom, naming the exact attempt digest.
        let rows = store.study_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].bloom, bloom.0.as_bytes().to_vec());
        assert_eq!(rows[0].attempt_digest, attempt.as_bytes().to_vec());
        assert_eq!(rows[0].study_artifact, admission.study_artifact);

        // The artifact grades exactly that attempt with the parsed cost.
        let decoded: StudyRecord = from_bytes(&artifact_bytes(&mut artifacts, &admission.study_artifact)).unwrap();
        assert!(decoded.grades(&attempt));
        assert_eq!(decoded.cost.cost_micro_usd, 420_000);
        assert_eq!(decoded.cost.output_tokens, 900);

        // A mismatched upload → refused, store untouched.
        let bad = UploadedStudyRecord { nonce: Nonce("n-e2e".to_owned()), subject: Digest::from_bytes([9; 32]), cost };
        assert!(matches!(
            admit_study(&mut store, &mut artifacts, &bad).unwrap(),
            StudyAdmitDecision::Refused(StudyRefusal::DigestMismatch { .. })
        ));
        assert_eq!(store.study_rows().unwrap().len(), 1, "the refuse added no row");
    }
}
