//! The `BloomeryApiCapability` REST control router (ADR-0149 §Packaging, issue
//! #3498).
//!
//! A native `aether.http.server`-mounted router that lets an operator drive a
//! bloom end-to-end from `curl`: stage workpieces, shape and seal drafts,
//! supersede, and read the live blooms / view document / journal / artifacts —
//! no typed-mail RPC vocabulary required. Its routes are authored through the
//! typed `#[http::router]` / `#[http::route]` surface (ADR-0131/ADR-0154): each
//! `#[http::route]` method claims one exact path + method on the
//! `aether.http.server` ingress cap (ADR-0108/ADR-0130), and the macro generates
//! the segment dispatch and path-capture binding the hand-rolled
//! method-plus-path `match` used to carry (#3672).
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
//!   streaming path instead of delivering a buffered `HttpServerRequest`.
//! - A settlement subscription answers `504` for a request whose downstream
//!   chain settles without ever replying (a dropped or unloaded control core).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;

use aether_actor::{Manual, runtime};
use aether_bloomery::{
    Admit, AdmitResult, BloomDraft, BloomId, BloomView, Digest, Event, Fact, IdempotencyKey, Membership, Outcome,
    Query, QueryResult, ReplayJournal, ReplayJournalResult, Statement, ViewDocument, Workpiece, digest_of,
};
use aether_capabilities::http::{self, HttpHeader, HttpServerResponse};
use aether_data::wire::{from_bytes, to_vec};
use aether_data::{Kind, MailId, MailboxId};
use aether_kinds::trace::Settled;
use aether_substrate::{InboundMail, Mailer};

use aether_actor::HandlesKind;
use aether_substrate::actor::native::NativeActorMailbox;
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

use super::BloomeryApiCapability;
use super::dto::{
    DraftPatch, DraftView, DraftsView, ErrorView, JournalEntry, JournalView, MemberProjection, OutcomeView,
    SealRequest, SupersedeRequest, WorkpiecesView,
};
use crate::artifacts::{ArtifactsCapability, ArtifactsError, Get, GetResult};
use crate::bloomery::{
    AdmissionRequest, ApprovalPolicy, Decision, Gate, precheck_statement, verified_statement_approval,
};
use crate::signing::{SigningCapability, Verify, VerifyResult};
use crate::store::{RecordDispatchDescription, RecordDispatchDescriptionResult, StoreCapability};

// The control core is addressed by its one exported namespace const, its
// ADR-0099 lineage computed by the component host's own `resolve_embedded` —
// never a re-spelled path literal (#3668).
use aether_bloomery::CONTROL_CORE_NAMESPACE;
use aether_capabilities::resolve_embedded;

/// Per-process ceilings on the pre-seal shaping maps. Staged workpieces and
/// open drafts are pure in-memory shaping state with no durable owner to evict
/// them, so the router caps each map and rejects growth past the cap rather
/// than letting an operator (or a runaway client) grow it without bound
/// (CLAUDE.md §Runtime: error rather than grow unboundedly). Capacity frees
/// only when the shaping session restarts.
const MAX_STAGED_WORKPIECES: usize = 1024;
const MAX_OPEN_DRAFTS: usize = 1024;

/// Ceiling on the outstanding deferred-seal count — the `seals` map (and its
/// paired `seal_verifications`) grow one entry per above-auto seal in flight, so
/// cap the map the same way `staged` / `drafts` are capped and refuse a new
/// deferred seal at the ceiling before dispatching any verification (issue #3599).
const MAX_OPEN_SEALS: usize = 1024;

/// Ceiling on one seal's membership count. Each above-auto member fans out one
/// `aether.signing` `Verify` dispatch and one held `SealVerify` correlation, so a
/// single seal request amplifies into N in-flight verifications plus a held
/// `PendingSeal`; cap the draft's member count and refuse a seal past it before
/// any dispatch, so one request cannot grow the in-flight `seals` /
/// `seal_verifications` maps without bound (issue #3599). A bloom's realistic
/// membership is a handful; the ceiling is generous headroom over that.
const MAX_SEAL_MEMBERS: usize = 256;

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
    /// Seals held across N above-auto member signature verifications, keyed by a
    /// minted `next_seal` handle. The held seal admits `Fact::Seal` only when
    /// every above-auto member's signature has verified (issue #3599); any
    /// rejection refuses the whole seal (`422`, fail closed) and tears down its
    /// sibling verify correlations.
    seals: HashMap<u64, PendingSeal>,
    /// The next seal handle to mint.
    next_seal: u64,
    /// Each in-flight above-auto member verification, keyed by its `Verify`
    /// dispatch `MailId.correlation_id`, back-pointing at the held [`PendingSeal`]
    /// and the member it forms the approval for on a verified reply.
    seal_verifications: HashMap<u64, SealVerify>,
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

