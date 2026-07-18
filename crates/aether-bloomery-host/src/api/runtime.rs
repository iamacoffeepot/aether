//! The `BloomeryApiCapability` REST control router (ADR-0149 §Packaging, issue
//! #3498).
//!
//! A native `aether.http.server`-mounted router that lets an operator drive a
//! bloom end-to-end from `curl`: stage workpieces, shape and seal drafts,
//! supersede, and read the live blooms / view document / journal / artifacts —
//! no typed-mail RPC vocabulary required. It claims a small set of path
//! prefixes on the `aether.http.server` ingress cap (ADR-0108/ADR-0130) and
//! dispatches every request through one manual handler that switches on method
//! + path.
//!
//! # The two route shapes
//!
//! Pre-seal shaping is pure in-memory state (ADR-0149 §The bloom: drafts
//! "claim nothing"), so those routes reply synchronously. The routes that read
//! or write durable state forward a mail to a peer cap and reply only on its
//! reply — the control-core actor (`aether.bloomery.admit` / `aether.bloomery.query`),
//! the store (`aether.store.replay_journal`), or the artifacts cap
//! (`aether.artifacts.get`). An HTTP handler cannot block on a mail reply, so
//! the deferral rides the request→dispatch→reply correlation the RPC and HTTP
//! server caps use:
//!
//! - [`take_inbound`](NativeCtx::take_inbound) moves the request's reply
//!   obligation into a guard that keeps its causal chain open across the async
//!   boundary — without it the chain settles when the handler returns and the
//!   HTTP server's own `502` safety net fires.
//! - The downstream mail is dispatched as a fresh root via
//!   [`send_envelope_detached`](NativeCtx::send_envelope_detached); the returned
//!   [`MailId`]'s `correlation_id` keys the held guard in `pending`.
//! - The downstream reply's `sender.correlation_id` is auto-echoed to that same
//!   id (ADR-0042), so a **typed** reply handler recovers the guard via
//!   `ctx.reply_target().correlation_id` and answers through it. The reply
//!   correlation deliberately does *not* go through a `#[fallback]`: a fallback
//!   would widen the actor's accept-set to every kind — including the request-
//!   stream kinds — and the HTTP server would then route each request down the
//!   streaming path instead of delivering a buffered [`HttpServerRequest`].
//! - A settlement subscription answers `504` for a request whose downstream
//!   chain settles without ever replying (a dropped or unloaded control core).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

use aether_actor::{Manual, runtime};
use aether_bloomery::{
    Admit, AdmitResult, BloomDraft, BloomId, BloomView, Digest, Event, Fact, IdempotencyKey, Outcome, Query,
    QueryResult, ReplayJournal, ReplayJournalResult, Statement, ViewDocument, Workpiece, digest_of,
};
use aether_capabilities::http::{
    HttpHeader, HttpMethod, HttpServerCapability, HttpServerRequest, HttpServerResponse, RegisterRouteSelf,
};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailboxId, mailbox_id_from_path};
use aether_kinds::trace::Settled;
use aether_substrate::{InboundMail, Mailer};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

use super::BloomeryApiCapability;
use super::dto::{
    DraftPatch, DraftView, DraftsView, ErrorView, JournalEntry, JournalView, OutcomeView, SealRequest,
    SupersedeRequest, WorkpiecesView,
};
use crate::artifacts::{ArtifactsError, Get, GetResult};
use crate::bloomery::{AdmissionRequest, ApprovalPolicy, Decision, Gate};
use crate::signing::{Verify, VerifyResult};
use crate::store::{RecordDispatchDescription, RecordDispatchDescriptionResult};

/// The runtime name of the control-core wasm component's mailbox — the
/// ADR-0099 lineage address the component host registers the autoloaded actor
/// at (`aether.component/aether.embedded:<NAMESPACE>`), resolved with
/// `mailbox_id_from_path` rather than the depth-1 `mailbox_id_from_name`.
const CONTROL_CORE_PATH: &str = "aether.component/aether.embedded:aether.bloomery.control";

/// The path prefixes the router claims on the HTTP ingress cap; every one
/// dispatches to [`BloomeryApiCapability::on_request`] under the
/// [`HttpServerRequest`] kind, which switches on method + remaining path.
const ROUTE_PREFIXES: [&str; 6] = ["/workpieces", "/drafts", "/blooms", "/view", "/journal", "/artifacts"];

