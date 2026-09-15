//! The projection backend (#3459 step 5, narrowed by [#4663]): reconcile a
//! [`ViewDocument`] onto the objects the repository already holds.
//!
//! Every projected comment carries a stable [`Marker`] — its internal Bloomery
//! key plus a content digest of the desired render. Reconcile is *find by key
//! → compare digest → create / update / no-op*, so:
//!
//! - **Idempotent** — reconciling the same document twice is all no-ops, since
//!   the second pass finds each comment with a matching digest.
//! - **Rebuildable** — a deleted comment leaves no marker to find, so the next
//!   reconcile recreates it (the "delete → reappear" property the demo proves).
//!
//! # What maps to what
//!
//! - Each member → one **comment on the issue its workpiece addresses**, keyed
//!   by workpiece *and* bloom. State, approval, resolution, wedge, a graph
//!   hold (`blocked_by`), and any held question fold into that one comment
//!   rather than taking one apiece: they derive from the same [`MemberView`]
//!   and change together. The bloom half of the key is load-bearing — a
//!   successor bloom re-admitting the same workpiece shares one issue with its
//!   predecessor, and a workpiece-only key would have the two overwrite each
//!   other.
//! - A landing receipt → one comment on the landing pull request when one
//!   exists. The member's own landing comment is written by the land reactor
//!   at close time, not here.
//! - A commission whose workpiece names a repository object (`issue-N`) →
//!   one marker-keyed comment on that object. The projector owns no issue
//!   for it. A commission with no GitHub home still gets a replica issue.
//! - A bloom has no object of its own. Before it lands there is nothing to
//!   aggregate that `GET /view` does not serve live; afterwards its landing
//!   pull request *is* the aggregate (ADR-0149 §What each object is).
//!
//! # The write surface
//!
//! Comments stay comments-only: [`GithubApi`] still carries no verb that
//! could address a human-authored title or body. Replica issues a commission
//! has no GitHub home for are a second class of object (ADR-0149 2026-08-16
//! amendment, derived from ADR-0199). Those create / find / update / close
//! verbs live on [`CommissionProjectionApi`], and a title or body write
//! addresses only a number recorded from this projector's own create.
//! A plaintext marker is not ownership — not on a human issue, not on an
//! unrelated Bot issue, and not on an issue later edited to carry the marker,
//! including one authored by the same identity the projector authenticates as.
//! A crash between create and persist may leave an orphan replica; the next
//! projection mints a sibling rather than guessing.
//!
//! The projector reads only its own markers; free-form platform content is
//! never interpreted as intent. GitHub edits of a replica are overwritten
//! on the next projection.
//!
//! [#4663]: https://github.com/iamacoffeepot/aether/issues/4663

use std::fmt::Write as _;

