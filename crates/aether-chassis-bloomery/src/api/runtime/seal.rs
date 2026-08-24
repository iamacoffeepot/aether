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

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::de::DeserializeOwned;

use aether_actor::Manual;
use aether_bloomery::{
    Admit, ApprovalPolicy, AuthorityDoor, BloomDraft, BloomId, BloomSpec, CommissionStatus, ConfigScopes,
    DependencyError, Digest, Event, Fact, IdempotencyKey, MemberDependency, Membership, ScopeRevision, SpendCeiling,
    Statement, SurfacePattern, WorkpieceId, resolve_member_dependencies, surface_intersection,
};
use aether_data::wire::to_vec;
use aether_http::HttpServerResponse;
use aether_substrate::actor::native::NativeCtx;

use super::commission_reader::{DependencyResolution, TreeAdrs, admit_member};
use super::hex::{self, hex_encode};
use super::response::error_response;
use super::state::{
    ApiCapabilityState, MAX_OPEN_SEALS, MAX_SEAL_DEPENDENCY_LOADS, MAX_SEAL_MEMBERS, PendingCommissionSeal,
    PendingCommissionSealSetup, PendingSeal, PendingSealSetup, PendingVerify, Routed, SealCommissionLoad, SealVerify,
    admit,
};
use crate::api::dto::{MemberProjection, SealRequest};
use crate::bloomery::{
    AdmissionRequest, Decision, Gate, Tier, precheck_statement, projection_digest, verified_statement_approval,
};
use crate::control::ControlCore;
use crate::signing::{SigningCapability, Verify, VerifyResult, authority_bytes};
use crate::store::{LoadCommission, LoadCommissionResult, RecordDispatchDescription, StoreCapability};

/// Which door a completed seal admits through (#4638): a first seal, or a
/// supersession of `predecessor`. Both carry the identical [`BloomSpec`] — the
/// gate, the descriptions, and the deferred-verify path are the same for each,
/// so the door is the only thing that varies.
fn admission_fact(predecessor: Option<BloomId>, spec: BloomSpec, edges: Vec<MemberDependency>) -> Fact {
    if edges.is_empty() {
        match predecessor {
            None => Fact::Seal(spec),
            Some(predecessor) => Fact::Supersede { predecessor, successor: spec },
        }
    } else {
        Fact::GraphSeal { predecessor, spec, edges }
    }
}

impl ApiCapabilityState {
    /// `POST /drafts/{id}/seal` — load each member's commission from the store,
    /// run the pre-seal approve gate, then freeze the draft into a `BloomSpec`
    /// and admit `Fact::Seal` (issue #5048, over the #3583 gate).
    ///
    /// Draft membership still names the workpiece id and the exact scope
    /// digest. Everything the gate used to take from the request —
    /// declared surface, completeness, description, approval — is read from
    /// the stored revision and its approval rows. A caller-supplied
    /// projection of the same digest is ignored.
    pub(super) fn seal_draft(&self, ctx: &NativeCtx<'_, Manual>, id: &str, body: &[u8]) -> Routed {
        let (_, draft) = match self.lookup_draft(id) {
            Ok(found) => found,
            Err(response) => return Routed::Reply(response),
        };
        let request: SealRequest = match parse_optional_body(body) {
            Ok(request) => request,
            Err(response) => return Routed::Reply(response),
        };
        self.begin_store_seal(ctx, draft, None, request.idempotency_key, request.edges)
    }

    /// Load every draft member's commission, then [`gate_and_admit`](ApiCapabilityState::gate_and_admit). Both
    /// admission doors share this so a supersession cannot keep a writable
    /// projection after seal lost one.
    pub(super) fn begin_store_seal(
        &self,
        ctx: &NativeCtx<'_, Manual>,
        draft: BloomDraft,
        predecessor: Option<BloomId>,
        idempotency_key: Option<String>,
        edges: Vec<MemberDependency>,
    ) -> Routed {
        if draft.proposals.len() > MAX_SEAL_MEMBERS {
            return Routed::Reply(error_response(
                422,
                &format!("draft has {} members; a seal is capped at {MAX_SEAL_MEMBERS}", draft.proposals.len()),
            ));
        }
        if draft.proposals.is_empty() {
            return self.gate_and_admit(ctx, draft, predecessor, &[], BTreeMap::new(), idempotency_key, &edges);
        }
        if self.commission_seals.len() >= MAX_OPEN_SEALS {
            return Routed::Reply(error_response(429, "outstanding-seal budget exhausted"));
        }
        let mut loads = Vec::with_capacity(draft.proposals.len());
        for proposal in &draft.proposals {
            let correlation =
                self.send_tracked(ctx.actor::<StoreCapability>(), &LoadCommission { id: proposal.workpiece.0.clone() });
            loads.push((correlation, proposal.workpiece.clone()));
        }
        Routed::DeferredCommissionSeal(Box::new(PendingCommissionSealSetup {
            draft,
            predecessor,
            idempotency_key,
            edges,
            loads,
        }))
    }

    /// Join one `LoadCommission` into its held seal. When the member round
    /// completes, a second round loads any declared non-member dependencies;
    /// the last load of that round materializes store projections and continues
    /// into the gate.
    pub(super) fn resolve_seal_commission_load(&mut self, ctx: &NativeCtx<'_, Manual>, result: LoadCommissionResult) {
        let correlation = ctx.reply_target().correlation_id;
        let Some(SealCommissionLoad { seal, workpiece }) = self.seal_commission_loads.remove(&correlation) else {
            return;
        };
        let Some(pending) = self.commission_seals.get_mut(&seal) else {
            return;
        };
        pending.loaded.insert(workpiece.0, result);
        pending.remaining -= 1;
        if pending.remaining > 0 {
            return;
        }
        match self.enqueue_seal_dependency_loads(ctx, seal) {
            DependencyLoadRound::Dispatched | DependencyLoadRound::Refused => return,
            DependencyLoadRound::Ready => {}
        }
        let PendingCommissionSeal { inbound, draft, predecessor, idempotency_key, edges, mut loaded, .. } =
            self.commission_seals.remove(&seal).expect("seal present; just mutated it");
        match self.materialize_and_admit(ctx, draft, predecessor, idempotency_key, edges, &mut loaded) {
            Routed::Reply(response) => {
                inbound.reply(&response);
            }
            Routed::Admit(request) => {
                let correlation = self.send_tracked(ctx.actor::<ControlCore>(), &request);
                self.pending.insert(correlation, inbound);
            }
            Routed::DeferredSeal(setup) => self.begin_deferred_seal(inbound, *setup),
            _ => {
                inbound.reply(&error_response(500, "unexpected seal disposition after commission load"));
            }
        }
    }

