//! Authenticated commission authoring and query routes (ADR-0199 slice 2).
//!
//! Every route here requires `Authorization: Bearer` against the configured
//! control token. The coordinator never signs and never accepts private-key
//! bytes: create / revision / show / list talk to the store; approval and
//! cancel verify a submitted envelope through `aether.signing` before the
//! store write.

use aether_actor::Manual;
use aether_bloomery::{
    AuthorityDoor, CommissionStatus, Digest, Observation, Provenance, ScopeRevision, ScopeVerifyReport, Statement,
    WorkpieceId, digest_of,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::{HttpHeader, HttpServerRequest, HttpServerResponse};
use aether_substrate::actor::native::NativeCtx;

use super::hex;
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed};
use crate::api::dto::{
    CancelCommissionRequest, CommissionApprovalView, CommissionCancelledView, CommissionCreatedView,
    CommissionHeadView, CommissionReopenedView, CommissionShowView, CommissionsView, CreateCommissionRequest,
    ReopenCommissionRequest, ScopeRevisionWrittenView, ScopeRunOpenedView, ScopeRunRequest, WriteRevisionRequest,
};
use crate::bloomery::{ApprovalPolicy, Gate, StatementRejected, Tier, precheck_statement, verified_statement_approval};
use crate::signing::{SigningCapability, Verify, VerifyResult, authority_bytes};
use crate::store::{
    CancelCommission, CancelCommissionResult, CreateCommission, CreateCommissionResult, EnqueueScopeRun,
    EnqueueScopeRunResult, ListCommissions, ListCommissionsResult, ListedCommission, LoadCommission,
    LoadCommissionResult, RecordCommissionApproval, RecordCommissionApprovalResult, ReopenCommission,
    ReopenCommissionResult, StoreCapability, WriteScopeRevision, WriteScopeRevisionResult,
};

#[cfg(test)]
mod tests;

/// Why a commission write is held across `aether.signing` verification.
pub(super) enum CommissionWrite {
    /// Persist an approval for the current revision after the signature verifies.
    Approval {
        /// The workpiece the path named, so a verified write can refuse a
        /// statement bound to a different commission.
        id: WorkpieceId,
        /// The submitted statement.
        statement: Statement,
    },
    /// Persist a cancel after the signature verifies.
    Cancel {
        /// The workpiece the path named.
        id: WorkpieceId,
        /// The submitted cancel statement.
        statement: Statement,
        /// Operator context for the cancel. Never authority: the signature and
        /// the intent digest decide, and this field changes nothing about that.
        reason: String,
    },
    /// Restore a stranded commission after the signature verifies.
    Reopen {
        /// The workpiece the path named.
        id: WorkpieceId,
        /// The submitted reopen statement.
        statement: Statement,
        /// Operator context for the reopen. Never authority, exactly as the
        /// cancel's is not.
        reason: String,
    },
}

/// A commission write waiting on a signature verification.
pub(super) struct CommissionVerify {
    /// The held HTTP reply obligation.
    pub(super) inbound: aether_substrate::InboundMail,
    /// The write to persist once (and only if) the signature verifies.
    pub(super) write: CommissionWrite,
}

/// Check the request's bearer against the configured control token.
///
/// An empty configured token refuses every request: the surface that can
/// approve work is fail-closed when nothing has been configured to authenticate.
pub(in crate::api::runtime) fn authorize(request: &HttpServerRequest, token: &str) -> Result<(), HttpServerResponse> {
    if token.is_empty() {
        return Err(error_response(401, "unauthenticated"));
    }
    match bearer(&request.headers) {
        Some(provided) if tokens_match(token, provided) => Ok(()),
        _ => Err(error_response(401, "unauthenticated")),
    }
}

fn bearer(headers: &[HttpHeader]) -> Option<&str> {
    headers.iter().find_map(|header| {
        if !header.name.eq_ignore_ascii_case("authorization") {
            return None;
        }
        let value = header.value.trim();
        let (scheme, rest) = value.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then_some(rest.trim())
    })
}