use aether_bloomery::{
    AwaitingSurfaceView, BloomId, CommissionProjection, Digest, LandingReceipt, MemberView, PendingDecisionView,
    ProjectedReceipt, ProjectionBackend, ViewDocument, WorkpieceId, readable_title,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::client::{CommissionProjectionApi, GithubApi, GithubError, NewComment, NewIssue, PullRequestApi};
use crate::landing::{commission_floor_title, issue_title_is_valid};
use crate::marker::{Marker, render_marker};
use crate::short_hex;
use crate::source::landing_branch;

/// The prefix a [`WorkpieceId`] carries to address an object in the configured
/// repository. Adapter-local by construction: the core gains no issue
/// semantics and answers no question about numbers (ADR-0149 §Addressing).
const ISSUE_PREFIX: &str = "issue-";

/// GitHub refuses issue and comment bodies over this many characters with a
/// 422, so a commission mirror rendered verbatim fails to create or update on
/// every drain once its revision outgrows it.
const GITHUB_BODY_LIMIT_CHARS: usize = 65_536;

/// The cap a mirrored commission's full body (banner, intent, work order,
/// footer, and marker) stays under — below `GITHUB_BODY_LIMIT_CHARS` with
/// headroom for the marker the caller appends after the bound render.
pub const MAX_COMMISSION_BODY_CHARS: usize = 60_000;

const _: () = assert!(
    MAX_COMMISSION_BODY_CHARS < GITHUB_BODY_LIMIT_CHARS,
    "the mirror cap must stay below the platform limit it exists to respect",
);

/// The outward projection mirror over a [`GithubApi`] client.
pub struct GithubProjection<C> {
    client: C,
}

impl<C> GithubProjection<C> {
    /// Build a projection over `client`.
    pub const fn new(client: C) -> Self {
        Self { client }
    }

    /// Borrow the underlying client (test introspection / receipt routing).
    pub const fn client(&self) -> &C {
        &self.client
    }
}

impl<C: GithubApi> GithubProjection<C> {
    /// Upsert the marker-keyed comment `key` on `object`, absorbing the
    /// repository's refusal of that object.
    ///
    /// A refusal — the object is absent, or locked against comment — is
    /// permanent for this target, and outbox delivery holds a topic until its
    /// entry succeeds. Surfacing it as an error would stall the mirror on one
    /// unreachable member forever, so it is skipped and the entry settles
    /// (ADR-0149 §Failure is skipped, not stalled). Only a transport fault or
    /// an unexpected status is returned, and that is what re-drives.
    ///
    /// The skip traces at `warn`: the id named a number, so someone expected an
    /// object there, and swallowing the refusal silently would leave the miss
    /// with no trace anywhere.
    fn comment_on(&self, object: u64, key: &str, digest: Digest, human_body: &str) -> Result<(), GithubError> {
        match self.upsert_comment(object, key, digest, human_body) {
            Err(error) if refuses_comment(&error) => {
                tracing::warn!(
                    target: "aether_bloomery_github::projection",
                    object,
                    key,
                    error = %error,
                    "the repository refused a comment on the addressed object; skipping it rather than stalling the mirror"
                );
                Ok(())
            }
            other => other,
        }
    }

    fn upsert_comment(&self, object: u64, key: &str, digest: Digest, human_body: &str) -> Result<(), GithubError> {
        let marker = Marker { key: key.to_owned(), digest };
        let body = format!("{human_body}\n\n{}", render_marker(&marker));

        if let Some(existing) = self.client.find_comment(object, key)? {
            if existing.marker.as_ref().map(|m| m.digest) == Some(digest) {
                return Ok(()); // matching digest — no-op.
            }
            return self.client.update_comment(existing.id, &body);
        }

        self.client.create_comment(&NewComment { issue_number: object, body })?;
        Ok(())
    }
}

impl<C: GithubApi + CommissionProjectionApi> GithubProjection<C> {
    /// Reconcile one commission onto GitHub (ADR-0149 2026-08-16 amendment).
    ///
    /// A workpiece that names a repository object (`issue-<N>`) is a comment
    /// on that object: adopting the number would write a human-authored
    /// title and body. `Ok(None)` is that case — this projector owns no
    /// issue for it. Otherwise title and body are written only to
    /// `projection.recorded_issue` — the store row the reactor overlays at
    /// drain, originating from a successful create. That persisted number is
    /// the only ownership authority and is never discarded.
    ///
    /// When it is absent, this path creates a new replica rather than adopting
    /// a marker match. A copied marker on a human issue, an unrelated Bot
    /// issue, or an issue the projector's own identity authored is not a
    /// creation receipt. A crash between create and persist may therefore
    /// leave an orphan and mint a sibling on the next drain; data preservation
    /// wins over destructive heuristic recovery.
    pub fn project_owned_commission(&self, projection: &CommissionProjection) -> Result<Option<u64>, GithubError> {
        let key = commission_key(&projection.workpiece.0);
        let digest = content_digest("bloomery.commission", projection);
        let marker = render_marker(&Marker { key: key.clone(), digest });
        let reserved_chars = marker.chars().count() + 2;
        if let Some(number) = canonical_issue_number(&projection.workpiece.0) {
            let human = bound_body(&render_source_comment(projection), projection, reserved_chars);
            self.comment_on(number, &key, digest, &human)?;
            self.retire_replica(projection, number)?;
            return Ok(None);
        }

        let title = render_commission_title(projection);
        let human = bound_body(&render_commission_body(projection), projection, reserved_chars);
        let body = format!("{human}\n\n{marker}");

        // Only a durable recorded_issue from this projector's own create is
        // ownership. Marker search is not consulted: a match is not a receipt.
        if let Some(number) = projection.recorded_issue {
            self.client.update_issue(number, &title, &body)?;
            self.close_if_terminal(number, &projection.status)?;
            Ok(Some(number))
        } else {
            let created = self.client.create_issue(&NewIssue { title, body })?;
            self.close_if_terminal(created.number, &projection.status)?;
            Ok(Some(created.number))
        }
    }

    /// Close a replica this projector opened for a commission that now lives
    /// as a comment on `source`. A projector that never recorded a create
    /// leaves unknown issues alone — a marker match is not a replica to
    /// retire. Both the retirement comment and the close are idempotent.
    fn retire_replica(&self, projection: &CommissionProjection, source: u64) -> Result<(), GithubError> {
        let Some(replica) = projection.recorded_issue.filter(|&number| number != source) else {
            return Ok(());
        };
        let key = commission_key(&projection.workpiece.0);
        let retired_key = format!("{key}:retired");
        let body = format!("This replica is retired. The commission is tracked on #{source}.");
        self.comment_on(
            replica,
            &retired_key,
            content_digest("bloomery.commission.retired", &(source, replica)),
            &body,
        )?;
        CommissionProjectionApi::close_issue(&self.client, replica)?;
        Ok(())
    }

    fn close_if_terminal(&self, number: u64, status: &str) -> Result<(), GithubError> {
        if status == "landed" || status == "cancelled" {
            CommissionProjectionApi::close_issue(&self.client, number)?;
        }
        Ok(())
    }
}

impl<C: GithubApi + PullRequestApi + CommissionProjectionApi> ProjectionBackend for GithubProjection<C> {
    type Error = GithubError;

    fn reconcile_view(&self, view: &ViewDocument) -> Result<(), Self::Error> {
        for bloom in &view.blooms {
            for member in &bloom.members {
                // A workpiece with no GitHub home is an ordinary state, not a
                // fault: `GET /view` stays its authoritative view.
                let Some(object) = addressed_object(&member.workpiece) else {
                    continue;
                };
                self.comment_on(
                    object,
                    &member_key(bloom.id, &member.workpiece.0),
                    content_digest("bloomery.view.member", member),
                    &render_member_body(bloom.id, member),
                )?;
            }
        }
        Ok(())
    }

    /// Comment the landing receipt onto the bloom's landing pull request, if
    /// one exists. Member issues are not a target: the land reactor writes
    /// that comment as it closes the issue, so projecting it here would
    /// duplicate the sentence.
    fn project_receipt(&self, projected: &ProjectedReceipt) -> Result<(), Self::Error> {
        let receipt = &projected.receipt;
        let key = receipt_key(receipt.bloom);
        let digest = content_digest("bloomery.receipt", receipt);
        let body = render_receipt_body(receipt);

        // The landing pull request is a target, not a precondition — a bloom can
        // land through a path that opened none, and requiring one would wedge
        // those lanes (ADR-0149 §What each object is). Its branch name comes
        // from the source port's own spelling, so the receipt cannot point at a
        // proposal that was never opened under that name.
        if let Some(landing) = self.client.find_pull_request_for_head(&landing_branch(&receipt.bloom))? {
            self.comment_on(landing.number, &key, digest, &body)?;
        }
        Ok(())
    }

    fn project_commission(&self, projection: &CommissionProjection) -> Result<Option<u64>, Self::Error> {
        self.project_owned_commission(projection)
    }
}

/// The object a workpiece addresses in the configured repository, if any.
///
/// A [`WorkpieceId`] addresses one iff it is exactly `issue-<N>` with `<N>` a
/// canonical decimal — non-zero, no leading zeros, no sign, no surrounding
/// space. Any other shape is unaddressable on GitHub, which is an ordinary
/// state rather than a fault (ADR-0149 §Addressing). The number resolves
/// against whatever the repository holds: a closed issue is a target like any
/// other, and so is a pull request, since GitHub numbers both from one sequence
/// and shares the comment route.
///
/// The unaddressable case traces at `debug`, not `warn`: it is the steady state
/// of the local and fixture lanes, whose workpiece ids are never issue numbers,
/// so a louder level would be noise on every reconcile they drive.
fn addressed_object(workpiece: &WorkpieceId) -> Option<u64> {
    let object = canonical_issue_number(&workpiece.0);
    if object.is_none() {
        tracing::debug!(
            target: "aether_bloomery_github::projection",
            workpiece = workpiece.0.as_str(),
            "workpiece addresses no object in the configured repository; projecting nothing for it"
        );
    }
    object
}

/// The `<N>` of a canonical `issue-<N>` id, if `id` is one.
///
/// The one spelling of "which object does this workpiece name", so the landing
/// assembly's `Closes #N` lines address exactly what the projection comments on
/// — an id this refuses contributes no closing line rather than a guessed
/// number.
#[must_use]
pub fn canonical_issue_number(id: &str) -> Option<u64> {
    let number = id.strip_prefix(ISSUE_PREFIX)?;
    // `str::parse` would accept `+7` and ` 7`, and would read `007` as 7 — three
    // spellings of one object, so three markers on it. Canonical or nothing.
    if number.is_empty() || number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// Whether `error` is the target refusing the write rather than a transport
/// fault: absent (404, or 410 for a deleted object) or locked / inaccessible
/// against comment (403). Permanent for that target, so it is recorded and
/// skipped. An allowance refusal is `429` by the time it reaches here — the
/// client normalizes primary, secondary, and withheld windows to that status —
/// and does not match, so the comment re-drives rather than acking as delivered.
fn refuses_comment(error: &GithubError) -> bool {
    matches!(error, GithubError::Status { status: 403 | 404 | 410, .. })
}

fn commission_key(workpiece: &str) -> String {
    format!("commission:{workpiece}")
}

fn render_commission_title(projection: &CommissionProjection) -> String {
    // The replica title is the repository's issue-title rule, a readable
    // rendering of the intent heading under that rule, or a floor that
    // satisfies it. Lifecycle lives in the issue's open/closed state; a
    // ` — {status}` suffix would rewrite the title on every transition and
    // re-run the label workflow for nothing.
    let title = projection.title.trim();
    if title.is_empty() {
        commission_floor_title(&projection.workpiece.0)
    } else if issue_title_is_valid(title) {
        title.to_owned()
    } else {
        readable_title(title)
    }
}

fn render_commission_body(projection: &CommissionProjection) -> String {
    let mut body = String::new();
    body.push_str(
        "**Bloomery replica** — do not edit this issue. It is an outbound projection of a local \
         commission (ADR-0199). Edits here are overwritten and are never read as input.\n",
    );
    push_commission_content(&mut body, projection);
    body
}

fn render_source_comment(projection: &CommissionProjection) -> String {
    let mut body = String::new();
    push_commission_content(&mut body, projection);
    body
}

/// The commission itself as a person would file it: the intent text verbatim,
/// then — when a current revision exists — a rule and the rendered work
/// order, then a short footer beside the store marker the caller appends.
fn push_commission_content(body: &mut String, projection: &CommissionProjection) {
    if let Some(intent) = projection.intent_text.as_deref().filter(|intent| !intent.trim().is_empty()) {
        body.push('\n');
        body.push_str(intent);
        if !intent.ends_with('\n') {
            body.push('\n');
        }
    }
    if let Some(scope) = projection.scope.as_deref().map(str::trim).filter(|scope| !scope.is_empty()) {
        body.push_str("\n---\n\n");
        body.push_str(scope);
        if !scope.ends_with('\n') {
            body.push('\n');
        }
    }
    body.push('\n');
    let _ = writeln!(body, "- Workpiece: `{}`", projection.workpiece.0);
    match projection.approval_signer.as_deref() {
        Some(signer) => {
            let _ = writeln!(body, "- Approval: signer `{signer}`.");
        }
        None => {
            let _ = writeln!(body, "- Approval: _none_");
        }
    }
    let _ = writeln!(body, "- State: {}", projection.status);
}

/// Bound a commission's marker-less human body so the marker-appended final
/// stays under [`MAX_COMMISSION_BODY_CHARS`]. `reserved_chars` is the marker
/// plus its separators, measured by the caller off the same marker the final
/// carries — the bound is on the body the repository sees, not on the prefix
/// alone.
///
/// An over-long intent or work order is cut on a character boundary and the
/// declared-surface block plus the footer ride intact: the surface is what the
/// seal door checks, so it is the one block the mirror must never paraphrase.
/// A truncation line names the stored digest the rest can be read from.
#[must_use]
fn bound_body(human: &str, projection: &CommissionProjection, reserved_chars: usize) -> String {
    let budget = MAX_COMMISSION_BODY_CHARS.saturating_sub(reserved_chars);
    if human.chars().count() <= budget {
        return human.to_owned();
    }

    let line = truncation_line(projection);
    let tail = intact_tail(human);
    let tail_chars = tail.chars().count();
    let line_chars = line.chars().count();
    if tail_chars + line_chars >= budget {
        let kept: String = human.chars().take(budget.saturating_sub(line_chars)).collect();
        return format!("{kept}{line}");
    }

    let prefix = &human[..human.len() - tail.len()];
    let kept: String = prefix.chars().take(budget - tail_chars - line_chars).collect();
    format!("{kept}{line}{tail}")
}

/// The line a bounded body carries where the cut happened: which stored value
/// holds the rest. A scope cut names its revision; an intent-only commission
/// has no revision, so its cut names the intent statement instead.
#[must_use]
fn truncation_line(projection: &CommissionProjection) -> String {
    projection.scope_revision.map_or_else(
        || {
            let digest = short_hex(&projection.intent);
            format!("\n… truncated; read the stored intent `{digest}`.\n")
        },
        |revision| {
            let digest = short_hex(&revision);
            format!("\n… truncated; read the stored revision `{digest}`.\n")
        },
    )
}

/// The suffix a bound body keeps verbatim: the declared-surface block through
/// the footer. Without a surface heading — an intent-only commission, or a
/// scope carrying none — the footer alone is what must survive.
#[must_use]
fn intact_tail(human: &str) -> &str {
    for heading in ["\n## Declared surface\n", "\n## Declared crates\n"] {
        if let Some(start) = human.find(heading) {
            return &human[start..];
        }
    }
    if let Some(start) = human.find("\n- Workpiece:") {
        return &human[start..];
    }
    ""
}

fn member_key(bloom: BloomId, workpiece: &str) -> String {
    format!("member:{workpiece}@bloom:{}", short_hex(&bloom.0))
}

fn receipt_key(bloom: BloomId) -> String {
    format!("receipt:bloom:{}", short_hex(&bloom.0))
}

/// sha256 over a domain tag (null-separated) and the value's canonical JSON —
/// the change-detection digest a marker carries. Not the control core's
/// `digest_of` (this is a projection-local change key, not a persisted
/// content address), but stable for a given value so a re-render no-ops.
fn content_digest<T: Serialize>(domain: &str, value: &T) -> Digest {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0u8]);
    let bytes = serde_json::to_vec(value).expect("view values serialize to json");
    hasher.update(&bytes);
    Digest::from_bytes(hasher.finalize().into())
}

