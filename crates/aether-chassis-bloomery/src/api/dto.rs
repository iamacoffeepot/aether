//! JSON request/response shapes for the REST control API (ADR-0149 §Packaging,
//! issue #3498).
//!
//! These are the wire contract an operator's `curl` speaks — plain serde
//! structs over the `aether-bloomery` value types (`Workpiece`, `BloomDraft`,
//! `Membership`, `Forecast`, `Digest`, and the reducer `Event` /
//! `Outcome` / projection `ViewDocument` / `BloomView`). The value types
//! already derive serde, so the API layer serializes them directly; these
//! structs are the request bodies and the small response envelopes that bundle
//! a minted draft handle alongside the value. `GET /view` and `GET /blooms`
//! render [`ViewDocument`](aether_bloomery::ViewDocument) as-is, including the
//! spend-quiesce marker (ADR-0192) when the seal door is closed.
//!
//! They carry no `aether_data::Kind` — they are HTTP-JSON bodies, not mailbox
//! mail, and never cross the wire codec.
//!
//! A digest-typed field here is a plain [`Digest`] and stays one. How it is
//! spelled in JSON belongs to the codecs the routes read and write these
//! through (`runtime::hex`), which take 64 hex characters or the canonical byte
//! array on the way in and render hex on the way out — so a body agrees with
//! the path segments beside it without any type in this file saying so.

use serde::{Deserialize, Serialize};

#[cfg(feature = "github")]
use aether_bloomery::{BloomId, ClaimHolder, ClaimRefKind};
use aether_bloomery::{
    CandidateRef, ConfigRegistry, Digest, Disposition, Event, Forecast, MemberDependency, Membership, Outcome,
    ScopeRevision, ScopeVerifyReport, StageId, Statement, SuppressionVerdict, Workpiece, WorkpieceId,
};

use crate::bloomery::{AdrTouch, Completeness};
use crate::store::RevisionEvidence;

/// A draft plus its server-minted handle. The handle keys the in-memory
/// shaping state, so a subsequent `PATCH` / `seal` names the draft by it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftView {
    /// The draft's handle (a monotonic per-process id, rendered as a string).
    pub draft_id: String,
    /// The draft's current shape.
    pub draft: aether_bloomery::BloomDraft,
}

/// `GET /workpieces` — every durable open commission that has a current revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkpiecesView {
    /// The open commissions, materialized as workpieces, in id order.
    pub workpieces: Vec<Workpiece>,
}

/// `GET /drafts` — every open draft with its handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftsView {
    /// The open drafts, in handle order.
    pub drafts: Vec<DraftView>,
}

/// `PATCH /drafts/{id}` body — every field optional; a present field replaces
/// that part of the draft, an absent one leaves it unchanged.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DraftPatch {
    /// Replace the proposed memberships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposals: Option<Vec<Membership>>,
    /// Replace the bloom configuration registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configs: Option<ConfigRegistry>,
    /// Replace the base tree digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Digest>,
    /// Replace the forecast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<Forecast>,
}