fn tokens_match(expected: &str, provided: &str) -> bool {
    if expected.len() != provided.len() {
        return false;
    }
    expected.bytes().zip(provided.bytes()).fold(0_u8, |acc, (left, right)| acc | (left ^ right)) == 0
}

impl ApiCapabilityState {
    /// `POST /commissions` — persist a new open commission.
    pub(super) fn create_commission(&self, request: &HttpServerRequest) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        let body: CreateCommissionRequest = match hex::from_slice(&request.body) {
            Ok(body) => body,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid commission body: {error}"))),
        };
        let intent = match to_vec(&body.intent) {
            Ok(intent) => intent,
            Err(error) => return Routed::Reply(error_response(500, &format!("intent encode failed: {error}"))),
        };
        Routed::CreateCommission(CreateCommission { id: body.id.0, intent })
    }

    /// `POST /commissions/{id}/scope-runs` — open a pre-bloom scoping run.
    pub(super) fn enqueue_scope_run(&self, request: &HttpServerRequest, id: &str) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        let body: ScopeRunRequest = match hex::from_slice(&request.body) {
            Ok(body) => body,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid scope-run body: {error}"))),
        };
        Routed::EnqueueScopeRun(EnqueueScopeRun { id: id.to_owned(), base: body.base.as_bytes().to_vec() })
    }

    /// `POST /commissions/{id}/revisions` — write a scope revision.
    pub(super) fn write_commission_revision(&self, request: &HttpServerRequest, id: &str) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        let body: WriteRevisionRequest = match hex::from_slice(&request.body) {
            Ok(body) => body,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid revision body: {error}"))),
        };
        if body.revision.workpiece.0 != id {
            return Routed::Reply(error_response(400, "scope revision workpiece does not match the path"));
        }
        Routed::WriteScopeRevision(WriteScopeRevision {
            canonical: body.revision.to_canonical(),
            evidence: body.evidence.encode(),
        })
    }

    /// `POST /commissions/{id}/approvals` — precheck, then verify, then store.
    pub(super) fn submit_commission_approval(
        &self,
        ctx: &NativeCtx<'_, Manual>,
        request: &HttpServerRequest,
        id: &str,
    ) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        let statement: Statement = match hex::from_slice(&request.body) {
            Ok(statement) => statement,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid approval statement: {error}"))),
        };
        let Some(scope) = Digest::from_slice(&statement.words) else {
            return Routed::Reply(error_response(400, "approval words are not a scope digest"));
        };
        if let Err(rejected) = precheck_statement(scope, &statement) {
            return Routed::Reply(approval_rejected(rejected));
        }
        let encoded = match to_vec(&statement) {
            Ok(encoded) => encoded,
            Err(error) => return Routed::Reply(error_response(500, &format!("approval encode failed: {error}"))),
        };
        let correlation = self.send_tracked(
            ctx.actor::<SigningCapability>(),
            // No tier requirement: this door records an approval row, and it
            // holds no declared surface to resolve a tier from. The tier→signer
            // binding is applied where the tier exists — the seal gate, which
            // resolves each member's surface and refuses a row signed below it
            // (#5324) — so a row stored here still faces that check before it
            // ever admits a member.
            &Verify {
                statement: encoded,
                authority: authority_bytes(AuthorityDoor::Approve, scope),
                required_tier: None,
            },
        );
        Routed::DeferredCommissionVerify {
            correlation,
            write: CommissionWrite::Approval { id: WorkpieceId(id.to_owned()), statement },
        }
    }

    /// `POST /commissions/{id}/approvals/auto` — record the unsigned `auto`
    /// approval the store already models but nothing produced (#5325).
    ///
    /// The store has always accepted an `ObservationAttestation` approval row
    /// as tier `Auto` — the seal gate forms exactly such a row for itself — but
    /// [`submit_commission_approval`](Self::submit_commission_approval) refuses
    /// any statement that is not an author signature, so the row had no
    /// producer over the control API and even an `auto`-tier commission had to
    /// reach for the operator's signing key.
    ///
    /// This door mints the row instead of accepting one. The caller supplies no
    /// statement and no signature, because there is nothing here for a caller
    /// to assert: the words are the current revision's own digest and the
    /// provenance is this gate's observation that the tier policy resolved
    /// `auto`. What the caller *cannot* do is claim the tier — the door reads
    /// the stored revision's declared surface and resolves it against the
    /// policy itself, and refuses upward with the tier it found. That refusal
    /// is what keeps this from becoming a way to self-approve anything: an
    /// unsigned approval is only ever available where a signature was never
    /// what the ladder asked for.
    ///
    /// Deferred behind a [`LoadCommission`], because the surface lives in the
    /// store and a door that trusted the caller's account of it would be
    /// deciding the tier from the request it is gating.
    pub(super) fn auto_approve_commission(&self, request: &HttpServerRequest, id: &str) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        Routed::AutoApproveCommission { request: LoadCommission { id: id.to_owned() }, id: WorkpieceId(id.to_owned()) }
    }

    /// Resolve a held auto-approval door from the store's commission load:
    /// decide it against [`auto_approval_write`], then dispatch the minted
    /// unsigned approval (#5325).
    ///
    /// Returns the write correlation to hold the obligation against, or the
    /// response that answers it now.
    pub(super) fn resolve_auto_approval(
        &self,
        ctx: &NativeCtx<'_, Manual>,
        id: &WorkpieceId,
        result: LoadCommissionResult,
    ) -> Result<u64, HttpServerResponse> {
        let write = auto_approval_write(self.file_policy.as_ref(), id, result)?;

        Ok(self.send_tracked(ctx.actor::<StoreCapability>(), &write))
    }

    /// `GET /commissions/{id}` — show one commission.
    pub(super) fn show_commission(&self, request: &HttpServerRequest, id: &str) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        Routed::LoadCommission(LoadCommission { id: id.to_owned() })
    }

    /// `GET /commissions` — list commissions, optionally filtered by status.
    pub(super) fn list_commissions(&self, request: &HttpServerRequest) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        let status = query_status(&request.query);
        Routed::ListCommissions(ListCommissions { status })
    }

    /// `POST /commissions/{id}/cancel` — verify a cancel envelope, then close.
    pub(super) fn cancel_commission(
        &self,
        ctx: &NativeCtx<'_, Manual>,
        request: &HttpServerRequest,
        id: &str,
    ) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        let (statement, intent, reason) = match cancel_request(&request.body) {
            Ok(parsed) => parsed,
            Err(response) => return Routed::Reply(response),
        };
        let encoded = match to_vec(&statement) {
            Ok(encoded) => encoded,
            Err(error) => return Routed::Reply(error_response(500, &format!("cancel encode failed: {error}"))),
        };
        let correlation = self.send_tracked(
            ctx.actor::<SigningCapability>(),
            &Verify {
                statement: encoded,
                authority: authority_bytes(AuthorityDoor::Cancel, intent),
                required_tier: None,
            },
        );
        Routed::DeferredCommissionVerify {
            correlation,
            write: CommissionWrite::Cancel { id: WorkpieceId(id.to_owned()), statement, reason },
        }
    }

    /// `POST /commissions/{id}/reopen` — verify a reopen envelope, then restore.
    ///
    /// The store decides whether the commission may come back: this door proves
    /// only that an authorized operator asked for it at the Reopen door, bound
    /// to this commission's own intent.
    pub(super) fn reopen_commission(
        &self,
        ctx: &NativeCtx<'_, Manual>,
        request: &HttpServerRequest,
        id: &str,
    ) -> Routed {
        if let Err(response) = authorize(request, &self.control_token) {
            return Routed::Reply(response);
        }
        let (statement, intent, reason) = match reopen_request(&request.body) {
            Ok(parsed) => parsed,
            Err(response) => return Routed::Reply(response),
        };
        let encoded = match to_vec(&statement) {
            Ok(encoded) => encoded,
            Err(error) => return Routed::Reply(error_response(500, &format!("reopen encode failed: {error}"))),
        };
        let correlation = self.send_tracked(
            ctx.actor::<SigningCapability>(),
            &Verify {
                statement: encoded,
                authority: authority_bytes(AuthorityDoor::Reopen, intent),
                required_tier: None,
            },
        );
        Routed::DeferredCommissionVerify {
            correlation,
            write: CommissionWrite::Reopen { id: WorkpieceId(id.to_owned()), statement, reason },
        }
    }

    /// Resolve a held commission verify: persist on a verified signature, or
    /// answer `400` so a refusal is not a transport error.
    pub(super) fn resolve_commission_verify(&mut self, ctx: &NativeCtx<'_, Manual>, result: VerifyResult) {
        let correlation = ctx.reply_target().correlation_id;
        let Some(CommissionVerify { inbound, write }) = self.commission_verifying.remove(&correlation) else {
            return;
        };
        match result {
            VerifyResult::Ok { verified: true } => match persist_verified(self, ctx, write) {
                Ok(correlation) => {
                    self.commission_writing.insert(correlation, inbound);
                }
                Err(response) => {
                    inbound.reply(&response);
                }
            },
            VerifyResult::Ok { verified: false } => {
                inbound.reply(&error_response(400, "signed statement is not an author signature or did not verify"));
            }
            VerifyResult::Err { error } => {
                inbound.reply(&error_response(400, &format!("signed statement did not verify: {error}")));
            }
            // Unreachable: none of these doors names a `required_tier`, so the
            // signing cap has no ladder to refuse against. Answered rather than
            // ignored, so a door that later starts naming one cannot lose its
            // refusal to a silent fall-through.
            VerifyResult::BelowTier { required, ceiling } => {
                inbound.reply(&error_response(
                    400,
                    &format!("signer is authorized to {ceiling:?} tier, below the {required:?} tier required"),
                ));
            }
        }
    }

    /// Answer a held commission write from the store's reply.
    pub(super) fn answer_commission_write(&mut self, ctx: &NativeCtx<'_, Manual>, response: &HttpServerResponse) {
        if let Some(inbound) = self.commission_writing.remove(&ctx.reply_target().correlation_id) {
            inbound.reply(response);
        }
    }
}