    /// When every member load has joined, dispatch one `LoadCommission` per
    /// distinct declared dependency that is not already a loaded key. Empty
    /// needed-set means the second round is done (or was never needed). Over
    /// the ceiling is a fail-closed `422`.
    fn enqueue_seal_dependency_loads(&mut self, ctx: &NativeCtx<'_, Manual>, seal: u64) -> DependencyLoadRound {
        let collected = {
            let Some(pending) = self.commission_seals.get(&seal) else {
                return DependencyLoadRound::Ready;
            };
            unloaded_dependency_ids(&pending.draft, &pending.loaded)
        };
        let needed = match collected {
            UnloadedDependencies::OverCap => {
                self.fail_commission_seal(
                    seal,
                    422,
                    &format!(
                        "seal names more than {MAX_SEAL_DEPENDENCY_LOADS} distinct non-member dependencies; a seal is capped at {MAX_SEAL_DEPENDENCY_LOADS}"
                    ),
                );
                return DependencyLoadRound::Refused;
            }
            UnloadedDependencies::Ids(needed) => needed,
        };
        if needed.is_empty() {
            return DependencyLoadRound::Ready;
        }
        let mut loads = Vec::with_capacity(needed.len());
        for id in needed {
            let correlation = self.send_tracked(ctx.actor::<StoreCapability>(), &LoadCommission { id: id.clone() });
            loads.push((correlation, WorkpieceId(id)));
        }
        let remaining = loads.len();
        let pending = self.commission_seals.get_mut(&seal).expect("seal present; just read it");
        pending.remaining = remaining;
        for (correlation, workpiece) in loads {
            self.seal_commission_loads.insert(correlation, SealCommissionLoad { seal, workpiece });
        }
        DependencyLoadRound::Dispatched
    }

    /// Tear down a store-backed seal whose commission load never answered.
    pub(super) fn fail_commission_seal(&mut self, seal: u64, status: u16, message: &str) {
        let Some(PendingCommissionSeal { inbound, .. }) = self.commission_seals.remove(&seal) else {
            return;
        };
        self.seal_commission_loads.retain(|_, load| load.seal != seal);
        inbound.reply(&error_response(status, message));
    }

    pub(super) fn begin_deferred_seal(&mut self, inbound: aether_substrate::InboundMail, setup: PendingSealSetup) {
        let PendingSealSetup { gated, predecessor, descriptions, idempotency_key, edges, verifications } = setup;
        let seal = self.next_seal;
        self.next_seal += 1;
        let remaining = verifications.len();
        for verify in verifications {
            let PendingVerify { correlation, member_index, statement, .. } = verify;
            self.seal_verifications.insert(correlation, SealVerify { seal, member_index, statement });
        }
        self.seals
            .insert(seal, PendingSeal { inbound, predecessor, gated, descriptions, idempotency_key, edges, remaining });
    }

    fn materialize_and_admit(
        &self,
        ctx: &NativeCtx<'_, Manual>,
        draft: BloomDraft,
        predecessor: Option<BloomId>,
        idempotency_key: Option<String>,
        mut edges: Vec<MemberDependency>,
        loaded: &mut BTreeMap<String, LoadCommissionResult>,
    ) -> Routed {
        let resolution = seal_dependency_resolution(&draft, loaded);
        let mut projections = Vec::with_capacity(draft.proposals.len());
        let mut descriptions = BTreeMap::new();
        for proposal in &draft.proposals {
            let Some(result) = loaded.remove(&proposal.workpiece.0) else {
                return Routed::Reply(error_response(
                    500,
                    &format!("commission load for {} was not joined", proposal.workpiece.0),
                ));
            };
            match admit_member(proposal.scope_revision, result, &TreeAdrs::working_tree(), &resolution) {
                Ok(admitted) => {
                    descriptions.insert(admitted.workpiece.id.0.clone(), admitted.description);
                    edges.extend(admitted.edges);
                    projections.push(admitted.projection);
                }
                Err(error) => return Routed::Reply(error.response()),
            }
        }
        self.gate_and_admit(ctx, draft, predecessor, &projections, descriptions, idempotency_key, &edges)
    }

