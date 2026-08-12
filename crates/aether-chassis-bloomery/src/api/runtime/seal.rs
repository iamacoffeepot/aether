//! `POST /drafts/{id}/seal` — the pre-seal approve gate and the deferred
//! signature verification it fans out.
//!
//! Sealing is the one route that runs a policy decision before it admits: every
//! draft membership is resolved against the tier policy (Pass 1), and a draft
//! whose members all resolve `auto` seals synchronously. Any above-`auto` member
//! turns the route into an N-way deferral (Pass 2) — one `aether.signing`
//! `Verify` per member, a held [`PendingSeal`] that
//! admits only once every signature verifies, and a fail-closed teardown the
//! moment one does not.

use std::collections::{BTreeMap, HashMap};

use serde::de::DeserializeOwned;

use aether_actor::Manual;
use aether_bloomery::{
    Admit, BloomDraft, BloomId, BloomSpec, Digest, Event, Fact, IdempotencyKey, Membership, Statement,
};
use aether_data::wire::to_vec;
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::hex::hex_encode;
use super::response::error_response;
use super::state::{
    ApiCapabilityState, MAX_OPEN_SEALS, MAX_SEAL_MEMBERS, PendingSeal, PendingSealSetup, PendingVerify, Routed,
    SealVerify, admit,
};
use crate::api::dto::{MemberProjection, SealRequest};
use crate::bloomery::{AdmissionRequest, Decision, Gate, precheck_statement, verified_statement_approval};
use crate::control::ControlCore;
use crate::signing::{SigningCapability, Verify, VerifyResult};
use crate::store::{RecordDispatchDescription, StoreCapability};

/// Which door a completed seal admits through (#4638): a first seal, or a
/// supersession of `predecessor`. Both carry the identical [`BloomSpec`] — the
/// gate, the descriptions, and the deferred-verify path are the same for each,
/// so the door is the only thing that varies.
fn admission_fact(predecessor: Option<BloomId>, spec: BloomSpec) -> Fact {
    match predecessor {
        None => Fact::Seal(spec),
        Some(predecessor) => Fact::Supersede { predecessor, successor: spec },
    }
}

impl ApiCapabilityState {
    /// `POST /drafts/{id}/seal` — run the pre-seal approve gate over every
    /// membership, then freeze the draft into a `BloomSpec` and admit `Fact::Seal`
    /// through the control core (issue #3583, the enforcement half of #3571's gate
    /// library).
    ///
    /// For each draft proposal the operator supplies a
    /// [`MemberProjection`] in the `SealRequest`,
    /// matched by `{workpiece, scope_revision}`. The host builds a
    /// [`Gate::AdmissionRequest`](crate::bloomery::AdmissionRequest) from it and
    /// runs [`Gate::evaluate`], replacing the proposal's operator-set `approval`
    /// with the gate-formed one so the seal-time `validate_member_admission`
    /// admits a policy-authored approval rather than an unchecked assertion.
    ///
    /// The gate is fail-closed at every branch: no loaded policy, a missing
    /// projection, or an `Incomplete` verdict all **refuse the seal** (`422`)
    /// rather than admit. An above-`auto` member takes the deferred
    /// signature-verification path (issue #3599): its projection's
    /// `signed_statement` is pre-checked synchronously (subject + author
    /// signature), then its signature is verified through the `aether.signing`
    /// capability's `Verify` round trip; the seal admits only once every
    /// above-`auto` member verifies, and a missing / mis-subjected / non-author /
    /// unverified statement refuses the whole seal (`422`, fail closed) — never
    /// admitted on the operator's unverified assertion. A seal with any
    /// above-`auto` member is therefore a deferred route ([`Routed::DeferredSeal`]);
    /// an all-`auto` draft stays synchronous.
    pub(super) fn seal_draft(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let (_, draft) = match self.lookup_draft(id) {
            Ok(found) => found,
            Err(response) => return Routed::Reply(response),
        };
        let request: SealRequest = match parse_optional_body(body) {
            Ok(request) => request,
            Err(response) => return Routed::Reply(response),
        };

        self.gate_and_admit(ctx, draft, None, &request.projections, request.descriptions, request.idempotency_key)
    }