/// The whole member render, folded into one comment body. Everything here
/// derives from one [`MemberView`] and changes with it, so one marker digest
/// over the whole view decides create / update / no-op for all of it.
fn render_member_body(bloom: BloomId, member: &MemberView) -> String {
    let mut body = format!("**Bloomery** — admitted into bloom `{}`.\n\n", short_hex(&bloom.0));

    let _ = writeln!(body, "- Scope revision: `{}`", short_hex(&member.scope_revision));
    let _ = writeln!(
        body,
        "- Approval: {:?} bound to `{}` (detail `{}`).",
        member.approval.kind,
        short_hex(&member.approval.subject),
        short_hex(&member.approval.detail)
    );
    let _ = writeln!(body, "- State: {}", member_state(member));

    if let Some(cursor) = &member.cursor {
        let _ = writeln!(body, "- Stage: {:?} (attempts {}).", cursor.stage, cursor.attempts);
    }
    if let Some(fault) = &member.host_fault {
        let _ = writeln!(body, "- **Host fault**: {}", fault.findings);
    }
    if let Some(park) = &member.park {
        let _ = writeln!(
            body,
            "- **Parked** at {:?}: construct concluded without a candidate. Evidence: `{}`.",
            park.stage,
            short_hex(&park.evidence)
        );
    }
    if let Some(eviction) = &member.evicted_by {
        let _ = writeln!(body, "- **Evicted** by `{}` on `{}`.", eviction.by.0, eviction.path);
    }
    if let Some(withdrawal) = &member.withdrawn {
        match &withdrawal.depends_on {
            Some(depends_on) => {
                let _ = writeln!(
                    body,
                    "- **Withdrawn** ({} of `{}`): {} — operator `{}`.",
                    withdrawal.cause, depends_on.0, withdrawal.reason, withdrawal.operator
                );
            }
            None => {
                let _ = writeln!(
                    body,
                    "- **Withdrawn** ({}): {} — operator `{}`.",
                    withdrawal.cause, withdrawal.reason, withdrawal.operator
                );
            }
        }
    }

    if let Some(blocker) = &member.blocked_by {
        let _ = writeln!(body, "- **Blocked** by `{}`: construct waits until that ancestor resolves.", blocker.0);
    }

    if let Some(resolution) = &member.resolution {
        let _ = writeln!(
            body,
            "- Resolution: candidate `{}` resolves this workpiece at scope `{}`.",
            short_hex(&resolution.candidate),
            short_hex(&resolution.scope_revision)
        );
    }

    // A wedge is terminal, so it is stated rather than left to be inferred from
    // a member that simply stops changing.
    if let Some(wedge) = &member.wedge {
        let _ = writeln!(
            body,
            "- **Wedged** at {:?}: the stage's retry budget is spent, so this member has stopped \
             dispatching and the bloom cannot resolve. Superseding the bloom is the escape. \
             Failing evidence: `{}`.",
            wedge.stage,
            short_hex(&wedge.evidence)
        );
    }

    if let Some(pending) = &member.pending_decision {
        push_pending_decision(&mut body, pending);
    }

    if let Some(awaiting) = &member.awaiting_surface {
        push_awaiting_surface(&mut body, awaiting);
    }

    body
}

