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
//! # The three route shapes
//!
//! Pre-seal shaping is pure in-memory state (ADR-0149 §The bloom: drafts
//! "claim nothing"), so those routes reply synchronously.
//!
//! A route that reads or writes durable state forwards one request to a peer cap
//! and answers its one reply. Those are **deferred routes** over the ADR-0154
//! relay: the route returns `ctx.defer(&request).to::<Peer>()` and a paired
//! `#[http::reply]` method maps the peer's reply into the response. The send
//! inherits the request's causal chain (ADR-0080 §7), so the request stays in
//! flight across the round-trip without anything being held here, and the
//! requester's reply target rides the ADR-0139 request-context table rather than
//! a correlation map of ours. A peer that settles without replying is answered
//! by the HTTP server's own `502`; one that never settles, by its request
//! timeout. Neither needs machinery in this cap.
//!
//! Three routes are genuinely **multi-hop** and keep an explicit obligation,
//! because their next hop is not the answer: `POST /blooms/{id}/answer/{question}` and
//! `POST /claims/releases` each verify a signature before they admit, and
//! `POST /drafts/{id}/seal` joins N member verifications before admitting once
//! (the ADR-0154 §2 scatter/gather exclusion). Their terminal `Admit` is
//! dispatched from a reply handler, which holds no route obligation to defer, so
//! it goes out as a tracked fresh root and is answered by hand from
//! [`state::ApiCapabilityState::pending`]; a settlement subscription answers
//! `504` if that chain never replies.
//!
//! A reply is delivered to a **typed** handler and deliberately not to a
//! `#[fallback]`: a fallback would widen the actor's accept-set to every kind —
//! including the request-stream kinds — and the HTTP server would then route each
//! request down the streaming path instead of delivering a buffered
//! `HttpServerRequest`.
//!
//! # The module tree
//!
//! `#[http::router]` collects its route set from one impl block, so every
//! `#[http::route]` method and every typed reply handler stays here, each a
//! two-line delegation to the module that owns the work. [`state`] holds the
//! router state, the ceilings that bound it, and the deferral machinery;
//! [`workpieces`], [`drafts`], [`seal`], [`blooms`], `claims` and [`reads`] hold
//! one resource each; [`response`] and [`hex`] hold the shared response
//! constructors and the bloom-id URL codec.

mod blooms;
mod calibration;
#[cfg(feature = "github")]
mod claims;
mod configs;
mod drafts;
mod hex;
mod reads;
mod response;
mod seal;
mod state;
mod workpieces;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use aether_actor::{Manual, runtime};
use aether_bloomery::{
    AdmitResult, EnumerateClaimsResult, LoadConfigsResult, QueryResult, ReplayJournal, ReplayJournalResult,
    ResolvedConfigs,
};
use aether_http as http;
use aether_http::HttpServerResponse;
use aether_kinds::trace::Settled;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

pub use state::ApiCapabilityState;

use blooms::{admit_response, query_response};
use calibration::calibration_response;
#[cfg(feature = "github")]
use claims::{claims_response, release_status_response};
use configs::{config_response, load_configs};
use reads::{artifact_response, journal_response};
use response::{error_response, json};
use state::{Routed, SealVerify, VerifyPending, finish};

use super::BloomeryApiCapability;
use super::dto::WorkpiecesView;
use crate::artifacts::{Get, GetResult};
use crate::bloomery::load_policy;
use crate::signing::VerifyResult;
use crate::store::{RecordConfigResult, RecordDispatchDescriptionResult};

/// The claim routes' bodies for a build with no GitHub source runtime: an
/// immediate `503` rather than a deferral onto a `SourceCapability` mailbox that
/// was never registered.
///
/// The three routes themselves stay unconditional, because `#[http::router]`
/// collects its method list before the compiler strips `#[cfg]`s — a per-method
/// gate leaves the generated dispatch table naming a method that is not there.
/// Answering `503` from the body is also the honest behavior rather than a
/// workaround: without this the routes would be actively harmful instead of
/// merely absent. `GET /claims` would relay onto a mailbox nothing registered and
/// hang its caller on the HTTP server's timeout, and `POST /claims/releases`
/// would verify the signature, journal a pending release, and enqueue an outbox
/// row that no reactor drains in that build — a request durably pending forever.
#[cfg(not(feature = "github"))]
impl ApiCapabilityState {
    fn claims_unavailable() -> Routed {
        Routed::Reply(error_response(503, "claim inspection and release need the GitHub source runtime"))
    }

