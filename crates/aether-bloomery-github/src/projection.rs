//! The projection backend (#3459 step 5, revised): reconcile a [`ViewDocument`]
//! into GitHub objects that already exist.
//!
//! Every projected object carries a stable [`Marker`] — its internal Bloomery
//! key plus a content digest of the desired render. Reconcile is *find by key
//! → compare digest → create / update / no-op*, so:
//!
//! - **Idempotent** — reconciling the same document twice is all no-ops, since
//!   the second pass finds each object with a matching digest.
//! - **Rebuildable** — a deleted comment leaves no marker to find, so the next
//!   reconcile recreates it (the "delete → reappear" property the demo proves).
//!
//! # What maps to what
//!
//! - Each bloom's aggregate view → a **comment on the bloom's landing pull
//!   request**, keyed by the bloom id, its body summarizing status and
//!   membership. The pull request is the per-bloom aggregate a multi-member
//!   bloom needs, and it closes on merge. If the pull request does not yet
//!   exist when the first view projects, the aggregate is skipped explicitly
//!   rather than falling back to a shadow umbrella issue.
//! - Each member → **comments on the member's source issue**, keyed by
//!   workpiece *and* bloom so a successor bloom's re-admission of the same
//!   workpiece is its own faithful projection rather than clobbering the
//!   predecessor's. The source issue is the GitHub issue the workpiece already
//!   names. Evidence (approval, resolution, parked question) is projected as
//!   comments, not check-runs: a check-run must attach to a commit, which only
//!   the git source port (a separate slice) produces.
//! - A [`LandingReceipt`] → a comment on the bloom's landing pull request and,
//!   when cached members are available, on each member's source issue. If the
//!   pull request is not yet present, the receipt's PR copy is skipped; if the
//!   bloom was never reconciled, the source-issue copies are skipped. No shadow
//!   umbrella issue is ever created.
//!
//! # `WorkpieceId` → issue number
//!
//! A workpiece whose id is `issue-4628` names issue 4628. The mapping is:
//!
//! - If the id starts with `issue-` and the remainder is a non-empty decimal
//!   string of ASCII digits parsing to a non-zero `u64`, that number is the
//!   source issue.
//! - Any other id does not resolve to an issue. The projection skips that
//!   member explicitly — no comment is written, no shadow issue is opened, and
//!   the rest of the reconcile continues. A bloom sealed against a workpiece
//!   with no corresponding issue has no source-issue home for its comments.
//!
//! The projector reads only its own markers; free-form platform content is
//! never interpreted as intent. No projection path calls the repository-wide
//! issue-list walk (`find_issue`).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use aether_bloomery::{
    BloomId, BloomView, Digest, Evidence, LandingReceipt, MemberView, PendingDecisionView, ProjectionBackend,
    ResolutionClaim, ViewDocument, WorkpieceId,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::client::{GithubApi, GithubError, NewComment, PullRequestApi};
use crate::marker::{Marker, render_marker};
use crate::short_hex;

/// Parse a workpiece id of the form `issue-<number>` into its source issue
/// number. Returns `None` when the id does not name an issue — the explicit
/// handling the projection relies on to avoid opening a shadow issue.
#[must_use]
pub fn parse_source_issue_number(workpiece: &WorkpieceId) -> Option<u64> {
    let suffix = workpiece.0.strip_prefix("issue-")?;
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u64>().ok().filter(|&n| n != 0)
}

fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
}

/// The outward projection mirror over a [`GithubApi`] + [`PullRequestApi`] client.
pub struct GithubProjection<C: GithubApi + PullRequestApi> {
    client: C,
    seen: Mutex<HashMap<BloomId, Vec<WorkpieceId>>>,
}

impl<C: GithubApi + PullRequestApi> GithubProjection<C> {
    /// Build a projection over `client`.
    pub fn new(client: C) -> Self {
        Self { client, seen: Mutex::new(HashMap::new()) }
    }

    /// Borrow the underlying client (test introspection / receipt routing).
    pub fn client(&self) -> &C {
        &self.client
    }

