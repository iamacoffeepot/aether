//! The live-bloom routes — read the projection, supersede a bloom, adopt a
//! signed answer — and the renderers for the two control-core replies those
//! routes (and the seal routes next door) defer on. Every route here reaches
//! durable state through the control core, so each one defers.

use aether_actor::Manual;
use aether_bloomery::{
    Admit, AdmitResult, BloomId, BloomView, Event, Fact, IdempotencyKey, Outcome, Query, QueryResult, Statement,
    ViewDocument, digest_of,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::hex::{digest_from_hex, hex_encode};
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed, VerifyPending, admit};
use crate::api::dto::{GrantRequest, OutcomeView, SupersedeRequest};
use crate::control::ControlCore;
use crate::signing::{SigningCapability, Verify, VerifyResult};

impl ApiCapabilityState {
    /// `POST /blooms/{id}/supersede` — seal the named successor draft and admit
    /// `Fact::Supersede` against the `{id}` predecessor bloom.
    pub(super) fn supersede(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let predecessor = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "predecessor id is not a 32-byte hex bloom id")),
        };
        let request: SupersedeRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid supersede body: {error}"))),
        };
        let (_, draft) = match self.lookup_draft(&request.successor_draft) {
            Ok(found) => found,
            Err(response) => return Routed::Reply(response),
        };
        // Run the same approve gate the seal route runs, then admit through the
        // supersede door (#4638). Sealing the draft directly here could never
        // work: a proposal's `approval` is a placeholder the gate is expected to
        // overwrite, so an ungated seal admits a member the reducer refuses as
        // unapproved — which it did, for every draft an operator could build.
        self.gate_and_admit(
            ctx,
            draft,
            Some(predecessor),
            &request.projections,
            request.descriptions,
            request.idempotency_key,
        )
    }

    /// `POST /blooms/{id}/grant` — hand a wedged member more attempts on the
    /// `{id}` bloom and resume it (#4708).
    ///
    /// The counterpart to supersession, along the line the sealed `base` draws:
    /// a base that has not moved, with scope, membership, and configuration
    /// unchanged, is an execution decision and belongs here; anything else is a
    /// successor doing real work. Admitting it needs no approve gate — a grant
    /// seals nothing, claims nothing, and alters no field the members' approvals
    /// bind — so unlike the supersede route it admits straight through.
    pub(super) fn grant(id: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let request: GrantRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid grant body: {error}"))),
        };
        let GrantRequest { workpiece, stage, attempts, idempotency_key } = request;
        let key = idempotency_key.unwrap_or_else(|| {
            format!("aether.bloomery.grant:{}:{}:{stage:?}:{attempts}", hex_encode(bloom.0.as_bytes()), workpiece.0)
        });

        admit(&Event {
            idempotency_key: IdempotencyKey(key),
            fact: Fact::GrantAttempts { bloom, workpiece, stage, attempts },
        })
    }

    /// `POST /blooms/{id}/answer` — adopt an answer to a parked question,
    /// releasing its hold and re-dispatching the held stage (ADR-0151).
    ///
    /// The body is the native author-signed answer statement. The route is the
    /// cryptographic trust gate: it dials the `aether.signing` capability to
    /// verify the signature against the host-custodied authorized-signer
    /// allowlist (ADR-0149 step 3, ADR-0150/ADR-0151) before admitting — the
    /// reducer holds no key material and only re-checks the structural adoption.
    /// A body that is not a decodable statement is a `400`; one whose signature
    /// does not verify is a `400` (answered from the verify reply); a valid
    /// answer admits `Fact::AdoptAnswer` and defers on the reducer outcome the
    /// same way seal / supersede do. Custody lives behind the port, so the fake
    /// always-valid provider no longer appears at the live gate.
    pub(super) fn answer_bloom(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let answer: Statement = match serde_json::from_slice(body) {
            Ok(answer) => answer,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid answer statement: {error}"))),
        };
        let statement = match to_vec(&answer) {
            Ok(bytes) => bytes,
            Err(error) => return Routed::Reply(error_response(500, &format!("answer encode failed: {error}"))),
        };
        // Build the adoption event up front and hold it across the verify round
        // trip; it admits only if the signature verifies (`resolve_verify`).
        let key = format!("aether.bloomery.answer:{}", hex_encode(digest_of(&answer).as_bytes()));
        let event = Event { idempotency_key: IdempotencyKey(key), fact: Fact::AdoptAnswer { bloom, answer } };
        let correlation = self.send_tracked(ctx.actor::<SigningCapability>(), &Verify { statement });
        Routed::DeferredVerify { correlation, event: Box::new(event) }
    }

    /// Resolve a held answer request from the `aether.signing` verify reply: a
    /// verified signature admits the stashed adoption event (re-deferring on the
    /// reducer reply); a `verified: false` verdict or an undecodable-statement
    /// error is a `400`.
    pub(super) fn resolve_verify(&mut self, ctx: &NativeCtx<'_, Manual>, result: VerifyResult) {
        let correlation = ctx.reply_target().correlation_id;
        let Some(VerifyPending { inbound, event }) = self.verifying.remove(&correlation) else {
            return;
        };
        match result {
            VerifyResult::Ok { verified: true } => match to_vec(&event) {
                Ok(bytes) => {
                    let correlation = self.send_tracked(ctx.actor::<ControlCore>(), &Admit { event: bytes });
                    self.pending.insert(correlation, inbound);
                }
                Err(error) => {
                    inbound.reply(&error_response(500, &format!("event encode failed: {error}")));
                }
            },
            VerifyResult::Ok { verified: false } => {
                inbound.reply(&error_response(400, "answer statement is not an author signature or did not verify"));
            }
            VerifyResult::Err { error } => {
                inbound.reply(&error_response(400, &format!("answer statement did not verify: {error}")));
            }
        }
    }

    /// `GET /blooms` and `GET /view` — read the whole live projection.
    pub(super) fn query(bloom: Option<Vec<u8>>) -> Routed {
        Routed::Query(Query { bloom })
    }

    /// `GET /blooms/{id}` — read one bloom's live view by hex id.
    pub(super) fn query_bloom(id: &str) -> Routed {
        digest_from_hex(id).map_or_else(
            || Routed::Reply(error_response(400, "bloom id is not a 32-byte hex digest")),
            |digest| Self::query(Some(digest.as_bytes().to_vec())),
        )
    }
}

