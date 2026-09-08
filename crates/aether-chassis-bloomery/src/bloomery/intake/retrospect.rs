//! The coordinator-applied filing path (ADR-0216 §3): what a consumed
//! `retrospect.read` result leaves in the commission store.
//!
//! The reader gets exactly one write, and this is it. Each finding it will not
//! fix becomes an **open, unapproved** commission whose intent carries the
//! read's own [`Provenance::StageReceipt`](aether_bloomery::Provenance) and
//! names the read's derivation statement as its parent. Nothing else moves: no
//! scope revision, no approval row, no bearer token, no journal shape.
//!
//! # Why the write is here rather than through `POST /commissions`
//!
//! The bearer token gates the surface that can *approve* work. A lane holding
//! it could file and approve in one breath, so the reader never holds it: the
//! filing rides the same admission its verdict rides, exactly as an aggregate
//! review's findings ride theirs, and the authenticated route stays the human
//! path. What lands is the ordinary commission shape — same head row, same
//! statement table, same replica enqueue — because a filed finding that were
//! its own kind of record would be one more thing nobody reads, which is the
//! artifact this whole arc exists to replace.
//!
//! # Why nothing filed here is work
//!
//! A commission becomes sealable work by two acts a person performs: freezing a
//! scope revision and signing an Approve statement over it. A filing carries
//! neither, and the doors say so on their own —
//! [`Statement::verify_authority`](aether_bloomery::Statement::verify_authority)
//! is `false` for a stage receipt, the commission store's approval classifier
//! refuses that provenance outright, and the seal door refuses a member whose
//! commission names no current revision. This module adds no fourth check; it
//! writes a record the existing three already know how to refuse.
//!
//! # What a bad read costs
//!
//! Nothing that was already landed. A malformed emission files nothing and logs
//! loudly, and the study is still recorded as evidence — the read happened, and
//! saying so is worth more than pretending it did not. A crash between the
//! order's consume and this write loses the filings for that read, which is the
//! same shape as every other post-consume write here and the same answer:
//! findings are a product, never a gate.

use aether_bloomery::{RetrospectFinding, StageId, digest_of, filed_intent, reader_derivation};
use aether_bloomery_github::short_hex;

use super::admit::{IntakeError, UploadedEvidence};
use super::dispatch::DispatchRecord;
use crate::store::StoreBackend;

/// File the findings a consumed `retrospect.read` result carried.
///
/// The receipt every filing derives from is the order's own displayed digest —
/// the landing receipt the reducer pinned as `inputs[0]` — never a digest the
/// lane named, so a read cannot file findings derived from a bloom it did not
/// open. The landed range comes off the same order for the same reason.
///
/// # Errors
/// [`IntakeError::Store`] when a filing write faults. A *refused* emission is
/// not an error: it is logged and files nothing, because the read still
/// happened and its evidence is still admitted.
pub(super) fn file_retrospect_findings(
    store: &mut dyn StoreBackend,
    record: &DispatchRecord,
    upload: &UploadedEvidence,
) -> Result<(), IntakeError> {
    debug_assert_eq!(record.stage, StageId::Study, "only the reader files findings");
    if upload.observation.retrospect_findings.is_empty() {
        return Ok(());
    }

    let receipt = record.displayed_digest;
    let emission = match RetrospectFinding::normalize(receipt, upload.observation.retrospect_findings.iter().cloned()) {
        Ok(emission) => emission,
        Err(refusal) => {
            tracing::warn!(
                target: "aether_chassis_bloomery::intake",
                nonce = %record.nonce.0,
                bloom = %short_hex(&record.bloom.0),
                refusal = ?refusal,
                "reader emission is malformed; filing nothing and keeping the study as evidence",
            );
            return Ok(());
        }
    };
    if emission.dropped > 0 {
        tracing::warn!(
            target: "aether_chassis_bloomery::intake",
            nonce = %record.nonce.0,
            bloom = %short_hex(&record.bloom.0),
            ceiling = RetrospectFinding::MAX_FINDINGS,
            dropped = emission.dropped,
            "reader emitted past the per-read ceiling; the surplus findings were refused",
        );
    }

    let derivation = reader_derivation(
        digest_of(&record.profile),
        receipt,
        record.transformation.diff_base,
        record.transformation.checkout,
        &emission.findings,
    );
    for finding in &emission.findings {
        let workpiece = finding.workpiece();
        let filed = store.file_derived_commission(&workpiece, &filed_intent(finding, &derivation))?;
        // Said at info, not debug: a machine-authored work order entering the
        // estate is a fact an operator reading the coordinator's log should see
        // without turning anything up.
        tracing::info!(
            target: "aether_chassis_bloomery::intake",
            bloom = %short_hex(&record.bloom.0),
            workpiece = %workpiece.0,
            derivation = %short_hex(&digest_of(&derivation)),
            filed,
            "reader finding filed as an open unapproved commission",
        );
    }

    Ok(())
}
