//! The scope-revision authoring route — `POST /scope-revisions`.
//!
//! A [`Workpiece`](aether_bloomery::Workpiece) names its `scope_revision` as an
//! opaque [`Digest`](aether_bloomery::Digest), so without this route an operator
//! could seal a per-workpiece [`ModelOverride`](aether_bloomery::ModelOverride)
//! only by computing its content address out-of-band — which is to say, not at
//! all. Post the revision, get the digest back, stage a workpiece naming it.
//!
//! This is the authoring half of choosing a harness / model without editing the
//! line: the stage catalog names the fleet-wide defaults and changing it
//! re-digests the catalog, whereas a revision sealed through here scopes the
//! choice to one bloom and stays attestable, because the bloom pins the
//! revision's digest and the digest covers the override.
//!
//! Authoring also **stores** the revision under its digest (#4588), which is
//! what makes the seal more than an attestation: the executor reactor resolves a
//! member's sealed `scope_revision` against that row at dispatch, so the lane
//! runs under the authored harness / model instead of the stage's calibrated
//! default. The write happens here rather than at seal because a seal request
//! carries only the digest — the content exists nowhere else. A digest with no
//! stored row still dispatches the stage default, so a revision authored out-of-
//! band degrades rather than failing.
//!
//! What stays refused is an ambient env or config-file override (#4327 removed
//! exactly that): a knob overriding the sealed profile would let a receipt attest
//! a model that never ran — the same divergence an inert override was a case of.
//!
//! Content addressing makes the write idempotent — identical content addresses
//! to the same digest and rewrites the same row — so the route is safely
//! repeatable. It answers on the store's reply rather than inline, so a `200`
//! means the digest it hands back will actually resolve.

use aether_actor::Manual;
use aether_bloomery::ScopeRevision;
use aether_data::wire::to_vec;
use aether_substrate::actor::native::NativeCtx;
use serde::Serialize;

use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};
use crate::store::{RecordScopeRevision, StoreCapability};

/// The reply: the authored revision and the address a workpiece names it by.
#[derive(Serialize)]
pub(super) struct ScopeRevisionView {
    /// The content address to set as a workpiece's `scope_revision`.
    digest: aether_bloomery::Digest,
    /// The revision itself, echoed so a caller can confirm what it addressed.
    scope_revision: ScopeRevision,
}

impl ApiCapabilityState {
    /// `POST /scope-revisions` — content-address a scope revision, store it under
    /// that address, and reply both once the write lands.
    pub(super) fn author_scope_revision(&mut self, ctx: &NativeCtx<'_, Manual>, body: &[u8]) -> Routed {
        let scope_revision: ScopeRevision = match serde_json::from_slice(body) {
            Ok(scope_revision) => scope_revision,
            Err(error) => {
                return Routed::Reply(error_response(400, &format!("invalid scope revision body: {error}")));
            }
        };
        let revision = match to_vec(&scope_revision) {
            Ok(revision) => revision,
            Err(error) => {
                return Routed::Reply(error_response(500, &format!("scope revision encode failed: {error}")));
            }
        };

        let digest = scope_revision.digest();
        let record = RecordScopeRevision { digest: digest.as_bytes().to_vec(), revision };
        let correlation = self.send_tracked(ctx.actor::<StoreCapability>(), &record);
        // The view is what the caller gets back, so it is held across the write
        // rather than reconstructed from the store's reply — the result carries
        // only success or failure, and the digest is this route's to report.
        self.scope_revisions.insert(correlation, ScopeRevisionView { digest, scope_revision });
        Routed::Deferred(correlation)
    }

    /// Answer a held authoring request from the store's write reply: `200` with
    /// the view on a durable write, `500` on a failed one — never a digest the
    /// caller could seal against nothing.
    pub(super) fn resolve_scope_revision_write(
        &mut self,
        ctx: &NativeCtx<'_, Manual>,
        error: Option<&str>,
    ) -> Option<()> {
        let view = self.scope_revisions.remove(&ctx.reply_target().correlation_id)?;
        let response = error.map_or_else(
            || json(200, &view),
            |error| error_response(500, &format!("scope revision write failed: {error}")),
        );
        self.answer(ctx, &response);
        Some(())
    }
}