/// The seal-time scope projection the store-backed commission reader
/// materializes per draft membership so the pre-seal approve gate (issue
/// #3583 / #5048) can decide the member's admission. It mirrors the gate's
/// [`AdmissionRequest`](crate::bloomery::AdmissionRequest) inputs, keyed
/// by `{workpiece, scope_revision}` so the host matches it to the exact draft
/// proposal. These are reconstructed from the frozen scope revision and
/// stored approval — they are never taken from the seal request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberProjection {
    /// The workpiece this projection describes — matches a draft proposal's
    /// `workpiece`.
    pub workpiece: WorkpieceId,
    /// The exact scope-revision digest — matches the proposal's `scope_revision`
    /// and the digest the formed approval binds.
    pub scope_revision: Digest,
    /// The declared-surface globs the containment bound is drawn from.
    pub declared_surface: Vec<String>,
    /// The workspace crates the scope declared, when it declared crates rather
    /// than globs. Non-empty means `declared_surface` above was *derived* — the
    /// declared crates, their reverse-dependency closure, the shared roots, and
    /// the protected files — and the tier resolves over the protected files
    /// alone. Defaulted so a projection written before the block existed reads
    /// back as the glob-declared surface it is.
    #[serde(default)]
    pub declared_crates: Vec<String>,
    /// The workspace crates the scope declared it load-bearingly *reads*
    /// (ADR-0204) — the `## Reads` block. The door turns these into
    /// conditional ordering against the co-members that declared they will
    /// change those crates, and into nothing at all when no co-member does.
    ///
    /// The gate never reads this: a read is not authority, so it lifts no
    /// tier and widens no surface. It is therefore outside the approval digest,
    /// which is why this field may be appended to freely. Defaulted so a
    /// projection written before the block existed reads back as declaring no
    /// reads.
    #[serde(default)]
    pub declared_reads: Vec<String>,
    /// The nine completeness facts the gate fails closed on.
    pub completeness: Completeness,
    /// The ADR-maturity of the change, for the unconditional hard gate.
    pub adr_touch: AdrTouch,
    /// Whether an owner-actor-verified `approval:pre-approved` override is
    /// present (waives the tier to `auto`, never the gate checks).
    pub pre_approved: bool,
    /// The owner-signed statement for an above-`auto` member. Consumed by the
    /// deferred-verify enforcement (its live wiring is the follow-up child #3599);
    /// an above-`auto` member fails closed until then. The gate never reads this,
    /// so it is outside the approval digest and may be appended to freely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_statement: Option<Statement>,
}

/// `POST /drafts/{id}/seal` body — optional. The idempotency key defaults to
/// the sealed bloom's own id, so re-POSTing the same seal is a no-op duplicate.
///
/// Scope, approval, description, and completeness are not fields here: the
/// door loads them from the commission store (#5048). A body that still
/// carries `projections` or `descriptions` is accepted and those fields are
/// ignored, so a caller cannot override the signed revision.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SealRequest {
    /// Override the admit idempotency key; defaults to the sealed bloom id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Declared member-dependency edges (ADR-0196): `member` depends on
    /// `depends_on`. The door unions these with derived overlap-ordering
    /// edges and with edges frozen on each member's scope revision. Empty
    /// (the default) is today's edgeless seal plus whatever the store named.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<MemberDependency>,
}

/// `POST /blooms/{id}/supersede` body — names the open draft to seal as the
/// successor. The predecessor bloom id is the `{id}` path segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupersedeRequest {
    /// The open draft handle to seal into the successor bloom.
    pub successor_draft: String,
    /// Override the admit idempotency key; defaults to the successor bloom id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Declared member-dependency edges — the same optional list [`SealRequest::edges`]
    /// carries. The door unions these with derived overlap-ordering edges and
    /// with edges frozen on each successor member's scope revision, then
    /// refuses a cycle or a non-member. Empty (the default) is the edgeless
    /// drop-a-subtree supersede plus whatever the store named: the reducer
    /// keeps the predecessor's remaining member graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<MemberDependency>,
}

/// `POST /blooms/{id}/grant` body — hand a wedged member more attempts on the
/// bloom it already belongs to (#4708), instead of superseding an unchanged one.
///
/// `reason` and `operator` are the audit trail of an act no verdict produced,
/// the same two fields the other operator doors require. Blank is `422`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRequest {
    /// The wedged member to resume.
    pub workpiece: WorkpieceId,
    /// The stage the grant believes the member is wedged at — the reducer
    /// refuses a grant naming any other, so a stale read cannot act.
    pub stage: StageId,
    /// How many more dispatched attempts the member may spend before it wedges
    /// again.
    pub attempts: u32,
    /// Why the operator is buying another round. Required and non-blank.
    pub reason: String,
    /// Who is asking. Recorded as the decider; required and non-blank.
    pub operator: String,
    /// Override the admit idempotency key.
    ///
    /// The default is derived from the grant's own content, so re-POSTing the
    /// same request is a no-op duplicate rather than a second grant. That also
    /// means a *deliberate* second grant of the same shape — the member wedged
    /// again and the operator is buying another round — has to name its own key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `POST /blooms/{id}/adjudicate` body — the manager override's first move
