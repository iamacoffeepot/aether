//! The projection backend (#3459 step 5, revised to project onto existing
//! objects rather than shadow issues).
//!
//! Every projected comment carries a stable [`Marker`] — its internal
//! Bloomery key plus a content digest of the desired render. Reconcile is
//! *find by key → compare digest → create / update / no-op*, so:
//!
//! - **Idempotent** — reconciling the same document twice is all no-ops, since
//!   the second pass finds each comment with a matching digest.
//! - **Rebuildable** — a deleted comment leaves no marker to find, so the next
//!   reconcile recreates it.
//!
//! # What maps to what
//!
//! - Each bloom's aggregate view → a **comment on its landing pull request**,
//!   keyed by the bloom id, its body summarizing status and membership. The
//!   pull request is addressed by `landing_branch(bloom)` — one pull request
//!   per bloom — so the aggregate view lives where it closes on merge and no
//!   shadow issue is needed. If the pull request does not yet exist (the first
//!   view projects before the landing branch is pushed), the bloom aggregate is
//!   skipped until the pull request appears; the next reconcile creates it.
//! - Each member's workpiece view → a **comment on its source issue**, keyed by
//!   workpiece and bloom. The source issue is the GitHub issue whose number
//!   the workpiece id encodes (`issue-4628` → issue 4628), where a person
//!   already looks. Resolution, approval, and parked-question evidence are
//!   further comments on that same source issue.
//! - A [`LandingReceipt`] → a **comment on the bloom's landing pull request**.
//!   The receipt is an `UpsertComment` on that pull request by its stable
//!   `receipt:<bloom>` key. Member resolution is already visible on each
//!   member's source issue via the member view's resolution comment; the
//!   receipt on the pull request is the aggregate landing signal.
//!
//! # Mapping `WorkpieceId` → issue number
//!
//! `issue_number_for_workpiece("issue-4628") == Some(4628)`. The mapping is
//! the literal suffix after the `issue-` prefix parsed as a strictly positive
//! decimal `u64` consisting solely of ASCII digits. Any other shape
//! (`"coolant-loop"`, `"issue-abc"`, `"issue-0"`, `"issue-0128-extra"`) maps
//! to `None` and is handled explicitly: the projection skips that workpiece's
//! comments rather than opening a shadow issue. A parseable number whose issue
//! does not exist (no `GET /issues/{n}`) is likewise skipped — there is no
//! home to comment on.
//!
//! The projector reads only its own markers plus the direct `get_issue` /
//! `find_pull_request_for_head` lookups; it never walks the repository-wide
//! issue list (`find_issue`). Free-form platform content is never interpreted
//! as intent.

use std::fmt::Write as _;

