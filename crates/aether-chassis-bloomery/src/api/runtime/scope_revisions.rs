//! The scope-revision authoring route — `POST /scope-revisions`.
//!
//! A [`Workpiece`](aether_bloomery::Workpiece) names its `scope_revision` as an
//! opaque [`Digest`](aether_bloomery::Digest), so without this route an operator
//! could seal a per-workpiece [`ModelOverride`] only by computing its content
//! address out-of-band — which is to say, not at all. This is where the override
//! becomes settable: post the revision, get the digest back, stage a workpiece
//! naming it.
//!
//! It is how the harness / model calibration is **chosen** without editing the
//! line. The stage catalog names the fleet-wide defaults and changing it
//! re-digests the catalog; an override sealed here changes one bloom and stays
//! attestable, because the bloom pins the revision's digest and the digest
//! covers the override. There is deliberately no ambient env or config-file
//! equivalent (#4327 removed exactly that): a knob that overrode the sealed
//! profile would let a receipt attest a model that never ran.
//!
//! Pure content addressing — nothing is stored, claimed, or made durable — so
//! the route answers synchronously and is safely repeatable.

use aether_bloomery::ScopeRevision;
use serde::Serialize;

use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};

/// The reply: the authored revision and the address a workpiece names it by.
#[derive(Serialize)]
pub(super) struct ScopeRevisionView {
    /// The content address to set as a workpiece's `scope_revision`.
    digest: aether_bloomery::Digest,
    /// The revision itself, echoed so a caller can confirm what it addressed.
    scope_revision: ScopeRevision,
}

impl ApiCapabilityState {
    /// `POST /scope-revisions` — content-address a scope revision so a workpiece
    /// can name it.
    pub(super) fn author_scope_revision(body: &[u8]) -> Routed {
        let scope_revision: ScopeRevision = match serde_json::from_slice(body) {
            Ok(scope_revision) => scope_revision,
            Err(error) => {
                return Routed::Reply(error_response(400, &format!("invalid scope revision body: {error}")));
            }
        };
        Routed::Reply(json(200, &ScopeRevisionView { digest: scope_revision.digest(), scope_revision }))
    }
}