/// Per-process ceilings on the pre-seal shaping maps. Staged workpieces and
/// open drafts are pure in-memory shaping state with no durable owner to evict
/// them, so the router caps each map and rejects growth past the cap rather
/// than letting an operator (or a runaway client) grow it without bound
/// (CLAUDE.md §Runtime: error rather than grow unboundedly). Capacity frees
/// only when the shaping session restarts.
const MAX_STAGED_WORKPIECES: usize = 1024;
const MAX_OPEN_DRAFTS: usize = 1024;

/// Boot config for the REST control api cap: the tier-policy file the pre-seal
/// approve gate loads at init (issue #3583). Threaded from the shared GitHub
/// config's `approval_policy_file` at chassis build so one Bloomery configuration
/// serves every reader. A policy that fails to load leaves the cap with no
/// policy, so the gate fails **closed** — every member resolves `human` and its
/// seal is refused, never silently `auto`.
pub struct ApiConfig {
    /// Repository-relative path to the Bloomery-owned tier policy
    /// (`bloomery/approval-policy.yml`).
    pub approval_policy_file: String,
}

/// The control-plane REST router state: the pre-seal shaping maps plus the
/// in-flight reply-correlation table.
pub struct ApiCapabilityState {
    /// This cap's own mailbox, the settlement-notice target.
    self_mailbox: MailboxId,
    /// The parsed tier policy the pre-seal approve gate decides over (issue
    /// #3583). `None` when the policy file was unreadable or malformed at init —
    /// the gate then fails closed (no member resolves `auto`).
    policy: Option<ApprovalPolicy>,
    /// Cached mailer for `send_envelope_detached` settlement subscriptions.
    mailer: Arc<Mailer>,
    /// Staged workpieces, keyed by their workpiece id.
    staged: BTreeMap<String, Workpiece>,
    /// Open drafts, keyed by a monotonic per-process handle.
    drafts: BTreeMap<u64, BloomDraft>,
    /// The next draft handle to mint.
    next_draft: u64,
    /// Deferred HTTP replies awaiting a downstream cap reply, keyed by the
    /// downstream dispatch's `MailId.correlation_id`.
    pending: HashMap<u64, InboundMail>,
    /// Answer requests awaiting a signature verification from the
    /// `aether.signing` capability, keyed by the verify dispatch's
    /// `MailId.correlation_id`. On a verified reply the held request admits its
    /// stashed `Fact::AdoptAnswer` event (re-deferring into `pending` on the
    /// reducer reply); on a rejection it answers `400`.
    verifying: HashMap<u64, VerifyPending>,
}

/// An answer request held across the signature-verification round trip: the
/// reply obligation and the adoption event to admit once (and only if) the
/// signature verifies.
struct VerifyPending {
    /// The held HTTP reply obligation.
    inbound: InboundMail,
    /// The `Fact::AdoptAnswer` event to admit on a verified signature.
    event: Event,
}

/// A route's disposition: an immediate response (the in-memory shaping routes),
/// a deferral keyed by the downstream dispatch correlation (the durable
/// read/write routes), or a signature-gated deferral (the answer route).
enum Routed {
    /// Reply now with this response.
    Reply(HttpServerResponse),
    /// Await the downstream reply correlated by this id.
    Deferred(u64),
    /// Await the `aether.signing` verify reply correlated by this id, then admit
    /// `event` on a verified signature or answer `400` on a rejection.
    DeferredVerify {
        /// The verify dispatch correlation the reply will echo.
        correlation: u64,
        /// The adoption event to admit once the signature verifies.
        event: Box<Event>,
    },
}

#[runtime]
impl NativeActor for BloomeryApiCapability {
    type State = ApiCapabilityState;
    type Config = ApiConfig;
    const NAMESPACE: &'static str = "aether.bloomery.api";