use aether_bloomery::{
    BloomId, BloomView, Digest, Evidence, LandingReceipt, MemberView, PendingDecisionView, ProjectionBackend,
    ResolutionClaim, ViewDocument,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::client::{GithubApi, GithubError, NewComment, PullRequestApi};
use crate::marker::{Marker, render_marker};
use crate::short_hex;

/// Map a `WorkpieceId` string to the source-issue number it names, if any.
///
/// The canonical form is `issue-<digits>` where `<digits>` is a non-empty,
/// all-ASCII-digit, strictly positive decimal `u64`. `issue-4628` → `Some(4628)`;
/// any other shape → `None`.
///
/// This is the *only* mapping from workpiece to real GitHub object; a
/// workpiece whose id does not name an issue has no source-issue home for its
/// comments, and the projection skips it rather than opening a shadow issue.
#[must_use]
pub fn issue_number_for_workpiece(workpiece: &str) -> Option<u64> {
    let rest = workpiece.strip_prefix("issue-")?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Leading zeros are accepted (they still parse), but `0` itself is not a
    // valid issue number and is rejected.
    let number: u64 = rest.parse().ok()?;
    (number != 0).then_some(number)
}

/// The landing branch for a bloom — `bloom/<short>/landing` — whose pull
/// request is the bloom's aggregate view. Mirrors `GitSource::landing_branch`
/// so the two ends cannot drift.
fn landing_branch(bloom: BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
}

/// The outward projection mirror over a [`GithubApi`] + [`PullRequestApi`] client.
pub struct GithubProjection<C> {
    client: C,
}

impl<C> GithubProjection<C>
where
    C: GithubApi + PullRequestApi,
{
    /// Build a projection over `client`.
    pub const fn new(client: C) -> Self {
        Self { client }
    }

    /// Borrow the underlying client (test introspection / receipt routing).
    pub const fn client(&self) -> &C {
        &self.client
    }

    fn reconcile_bloom(&self, bloom: &BloomView) -> Result<(), GithubError> {
        // Bloom aggregate → comment on its landing pull request, if it exists
        // yet. A bloom sealed before its landing branch is pushed has no pull
        // request; skipping is the explicit handling — the next reconcile after
        // the branch appears creates the comment, and no shadow issue is opened.
        let branch = landing_branch(bloom.id);
        if let Some(pull) = self.client.find_pull_request_for_head(&branch)? {
            let key = umbrella_key(bloom.id);
            let digest = content_digest("bloomery.view.bloom", bloom);
            let body = render_bloom_body(bloom);
            self.upsert_comment(pull.number, &key, digest, &body)?;
        }

        for member in &bloom.members {
            let Some(issue_number) = issue_number_for_workpiece(&member.workpiece.0) else {
                continue;
            };
            // No home if the source issue does not exist — skip explicitly.
            if self.client.get_issue(issue_number)?.is_none() {
                continue;
            }
            // Member summary comment on its source issue (replaces the former
            // per-bloom workpiece issue).
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

        // A parked question projects as a comment on the source issue — visible
        // where a person already looks (ADR-0151). Keyed by workpiece so it
        // upserts in place, and content-digested over the pending decision so
        // re-reconciling the same hold is a no-op.
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
        // A landing receipt lands as a comment on the bloom's landing pull
        // request. If the pull request does not yet exist — a receipt racing
        // ahead of the landing branch — the projection skips (no shadow issue).
        // Member resolution is already visible on each source issue via the
        // member view's resolution comment, so the pull-request comment is the
        // aggregate landing signal.
        let branch = landing_branch(receipt.bloom);
        let Some(pull) = self.client.find_pull_request_for_head(&branch)? else {
            return Ok(());
        };
        let receipt_key = format!("receipt:{}", short_hex(&receipt.bloom.0));
        let digest = content_digest("bloomery.receipt", receipt);
        self.upsert_comment(pull.number, &receipt_key, digest, &render_receipt_body(receipt))
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
    // A wedged member is terminal for the whole bloom, so it is called out at
    // bloom scope too: "one member pending" and "one member that will never
    // move again" are the same line otherwise.
    let wedged = bloom.members.iter().filter(|member| member.wedge.is_some()).count();
    if wedged > 0 {
        let _ = writeln!(body, "- **Wedged members: {wedged}** — this bloom cannot resolve without a supersession.");
    }

    // A refused landing is the one blocked state that is invisible from member
    // rows alone: every member reads integrated while the bloom sits on a gate
    // it cannot pass.
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::port::{BloomView, MemberView};
    use aether_bloomery::{
        BloomDraft, BloomId, BloomStatus, ConfigRegistry, Digest, Evidence, EvidenceKind, Membership, ViewDocument,
        WorkpieceId,
    };

    use crate::client::{Comment, GithubApi, GithubError, Issue, NewComment, NewIssue, PullRequestApi};
    use crate::testing::FakeGithub;

    use super::{GithubProjection, issue_number_for_workpiece};

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn membership(name: &str, revision: u8) -> Membership {
        let mut member = Membership {
            workpiece: WorkpieceId(name.to_owned()),
            scope_revision: digest(revision),
            configs: ConfigRegistry::default(),
            approval: Evidence { subject: digest(0), kind: EvidenceKind::Approval, detail: digest(200) },
        };
        member.approval.subject = member.subject();
        member
    }

    fn synthetic_view_with_workpieces(names: &[&str]) -> ViewDocument {
        let base = digest(0);
        let proposals: Vec<Membership> = names.iter().map(|name| membership(name, 10)).collect();
        let spec = BloomDraft { proposals, base, ..BloomDraft::default() }.seal();
        let bloom_id = spec.id();
        let members: Vec<MemberView> = spec
            .members()
            .iter()
            .map(|member| MemberView {
                workpiece: member.workpiece.clone(),
                scope_revision: member.scope_revision,
                approval: member.approval.clone(),
                resolution: None,
                pending_decision: None,
                wedge: None,
            })
            .collect();
        ViewDocument {
            mainline: base,
            blooms: vec![BloomView {
                id: bloom_id,
                status: BloomStatus::Sealed,
                superseded_by: None,
                members,
                landing_blocked: None,
            }],
        }
    }

    /// Seed a source issue for `workpiece` at its mapped number, ensuring the
    /// fake contains that issue before projection. `issue-1` → issue 1, etc.
    fn seed_source_issue(fake: &FakeGithub, workpiece: &str) -> u64 {
        let number = issue_number_for_workpiece(workpiece).expect("workpiece must map");
        // FakeGithub's next_issue is sequential, so create dummy issues until we
        // reach the desired number.
        while fake.issue_count() < number as usize {
            // Create placeholder issues with distinct titles so the numbering
            // advances predictably.
            let placeholder = format!("placeholder-{}", fake.issue_count() + 1);
            fake.create_issue(&NewIssue { title: placeholder.clone(), body: placeholder }).unwrap();
        }
        // Now issue `number` exists. If it was just created as placeholder,
        // replace its title/body to look like a real source issue.
        number
    }

    fn seed_landing_pr(fake: &FakeGithub, bloom: BloomId) -> u64 {
        let branch = format!("bloom/{}/landing", crate::short_hex(&bloom.0));
        // The pull-request head must resolve to a commit; seed a ref.
        let commit = fake.create_commit("landing", "tree", &[]).unwrap();
        fake.seed_ref(&format!("heads/{branch}"), &commit.sha);
        fake.create_pull_request(&crate::client::NewPullRequest {
            title: format!("landing {}", crate::short_hex(&bloom.0)),
            body: String::new(),
            head: branch,
            base: "main".to_owned(),
        })
        .unwrap()
        .number
    }

    #[test]
    fn issue_number_for_workpiece_parses_only_issue_dash_digits() {
        assert_eq!(issue_number_for_workpiece("issue-4628"), Some(4628));
        assert_eq!(issue_number_for_workpiece("issue-1"), Some(1));
        assert_eq!(issue_number_for_workpiece("issue-0"), None, "zero is not a valid issue");
        assert_eq!(issue_number_for_workpiece("coolant-loop"), None);
        assert_eq!(issue_number_for_workpiece("issue-abc"), None);
        assert_eq!(issue_number_for_workpiece("issue-"), None);
        assert_eq!(issue_number_for_workpiece("issue-12a"), None);
        assert_eq!(issue_number_for_workpiece("ISSUE-123"), None, "case-sensitive");
        assert_eq!(issue_number_for_workpiece("issue-001"), Some(1), "leading zeros parse");
    }

    #[test]
    fn landing_receipt_lands_as_comment_without_opening_issue() {
        // A landing receipt must land as a comment on the bloom's landing PR
        // and leave the issue count unchanged. This would fail under the old
        // shadow-issue behaviour which opened an umbrella issue for the receipt.
        let fake = FakeGithub::new();
        let view = synthetic_view_with_workpieces(&["issue-1"]);
        let bloom_id = view.blooms[0].id;

        // Pre-seed the source issue and the landing PR so the projection has
        // homes that already exist and already close themselves.
        let source_number = seed_source_issue(&fake, "issue-1");
        let pr_number = seed_landing_pr(&fake, bloom_id);
        let issues_before = fake.issue_count();

        let projection = GithubProjection::new(fake.clone());

        // Reconcile member evidence onto the source issue.
        projection.reconcile_view(&view).unwrap();
        assert_eq!(fake.issue_count(), issues_before, "reconcile must not open a shadow issue");
        // A member summary comment should appear on the source issue.
        let comments_on_source =
            fake.find_comment(source_number, &format!("wp:issue-1@bloom:{}", crate::short_hex(&bloom_id.0))).unwrap();
        assert!(comments_on_source.is_some(), "member view comment on source issue");

        // Project a landing receipt — it must appear on the pull request.
        let receipt =
            aether_bloomery::LandingReceipt { bloom: bloom_id, previous_base: digest(0), new_head: digest(99) };
        projection.project_receipt(&receipt).unwrap();
        assert_eq!(fake.issue_count(), issues_before, "receipt must not open an issue");
        let receipt_comment =
            fake.find_comment(pr_number, &format!("receipt:{}", crate::short_hex(&bloom_id.0))).unwrap();
        assert!(receipt_comment.is_some(), "receipt comment on landing PR");
    }

    #[test]
    fn no_projection_path_calls_find_issue() {
        // The old projection walked the whole repository issue list via
        // `find_issue` (47 sequential requests). The new projection must not
        // call it at all — it addresses issues by number and pull requests by
        // head. This test wraps the fake so `find_issue` panics; if any
        // projection path still calls it, the test fails.
        struct NoListGithub {
            inner: FakeGithub,
        }

        impl GithubApi for NoListGithub {
            fn find_issue(&self, _key: &str) -> Result<Option<Issue>, GithubError> {
                panic!("projection called find_issue — repository-wide scan must not be reachable");
            }
            fn create_issue(&self, _new: &NewIssue) -> Result<Issue, GithubError> {
                panic!("projection must not open a shadow issue");
            }
            fn update_issue(&self, _number: u64, _title: &str, _body: &str) -> Result<(), GithubError> {
                panic!("projection must not update an issue");
            }
            fn get_issue(&self, number: u64) -> Result<Option<Issue>, GithubError> {
                self.inner.get_issue(number)
            }
            fn find_comment(&self, issue_number: u64, key: &str) -> Result<Option<Comment>, GithubError> {
                self.inner.find_comment(issue_number, key)
            }
            fn create_comment(&self, new: &NewComment) -> Result<Comment, GithubError> {
                self.inner.create_comment(new)
            }
            fn update_comment(&self, comment_id: u64, body: &str) -> Result<(), GithubError> {
                self.inner.update_comment(comment_id, body)
            }
        }

        impl PullRequestApi for NoListGithub {
            fn create_pull_request(
                &self,
                _new: &crate::client::NewPullRequest,
            ) -> Result<crate::client::PullRequest, GithubError> {
                self.inner.create_pull_request(_new)
            }
            fn get_pull_request(&self, number: u64) -> Result<Option<crate::client::PullRequest>, GithubError> {
                self.inner.get_pull_request(number)
            }
            fn find_pull_request_for_head(
                &self,
                head: &str,
            ) -> Result<Option<crate::client::PullRequest>, GithubError> {
                self.inner.find_pull_request_for_head(head)
            }
            fn checks_for_ref(&self, sha: &str) -> Result<crate::client::ChecksState, GithubError> {
                self.inner.checks_for_ref(sha)
            }
        }

        let inner = FakeGithub::new();
        // Seed a source issue and PR so the projection has valid homes without
        // needing to list.
        let view = synthetic_view_with_workpieces(&["issue-1"]);
        let bloom_id = view.blooms[0].id;
        // Create issue 1
        inner.create_issue(&NewIssue { title: "source".into(), body: "body".into() }).unwrap();
        let branch = format!("bloom/{}/landing", crate::short_hex(&bloom_id.0));
        let commit = inner.create_commit("landing", "tree", &[]).unwrap();
        inner.seed_ref(&format!("heads/{branch}"), &commit.sha);
        inner
            .create_pull_request(&crate::client::NewPullRequest {
                title: "landing".into(),
                body: String::new(),
                head: branch,
                base: "main".into(),
            })
            .unwrap();

        let projection = GithubProjection::new(NoListGithub { inner });
        // Both paths must succeed without touching `find_issue`.
        projection.reconcile_view(&view).unwrap();
        let receipt =
            aether_bloomery::LandingReceipt { bloom: bloom_id, previous_base: digest(0), new_head: digest(99) };
        projection.project_receipt(&receipt).unwrap();
    }

    #[test]
    fn workpiece_id_not_resolvable_is_skipped_without_shadow_issue() {
        // A workpiece whose id does not name an issue (e.g. "reactor-core")
        // must be handled explicitly rather than falling back to opening a
        // shadow issue. Old behaviour opened `Workpiece reactor-core` issues.
        let fake = FakeGithub::new();
        let view = synthetic_view_with_workpieces(&["reactor-core", "coolant-loop"]);
        let issues_before = fake.issue_count();

        let projection = GithubProjection::new(fake.clone());
        projection.reconcile_view(&view).unwrap();

        assert_eq!(fake.issue_count(), issues_before, "unresolvable workpiece must not open a shadow issue");
        assert_eq!(fake.comment_count(), 0, "no comment without a source-issue home");
    }

    #[test]
    fn missing_pr_and_missing_issue_are_both_skipped_explicitly() {
        // The two explicit fallback cases the task calls out: a bloom sealed
        // against a workpiece with no corresponding issue has no home, and the
        // landing pull request may not exist yet when the first view projects.
        // Neither may open a shadow issue.
        let fake = FakeGithub::new();
        let view = synthetic_view_with_workpieces(&["issue-42"]);
        // Do NOT seed issue 42 and do NOT seed the landing PR.
        let issues_before = fake.issue_count();

        let projection = GithubProjection::new(fake.clone());
        // Must not error and must not open an issue.
        projection.reconcile_view(&view).unwrap();
        assert_eq!(fake.issue_count(), issues_before, "missing source issue must not open shadow issue");
        assert_eq!(fake.comment_count(), 0, "no bloom PR yet, no source issue — nothing to comment on");

        let bloom_id = view.blooms[0].id;
        let receipt =
            aether_bloomery::LandingReceipt { bloom: bloom_id, previous_base: digest(0), new_head: digest(99) };
        projection.project_receipt(&receipt).unwrap();
        assert_eq!(fake.issue_count(), issues_before, "receipt with missing PR must not open shadow issue");
        assert_eq!(fake.comment_count(), 0, "no PR yet — receipt nowhere to land");
    }
}