    /// The gate-then-admit core both doors share (#4638): resolve every
    /// membership through [`Gate::evaluate`], then seal — synchronously when
    /// every member resolves `auto`, or deferred behind one `Verify` per
    /// above-`auto` member.
    ///
    /// Both doors resolve the draft's own tier policy first
    /// ([`gate_policy`](ApiCapabilityState::gate_policy)), so a supersession is
    /// admitted under the successor's sealed policy rather than under whatever
    /// the predecessor ran or the host booted with.
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
        declared_edges: &[MemberDependency],
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
        if let Some(member) = draft.proposals.iter().find(|member| member.configs.address::<SpendCeiling>().is_some()) {
            return Routed::Reply(error_response(
                422,
                &format!(
                    "member {} seals its own spend ceiling; the ceiling is bloom-wide only, seal fails closed",
                    member.workpiece.0
                ),
            ));
        }
        // Which policy this draft is admitted under — the one it seals, or the
        // host's file fallback. Fail closed on anything ambiguous.
        let policy = match self.gate_policy(&draft) {
            Ok(policy) => policy,
            Err(response) => return Routed::Reply(response),
        };
        let gate = Gate::new(&policy);
        // Pass 1: resolve every membership synchronously (fail-closed 422 on any
        // shortfall, before any signing dispatch).
        let (sealed_proposals, pending_verifications) =
            match resolve_seal_memberships(&gate, &policy, &draft.proposals, projections) {
                Ok(resolved) => resolved,
                Err(response) => return Routed::Reply(response),
            };
        let edges = match resolve_seal_graph(&sealed_proposals, projections, declared_edges) {
            Ok(edges) => edges,
            Err(response) => return Routed::Reply(response),
        };
        // Every member's projection resolved, so the door now holds every
        // declared surface at once — the one place that ever does. Journal the
        // cross-member overlaps before the seal admits (#4931).
        Self::journal_surface_overlaps(ctx, &draft.proposals, projections);
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
            return admit(&Event {
                idempotency_key: IdempotencyKey(key),
                fact: admission_fact(predecessor, spec, edges),
            });
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
        for (member_index, scope_revision, statement, tier) in pending_verifications {
            let statement_bytes = match to_vec(&statement) {
                Ok(bytes) => bytes,
                Err(error) => return Routed::Reply(error_response(500, &format!("statement encode failed: {error}"))),
            };
            encoded.push((member_index, scope_revision, statement, statement_bytes, tier));
        }
        let mut verifications = Vec::with_capacity(encoded.len());
        for (member_index, scope_revision, statement, statement_bytes, tier) in encoded {
            // Bound to the member's own scope revision, which this path already
            // holds and derives from the gated draft rather than from the
            // envelope — so a statement signed for another revision has no
            // verifying signature here (ADR-0182).
            let correlation = self.send_tracked(
                ctx.actor::<SigningCapability>(),
                &Verify {
                    statement: statement_bytes,
                    authority: authority_bytes(AuthorityDoor::Approve, scope_revision),
                    // The tier this member's own declared surface resolved at,
                    // so the signing cap refuses a signer authorized lower
                    // however good the signature is (#5324).
                    required_tier: Some(tier),
                },
            );
            verifications.push(PendingVerify { correlation, member_index, statement });
        }
        Routed::DeferredSeal(Box::new(PendingSealSetup {
            gated,
            predecessor,
            descriptions,
            idempotency_key,
            edges,
            verifications,
        }))
    }

    /// The tier policy this draft's members are admitted under (#4616): the
    /// `aether.bloomery.approval_policy` it seals bloom-wide, or the host's
    /// file-loaded fallback when it seals none.
    ///
    /// Bloom-wide only, and a member-scoped entry is **refused** rather than
    /// resolved or ignored. Resolving it would let a member seal the policy that
    /// decides whether that member may be admitted — self-authorization at the
    /// one gate that exists to prevent it — and ignoring it would leave a sealed
    /// configuration nothing reads, which is exactly the attested-but-inert
    /// divergence ADR-0174 removes.
    ///
    /// Every other branch fails closed too. An unready cache cannot tell an
    /// unsealed policy from unfetched content, so it refuses instead of falling
    /// back. A sealed address that is missing, misfiled, or undecodable refuses
    /// rather than defaulting past it: the bloom would otherwise be admitted at a
    /// tier its own receipt contradicts.
    fn gate_policy(&self, draft: &BloomDraft) -> Result<ApprovalPolicy, HttpServerResponse> {
        if let Some(member) = draft.proposals.iter().find(|member| member.configs.address::<ApprovalPolicy>().is_some())
        {
            return Err(error_response(
                422,
                &format!(
                    "member {} seals its own approval policy; the tier policy is bloom-wide only, seal fails closed",
                    member.workpiece.0
                ),
            ));
        }
        if !self.configs_ready {
            return Err(error_response(422, "configuration set not yet read; seal fails closed"));
        }

        match self.configs.resolve::<ApprovalPolicy>(ConfigScopes::bloom_wide(&draft.configs)) {
            Ok(Some(sealed)) => Ok(sealed),
            Ok(None) => self
                .file_policy
                .clone()
                .ok_or_else(|| error_response(422, "approval policy unavailable; seal fails closed")),
            Err(error) => Err(error_response(422, &format!("sealed approval policy unresolvable: {error}"))),
        }
    }

    /// Admit one [`Fact::SurfaceOverlap`] per pair of this seal's members whose
    /// declared surfaces intersect (#4931).
    ///
    /// Advisory throughout: fire-and-forget to the control core, for the reason
    /// [`persist_descriptions`](ApiCapabilityState::persist_descriptions) is —
    /// a record *about* an admission must not be able to fail the admission it
    /// describes. The seal's own admit follows on its own path and is unaffected
    /// by anything here, including an overlap the reducer refuses as a duplicate.
    ///
    /// The idempotency key is the fact's own content digest, so an operator who
    /// re-POSTs the same seal records each overlap once rather than once per
    /// attempt. That is also what lets this run once here, ahead of the
    /// synchronous and deferred admits alike, instead of at each of the two
    /// places a seal finally lands: the warning names the membership the door
    /// was handed, and neither depends on that seal reaching the reducer first
    /// nor on it being admitted at all. The cost is an orphan warning when the
    /// seal is later refused for an unverified signature — the same harmless
    /// orphan a description row leaves, and the retry that fixes the signature
    /// dedups onto this row rather than filing a second.
    fn journal_surface_overlaps(ctx: &NativeCtx<'_, Manual>, members: &[Membership], projections: &[MemberProjection]) {
        for fact in surface_overlaps(members, projections) {
            // Advisory, so an encode fault costs the warning and not the seal —
            // the one path here that must never become a refusal.
            let Some(admit) = overlap_admit(fact) else {
                tracing::warn!(
                    target: "aether_chassis_bloomery::api",
                    "surface-overlap warning did not encode; seal proceeds unwarned"
                );
                continue;
            };
            ctx.actor::<ControlCore>().send_detached(&admit);
        }
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
            if description.trim().is_empty() {
                continue;
            }
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
        let Some(SealVerify { seal, member_index, statement, .. }) = self.seal_verifications.remove(&correlation)
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
                let subject = pending.gated.proposals[member_index].subject();
                pending.gated.proposals[member_index].approval = verified_statement_approval(subject, &statement);
                pending.remaining -= 1;
                if pending.remaining > 0 {
                    return;
                }
                // Last verification: seal the fully-approved draft and admit,
                // deferring on the reducer reply exactly as the synchronous path.
                let PendingSeal { inbound, predecessor, gated, descriptions, idempotency_key, edges, .. } =
                    self.seals.remove(&seal).expect("seal present; just mutated it");
                let spec = gated.seal();
                Self::persist_descriptions(ctx, &spec, &descriptions);
                let key = idempotency_key.unwrap_or_else(|| hex_encode(spec.id().0.as_bytes()));
                match to_vec(&Event {
                    idempotency_key: IdempotencyKey(key),
                    fact: admission_fact(predecessor, spec, edges),
                }) {
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
            VerifyResult::BelowTier { required, ceiling } => {
                self.fail_seal(
                    seal,
                    422,
                    &format!(
                        "an above-auto member's declared surface resolves {required:?} tier, and its statement is \
                         signed by a key the signing allowlist authorizes only to {ceiling:?}; seal fails closed"
                    ),
                );
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
/// index, scope-revision digest, signed statement, and the tier its declared
/// surface resolved at — Pass 1's carry into Pass 2.
///
/// The tier rides along because Pass 1 is the only place that resolves it and
/// Pass 2 is the only place that can enforce it: the api cap holds no key
/// material, so "may this signer approve this high" is a question only
/// `aether.signing` can answer, and it can only answer the question it is
/// handed (#5324).
type PendingVerification = (usize, Digest, Statement, Tier);

/// Build the gate request from a stored membership and its projection, then
/// bind `projection_digest` over the fields the gate will evaluate.
fn admission_request(proposal: &Membership, projection: &MemberProjection) -> AdmissionRequest {
    let mut admission = AdmissionRequest {
        subject: proposal.subject(),
        declared_surface: projection.declared_surface.clone(),
        declared_crates: projection.declared_crates.clone(),
        completeness: projection.completeness,
        adr_touch: projection.adr_touch,
        pre_approved: projection.pre_approved,
        projection_digest: Digest::default(),
    };
    admission.projection_digest = projection_digest(&admission);
    admission
}

/// Pass 1 of a seal: resolve every draft membership synchronously against the
/// `gate`. An auto member is gate-formed in place; an above-auto member has its
/// signed statement pre-checked and is queued (its `(index, scope_revision,
/// statement)`) for the deferred `aether.signing` verify. Any missing projection,
/// glob outside the surface grammar, `Incomplete` verdict, missing statement, or
/// failing pre-check returns the fail-closed `422` response instead — resolved
/// before any signing dispatch.
///
/// Extracted from [`seal_draft`](ApiCapabilityState::seal_draft) so that hot
/// path stays under the line ceiling; the two returned vectors are its Pass-2
/// input (the gated proposals and the above-auto members still to verify).
fn resolve_seal_memberships(
    gate: &Gate<'_>,
    policy: &ApprovalPolicy,
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
        // A glob outside the grammar is not an empty surface. Skipping it would
        // admit the member, derive no overlap edges, and dispatch it beside the
        // peers it actually shares files with. `--pre-approved` waives the tier,
        // not this check — the grammar is decided before the gate runs.
        if let Some(glob) = projection.declared_surface.iter().find(|glob| SurfacePattern::parse(glob).is_none()) {
            return Err(error_response(
                422,
                &format!("member {member} declared surface {glob:?} is outside the surface grammar; seal fails closed"),
            ));
        }
        // A file-granular declaration is a forecast, and the forecast is wrong
        // often enough to cost blooms: the plan lists the files it mentions,
        // the work needs one more sibling in the same crate, and the member is
        // superseded for a glob. The policy already maintains the list of files
        // special enough to name, so it is what decides granularity too — the
        // same owner-signed table this door already consults for tier. Runs
        // after the grammar check, so it only ever sees parseable globs.
        if let Some(glob) = policy.unnamed_file_entries(&projection.declared_surface).first() {
            return Err(error_response(
                422,
                &format!(
                    "member {member} declared surface {glob:?} names one file and no approval-policy rule names \
                     that file; widen it to a crate glob such as crates/<crate>/src/**; seal fails closed"
                ),
            ));
        }
        // The digest binds the approval to the fields the gate evaluated, named
        // by `projection_digest` — not the transport DTO — so a field appended to
        // `MemberProjection` re-keys nothing unless it is also fed to the gate.
        let admission = admission_request(proposal, projection);
        match gate.evaluate(&admission) {
            Decision::AutoApproved(approval) => {
                let mut sealed = proposal.clone();
                sealed.approval = approval;
                sealed_proposals.push(sealed);
            }
            Decision::Incomplete { reason, refusal } => {
                return Err(error_response(422, &format!("member {member} is incomplete: {reason:?} ({refusal})")));
            }
            Decision::RequiresStatement(tier) => {
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
                            "member {member} stored approval does not satisfy the signer policy; seal fails closed"
                        ),
                    ));
                };
                if let Err(rejected) = precheck_statement(proposal.scope_revision, statement) {
                    return Err(error_response(
                        422,
                        &format!(
                            "member {member} stored approval failed signer policy: {rejected:?}; seal fails closed"
                        ),
                    ));
                }
                sealed_proposals.push(proposal.clone());
                pending_verifications.push((index, proposal.scope_revision, statement.clone(), tier));
            }
        }
    }
    Ok((sealed_proposals, pending_verifications))
}

