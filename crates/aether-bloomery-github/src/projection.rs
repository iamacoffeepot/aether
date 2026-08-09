//! The projection backend (#3459 step 5): reconcile a [`ViewDocument`] into
//! GitHub objects.
//!
//! Every projected object carries a stable [`Marker`] — its internal Bloomery
//! key plus a content digest of the desired render. Reconcile is *find by key
//! → compare digest → create / update / no-op*, so:
//!
//! - **Idempotent** — reconciling the same document twice is all no-ops, since
//!   the second pass finds each object with a matching digest.
//! - **Rebuildable** — a deleted comment leaves no marker to find, so the next
//!   reconcile recreates it.
//!
//! # What maps to what
//!
//! - Each bloom's aggregate view → a **comment on its landing pull request**,
//!   keyed by the bloom id. The PR is `landing_branch(bloom)` (`bloom/<hex>/landing`)
//!   found via [`PullRequestApi::find_pull_request_for_head`]. If the PR does
//!   not exist yet (the first view raced ahead of the source port's propose),
//!   the bloom view is skipped explicitly — no shadow issue is opened.
//! - Each member → **comments on its source issue**, keyed by workpiece *and*
//!   bloom so a successor bloom's re-admission is its own faithful projection.
//!   The source issue number is parsed from the [`WorkpieceId`]: `issue-4628`
//!   maps to issue 4628, any other id has no source-issue home and is skipped
//!   explicitly (no shadow issue).
//! - Each member's approval, resolution, and parked question → **comments**
//!   on that source issue (visible where a person already looks, ADR-0151).
//! - A [`LandingReceipt`] → a comment on the bloom's landing pull request and,
//!   where derivable, on the source issues of that bloom. If the PR does not
//!   exist yet the receipt's PR comment is skipped; if a member has no
//!   source-issue home its receipt comment is skipped. No shadow issue is
//!   ever opened for either.
//!
//! The projector reads only its own markers; free-form platform content is
//! never interpreted as intent. No projection path calls the repository-wide
//! issue-list walk (`find_issue`).

use std::fmt::Write as _;

use aether_bloomery::{
    BloomId, BloomView, Digest, Evidence, LandingReceipt, MemberView, PendingDecisionView, ProjectionBackend,
    ResolutionClaim, ViewDocument, WorkpieceId,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::client::{GithubApi, GithubError, NewComment, PullRequestApi};
use crate::marker::{Marker, render_marker};
use crate::short_hex;

/// The outward projection mirror over a [`GithubApi`] + [`PullRequestApi`] client.
pub struct GithubProjection<C: GithubApi + PullRequestApi> {
    client: C,
}

impl<C: GithubApi + PullRequestApi> GithubProjection<C> {
    /// Build a projection over `client`.
    pub const fn new(client: C) -> Self {
        Self { client }
    }

    /// Borrow the underlying client (test introspection / receipt routing).
    pub const fn client(&self) -> &C {
        &self.client
    }

    fn reconcile_bloom(&self, bloom: &BloomView) -> Result<(), GithubError> {
        // Aggregate bloom view onto its landing PR, if the PR exists.
        let branch = landing_branch(bloom.id);
        if let Some(pr) = self.client.find_pull_request_for_head(&branch)? {
            let key = umbrella_key(bloom.id);
            let digest = content_digest("bloomery.view.bloom", bloom);
            let body = render_bloom_body(bloom);
            self.upsert_comment(pr.number, &key, digest, &body)?;
        } else {
            // Landing PR not yet proposed — explicit skip, no shadow issue.
        }

        for member in &bloom.members {
            let Some(issue_number) = source_issue_number(&member.workpiece) else {
                // No source-issue home for this workpiece — explicit skip, no shadow issue.
                continue;
            };
            let key = member_key(bloom.id, &member.workpiece.0);
            let digest = content_digest("bloomery.view.member", member);
            let body = render_member_body(bloom.id, member);
            self.upsert_comment(issue_number, &key, digest, &body)?;
            self.reconcile_member_evidence(issue_number, member)?;
        }
        Ok(())
    }

    fn reconcile_member_evidence(&self, issue_number: u64, member: &MemberView) -> Result<(), GithubError> {
        let approval_key = format!("approval:{}", short_hex(&member.approval.subject));
        let approval_digest = content_digest("bloomery.view.evidence", &member.approval);
        self.upsert_comment(issue_number, &approval_key, approval_digest, &render_evidence_body(&member.approval))?;

        if let Some(resolution) = &member.resolution {
            let key = format!("resolution:{}", member.workpiece.0);
            let digest = content_digest("bloomery.view.resolution", resolution);
            self.upsert_comment(issue_number, &key, digest, &render_resolution_body(resolution))?;
        }

        if let Some(pending) = &member.pending_decision {
            let key = format!("question:{}", member.workpiece.0);
            let digest = content_digest("bloomery.view.pending_decision", pending);
            self.upsert_comment(issue_number, &key, digest, &render_pending_decision_body(pending))?;
        }
        Ok(())
    }

    fn upsert_comment(
        &self,
        issue_number: u64,
        key: &str,
        digest: Digest,
        human_body: &str,
    ) -> Result<(), GithubError> {
        let marker = Marker { key: key.to_owned(), digest };
        let body = format!("{human_body}\n\n{}", render_marker(&marker));
        if let Some(existing) = self.client.find_comment(issue_number, key)? {
            if existing.marker.as_ref().map(|m| m.digest) == Some(digest) {
                return Ok(()); // matching digest — no-op.
            }
            self.client.update_comment(existing.id, &body)
        } else {
            self.client.create_comment(&NewComment { issue_number, body })?;
            Ok(())
        }
    }
}

impl<C: GithubApi + PullRequestApi> ProjectionBackend for GithubProjection<C> {
    type Error = GithubError;

    fn reconcile_view(&self, view: &ViewDocument) -> Result<(), Self::Error> {
        for bloom in &view.blooms {
            self.reconcile_bloom(bloom)?;
        }
        Ok(())
    }

    fn project_receipt(&self, receipt: &LandingReceipt) -> Result<(), Self::Error> {
        let receipt_key = format!("receipt:{}", short_hex(&receipt.bloom.0));
        let digest = content_digest("bloomery.receipt", receipt);
        let body = render_receipt_body(receipt);

        // Landing PR home — if the PR does not exist yet, skip explicitly.
        let branch = landing_branch(receipt.bloom);
        if let Some(pr) = self.client.find_pull_request_for_head(&branch)? {
            self.upsert_comment(pr.number, &receipt_key, digest, &body)?;
        } else {
            // No landing PR yet — explicit skip, no shadow issue.
        }

        // Source-issue homes are per-member, but the receipt carries no member
        // list. The receipt's PR comment is the aggregate home; per-member
        // source-issue receipt comments are not derivable from the receipt
        // alone and are therefore not projected here. This is explicit handling:
        // a missing home is a skip, not a shadow issue. If per-member receipt
        // visibility is needed, it rides the member's resolution view already
        // projected onto its source issue.

        Ok(())
    }
}

fn umbrella_key(bloom: BloomId) -> String {
    format!("bloom:{}", short_hex(&bloom.0))
}

fn member_key(bloom: BloomId, workpiece: &str) -> String {
    format!("wp:{workpiece}@bloom:{}", short_hex(&bloom.0))
}

fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
}