/// (#4957): close the composition findings the operator has read, with a stated
/// reason, and let the bloom proceed to its landing.
///
/// The findings are named by the verdict artifact digest each carries. There is
/// deliberately no workpiece field: an adjudication acts on the composition's
/// findings channel, and a member that has passed its review is immutable
/// (ADR-0191 §4), so there is no member for this body to reach.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdjudicateRequest {
    /// The findings to close, by verdict artifact digest.
    pub findings: Vec<Digest>,
    /// Accepted as they stand, or deferred to a filed issue.
    pub disposition: Disposition,
    /// Why. Required and non-blank — the reason is what the landing proposal
    /// quotes as the grounds for the waiver, so an absent one is refused (`422`)
    /// rather than defaulted.
    pub reason: String,
    /// Who is adjudicating. Recorded as the decider; required and non-blank.
    pub operator: String,
    /// Override the admit idempotency key.
    ///
    /// The default is derived from the adjudication's own content, so re-POSTing
    /// the same request is a no-op duplicate rather than a second waiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `POST /blooms/{id}/members/{workpiece}/suppression` body — the reviewer's
/// answer to the suppression requests a member's candidate is carrying
/// (ADR-0193 §5).
///
/// The lane states; only a reviewer grants. Both answers arrive at this one
/// door, because what is recorded is the same thing either way — who answered
/// what, and how — and there is no marker for "no" to place anywhere else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuppressionAnswerRequest {
    /// The requests being answered, by [`SuppressionRequest`] digest. Required
    /// and non-empty: an answer that closes nothing is not an answer, and one
    /// that closed *everything standing* would silently answer a request the
    /// reviewer had not read.
    ///
    /// [`SuppressionRequest`]: aether_bloomery::SuppressionRequest
    pub requests: Vec<Digest>,
    /// The answer. `Granted` lets the candidate keep its suppressions;
    /// `Denied` bounces the member to a repair lap at its own budget's expense.
    pub verdict: SuppressionVerdict,
    /// Why. Required and non-blank — for a denial it is what the repair lap is
    /// told, and for a grant it is the record of the judgment.
    pub reason: String,
    /// Who answered. Recorded as the decider; required and non-blank, because
    /// "who granted this allow" is the audit question the mechanism exists for.
    pub operator: String,
    /// Override the admit idempotency key.
    ///
    /// The default is derived from the answer's own content, so re-POSTing the
    /// same answer is a no-op duplicate rather than a second decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `POST /blooms/{id}/members/{workpiece}/repair` body — the manager override's
/// second move (#4957): the candidate the operator pushed to the workpiece's
/// candidate ref, offered to the ordinary gates.
///
/// Name exactly one source. `candidate` is the low-level (tree, commit) pair
/// every candidate is in this vocabulary (ADR-0152). `from_commit` /
/// `from_worktree` ask the chassis to derive that pair, push the candidate
/// ref, and record both correspondence rows (#5032) so the operator does not
/// re-state the digest scheme.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairRequest {
    /// The candidate the operator already pushed. Required unless `from_commit`
    /// or `from_worktree` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<CandidateRef>,
    /// A commit reachable from the coordinator's repository. The chassis
    /// derives the candidate, pushes the ref, and records correspondence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_commit: Option<String>,
    /// A worktree whose `HEAD` is a commit the coordinator's repository can
    /// already see. Resolved, then treated as `from_commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_worktree: Option<String>,
    /// Why the operator took the lap themselves. Required and non-blank.
    pub reason: String,
    /// Who supplied it. Required and non-blank.
    pub operator: String,
    /// Override the admit idempotency key; defaults to the repair's own content,
    /// so a resend is a duplicate rather than a second dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `POST /blooms/{id}/hold` and `POST /blooms/{id}/release` body — the operator