/// Resolve the seal's member-dependency graph (ADR-0196 / ADR-0204).
///
/// Cycle detection still walks the union of declared edges and one ordering
/// edge per overlapping declared-surface pair. What the door journals — and
/// what construct dispatch waits on — is the **declared** subset. A derived
/// overlap is not a dispatch gate: two members that share a glob and name no
/// edge both enter Construct at seal. A cycle, an edge naming a non-member,
/// or a member with no matching projection is a fail-closed `422`. A missing
/// projection is a malformed request, not an edgeless member: dropping it
/// would derive a graph that pretends the member was never there. The door
/// matches projections by `{workpiece, scope_revision}` and leaves order
/// independence to the resolver, so a permuted request of the same member
/// set journals the graph its canonical [`BloomSpec`] implies.
fn resolve_seal_graph(
    members: &[Membership],
    projections: &[MemberProjection],
    declared: &[MemberDependency],
) -> Result<Vec<MemberDependency>, HttpServerResponse> {
    let mut listed = Vec::with_capacity(members.len());
    let mut matched = Vec::with_capacity(members.len());
    for member in members {
        let Some(projection) = projections.iter().find(|projection| {
            projection.workpiece == member.workpiece && projection.scope_revision == member.scope_revision
        }) else {
            return Err(error_response(
                422,
                &format!("member {} has no scope projection; seal fails closed", member.workpiece.0),
            ));
        };
        listed.push((member.workpiece.clone(), projection.declared_surface.as_slice()));
        matched.push(projection);
    }

    // Declared edges and read-derived ones go into the same argument, so the
    // cycle check covers both: a read that closes a loop with an authored edge
    // has to refuse at the door, not deadlock the line.
    let mut authored: Vec<MemberDependency> = declared.to_vec();
    authored.extend(read_ordering_edges(&matched));

    match resolve_member_dependencies(&listed, &authored) {
        Ok(resolved) => Ok(resolved.declared),
        Err(DependencyError::UnknownWorkpiece(workpiece)) => Err(error_response(
            422,
            &format!("edge names workpiece {} which is not a member of this bloom", workpiece.0),
        )),
        Err(DependencyError::Cycle(cycle)) => {
            let named = cycle.iter().map(|workpiece| workpiece.0.as_str()).collect::<Vec<_>>().join(" -> ");
            Err(error_response(422, &format!("cyclic member dependencies: {named}")))
        }
    }
}

/// The ordering a member's declared reads earn against its co-members
/// (ADR-0204 / #5258).
///
/// Conditional by construction, which is the entire difference between this
/// and an authored `## Depends on` line: an edge appears only where a reader's
/// declared crate is one a co-member declared it will *change*. Declare a read
/// on a crate nobody in this seal is changing and it costs nothing — no edge,
/// no serialization, no wait. An authored edge pays that cost whether or not
/// the write ever happens, which is why reads could not be expressed as one.
///
/// Read against `declared_crates` rather than `declared_surface` because a
/// crate-declared member's surface is its blast radius, not its intent (FIX-7 /
/// ADR-0204): the closure admits every crate that depends on the declared ones,
/// so ordering on the surface would put every reader behind every member whose
/// closure happened to reach the crate — which is the over-serialization the
/// conditional form exists to remove. A glob-declared member declares no
/// crates and so is never a writer here; that is honest rather than a gap,
/// because a glob surface states paths and this question is about crates.
///
/// A member never orders behind itself, and the edge set is deduplicated by
/// `resolve_member_dependencies`, which also cycle-checks it.
fn read_ordering_edges(projections: &[&MemberProjection]) -> Vec<MemberDependency> {
    let mut edges = Vec::new();
    for reader in projections {
        for writer in projections {
            if reader.workpiece == writer.workpiece {
                continue;
            }
            if reader.declared_reads.iter().any(|read| writer.declared_crates.contains(read)) {
                edges.push(MemberDependency { member: reader.workpiece.clone(), depends_on: writer.workpiece.clone() });
            }
        }
    }
    edges
}