    /// The gate-then-admit core both doors share (#4638): resolve every
    /// membership through [`Gate::evaluate`], then seal — synchronously when
    /// every member resolves `auto`, or deferred behind one `Verify` per
    /// above-`auto` member.
    ///
    /// `predecessor` selects the door. `None` admits [`Fact::Seal`]; `Some(id)`
    /// admits [`Fact::Supersede`] against that predecessor. A supersession is a
    /// second door into `active` claiming a fresh membership set, so it faces the
    /// same tier evaluation a first seal does — and, just as importantly, takes
    /// its approval from the gate rather than from the draft. A draft's own
    /// `approval` is a placeholder on both paths (the operator cannot compute
    /// [`Membership::subject`](aether_bloomery::Membership::subject)), so a route
    /// that sealed a draft ungated could never admit at all.
    #[allow(clippy::too_many_arguments, reason = "one request's parts, threaded from two differently-shaped bodies")]
    pub(super) fn gate_and_admit(
        &self,
        ctx: &NativeCtx<'_, Manual>,
        draft: BloomDraft,
        predecessor: Option<BloomId>,
        projections: &[MemberProjection],
        descriptions: BTreeMap<String, String>,
        idempotency_key: Option<String>,
    ) -> Routed {
        // Cap the seal's membership before any gate work or signing dispatch: each
        // above-auto member fans out one `Verify` and one held `SealVerify`
        // correlation, so an oversized draft would amplify one request into an
        // unbounded number of in-flight verifications and held-seal state. Refuse
        // past the ceiling (mirroring MAX_STAGED_WORKPIECES / MAX_OPEN_DRAFTS).
        if draft.proposals.len() > MAX_SEAL_MEMBERS {
            return Routed::Reply(error_response(
                422,
                &format!("draft has {} members; a seal is capped at {MAX_SEAL_MEMBERS}", draft.proposals.len()),
            ));
        }
        // No policy → fail closed: never admit a seal the gate could not decide.
        let Some(policy) = self.policy.as_ref() else {
            return Routed::Reply(error_response(422, "approval policy unavailable; seal fails closed"));
        };
        let gate = Gate::new(policy);
        // Pass 1: resolve every membership synchronously (fail-closed 422 on any
        // shortfall, before any signing dispatch).
        let (sealed_proposals, pending_verifications) =
            match resolve_seal_memberships(&gate, &draft.proposals, projections) {
                Ok(resolved) => resolved,
                Err(response) => return Routed::Reply(response),
            };
        let mut gated = draft;
        gated.proposals = sealed_proposals;
        // Pass 2. No above-auto member → seal synchronously (the all-auto fast
        // path, byte-for-byte #3583). Otherwise defer: dispatch one `Verify` per
        // above-auto member and hold the seal until every signature verifies.
        if pending_verifications.is_empty() {
            let spec = gated.seal();
            // Persist each member's advisory work-order description keyed by the
            // sealed bloom id, before the seal defers — the text is operator-supplied
            // context (#3595) the executor reactor reads back at dispatch, and the api
            // cap is mail-only, so it rides a store write rather than the sealed spec.
            // Best-effort and fire-and-forget: a description write never gates the
            // seal, and a member with none simply dispatches subject-only.
            Self::persist_descriptions(ctx, &spec, &descriptions);
            let key = idempotency_key.unwrap_or_else(|| hex_encode(spec.id().0.as_bytes()));
            return admit(&Event { idempotency_key: IdempotencyKey(key), fact: admission_fact(predecessor, spec) });
        }
        // Deferred path only: cap the outstanding `seals` map before dispatching any
        // `Verify`, so a flood of above-auto seals cannot grow the in-flight seal /
        // seal-verification state without bound (the all-auto fast path above never
        // touches `seals`, so it is deliberately not gated here). Mirrors the
        // `staged` / `drafts` admission caps.
        if self.seals.len() >= MAX_OPEN_SEALS {
            return Routed::Reply(error_response(429, "outstanding-seal budget exhausted"));
        }
        // Encode every above-auto member's statement before dispatching any
        // `Verify`: a `to_vec` fault inside the dispatch loop would 500 only
        // after members 1..k-1 had already been sent to `aether.signing`,
        // stranding those verifications with no held seal to correlate them.
        // Pre-encoding lets an encode failure bail with the 500 while the
        // dispatch state is still empty.
        let mut encoded = Vec::with_capacity(pending_verifications.len());
        for (member_index, scope_revision, statement) in pending_verifications {
            let statement_bytes = match to_vec(&statement) {
                Ok(bytes) => bytes,
                Err(error) => return Routed::Reply(error_response(500, &format!("statement encode failed: {error}"))),
            };
            encoded.push((member_index, scope_revision, statement, statement_bytes));
        }
        let mut verifications = Vec::with_capacity(encoded.len());
        for (member_index, scope_revision, statement, statement_bytes) in encoded {
            let correlation =
                self.send_tracked(ctx.actor::<SigningCapability>(), &Verify { statement: statement_bytes });
            verifications.push(PendingVerify { correlation, member_index, scope_revision, statement });
        }
        Routed::DeferredSeal(Box::new(PendingSealSetup {
            gated,
            predecessor,
            descriptions,
            idempotency_key,
            verifications,
        }))
    }