    fn init(config: ApiConfig, ctx: &mut NativeInitCtx<'_>) -> Result<ApiCapabilityState, BootError> {
        // Load the tier policy once at init. An unreadable or malformed policy is
        // not a boot failure — it leaves the cap policy-less, and the pre-seal
        // gate then fails closed (every member resolves `human`, its seal is
        // refused), which is the security-required posture: never silently `auto`.
        let policy = match ApprovalPolicy::load(Path::new(&config.approval_policy_file)) {
            Ok(policy) => Some(policy),
            Err(error) => {
                tracing::warn!(
                    target: "aether_bloomery_host::api",
                    path = %config.approval_policy_file,
                    ?error,
                    "approval policy unavailable; the pre-seal approve gate fails closed (no auto tier)"
                );
                None
            }
        };
        tracing::info!(
            target: "aether_bloomery_host::api",
            policy_loaded = policy.is_some(),
            "bloomery REST control api mounted"
        );
        Ok(ApiCapabilityState {
            self_mailbox: ctx.self_id(),
            policy,
            mailer: ctx.mailer(),
            staged: BTreeMap::new(),
            drafts: BTreeMap::new(),
            next_draft: 1,
            pending: HashMap::new(),
            verifying: HashMap::new(),
        })
    }

    /// Claim the route prefixes on the HTTP ingress cap (ADR-0130). Runs
    /// post-init, where mail is allowed; each claim routes its prefix to this
    /// actor under the [`HttpServerRequest`] kind.
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        for prefix in ROUTE_PREFIXES {
            ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
                prefix: prefix.to_owned(),
                method: None,
                kind: <HttpServerRequest as Kind>::ID,
                shared: false,
            });
        }
    }

    /// The single ingress: take the request's reply obligation (keeping its
    /// chain open across any deferral), route it, and either answer now or
    /// stash the guard against the downstream correlation.
    #[handler::manual]
    fn on_request(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: HttpServerRequest) {
        let inbound = ctx.take_inbound();
        match state.route(ctx, mail) {
            Routed::Reply(response) => {
                inbound.reply(&response);
            }
            Routed::Deferred(correlation) => {
                state.pending.insert(correlation, inbound);
            }
            Routed::DeferredVerify { correlation, event } => {
                state.verifying.insert(correlation, VerifyPending { inbound, event: *event });
            }
        }
    }

    /// The control core's reply to a `Fact::Seal` / `Fact::Supersede` admit —
    /// the reducer outcome, or an admit error.
    #[handler::manual]
    fn on_admit_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: AdmitResult) {
        state.answer(ctx, &admit_response(mail));
    }

    /// The control core's reply to a live projection read.
    #[handler::manual]
    fn on_query_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: QueryResult) {
        state.answer(ctx, &query_response(mail));
    }

    /// The store's reply to a journal read.
    #[handler::manual]
    fn on_replay_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: ReplayJournalResult) {
        state.answer(ctx, &journal_response(mail));
    }

    /// The artifacts cap's reply to an artifact fetch.
    #[handler::manual]
    fn on_get_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: GetResult) {
        state.answer(ctx, &artifact_response(mail));
    }

    /// The `aether.signing` capability's reply to an answer-gate verify. A
    /// verified signature admits the held adoption event (re-deferring on the
    /// reducer reply the same way seal / supersede do); a rejection or a decode
    /// error is a `400` — the fake always-valid provider is gone from the gate.
    #[handler::manual]
    fn on_verify_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: VerifyResult) {
        state.resolve_verify(ctx, mail);
    }

    /// The store's reply to a fire-and-forget dispatch-description write (#3595).
    /// The seal already replied to the operator from the admit outcome, so this
    /// reply carries no obligation — absorb it (logging a failed write) rather
    /// than letting it warn-drop as an unhandled kind.
    #[handler::manual]
    fn on_record_description_result(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_, Manual>,
        mail: RecordDispatchDescriptionResult,
    ) {
        if let RecordDispatchDescriptionResult::Err { error } = mail {
            tracing::warn!(target: "aether_bloomery_host::api", %error, "dispatch-description write failed at seal");
        }
    }

    /// A downstream chain settled. If its request is still pending, the
    /// downstream produced no reply (a dropped or unloaded control core, or a
    /// dropped signing capability) — answer `504` rather than leave the client
    /// hung.
    #[handler::manual]
    fn on_settled(state: &mut Self::State, _ctx: &mut NativeCtx<'_, Manual>, mail: Settled) {
        if let Some(inbound) = state.pending.remove(&mail.root.correlation_id) {
            inbound.reply(&error_response(504, "control-plane request settled without a reply"));
        } else if let Some(VerifyPending { inbound, .. }) = state.verifying.remove(&mail.root.correlation_id) {
            inbound.reply(&error_response(504, "signature verification settled without a reply"));
        }
    }
}