/// Every pair of this seal's members whose declared surfaces intersect, as the
/// facts that record them (#4931).
///
/// The scan lives at the door because only the door can run it: a declared
/// surface rides the seal request rather than the sealed spec, so by the time
/// the reducer holds the membership the surfaces are gone. Both admission doors
/// route through [`gate_and_admit`](ApiCapabilityState::gate_and_admit), so a
/// supersession's fresh membership is scanned exactly as a first seal's is.
///
/// Pairs are drawn from the members forward, so each unordered pair is reported
/// once and no member is ever compared against itself. The globs come from
/// [`surface_intersection`], the same set algebra the tier policy resolves a
/// rule against — the overlap the door names and the subtree the policy matches
/// are one definition, not two.
///
/// Quadratic in members, over a membership `gate_and_admit` has already capped
/// at [`MAX_SEAL_MEMBERS`], and quadratic again in each member's globs. A
/// realistic bloom is a handful of members declaring a handful of globs each, so
/// the product is small; the cap is what keeps the worst case bounded rather
/// than the shape.
fn surface_overlaps(members: &[Membership], projections: &[MemberProjection]) -> Vec<Fact> {
    // Each member paired with the surface it was gated on. Pass 1 has already
    // refused a member with no projection, so a miss here cannot happen on the
    // path that calls this; skipping rather than assuming keeps the advisory
    // scan from being the thing that panics a seal.
    let surfaces: Vec<(&WorkpieceId, &[String])> = members
        .iter()
        .filter_map(|member| {
            projections
                .iter()
                .find(|projection| {
                    projection.workpiece == member.workpiece && projection.scope_revision == member.scope_revision
                })
                .map(|projection| (&member.workpiece, projection.declared_surface.as_slice()))
        })
        .collect();

    let mut overlaps = Vec::new();
    for (index, (workpiece, surface)) in surfaces.iter().enumerate() {
        for (peer, peer_surface) in &surfaces[index + 1..] {
            let intersection = surface_intersection(surface, peer_surface);
            if !intersection.is_empty() {
                overlaps
                    .push(Fact::SurfaceOverlap { members: vec![(*workpiece).clone(), (*peer).clone()], intersection });
            }
        }
    }
    overlaps
}

/// One overlap warning as the admit that journals it, or [`None`] when it will
/// not encode.
///
/// The idempotency key is the fact's own content digest, so the same observed
/// overlap dedups across seal attempts, while two overlaps from one seal — a
/// different pair, or the same pair at a different intersection — stay distinct
/// rows rather than collapsing into whichever arrived first.
fn overlap_admit(fact: Fact) -> Option<Admit> {
    let key = hex_encode(Digest::of_wire_bytes(&to_vec(&fact).ok()?).as_bytes());

    Some(Admit { event: to_vec(&Event { idempotency_key: IdempotencyKey(key), fact }).ok()? })
}

/// Parse a possibly-empty request body into a `Default` body type: an empty
/// body is the default, a non-empty one is parsed, a malformed one is a `400`.
fn parse_optional_body<T: DeserializeOwned + Default>(body: &[u8]) -> Result<T, HttpServerResponse> {
    if body.is_empty() {
        return Ok(T::default());
    }
    hex::from_slice(body).map_err(|error| error_response(400, &format!("invalid request body: {error}")))
}

enum DependencyLoadRound {
    Dispatched,
    Ready,
    Refused,
}

enum UnloadedDependencies {
    Ids(Vec<String>),
    OverCap,
}

/// Distinct declared dependency ids across this seal's members that have not
/// already joined `loaded`. Best-effort: a member whose current bytes will not
/// decode contributes no ids and is refused later by `admit_loaded`. Over the
/// ceiling is `OverCap` so the door can refuse `422` without feeding a
/// half-decoded revision into the resolution.
fn unloaded_dependency_ids(
    draft: &BloomDraft,
    loaded: &BTreeMap<String, LoadCommissionResult>,
) -> UnloadedDependencies {
    let mut needed = BTreeSet::new();
    for proposal in &draft.proposals {
        let Some(LoadCommissionResult::Ok { current: Some(bytes), .. }) = loaded.get(&proposal.workpiece.0) else {
            continue;
        };
        let Ok(revision) = ScopeRevision::from_canonical(bytes) else {
            continue;
        };
        for depends_on in revision.dependencies {
            if loaded.contains_key(&depends_on.0) {
                continue;
            }
            needed.insert(depends_on.0);
            if needed.len() > MAX_SEAL_DEPENDENCY_LOADS {
                return UnloadedDependencies::OverCap;
            }
        }
    }
    UnloadedDependencies::Ids(needed.into_iter().collect())
}