    /// Write one dispatch-description row per member the operator supplied text
    /// for, keyed by (sealed bloom id, workpiece). Fire-and-forget to the
    /// `aether.store` mailbox — the reply is absorbed by
    /// [`on_record_description_result`](super::BloomeryApiCapability::on_record_description_result);
    /// the seal's own outcome is unaffected. A description for a member that later
    /// fails to seal is an orphan row keyed by a bloom id that never dispatches —
    /// harmless and never read.
    ///
    /// Shared with the supersede route next door, which mints a second bloom id
    /// and so needs its own rows (#4631).
    pub(super) fn persist_descriptions(
        ctx: &NativeCtx<'_, Manual>,
        spec: &BloomSpec,
        descriptions: &BTreeMap<String, String>,
    ) {
        let bloom = spec.id().0.as_bytes().to_vec();
        for member in spec.members() {
            let Some(description) = descriptions.get(&member.workpiece.0) else {
                continue;
            };
            let record = RecordDispatchDescription {
                bloom: bloom.clone(),
                workpiece: member.workpiece.0.clone(),
                description: description.clone(),
            };
            // Fire-and-forget: the seal replies from its own admit outcome, so the
            // returned MailId is deliberately dropped (no settlement subscription).
            ctx.actor::<StoreCapability>().send_detached(&record);
        }
    }

    /// Resolve one above-auto member verification for a held seal (issue #3599):
    /// a verified signature forms the member's approval and decrements the seal's
    /// countdown, sealing and admitting when the last one lands; a `verified:
    /// false` verdict or an `Err` refuses the whole seal (`422`, fail closed) and
    /// tears down its sibling correlations.
    pub(super) fn resolve_seal_verify(&mut self, ctx: &NativeCtx<'_, Manual>, correlation: u64, result: VerifyResult) {
        let Some(SealVerify { seal, member_index, scope_revision, statement }) =
            self.seal_verifications.remove(&correlation)
        else {
            return;
        };
        match result {
            VerifyResult::Ok { verified: true } => {
                // A sibling verification may have already failed the seal and torn
                // it down; if so this verified reply has nothing to fill.
                let Some(pending) = self.seals.get_mut(&seal) else {
                    return;
                };
                pending.gated.proposals[member_index].approval =
                    verified_statement_approval(scope_revision, &statement);
                pending.remaining -= 1;
                if pending.remaining > 0 {
                    return;
                }
                // Last verification: seal the fully-approved draft and admit,
                // deferring on the reducer reply exactly as the synchronous path.
                let PendingSeal { inbound, predecessor, gated, descriptions, idempotency_key, .. } =
                    self.seals.remove(&seal).expect("seal present; just mutated it");
                let spec = gated.seal();
                Self::persist_descriptions(ctx, &spec, &descriptions);
                let key = idempotency_key.unwrap_or_else(|| hex_encode(spec.id().0.as_bytes()));
                match to_vec(&Event { idempotency_key: IdempotencyKey(key), fact: admission_fact(predecessor, spec) }) {
                    Ok(bytes) => {
                        let correlation = self.send_tracked(ctx.actor::<ControlCore>(), &Admit { event: bytes });
                        self.pending.insert(correlation, inbound);
                    }
                    Err(error) => {
                        inbound.reply(&error_response(500, &format!("event encode failed: {error}")));
                    }
                }
            }
            VerifyResult::Ok { verified: false } => {
                self.fail_seal(seal, 422, "an above-auto member's signed statement did not verify; seal fails closed");
            }
            VerifyResult::Err { error } => {
                self.fail_seal(
                    seal,
                    422,
                    &format!("an above-auto member's signature verification failed: {error}; seal fails closed"),
                );
            }
        }
    }