/// The store write an auto-approval door produces, or the refusal that answers
/// it instead (#5325).
///
/// The whole decision, kept pure so the tier gate is testable without a live
/// capability: the only thing [`resolve_auto_approval`] adds is the dispatch.
///
/// `policy` is the host's file-loaded ladder. A lone commission seals no
/// bloom-wide configuration, so there is no attested policy to prefer over it —
/// and `None` is a refusal rather than a default, because a door that guessed
/// `auto` because it could not read the ladder is the one thing this route must
/// never do.
///
/// # Errors
/// The commission has nothing to approve, no policy was loaded, or its declared
/// surface resolves above `auto` — in which case the refusal names the tier it
/// found, since "not auto" alone does not tell an operator whose signature to
/// go and get.
///
/// [`resolve_auto_approval`]: ApiCapabilityState::resolve_auto_approval
fn auto_approval_write(
    policy: Option<&ApprovalPolicy>,
    id: &WorkpieceId,
    result: LoadCommissionResult,
) -> Result<RecordCommissionApproval, HttpServerResponse> {
    let revision = auto_approval_revision(id, result)?;

    let Some(policy) = policy else {
        return Err(error_response(422, "approval policy unavailable; auto approval fails closed"));
    };
    let tier = Gate::new(policy).tier_of_declaration(&revision.declared_surface, &revision.declared_crates);
    if tier != Tier::Auto {
        return Err(error_response(
            422,
            &format!(
                "commission {} declares a surface that resolves {tier:?} tier; an unsigned auto approval is \
                 available only at auto tier, so this one needs a signed statement",
                id.0
            ),
        ));
    }

    // The digest is recomputed from the canonical bytes rather than read off
    // the load's index column, so the approval binds the revision the store
    // actually holds.
    let statement = to_vec(&auto_approval_statement(digest_of(&revision)))
        .map_err(|error| error_response(500, &format!("auto approval encode failed: {error}")))?;

    Ok(RecordCommissionApproval { id: id.0.clone(), statement })
}

