//! The projection backend (#3459 step 5): reconcile a [`ViewDocument`] into
//! GitHub objects.
//!
//! Every projected object carries a stable [`Marker`] — its internal Bloomery
//! key plus a content digest of the desired render. Reconcile is *find by key
//! → compare digest → create / update / no-op*, so:
//!
//! - **Idempotent** — reconciling the same document twice is all no-ops, since
//!   the second pass finds each object with a matching digest.
//! - **Rebuildable** — a deleted object leaves no marker to find, so the next
//!   reconcile recreates it (the "delete → reappear" property the demo proves).
//!
//! # What maps to what
//!
//! - The **source issue** is the existing GitHub issue the workpiece already
//!   lives as. A [`WorkpieceId`] of the form `issue-<N>` maps to issue number
//!   `N` (decimal, `N > 0`). Any other workpiece id has no source-issue home;
//!   its per-member comments are skipped explicitly — no shadow issue is opened.
//!   The mapping is `strip_prefix("issue-")` then `parse::<u64>()`; a parse
//!   failure is the same as no prefix and is skipped. This is stated in the
//!   pull request body as the mapping decision.
//! - The **landing pull request** is the one pull request per bloom on
//!   `landing_branch(bloom)` (`bloom/<short hex>/landing`). It is the aggregate
//!   view a multi-member bloom needs and it closes on merge. The projection
//!   posts the bloom aggregate as a comment on that PR; if the PR does not yet
//!   exist when the first view document projects, the bloom aggregate is skipped
//!   explicitly — no umbrella issue is opened as a fallback.
//! - Each member's approval, resolution, and parked question → **comments** on
//!   its source issue (or skipped if the source issue does not exist or the
//!   workpiece does not name one). Evidence is projected as comments, not
//!   check-runs.
//! - A [`LandingReceipt`] → a comment on the bloom's landing pull request and,
//!   for every member of that bloom that resolves to an existing source issue,
//!   a copy on that source issue. If the PR or a source issue does not exist,
//!   that copy is skipped. No new issue is ever opened.
//!
//! The projector reads only its own markers; free-form platform content is
//! never interpreted as intent.
//!
//! No projection path calls the repository-wide issue-list walk
//! (`find_issue`): source-issue existence is checked via direct
//! `get_issue(number)`, and the landing PR via `find_pull_request_for_head`.
//! Comments are still upserted per-issue via `find_comment`, which is scoped to
//! a single issue and not a repository scan.

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

/// The outward projection mirror over a combined GitHub client.
pub struct GithubProjection<C> {
    client: C,
    /// Cache of bloom → member workpieces that resolved to a source issue at
    /// the last `reconcile_view`. Lets `project_receipt` know which source
    /// issues to post the receipt copy to without requiring the receipt to
    /// carry membership. `Mutex` because the projection is `&self` and may be
    /// used from a relay thread.
    receipt_cache: Mutex<HashMap<BloomId, Vec<WorkpieceId>>>,
}

impl<C> GithubProjection<C>
where
    C: GithubApi + PullRequestApi,
{
    /// Build a projection over `client`.
    pub fn new(client: C) -> Self {
        Self { client, receipt_cache: Mutex::new(HashMap::new()) }
    }

    /// Borrow the underlying client (test introspection / receipt routing).
    pub const fn client(&self) -> &C {
        &self.client
    }

    fn reconcile_bloom(&self, bloom: &BloomView) -> Result<(), GithubError> {
        // Aggregate bloom view → comment on landing PR, if it exists.
        let bloom_key = format!("bloom:{}", short_hex(&bloom.id.0));
        let bloom_digest = content_digest("bloomery.view.bloom", bloom);
        let bloom_body = render_bloom_body(bloom);
        if let Some(pr) = self.client.find_pull_request_for_head(&landing_branch(bloom.id))? {
            self.upsert_comment(pr.number, &bloom_key, bloom_digest, &bloom_body)?;
        }
        // Cache resolvable members for later receipt fan-out, regardless of PR
        // existence — the receipt needs them even if the aggregate was skipped.
        let mut resolvable: Vec<WorkpieceId> = Vec::new();
        for member in &bloom.members {
            let Some(issue_number) = workpiece_issue_number(&member.workpiece.0) else {
                continue;
            };
            if self.client.get_issue(issue_number)?.is_none() {
                continue;
            }
            resolvable.push(member.workpiece.clone());
            self.reconcile_member_evidence(issue_number, member)?;
        }
        if !resolvable.is_empty() || !bloom.members.is_empty() {
            // Record even empty resolvable set so a bloom whose every member is
            // unresolvable still has an entry (receipt will fan out to zero
            // source issues, PR only).
            if let Ok(mut cache) = self.receipt_cache.lock() {
                cache.insert(bloom.id, resolvable);
            }
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
            self.reconcile_bloom(bloom)?;
        }
        Ok(())
    }

    fn project_receipt(&self, receipt: &LandingReceipt) -> Result<(), Self::Error> {
        let receipt_key = format!("receipt:{}", short_hex(&receipt.bloom.0));
        let digest = content_digest("bloomery.receipt", receipt);
        let body = render_receipt_body(receipt);

        // Landing PR copy — explicitly skipped if the PR does not yet exist.
        if let Some(pr) = self.client.find_pull_request_for_head(&landing_branch(receipt.bloom))? {
            self.upsert_comment(pr.number, &receipt_key, digest, &body)?;
        }

        // Source-issue copies: fan out to every member of this bloom that
        // resolved to an existing source issue at the last reconcile. If the
        // bloom has never been reconciled, there is no work to fan out to —
        // the PR copy is the only home, and no shadow issue is opened.
        let targets: Vec<WorkpieceId> =
            self.receipt_cache.lock().ok().and_then(|c| c.get(&receipt.bloom).cloned()).unwrap_or_default();
        for workpiece in targets {
            if let Some(issue_number) = workpiece_issue_number(&workpiece.0) {
                if self.client.get_issue(issue_number)?.is_none() {
                    continue;
                }
                // Use a per-workpiece receipt key so each source issue's
                // receipt comment is independently idempotent.
                let source_key = format!("receipt:{}:{}", short_hex(&receipt.bloom.0), workpiece.0);
                self.upsert_comment(issue_number, &source_key, digest, &body)?;
            }
        }
        Ok(())
    }
}

/// Map a `WorkpieceId` string to its source-issue number, if it names one.
///
/// A workpiece id of the form `issue-<N>` (decimal, `N >= 1`) maps to issue
/// number `N`. Any other form — including `issue-` with a non-numeric suffix,
/// an empty suffix, or no `issue-` prefix — returns `None` and the projection
/// skips it explicitly rather than opening a shadow issue.
fn workpiece_issue_number(workpiece: &str) -> Option<u64> {
    let suffix = workpiece.strip_prefix("issue-")?;
    if suffix.is_empty() {
        return None;
    }
    let number: u64 = suffix.parse().ok()?;
    if number == 0 {
        None
    } else {
        Some(number)
    }
}

fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
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