    /// Refuse a held seal and tear it down: reply `status`/`message` to the held
    /// obligation and drop every sibling verify correlation still pointing at it,
    /// so a late sibling reply (or settlement) is a no-op rather than a second
    /// teardown or a double reply. A no-op if the seal was already torn down.
    pub(super) fn fail_seal(&mut self, seal: u64, status: u16, message: &str) {
        let Some(PendingSeal { inbound, .. }) = self.seals.remove(&seal) else {
            return;
        };
        self.seal_verifications.retain(|_, verify| verify.seal != seal);
        inbound.reply(&error_response(status, message));
    }
}

/// One above-auto member queued for the deferred signature verify: its proposal
/// index, scope-revision digest, and signed statement — Pass 1's carry into Pass 2.
type PendingVerification = (usize, Digest, Statement);

/// Pass 1 of a seal: resolve every draft membership synchronously against the
/// `gate`. An auto member is gate-formed in place; an above-auto member has its
/// signed statement pre-checked and is queued (its `(index, scope_revision,
/// statement)`) for the deferred `aether.signing` verify. Any missing projection,
/// `Incomplete` verdict, missing statement, or failing pre-check returns the
/// fail-closed `422` response instead — resolved before any signing dispatch.
///
/// Extracted from [`seal_draft`](ApiCapabilityState::seal_draft) so that hot
/// path stays under the line ceiling; the two returned vectors are its Pass-2
/// input (the gated proposals and the above-auto members still to verify).
fn resolve_seal_memberships(
    gate: &Gate<'_>,
    proposals: &[Membership],
    request_projections: &[MemberProjection],
) -> Result<(Vec<Membership>, Vec<PendingVerification>), HttpServerResponse> {
    // Indexed once so the member loop stays O(n + m) however many projections the
    // request carries; first occurrence wins on a duplicate key, matching the
    // linear scan this replaces.
    let mut projections = HashMap::with_capacity(request_projections.len());
    for projection in request_projections {
        projections.entry((&projection.workpiece, &projection.scope_revision)).or_insert(projection);
    }
    let mut sealed_proposals = Vec::with_capacity(proposals.len());
    let mut pending_verifications: Vec<PendingVerification> = Vec::new();
    for (index, proposal) in proposals.iter().enumerate() {
        let member = &proposal.workpiece.0;
        let Some(&projection) = projections.get(&(&proposal.workpiece, &proposal.scope_revision)) else {
            return Err(error_response(422, &format!("member {member} has no scope projection; seal fails closed")));
        };
        // The digest binds the approval to the projection as evaluated. It is
        // computed over the canonical wire re-encoding of the decoded struct, not
        // the raw request slice: the JSON body carries no per-projection byte
        // boundaries, and the canonical form is reproducible from the shared DTO by
        // any party rather than sensitive to the sender's whitespace and field order.
        let projection_digest = match to_vec(projection) {
            Ok(bytes) => Digest::of_wire_bytes(&bytes),
            Err(error) => return Err(error_response(500, &format!("projection encode failed: {error}"))),
        };
        let admission = AdmissionRequest {
            subject: proposal.subject(),
            declared_surface: projection.declared_surface.clone(),
            completeness: projection.completeness,
            adr_touch: projection.adr_touch,
            pre_approved: projection.pre_approved,
            projection_digest,
        };
        match gate.evaluate(&admission) {
            Decision::AutoApproved(approval) => {
                let mut sealed = proposal.clone();
                sealed.approval = approval;
                sealed_proposals.push(sealed);
            }
            Decision::Incomplete(incompleteness) => {
                return Err(error_response(422, &format!("member {member} is incomplete: {incompleteness:?}")));
            }
            Decision::RequiresStatement(_tier) => {
                // Above-auto: consume the member projection's signed statement, run
                // the two synchronous pre-checks (subject + author signature), and
                // queue it for the async signature verify. A missing or
                // pre-check-failing statement fails closed here, before any signing
                // dispatch. The proposal's placeholder approval is overwritten by the
                // verified evidence before seal.
                let Some(statement) = projection.signed_statement.as_ref() else {
                    return Err(error_response(
                        422,
                        &format!(
                            "member {member} resolves above auto but carries no signed statement; seal fails closed"
                        ),
                    ));
                };
                if let Err(rejected) = precheck_statement(proposal.subject(), statement) {
                    return Err(error_response(
                        422,
                        &format!("member {member} signed statement rejected: {rejected:?}; seal fails closed"),
                    ));
                }
                sealed_proposals.push(proposal.clone());
                pending_verifications.push((index, proposal.subject(), statement.clone()));
            }
        }
    }
    Ok((sealed_proposals, pending_verifications))
}

