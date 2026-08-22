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
//! addresses only a number recorded from this projector's own create (or
//! found by its own marker after a crash between create and persist).
//!
//! The projector reads only its own markers; free-form platform content is
//! never interpreted as intent. GitHub edits of a replica are overwritten
//! on the next projection.
//!
//! [#4663]: https://github.com/iamacoffeepot/aether/issues/4663

use std::fmt::Write as _;

use aether_bloomery::{
    AwaitingSurfaceView, BloomId, CommissionProjection, Digest, LandingReceipt, MemberView, PendingDecisionView,
    ProjectedReceipt, ProjectionBackend, ViewDocument, WorkpieceId,
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
    /// drain — or, when that is still absent, to a number
    /// [`CommissionProjectionApi::find_issue`] returns for this commission's
    /// marker. Search is advisory crash-recovery after a create that has
    /// not yet been persisted; it is not the create-vs-update authority.
    pub fn project_owned_commission(&self, projection: &CommissionProjection) -> Result<Option<u64>, GithubError> {
        let key = commission_key(&projection.workpiece.0);
        let digest = content_digest("bloomery.commission", projection);
        if let Some(number) = canonical_issue_number(&projection.workpiece.0) {
            self.comment_on(number, &key, digest, &render_source_comment(projection))?;
            self.retire_replica(projection, number)?;
            return Ok(None);
        }

        let title = render_commission_title(projection);
        let body = format!(
            "{}\n\n{}",
            render_commission_body(projection),
            render_marker(&Marker { key: key.clone(), digest })
        );

        // The recorded number is the authority. Search recovers a create that
        // has not been persisted yet; a lagging index must not mint a sibling
        // when the store already owns a number.
        let owned = match projection.recorded_issue {
            Some(number) => Some(number),
            None => self.client.find_issue(&key)?.map(|issue| issue.number),
        };

        if let Some(number) = owned {
            if let Some(existing) = self.client.find_issue(&key)?
                && existing.number == number
                && existing.marker.as_ref().map(|marker| marker.digest) == Some(digest)
            {
                self.close_if_terminal(number, &projection.status)?;
                return Ok(Some(number));
            }
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
    /// as a comment on `source`. A projector that never created one does
    /// nothing. Both the retirement comment and the close are idempotent.
    fn retire_replica(&self, projection: &CommissionProjection, source: u64) -> Result<(), GithubError> {
        let key = commission_key(&projection.workpiece.0);
        let stray = match projection.recorded_issue {
            Some(number) if number != source => Some(number),
            Some(_) => None,
            None => self.client.find_issue(&key)?.map(|issue| issue.number).filter(|&number| number != source),
        };
        let Some(replica) = stray else {
            return Ok(());
        };
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
/// fault: absent (404, or 410 for a deleted object) or locked against comment
/// (403). Permanent for that target, so it is recorded and skipped.
fn refuses_comment(error: &GithubError) -> bool {
    matches!(error, GithubError::Status { status: 403 | 404 | 410, .. })
}

fn commission_key(workpiece: &str) -> String {
    format!("commission:{workpiece}")
}

fn render_commission_title(projection: &CommissionProjection) -> String {
    // The replica title is the repository's issue-title rule or a floor that
    // satisfies it. Lifecycle lives in the issue's open/closed state; a
    // ` — {status}` suffix would rewrite the title on every transition and
    // re-run the label workflow for nothing.
    let title = projection.title.trim();
    if issue_title_is_valid(title) {
        title.to_owned()
    } else {
        commission_floor_title(&projection.workpiece.0)
    }
}

fn render_commission_body(projection: &CommissionProjection) -> String {
    format!(
        "**Bloomery replica** — do not edit this issue. It is an outbound projection of a local \
         commission (ADR-0199). Edits here are overwritten and are never read as input.\n\n{}",
        render_commission_fields(projection)
    )
}

fn render_source_comment(projection: &CommissionProjection) -> String {
    render_commission_fields(projection)
}

fn render_commission_fields(projection: &CommissionProjection) -> String {
    let mut body = String::new();
    let _ = writeln!(body, "- Workpiece: `{}`", projection.workpiece.0);
    let _ = writeln!(body, "- Intent: `{}`", short_hex(&projection.intent));
    match projection.scope_revision {
        Some(digest) => {
            let _ = writeln!(body, "- Scope revision: `{}`", short_hex(&digest));
        }
        None => {
            let _ = writeln!(body, "- Scope revision: _none_");
        }
    }
    match (&projection.approval_signer, projection.approval_digest) {
        (Some(signer), Some(digest)) => {
            let _ = writeln!(body, "- Approval: signer `{signer}` digest `{}`.", short_hex(&digest));
        }
        (_, Some(digest)) => {
            let _ = writeln!(body, "- Approval: digest `{}`.", short_hex(&digest));
        }
        _ => {
            let _ = writeln!(body, "- Approval: _none_");
        }
    }
    let _ = writeln!(body, "- State: {}", projection.status);
    body
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
    match (&member.resolution, &member.wedge, &member.blocked_by) {
        (_, Some(wedge), _) => format!("**wedged** at {:?}", wedge.stage),
        (Some(_), None, _) => "integrated".to_owned(),
        (None, None, Some(blocker)) => format!("blocked by `{}`", blocker.0),
        (None, None, None) => "in progress".to_owned(),
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
    use aether_bloomery::WorkpieceId;

    use super::addressed_object;

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
}