/// Map a [`WorkpieceId`] to its source GitHub issue number, if it names one.
///
/// The canonical form is `issue-4628` → `4628`. Any other string has no
/// source-issue home and is handled explicitly by skipping its projection
/// (no shadow issue is opened).
pub fn source_issue_number(id: &WorkpieceId) -> Option<u64> {
    let rest = id.0.strip_prefix("issue-")?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u64>().ok()
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

fn render_bloom_body(bloom: &BloomView) -> String {
    let mut body = format!("Aggregate view of bloom `{}`.\n\n", short_hex(&bloom.id.0));
    let _ = writeln!(body, "- Status: {:?}", bloom.status);
    if let Some(successor) = &bloom.superseded_by {
        let _ = writeln!(body, "- Superseded by: `{}`", short_hex(&successor.0));
    }
    let wedged = bloom.members.iter().filter(|member| member.wedge.is_some()).count();
    if wedged > 0 {
        let _ = writeln!(body, "- **Wedged members: {wedged}** — this bloom cannot resolve without a supersession.");
    }

    if let Some(landing) = &bloom.landing_blocked {
        let _ = writeln!(
            body,
            "- **Landing CI refused this bloom** ({} of {} attempts spent){}",
            landing.rolls,
            landing.budget,
            if landing.rolls >= landing.budget {
                " — parked for the owner; the machine will not re-propose it."
            } else {
                " — its members re-opened for repair against current mainline."
            },
        );
    }

    let _ = writeln!(body, "- Members: {}", bloom.members.len());
    for member in &bloom.members {
        let resolved = match (&member.resolution, &member.wedge) {
            (_, Some(wedge)) => format!("WEDGED at {:?}", wedge.stage),
            (Some(_), None) => "integrated".to_owned(),
            (None, None) => "pending".to_owned(),
        };
        let _ = writeln!(body, "  - `{}` ({resolved})", member.workpiece.0);
    }
    body
}

fn render_member_body(bloom: BloomId, member: &MemberView) -> String {
    let resolution = if member.resolution.is_some() {
        "integrated"
    } else {
        "not yet integrated"
    };
    let mut body = format!("Workpiece `{}` as admitted into bloom `{}`.\n\n", member.workpiece.0, short_hex(&bloom.0));
    let _ = writeln!(body, "- Scope revision: `{}`", short_hex(&member.scope_revision));
    let _ = writeln!(body, "- Approval: {:?}", member.approval.kind);
    let _ = writeln!(body, "- Resolution: {resolution}");
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
    body
}

fn render_evidence_body(evidence: &Evidence) -> String {
    format!(
        "**Evidence** — {:?} bound to `{}` (detail `{}`).",
        evidence.kind,
        short_hex(&evidence.subject),
        short_hex(&evidence.detail)
    )
}

fn render_resolution_body(resolution: &ResolutionClaim) -> String {
    format!(
        "**Resolution** — candidate `{}` resolves `{}` at scope `{}`.",
        short_hex(&resolution.candidate),
        resolution.workpiece.0,
        short_hex(&resolution.scope_revision)
    )
}

fn render_pending_decision_body(pending: &PendingDecisionView) -> String {
    let mut body = format!("**Decision needed** — parked on question `{}`.\n\n", short_hex(&pending.question));
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
    body
}

fn render_receipt_body(receipt: &LandingReceipt) -> String {
    format!("**Landed** — mainline moved `{}` → `{}`.", short_hex(&receipt.previous_base), short_hex(&receipt.new_head))
}