/// Parse a possibly-empty request body into a `Default` body type: an empty
/// body is the default, a non-empty one is parsed, a malformed one is a `400`.
fn parse_optional_body<T: DeserializeOwned + Default>(body: &[u8]) -> Result<T, HttpServerResponse> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|error| error_response(400, &format!("invalid request body: {error}")))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{BloomDraft, BloomId, Digest, Fact};

    use super::{SealRequest, admission_fact, parse_optional_body};

    #[test]
    fn no_predecessor_admits_through_the_seal_door() {
        // Tripwire on the door selection (#4638). Both doors carry an identical
        // spec through an identical gate, so nothing downstream would notice a
        // swap — but a first seal admitted as a supersession names a predecessor
        // that does not exist, and a supersession admitted as a seal is refused
        // for the very active bloom it was meant to replace.
        let spec = BloomDraft::default().seal();

        assert!(matches!(admission_fact(None, spec), Fact::Seal(_)), "an unset predecessor is a first seal");
    }

    #[test]
    fn a_predecessor_admits_through_the_supersede_door() {
        let predecessor = BloomId(Digest::from_bytes([3; 32]));
        let spec = BloomDraft::default().seal();

        match admission_fact(Some(predecessor), spec) {
            Fact::Supersede { predecessor: named, .. } => {
                assert_eq!(named, predecessor, "the supersession must name the predecessor it was given");
            }
            other => panic!("a predecessor must admit a supersession, got {other:?}"),
        }
    }

    #[test]
    fn optional_body_defaults_when_empty() {
        // An empty seal body resolves the default (no idempotency-key override);
        // a malformed one is a `400`, not a panic.
        let empty: SealRequest = parse_optional_body(b"").expect("empty body is the default");
        assert!(empty.idempotency_key.is_none());
        let parsed: SealRequest = parse_optional_body(br#"{"idempotency_key":"k"}"#).expect("well-formed body parses");
        assert_eq!(parsed.idempotency_key.as_deref(), Some("k"));
        assert!(parse_optional_body::<SealRequest>(b"not json").is_err());
    }

    #[test]
    fn seal_request_descriptions_default_empty_and_parse_per_member() {
        // The #3595 operator contract: `descriptions` is optional (a body without
        // it still seals, not a 400 — the `#[serde(default)]` guard), and when
        // present it maps each workpiece id to its work-order text. A regression
        // dropping the default would break every description-less seal.
        let none: SealRequest =
            parse_optional_body(br#"{"idempotency_key":"k"}"#).expect("no descriptions still parses");
        assert!(none.descriptions.is_empty(), "an absent descriptions map defaults empty rather than erroring");

        let with: SealRequest =
            parse_optional_body(br#"{"descriptions":{"wp-a":"build the thing","wp-b":"and the other"}}"#)
                .expect("a descriptions map parses");
        assert_eq!(with.descriptions.get("wp-a").map(String::as_str), Some("build the thing"));
        assert_eq!(with.descriptions.get("wp-b").map(String::as_str), Some("and the other"));
    }
}
