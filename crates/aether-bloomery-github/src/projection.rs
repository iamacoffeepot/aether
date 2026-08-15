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
//! - A landing receipt → one comment per bloom on every resolvable member
//!   issue, and on the landing pull request when one exists.
//! - A bloom has no object of its own. Before it lands there is nothing to
//!   aggregate that `GET /view` does not serve live; afterwards its landing
//!   pull request *is* the aggregate (ADR-0149 §What each object is).
//!
//! # The write surface is comments only
//!
//! A projection creates and updates only comments carrying its own marker, on
//! objects it did not create. It writes no title and no body, and it never
//! opens, closes, reopens, locks, labels, assigns, or merges. That bound holds
//! by **absence**: [`GithubApi`] carries no verb that could address a
//! human-authored title or body, so no method reachable from here can.
//!
//! The projector reads only its own markers; free-form platform content is
//! never interpreted as intent.
//!
//! [#4663]: https://github.com/iamacoffeepot/aether/issues/4663

use std::fmt::Write as _;

use aether_bloomery::{
    BloomId, Digest, LandingReceipt, MemberView, PendingDecisionView, ProjectedReceipt, ProjectionBackend,
    ViewDocument, WorkpieceId,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::client::{GithubApi, GithubError, NewComment, PullRequestApi};
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

impl<C: GithubApi + PullRequestApi> ProjectionBackend for GithubProjection<C> {
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

    fn project_receipt(&self, projected: &ProjectedReceipt) -> Result<(), Self::Error> {
        let receipt = &projected.receipt;
        let key = receipt_key(receipt.bloom);
        let digest = content_digest("bloomery.receipt", receipt);
        let body = render_receipt_body(receipt);

        for workpiece in &projected.members {
            let Some(object) = addressed_object(workpiece) else {
                continue;
            };
            self.comment_on(object, &key, digest, &body)?;
        }

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
