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
//!   [`MailId`](aether_data::MailId)'s `correlation_id` keys the held guard in `pending`.
//! - The downstream reply's `sender.correlation_id` is auto-echoed to that same
//!   id (ADR-0042), so a **typed** reply handler recovers the guard via
//!   `ctx.reply_target().correlation_id` and answers through it. The reply
//!   correlation deliberately does *not* go through a `#[fallback]`: a fallback
//!   would widen the actor's accept-set to every kind — including the request-
//!   stream kinds — and the HTTP server would then route each request down the
//!   streaming path instead of delivering a buffered `HttpServerRequest`.
//! - A settlement subscription answers `504` for a request whose downstream
//!   chain settles without ever replying (a dropped or unloaded control core).
//!
//! # The module tree
//!
//! `#[http::router]` collects its route set from one impl block, so every
//! `#[http::route]` method and every typed reply handler stays here, each a
//! two-line delegation to the module that owns the work. [`state`] holds the
//! router state, the ceilings that bound it, and the deferral machinery;
//! [`workpieces`], [`drafts`], [`seal`], [`blooms`] and [`reads`] hold one
//! resource each; [`response`] and [`hex`] hold the shared response
//! constructors and the bloom-id URL codec.

mod blooms;
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
    AdmitResult, LoadConfigsResult, QueryResult, ReplayJournal, ReplayJournalResult, ResolvedConfigs,
};
use aether_http as http;
use aether_kinds::trace::Settled;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

pub use state::ApiCapabilityState;

use blooms::{admit_response, query_response};
use configs::load_configs;
use reads::{artifact_response, journal_response};
use response::{error_response, json};
use state::{Routed, SealVerify, VerifyPending, finish};

use super::BloomeryApiCapability;
use super::dto::WorkpiecesView;
use crate::artifacts::{ArtifactsCapability, Get, GetResult};
use crate::bloomery::load_policy;
use crate::signing::VerifyResult;
use crate::store::{RecordConfigResult, RecordDispatchDescriptionResult, StoreCapability};

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
    /// (`approval-policy.yml`).
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
            authoring: HashMap::new(),
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
        let routed = state.author_config(&ctx, &ctx.request().body);
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

    /// `POST /blooms/{id}/grant` — hand a wedged member more attempts and
    /// resume it on the bloom it already belongs to (#4708).
    #[http::route(Post, "/blooms/{id}/grant")]
    fn on_grant(
        state: &mut ApiCapabilityState,
        ctx: http::Ctx<'_, NativeCtx<'_, Manual>>,
        id: http::Path<String>,
    ) -> http::Outcome {
        let id = id.0;
        let routed = state.grant(&ctx, &id, &ctx.request().body);
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

    /// The store's reply to an authored configuration's write (ADR-0174). The
    /// authoring route deferred its caller on this, so the reply answers with the
    /// address on a durable write and a `500` on a failed one — a caller never
    /// gets an address it could seal against a row that is not there.
    #[handler::manual]
    fn on_record_config_result(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: RecordConfigResult) {
        let error = match mail {
            RecordConfigResult::Ok => None,
            RecordConfigResult::Err { error } => Some(error),
        };
        state.resolve_config_write(ctx, error.as_deref());
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