fn member_state(member: &MemberView) -> String {
    // A withdrawal is a person's one-way decision: it outranks a wedge, a
    // resolution, or any still-working hold that might also be on the view.
    if member.withdrawn.is_some() {
        "withdrawn".to_owned()
    } else if let Some(wedge) = &member.wedge {
        format!("**wedged** at {:?}", wedge.stage)
    } else if member.resolution.is_some() {
        "integrated".to_owned()
    } else if let Some(blocker) = &member.blocked_by {
        format!("blocked by `{}`", blocker.0)
    } else if let Some(eviction) = &member.evicted_by {
        format!("evicted by `{}`", eviction.by.0)
    } else if member.host_fault.is_some() {
        "held on host".to_owned()
    } else {
        "in progress".to_owned()
    }
}

/// The parked-question section of a member's comment (ADR-0151): visible where
/// a person already looks, carrying the question digest as the stable metadata
/// an adopting answer names. A projected comment is an outward mirror only —
/// never a command (ADR-0149 §The boundary).
fn push_pending_decision(body: &mut String, pending: &PendingDecisionView) {
    let _ = writeln!(body, "\n**Decision needed** — parked on question `{}`.\n", short_hex(&pending.question));
    let _ = writeln!(body, "{}\n", pending.prompt);
    let _ = writeln!(body, "- Held stage: {:?}", pending.stage);
    let _ = writeln!(body, "- Blocked: {}", pending.blocked);

    if !pending.options.is_empty() {
        let _ = writeln!(body, "\nOptions:");
        for (index, option) in pending.options.iter().enumerate() {
            let _ = writeln!(body, "{}. {option}", index + 1);
        }
    }

    let _ = writeln!(
        body,
        "\nAnswer natively — a signed statement adopting question `{}`; a comment never becomes a command.",
        short_hex(&pending.question)
    );
}