/// brake (#4976).
///
/// One shape for both routes because both edges say exactly the same two things
/// and neither says anything else. A hold carries no member selector, no
/// priority, and no expiry: it is bloom-level and flat, and the request that
/// raises it looks like the request that drops it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HoldRequest {
    /// Why the bloom is being frozen, or let go. Required and non-blank — a
    /// brake pulled on a running bloom is an act no verdict produced, so a
    /// record of it that says nothing is the whole failure. Blank is `422`.
    pub reason: String,
    /// Who is asking. Recorded as the decider; required and non-blank.
    pub operator: String,
    /// Override the admit idempotency key; defaults to the request's own
    /// content under this route's name, so a resend is a duplicate rather than a
    /// second brake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `POST /blooms/{id}/members/{workpiece}/withdraw` body — take one member out
/// of a walking bloom without superseding it (#5327).
///
/// Unauthenticated on the host-local bind like every other bloom operator
/// route, and gated by the same mandatory non-blank `reason` + `operator`: the
/// audit trail is the whole product of an act no verdict produced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WithdrawRequest {
    /// Why this member is leaving, in the operator's own words. Required and
    /// non-blank; a blank one is `422`.
    pub reason: String,
    /// Who is deciding. Recorded as the decider; required and non-blank.
    pub operator: String,
    /// Also withdraw every member that transitively depends on this one.
    /// Without it, a withdrawal that would strand a dependent is refused
    /// `422`, naming them — a dependent left behind pins the bloom the
    /// withdrawal was meant to free.
    #[serde(default)]
    pub cascade: bool,
    /// Override the admit idempotency key; defaults to the withdrawal's own
    /// content, so a resend is a duplicate rather than a second act.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// `POST /blooms/{id}/members/{workpiece}/retry` body — re-dispatch one member's
/// current stage on the candidate it already holds (#5423).
///
/// The operator states the stage and the subject it read them off the view at,
/// and the reducer refuses both if the member has moved since: a retry aimed
/// from a stale read must not spend a machinery roll on a stage the member is no
/// longer sitting at, or bind a fault to a candidate it no longer holds.
///
/// Unauthenticated on the host-local bind like every other bloom operator route,
/// and gated by the same mandatory non-blank `reason` + `operator`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRequest {
    /// The stage to re-dispatch — the member's current cursor stage. A mismatch
    /// is refused rather than applied to wherever the member actually is.
    pub stage: StageId,
    /// The subject the retry binds its fault evidence to: the member's candidate
    /// tree, or its scope revision when it holds no candidate yet.
    pub subject: Digest,
    /// Why this stage is being run again, in the operator's own words. Required
    /// and non-blank; a blank one is `422`.
    pub reason: String,
    /// Who is deciding. Recorded as the decider; required and non-blank.
    pub operator: String,
    /// Override the admit idempotency key; defaults to the retry's own content,
    /// so a resend is a duplicate rather than a second roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// The reply to a write route: the reducer outcome the admitted event resolved
/// to (decoded from the control core's wire bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeView {
    /// The reducer outcome (sealed / superseded / rejected, and why).
    pub outcome: Outcome,
}

/// One decoded journal record for `GET /journal`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The record's journal sequence.
    pub sequence: u64,
    /// The record's idempotency key.
    pub idempotency_key: String,
    /// The decoded event the record journaled.
    pub event: Event,
    /// The outcome the event reduced to when it was admitted (ADR-0190) — read
    /// from the record, so it names what was decided, not what the current
    /// reducer would decide.
    pub outcome: Outcome,
    /// The identity of the build whose reducer decided the event.
    pub decider: String,
}