/// Co-sealed member ids plus the parsed status of each loaded non-member.
fn seal_dependency_resolution(
    draft: &BloomDraft,
    loaded: &BTreeMap<String, LoadCommissionResult>,
) -> DependencyResolution {
    let members: BTreeSet<WorkpieceId> = draft.proposals.iter().map(|proposal| proposal.workpiece.clone()).collect();
    let statuses: Vec<(WorkpieceId, CommissionStatus)> = loaded
        .iter()
        .filter_map(|(id, result)| {
            let workpiece = WorkpieceId(id.clone());
            if members.contains(&workpiece) {
                return None;
            }
            match result {
                LoadCommissionResult::Ok { status, .. } => {
                    CommissionStatus::parse(status).map(|parsed| (workpiece, parsed))
                }
                _ => None,
            }
        })
        .collect();
    DependencyResolution::new(members, statuses)
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{
        ApprovalPolicy, BloomDraft, BloomId, ConfigRegistry, Digest, Evidence, EvidenceKind, Fact, MemberDependency,
        Membership, Observation, Provenance, Statement, Tier, WorkpieceId,
    };

    use super::{
        MemberProjection, SealRequest, admission_fact, admission_request, parse_optional_body, resolve_seal_graph,
        resolve_seal_memberships, surface_overlaps,
    };
    use crate::bloomery::{AdrTouch, Completeness, Decision, Gate};

    /// A sealed member at `revision`. Only its workpiece and scope revision are
    /// read by the overlap scan; the rest is what a member is made of.
    fn member(workpiece: &str, revision: u8) -> Membership {
        Membership {
            workpiece: WorkpieceId(workpiece.to_owned()),
            scope_revision: Digest::from_bytes([revision; 32]),
            configs: ConfigRegistry::default(),
            approval: Evidence {
                subject: Digest::from_bytes([revision; 32]),
                kind: EvidenceKind::Approval,
                detail: Digest::from_bytes([revision; 32]),
            },
        }
    }

    /// The projection that member arrived with, declaring `surface`.
    fn projection(workpiece: &str, revision: u8, surface: &[&str]) -> MemberProjection {
        MemberProjection {
            workpiece: WorkpieceId(workpiece.to_owned()),
            scope_revision: Digest::from_bytes([revision; 32]),
            declared_surface: surface.iter().map(|glob| (*glob).to_owned()).collect(),
            declared_crates: Vec::new(),
            declared_reads: Vec::new(),
            completeness: Completeness {
                has_problem_statement: true,
                has_design_notes: true,
                has_implementation_plan: true,
                referenced_adr_prs_merged: true,
                model_routing_count: 1,
                blocked: false,
                declared_surface_fresh: true,
                dependencies_all_closed: true,
                umbrella_integrity: true,
            },
            adr_touch: AdrTouch::None,
            pre_approved: false,
            signed_statement: None,
        }
    }

    /// The pair and intersection one warning names.
    fn overlap(fact: &Fact) -> (&[WorkpieceId], &[String]) {
        match fact {
            Fact::SurfaceOverlap { members, intersection } => (members, intersection),
            other => panic!("expected a surface overlap, got {other:?}"),
        }
    }

    #[test]
    fn an_overlapping_pair_warns_once_and_a_disjoint_one_not_at_all() {
        // Three things the scan's shape has to get right at once. Pairs are
        // drawn forward from each member, which keeps a member off its own list
        // — every surface trivially intersects itself, so a self-comparison
        // would warn on every seal that has ever happened — and reports a
        // genuinely overlapping pair once rather than once from each side. The
        // emptiness guard is the third: without it the disjoint member below
        // files two warnings naming no paths.
        let members = [member("wp-a", 1), member("wp-b", 2), member("wp-c", 3)];
        let projections = [
            projection("wp-a", 1, &["crates/aether-bloomery/**"]),
            projection("wp-b", 2, &["crates/aether-bloomery/src/values/price.rs"]),
            projection("wp-c", 3, &["docs/guide/**"]),
        ];

        let overlaps = surface_overlaps(&members, &projections);

        assert_eq!(overlaps.len(), 1, "one overlapping pair is one warning, got {overlaps:?}");
        let (pair, intersection) = overlap(&overlaps[0]);
        assert_eq!(pair, [WorkpieceId("wp-a".to_owned()), WorkpieceId("wp-b".to_owned())]);
        assert_eq!(intersection, ["crates/aether-bloomery/src/values/price.rs".to_owned()]);
    }

    #[test]
    fn each_member_is_scanned_on_the_surface_its_own_projection_declared() {
        // The operator sends projections in whatever order they like, so the
        // scan matches on {workpiece, scope_revision} rather than on position.
        // Pairing by index here hands wp-a the surface wp-c declared and wp-b
        // the one wp-a did, and warns about the wrong pair entirely.
        let members = [member("wp-a", 1), member("wp-b", 2), member("wp-c", 3)];
        let projections = [
            projection("wp-c", 3, &["crates/aether-fs/src/lib.rs"]),
            projection("wp-a", 1, &["crates/aether-fs/**"]),
            projection("wp-b", 2, &["crates/aether-http/**"]),
        ];

        let overlaps = surface_overlaps(&members, &projections);

        assert_eq!(overlaps.len(), 1, "one overlapping pair is one warning, got {overlaps:?}");
        let (pair, intersection) = overlap(&overlaps[0]);
        assert_eq!(pair, [WorkpieceId("wp-a".to_owned()), WorkpieceId("wp-c".to_owned())]);
        assert_eq!(intersection, ["crates/aether-fs/src/lib.rs".to_owned()]);
    }

    #[test]
    fn no_predecessor_admits_through_the_seal_door() {
        // Tripwire on the door selection (#4638). Both doors carry an identical
        // spec through an identical gate, so nothing downstream would notice a
        // swap — but a first seal admitted as a supersession names a predecessor
        // that does not exist, and a supersession admitted as a seal is refused
        // for the very active bloom it was meant to replace.
        let spec = BloomDraft::default().seal();

        assert!(
            matches!(admission_fact(None, spec, Vec::new()), Fact::Seal(_)),
            "an unset predecessor is a first seal"
        );
    }

    #[test]
    fn a_predecessor_admits_through_the_supersede_door() {
        let predecessor = BloomId(Digest::from_bytes([3; 32]));
        let spec = BloomDraft::default().seal();

        match admission_fact(Some(predecessor), spec, Vec::new()) {
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
    fn seal_request_ignores_caller_projections_and_descriptions() {
        // #5048: the compatibility boundary is one cut. A body that still
        // carries the retired fields must parse, and those fields must not
        // become a second writable representation of scope or approval.
        let parsed: SealRequest = parse_optional_body(
            br#"{"projections":[{"workpiece":"wp-a","scope_revision":[1,2,3]}],"descriptions":{"wp-a":"override"}}"#,
        )
        .expect("legacy fields are ignored, not required");
        assert!(parsed.idempotency_key.is_none());
        assert!(parsed.edges.is_empty());
    }

    #[test]
    fn seal_request_edges_default_empty_and_parse() {
        // An absent `edges` field must still seal — the `#[serde(default)]`
        // guard. Dropping it would 400 every edgeless client.
        let none: SealRequest = parse_optional_body(br#"{"idempotency_key":"k"}"#).expect("no edges still parses");
        assert!(none.edges.is_empty(), "an absent edges list defaults empty rather than erroring");

        let with: SealRequest = parse_optional_body(br#"{"edges":[{"member":"issue-B","depends_on":"issue-A"}]}"#)
            .expect("an edges list parses");
        assert_eq!(
            with.edges,
            [MemberDependency {
                member: WorkpieceId("issue-B".to_owned()),
                depends_on: WorkpieceId("issue-A".to_owned())
            }]
        );
    }

    #[test]
    fn a_nonempty_graph_admits_as_graph_seal() {
        // The edgeless path stays `Fact::Seal` so today's event bytes do not
        // move. A non-empty graph has to ride a new variant or it cannot reach
        // the reducer — swapping those two would reshape every historical seal.
        let spec = BloomDraft::default().seal();
        let edges = vec![MemberDependency {
            member: WorkpieceId("issue-B".to_owned()),
            depends_on: WorkpieceId("issue-A".to_owned()),
        }];
        match admission_fact(None, spec, edges.clone()) {
            Fact::GraphSeal { predecessor: None, edges: named, .. } => assert_eq!(named, edges),
            other => panic!("a non-empty graph must admit GraphSeal, got {other:?}"),
        }
    }

    #[test]
    fn the_door_derives_an_ordering_edge_from_overlapping_surfaces() {
        // Surfaces ride the request, not the spec. Overlap still feeds cycle
        // detection, but ADR-0204 demotes it: a derived edge is not a dispatch
        // gate. Pre-fix, this test required the later member to wait.
        let members = [member("wp-a", 1), member("wp-b", 2)];
        let projections = [
            projection("wp-a", 1, &["crates/aether-bloomery/**"]),
            projection("wp-b", 2, &["crates/aether-bloomery/src/lib.rs"]),
        ];

        let edges = resolve_seal_graph(&members, &projections, &[]).expect("acyclic overlap");
        assert!(edges.is_empty(), "an overlap without a declared edge journals no dispatch gate: {edges:?}");
    }

    #[test]
    fn permuted_request_order_resolves_the_same_graph_as_the_canonical_spec() {
        // The door matches projections by {workpiece, scope_revision} and
        // hands the resolver the request's listing. BloomDraft::seal then
        // sorts the same members into the spec. If derivation followed
        // request order, two permutations would share a BloomId and
        // journal opposite edges — the graph would not be a function of
        // the sealed spec. Three overlapping members so a first/last-only
        // swap cannot hide a leftover middle-order dependence.
        let members = [member("wp-c", 3), member("wp-a", 1), member("wp-b", 2)];
        let projections = [
            projection("wp-c", 3, &["crates/aether-bloomery/src/values/price.rs"]),
            projection("wp-a", 1, &["crates/aether-bloomery/**"]),
            projection("wp-b", 2, &["crates/aether-bloomery/src/values/**"]),
        ];
        let reversed_members = [member("wp-b", 2), member("wp-a", 1), member("wp-c", 3)];
        let reversed_projections = [
            projection("wp-b", 2, &["crates/aether-bloomery/src/values/**"]),
            projection("wp-a", 1, &["crates/aether-bloomery/**"]),
            projection("wp-c", 3, &["crates/aether-bloomery/src/values/price.rs"]),
        ];

        let forward = resolve_seal_graph(&members, &projections, &[]).expect("acyclic");
        let reversed = resolve_seal_graph(&reversed_members, &reversed_projections, &[]).expect("acyclic");
        assert_eq!(forward, reversed, "request order must not change the journaled graph");

        let spec_forward = BloomDraft { proposals: members.to_vec(), ..BloomDraft::default() }.seal();
        let spec_reversed = BloomDraft { proposals: reversed_members.to_vec(), ..BloomDraft::default() }.seal();
        assert_eq!(spec_forward.id(), spec_reversed.id(), "the same member set seals to one BloomId");
        assert!(
            forward.is_empty(),
            "overlap-only members journal no declared gate, regardless of request order: {forward:?}"
        );
    }

    #[test]
    fn a_declared_edge_still_journals_when_surfaces_also_overlap() {
        // A declared dependency that the surfaces would also derive must
        // still gate dispatch. Dropping it because overlap "already named
        // it" would leave an authored wait as an optimistic Construct.
        let members = [member("wp-a", 1), member("wp-b", 2)];
        let projections = [
            projection("wp-a", 1, &["crates/aether-bloomery/**"]),
            projection("wp-b", 2, &["crates/aether-bloomery/src/lib.rs"]),
        ];
        let declared =
            [MemberDependency { member: WorkpieceId("wp-b".to_owned()), depends_on: WorkpieceId("wp-a".to_owned()) }];

        let edges = resolve_seal_graph(&members, &projections, &declared).expect("acyclic");
        assert_eq!(edges, declared);
    }

    /// `projection`, plus the crate blocks the read-ordering rule reads.
    fn declaring(workpiece: &str, revision: u8, crates: &[&str], reads: &[&str]) -> MemberProjection {
        MemberProjection {
            declared_crates: crates.iter().map(|name| (*name).to_owned()).collect(),
            declared_reads: reads.iter().map(|name| (*name).to_owned()).collect(),
            ..projection(workpiece, revision, &["crates/aether-bloomery/**"])
        }
    }

    #[test]
    fn a_declared_read_orders_only_against_a_co_member_that_declares_that_crate() {
        // ADR-0204's conditional half, and the reason a read could not just be
        // written as a `## Depends on` line. The pre-fix failure is that the
        // only ordering vocabulary was unconditional: B declaring that it reads
        // aether-data had to either order behind A unconditionally — paying the
        // serialization on every seal, including the ones where A never touches
        // aether-data — or say nothing and race it.
        let members = [member("wp-a", 1), member("wp-b", 2)];

        let writing =
            [declaring("wp-a", 1, &["aether-data"], &[]), declaring("wp-b", 2, &["aether-codec"], &["aether-data"])];
        assert_eq!(
            resolve_seal_graph(&members, &writing, &[]).expect("acyclic"),
            [MemberDependency { member: WorkpieceId("wp-b".to_owned()), depends_on: WorkpieceId("wp-a".to_owned()) }],
            "B reads what A declares it will change, so B waits",
        );

        // The negative case, which is the whole point: A's *surface* covers
        // aether-data (its closure reaches it) but A never declares it, so A is
        // not going to change it and B is owed no wait.
        let quiet =
            [declaring("wp-a", 1, &["aether-math"], &[]), declaring("wp-b", 2, &["aether-codec"], &["aether-data"])];
        assert!(
            resolve_seal_graph(&members, &quiet, &[]).expect("acyclic").is_empty(),
            "a read against a crate nobody in the seal is changing costs nothing",
        );
    }

    #[test]
    fn a_read_that_closes_a_loop_refuses_at_the_door() {
        // A read edge is a real edge, so it has to be cycle-checked with the
        // authored ones rather than appended past the check. Two members that
        // each read what the other changes is a graph no scheduler can fire.
        let members = [member("wp-a", 1), member("wp-b", 2)];
        let projections = [
            declaring("wp-a", 1, &["aether-data"], &["aether-codec"]),
            declaring("wp-b", 2, &["aether-codec"], &["aether-data"]),
        ];

        let error = resolve_seal_graph(&members, &projections, &[]).expect_err("a read cycle must refuse");

        assert_eq!(error.status, 422);
    }

    #[test]
    fn the_door_refuses_a_cycle_naming_its_members() {
        // The door is the refuse — a cycle that reached the reducer would
        // journal a graph no scheduler can fire. The message must name both
        // members so the operator can see the loop they wrote.
        let members = [member("wp-a", 1), member("wp-b", 2)];
        let projections = [projection("wp-a", 1, &["docs/a/**"]), projection("wp-b", 2, &["docs/b/**"])];
        let declared = [
            MemberDependency { member: WorkpieceId("wp-a".to_owned()), depends_on: WorkpieceId("wp-b".to_owned()) },
            MemberDependency { member: WorkpieceId("wp-b".to_owned()), depends_on: WorkpieceId("wp-a".to_owned()) },
        ];

        let error = resolve_seal_graph(&members, &projections, &declared).expect_err("a cycle must refuse");
        let body = String::from_utf8_lossy(&error.body);
        assert_eq!(error.status, 422);
        assert!(body.contains("wp-a"), "cycle names wp-a: {body}");
        assert!(body.contains("wp-b"), "cycle names wp-b: {body}");
    }

    #[test]
    fn the_door_refuses_a_non_member_naming_it() {
        // An edge pointing outside the bloom cannot be scheduled here.
        // Naming the in-bloom end instead would hide the workpiece the
        // operator actually misspelled.
        let members = [member("wp-a", 1)];
        let projections = [projection("wp-a", 1, &["docs/**"])];
        let declared =
            [MemberDependency { member: WorkpieceId("wp-a".to_owned()), depends_on: WorkpieceId("wp-z".to_owned()) }];

        let error = resolve_seal_graph(&members, &projections, &declared).expect_err("a non-member must refuse");
        let body = String::from_utf8_lossy(&error.body);
        assert_eq!(error.status, 422);
        assert!(body.contains("wp-z"), "refusal names the outsider: {body}");
    }

    #[test]
    fn the_door_refuses_an_unparseable_declared_surface_glob() {
        // A comma-joined path list is one glob containing `,`, which the
        // grammar rejects. Skipping it would admit the member with an empty
        // surface, derive no overlap edges, and dispatch it beside the peers
        // it actually shares files with. `--pre-approved` waives the tier, not
        // the grammar — the observed hole was sealing that typo as auto.
        let policy = ApprovalPolicy { default: Tier::Auto, rules: Vec::new() };
        let gate = Gate::new(&policy);
        let members = [member("wp-a", 1)];
        let mut projections = [projection("wp-a", 1, &["crates/foo/**", "crates/foo/**,crates/bar/**"])];
        projections[0].pre_approved = true;

        let error = resolve_seal_memberships(&gate, &policy, &members, &projections)
            .expect_err("an unparseable glob must refuse");
        let body = String::from_utf8_lossy(&error.body);
        assert_eq!(error.status, 422);
        assert!(body.contains("wp-a"), "refusal names the member: {body}");
        assert!(
            body.contains("crates/foo/**,crates/bar/**"),
            "refusal names the offending glob, not only its valid sibling: {body}"
        );
    }

    #[test]
    fn an_above_auto_member_with_only_an_auto_approval_is_signer_policy() {
        // The store has an approval row, so this is not "absent approval".
        // The gate still needs a signed statement for a human surface, and
        // the message must name signer policy so the two refusals stay
        // distinguishable.
        let policy = ApprovalPolicy { default: Tier::Human, rules: Vec::new() };
        let gate = Gate::new(&policy);
        let members = [member("wp-a", 1)];
        // A subtree glob, so the granularity check ahead of the gate passes it
        // through and this test still exercises the signer-policy refusal.
        let projections = [projection("wp-a", 1, &["crates/aether-data/src/**"])];

        let error = resolve_seal_memberships(&gate, &policy, &members, &projections)
            .expect_err("an auto-only above-auto member must refuse");
        let body = String::from_utf8_lossy(&error.body);
        assert_eq!(error.status, 422);
        assert!(body.contains("signer policy"), "signer-policy refusal: {body}");
        assert!(!body.contains("no stored approval"), "must not read as absent approval: {body}");
        assert!(!body.contains("stale"), "must not read as stale: {body}");
        assert!(!body.contains("malformed"), "must not read as malformed: {body}");
    }

    #[test]
    fn the_door_refuses_a_member_with_no_matching_projection() {
        // A missing projection is a malformed request, not an edgeless
        // member. Dropping it from the derivation set would journal a graph
        // that pretends the member was never there, so overlapping peers
        // would dispatch as if they had no neighbor.
        let members = [member("wp-a", 1), member("wp-b", 2)];
        let projections = [projection("wp-a", 1, &["docs/**"])];

        let error = resolve_seal_graph(&members, &projections, &[]).expect_err("a missing projection must refuse");
        let body = String::from_utf8_lossy(&error.body);
        assert_eq!(error.status, 422);
        assert!(body.contains("wp-b"), "refusal names the unmatched member: {body}");
    }

    /// A fully populated projection: every DTO field is set, including the two
    /// the gate never reads, so a revert to hashing the transport struct moves
    /// the pin.
    fn pinned_projection() -> MemberProjection {
        MemberProjection {
            declared_crates: vec!["aether-bloomery".to_owned()],
            adr_touch: AdrTouch::ProposedOnly,
            pre_approved: true,
            signed_statement: Some(Statement {
                words: b"fixture statement".to_vec(),
                provenance: Provenance::ObservationAttestation(Observation { source: "fixture".to_owned() }),
                parents: Vec::new(),
            }),
            ..projection("wp-a", 1, &["crates/aether-bloomery/**"])
        }
    }

    #[test]
    fn the_projection_digest_is_pinned() {
        // Tripwire: the pinned value is computed from the gate inputs, so it
        // moves exactly when what an auto approval binds moves — which is the
        // event this member exists to make visible.
        let policy = ApprovalPolicy { default: Tier::Auto, rules: Vec::new() };
        let gate = Gate::new(&policy);
        let members = [member("wp-a", 1)];
        let projections = [pinned_projection()];

        let (sealed, pending) = resolve_seal_memberships(&gate, &policy, &members, &projections)
            .expect("a complete auto-tier member admits");
        assert!(pending.is_empty(), "pre-approved auto forms inline");

        let admission = admission_request(&members[0], &projections[0]);
        assert_eq!(
            admission.projection_digest.to_hex(),
            "f40f0733dc88a1cdc6eddd52ef19c500544e92aa4423e9eb1d35c872de055a53",
            "the named gate-input digest moved; an auto approval now binds different facts",
        );
        let Decision::AutoApproved(expected) = gate.evaluate(&admission) else {
            panic!("pinned fixture resolves auto");
        };
        assert_eq!(sealed[0].approval, expected, "the door folded the named-field digest");
    }

    #[test]
    fn a_field_the_gate_never_reads_does_not_rekey_the_approval() {
        // Two slices in a row appended a field (`declared_crates`, then
        // `declared_reads`) and silently re-keyed every auto approval formed
        // afterwards, because the door hashed the whole transport DTO.
        let policy = ApprovalPolicy { default: Tier::Auto, rules: Vec::new() };
        let gate = Gate::new(&policy);
        let members = [member("wp-a", 1)];
        let empty_reads = [pinned_projection()];
        let mut with_reads = pinned_projection();
        with_reads.declared_reads = vec!["aether-data".to_owned()];

        let (without, _) =
            resolve_seal_memberships(&gate, &policy, &members, &empty_reads).expect("empty reads admits");
        let (with, _) =
            resolve_seal_memberships(&gate, &policy, &members, &[with_reads]).expect("declared reads still admits");
        assert_eq!(
            without[0].approval.detail, with[0].approval.detail,
            "declared_reads is outside the approval digest",
        );
    }
}
