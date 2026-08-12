//! The claim routes (ADR-0179): inspect the live claim refs, submit an
//! author-signed orphan release, and read one release's journal-derived state.
//!
//! The recovery path for a claim ref that outlived every journal knowing its
//! holder. Before this, an operator whose seal answered `ActiveBloomExists` had
//! to read the reducer, the heal planner, and `git ls-remote` to find the ref,
//! then `git push --delete` it out of band; `GET /claims` ends the hunt and
//! `POST /claims/releases` makes the deletion a first-class, signed, journaled
//! act instead of tribal knowledge.
//!
//! Enumeration is diagnostic, never a liveness oracle: a holder this instance
//! does not know may be another instance's live bloom. That is exactly why the
//! release requires a signature — the operator investigates the holder and
//! signs, accepting the uncertainty the machine cannot resolve on its own.

use aether_actor::Manual;
use aether_bloomery::{
    Admit, ClaimRefState, Digest, EnumerateClaims, EnumerateClaimsResult, Event, Fact, IdempotencyKey,
    OrphanClaimRelease, OrphanClaimReleaseRecord, Query, QueryResult,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::hex::{digest_from_hex, hex_encode};
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, ReleasePending, Routed};
use crate::api::dto::{ClaimRefView, ClaimsView, ReleaseAcceptedView, ReleaseRequest};
use crate::control::ControlCore;
use crate::signing::{SigningCapability, Verify};
use crate::source::SourceCapability;

impl ApiCapabilityState {
    /// `GET /claims` — enumerate every live claim ref and its holder.
    pub(super) fn list_claims(&self, ctx: &NativeCtx<'_, Manual>) -> Routed {
        Routed::Deferred(self.send_tracked(ctx.actor::<SourceCapability>(), &EnumerateClaims))
    }

    /// `POST /claims/releases` — authorize releasing one orphaned claim ref.
    ///
    /// The route is the cryptographic trust gate, exactly as the answer route
    /// is: it dials `aether.signing` to verify the author signature against the
    /// host-custodied allowlist before admitting, and the reducer independently
    /// re-checks that the statement's words and parents bind *this* request. Both
    /// halves are load-bearing — a verified signature over the right words but
    /// the wrong parent would otherwise authorize releasing any ref at all.
    ///
    /// A body that does not decode is a `400`. Everything past that is decided
    /// downstream: a signature that does not verify answers `400` from the verify
    /// reply, and a locally-known holder is the reducer's refusal.
    pub(super) fn request_claim_release(&self, ctx: &NativeCtx<'_, Manual>, body: &[u8]) -> Routed {
        let request: ReleaseRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid release request: {error}"))),
        };
        let ReleaseRequest { ref_kind, expected_holder, authorization } = request;
        let target = OrphanClaimRelease { ref_kind, expected_holder };
        let statement = match to_vec(&authorization) {
            Ok(bytes) => bytes,
            Err(error) => return Routed::Reply(error_response(500, &format!("authorization encode failed: {error}"))),
        };

        // The request digest is the idempotency key as well as the handle, so a
        // resubmitted release is a duplicate rather than a second deletion — the
        // reducer's own repeat guard and this key agree on what "the same
        // request" means.
        let digest = target.request();
        let event = Event {
            idempotency_key: IdempotencyKey(format!(
                "aether.bloomery.orphan_claim_release:{}",
                hex_encode(digest.as_bytes())
            )),
            fact: Fact::RequestOrphanClaimRelease { request: target, authorization },
        };
        let correlation = self.send_tracked(ctx.actor::<SigningCapability>(), &Verify { statement });
        Routed::DeferredRelease { correlation, request: digest, event: Box::new(event) }
    }

    /// `GET /claims/releases/{digest}` — read one release request's state.
    pub(super) fn query_claim_release(&self, ctx: &NativeCtx<'_, Manual>, digest: &str) -> Routed {
        digest_from_hex(digest).map_or_else(
            || Routed::Reply(error_response(400, "release id is not a 32-byte hex digest")),
            |digest| {
                Routed::Deferred(self.send_tracked(
                    ctx.actor::<ControlCore>(),
                    &Query { bloom: None, release: Some(digest.as_bytes().to_vec()) },
                ))
            },
        )
    }

    /// Resolve a held release request from the `aether.signing` verify reply: a
    /// verified signature admits the stashed request event (re-deferring on the
    /// reducer reply); a rejection is a `400` with nothing attempted.
    pub(super) fn resolve_release_verify(
        &mut self,
        ctx: &NativeCtx<'_, Manual>,
        correlation: u64,
        verified: Result<bool, String>,
    ) {
        let Some(ReleasePending { inbound, request, event }) = self.releasing.remove(&correlation) else {
            return;
        };
        match verified {
            Ok(true) => match to_vec(&event) {
                Ok(bytes) => {
                    let correlation = self.send_tracked(ctx.actor::<ControlCore>(), &Admit { event: bytes });
                    self.pending.insert(correlation, inbound);
                    self.release_admits.insert(correlation, request);
                }
                Err(error) => {
                    inbound.reply(&error_response(500, &format!("event encode failed: {error}")));
                }
            },
            Ok(false) => {
                inbound
                    .reply(&error_response(400, "release authorization is not an author signature or did not verify"));
            }
            Err(error) => {
                inbound.reply(&error_response(400, &format!("release authorization did not verify: {error}")));
            }
        }
    }
}

/// Render an enumeration reply into `GET /claims`' response.
pub(super) fn claims_response(result: EnumerateClaimsResult) -> HttpServerResponse {
    match result {
        EnumerateClaimsResult::Ok { states } => {
            let decoded: Result<Vec<ClaimRefState>, _> = states.iter().map(|bytes| from_bytes(bytes)).collect();
            match decoded {
                Ok(states) => json(
                    200,
                    &ClaimsView {
                        claims: states
                            .into_iter()
                            .map(|state| ClaimRefView { ref_kind: state.ref_kind, holder: state.holder })
                            .collect(),
                    },
                ),
                Err(error) => error_response(500, &format!("claim state decode failed: {error}")),
            }
        }
        EnumerateClaimsResult::Err { error } => error_response(500, &error),
    }
}

/// Render a release submission's admit reply: `202` with the request digest once
/// the fact is durably admitted, so the operator has the handle to poll before
/// the reactor has run.
pub(super) fn release_accepted_response(request: Digest, outcome: aether_bloomery::Outcome) -> HttpServerResponse {
    json(202, &ReleaseAcceptedView { request: hex_encode(request.as_bytes()), outcome })
}

/// Render a release-status read. A `Release` reply carries the record; a
/// `NotFound` means no such request was ever admitted here.
pub(super) fn release_status_response(result: QueryResult) -> HttpServerResponse {
    match result {
        QueryResult::Release { record } => match from_bytes::<OrphanClaimReleaseRecord>(&record) {
            Ok(record) => json(200, &record),
            Err(error) => error_response(500, &format!("release record decode failed: {error}")),
        },
        QueryResult::NotFound => error_response(404, "no orphan claim release with that request digest"),
        QueryResult::Err { error } => error_response(500, &error),
        // A release read asks for one record; the document / bloom variants
        // answer a different question and cannot arrive on this correlation.
        QueryResult::Document { .. } | QueryResult::Bloom { .. } => {
            error_response(500, "release read answered with a projection")
        }
    }
}