/// A seal held across N above-auto member signature verifications (issue #3599).
/// The gate resolved every auto member synchronously (their approvals already
/// sit in `gated.proposals`); each above-auto member's approval is slotted in as
/// its signature verifies, and the last verification seals `gated` and admits
/// `Fact::Seal`.
struct PendingSeal {
    /// The held HTTP reply obligation.
    inbound: InboundMail,
    /// The gated draft: auto members carry their gate-formed approval; each
    /// above-auto member's approval is overwritten by
    /// [`verified_statement_approval`] as its signature verifies.
    gated: BloomDraft,
    /// The operator-supplied per-member work-order descriptions (#3595),
    /// persisted once the seal completes — the same fire-and-forget store write
    /// the synchronous path makes.
    descriptions: BTreeMap<String, String>,
    /// The admit idempotency key override, defaulted to the sealed bloom id when
    /// the last verification seals the spec.
    idempotency_key: Option<String>,
    /// Above-auto members whose signature has not yet verified; the verification
    /// that drops this to zero seals and admits.
    remaining: usize,
}

/// One in-flight above-auto member verification: which held seal and member it
/// resolves, and the statement whose verified form becomes that member's
/// approval evidence.
struct SealVerify {
    /// The held [`PendingSeal`] handle this verification belongs to.
    seal: u64,
    /// The index into the seal's `gated.proposals` of the member it forms.
    member_index: usize,
    /// The scope revision the formed approval binds.
    scope_revision: Digest,
    /// The signed statement whose verified form (`verified_statement_approval`)
    /// becomes the member's approval.
    statement: Statement,
}

/// The pre-wired parts of a deferred seal, handed from [`ApiCapabilityState::seal_draft`]
/// to [`BloomeryApiCapability::on_request`] so the handle mint and map inserts
/// (which need `&mut self`) happen there, mirroring how `DeferredVerify` defers
/// its map insert to the ingress handler.
struct PendingSealSetup {
    gated: BloomDraft,
    descriptions: BTreeMap<String, String>,
    idempotency_key: Option<String>,
    /// One entry per above-auto member — the dispatched `Verify` correlation and
    /// the member it forms.
    verifications: Vec<PendingVerify>,
}

/// One above-auto member's dispatched verification, pre-`PendingSeal`-handle.
struct PendingVerify {
    correlation: u64,
    member_index: usize,
    scope_revision: Digest,
    statement: Statement,
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
    /// Await N `aether.signing` verify replies for an above-auto seal (issue
    /// #3599); the last verified reply seals and admits, any rejection refuses
    /// the whole seal (`422`, fail closed).
    DeferredSeal(Box<PendingSealSetup>),
}