impl ApiCapabilityState {
    /// Route one request. Pure-shaping routes reply inline; durable
    /// read/write routes dispatch downstream and defer.
    fn route(&mut self, ctx: &NativeCtx<'_, Manual>, req: HttpServerRequest) -> Routed {
        // Destructure so the body is owned into the route arms rather than
        // borrowed from a by-value param the handler ABI hands us.
        let HttpServerRequest { method, path, body, .. } = req;
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        match (method, segments.as_slice()) {
            (HttpMethod::Post, ["workpieces"]) => self.stage_workpiece(&body),
            (HttpMethod::Get, ["workpieces"]) => {
                Routed::Reply(json(200, &WorkpiecesView { workpieces: self.staged.values().cloned().collect() }))
            }
            (HttpMethod::Post, ["drafts"]) => self.open_draft(),
            (HttpMethod::Get, ["drafts"]) => Routed::Reply(json(200, &self.drafts_view())),
            (HttpMethod::Get, ["drafts", id]) => self.get_draft(id),
            (HttpMethod::Patch, ["drafts", id]) => self.patch_draft(id, &body),
            (HttpMethod::Post, ["drafts", id, "seal"]) => self.seal_draft(ctx, id, &body),
            (HttpMethod::Get, ["blooms" | "view"]) => self.query(ctx, None),
            (HttpMethod::Get, ["blooms", id]) => self.query_bloom(ctx, id),
            (HttpMethod::Post, ["blooms", id, "supersede"]) => self.supersede(ctx, id, &body),
            (HttpMethod::Post, ["blooms", id, "answer"]) => self.answer_bloom(ctx, id, &body),
            (HttpMethod::Get, ["journal"]) => Routed::Deferred(self.send_tracked(ctx, store_mailbox(), &ReplayJournal)),
            (HttpMethod::Get, ["artifacts", digest]) => {
                Routed::Deferred(self.send_tracked(ctx, artifacts_mailbox(), &Get { digest: (*digest).to_owned() }))
            }
            _ => Routed::Reply(error_response(404, "no such route")),
        }
    }

    /// `POST /workpieces` — stage a workpiece for later draft membership.
    fn stage_workpiece(&mut self, body: &[u8]) -> Routed {
        let workpiece: Workpiece = match serde_json::from_slice(body) {
            Ok(workpiece) => workpiece,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid workpiece body: {error}"))),
        };
        // Re-staging an existing id overwrites in place; only a net-new id grows
        // the map, so the cap gates new keys and lets an idempotent re-stage
        // through at the ceiling.
        if !self.staged.contains_key(&workpiece.id.0) && self.staged.len() >= MAX_STAGED_WORKPIECES {
            return Routed::Reply(error_response(429, "staged-workpiece budget exhausted"));
        }
        self.staged.insert(workpiece.id.0.clone(), workpiece.clone());
        Routed::Reply(json(201, &workpiece))
    }

    /// `POST /drafts` — open a fresh empty draft under a new handle.
    fn open_draft(&mut self) -> Routed {
        if self.drafts.len() >= MAX_OPEN_DRAFTS {
            return Routed::Reply(error_response(429, "open-draft budget exhausted"));
        }
        let draft_id = self.next_draft;
        self.next_draft += 1;
        let draft = BloomDraft::default();
        self.drafts.insert(draft_id, draft.clone());
        Routed::Reply(json(201, &DraftView { draft_id: draft_id.to_string(), draft }))
    }

    /// `GET /drafts/{id}` — read one open draft.
    fn get_draft(&self, id: &str) -> Routed {
        match self.lookup_draft(id) {
            Ok((draft_id, draft)) => Routed::Reply(json(200, &DraftView { draft_id: draft_id.to_string(), draft })),
            Err(response) => Routed::Reply(response),
        }
    }