/// The source label an auto-tier commission approval's observation carries.
///
/// Distinct from the seal gate's own `approve_gate:auto-tier` label: both
/// record that a policy resolved `auto`, and a reader of a stored row should be
/// able to tell which door decided it.
const AUTO_APPROVAL_SOURCE: &str = "aether.bloomery.commission_approve:auto-tier";

/// The unsigned `auto` approval for `scope` (#5325).
///
/// An observation attestation, never an author signature: nobody asserted
/// anything here, the gate observed that the tier policy resolved `auto`. That
/// is precisely the provenance `classify_approval` files as tier `Auto`, so the
/// store needs no new shape to accept it.
///
/// Deterministic from `scope` alone, so a re-POST re-mints a byte-identical
/// statement and the store's duplicate check makes the second attempt a no-op
/// rather than a second row.
fn auto_approval_statement(scope: Digest) -> Statement {
    Statement {
        words: scope.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: AUTO_APPROVAL_SOURCE.to_owned() }),
        parents: vec![scope],
    }
}

/// The current revision an auto approval would bind, or the refusal that
/// answers the door instead.
///
/// Every branch here is a commission that has nothing for this door to approve
/// — no such commission, no revision written yet, bytes that do not decode, or
/// a lifecycle that has already closed. The digest is recomputed from the
/// canonical bytes rather than taken from the row's index column, because that
/// digest is what the minted approval binds.
fn auto_approval_revision(id: &WorkpieceId, result: LoadCommissionResult) -> Result<ScopeRevision, HttpServerResponse> {
    match result {
        LoadCommissionResult::Ok { status, current, current_unreadable, .. } => {
            if status != CommissionStatus::Open.as_str() {
                return Err(error_response(422, &format!("commission {} is not open", id.0)));
            }
            if let Some(reason) = current_unreadable {
                return Err(error_response(
                    422,
                    &format!("commission {} current revision is unreadable: {reason}", id.0),
                ));
            }
            let Some(canonical) = current else {
                return Err(error_response(422, &format!("commission {} has no scope revision to approve", id.0)));
            };
            ScopeRevision::from_canonical(&canonical).map_err(|error| {
                error_response(422, &format!("commission {} current revision is unreadable: {error}", id.0))
            })
        }
        LoadCommissionResult::Missing { id } => Err(error_response(404, &format!("commission {id} not found"))),
        LoadCommissionResult::Err { error } => Err(error_response(500, &format!("commission load failed: {error}"))),
    }
}

