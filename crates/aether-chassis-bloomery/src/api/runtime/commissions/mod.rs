//! Authenticated commission authoring and query routes (ADR-0199 slice 2).
//!
//! Every route here requires `Authorization: Bearer` against the configured
//! control token. The coordinator never signs and never accepts private-key
//! bytes: create / revision / show / list talk to the store; approval and
//! cancel verify a submitted envelope through `aether.signing` before the
//! store write.

use aether_actor::Manual;
use aether_bloomery::{AuthorityDoor, Digest, Provenance, ScopeRevision, ScopeVerifyReport, Statement, WorkpieceId};
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
use crate::bloomery::{StatementRejected, precheck_statement, verified_statement_approval};
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
pub(super) fn authorize(request: &HttpServerRequest, token: &str) -> Result<(), HttpServerResponse> {
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
            &Verify { statement: encoded, authority: authority_bytes(AuthorityDoor::Approve, scope) },
        );
        Routed::DeferredCommissionVerify {
            correlation,
            write: CommissionWrite::Approval { id: WorkpieceId(id.to_owned()), statement },
        }
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
            &Verify { statement: encoded, authority: authority_bytes(AuthorityDoor::Cancel, intent) },
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
            &Verify { statement: encoded, authority: authority_bytes(AuthorityDoor::Reopen, intent) },
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
        }
    }

    /// Answer a held commission write from the store's reply.
    pub(super) fn answer_commission_write(&mut self, ctx: &NativeCtx<'_, Manual>, response: &HttpServerResponse) {
        if let Some(inbound) = self.commission_writing.remove(&ctx.reply_target().correlation_id) {
            inbound.reply(response);
        }
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
            let current = match current {
                Some(bytes) => match ScopeRevision::from_canonical(&bytes) {
                    Ok(revision) => Some(revision),
                    Err(_) => return error_response(500, "stored current revision is malformed"),
                },
                None => None,
            };
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
