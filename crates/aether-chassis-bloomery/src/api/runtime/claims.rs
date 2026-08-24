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
//!
//! Two of the three routes are ordinary ADR-0154 relays — `GET /claims` defers
//! on the source cap's enumeration and `GET /claims/releases/{digest}` on the
//! control core's `Query`, each answered by the paired `#[http::reply]` next
//! door. The submission is the one that is not: it verifies a signature *before*
//! it admits, so it is a genuine multi-hop and holds its request in the shared
//! [`verifying`](super::state::ApiCapabilityState::verifying) table the answer
//! route already keeps.

use aether_actor::Manual;
use aether_bloomery::{
    AuthorityDoor, ClaimRefState, EnumerateClaims, EnumerateClaimsResult, Event, Fact, IdempotencyKey,
    OrphanClaimRelease, OrphanClaimReleaseRecord, QueryResult, QuerySelector,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::hex::{self, digest_from_hex, hex_encode};
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};
use crate::api::dto::{ClaimRefView, ClaimsView, ReleaseRequest};
use crate::signing::{SigningCapability, Verify, authority_bytes};

impl ApiCapabilityState {
    /// `GET /claims` — enumerate every live claim ref and its holder.
    pub(super) fn list_claims() -> Routed {
        Routed::EnumerateClaims(EnumerateClaims)
    }

    /// `POST /claims/releases` — authorize releasing one orphaned claim ref.
    ///
    /// The route is the cryptographic trust gate, exactly as the answer route
    /// is: it dials `aether.signing` to verify the author signature against the
    /// host-custodied allowlist before admitting, bound to the request digest it
    /// recomputes from the typed target (ADR-0182), and the reducer independently
    /// re-checks that the statement's words and parents bind *this* request. Both
    /// halves are load-bearing, and in different ways. The signed binding is what
    /// stops a captured envelope authorizing a second release — `parents` is
    /// outside the signature, so the reducer's check alone was rewritable by
    /// whoever held the envelope. The reducer's check is what still holds on
    /// replay, where no key material exists to re-run the first.
    ///
    /// A body that does not decode is a `400`. Everything past that is decided
    /// downstream: a signature that does not verify answers `400` from the verify
    /// reply, and a locally-known holder is the reducer's refusal.
    pub(super) fn request_claim_release(&self, ctx: &NativeCtx<'_, Manual>, body: &[u8]) -> Routed {
        let request: ReleaseRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid release request: {error}"))),
        };
        let ReleaseRequest { ref_kind, expected_holder, authorization } = request;
        let target = OrphanClaimRelease { ref_kind, expected_holder };
        let statement = match to_vec(&authorization) {
            Ok(bytes) => bytes,
            Err(error) => return Routed::Reply(error_response(500, &format!("authorization encode failed: {error}"))),
        };

        // Recomputed from the typed target in the request body, never read out of
        // the envelope (ADR-0182), and used for both bindings below.
        let binding = target.request();

        // The request digest is the idempotency key as well as the handle, so a
        // resubmitted release is a duplicate rather than a second deletion — the
        // reducer's own repeat guard and this key agree on what "the same
        // request" means. It rides the admitted outcome back out, so nothing has
        // to be held here to report it in the `202`.
        let event = Event {
            idempotency_key: IdempotencyKey(format!(
                "aether.bloomery.orphan_claim_release:{}",
                hex_encode(binding.as_bytes())
            )),
            fact: Fact::RequestOrphanClaimRelease { request: target, authorization },
        };
        // The words this door signs are a fixed constant, so before the binding
        // was signed one author signature verified forever and re-pointed at any
        // ref by a rewrite of `parents` — a universal release token for this
        // door. Signing the binding is what makes a captured envelope good for
        // the one release it was minted for.
        let correlation = self.send_tracked(
            ctx.actor::<SigningCapability>(),
            &Verify {
                statement,
                authority: authority_bytes(AuthorityDoor::OrphanClaimRelease, binding),
                required_tier: None,
            },
        );
        Routed::DeferredVerify { correlation, subject: "release authorization", event: Box::new(event) }
    }

    /// `GET /claims/releases/{digest}` — read one release request's state.
    pub(super) fn query_claim_release(digest: &str) -> Routed {
        digest_from_hex(digest).map_or_else(
            || Routed::Reply(error_response(400, "release id is not a 32-byte hex digest")),
            |digest| Self::query(QuerySelector::Release { digest: digest.as_bytes().to_vec() }),
        )
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

/// Render a release-status read. A `Release` reply carries the record; a
/// `ReleaseNotFound` means no such request was ever admitted here.
pub(super) fn release_status_response(result: QueryResult) -> HttpServerResponse {
    match result {
        QueryResult::Release { record } => match from_bytes::<OrphanClaimReleaseRecord>(&record) {
            Ok(record) => json(200, &record),
            Err(error) => error_response(500, &format!("release record decode failed: {error}")),
        },
        QueryResult::ReleaseNotFound => error_response(404, "no orphan claim release with that request digest"),
        QueryResult::Err { error } => error_response(500, &error),
        // A release read asks for one record; the projection and calibration
        // variants — including the bloom-shaped `NotFound` — answer a different
        // question and cannot arrive on this reply.
        QueryResult::Document { .. }
        | QueryResult::Bloom { .. }
        | QueryResult::NotFound
        | QueryResult::Calibration { .. }
        | QueryResult::Why { .. } => error_response(500, "release read answered with a projection"),
    }
}