/// The surface-amendment section of a member's comment (ADR-0207): the paths a
/// declining lane asked for, named where a person already looks so the remedy
/// is readable without opening an evidence file. An outward mirror only —
/// widening the surface is an authored successor, never a comment.
fn push_awaiting_surface(body: &mut String, awaiting: &AwaitingSurfaceView) {
    let _ = writeln!(
        body,
        "\n**Surface needed** — the lane declined at {:?} against scope revision `{}`.\n",
        awaiting.stage,
        short_hex(&awaiting.scope_revision)
    );
    let _ = writeln!(body, "{}\n", awaiting.summary);
    for request in &awaiting.paths {
        let _ = writeln!(body, "- `{}` — {}", request.path, request.reason);
    }
    let _ = writeln!(
        body,
        "\nRequests so far: {}. Widening the surface is an authored successor scope revision.",
        awaiting.requests
    );
}

fn render_receipt_body(receipt: &LandingReceipt) -> String {
    format!(
        "**Landed** — bloom `{}` landed; mainline moved `{}` → `{}`.",
        short_hex(&receipt.bloom.0),
        short_hex(&receipt.previous_base),
        short_hex(&receipt.new_head)
    )
}

#[cfg(test)]
mod tests {
    use aether_bloomery::{CommissionProjection, Digest, WorkpieceId};