fn persist_verified(
    state: &ApiCapabilityState,
    ctx: &NativeCtx<'_, Manual>,
    write: CommissionWrite,
) -> Result<u64, HttpServerResponse> {
    match write {
        CommissionWrite::Approval { id, statement } => {
            let statement =
                to_vec(&statement).map_err(|error| error_response(500, &format!("approval encode failed: {error}")))?;
            Ok(state.send_tracked(ctx.actor::<StoreCapability>(), &RecordCommissionApproval { id: id.0, statement }))
        }
        CommissionWrite::Cancel { id, statement, reason } => {
            let statement =
                to_vec(&statement).map_err(|error| error_response(500, &format!("cancel encode failed: {error}")))?;
            tracing::info!(
                target: "aether_chassis_bloomery::api",
                commission = %id.0,
                reason = %reason,
                "commission cancelled"
            );
            Ok(state.send_tracked(ctx.actor::<StoreCapability>(), &CancelCommission { id: id.0, statement }))
        }
        CommissionWrite::Reopen { id, statement, reason } => {
            // The one durable trace of who restored this commission: the
            // statement is not filed (see the store's `reopen_commission`), so
            // the log line carries the signer the door verified.
            let signer = match &statement.provenance {
                Provenance::AuthorSignature(envelope) => envelope.signer.0.clone(),
                // The door refuses everything else before the verification is
                // even spent, so these cannot reach a persisted write.
                Provenance::ObservationAttestation(_) | Provenance::StageReceipt(_) => "unsigned".to_owned(),
            };
            let statement =
                to_vec(&statement).map_err(|error| error_response(500, &format!("reopen encode failed: {error}")))?;
            tracing::info!(
                target: "aether_chassis_bloomery::api",
                commission = %id.0,
                signer = %signer,
                reason = %reason,
                "commission reopen authorized"
            );
            Ok(state.send_tracked(ctx.actor::<StoreCapability>(), &ReopenCommission { id: id.0, statement }))
        }
    }
}