    /// `PATCH /drafts/{id}` — replace the present fields of an open draft.
    fn patch_draft(&mut self, id: &str, body: &[u8]) -> Routed {
        let handle = match parse_draft_id(id) {
            Some(handle) if self.drafts.contains_key(&handle) => handle,
            _ => return Routed::Reply(error_response(404, "no such draft")),
        };
        let patch: DraftPatch = match serde_json::from_slice(body) {
            Ok(patch) => patch,
            Err(error) => return Routed::Reply(error_response(400, &format!("invalid draft patch: {error}"))),
        };
        let draft = self.drafts.get_mut(&handle).expect("draft presence checked above");
        if let Some(proposals) = patch.proposals {
            draft.proposals = proposals;
        }
        if let Some(base) = patch.base {
            draft.base = base;
        }
        if let Some(stage_catalog) = patch.stage_catalog {
            draft.stage_catalog = stage_catalog;
        }
        if let Some(toolchain) = patch.toolchain {
            draft.toolchain = toolchain;
        }
        if let Some(policy) = patch.policy {
            draft.policy = policy;
        }
        if let Some(budget) = patch.budget {
            draft.budget = budget;
        }
        if let Some(forecast) = patch.forecast {
            draft.forecast = forecast;
        }
        Routed::Reply(json(200, &DraftView { draft_id: handle.to_string(), draft: draft.clone() }))
    }

