//! Store-backed commission reader (ADR-0199 slice 2, #5048).
//!
//! The admission door materializes a [`Workpiece`] and the gate's
//! [`MemberProjection`] from verified store rows. A caller-supplied
//! projection of the same digest is not an input — the canonical bytes the
//! operator signed are.

use aether_bloomery::{
    CommissionStatus, Digest, MemberDependency, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision, Statement, Workpiece,
    WorkpieceId, digest_of,
};
use aether_data::wire::from_bytes;
use aether_http::HttpServerResponse;

use super::response::error_response;
use crate::api::dto::MemberProjection;
use crate::bloomery::Completeness;
use crate::commission::scope::task_text;
use crate::store::{ListCommissionsResult, ListedCommission, LoadCommissionResult};

mod adr_touch;
use adr_touch::adr_touch;
pub(super) use adr_touch::{AdrMaturity, TreeAdrs};

#[cfg(test)]
mod tests;

/// Why a store row could not become an admitted member or a listed workpiece.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum AdmissionRefusal {
    /// No commission exists under this workpiece id.
    MissingCommission { id: String },
    /// Canonical bytes did not decode, or a stored digest was not 32 bytes.
    MalformedCanonical { id: String },
    /// The row's claimed digest is not the hash of its canonical bytes.
    DigestMismatch { id: String },
    /// The draft names a revision that is not the commission's current tip.
    StaleScope { id: String },
    /// The current revision has no stored approval.
    AbsentApproval { id: String },
}

impl AdmissionRefusal {
    pub(super) fn response(&self) -> HttpServerResponse {
        error_response(422, &self.message())
    }

    pub(super) fn message(&self) -> String {
        match self {
            Self::MissingCommission { id } => {
                format!("member {id} has no commission in the store; seal fails closed")
            }
            Self::MalformedCanonical { id } => {
                format!("member {id} stored commission bytes are malformed; seal fails closed")
            }
            Self::DigestMismatch { id } => {
                format!("member {id} stored revision digest does not match its canonical bytes; seal fails closed")
            }
            Self::StaleScope { id } => {
                format!("member {id} names a stale scope revision; seal fails closed")
            }
            Self::AbsentApproval { id } => {
                format!("member {id} has no stored approval; seal fails closed")
            }
        }
    }
}

/// One draft member materialized from verified store rows.
#[derive(Clone, Debug)]
pub(super) struct AdmittedMember {
    /// The workpiece identity the draft named.
    pub workpiece: Workpiece,
    /// The gate projection reconstructed from the frozen revision.
    pub projection: MemberProjection,
    /// Advisory description frozen on the revision.
    pub description: String,
    /// Dependency edges declared on the revision, `member` = this workpiece.
    pub edges: Vec<MemberDependency>,
}

/// Materialize open commissions that have a current revision into workpieces.
///
/// A head without a current revision is not yet a workpiece and is omitted.
/// Malformed digest bytes fail the whole list closed.
pub(super) fn workpieces_from_list(result: ListCommissionsResult) -> Result<Vec<Workpiece>, HttpServerResponse> {
    match result {
        ListCommissionsResult::Ok { commissions } => {
            let mut workpieces = Vec::new();
            for listed in commissions {
                if let Some(workpiece) = workpiece_from_listed(&listed).map_err(|refusal| refusal.response())? {
                    workpieces.push(workpiece);
                }
            }
            Ok(workpieces)
        }
        ListCommissionsResult::Err { error } => Err(error_response(500, &format!("commission list failed: {error}"))),
    }
}

/// One listed head as a [`Workpiece`], or `None` when it has no current revision.
pub(super) fn workpiece_from_listed(listed: &ListedCommission) -> Result<Option<Workpiece>, AdmissionRefusal> {
    if listed.status != CommissionStatus::Open.as_str() {
        return Ok(None);
    }
    let Some(revision) = listed.current_revision.as_deref() else {
        return Ok(None);
    };
    Ok(Some(Workpiece {
        id: WorkpieceId(listed.id.clone()),
        intent: digest32(&listed.intent, &listed.id)?,
        scope_revision: digest32(revision, &listed.id)?,
    }))
}

/// Why a store load could not admit a member: a named refusal, or a store fault.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum AdmitError {
    /// A fail-closed admission refusal.
    Refused(AdmissionRefusal),
    /// The store could not read the row. Not an admission decision.
    Store(String),
}

