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
//! - Each bloom → an **umbrella issue** (the aggregate bloom view), keyed by
//!   the bloom id, its body summarizing status and membership.
//! - Each member → a **workpiece issue**, keyed by workpiece *and* bloom so a
//!   successor bloom's re-admission of the same workpiece is its own faithful
//!   projection rather than clobbering the predecessor's (a shadow mirror
//!   tracks every bloom's membership, superseded ones included).
//! - Each member's approval and — once integrated — resolution → **comments**
//!   on that workpiece issue. Evidence is projected as comments, not
//!   check-runs: a check-run must attach to a commit, which only the git
//!   source port (a separate slice) produces.
//! - A [`LandingReceipt`] → a comment on the bloom's umbrella issue.
//!
//! The projector reads only its own markers; free-form platform content is
//! never interpreted as intent.

use std::fmt::Write as _;

use aether_bloomery::{
    BloomId, BloomView, Digest, Evidence, LandingReceipt, MemberView, ProjectionBackend, ResolutionClaim, ViewDocument,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::client::{GithubApi, GithubError, NewComment, NewIssue};
use crate::marker::{Marker, render_marker};

/// The outward projection mirror over a [`GithubApi`] client.
pub struct GithubProjection<C: GithubApi> {
    client: C,
}

impl<C: GithubApi> GithubProjection<C> {
    /// Build a projection over `client`.
    pub const fn new(client: C) -> Self {
        Self { client }
    }

    /// Borrow the underlying client (test introspection / receipt routing).
    pub const fn client(&self) -> &C {
        &self.client
    }

    fn reconcile_bloom(&self, bloom: &BloomView) -> Result<(), GithubError> {
        let key = umbrella_key(bloom.id);
        let digest = content_digest("bloomery.view.bloom", bloom);
        let title = format!("Bloom {}", short_hex(&bloom.id.0));
        let body = render_bloom_body(bloom);
        self.upsert_issue(&key, digest, &title, &body)?;

        for member in &bloom.members {
            let issue_number = self.reconcile_member(bloom.id, member)?;
            self.reconcile_member_evidence(issue_number, member)?;
        }
        Ok(())
    }

    fn reconcile_member(&self, bloom: BloomId, member: &MemberView) -> Result<u64, GithubError> {
        let key = member_key(bloom, &member.workpiece.0);
        let digest = content_digest("bloomery.view.member", member);
        let title = format!("Workpiece {}", member.workpiece.0);
        let body = render_member_body(bloom, member);
        self.upsert_issue(&key, digest, &title, &body)
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
        Ok(())
    }

    fn upsert_issue(&self, key: &str, digest: Digest, title: &str, human_body: &str) -> Result<u64, GithubError> {
        let marker = Marker { key: key.to_owned(), digest };
        let body = format!("{human_body}\n\n{}", render_marker(&marker));
        match self.client.find_issue(key)? {
            Some(existing) => {
                if existing.marker.as_ref().map(|m| m.digest) == Some(digest) {
                    return Ok(existing.number); // matching digest — no-op.
                }
                self.client.update_issue(existing.number, title, &body)?;
                Ok(existing.number)
            }
            None => Ok(self.client.create_issue(&NewIssue { title: title.to_owned(), body })?.number),
        }
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

impl<C: GithubApi> ProjectionBackend for GithubProjection<C> {
    type Error = GithubError;

    fn reconcile_view(&self, view: &ViewDocument) -> Result<(), Self::Error> {
        for bloom in &view.blooms {
            self.reconcile_bloom(bloom)?;
        }
        Ok(())
    }

    fn project_receipt(&self, receipt: &LandingReceipt) -> Result<(), Self::Error> {
        let key = umbrella_key(receipt.bloom);
        // The umbrella issue is normally projected by a prior reconcile; if a
        // receipt races ahead of it, open a minimal umbrella so the landing
        // note has a home.
        let issue_number = if let Some(existing) = self.client.find_issue(&key)? {
            existing.number
        } else {
            let marker = Marker { key, digest: content_digest("bloomery.view.bloom.stub", receipt) };
            let title = format!("Bloom {}", short_hex(&receipt.bloom.0));
            let body = format!("Bloom umbrella (opened by landing receipt).\n\n{}", render_marker(&marker));
            self.client.create_issue(&NewIssue { title, body })?.number
        };
        let receipt_key = format!("receipt:{}", short_hex(&receipt.bloom.0));
        let digest = content_digest("bloomery.receipt", receipt);
        self.upsert_comment(issue_number, &receipt_key, digest, &render_receipt_body(receipt))
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

fn short_hex(digest: &Digest) -> String {
    let bytes = digest.as_bytes();
    let mut out = String::with_capacity(12);
    for byte in &bytes[..6] {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn render_bloom_body(bloom: &BloomView) -> String {
    let mut body = format!("Aggregate view of bloom `{}`.\n\n", short_hex(&bloom.id.0));
    let _ = writeln!(body, "- Status: {:?}", bloom.status);
    if let Some(successor) = &bloom.superseded_by {
        let _ = writeln!(body, "- Superseded by: `{}`", short_hex(&successor.0));
    }
    let _ = writeln!(body, "- Members: {}", bloom.members.len());
    for member in &bloom.members {
        let resolved = if member.resolution.is_some() {
            "integrated"
        } else {
            "pending"
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

fn render_receipt_body(receipt: &LandingReceipt) -> String {
    format!("**Landed** — mainline moved `{}` → `{}`.", short_hex(&receipt.previous_base), short_hex(&receipt.new_head))
}