/// `GET /journal` — one bounded page of decoded records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalView {
    /// The page of journaled events, in the requested order.
    pub records: Vec<JournalEntry>,
    /// How many records match the filter (the whole journal, or one bloom).
    pub total_matched: u64,
    /// How many records this page carries.
    pub shown: u64,
    /// True when more matching records remain after this page.
    pub truncated: bool,
    /// Exclusive cursor for the next page. Absent when [`truncated`](Self::truncated)
    /// is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_from_sequence: Option<u64>,
    /// Set when the caller named a `limit` above the clamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// `GET /artifacts/{digest}/decoded` — a known kind, or a raw range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedArtifactView {
    /// The content-address domain of the resolved type, or `null`.
    pub kind: Option<String>,
    /// The decoded value, present only when [`kind`](Self::kind) is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// A raw byte range, present only when no known type matched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    /// Offset of [`bytes`](Self::bytes) when this is a raw fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// Full artifact length when this is a raw fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Whether the raw fallback was truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// Set when the caller named a `limit` above the clamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

#[cfg(feature = "github")]
/// One enumerated claim ref for `GET /claims` (ADR-0179).
///
/// The diagnostic surface that used to require leaving the API for `git
/// ls-remote`: an operator blocked by `ActiveBloomExists` can now see which ref
/// holds the bloom it named. Enumeration is **not** a liveness oracle — a holder
/// absent from this instance's journal may be another instance's live bloom —
/// so the operator investigates before signing a release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRefView {
    /// Which typed claim ref this is.
    pub ref_kind: ClaimRefKind,
    /// Who currently holds it (or the tombstone marker an interrupted release
    /// stranded).
    pub holder: ClaimHolder,
}

#[cfg(feature = "github")]
/// `GET /claims` — every live claim ref, with its holder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimsView {
    /// The live claim refs, in enumeration order.
    pub claims: Vec<ClaimRefView>,
}

#[cfg(feature = "github")]
/// `POST /claims/releases` body — the typed release target plus the author
/// signature authorizing it (ADR-0179).
///
/// There is deliberately no ref-path field. The target is named by
/// [`ClaimRefKind`] and [`BloomId`], so no spelling of this body reaches a Git
/// ref outside the claim namespace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequest {
    /// The typed claim ref to release.
    pub ref_kind: ClaimRefKind,
    /// The holder the release expects to find on it — the compare half of the
    /// source port's compare-and-swap.
    pub expected_holder: BloomId,
    /// The author-signed statement whose words are exactly
    /// `release orphan bloomery claim` and whose parents name the request
    /// digest. Verified against the custodied signer allowlist before admission.
    pub authorization: Statement,
}

/// `POST /claims/releases` reply — the request digest, returned with `202` once
/// the request fact is durably admitted. The operator polls
/// `GET /claims/releases/{digest}` for the terminal result.
///
/// Ungated, unlike the three views above: the admit reply renderer is one
/// function for every write route, and it recognizes the accepted release by the
/// outcome the reducer produced rather than by anything the route held — so the
/// shape it renders has to exist in a build whose release route answers `503`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAcceptedView {
    /// The request digest, lowercase hex — the status route's path segment.
    pub request: String,
    /// The reducer outcome the admitted request resolved to.
    pub outcome: Outcome,
}

/// A structured error body for a `4xx` / `5xx` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorView {
    /// A human-readable failure reason.
    pub error: String,
}

/// `GET /blooms/{id}/dispatches` — rollup attempts plus live outstanding orders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomDispatchesView {
    /// One row per attempt, oldest first.
    pub dispatches: Vec<BloomDispatchView>,
}

/// One attempt on a bloom: a completed rollup row and/or a still-live order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomDispatchView {
    /// Host nonce (`dispatch-` / `redispatch-`) when known.
    pub nonce: String,
    /// Member workpiece, empty for a bloom-wide stage.
    pub workpiece: String,
    /// Stage this attempt ran.
    pub stage: StageId,
    /// 1-based rank among this workpiece+stage pair, oldest first.
    pub attempt: u32,
    /// Lane status when evidence is still on disk; absent while in flight or swept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// Study cost in micro-USD. `None` when no study record exists — never a
    /// synthesized zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<u64>,
    /// Whether `{nonce}-evidence` is still on disk.
    pub evidence_retained: bool,
}