impl AdmitError {
    pub(super) fn response(&self) -> HttpServerResponse {
        match self {
            Self::Refused(refusal) => refusal.response(),
            Self::Store(error) => error_response(500, &format!("commission load failed: {error}")),
        }
    }
}

/// Materialize one draft member from a store load, failing closed on each
/// named refusal. `expected` is the exact scope digest the draft membership
/// pinned.
pub(super) fn admit_member(
    expected: Digest,
    result: LoadCommissionResult,
    maturity: &impl AdrMaturity,
) -> Result<AdmittedMember, AdmitError> {
    match result {
        LoadCommissionResult::Missing { id } => Err(AdmitError::Refused(AdmissionRefusal::MissingCommission { id })),
        LoadCommissionResult::Err { error } => Err(AdmitError::Store(error)),
        LoadCommissionResult::Ok { id, intent, current_revision, status, current, approvals, .. } => {
            admit_loaded(expected, id, &intent, current_revision, &status, current, approvals, maturity)
                .map_err(AdmitError::Refused)
        }
    }
}

fn admit_loaded(
    expected: Digest,
    id: String,
    intent: &[u8],
    current_revision: Option<Vec<u8>>,
    status: &str,
    current: Option<Vec<u8>>,
    approvals: Vec<Vec<u8>>,
    maturity: &impl AdrMaturity,
) -> Result<AdmittedMember, AdmissionRefusal> {
    if status != CommissionStatus::Open.as_str() {
        return Err(AdmissionRefusal::StaleScope { id });
    }
    let Some(current_bytes) = current_revision else {
        return Err(AdmissionRefusal::StaleScope { id });
    };
    let current_digest = digest32(&current_bytes, &id)?;
    let Some(canonical) = current else {
        return Err(AdmissionRefusal::MalformedCanonical { id });
    };
    let revision = ScopeRevision::from_canonical(&canonical)
        .map_err(|_| AdmissionRefusal::MalformedCanonical { id: id.clone() })?;
    if revision.schema != SCOPE_REVISION_SCHEMA {
        return Err(AdmissionRefusal::MalformedCanonical { id });
    }
    if digest_of(&revision) != current_digest {
        return Err(AdmissionRefusal::DigestMismatch { id });
    }
    if current_digest != expected {
        return Err(AdmissionRefusal::StaleScope { id });
    }
    if revision.workpiece.0 != id {
        return Err(AdmissionRefusal::DigestMismatch { id });
    }

    let mut decoded = Vec::new();
    for bytes in approvals {
        let statement: Statement =
            from_bytes(&bytes).map_err(|_| AdmissionRefusal::MalformedCanonical { id: id.clone() })?;
        decoded.push(statement);
    }
    if decoded.is_empty() {
        return Err(AdmissionRefusal::AbsentApproval { id });
    }

    let signed_statement =
        decoded.into_iter().find(|statement| matches!(statement.provenance, Provenance::AuthorSignature(_)));
    let workpiece = Workpiece { id: WorkpieceId(id.clone()), intent: digest32(intent, &id)?, scope_revision: expected };
    let edges = revision
        .dependencies
        .iter()
        .map(|depends_on| MemberDependency { member: workpiece.id.clone(), depends_on: depends_on.clone() })
        .collect();
    let projection = MemberProjection {
        workpiece: workpiece.id.clone(),
        scope_revision: expected,
        declared_surface: revision.declared_surface.clone(),
        completeness: completeness_from(&revision, status, current_digest == expected),
        adr_touch: adr_touch(&revision.declared_surface, maturity),
        pre_approved: false,
        signed_statement,
    };
    Ok(AdmittedMember { workpiece, projection, description: task_text(&revision), edges })
}

fn completeness_from(revision: &ScopeRevision, status: &str, surface_fresh: bool) -> Completeness {
    Completeness {
        has_problem_statement: !revision.problem.trim().is_empty(),
        has_design_notes: !revision.design.trim().is_empty(),
        has_implementation_plan: !revision.plan.trim().is_empty(),
        referenced_adr_prs_merged: true,
        model_routing_count: usize::from(!revision.routing.model.trim().is_empty()),
        blocked: status != CommissionStatus::Open.as_str(),
        declared_surface_fresh: surface_fresh,
        dependencies_all_closed: true,
        umbrella_integrity: true,
    }
}

fn digest32(bytes: &[u8], id: &str) -> Result<Digest, AdmissionRefusal> {
    Digest::from_slice(bytes).ok_or_else(|| AdmissionRefusal::MalformedCanonical { id: id.to_owned() })
}