    fn list_claims() -> Routed {
        Self::claims_unavailable()
    }

    fn request_claim_release(&self, _ctx: &NativeCtx<'_, Manual>, _body: &[u8]) -> Routed {
        Self::claims_unavailable()
    }

    fn query_claim_release(_digest: &str) -> Routed {
        Self::claims_unavailable()
    }
}

/// Render a claim enumeration. Unreachable without the GitHub source runtime —
/// the route that would ask for one refuses first — so it only has to exist for
/// the `#[http::reply]` glue the route's paired handler generates.
#[cfg(not(feature = "github"))]
fn claims_response(_result: EnumerateClaimsResult) -> HttpServerResponse {
    error_response(503, "claim inspection needs the GitHub source runtime")
}

/// Render a release-status read, unreachable for the same reason.
#[cfg(not(feature = "github"))]
fn release_status_response(_result: QueryResult) -> HttpServerResponse {
    error_response(503, "claim release status needs the GitHub source runtime")
}

/// Composer-supplied params for the REST control api cap (ADR-0156 §3 `Params`
/// channel): the fallback tier-policy file the pre-seal approve gate loads at
/// init (issue #3583). Threaded from the shared GitHub config's
/// `approval_policy_file` at chassis build so one Bloomery configuration serves
/// every reader — a composer-computed value, not an operator-resolvable knob, so
/// it is `Params`.
///
/// The file is the fallback, not the authority: a draft that seals an
/// `aether.bloomery.approval_policy` entry is gated against that sealed value
/// (#4616). A policy file that fails to load leaves the cap with no fallback, so
/// a draft sealing none fails **closed** — its seal is refused rather than
/// admitted at a tier nothing decided.
pub struct ApiParams {
    /// Repository-relative path to the Bloomery-owned fallback tier policy
    /// (`approval-policy.toml`).
    pub approval_policy_file: String,
}

#[http::router]
#[runtime]
impl NativeActor for BloomeryApiCapability {
    type State = ApiCapabilityState;
    // ADR-0156 §3: the approval-policy path is threaded from the shared
    // Bloomery GitHub config at chassis build — composer-computed construction
    // input, not an operator-resolvable knob — so it rides `Params`.
    type Config = ();
    type Params = ApiParams;
    const NAMESPACE: &'static str = "aether.bloomery.api";

