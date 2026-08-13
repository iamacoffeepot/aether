//! The live-bloom routes — read the projection, supersede a bloom, adopt a
//! signed answer — and the renderers for the two control-core replies those
//! routes (and the seal routes next door) defer on. Every route here reaches
//! durable state through the control core, so each one defers.

use aether_actor::Manual;
use aether_bloomery::{
    Admit, AdmitResult, AuthorityDoor, BloomId, BloomView, Event, Fact, IdempotencyKey, Outcome, Query, QueryResult,
    Statement, ViewDocument, digest_of,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::hex::{self, digest_from_hex, hex_encode};
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed, VerifyPending, admit};
use crate::api::dto::{GrantRequest, OutcomeView, ReleaseAcceptedView, SupersedeRequest};
use crate::control::ControlCore;
use crate::signing::{SigningCapability, Verify, VerifyResult, authority_bytes};

impl ApiCapabilityState {
    /// `POST /blooms/{id}/supersede` — seal the named successor draft and admit
    /// `Fact::Supersede` against the `{id}` predecessor bloom.
    pub(super) fn supersede(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let predecessor = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "predecessor id is not a 32-byte hex bloom id")),
        };
        let request: SupersedeRequest = match hex::from_slice(body) {
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
        let request: GrantRequest = match hex::from_slice(body) {
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

    /// `POST /blooms/{id}/answer/{question}` — adopt an answer to the parked
    /// question `{question}` names, releasing its hold and re-dispatching the
    /// held stage (ADR-0151).
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
    ///
    /// The question rides the path rather than the body, matching ADR-0179's
    /// `GET /claims/releases/{digest}` and keeping the body a bare `Statement`.
    /// It is what the signature is bound to (ADR-0182): the reducer used to
    /// discover the question by scanning `parents` *after* verification had
    /// already happened, which left the answer door binding on a field outside
    /// the signature — two questions answered with the same words shared signed
    /// bytes, so the first envelope could be re-parented onto the second hold.
    /// Naming it here gives the route a binding it derives from the request
    /// instead of from the envelope.
    ///
    /// Naming it is not sufficient on its own, which is why the `parents` check
    /// below is part of the gate rather than a nicety. `Fact::AdoptAnswer` has
    /// no question field — the wire shape is frozen (ADR-0182 §Migration) — so
    /// the reducer re-derives its target by scanning `parents` for an open hold.
    /// A route that only *verified* against the path question would let the
    /// submitter supply both halves of the equality: a genuine envelope signed
    /// at `(Answer, Q1)` verifies when posted to `.../answer/{Q1}`, and its
    /// unsigned `parents` — rewritten to `[Q2]` — is what the reducer then acts
    /// on, releasing a hold nobody signed for. So the route refuses unless
    /// `parents` is exactly the one question the path names, which is what makes
    /// the path binding and the reducer's target provably the same digest.
    /// Membership (`parents.contains(&question)`) would not do it: the reducer
    /// takes the first parent that is an open hold in submitter order, so
    /// `[Q2, Q1]` contains the path question and still releases `Q2`.
    pub(super) fn answer_bloom(&self, ctx: &NativeCtx<'_, Manual>, id: &str, question: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let Some(question) = digest_from_hex(question) else {
            return Routed::Reply(error_response(400, "question is not a 32-byte hex digest"));
        };
        let answer: Statement = match hex::from_slice(body) {
            Ok(answer) => answer,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid answer statement: {error}"))),
        };
        if answer.parents.as_slice() != [question] {
            return Routed::Reply(error_response(
                400,
                "answer parents must name exactly the question the path names, and nothing else",
            ));
        }

        let statement = match to_vec(&answer) {
            Ok(bytes) => bytes,
            Err(error) => return Routed::Reply(error_response(500, &format!("answer encode failed: {error}"))),
        };
        // Build the adoption event up front and hold it across the verify round
        // trip; it admits only if the signature verifies (`resolve_verify`).
        let key = format!("aether.bloomery.answer:{}", hex_encode(digest_of(&answer).as_bytes()));
        let event = Event { idempotency_key: IdempotencyKey(key), fact: Fact::AdoptAnswer { bloom, answer } };
        let correlation = self.send_tracked(
            ctx.actor::<SigningCapability>(),
            &Verify { statement, authority: authority_bytes(AuthorityDoor::Answer, question) },
        );
        Routed::DeferredVerify { correlation, subject: "answer statement", event: Box::new(event) }
    }

    /// Resolve a held verify-then-admit request from the `aether.signing` verify
    /// reply: a verified signature admits the stashed event (re-deferring on the
    /// reducer reply); a `verified: false` verdict or an undecodable-statement
    /// error is a `400` naming what the operator submitted.
    ///
    /// Serves both flows that hold across a verification — the adopted answer and
    /// the orphan-claim release (ADR-0179) — because past the signature they are
    /// the same act: admit the held event, then answer from the reducer's reply.
    pub(super) fn resolve_verify(&mut self, ctx: &NativeCtx<'_, Manual>, result: VerifyResult) {
        let correlation = ctx.reply_target().correlation_id;
        let Some(VerifyPending { inbound, subject, event }) = self.verifying.remove(&correlation) else {
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
                inbound.reply(&error_response(400, &format!("{subject} is not an author signature or did not verify")));
            }
            VerifyResult::Err { error } => {
                inbound.reply(&error_response(400, &format!("{subject} did not verify: {error}")));
            }
        }
    }

    /// `GET /blooms` and `GET /view` — read the whole live projection.
    pub(super) fn query(bloom: Option<Vec<u8>>) -> Routed {
        Routed::Query(Query { bloom, release: None })
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
            Ok(outcome) => admitted_response(outcome),
            Err(error) => error_response(500, &format!("outcome decode failed: {error}")),
        },
        AdmitResult::Err { error } => error_response(500, &error),
    }
}

/// Render one admitted reducer outcome into the write route's response.
///
/// Every write route answers `200` with the outcome it produced, except the one
/// whose admission only *accepts* work: an authorized orphan-claim release is
/// durably queued for the release reactor rather than performed, so it answers
/// `202` and hands back the request digest `GET /claims/releases/{digest}` reads
/// by (ADR-0179). The digest rides the outcome itself, so the route holds
/// nothing across the admit to report it — the same reason `RecordConfigResult`
/// carries its stored bytes rather than the authoring route keeping a
/// correlation map (ADR-0154 §3).
fn admitted_response(outcome: Outcome) -> HttpServerResponse {
    match &outcome {
        Outcome::OrphanClaimReleaseRequested { request } => {
            json(202, &ReleaseAcceptedView { request: hex_encode(request.as_bytes()), outcome })
        }
        _ => json(200, &OutcomeView { outcome }),
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
        // The shared `#[http::reply]` sends both release variants to
        // `release_status_response` before either reaches here, so one arriving
        // is a routing bug rather than an answer to render.
        QueryResult::Release { .. } | QueryResult::ReleaseNotFound => {
            error_response(500, "projection read answered with a release record")
        }
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