fn approval_rejected(rejected: StatementRejected) -> HttpServerResponse {
    match rejected {
        StatementRejected::WrongSubject => error_response(400, "approval words are not the scope digest"),
        StatementRejected::NotAnAuthorSignature => error_response(400, "approval is not an author signature"),
        StatementRejected::Unverified => error_response(400, "approval signature did not verify"),
        StatementRejected::BelowTier { required, ceiling } => error_response(
            403,
            &format!("approval signer is authorized to {ceiling:?} tier, below the {required:?} tier required"),
        ),
    }
}

fn cancel_request(body: &[u8]) -> Result<(Statement, Digest, String), HttpServerResponse> {
    let request: CancelCommissionRequest = match hex::from_slice(body) {
        Ok(request) => request,
        Err(error) => return Err(error_response(400, &format!("invalid cancel body: {error}"))),
    };
    intent_door_request(request.statement, request.reason, "cancel")
}

fn reopen_request(body: &[u8]) -> Result<(Statement, Digest, String), HttpServerResponse> {
    let request: ReopenCommissionRequest = match hex::from_slice(body) {
        Ok(request) => request,
        Err(error) => return Err(error_response(400, &format!("invalid reopen body: {error}"))),
    };
    intent_door_request(request.statement, request.reason, "reopen")
}

/// The checks an intent-bound door runs before it spends a signature
/// verification: the operator said why, the words are a digest at all, and the
/// statement is shaped like an author signature over them.
///
/// `door` names the caller in the refusals, because a message that says
/// "reason is required" without saying which act was refused sends the operator
/// back to the route table.
fn intent_door_request(
    statement: Statement,
    reason: String,
    door: &str,
) -> Result<(Statement, Digest, String), HttpServerResponse> {
    if reason.trim().is_empty() {
        return Err(error_response(400, &format!("{door} reason is required")));
    }
    let Some(intent) = Digest::from_slice(&statement.words) else {
        return Err(error_response(400, &format!("{door} words are not an intent digest")));
    };
    if let Err(rejected) = precheck_statement(intent, &statement) {
        return Err(approval_rejected(rejected));
    }
    Ok((statement, intent, reason))
}

fn query_status(query: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "status" && !value.is_empty()).then(|| value.to_owned())
    })
}

fn digest_of_bytes(bytes: &[u8]) -> Result<Digest, HttpServerResponse> {
    Digest::from_slice(bytes).ok_or_else(|| error_response(500, "store echoed a digest that is not 32 bytes"))
}

/// Render [`CreateCommissionResult`].
pub(super) fn create_response(result: CreateCommissionResult) -> HttpServerResponse {
    match result {
        CreateCommissionResult::Ok { id, digest } => match digest_of_bytes(&digest) {
            Ok(intent) => json(201, &CommissionCreatedView { id: WorkpieceId(id), intent }),
            Err(response) => response,
        },
        CreateCommissionResult::Duplicate { id } => error_response(409, &format!("commission {id} already exists")),
        CreateCommissionResult::Err { error } => error_response(500, &format!("commission create failed: {error}")),
    }
}

/// Render [`WriteScopeRevisionResult`].
pub(super) fn revision_response(result: WriteScopeRevisionResult) -> HttpServerResponse {
    match result {
        WriteScopeRevisionResult::Ok { digest } => match digest_of_bytes(&digest) {
            Ok(digest) => json(201, &ScopeRevisionWrittenView { digest }),
            Err(response) => response,
        },
        WriteScopeRevisionResult::Missing { id } => error_response(404, &format!("no commission named {id}")),
        WriteScopeRevisionResult::Stale => {
            error_response(409, "scope revision is not the commission's current revision")
        }
        WriteScopeRevisionResult::Duplicate => error_response(409, "scope revision is already stored"),
        WriteScopeRevisionResult::Ordinal { expected } => {
            error_response(409, &format!("scope revision is not the next ordinal (expected {expected})"))
        }
        WriteScopeRevisionResult::UnsupportedSchema { schema } => {
            error_response(400, &format!("scope revision schema {schema} is not supported"))
        }
        WriteScopeRevisionResult::Malformed => error_response(400, "canonical commission bytes are malformed"),
        WriteScopeRevisionResult::Err { error } => {
            error_response(500, &format!("scope revision write failed: {error}"))
        }
        WriteScopeRevisionResult::NotOpen => error_response(409, "commission is not open"),
        WriteScopeRevisionResult::SurfaceGap { paths } => {
            error_response(422, &format!("declared surface does not cover {}", paths.join(", ")))
        }
    }
}