    fn init((): (), params: ApiParams, ctx: &mut NativeInitCtx<'_>) -> Result<ApiCapabilityState, BootError> {
        // Load the fallback tier policy once at init. An unreadable or malformed
        // file is not a boot failure — it leaves the cap fallback-less, and a
        // draft that seals no policy of its own then fails closed (its seal is
        // refused), which is the security-required posture: never silently `auto`.
        let file_policy = match load_policy(Path::new(&params.approval_policy_file)) {
            Ok(policy) => Some(policy),
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::api",
                    path = %params.approval_policy_file,
                    ?error,
                    "fallback approval policy unavailable; a draft sealing none fails closed (no auto tier)"
                );
                None
            }
        };
        tracing::info!(
            target: "aether_chassis_bloomery::api",
            policy_loaded = file_policy.is_some(),
            "bloomery REST control api mounted"
        );
        Ok(ApiCapabilityState {
            self_mailbox: ctx.self_id(),
            file_policy,
            configs: ResolvedConfigs::default(),
            configs_ready: false,
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

    /// Read the stored configuration set so a sealed tier policy is resolvable
    /// from inside the synchronous pre-seal gate (#4616). Lives here rather than
    /// in `init` because it is mail, and `init` has none.
    ///
    /// `#[http::router]` appends the per-route `RegisterRouteSelf` sends to
    /// this body — one exact-match claim per `(static head, method)` group,
    /// registered on the HTTP ingress cap (ADR-0130) post-init (#3672).
    fn wire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        load_configs(ctx);
    }

    /// `POST /workpieces` — stage a workpiece for later draft membership.
    #[http::route(Post, "/workpieces")]
    fn on_post_workpieces(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = state.stage_workpiece(&ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /configs` — content-address a configuration of any kind in the
    /// descriptor inventory so a draft or a member can name it (ADR-0174). One
    /// route for every configuration kind: adding a kind adds no route.
    #[http::route(Post, "/configs")]
    fn on_post_configs(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = configs::author_config(&ctx.request().body);
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
        let routed = ApiCapabilityState::query(None);
        finish(state, ctx, routed)
    }

    /// `GET /view` — read the whole live projection (the `GET /blooms` alias).
    #[http::route(Get, "/view")]
    fn on_get_view(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = ApiCapabilityState::query(None);
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
        let routed = ApiCapabilityState::query_bloom(&id);
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

    /// `POST /blooms/{id}/grant` — hand a wedged member more attempts and
    /// resume it on the bloom it already belongs to (#4708).
    #[http::route(Post, "/blooms/{id}/grant")]
    fn on_grant(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = ApiCapabilityState::grant(&id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /blooms/{id}/adjudicate` — close the composition findings the
    /// operator has read, with a stated reason, and let the bloom proceed to
    /// its landing (#4957).
    #[http::route(Post, "/blooms/{id}/adjudicate")]
    fn on_adjudicate(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = ApiCapabilityState::adjudicate(&id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /blooms/{id}/members/{workpiece}/repair` — re-enter a wedged
    /// workpiece at `Verify` on the candidate the operator pushed (#4957). The
    /// gates still run; only the model lap is skipped.
    #[http::route(Post, "/blooms/{id}/members/{workpiece}/repair")]
    fn on_repair(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
        workpiece: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let workpiece = workpiece.0;
        let routed = ApiCapabilityState::repair(&id, &workpiece, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /blooms/{id}/hold` — freeze the bloom's dispatch while the laps
    /// already running finish and journal normally (#4976).
    #[http::route(Post, "/blooms/{id}/hold")]
    fn on_hold(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = ApiCapabilityState::hold(&id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /blooms/{id}/release` — take the brake off and dispatch what the
    /// hold owes, re-derived from the bloom's cursors (#4976).
    #[http::route(Post, "/blooms/{id}/release")]
    fn on_release(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = ApiCapabilityState::release(&id, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `POST /blooms/{id}/answer/{question}` — adopt a signed answer to the
    /// parked question `{question}` names. The question is a path segment
    /// because the signature is bound to it (ADR-0182).
    #[http::route(Post, "/blooms/{id}/answer/{question}")]
    fn on_answer(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
        question: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let question = question.0;
        let routed = state.answer_bloom(&ctx, &id, &question, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `GET /claims` — enumerate the live claim refs and their holders
    /// (ADR-0179). The diagnostic that used to require `git ls-remote`.
    #[http::route(Get, "/claims")]
    fn on_get_claims(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        let routed = ApiCapabilityState::list_claims();
        finish(state, ctx, routed)
    }

    /// `POST /claims/releases` — authorize releasing one orphaned claim ref with
    /// an author signature (ADR-0179).
    #[http::route(Post, "/claims/releases")]
    fn on_post_claim_release(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
    ) -> http::Outcome {
        let routed = state.request_claim_release(&ctx, &ctx.request().body);
        finish(state, ctx, routed)
    }

    /// `GET /claims/releases/{digest}` — read one authorized release's
    /// journal-derived state: pending, or its terminal result.
    #[http::route(Get, "/claims/releases/{digest}")]
    fn on_get_claim_release(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        digest: http::Path<String>,
    ) -> http::Outcome {
        let digest = digest.0;
        let routed = ApiCapabilityState::query_claim_release(&digest);
        finish(state, ctx, routed)
    }

    /// `GET /calibration` — read the measured capability ledger and the
    /// forecast grade beside it (ADR-0184).
    #[http::route(Get, "/calibration")]
    fn on_get_calibration(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        finish(state, ctx, calibration::read())
    }

    /// `GET /journal` — read the durable event journal from the store.
    #[http::route(Get, "/journal")]
    fn on_get_journal(state: &mut ApiCapabilityState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        finish(state, ctx, Routed::ReplayJournal(ReplayJournal))
    }

    /// `GET /artifacts/{digest}` — fetch a content-addressed artifact.
    #[http::route(Get, "/artifacts/{digest}")]
    fn on_get_artifact(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        digest: http::Path<String>,
    ) -> http::Outcome {
        finish(state, ctx, Routed::Get(Get { digest: digest.0 }))
    }

    /// The control core's reply to an admit — the reducer outcome, or an admit
    /// error.
    ///
    /// The one reply kind two different flows produce, so it is the one reply
    /// handler still written by hand. A direct admit route (grant, and the
    /// all-auto seal / supersede fast path) relayed its request, so its
    /// requester is recovered from the deferred-source context; the answer and
    /// seal flows dispatched their terminal admit from a reply handler, so
    /// theirs is held in `pending`. The two stores are mutually exclusive — the
    /// recovery that does not match this reply is a no-op — so calling both is
    /// how one handler serves both without needing to know which flow it is
    /// answering.
    #[handler::manual]
    fn on_admit_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: AdmitResult) {
        let response = admit_response(mail);

        http::answer_deferred(ctx, &response);
        state.answer(ctx, &response);
    }

    /// The control core's reply to a live projection read, to an orphan-claim
    /// release's status read (ADR-0179), or to a calibration read (ADR-0184).
    ///
    /// One `Query` kind answers all three, so which read this is answering is
    /// decided by the reply's own variant rather than by anything the route held
    /// — the relay surfaces no correlation to key a second table on.
    #[http::reply]
    fn on_query_result(
        _state: &mut ApiCapabilityState,
        _ctx: &mut NativeCtx<'_, Manual>,
        mail: QueryResult,
    ) -> HttpServerResponse {
        match mail {
            QueryResult::Release { .. } | QueryResult::ReleaseNotFound => release_status_response(mail),
            QueryResult::Calibration { document } => calibration_response(&document),
            mail => query_response(mail),
        }
    }

    /// The source cap's reply to a claim enumeration (ADR-0179).
    #[http::reply]
    fn on_enumerate_claims_result(
        _state: &mut ApiCapabilityState,
        _ctx: &mut NativeCtx<'_, Manual>,
        mail: EnumerateClaimsResult,
    ) -> HttpServerResponse {
        claims_response(mail)
    }

    /// The store's reply to a journal read.
    #[http::reply]
    fn on_replay_result(
        _state: &mut ApiCapabilityState,
        _ctx: &mut NativeCtx<'_, Manual>,
        mail: ReplayJournalResult,
    ) -> HttpServerResponse {
        journal_response(mail)
    }

    /// The artifacts cap's reply to an artifact fetch.
    #[http::reply]
    fn on_get_result(
        _state: &mut ApiCapabilityState,
        _ctx: &mut NativeCtx<'_, Manual>,
        mail: GetResult,
    ) -> HttpServerResponse {
        artifact_response(mail)
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

    /// The store's reply to an authored configuration's write (ADR-0174). The
    /// authoring route deferred its caller on this, so the reply answers with the
    /// address on a durable write and a `500` on a failed one — a caller never
    /// gets an address it could seal against a row that is not there. The address
    /// is the reply's own echo, so the route holds nothing across the write. The
    /// same echo carries the stored bytes, so a durable write also files the
    /// content in the resolved-configuration cache the pre-seal gate reads
    /// (#4616) — an operator can seal the address the write just handed back
    /// without racing another store read.
    #[http::reply]
    fn on_record_config_result(
        state: &mut ApiCapabilityState,
        _ctx: &mut NativeCtx<'_, Manual>,
        mail: RecordConfigResult,
    ) -> HttpServerResponse {
        config_response(state, mail)
    }

    /// The store's reply to the boot configuration read (#4616). Fills the
    /// resolved-configuration cache the pre-seal gate resolves a draft's sealed
    /// tier policy out of, and marks it ready — a seal arriving before this is
    /// refused rather than gated against content the cap has not read.
    #[handler::manual]
    fn on_load_configs_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: LoadConfigsResult) {
        state.hydrate_configs(ctx, mail);
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
            tracing::warn!(target: "aether_chassis_bloomery::api", %error, "dispatch-description write failed at seal");
        }
    }

    /// A multi-hop chain settled. If its request is still held, the downstream
    /// produced no reply (a dropped or unloaded control core, or a dropped
    /// signing capability) — answer `504` rather than leave the client hung.
    ///
    /// Only the multi-hop hops subscribe here. A deferred route's downstream is
    /// sent on the request's own inherited chain, so a silent peer is answered
    /// by the HTTP server's `502` net and a hung one by its request timeout —
    /// neither reaches this handler (ADR-0154 §3).
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