/// `GET /dispatches/{nonce}` — one dispatch's evidence header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchEvidenceView {
    /// The nonce that matched (may be the alternate prefix).
    pub nonce: String,
    /// Whether the evidence directory is still on disk.
    pub retained: bool,
    /// Set when the journal names the nonce but the directory is gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// Public assistant prose, independently capped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_text: Option<String>,
    /// True when [`assistant_text`](Self::assistant_text) was truncated to the cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub assistant_text_truncated: bool,
    /// Construct/refine commit message, independently capped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    /// True when [`commit_message`](Self::commit_message) was truncated to the cap.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub commit_message_truncated: bool,
    /// Process identity recorded beside the evidence, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process: Option<DispatchProcessView>,
    /// File names in the evidence directory, sorted, bounded.
    pub files: Vec<String>,
}

/// The pid / pgid / starttime / boot id a lane recorded at spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchProcessView {
    pub pid: u32,
    pub pgid: u32,
    pub starttime: u64,
    pub boot_id: String,
}

/// A line-snapped page of `transcript.jsonl` or `prompt.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchFilePage {
    /// Complete lines in this page. A line longer than the per-line cap is
    /// truncated here; the cursor still advances past the whole line.
    pub lines: Vec<String>,
    /// Byte offset this page starts at (snapped to a line boundary).
    pub cursor: u64,
    /// Byte offset to pass as the next `cursor`. Absent at the current end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
    /// The file's length at the moment of the read — follow-tail compares this.
    pub length: u64,
    /// Set when the caller named a `limit` above the clamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// `GET /logs/coordinator` — one bounded page of journald output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorLogsView {
    /// Filtered entries, oldest first.
    pub entries: Vec<CoordinatorLogEntry>,
    /// Cursor to thread as `cursor` on the next call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// True when more matching entries remain after this page.
    pub truncated: bool,
    /// Set when the caller named a `limit` above the clamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// `POST /commissions` body — the workpiece id plus its intent statement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCommissionRequest {
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// The intent statement named by `commissions.intent`.
    pub intent: Statement,
}

/// `POST /commissions/{id}/revisions` body — the signed revision and sidecar
/// evidence about it.
///
/// The revision's own bytes are the signed subject and must stay exactly what
/// the encoder writes; [`evidence`](Self::evidence) rides beside them and is
/// never hashed in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRevisionRequest {
    /// The revision being stored.
    pub revision: ScopeRevision,
    /// What is known about the revision without being part of it.
    #[serde(default)]
    pub evidence: RevisionEvidence,
}

/// `POST /commissions` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionCreatedView {
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// Digest of the stored intent statement.
    pub intent: Digest,
}

/// One commission head as the list and show routes render it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionHeadView {
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// Digest of the stored intent statement.
    pub intent: Digest,
    /// Digest of the current scope revision, when one has been written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<Digest>,
    /// Store-side chain position of the current revision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_ordinal: Option<u64>,
    /// Lifecycle flag. Not signed.
    pub status: String,
}

/// `GET /commissions` — every matching commission head.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionsView {
    /// Matching commissions, in workpiece-id order.
    pub commissions: Vec<CommissionHeadView>,
}

/// `GET /commissions/{id}` — the head, current revision, and current approvals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionShowView {
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// Digest of the stored intent statement.
    pub intent: Digest,
    /// Digest of the current scope revision, when one has been written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<Digest>,
    /// Store-side chain position of the current revision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_ordinal: Option<u64>,
    /// Lifecycle flag. Not signed.
    pub status: String,
    /// The current revision decoded from its canonical bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<ScopeRevision>,
    /// Why [`Self::current`] is absent even though [`Self::current_revision`]
    /// names a tip. Omitted when the tip is absent or readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_unreadable: Option<String>,
    /// Approval statements stored for the current revision, in insert order.
    pub approvals: Vec<Statement>,
    /// The scope-verify report journaled for the current revision (ADR-0208).
    ///
    /// Rendered even when `null`, unlike the optional fields above: `null` is
    /// the explicit statement that no scope-verify evidence exists for these
    /// bytes, and omitting the key would let a reader mistake absence for a
    /// clean report.
    pub scope_verify: Option<ScopeVerifyReport>,
}

