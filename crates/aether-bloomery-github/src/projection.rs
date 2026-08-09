//! The projection backend — reconcile a [`ViewDocument`] onto existing GitHub
//! objects, never opening shadow issues.
//!
//! # Mapping
//!
//! - **Source issue.** A `WorkpieceId` of the form `issue-<digits>` (e.g.
//!   `issue-4628`) maps to the existing GitHub issue numbered `<digits>`.
//!   The projection posts member evidence (approval, resolution,
//!   pending-decision) as comments on that source issue, where a person
//!   already looks. The check is strict: the prefix is `issue-`, the suffix
//!   is non-empty ASCII digits, and the numeric value is the issue number.
//!   A workpiece id that does not match this form has no source-issue home;
//!   the projection handles it explicitly by skipping that member's
//!   per-member comments — it does not open a shadow `Workpiece issue-…`
//!   issue, it does not error the whole bloom, and it does not synthesize
//!   a placeholder.
//! - **Landing pull request.** `landing_branch(bloom)` is one pull request
//!   per bloom; that pull request IS the aggregate bloom view. The projection
//!   posts the bloom summary and the landing receipt as comments on the pull
//!   request (found via `find_pull_request_for_head`), which closes on merge.
//!   If the landing pull request does not yet exist when a view or receipt
//!   projects, the projection handles it explicitly by skipping the
//!   bloom-level / receipt-on-PR comment — no shadow umbrella issue is opened,
//!   and the next reconcile after the PR is created will create the comment.
//!   A receipt also fans out to each resolvable source issue of the bloom (via
//!   a bloom→members cache populated by prior `reconcile_view` calls); if the
//!   cache is empty the PR side is still attempted and the source-issue side
//!   is skipped, never synthesized.
//!
//! No projection path calls the repository-wide issue-list walk
//! (`find_issue`); existence is via direct issue number or via
//! `find_pull_request_for_head`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use aether_bloomery::{
    BloomId, BloomView, Digest, Evidence, LandingReceipt, MemberView, PendingDecisionView, ProjectionBackend,
    ResolutionClaim, ViewDocument,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::client::{GithubApi, GithubError, NewComment, PullRequestApi};
use crate::marker::{Marker, render_marker};
use crate::short_hex;

/// The outward projection mirror over a [`GithubApi`] + [`PullRequestApi`] client.
///
/// The client is held directly; bloom→members is cached for receipt fan-out.
pub struct GithubProjection<C> {
    client: C,
    known_members: Mutex<HashMap<BloomId, Vec<String>>>,
}

impl<C> GithubProjection<C> {
    /// Build a projection over `client`.
    pub fn new(client: C) -> Self {
        Self { client, known_members: Mutex::new(HashMap::new()) }
    }

    /// Borrow the underlying client (test introspection / receipt routing).
    pub const fn client(&self) -> &C {
        &self.client
    }

    fn remember_members(&self, bloom: BloomId, members: &[MemberView]) {
        let mut guard = self.known_members.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(bloom, members.iter().map(|member| member.workpiece.0.clone()).collect());
    }

    fn reconcile_member_evidence(&self, issue_number: u64, member: &MemberView) -> Result<(), GithubError>
    where
        C: GithubApi,
    {
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

    fn upsert_comment(&self, issue_number: u64, key: &str, digest: Digest, human_body: &str) -> Result<(), GithubError>
    where
        C: GithubApi,
    {
        let marker = Marker { key: key.to_owned(), digest };
        let body = format!("{human_body}\n\n{}", render_marker(&marker));
        if let Some(existing) = self.client.find_comment(issue_number, key)? {
            if existing.marker.as_ref().map(|marker| marker.digest) == Some(digest) {
                return Ok(());
            }
            self.client.update_comment(existing.id, &body)
        } else {
            self.client.create_comment(&NewComment { issue_number, body })?;
            Ok(())
        }
    }
}

impl<C> ProjectionBackend for GithubProjection<C>
where
    C: GithubApi + PullRequestApi,
{
    type Error = GithubError;

    fn reconcile_view(&self, view: &ViewDocument) -> Result<(), Self::Error> {
        for bloom in &view.blooms {
            self.remember_members(bloom.id, &bloom.members);

            let branch = landing_branch(bloom.id);
            if let Some(pr) = self.client.find_pull_request_for_head(&branch)? {
                let key = format!("bloom:{}", short_hex(&bloom.id.0));
                let digest = content_digest("bloomery.view.bloom", bloom);
                let body = render_bloom_body(bloom);
                self.upsert_comment(pr.number, &key, digest, &body)?;
            }

            for member in &bloom.members {
                let Some(issue_number) = workpiece_issue_number(&member.workpiece.0) else {
                    continue;
                };
                self.reconcile_member_evidence(issue_number, member)?;
            }
        }
        Ok(())
    }

    fn project_receipt(&self, receipt: &LandingReceipt) -> Result<(), Self::Error> {
        let branch = landing_branch(receipt.bloom);
        let receipt_key = format!("receipt:{}", short_hex(&receipt.bloom.0));
        let digest = content_digest("bloomery.receipt", receipt);
        let body = render_receipt_body(receipt);

        if let Some(pr) = self.client.find_pull_request_for_head(&branch)? {
            self.upsert_comment(pr.number, &receipt_key, digest, &body)?;
        }

        let members: Vec<String> = {
            let guard = self.known_members.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.get(&receipt.bloom).cloned().unwrap_or_default()
        };
        for workpiece in members {
            let Some(issue_number) = workpiece_issue_number(&workpiece) else {
                continue;
            };
            self.upsert_comment(issue_number, &receipt_key, digest, &body)?;
        }
        Ok(())
    }
}

/// Map a `WorkpieceId` string to its source GitHub issue number.
///
/// Strict `issue-<digits>` form: prefix `issue-`, non-empty ASCII digits
/// suffix, parsed as `u64`. Returns `None` for any other form — the
/// explicit "no source-issue home" case that the projection skips rather
/// than opening a shadow issue.
fn workpiece_issue_number(workpiece: &str) -> Option<u64> {
    let suffix = workpiece.strip_prefix("issue-")?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u64>().ok().filter(|number| *number != 0)
}

fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
}

/// sha256 over a domain tag (null-separated) and the value's canonical JSON —
/// the change-detection digest a marker carries.
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