/// Render a write route's [`AdmitResult`] into its HTTP response: the reducer
/// outcome (decoded from the wire bytes the admit reply carries), or the error.
pub(super) fn admit_response(result: AdmitResult) -> HttpServerResponse {
    match result {
        AdmitResult::Ok { outcome } => match from_bytes::<Outcome>(&outcome) {
            Ok(outcome) => json(200, &OutcomeView { outcome }),
            Err(error) => error_response(500, &format!("outcome decode failed: {error}")),
        },
        AdmitResult::Err { error } => error_response(500, &error),
    }
}

/// Render a live-read route's [`QueryResult`] into its HTTP response: the whole
/// view document, one bloom view, a `404`, or the error.
pub(super) fn query_response(result: QueryResult) -> HttpServerResponse {
    match result {
        QueryResult::Document { document } => match from_bytes::<ViewDocument>(&document) {
            Ok(document) => json(200, &document),
            Err(error) => error_response(500, &format!("view document decode failed: {error}")),
        },
        QueryResult::Bloom { view } => match from_bytes::<BloomView>(&view) {
            Ok(view) => json(200, &view),
            Err(error) => error_response(500, &format!("bloom view decode failed: {error}")),
        },
        QueryResult::NotFound => error_response(404, "no bloom with that id"),
        QueryResult::Err { error } => error_response(500, &error),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::SupersedeRequest;

    #[test]
    fn a_supersede_body_without_descriptions_still_parses() {
        // Tripwire on the operator contract (#4631): descriptions were added to
        // this body after the route shipped, so every existing caller omits
        // them. Making the field required would turn each of those into a `400`
        // on the one route an operator reaches for when a bloom has already
        // failed to land.
        let body = br#"{"successor_draft":"1"}"#;

        let parsed: SupersedeRequest = serde_json::from_slice(body).expect("a body predating descriptions parses");

        assert!(parsed.descriptions.is_empty(), "an absent map defaults empty rather than erroring");
    }

    #[test]
    fn a_supersede_body_carries_descriptions_per_workpiece() {
        let body = br#"{"successor_draft":"1","descriptions":{"wp-a":"build the thing"}}"#;

        let parsed: SupersedeRequest = serde_json::from_slice(body).unwrap();

        assert_eq!(parsed.descriptions.get("wp-a").map(String::as_str), Some("build the thing"));
    }
}