/// `POST /commissions/{id}/revisions` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeRevisionWrittenView {
    /// Digest of the stored revision.
    pub digest: Digest,
}

/// `POST /commissions/{id}/approvals` reply — the stored statement plus the
/// evidence form the seal path consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionApprovalView {
    /// Digest of the stored approval statement.
    pub digest: Digest,
    /// The verified-statement evidence bound to the approved scope.
    pub evidence: aether_bloomery::Evidence,
}

/// `POST /commissions/{id}/cancel` body: the signature is the authority, the
/// reason is the operator's own words and is recorded in the coordinator log,
/// never in the signed bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelCommissionRequest {
    /// The Cancel-door statement bound to the commission's intent digest.
    pub statement: Statement,
    /// Operator context for the cancel. Never authority.
    pub reason: String,
}

/// `POST /commissions/{id}/cancel` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionCancelledView {
    /// Digest of the stored cancel statement.
    pub digest: Digest,
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// Always `"cancelled"`.
    pub status: String,
}

/// `POST /commissions/{id}/reopen` body: the same shape a cancel submits, at
/// the Reopen door. The signature is the authority; the reason is the
/// operator's own words and is recorded in the coordinator log, never in the
/// signed bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReopenCommissionRequest {
    /// The Reopen-door statement bound to the commission's intent digest.
    pub statement: Statement,
    /// Operator context for the reopen. Never authority.
    pub reason: String,
}

/// `POST /commissions/{id}/reopen` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommissionReopenedView {
    /// Digest of the reopen statement that authorized it.
    pub digest: Digest,
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// Always `"open"`.
    pub status: String,
}

/// `POST /commissions/{id}/scope-runs` body: the observed mainline the run
/// reads code at. The coordinator does not invent a tree; this is
/// `Snapshot.mainline` as the CLI (or a caller that has just read `GET /view`)
/// observed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeRunRequest {
    /// The observed mainline head.
    pub base: Digest,
}

/// `POST /commissions/{id}/scope-runs` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeRunOpenedView {
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// The attempt ordinal opened, from `1`.
    pub ordinal: u64,
    /// The outbox sequence the drain will mint a nonce from.
    pub sequence: u64,
    /// The run's content-addressed subject.
    pub subject: Digest,
}

/// One journald entry as the coordinator log route renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorLogEntry {
    /// Journal realtime timestamp, Unix microseconds when journald provides it.
    pub timestamp_unix_micros: u64,
    /// Canonical level string: `trace` / `debug` / `info` / `warn` / `error`.
    pub level: String,
    /// The MESSAGE field.
    pub message: String,
    /// journald `__CURSOR`, used as the page cursor.
    pub cursor: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{DraftPatch, ErrorView};

    #[test]
    fn error_body_is_a_single_error_field() {
        // Tripwire: the error body shape curl reads. A field rename or an added
        // field drifts the contract and breaks this pinned string.
        assert_eq!(serde_json::to_string(&ErrorView { error: "nope".to_owned() }).unwrap(), r#"{"error":"nope"}"#);
    }

    #[test]
    fn empty_patch_serializes_to_empty_object() {
        // Tripwire: `skip_serializing_if = "Option::is_none"` on every field is
        // what makes a partial `PATCH` partial — an absent field must not emit a
        // `null` that would clobber that part of the draft on a round-trip.
        assert_eq!(serde_json::to_string(&DraftPatch::default()).unwrap(), "{}");
    }
}