/// Render [`RecordCommissionApprovalResult`].
pub(super) fn approval_response(result: RecordCommissionApprovalResult) -> HttpServerResponse {
    match result {
        RecordCommissionApprovalResult::Ok { digest, statement } => {
            let Ok(digest) = digest_of_bytes(&digest) else {
                return error_response(500, "store echoed a digest that is not 32 bytes");
            };
            let statement: Statement = match from_bytes(&statement) {
                Ok(statement) => statement,
                Err(_) => return error_response(500, "stored approval is malformed"),
            };
            let Some(scope) = Digest::from_slice(&statement.words) else {
                return error_response(500, "stored approval words are not a scope digest");
            };
            json(201, &CommissionApprovalView { digest, evidence: verified_statement_approval(scope, &statement) })
        }
        RecordCommissionApprovalResult::MissingRevision => error_response(404, "scope revision is not in the store"),
        RecordCommissionApprovalResult::Stale => {
            error_response(409, "scope revision is not the commission's current revision")
        }
        RecordCommissionApprovalResult::Refused { error } => error_response(400, &error),
        RecordCommissionApprovalResult::Err { error } => {
            error_response(500, &format!("commission approval write failed: {error}"))
        }
        RecordCommissionApprovalResult::NotOpen => error_response(409, "commission is not open"),
    }
}

/// The decoded current revision, or the unreadable-body marker when those
/// bytes are not this binary's shape. A missing tip stays missing.
fn show_current(
    current: Option<Vec<u8>>,
    current_unreadable: Option<String>,
) -> (Option<ScopeRevision>, Option<String>) {
    match current {
        Some(bytes) => match ScopeRevision::from_canonical(&bytes) {
            Ok(revision) => (Some(revision), None),
            Err(error) => (None, Some(current_unreadable.unwrap_or_else(|| error.to_string()))),
        },
        None => (None, current_unreadable),
    }
}

/// Render [`LoadCommissionResult`].
pub(super) fn show_response(result: LoadCommissionResult) -> HttpServerResponse {
    match result {
        LoadCommissionResult::Ok {
            id,
            intent,
            current_revision,
            current_ordinal,
            status,
            current,
            approvals,
            scope_verify,
            current_unreadable,
        } => {
            let Ok(intent) = digest_of_bytes(&intent) else {
                return error_response(500, "stored intent digest is not 32 bytes");
            };
            let current_revision = match current_revision {
                Some(bytes) => match digest_of_bytes(&bytes) {
                    Ok(digest) => Some(digest),
                    Err(response) => return response,
                },
                None => None,
            };
            let (current, current_unreadable) = show_current(current, current_unreadable);
            let mut decoded = Vec::new();
            for bytes in approvals {
                match from_bytes::<Statement>(&bytes) {
                    Ok(statement) => decoded.push(statement),
                    Err(_) => return error_response(500, "stored approval is malformed"),
                }
            }
            let scope_verify = match scope_verify {
                Some(bytes) => match ScopeVerifyReport::from_canonical(&bytes) {
                    Ok(report) => Some(report),
                    Err(_) => return error_response(500, "stored scope-verify report is malformed"),
                },
                None => None,
            };
            json(
                200,
                &CommissionShowView {
                    id: WorkpieceId(id),
                    intent,
                    current_revision,
                    current_ordinal,
                    status,
                    current,
                    current_unreadable,
                    approvals: decoded,
                    scope_verify,
                },
            )
        }
        LoadCommissionResult::Missing { id } => error_response(404, &format!("no commission named {id}")),
        LoadCommissionResult::Err { error } => error_response(500, &format!("commission load failed: {error}")),
    }
}