#[http::router]
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
            seals: HashMap::new(),
            next_seal: 1,
            seal_verifications: HashMap::new(),
        })
    }

    /// `#[http::router]` appends the per-route `RegisterRouteSelf` sends to
    /// this body — one exact-match claim per `(static head, method)` group,
    /// registered on the HTTP ingress cap (ADR-0130) post-init (#3672).
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {}

    /// `POST /workpieces` — stage a workpiece for later draft membership.
    #[http::route(Post, "/workpieces")]
    fn on_post_workpieces(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = state.stage_workpiece(&ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `GET /workpieces` — list the staged workpieces.
    #[http::route(Get, "/workpieces")]
    fn on_get_workpieces(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = Routed::Reply(json(200, &WorkpiecesView { workpieces: state.staged.values().cloned().collect() }));
        finish(state, ctx, routed)
    }

    /// `POST /drafts` — open a fresh empty draft under a new handle.
    #[http::route(Post, "/drafts")]
    fn on_post_drafts(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = state.open_draft();
        finish(state, ctx, routed)
    }

    /// `GET /drafts` — list the open drafts.
    #[http::route(Get, "/drafts")]
    fn on_get_drafts(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = Routed::Reply(json(200, &state.drafts_view()));
        finish(state, ctx, routed)
    }

    /// `GET /drafts/{id}` — read one open draft.
    #[http::route(Get, "/drafts/{id}")]
    fn on_get_draft(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = state.get_draft(&id);
        finish(state, ctx, routed)
    }

    /// `PATCH /drafts/{id}` — replace the present fields of an open draft.
    #[http::route(Patch, "/drafts/{id}")]
    fn on_patch_draft(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = state.patch_draft(&id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /drafts/{id}/seal` — run the pre-seal approve gate over every
    /// membership, then freeze the draft and admit `Fact::Seal`.
    #[http::route(Post, "/drafts/{id}/seal")]
    fn on_seal_draft(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = state.seal_draft(&ctx, &id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `GET /blooms` — read the whole live projection.
    #[http::route(Get, "/blooms")]
    fn on_get_blooms(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = state.query(&ctx, None);
        finish(state, ctx, routed)
    }

    /// `GET /view` — read the whole live projection (the `GET /blooms` alias).
    #[http::route(Get, "/view")]
    fn on_get_view(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = state.query(&ctx, None);
        finish(state, ctx, routed)
    }

    /// `GET /blooms/{id}` — read one bloom's live view by hex id.
    #[http::route(Get, "/blooms/{id}")]
    fn on_get_bloom(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = state.query_bloom(&ctx, &id);
        finish(state, ctx, routed)
    }

    /// `POST /blooms/{id}/supersede` — seal the successor draft and admit
    /// `Fact::Supersede` against the `{id}` predecessor bloom.
    #[http::route(Post, "/blooms/{id}/supersede")]
    fn on_supersede(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = state.supersede(&ctx, &id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /blooms/{id}/answer` — adopt a signed answer to a parked question.
    #[http::route(Post, "/blooms/{id}/answer")]
    fn on_answer(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = state.answer_bloom(&ctx, &id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `GET /journal` — read the durable event journal from the store.
    #[http::route(Get, "/journal")]
    fn on_get_journal(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = Routed::Deferred(state.send_tracked(ctx.actor::<StoreCapability>(), &ReplayJournal));
        finish(state, ctx, routed)
    }

    /// `GET /artifacts/{digest}` — fetch a content-addressed artifact.
    #[http::route(Get, "/artifacts/{digest}")]
    fn on_get_artifact(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        digest: http::Path<String>,
    ) -> http::Outcome {
        let routed =
            Routed::Deferred(state.send_tracked(ctx.actor::<ArtifactsCapability>(), &Get { digest: digest.0 }));
        finish(state, ctx, routed)
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
        // The same `VerifyResult` kind carries both the answer-gate verify and an
        // above-auto seal-member verify; the reply correlation says which. A seal
        // verify resolves against the held `PendingSeal`, an answer verify against
        // its held adoption event.
        let correlation = ctx.reply_target().correlation_id;
        if state.seal_verifications.contains_key(&correlation) {
            state.resolve_seal_verify(ctx, correlation, mail);
        } else {
            state.resolve_verify(ctx, mail);
        }
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
        } else if let Some(SealVerify { seal, .. }) = state.seal_verifications.remove(&mail.root.correlation_id) {
            // An above-auto member's verify chain settled without a reply — the
            // whole seal cannot complete, so fail it closed and tear down its
            // still-outstanding siblings (a dropped signing capability).
            state.fail_seal(seal, 504, "signature verification settled without a reply");
        }
    }
}

impl ApiCapabilityState {
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
    fn seal_draft(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let (_, draft) = match self.lookup_draft(id) {
            Ok(found) => found,
            Err(response) => return Routed::Reply(response),
        };
        let request: SealRequest = match parse_optional_body(body) {
            Ok(request) => request,
            Err(response) => return Routed::Reply(response),
        };
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
            match resolve_seal_memberships(&gate, &draft.proposals, &request.projections) {
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
            Self::persist_descriptions(ctx, &spec, &request.descriptions);
            let key = request.idempotency_key.unwrap_or_else(|| hex_encode(spec.id().0.as_bytes()));
            return self.admit(ctx, &Event { idempotency_key: IdempotencyKey(key), fact: Fact::Seal(spec) });
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
            descriptions: request.descriptions,
            idempotency_key: request.idempotency_key,
            verifications,
        }))
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
            ctx.actor::<StoreCapability>().send_detached(&record);
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
        let correlation = self.send_tracked(ctx.actor::<SigningCapability>(), &Verify { statement });
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
                    let correlation = self.send_tracked_control(ctx, &Admit { event: bytes });
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

    /// Resolve one above-auto member verification for a held seal (issue #3599):
    /// a verified signature forms the member's approval and decrements the seal's
    /// countdown, sealing and admitting when the last one lands; a `verified:
    /// false` verdict or an `Err` refuses the whole seal (`422`, fail closed) and
    /// tears down its sibling correlations.
    fn resolve_seal_verify(&mut self, ctx: &NativeCtx<'_, Manual>, correlation: u64, result: VerifyResult) {
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
                let PendingSeal { inbound, gated, descriptions, idempotency_key, .. } =
                    self.seals.remove(&seal).expect("seal present; just mutated it");
                let spec = gated.seal();
                Self::persist_descriptions(ctx, &spec, &descriptions);
                let key = idempotency_key.unwrap_or_else(|| hex_encode(spec.id().0.as_bytes()));
                match to_vec(&Event { idempotency_key: IdempotencyKey(key), fact: Fact::Seal(spec) }) {
                    Ok(bytes) => {
                        let correlation = self.send_tracked_control(ctx, &Admit { event: bytes });
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
    fn fail_seal(&mut self, seal: u64, status: u16, message: &str) {
        let Some(PendingSeal { inbound, .. }) = self.seals.remove(&seal) else {
            return;
        };
        self.seal_verifications.retain(|_, verify| verify.seal != seal);
        inbound.reply(&error_response(status, message));
    }

    /// `GET /blooms` and `GET /view` — read the whole live projection.
    fn query(&self, ctx: &NativeCtx<'_, Manual>, bloom: Option<Vec<u8>>) -> Routed {
        Routed::Deferred(self.send_tracked_control(ctx, &Query { bloom }))
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
        Routed::Deferred(self.send_tracked_control(ctx, &Admit { event: bytes }))
    }

    /// Dispatch a mail to a peer cap's typed handle as a fresh causal root,
    /// subscribe to its settlement (the no-reply safety net), and return the
    /// correlation the reply will echo. The `HandlesKind` gate makes a
    /// wrong-kind dispatch a compile error — the raw-envelope form this
    /// replaces had no such check.
    fn send_tracked<R, K>(&self, target: NativeActorMailbox<'_, R>, payload: &K) -> u64
    where
        R: HandlesKind<K>,
        K: Kind,
    {
        self.track(target.send_detached_tracked(payload))
    }

    /// The control-core variant of [`send_tracked`](Self::send_tracked): the
    /// control core is a loaded wasm component, not a nameable native sibling
    /// type, so its dispatch stays on the raw envelope against the
    /// `resolve_embedded`-computed mailbox.
    fn send_tracked_control<K: Kind>(&self, ctx: &NativeCtx<'_, Manual>, payload: &K) -> u64 {
        let bytes = payload.encode_into_bytes();
        self.track(ctx.send_envelope_detached(control_mailbox(), K::ID, &bytes))
    }

    /// Subscribe this cap to `mail_id`'s settlement and return the correlation
    /// id that keys the held HTTP reply guard.
    fn track(&self, mail_id: MailId) -> u64 {
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

/// Adapt a route helper's [`Routed`] disposition into the `http::Outcome` a
/// `#[http::route]` method returns, carrying the reply-obligation stash that the
/// single `on_request` ingress did before the router became per-route (#3672).
/// `Reply` answers inline — the macro glue replies through the still-held
/// inbound — while every deferred variant moves the request's inbound into its
/// correlation table and returns `Deferred` so the glue does not also answer;
/// the reply / settlement handlers recover it by correlation exactly as before.
fn finish(
    state: &mut ApiCapabilityState,
    mut ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
    routed: Routed,
) -> http::Outcome {
    match routed {
        Routed::Reply(response) => http::Outcome::Reply(response),
        Routed::Deferred(correlation) => {
            state.pending.insert(correlation, ctx.take_inbound());
            http::Outcome::Deferred
        }
        Routed::DeferredVerify { correlation, event } => {
            state.verifying.insert(correlation, VerifyPending { inbound: ctx.take_inbound(), event: *event });
            http::Outcome::Deferred
        }
        Routed::DeferredSeal(setup) => {
            let PendingSealSetup { gated, descriptions, idempotency_key, verifications } = *setup;
            let seal = state.next_seal;
            state.next_seal += 1;
            let remaining = verifications.len();
            for verify in verifications {
                let PendingVerify { correlation, member_index, scope_revision, statement } = verify;
                state
                    .seal_verifications
                    .insert(correlation, SealVerify { seal, member_index, scope_revision, statement });
            }
            let inbound = ctx.take_inbound();
            state.seals.insert(seal, PendingSeal { inbound, gated, descriptions, idempotency_key, remaining });
            http::Outcome::Deferred
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

fn control_mailbox() -> MailboxId {
    resolve_embedded(CONTROL_CORE_NAMESPACE)
}

/// A `Content-Type: application/json` header set.
fn json_headers() -> Vec<HttpHeader> {
    vec![HttpHeader { name: "content-type".to_owned(), value: "application/json".to_owned() }]
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
/// Extracted from [`seal_draft`](BloomeryApiCapability::seal_draft) so that hot
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
                if let Err(rejected) = precheck_statement(proposal.scope_revision, statement) {
                    return Err(error_response(
                        422,
                        &format!("member {member} signed statement rejected: {rejected:?}; seal fails closed"),
                    ));
                }
                sealed_proposals.push(proposal.clone());
                pending_verifications.push((index, proposal.scope_revision, statement.clone()));
            }
        }
    }
    Ok((sealed_proposals, pending_verifications))
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