    use super::{GithubProjection, addressed_object, commission_key};
    use crate::client::{CommissionProjectionApi, NewIssue};
    use crate::fixture::FakeGithub;
    use crate::landing::{commission_floor_title, issue_title_is_valid};
    use crate::marker::{Marker, parse_marker, render_marker};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn commission(workpiece: &str, recorded_issue: Option<u64>) -> CommissionProjection {
        CommissionProjection {
            workpiece: WorkpieceId(workpiece.to_owned()),
            intent: digest(1),
            scope_revision: Some(digest(2)),
            approval_signer: Some("operator".to_owned()),
            approval_digest: Some(digest(3)),
            status: "open".to_owned(),
            recorded_issue,
            title: String::new(),
            scope: None,
            intent_text: None,
        }
    }

    fn copied_marker_body(workpiece: &str, preamble: &str) -> String {
        format!("{preamble}\n\n{}", render_marker(&Marker { key: commission_key(workpiece), digest: digest(9) }))
    }

    fn assert_untouched(projection: &GithubProjection<FakeGithub>, number: u64, title: &str, body: &str) {
        assert_eq!(projection.client().issue_title(number).as_deref(), Some(title), "title of #{number}");
        assert_eq!(projection.client().issue_body(number).as_deref(), Some(body), "body of #{number}");
        assert_eq!(projection.client().issue_is_closed(number), Some(false), "#{number} must stay open");
    }

    #[test]
    fn only_a_canonical_issue_number_addresses_an_object() {
        // Tripwire: this predicate is the whole write-targeting rule. Every
        // rejected spelling below is one a lenient `parse` would accept as an
        // alias of a real object, so a slip would write the same member's
        // marker onto one issue under several keys — or, for `issue-0`, onto a
        // number GitHub never assigns.
        let addressed = |id: &str| addressed_object(&WorkpieceId(id.into()));

        assert_eq!(addressed("issue-4628"), Some(4628));
        assert_eq!(addressed("issue-1"), Some(1));

        assert_eq!(addressed("issue-0"), None, "zero is not an issue number");
        assert_eq!(addressed("issue-04628"), None, "a leading zero is a second spelling of one object");
        assert_eq!(addressed("issue-+7"), None, "a sign is a second spelling of one object");
        assert_eq!(addressed("issue- 7"), None, "surrounding space is a second spelling of one object");
        assert_eq!(addressed("issue-7 "), None, "trailing space is a second spelling of one object");
        assert_eq!(addressed("issue-"), None);
        assert_eq!(addressed("issue-abc"), None);
        assert_eq!(addressed("reactor-core"), None, "a local-lane workpiece has no GitHub home");
        assert_eq!(addressed("issue-99999999999999999999"), None, "a number past u64 addresses nothing");
    }

    #[test]
    fn a_copied_marker_on_a_foreign_human_issue_is_not_adopted() {
        // A person pastes the projection marker onto an issue they authored.
        // Without a recorded create, that match must not update or close it.
        let planted = 11;
        let title = "human title";
        let body = copied_marker_body("wp-1", "a person wrote this");
        let fake = FakeGithub::new();
        fake.seed_issue_with_title(planted, title, &body);
        let projection = GithubProjection::new(fake);

        let owned = projection.project_owned_commission(&commission("wp-1", None)).expect("project");

        assert!(owned.is_some(), "a commission with no GitHub home still gets its own replica");
        assert_ne!(owned, Some(planted), "a copied marker is not a creation receipt");
        assert_untouched(&projection, planted, title, &body);
        assert_eq!(projection.client().updated_issue_count(), 0);
        assert_eq!(projection.client().issue_is_closed(planted), Some(false));
    }