    fn reconcile_bloom(&self, bloom: &BloomView) -> Result<(), GithubError> {
        // Remember bloom membership for later receipt → source-issue fan-out.
        {
            let mut seen = self.seen.lock().expect("projection seen lock");
            seen.insert(bloom.id, bloom.members.iter().map(|m| m.workpiece.clone()).collect());
        }

        // Aggregate bloom view → comment on landing PR if it exists; otherwise
        // skip explicitly (no shadow issue).
        if let Some(pr) = self.client.find_pull_request_for_head(&landing_branch(bloom.id))? {
            let key = umbrella_key(bloom.id);
            let digest = content_digest("bloomery.view.bloom", bloom);
            let body = render_bloom_body(bloom);
            self.upsert_comment(pr.number, &key, digest, &body)?;
        }

        for member in &bloom.members {
            let Some(issue_number) = parse_source_issue_number(&member.workpiece) else {
                continue;
            };
            // Verify the source issue exists via direct lookup — never the
            // repository-wide list walk. Absent → skip explicitly.
            if self.client.get_issue(issue_number)?.is_none() {
                continue;
            }
            self.reconcile_member_on_issue(issue_number, bloom.id, member)?;
        }
        Ok(())
    }

    fn reconcile_member_on_issue(
        &self,
        issue_number: u64,
        bloom: BloomId,
        member: &MemberView,
    ) -> Result<(), GithubError> {
        let member_key = member_key(bloom, &member.workpiece.0);
        let member_digest = content_digest("bloomery.view.member", member);
        let member_body = render_member_body(bloom, member);
        self.upsert_comment(issue_number, &member_key, member_digest, &member_body)?;
        self.reconcile_member_evidence(issue_number, bloom, member)
    }

    fn reconcile_member_evidence(
        &self,
        issue_number: u64,
        bloom: BloomId,
        member: &MemberView,
    ) -> Result<(), GithubError> {
        let bloom_hex = short_hex(&bloom.0);
        let approval_key = format!("approval:{}@bloom:{bloom_hex}", short_hex(&member.approval.subject));
        let approval_digest = content_digest("bloomery.view.evidence", &member.approval);
        self.upsert_comment(issue_number, &approval_key, approval_digest, &render_evidence_body(&member.approval))?;

        if let Some(resolution) = &member.resolution {
            let key = format!("resolution:{}@bloom:{bloom_hex}", member.workpiece.0);
            let digest = content_digest("bloomery.view.resolution", resolution);
            self.upsert_comment(issue_number, &key, digest, &render_resolution_body(resolution))?;
        }

        if let Some(pending) = &member.pending_decision {
            let key = format!("question:{}@bloom:{bloom_hex}", member.workpiece.0);
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
                return Ok(());
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

        // Landing PR copy — skip explicitly if the PR does not yet exist.
        if let Some(pr) = self.client.find_pull_request_for_head(&landing_branch(receipt.bloom))? {
            self.upsert_comment(pr.number, &receipt_key, digest, &body)?;
        }

        // Source-issue copies — fan out to members cached from a prior reconcile.
        // If the bloom was never reconciled (no cached members), skip explicitly;
        // a fallback shadow issue is not an acceptable answer.
        let members = {
            let seen = self.seen.lock().expect("projection seen lock");
            seen.get(&receipt.bloom).cloned()
        };
        if let Some(workpieces) = members {
            for workpiece in workpieces {
                let Some(issue_number) = parse_source_issue_number(&workpiece) else {
                    continue;
                };
                if self.client.get_issue(issue_number)?.is_none() {
                    continue;
                }
                self.upsert_comment(issue_number, &receipt_key, digest, &body)?;
            }
        }
        Ok(())
    }
}

fn umbrella_key(bloom: BloomId) -> String {
    format!("bloom:{}", short_hex(&bloom.0))
}

fn member_key(bloom: BloomId, workpiece: &str) -> String {
    format!("wp:{workpiece}@bloom:{}", short_hex(&bloom.0))
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