    /// `POST /drafts/{id}/seal` — run the pre-seal approve gate over every
    /// membership, then freeze the draft into a `BloomSpec` and admit `Fact::Seal`
    /// through the control core (issue #3583, the enforcement half of #3571's gate
    /// library).
    ///
    /// For each draft proposal the operator supplies a
    /// [`MemberProjection`](super::dto::MemberProjection) in the `SealRequest`,
    /// matched by `{workpiece, scope_revision}`. The host builds a
    /// [`Gate::AdmissionRequest`](crate::bloomery::AdmissionRequest) from it and
    /// runs [`Gate::evaluate`], replacing the proposal's operator-set `approval`
    /// with the gate-formed one so the seal-time `validate_member_admission`
    /// admits a policy-authored approval rather than an unchecked assertion.
    ///
    /// The gate is fail-closed at every branch: no loaded policy, a missing
    /// projection, an `Incomplete` verdict, or an above-`auto` member all **refuse
    /// the seal** (`422`) rather than admit. Above-`auto` members require the
    /// deferred signature-verification path, whose live async wiring is this
    /// slice's follow-up child — until it lands an above-`auto` member fails
    /// closed here, never admitted on the operator's unverified assertion.
    fn seal_draft(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let (_, draft) = match self.lookup_draft(id) {
            Ok(found) => found,
            Err(response) => return Routed::Reply(response),
        };
        let request: SealRequest = match parse_optional_body(body) {
            Ok(request) => request,
            Err(response) => return Routed::Reply(response),
        };
        // No policy → fail closed: never admit a seal the gate could not decide.
        let Some(policy) = self.policy.as_ref() else {
            return Routed::Reply(error_response(422, "approval policy unavailable; seal fails closed"));
        };
        let gate = Gate::new(policy);
        // Indexed once so the member loop stays O(n + m) however many
        // projections the request carries; first occurrence wins on a
        // duplicate key, matching the linear scan this replaces.
        let mut projections = HashMap::with_capacity(request.projections.len());
        for projection in &request.projections {
            projections.entry((&projection.workpiece, &projection.scope_revision)).or_insert(projection);
        }
        let mut sealed_proposals = Vec::with_capacity(draft.proposals.len());
        for proposal in &draft.proposals {
            let member = &proposal.workpiece.0;
            let Some(&projection) = projections.get(&(&proposal.workpiece, &proposal.scope_revision)) else {
                return Routed::Reply(error_response(
                    422,
                    &format!("member {member} has no scope projection; seal fails closed"),
                ));
            };
            // The digest binds the approval to the projection as evaluated. It
            // is computed over the canonical wire re-encoding of the decoded
            // struct, not the raw request slice: the JSON body carries no
            // per-projection byte boundaries, and the canonical form is
            // reproducible from the shared DTO by any party rather than
            // sensitive to the sender's whitespace and field order.
            let projection_digest = match to_vec(projection) {
                Ok(bytes) => Digest::of_wire_bytes(&bytes),
                Err(error) => {
                    return Routed::Reply(error_response(500, &format!("projection encode failed: {error}")));
                }
            };
            let admission = AdmissionRequest {
                scope_revision: proposal.scope_revision,
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
                    return Routed::Reply(error_response(
                        422,
                        &format!("member {member} is incomplete: {incompleteness:?}"),
                    ));
                }
                Decision::RequiresStatement(tier) => {
                    return Routed::Reply(error_response(
                        422,
                        &format!(
                            "member {member} resolves tier {tier:?}; above-auto signed-statement enforcement \
                             is the follow-up child #3599 — seal fails closed until it lands"
                        ),
                    ));
                }
            }
        }
        let mut gated = draft;
        gated.proposals = sealed_proposals;
        let spec = gated.seal();
        // Persist each member's advisory work-order description keyed by the
        // sealed bloom id, before the seal defers — the text is operator-supplied
        // context (#3595) the executor driver reads back at dispatch, and the api
        // cap is mail-only, so it rides a store write rather than the sealed spec.
        // Best-effort and fire-and-forget: a description write never gates the
        // seal, and a member with none simply dispatches subject-only.
        Self::persist_descriptions(ctx, &spec, &request.descriptions);
        let key = request.idempotency_key.unwrap_or_else(|| hex_encode(spec.id().0.as_bytes()));
        self.admit(ctx, &Event { idempotency_key: IdempotencyKey(key), fact: Fact::Seal(spec) })
    }

    /// Write one dispatch-description row per member the operator supplied text
    /// for, keyed by (sealed bloom id, workpiece). Fire-and-forget to the
    /// `aether.store` mailbox — the reply is absorbed by
    /// [`on_record_description_result`](BloomeryApiCapability::on_record_description_result);
    /// the seal's own outcome is unaffected. A description for a member that later
    /// fails to seal is an orphan row keyed by a bloom id that never dispatches —
    /// harmless and never read.
    fn persist_descriptions(
        ctx: &NativeCtx<'_, Manual>,
        spec: &aether_bloomery::BloomSpec,
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
            let _ = ctx.send_envelope_detached(
                store_mailbox(),
                <RecordDispatchDescription as Kind>::ID,
                &record.encode_into_bytes(),
            );
        }
    }

    /// `POST /blooms/{id}/supersede` — seal the named successor draft and admit
    /// `Fact::Supersede` against the `{id}` predecessor bloom.
    fn supersede(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
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
        let successor = draft.seal();
        let key = request.idempotency_key.unwrap_or_else(|| hex_encode(successor.id().0.as_bytes()));
        self.admit(
            ctx,
            &Event { idempotency_key: IdempotencyKey(key), fact: Fact::Supersede { predecessor, successor } },
        )
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
    fn answer_bloom(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
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
        let correlation = self.send_tracked(ctx, signing_mailbox(), &Verify { statement });
        Routed::DeferredVerify { correlation, event: Box::new(event) }
    }

    /// Resolve a held answer request from the `aether.signing` verify reply: a
    /// verified signature admits the stashed adoption event (re-deferring on the
    /// reducer reply); a `verified: false` verdict or an undecodable-statement
    /// error is a `400`.
    fn resolve_verify(&mut self, ctx: &NativeCtx<'_, Manual>, result: VerifyResult) {
        let correlation = ctx.reply_target().correlation_id;
        let Some(VerifyPending { inbound, event }) = self.verifying.remove(&correlation) else {
            return;
        };
        match result {
            VerifyResult::Ok { verified: true } => match to_vec(&event) {
                Ok(bytes) => {
                    let correlation = self.send_tracked(ctx, control_mailbox(), &Admit { event: bytes });
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
    fn query(&self, ctx: &NativeCtx<'_, Manual>, bloom: Option<Vec<u8>>) -> Routed {
        Routed::Deferred(self.send_tracked(ctx, control_mailbox(), &Query { bloom }))
    }

    /// `GET /blooms/{id}` — read one bloom's live view by hex id.
    fn query_bloom(&self, ctx: &NativeCtx<'_, Manual>, id: &str) -> Routed {
        digest_from_hex(id).map_or_else(
            || Routed::Reply(error_response(400, "bloom id is not a 32-byte hex digest")),
            |digest| self.query(ctx, Some(digest.as_bytes().to_vec())),
        )
    }

    /// Encode the event, wrap it in an [`Admit`], and dispatch it to the
    /// control core, deferring the HTTP reply on the admit reply.
    fn admit(&self, ctx: &NativeCtx<'_, Manual>, event: &Event) -> Routed {
        let bytes = match to_vec(event) {
            Ok(bytes) => bytes,
            Err(error) => return Routed::Reply(error_response(500, &format!("event encode failed: {error}"))),
        };
        Routed::Deferred(self.send_tracked(ctx, control_mailbox(), &Admit { event: bytes }))
    }

    /// Dispatch a mail to `recipient` as a fresh causal root, subscribe to its
    /// settlement (the no-reply safety net), and return the correlation the
    /// reply will echo.
    fn send_tracked<K: Kind>(&self, ctx: &NativeCtx<'_, Manual>, recipient: MailboxId, payload: &K) -> u64 {
        let bytes = payload.encode_into_bytes();
        let mail_id = ctx.send_envelope_detached(recipient, K::ID, &bytes);
        if let Some(registry) = self.mailer.settlement_registry() {
            registry.subscribe_settlement_mail(
                mail_id,
                self.self_mailbox,
                <Settled as Kind>::ID,
                Arc::clone(&self.mailer),
            );
        }
        mail_id.correlation_id
    }

    /// Render every open draft with its handle.
    fn drafts_view(&self) -> DraftsView {
        DraftsView {
            drafts: self
                .drafts
                .iter()
                .map(|(id, draft)| DraftView { draft_id: id.to_string(), draft: draft.clone() })
                .collect(),
        }
    }

    /// Resolve a draft handle to its id + a clone, or the `404` to reply.
    fn lookup_draft(&self, id: &str) -> Result<(u64, BloomDraft), HttpServerResponse> {
        parse_draft_id(id)
            .and_then(|handle| self.drafts.get(&handle).map(|draft| (handle, draft.clone())))
            .ok_or_else(|| error_response(404, "no such draft"))
    }

    /// Answer a deferred request from a downstream reply: the reply's
    /// `sender.correlation_id` is auto-echoed (ADR-0042) to the dispatch that
    /// deferred it, so it recovers the held reply guard.
    fn answer(&mut self, ctx: &NativeCtx<'_, Manual>, response: &HttpServerResponse) {
        let correlation = ctx.reply_target().correlation_id;
        if let Some(inbound) = self.pending.remove(&correlation) {
            inbound.reply(response);
        }
    }
}

/// Render a write route's [`AdmitResult`] into its HTTP response: the reducer
/// outcome (decoded from the wire bytes the admit reply carries), or the error.
fn admit_response(result: AdmitResult) -> HttpServerResponse {
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
fn query_response(result: QueryResult) -> HttpServerResponse {
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

/// Render the store's [`ReplayJournalResult`] into its HTTP response: every
/// journaled event decoded, oldest first.
fn journal_response(result: ReplayJournalResult) -> HttpServerResponse {
    match result {
        ReplayJournalResult::Ok { records } => {
            let mut entries = Vec::with_capacity(records.len());
            for record in records {
                match from_bytes::<Event>(&record.event) {
                    Ok(event) => entries.push(JournalEntry {
                        sequence: record.sequence,
                        idempotency_key: record.idempotency_key,
                        event,
                    }),
                    Err(error) => {
                        return error_response(
                            500,
                            &format!("journal record {} decode failed: {error}", record.sequence),
                        );
                    }
                }
            }
            json(200, &JournalView { records: entries })
        }
        ReplayJournalResult::Err { error } => error_response(500, &error),
    }
}

/// Render the artifacts cap's [`GetResult`] into its HTTP response: the raw
/// bytes, a `404`, or the error.
fn artifact_response(result: GetResult) -> HttpServerResponse {
    match result {
        GetResult::Ok { bytes, .. } => bytes_response(200, bytes),
        GetResult::Err { error: ArtifactsError::NotFound, .. } => error_response(404, "no such artifact"),
        GetResult::Err { error, .. } => error_response(500, &format!("artifacts error: {error:?}")),
    }
}

/// The `aether.store` mailbox (a depth-1 root cap).
fn store_mailbox() -> MailboxId {
    mailbox_id_from_path("aether.store")
}

/// The `aether.artifacts` mailbox (a depth-1 root cap).
fn artifacts_mailbox() -> MailboxId {
    mailbox_id_from_path("aether.artifacts")
}

/// The autoloaded control-core component's lineage mailbox.
fn signing_mailbox() -> MailboxId {
    mailbox_id_from_path("aether.signing")
}

fn control_mailbox() -> MailboxId {
    mailbox_id_from_path(CONTROL_CORE_PATH)
}

/// A `Content-Type: application/json` header set.
fn json_headers() -> Vec<HttpHeader> {
    vec![HttpHeader { name: "content-type".to_owned(), value: "application/json".to_owned() }]
}

/// A JSON response over a serializable value; a `500` if it fails to encode.
fn json(status: u16, value: &impl Serialize) -> HttpServerResponse {
    match serde_json::to_vec(value) {
        Ok(body) => HttpServerResponse { status, headers: json_headers(), body },
        Err(error) => error_response(500, &format!("response encode failed: {error}")),
    }
}

/// A structured JSON error body.
fn error_response(status: u16, message: &str) -> HttpServerResponse {
    let body = serde_json::to_vec(&ErrorView { error: message.to_owned() }).unwrap_or_else(|_| message.into());
    HttpServerResponse { status, headers: json_headers(), body }
}

/// A raw `application/octet-stream` byte response (artifact bytes).
fn bytes_response(status: u16, body: Vec<u8>) -> HttpServerResponse {
    HttpServerResponse {
        status,
        headers: vec![HttpHeader { name: "content-type".to_owned(), value: "application/octet-stream".to_owned() }],
        body,
    }
}

/// Parse a possibly-empty request body into a `Default` body type: an empty
/// body is the default, a non-empty one is parsed, a malformed one is a `400`.
fn parse_optional_body<T: DeserializeOwned + Default>(body: &[u8]) -> Result<T, HttpServerResponse> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|error| error_response(400, &format!("invalid request body: {error}")))
}

/// Parse a draft handle path segment.
fn parse_draft_id(id: &str) -> Option<u64> {
    id.parse().ok()
}

/// Lowercase-hex-encode bytes (bloom ids in URLs).
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("high nibble is 0..16"));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).expect("low nibble is 0..16"));
    }
    out
}