    #[test]
    fn a_copied_marker_on_a_foreign_bot_issue_is_not_adopted() {
        // An unrelated Bot issue, or a Bot issue later edited to carry this
        // commission's marker, is still not this projector's recorded create.
        let planted = 12;
        let title = "unrelated bot replica";
        let body = copied_marker_body("wp-1", "**Bloomery replica** — do not edit this issue.");
        let fake = FakeGithub::new();
        fake.seed_issue_with_title(planted, title, &body);
        let projection = GithubProjection::new(fake);

        let owned = projection.project_owned_commission(&commission("wp-1", None)).expect("project");

        assert!(owned.is_some(), "a commission with no GitHub home still gets its own replica");
        assert_ne!(owned, Some(planted), "Bot authorship is not a creation receipt");
        assert_untouched(&projection, planted, title, &body);
        assert_eq!(projection.client().updated_issue_count(), 0);
        assert_eq!(projection.client().issue_is_closed(planted), Some(false));
    }

    #[test]
    fn a_copied_marker_on_the_projector_identity_is_not_adopted_without_a_receipt() {
        // Same authenticated identity as the projector, marker present, no
        // durable recorded_issue. Crash-before-receipt may mint a sibling;
        // the unmarked-as-recorded issue is not closed or overwritten.
        let fake = FakeGithub::new();
        let projection = GithubProjection::new(fake);
        let title = "same-identity copy";
        let body = copied_marker_body("wp-1", "copied onto an issue this identity opened");
        let planted = CommissionProjectionApi::create_issue(
            projection.client(),
            &NewIssue { title: title.into(), body: body.clone() },
        )
        .expect("plant")
        .number;

        let owned = projection.project_owned_commission(&commission("wp-1", None)).expect("project");

        assert!(owned.is_some(), "crash-before-receipt may mint a sibling replica");
        assert_ne!(owned, Some(planted), "same-identity create without a recorded receipt is not owned");
        assert_untouched(&projection, planted, title, &body);
        assert_eq!(projection.client().updated_issue_count(), 0);
        assert_eq!(projection.client().issue_is_closed(planted), Some(false));
    }

    #[test]
    fn a_landed_projection_does_not_close_a_marker_match_without_a_receipt() {
        let planted = 13;
        let title = "human landed bait";
        let body = copied_marker_body("wp-1", "leave me open");
        let fake = FakeGithub::new();
        fake.seed_issue_with_title(planted, title, &body);
        let projection = GithubProjection::new(fake);
        let mut landed = commission("wp-1", None);
        landed.status = "landed".to_owned();

        let owned = projection.project_owned_commission(&landed).expect("project");

        assert_ne!(owned, Some(planted));
        assert_eq!(projection.client().issue_is_closed(planted), Some(false), "the bait stays open");
        assert_untouched(&projection, planted, title, &body);
    }

    #[test]
    fn retire_replica_without_a_receipt_leaves_unknown_issues_alone() {
        let source = 42;
        let stray = 99;
        let title = "forged replica";
        let body = copied_marker_body("issue-42", "copied marker");
        let fake = FakeGithub::new();
        fake.seed_issue(source, "human source");
        fake.seed_issue_with_title(stray, title, &body);
        let projection = GithubProjection::new(fake);

        projection.project_owned_commission(&commission("issue-42", None)).expect("comment on the named source");

        assert_untouched(&projection, stray, title, &body);
        assert_eq!(projection.client().issue_is_closed(stray), Some(false));
        assert_eq!(projection.client().updated_issue_count(), 0);
        assert_eq!(projection.client().comments_on(source).len(), 1, "the source still carries the commission");
    }

    #[test]
    fn a_recorded_own_creation_still_reconciles_and_closes() {
        let projection = GithubProjection::new(FakeGithub::new());
        let number =
            projection.project_owned_commission(&commission("wp-1", None)).expect("create").expect("owns a replica");
        projection.client().edit_issue(number, "a person renamed this", "a person rewrote the body");

        let recorded = projection.project_owned_commission(&commission("wp-1", Some(number))).expect("overwrite");
        assert_eq!(recorded, Some(number), "the recorded create is still the replica");
        let title = projection.client().issue_title(number).expect("the replica still exists");
        let body = projection.client().issue_body(number).expect("the replica still exists");
        assert_eq!(title, commission_floor_title("wp-1"));
        assert!(body.contains("do not edit"), "the replica notice is restored: {body}");
        assert!(!body.contains("a person rewrote the body"), "the human body is not kept: {body}");

        let mut landed = commission("wp-1", Some(number));
        landed.status = "landed".to_owned();
        projection.project_owned_commission(&landed).expect("close");
        assert_eq!(projection.client().issue_is_closed(number), Some(true), "terminal close of a recorded replica");
    }

