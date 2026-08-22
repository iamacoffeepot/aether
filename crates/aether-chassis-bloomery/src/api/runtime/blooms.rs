//! The live-bloom routes — read the projection, supersede a bloom, adopt a
//! signed answer — and the renderers for the two control-core replies those
//! routes (and the seal routes next door) defer on. Every route here reaches
//! durable state through the control core, so each one defers.

use aether_actor::Manual;
use aether_bloomery::{
    Adjudication, Admit, AdmitResult, AuthorityDoor, BloomId, BloomView, CandidateRef, Disposition, Event, Fact,
    IdempotencyKey, OperatorHold, OperatorRepair, Outcome, Query, QueryResult, Statement, SuppressionDisposition,
    ViewDocument, WhyDocument, Withdrawal, WithdrawalCause, WorkpieceId, digest_of,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use serde::Serialize;

use super::hex::{self, digest_from_hex, hex_encode};
use super::response::{error_response, json};
use super::state::{ApiCapabilityState, Routed, VerifyPending, admit};
use crate::api::dto::{
    AdjudicateRequest, GrantRequest, HoldRequest, OutcomeView, ReleaseAcceptedView, RepairRequest, SupersedeRequest,
    SuppressionAnswerRequest, WithdrawRequest,
};
use crate::bloomery::DoctorReport;
use crate::control::ControlCore;
use crate::signing::{SigningCapability, Verify, VerifyResult, authority_bytes};

impl ApiCapabilityState {
    /// `POST /blooms/{id}/supersede` — seal the named successor draft and admit
    /// `Fact::Supersede` against the `{id}` predecessor bloom.
    ///
    /// Declared edges ride the same `resolve_seal_graph` path a first seal
    /// uses: they union with derived overlap-ordering edges and refuse a
    /// cycle or a non-member. An edgeless body is the drop-a-subtree case
    /// (ADR-0196): the reducer keeps the predecessor's remaining member
    /// graph, so adopted dependents stay based on their inherited ancestors
    /// rather than becoming roots.
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
        // Same store-backed door the first seal uses (#5048 / #4638). A
        // writable projection on this body would preserve the hole the
        // commission store closes.
        self.begin_store_seal(ctx, draft, Some(predecessor), request.idempotency_key, request.edges)
    }

    /// `POST /blooms/{id}/grant` — hand a wedged member more attempts on the
    /// `{id}` bloom and resume it (#4708).
    ///
    /// The counterpart to supersession, along the line the sealed `base` draws:
    /// a base that has not moved, with scope, membership, and configuration
    /// unchanged, is an execution decision and belongs here; anything else is a
    /// successor doing real work. Admitting it needs no approve gate — a grant
    /// seals nothing, claims nothing, and alters no field the members' approvals
    /// bind — so unlike the supersede route it admits straight through. `reason`
    /// and `operator` are required at this door, as on the other operator
    /// routes, and a reducer refusal answers `422`.
    pub(super) fn grant(id: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let request: GrantRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid grant body: {error}"))),
        };
        let GrantRequest { workpiece, stage, attempts, reason, operator, idempotency_key } = request;
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }
        let audit = OperatorHold { reason, operator };
        let key = idempotency_key.unwrap_or_else(|| {
            format!(
                "aether.bloomery.grant:{}:{}:{stage:?}:{attempts}:{}",
                hex_encode(bloom.0.as_bytes()),
                workpiece.0,
                hex_encode(digest_of(&audit).as_bytes())
            )
        });

        admit(&Event {
            idempotency_key: IdempotencyKey(key),
            fact: Fact::GrantAttempts { bloom, workpiece, stage, attempts },
        })
    }

    /// `POST /blooms/{id}/adjudicate` — close the composition findings the
    /// operator has read, with a stated reason, and let the bloom proceed
    /// (#4957).
    ///
    /// Journal-first, like `grant`: the route's only effect is appending
    /// `Fact::OperatorAdjudication`. Every state movement — closing the
    /// findings, releasing the park they raised, resolving the composition from
    /// the weave it holds, dispatching the land — is the reducer's, so an
    /// operator override replays exactly as it happened rather than as the
    /// current binary would re-decide it (ADR-0190).
    ///
    /// What the route decides for itself is only what it can see in the request:
    /// a body that says nothing. A blank reason, a blank operator, and a
    /// deferral naming no issue are `422` here rather than a round trip to the
    /// reducer, because each is a malformed override rather than a refused one —
    /// and the operator gets the same status either way, since a refused
    /// override renders `422` too (see [`admitted_response`]).
    pub(super) fn adjudicate(id: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let request: AdjudicateRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid adjudicate body: {error}"))),
        };
        let AdjudicateRequest { findings, disposition, reason, operator, idempotency_key } = request;
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }
        if disposition == (Disposition::Deferred { issue: 0 }) {
            return Routed::Reply(error_response(
                422,
                "a deferred adjudication must name the filed issue it defers to, so the finding does not vanish",
            ));
        }

        let adjudication = Adjudication { findings, disposition, reason, operator };
        let key = idempotency_key.unwrap_or_else(|| {
            format!(
                "aether.bloomery.adjudicate:{}:{}",
                hex_encode(bloom.0.as_bytes()),
                hex_encode(digest_of(&adjudication).as_bytes())
            )
        });

        admit(&Event { idempotency_key: IdempotencyKey(key), fact: Fact::OperatorAdjudication { bloom, adjudication } })
    }

    /// `POST /blooms/{id}/members/{workpiece}/suppression` — answer the
    /// suppression requests `{workpiece}`'s candidate is carrying (ADR-0193 §5).
    ///
    /// Journal-first like `adjudicate`: appending the fact is the route's whole
    /// effect, and every state movement a denial causes — the revoked claim, the
    /// `Refine` re-entry, the spent repair roll — is the reducer's, so a
    /// reviewer's answer replays exactly as it happened (ADR-0190).
    ///
    /// What the route decides for itself is only what it can see in the request:
    /// a body that says nothing. A blank reason and a blank operator are `422`
    /// here rather than a round trip, because each is a malformed answer rather
    /// than a refused one — and an empty request set is the same, since an answer
    /// that closes nothing has answered nothing.
    ///
    /// The reason travels on the fact rather than on the member's findings
    /// channel. That channel is written at intake, from what a lane's evidence
    /// said, and a route that wrote it directly would put a person's words in a
    /// place every reader treats as a gate's — with nothing to stop the next
    /// verify capture from overwriting them. The repair lap therefore reads the
    /// refusal from the journal, where the answer lives.
    pub(super) fn suppression(id: &str, workpiece: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let request: SuppressionAnswerRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid suppression body: {error}"))),
        };
        let SuppressionAnswerRequest { requests, verdict, reason, operator, idempotency_key } = request;
        if requests.is_empty() {
            return Routed::Reply(error_response(
                422,
                "a suppression answer must name the requests it closes; an answer that closes nothing is not one",
            ));
        }
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }

        let workpiece = WorkpieceId(workpiece.to_owned());
        let disposition = SuppressionDisposition { requests, verdict, reason, operator };
        let key = idempotency_key.unwrap_or_else(|| {
            format!(
                "aether.bloomery.suppression:{}:{}:{}",
                hex_encode(bloom.0.as_bytes()),
                workpiece.0,
                hex_encode(digest_of(&disposition).as_bytes())
            )
        });

        admit(&Event {
            idempotency_key: IdempotencyKey(key),
            fact: Fact::SuppressionDisposition { bloom, workpiece, disposition },
        })
    }

    /// `POST /blooms/{id}/members/{workpiece}/repair` — hand the wedged
    /// `{workpiece}` the candidate the operator pushed to its candidate ref, and
    /// let the ordinary gates judge it (#4957).
    ///
    /// The path names the workpiece for the reason the grant body does: the
    /// reducer refuses one that is not wedged, so a stale read cannot act. The
    /// reserved composition id is accepted here too — a composition whose weave
    /// repair wedged is repaired the same way a member is, by someone supplying
    /// the candidate its own lane could not.
    ///
    /// Journal-first and gate-preserving: the appended fact is the whole effect,
    /// and the reducer re-enters the workpiece at `Verify`, so the mechanical
    /// suite and the delta-confirm review still run over the operator's tree.
    /// Only the model lap is skipped.
    ///
    /// `from_commit` / `from_worktree` (#5032) run first, on the host: the
    /// chassis derives the pair, records correspondence, and pushes the
    /// candidate ref, then admits the same fact the low-level form does. A
    /// failure there is a `422` that names the precondition, so it never
    /// becomes a journaled repair of a candidate the verifying lane cannot see.
    pub(super) fn repair(&self, id: &str, workpiece: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let request: RepairRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid repair body: {error}"))),
        };
        let RepairRequest { candidate, from_commit, from_worktree, reason, operator, idempotency_key } = request;
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }
        let candidate = match resolve_repair_candidate(self, &bloom, workpiece, candidate, from_commit, from_worktree) {
            Ok(candidate) => candidate,
            Err(response) => return Routed::Reply(response),
        };

        let repair = OperatorRepair { workpiece: WorkpieceId(workpiece.to_owned()), candidate, reason, operator };
        let key = idempotency_key.unwrap_or_else(|| {
            format!(
                "aether.bloomery.repair:{}:{}",
                hex_encode(bloom.0.as_bytes()),
                hex_encode(digest_of(&repair).as_bytes())
            )
        });

        admit(&Event { idempotency_key: IdempotencyKey(key), fact: Fact::OperatorRepair { bloom, repair } })
    }

    /// `POST /blooms/{id}/hold` — freeze the `{id}` bloom's dispatch (#4976).
    ///
    /// The brake for a bloom that looks wrong but has not stopped. Journal-first
    /// like its siblings: the route appends `Fact::OperatorHold` and nothing
    /// else. From there the reducer emits no `DispatchAttempt`,
    /// `DispatchAggregateVerify`, or `DispatchAggregateReview` for the bloom,
    /// while every other fact — the laps already running, their verify verdicts,
    /// the fold outcomes — reduces and journals exactly as before. That is the
    /// difference between this and killing the coordinator, which strands what is
    /// in flight and re-runs it at the next boot.
    ///
    /// A hold on a held bloom is refused rather than absorbed: a second one would
    /// journal a fact that changed nothing and overwrite the reason the first
    /// recorded.
    pub(super) fn hold(id: &str, body: &[u8]) -> Routed {
        Self::brake(id, body, "hold", |bloom, hold| Fact::OperatorHold { bloom, hold })
    }

    /// `POST /blooms/{id}/release` — take the `{id}` bloom off the brake
    /// (#4976).
    ///
    /// The reducer clears the flag and re-derives what is due, dispatching each
    /// workpiece the hold owes from the cursor it is sitting at *now*, and each
    /// aggregate gate the hold owes from the fold the record is holding now.
    /// Nothing was stored when the hold went on, so a bloom that moved while it
    /// was held resumes where it actually is rather than where it was frozen.
    ///
    /// Releasing an unheld bloom is refused for the reason a second hold is: it
    /// clears nothing and dispatches nothing, and a `200` on it would read as
    /// proof the bloom is running.
    pub(super) fn release(id: &str, body: &[u8]) -> Routed {
        Self::brake(id, body, "release", |bloom, release| Fact::OperatorRelease { bloom, release })
    }

    /// The shared body of the two brake routes: parse, refuse a body that says
    /// nothing, and admit the fact `edge` builds.
    ///
    /// Written once because the two edges differ in exactly one expression. The
    /// route name is threaded into the default idempotency key so a hold and a
    /// release stating identical words stay distinct acts.
    fn brake(id: &str, body: &[u8], route: &str, edge: impl FnOnce(BloomId, OperatorHold) -> Fact) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let request: HoldRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid {route} body: {error}"))),
        };
        let HoldRequest { reason, operator, idempotency_key } = request;
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }

        let hold = OperatorHold { reason, operator };
        let key = idempotency_key.unwrap_or_else(|| {
            format!(
                "aether.bloomery.{route}:{}:{}",
                hex_encode(bloom.0.as_bytes()),
                hex_encode(digest_of(&hold).as_bytes())
            )
        });

        admit(&Event { idempotency_key: IdempotencyKey(key), fact: edge(bloom, hold) })
    }

    /// `POST /blooms/{id}/members/{workpiece}/withdraw` — take one member out
    /// of the `{id}` bloom without superseding it (#5327).
    ///
    /// Journal-first like its siblings: the route appends one `Fact::Withdraw`
    /// and nothing else. From there the reducer drops the member's cursor,
    /// skips it in the three folds that are otherwise total over the sealed
    /// member list, cancels its lane, and frees its claim ref alone — never the
    /// bloom's admission ref, because the bloom keeps walking.
    ///
    /// The cascade is opt-in and the refusal is fail-closed: a withdrawal that
    /// would strand dependents answers `422` naming them, because a parked
    /// dependent still pins the bloom this was meant to free. A cascaded
    /// dependent leaves with a derived `WithdrawalCause::Dependency`, which is
    /// its visible reason.
    ///
    /// One-way. A member wrongly withdrawn is re-scoped and sealed into a
    /// later bloom — which is exactly what freeing its claim ref makes
    /// possible.
    pub(super) fn withdraw(id: &str, workpiece: &str, body: &[u8]) -> Routed {
        let bloom = match digest_from_hex(id) {
            Some(digest) => BloomId(digest),
            None => return Routed::Reply(error_response(400, "bloom id is not a 32-byte hex bloom id")),
        };
        let request: WithdrawRequest = match hex::from_slice(body) {
            Ok(request) => request,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid withdraw body: {error}"))),
        };
        let WithdrawRequest { reason, operator, cascade, idempotency_key } = request;
        if let Some(refusal) = unstated(&reason, &operator) {
            return Routed::Reply(refusal);
        }

        let withdrawal = Withdrawal {
            workpiece: WorkpieceId(workpiece.to_owned()),
            cause: WithdrawalCause::Operator,
            reason,
            operator,
        };
        // Content-addressed like the brake's: a resent request is one act, and
        // a genuinely different reason is its own. The cascade flag rides the
        // key because withdrawing one member and withdrawing its whole subtree
        // are different acts stating identical words.
        let key = idempotency_key.unwrap_or_else(|| {
            format!(
                "aether.bloomery.withdraw:{}:{}:{}",
                hex_encode(bloom.0.as_bytes()),
                hex_encode(digest_of(&withdrawal).as_bytes()),
                u8::from(cascade)
            )
        });

        admit(&Event {
            idempotency_key: IdempotencyKey(key),
            fact: Fact::Withdraw { bloom, withdrawals: vec![withdrawal], cascade },
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
        Routed::Query(Query { bloom, release: None, calibration: false, why: false })
    }

    /// `GET /blooms/{id}` — read one bloom's live view by hex id.
    pub(super) fn query_bloom(id: &str) -> Routed {
        digest_from_hex(id).map_or_else(
            || Routed::Reply(error_response(400, "bloom id is not a 32-byte hex digest")),
            |digest| Self::query(Some(digest.as_bytes().to_vec())),
        )
    }

    /// `GET /blooms/{id}/why` — why the `{id}` bloom is not advancing (#5281).
    ///
    /// The same subject `query_bloom` names, rendered as the stored-fact chain
    /// instead of the projection: not landing because no integration is
    /// recorded, no integration because the fold refused, the fold refused
    /// because adoption found no candidate ref for this member.
    pub(super) fn query_why(id: &str) -> Routed {
        digest_from_hex(id).map_or_else(
            || Routed::Reply(error_response(400, "bloom id is not a 32-byte hex digest")),
            |digest| {
                Routed::Query(Query {
                    bloom: Some(digest.as_bytes().to_vec()),
                    release: None,
                    calibration: false,
                    why: true,
                })
            },
        )
    }
}

/// The one source a repair body is allowed to name.
#[derive(Debug)]
enum RepairSource {
    Candidate(CandidateRef),
    FromCommit(String),
    FromWorktree(String),
}

/// Pick exactly one candidate source off the request, or a `400` that names
/// the contract. Isolated so a body with two sources (or none) is refused
/// before any host-side mutation.
fn repair_source(
    candidate: Option<CandidateRef>,
    from_commit: Option<String>,
    from_worktree: Option<String>,
) -> Result<RepairSource, HttpServerResponse> {
    match (candidate, from_commit, from_worktree) {
        (Some(candidate), None, None) => Ok(RepairSource::Candidate(candidate)),
        (None, Some(commit), None) => Ok(RepairSource::FromCommit(commit)),
        (None, None, Some(path)) => Ok(RepairSource::FromWorktree(path)),
        _ => Err(error_response(400, "repair needs exactly one of `candidate`, `from_commit`, or `from_worktree`")),
    }
}

/// Pick the candidate the repair will admit: the operator-supplied pair, or
/// one derived from a reachable commit. Exactly one source is accepted.
fn resolve_repair_candidate(
    state: &ApiCapabilityState,
    bloom: &BloomId,
    workpiece: &str,
    candidate: Option<CandidateRef>,
    from_commit: Option<String>,
    from_worktree: Option<String>,
) -> Result<CandidateRef, HttpServerResponse> {
    match repair_source(candidate, from_commit, from_worktree)? {
        RepairSource::Candidate(candidate) => Ok(candidate),
        source => derive_repair_candidate(state, bloom, workpiece, &source),
    }
}

fn derive_repair_candidate(
    state: &ApiCapabilityState,
    bloom: &BloomId,
    workpiece: &str,
    source: &RepairSource,
) -> Result<CandidateRef, HttpServerResponse> {
    #[cfg(not(feature = "github"))]
    {
        let _ = (state, bloom, workpiece, source);
        Err(error_response(
            422,
            "this chassis cannot derive a candidate from a commit: the GitHub source runtime is not mounted",
        ))
    }

    #[cfg(feature = "github")]
    {
        use std::path::Path;

        use crate::bloomery::{CandidateSource, prepare_candidate};

        let (Some(correspondence), Some(pusher)) = (state.correspondence.as_ref(), state.pusher.as_ref()) else {
            return Err(error_response(
                422,
                "this chassis cannot derive a candidate from a commit: no correspondence store is mounted",
            ));
        };
        let prepared = match source {
            RepairSource::FromCommit(commit) => prepare_candidate(
                correspondence.as_ref(),
                pusher.as_ref(),
                bloom,
                workpiece,
                CandidateSource::Commit(commit),
                Path::new("."),
            ),
            RepairSource::FromWorktree(path) => prepare_candidate(
                correspondence.as_ref(),
                pusher.as_ref(),
                bloom,
                workpiece,
                CandidateSource::Worktree(Path::new(path)),
                Path::new("."),
            ),
            RepairSource::Candidate(_) => {
                return Err(error_response(500, "derive_repair_candidate was handed a pre-built candidate"));
            }
        };
        prepared.map_err(|error| error_response(422, &error.to_string()))
    }
}

/// The `422` a manager-override body earns by saying nothing (#4957), or `None`
/// when it states both.
///
/// Both override routes ask for the same two things and refuse an absent one the
/// same way, so the check lives once. Blank is refused rather than defaulted
/// because an override's whole product is its audit trail: a waiver with a
/// default reason and no named operator records that something was waived and
/// nothing about who or why.
fn unstated(reason: &str, operator: &str) -> Option<HttpServerResponse> {
    if reason.trim().is_empty() {
        return Some(error_response(422, "an override must state a reason; a blank one is refused, never defaulted"));
    }
    if operator.trim().is_empty() {
        return Some(error_response(422, "an override must name the operator making it"));
    }
    None
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
/// Every write route answers `200` with the outcome it produced, with two
/// exceptions.
///
/// An authorized orphan-claim release only *accepts* work: it is durably queued
/// for the release reactor rather than performed, so it answers `202` and hands
/// back the request digest `GET /claims/releases/{digest}` reads by (ADR-0179).
/// The digest rides the outcome itself, so the route holds nothing across the
/// admit to report it — the same reason `RecordConfigResult` carries its stored
/// bytes rather than the authoring route keeping a correlation map (ADR-0154
/// §3).
///
/// A **refused operator door** answers `422` — grant, adjudication, repair, and
/// the brake. Every other refusal this renders is the reducer declining a
/// request about the pipeline's own state, which an operator reads and re-aims;
/// an operator-door refusal is the reducer declining the operator's *authority*
/// — a finding that was never raised, a workpiece that is not stopped, a
/// membership that is not approved (ADR-0181). Answering those `200` would let a
/// script that only checks the status treat a refused waiver as an applied one.
/// The route's own synchronous refusals use the same status, so the operator
/// sees one answer for a refused override whichever side caught it.
fn admitted_response(outcome: Outcome) -> HttpServerResponse {
    match &outcome {
        Outcome::OrphanClaimReleaseRequested { request } => {
            json(202, &ReleaseAcceptedView { request: hex_encode(request.as_bytes()), outcome })
        }
        refused if refused.is_refused_override() => json(422, &OutcomeView { outcome }),
        _ => json(200, &OutcomeView { outcome }),
    }
}

/// Render a live-read route's [`QueryResult`] into its HTTP response: the whole
/// view document (with the doctor's latest report overlaid when one has run),
/// one bloom view, a `404`, or the error.
pub(super) fn query_response(result: QueryResult, doctor: Option<&DoctorReport>) -> HttpServerResponse {
    match result {
        QueryResult::Document { document } => match from_bytes::<ViewDocument>(&document) {
            Ok(document) => json(200, &ViewWithDoctor { document: &document, doctor }),
            Err(error) => error_response(500, &format!("view document decode failed: {error}")),
        },
        QueryResult::Bloom { view } => match from_bytes::<BloomView>(&view) {
            Ok(view) => json(200, &view),
            Err(error) => error_response(500, &format!("bloom view decode failed: {error}")),
        },
        QueryResult::NotFound => error_response(404, "no bloom with that id"),
        QueryResult::Err { error } => error_response(500, &error),
        // The shared `#[http::reply]` sends both release variants to
        // `release_status_response`, and a calibration reply to its own renderer,
        // before any of them reaches here — so one arriving is a routing bug
        // rather than an answer to render.
        QueryResult::Release { .. } | QueryResult::ReleaseNotFound => {
            error_response(500, "projection read answered with a release record")
        }
        QueryResult::Calibration { .. } => error_response(500, "projection read answered with a calibration document"),
        QueryResult::Why { document } => match from_bytes::<WhyDocument>(&document) {
            Ok(document) => json(200, &document),
            Err(error) => error_response(500, &format!("why document decode failed: {error}")),
        },
    }
}

/// `GET /view` is the journal projection plus the doctor's latest pass. The
/// doctor is not a [`ViewDocument`] field: that document is wire-encoded in
/// the outbox, and a trailing optional there would break queued payloads.
#[derive(Serialize)]
struct ViewWithDoctor<'a> {
    #[serde(flatten)]
    document: &'a ViewDocument,
    #[serde(skip_serializing_if = "Option::is_none")]
    doctor: Option<&'a DoctorReport>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{
        AdjudicationError, BloomId, Digest, Event, Fact, GrantAttemptsError, MemberDependency, OperatorHoldError,
        OperatorRepairError, Outcome, QueryResult, SpendQuiesce, ViewDocument, WorkpieceId,
    };
    use aether_data::wire::{from_bytes, to_vec};

    use super::{
        ApiCapabilityState, RepairSource, Routed, SupersedeRequest, admitted_response, hex, query_response,
        repair_source,
    };
    use crate::api::dto::RepairRequest;
    use crate::bloomery::{CheckResult, DoctorReport};

    /// A bloom id in the spelling the routes take.
    const BLOOM: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    // The plausible bug: GET /view / GET /blooms drop the snapshot marker, so
    // an operator cannot see which axis closed the door or by how much.
    #[test]
    fn query_response_renders_the_spend_quiesce_marker() {
        let document = ViewDocument {
            mainline: Digest::from_bytes([1; 32]),
            observed: Digest::from_bytes([2; 32]),
            spend_quiesce: Some(SpendQuiesce::Window {
                window: "bloomery/daily/2026-08-14".into(),
                spent_micro_usd: 12,
                ceiling_micro_usd: 10,
            }),
            blooms: Vec::new(),
            base_alert: None,
        };
        let result = QueryResult::Document { document: to_vec(&document).unwrap() };
        let response = query_response(result, None);
        assert_eq!(response.status, 200);
        let body = String::from_utf8(response.body).unwrap();
        assert!(body.contains("Window"), "the axis is named: {body}");
        assert!(body.contains("bloomery/daily/2026-08-14"), "the window is named: {body}");
        assert!(body.contains("12") && body.contains("10"), "spend and ceiling are named: {body}");
        assert!(body.contains("spent_micro_usd") && body.contains("ceiling_micro_usd"), "the fields are named: {body}");
    }

    // The plausible bug: GET /view renders the journal projection and drops
    // the doctor, so a Landed claim-ref violation is only a log line.
    #[test]
    fn query_response_overlays_doctor_violations() {
        let document = ViewDocument {
            mainline: Digest::from_bytes([1; 32]),
            observed: Digest::from_bytes([2; 32]),
            spend_quiesce: None,
            blooms: Vec::new(),
            base_alert: None,
        };
        let report = DoctorReport {
            checks: vec![CheckResult {
                name: "claim_refs_name_active_blooms".into(),
                statement: "every ref under refs/bloomery/claims/ names a bloom currently Sealed or Resolved — never Landed, never unknown".into(),
                passed: false,
                divergences: vec!["refs/bloomery/claims/issue-5175 held by Landed bloom ab".into()],
            }],
        };
        let result = QueryResult::Document { document: to_vec(&document).unwrap() };
        let response = query_response(result, Some(&report));
        assert_eq!(response.status, 200);
        let body = String::from_utf8(response.body).unwrap();
        assert!(body.contains("claim_refs_name_active_blooms"), "the invariant is named: {body}");
        assert!(body.contains("refs/bloomery/claims/issue-5175"), "the claim ref is named: {body}");
        assert!(body.contains("Landed"), "the holder status is named: {body}");
    }
    /// A finding digest in the same spelling.
    const FINDING: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// The status one route helper answered with, or `None` when it relayed
    /// instead of replying — a relayed request reached the reducer, which is
    /// exactly what "the route did not refuse this" means here.
    fn status(routed: &Routed) -> Option<u16> {
        match routed {
            Routed::Reply(response) => Some(response.status),
            _ => None,
        }
    }

    // Tripwire: a grant that states no reason is refused at the door, the same
    // way the other operator doors are. The grant's audit fields are why the
    // extra attempts were bought; a blank one records the extra spend and
    // nothing about who or why.
    #[test]
    fn a_grant_body_without_a_reason_or_an_operator_is_refused() {
        let grant = |reason: &str, operator: &str| {
            let body = serde_json::json!({
                "workpiece": "alpha",
                "stage": "Construct",
                "attempts": 1,
                "reason": reason,
                "operator": operator,
            })
            .to_string();
            status(&ApiCapabilityState::grant(BLOOM, body.as_bytes()))
        };

        assert_eq!(grant("  ", "eve"), Some(422), "a whitespace reason says nothing");
        assert_eq!(grant("sandbox recovered", ""), Some(422), "and a grant has to name who asked");
        assert_eq!(grant("sandbox recovered", "eve"), None, "a stated grant relays to the reducer");
    }

    // Tripwire (#4957): an override that states no reason is refused, not
    // defaulted. The reason is what the landing proposal quotes as the grounds
    // for a waiver, so a body without one would land a bloom whose merged
    // history records that something was overridden and nothing about why —
    // which is the failure this route exists to prevent, not a nicety.
    #[test]
    fn an_override_body_without_a_reason_is_refused() {
        let adjudicate = |reason: &str, operator: &str| {
            let body = serde_json::json!({
                "findings": [FINDING],
                "disposition": "Accepted",
                "reason": reason,
                "operator": operator,
            })
            .to_string();
            status(&ApiCapabilityState::adjudicate(BLOOM, body.as_bytes()))
        };

        assert_eq!(adjudicate("  ", "eve"), Some(422), "a whitespace reason says nothing");
        assert_eq!(adjudicate("read it", ""), Some(422), "and an override has to name who made it");
        assert_eq!(adjudicate("read it", "eve"), None, "a stated override relays to the reducer");
    }

    // Tripwire (#4957): a deferral that names no filed issue is refused at the
    // door. `Deferred` exists so a waived finding cannot silently vanish, and
    // issue `0` is no issue — accepting it would make the disposition
    // decorative.
    #[test]
    fn a_deferral_naming_no_issue_is_refused_at_the_door() {
        let deferred = |issue: u64| {
            let body = serde_json::json!({
                "findings": [FINDING],
                "disposition": { "Deferred": { "issue": issue } },
                "reason": "filed forward",
                "operator": "eve",
            })
            .to_string();
            status(&ApiCapabilityState::adjudicate(BLOOM, body.as_bytes()))
        };

        assert_eq!(deferred(0), Some(422), "issue 0 is no issue");
        assert_eq!(deferred(4957), None, "a named issue relays to the reducer");
    }

    // Tripwire (#4957 / ADR-0181): a refused override answers `4xx`, not the
    // `200` every other write route answers its outcome with. Answering `200`
    // would let a script that checks only the status treat a refused waiver as
    // an applied one — and the refusal this most matters for is the approval
    // one, where the mistake lands work nobody approved.
    #[test]
    fn a_refused_override_answers_422() {
        let workpiece = WorkpieceId("alpha".to_owned());

        for outcome in [
            Outcome::GrantAttemptsRejected(GrantAttemptsError::NotWedged(workpiece.clone())),
            Outcome::AdjudicationRejected(AdjudicationError::UnapprovedMember(workpiece.clone())),
            Outcome::OperatorRepairRejected(OperatorRepairError::UnapprovedMember(workpiece)),
            Outcome::AdjudicationRejected(AdjudicationError::UnknownFinding(Digest::from_bytes([2; 32]))),
        ] {
            assert_eq!(admitted_response(outcome.clone()).status, 422, "{outcome:?} is a refused override");
        }

        // And an ordinary admitted outcome is untouched: the `422` is scoped to
        // the override doors rather than a new blanket rule for every refusal
        // the reducer can return.
        assert_eq!(admitted_response(Outcome::Duplicate).status, 200);
    }

    // Tripwire (#4976): the brake routes state a reason and an operator or they
    // are refused at the door, exactly as the other override routes are. A hold
    // is an act no verdict produced, so a frozen bloom whose record says only
    // that somebody stopped it is the failure the field exists to prevent — and
    // one that says nothing about who stopped it is no better.
    #[test]
    fn a_brake_body_without_a_reason_or_an_operator_is_refused() {
        let brake = |route: fn(&str, &[u8]) -> Routed, reason: &str, operator: &str| {
            let body = serde_json::json!({ "reason": reason, "operator": operator }).to_string();
            status(&route(BLOOM, body.as_bytes()))
        };

        for (label, route) in [
            ("hold", ApiCapabilityState::hold as fn(&str, &[u8]) -> Routed),
            ("release", ApiCapabilityState::release as fn(&str, &[u8]) -> Routed),
        ] {
            assert_eq!(brake(route, "   ", "eve"), Some(422), "{label}: a whitespace reason says nothing");
            assert_eq!(brake(route, "wave-1 is stalled", ""), Some(422), "{label}: and it has to name who asked");
            assert_eq!(brake(route, "wave-1 is stalled", "eve"), None, "{label}: a stated brake relays to the reducer");
        }
    }

    // Tripwire (#4976): the two routes admit *different* facts, and a hold and a
    // release stating identical words admit under different idempotency keys. A
    // shared body type is what makes both mistakes possible — one copy-paste in
    // the edge closure and `POST /release` would journal a second hold, or the
    // content-derived default key would collapse a release onto the hold it
    // undoes and discard it as a duplicate.
    #[test]
    fn hold_and_release_admit_distinct_facts_under_distinct_keys() {
        let body = br#"{"reason":"wave-1 is stalled","operator":"eve"}"#;
        let admitted = |routed: Routed| match routed {
            Routed::Admit(admit) => from_bytes::<Event>(&admit.event).expect("the route encodes"),
            _ => panic!("a stated brake body admits rather than replying"),
        };

        let held = admitted(ApiCapabilityState::hold(BLOOM, body));
        let let_go = admitted(ApiCapabilityState::release(BLOOM, body));
        let bloom = BloomId(Digest::from_bytes([0x11; 32]));

        assert!(
            matches!(&held.fact, Fact::OperatorHold { bloom: id, hold }
                if *id == bloom && hold.reason == "wave-1 is stalled" && hold.operator == "eve"),
            "got {:?}",
            held.fact,
        );
        assert!(
            matches!(&let_go.fact, Fact::OperatorRelease { bloom: id, release }
                if *id == bloom && release.reason == "wave-1 is stalled" && release.operator == "eve"),
            "got {:?}",
            let_go.fact,
        );
        assert_ne!(held.idempotency_key, let_go.idempotency_key, "the same words on the two edges are two acts");
    }

    // Tripwire (#4976 / #4957): a refused brake answers `4xx` like every other
    // refused override. `AlreadyHeld` and `NotHeld` are the ones that matter — a
    // script that reads only the status would otherwise treat "this bloom was
    // never frozen" as "this bloom is now running".
    #[test]
    fn a_refused_brake_answers_422() {
        for error in [OperatorHoldError::AlreadyHeld, OperatorHoldError::NotHeld, OperatorHoldError::BlankReason] {
            let outcome = Outcome::OperatorHoldRejected(error);
            assert_eq!(admitted_response(outcome.clone()).status, 422, "{outcome:?} is a refused override");
        }
    }

    #[test]
    fn a_supersede_body_ignores_caller_projections_and_descriptions() {
        // #5048: the same cut as `SealRequest`. A successor body that still
        // carries the retired fields must parse, and those fields must not
        // remain a writable override.
        let body = br#"{"successor_draft":"1","projections":[],"descriptions":{"wp-a":"override"}}"#;

        let parsed: SupersedeRequest = serde_json::from_slice(body).expect("legacy fields are ignored");

        assert_eq!(parsed.successor_draft, "1");
        assert!(parsed.edges.is_empty());
    }

    #[test]
    fn a_supersede_body_without_edges_still_parses() {
        // Tripwire (#5115): edges were added to this body after the route
        // shipped, so every existing caller omits them. Making the field
        // required would turn each of those into a `400` on the one route an
        // operator reaches for when a bloom has already failed to land.
        let body = br#"{"successor_draft":"1"}"#;

        let parsed: SupersedeRequest = serde_json::from_slice(body).expect("a body predating edges parses");

        assert!(parsed.edges.is_empty(), "an absent list defaults empty rather than erroring");
    }

    #[test]
    fn a_supersede_body_carries_declared_edges() {
        let body = br#"{"successor_draft":"1","edges":[{"member":"issue-B","depends_on":"issue-A"}]}"#;

        let parsed: SupersedeRequest = serde_json::from_slice(body).unwrap();

        assert_eq!(
            parsed.edges,
            [MemberDependency {
                member: WorkpieceId("issue-B".to_owned()),
                depends_on: WorkpieceId("issue-A".to_owned())
            }]
        );
    }

    fn candidate_pair() -> aether_bloomery::CandidateRef {
        aether_bloomery::CandidateRef { tree: Digest::from_bytes([0xaa; 32]), checkout: Digest::from_bytes([0xbb; 32]) }
    }

    #[test]
    fn a_repair_body_names_exactly_one_candidate_source() {
        // Tripwire (#5032): a body that names two sources (or none) must not
        // silently prefer one. The low-level pair and from-commit derive
        // different host-side effects; picking the wrong one would skip the
        // push or overwrite a hand-built pair.
        let pair = candidate_pair();
        assert!(matches!(repair_source(Some(pair), None, None), Ok(RepairSource::Candidate(_))));
        assert!(matches!(repair_source(None, Some("abc".into()), None), Ok(RepairSource::FromCommit(_))));
        assert!(matches!(repair_source(None, None, Some("/tmp/wt".into())), Ok(RepairSource::FromWorktree(_))));

        for (candidate, commit, worktree, label) in [
            (None, None, None, "none"),
            (Some(pair), Some("abc".into()), None, "candidate+commit"),
            (Some(pair), None, Some("/tmp/wt".into()), "candidate+worktree"),
            (None, Some("abc".into()), Some("/tmp/wt".into()), "commit+worktree"),
            (Some(pair), Some("abc".into()), Some("/tmp/wt".into()), "all three"),
        ] {
            let refusal = repair_source(candidate, commit, worktree).expect_err(label);
            assert_eq!(refusal.status, 400, "{label} must be a 400, not a silent pick");
        }
    }

    #[test]
    fn a_legacy_repair_body_still_parses_as_the_candidate_source() {
        // Tripwire: `candidate` became optional so from_commit can land, but
        // every existing caller still sends the pair. Making the field vanish
        // would turn those into `400 invalid repair body`.
        let body = serde_json::json!({
            "candidate": {
                "tree": "aa".repeat(32),
                "checkout": "bb".repeat(32),
            },
            "reason": "hand-built",
            "operator": "eve",
        })
        .to_string();
        let parsed: RepairRequest = hex::from_slice(body.as_bytes()).expect("a pre-from-commit body still parses");
        assert!(parsed.from_commit.is_none());
        assert!(parsed.from_worktree.is_none());
        assert_eq!(parsed.candidate, Some(candidate_pair()));
    }
}