/// Decode a lowercase/uppercase hex string of exactly 32 bytes into a digest.
fn digest_from_hex(hex: &str) -> Option<Digest> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, slot) in bytes.iter_mut().enumerate() {
        let high = hex.as_bytes()[index * 2];
        let low = hex.as_bytes()[index * 2 + 1];
        *slot = (hex_nibble(high)? << 4) | hex_nibble(low)?;
    }
    Some(Digest::from_bytes(bytes))
}

/// One hex digit to its nibble value.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{SealRequest, digest_from_hex, hex_encode, parse_draft_id, parse_optional_body};
    use aether_bloomery::Digest;

    #[test]
    fn hex_round_trips_a_digest() {
        // The bloom-id URL encoding: 32 bytes → 64 lowercase hex chars → back to
        // the same 32 bytes. Catches a nibble-order or length bug in the hand-
        // rolled hex the id routes depend on.
        let digest = Digest::from_bytes([
            0x00, 0x0f, 0x10, 0xff, 0xa5, 0x5a, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
            0x0d, 0x0e, 0x1f, 0x2e, 0x3d, 0x4c, 0x5b, 0x6a, 0x79, 0x88, 0x97, 0xa6, 0xb5, 0xc4,
        ]);
        let hex = hex_encode(digest.as_bytes());
        assert_eq!(hex.len(), 64);
        assert_eq!(digest_from_hex(&hex), Some(digest));
    }

    #[test]
    fn digest_from_hex_rejects_bad_input() {
        // A 63/65-char string and a non-hex char are both rejected rather than
        // silently truncated or mis-decoded into a wrong bloom id.
        assert_eq!(digest_from_hex(&"a".repeat(63)), None);
        assert_eq!(digest_from_hex(&"a".repeat(65)), None);
        assert_eq!(digest_from_hex(&"g".repeat(64)), None);
    }

    #[test]
    fn parse_draft_id_is_a_u64() {
        assert_eq!(parse_draft_id("7"), Some(7));
        assert_eq!(parse_draft_id("notanid"), None);
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