    #[test]
    fn a_commission_with_intent_but_no_revision_mirrors_the_intent_not_bookkeeping() {
        // #6022: a retrospect commission has an intent but no scope revision,
        // and rendered as bookkeeping only. The replica carries the verbatim
        // intent above the footer, and the title falls back to a readable
        // rendering of the non-conventional heading rather than the id floor.
        let projection = GithubProjection::new(FakeGithub::new());
        let mut open = commission("retrospect-eb725152c84c", None);
        open.scope_revision = None;
        open.title = "A leak in the landing path".to_owned();
        open.intent_text = Some("# A leak in the landing path\n\nThe reader saw it and will not fix it.\n".to_owned());

        let number = projection.project_owned_commission(&open).expect("create").expect("owns a replica");
        let title = projection.client().issue_title(number).expect("the replica exists");
        let body = projection.client().issue_body(number).expect("the replica exists");
        assert_eq!(title, "chore(bloomery): a leak in the landing path");
        assert!(issue_title_is_valid(&title), "{title}");
        assert!(
            body.contains("# A leak in the landing path\n\nThe reader saw it and will not fix it.\n"),
            "the finding text: {body}",
        );
        assert!(!body.contains("Scope: _none_"), "no bookkeeping placeholder: {body}");
        assert!(body.contains("- Workpiece: `retrospect-eb725152c84c`"), "the footer: {body}");
    }

    #[test]
    fn a_canonical_commissions_comment_reads_the_same_minus_the_banner() {
        // A canonical `issue-N` commission's comment on its own issue renders
        // the same content minus the banner.
        let fake = FakeGithub::new();
        fake.seed_issue(42, "human");
        let projection = GithubProjection::new(fake);
        let mut open = commission("issue-42", None);
        open.scope_revision = None;
        open.title = "A leak in the landing path".to_owned();
        open.intent_text = Some("# A leak in the landing path\n\nThe reader saw it and will not fix it.\n".to_owned());

        projection.project_owned_commission(&open).expect("comment on the named source");

        let comments = projection.client().comments_on(42);
        assert_eq!(comments.len(), 1);
        let comment = &comments[0];
        assert!(comment.contains("The reader saw it and will not fix it."), "the finding text: {comment}");
        assert!(!comment.contains("do not edit"), "no replica preamble on a human issue: {comment}");
    }

    #[test]
    fn intent_text_joins_the_change_detection_digest() {
        // The digest covers the new field: `Some("")` renders no words, so
        // the human content is identical — yet the stored marker must differ,
        // and the re-drive must settle rather than flap. Every existing
        // replica therefore re-renders exactly once after the upgrade.
        let fake = FakeGithub::new();
        fake.seed_issue(42, "human");
        let projection = GithubProjection::new(fake);
        let mut open = commission("issue-42", None);
        open.scope_revision = None;

        projection.project_owned_commission(&open).expect("baseline comment");
        let baseline = projection.client().comments_on(42);
        assert_eq!(baseline.len(), 1);
        let baseline_marker = parse_marker(&baseline[0]).expect("the comment carries a marker");

        projection.project_owned_commission(&open).expect("identical re-drive");
        assert_eq!(projection.client().comments_on(42), baseline, "a matching digest is a no-op");

        open.intent_text = Some(String::new());
        projection.project_owned_commission(&open).expect("re-render");
        let rerendered = projection.client().comments_on(42);
        assert_eq!(rerendered.len(), 1, "the change edits the comment in place");
        let rerendered_marker = parse_marker(&rerendered[0]).expect("the comment carries a marker");
        assert_ne!(rerendered_marker.digest, baseline_marker.digest, "the new field joins the digest");
        assert_eq!(
            rerendered[0].replace(&render_marker(&rerendered_marker), ""),
            baseline[0].replace(&render_marker(&baseline_marker), ""),
            "the human content is unchanged: only the digest moved",
        );

        projection.project_owned_commission(&open).expect("settle");
        assert_eq!(projection.client().comments_on(42), rerendered, "the second pass is a no-op");
    }

    #[test]
    fn a_recorded_stray_replica_still_retires_onto_its_source() {
        let fake = FakeGithub::new();
        fake.seed_issue(42, "human");
        let projection = GithubProjection::new(fake);
        let replica = projection
            .project_owned_commission(&commission("wp-before-home", None))
            .expect("create stray")
            .expect("owns a replica");

        projection
            .project_owned_commission(&commission("issue-42", Some(replica)))
            .expect("retire onto the named source");

        assert_eq!(projection.client().issue_is_closed(replica), Some(true), "the recorded stray closes");
        assert_eq!(projection.client().issue_is_closed(42), Some(false), "the source stays open");
        assert_eq!(projection.client().comments_on(42).len(), 1, "the source carries the commission");
    }
}