/// Render [`ListCommissionsResult`].
pub(super) fn list_response(result: ListCommissionsResult) -> HttpServerResponse {
    match result {
        ListCommissionsResult::Ok { commissions } => {
            let mut heads = Vec::new();
            for listed in commissions {
                match head_view(listed) {
                    Ok(head) => heads.push(head),
                    Err(response) => return response,
                }
            }
            json(200, &CommissionsView { commissions: heads })
        }
        ListCommissionsResult::Err { error } => error_response(500, &format!("commission list failed: {error}")),
    }
}

fn head_view(listed: ListedCommission) -> Result<CommissionHeadView, HttpServerResponse> {
    Ok(CommissionHeadView {
        id: WorkpieceId(listed.id),
        intent: digest_of_bytes(&listed.intent)?,
        current_revision: listed.current_revision.map(|bytes| digest_of_bytes(&bytes)).transpose()?,
        current_ordinal: listed.current_ordinal,
        status: listed.status,
    })
}

/// Render [`EnqueueScopeRunResult`].
pub(super) fn scope_run_response(result: EnqueueScopeRunResult) -> HttpServerResponse {
    match result {
        EnqueueScopeRunResult::Ok { id, ordinal, sequence, subject } => match digest_of_bytes(&subject) {
            Ok(subject) => json(201, &ScopeRunOpenedView { id: WorkpieceId(id), ordinal, sequence, subject }),
            Err(response) => response,
        },
        EnqueueScopeRunResult::Missing { id } => error_response(404, &format!("no commission named {id}")),
        EnqueueScopeRunResult::NotOpen => error_response(409, "commission is not open"),
        EnqueueScopeRunResult::AlreadyInFlight { ordinal } => {
            error_response(409, &format!("scoping run {ordinal} is already in flight"))
        }
        EnqueueScopeRunResult::AlreadyFrozen => error_response(409, "commission already has a frozen scope revision"),
        EnqueueScopeRunResult::Exhausted { attempts } => {
            error_response(409, &format!("scoping run retry budget spent ({attempts} attempts)"))
        }
        EnqueueScopeRunResult::Err { error } => error_response(500, &format!("scope-run enqueue failed: {error}")),
    }
}

/// Render [`CancelCommissionResult`].
pub(super) fn cancel_response(result: CancelCommissionResult) -> HttpServerResponse {
    match result {
        CancelCommissionResult::Ok { id, digest } => match digest_of_bytes(&digest) {
            Ok(digest) => {
                json(200, &CommissionCancelledView { digest, id: WorkpieceId(id), status: "cancelled".to_owned() })
            }
            Err(response) => response,
        },
        CancelCommissionResult::Missing { id } => error_response(404, &format!("no commission named {id}")),
        CancelCommissionResult::NotOpen => error_response(409, "commission is not open"),
        CancelCommissionResult::WrongSubject => error_response(400, "cancel words are not the intent digest"),
        CancelCommissionResult::Err { error } => error_response(500, &format!("commission cancel failed: {error}")),
    }
}

/// Render [`ReopenCommissionResult`].
pub(super) fn reopen_response(result: ReopenCommissionResult) -> HttpServerResponse {
    match result {
        ReopenCommissionResult::Ok { id, digest } => match digest_of_bytes(&digest) {
            Ok(digest) => json(200, &CommissionReopenedView { digest, id: WorkpieceId(id), status: "open".to_owned() }),
            Err(response) => response,
        },
        ReopenCommissionResult::Missing { id } => error_response(404, &format!("no commission named {id}")),
        ReopenCommissionResult::NotLanded { status } => {
            error_response(409, &format!("commission is {status}, not landed"))
        }
        ReopenCommissionResult::Resolved { bloom } => {
            error_response(409, &format!("bloom {bloom} resolved this workpiece; its landing stands"))
        }
        ReopenCommissionResult::WrongSubject => error_response(400, "reopen words are not the intent digest"),
        ReopenCommissionResult::Err { error } => error_response(500, &format!("commission reopen failed: {error}")),
    }
}
