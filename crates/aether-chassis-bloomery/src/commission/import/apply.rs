//! Write imported commissions and sealed reconstructions into the store.

use aether_bloomery::{Observation, Provenance, Statement, WorkpieceId, digest_of};

use super::manifest::parse_issue;
use super::{ImportError, ImportReport, ImportRequest, SealedWorkpiece};
use crate::store::{CommissionBackend, CommissionError};

/// Import the explicit set. Refuses an empty set rather than scanning anything.
pub fn import(store: &mut impl CommissionBackend, request: &ImportRequest) -> Result<ImportReport, ImportError> {
    if request.issues.is_empty() && request.sealed.is_empty() {
        return Err(ImportError::EmptySet);
    }

    let mut report = ImportReport { entries: Vec::new(), imported: Vec::new(), reconstructed: Vec::new() };
    for snapshot in &request.issues {
        let parsed = parse_issue(snapshot);
        create_if_absent(store, &snapshot.workpiece, &parsed.intent)?;
        if let Some(revision) = parsed.revision {
            store.write_revision(&revision)?;
        }
        report.imported.push(snapshot.workpiece.clone());
        report.entries.push(parsed.entry);
    }
    for sealed in &request.sealed {
        reconstruct(store, sealed)?;
        report.reconstructed.push(sealed.revision.workpiece.clone());
    }
    Ok(report)
}

fn create_if_absent(
    store: &mut impl CommissionBackend,
    id: &WorkpieceId,
    intent: &Statement,
) -> Result<(), ImportError> {
    match store.create(id, intent) {
        Ok(_) | Err(CommissionError::DuplicateCommission(_)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reconstruct(store: &mut impl CommissionBackend, sealed: &SealedWorkpiece) -> Result<(), ImportError> {
    let id = &sealed.revision.workpiece;
    let member = sealed
        .spec
        .members()
        .iter()
        .find(|member| member.workpiece == *id)
        .ok_or_else(|| ImportError::UnknownSealedMember(id.0.clone()))?;
    if digest_of(&sealed.revision) != member.scope_revision {
        return Err(ImportError::PinnedDigestMismatch { workpiece: id.0.clone() });
    }
    if digest_of(&sealed.approval) != member.approval.detail {
        return Err(ImportError::PinnedEvidenceMismatch { workpiece: id.0.clone() });
    }
    if let Some(view) = store.load(id)?
        && let Some(current) = view.head.current_revision
        && current != member.scope_revision
    {
        return Err(ImportError::WouldDiverge { workpiece: id.0.clone() });
    }

    let intent = Statement {
        words: id.0.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation {
            source: format!("migration:sealed-bloom:{}", super::hex(sealed.spec.id().0.as_bytes())),
        }),
        parents: Vec::new(),
    };
    create_if_absent(store, id, &intent)?;
    store.write_revision(&sealed.revision)?;
    store.record_verified_approval(id, &sealed.approval)?;
    Ok(())
}
