//! GitHub landing presentation: propose, poll, accept, and close after local
//! truth is committed (ADR-0199).
//!
//! [`GitSource`] is the repository backend and is bounded on the git-data trait
//! alone. This module is the pull-request and issue face the land reactor drives
//! — it does not move mainline.

use std::fmt;
use std::sync::Arc;

use aether_bloomery::{BloomId, Digest, LandingReceipt};
use aether_bloomery_git::{
    GitDataApi, GitDataError, GitSource, GithubApi, IssueStateApi, MainlineRef, NewComment, NewPullRequest,
    PullMergeResult, PullRequestApi, PullRequestState, SourceError, landing_branch, short_hex, strip_heads, to_hex,
};

/// Where an issued land proposal has got to — the states a GitHub watch
/// distinguishes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LandProposal {
    /// Still open. Mainline has not moved; keep watching.
    Open,
    /// Accepted — mainline moved, and the receipt says where to.
    Landed(LandingReceipt),
    /// Terminated without landing — the proposal was declined.
    Declined,
    /// The proposal's own checks failed. No longer produced (#5110); kept so a
    /// residual encoding still decodes.
    ChecksFailed {
        /// The failing check names, in listing order.
        failing: Vec<String>,
    },
}

/// The outcome of opening a GitHub landing proposal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ProposalOutcome {
    /// The resolved head was proposed; mainline has not moved yet.
    Proposed {
        /// The pull-request number the watch re-reads.
        number: u64,
    },
    /// The land was refused: mainline had moved off the expected base.
    BaseMoved {
        /// The base the caller expected mainline to still be at.
        expected: Digest,
        /// The base mainline was actually at.
        actual: Digest,
    },
}

/// The prose a landing proposal is opened with, assembled by the caller that can
/// see the bloom's membership.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LandingProposal {
    /// The proposal's title, or `None` to land under the floor.
    pub title: Option<String>,
    /// The caller's half of the body. The provenance footer is appended below it.
    pub body: String,
}

/// What asking the port to accept a bloom's own landing proposal did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LandAcceptance {
    /// The proposal merged, or had already merged.
    Accepted,
    /// Nothing to accept yet.
    Pending,
    /// Refused, and why.
    Refused(LandingRefusal),
}

/// Why a landing acceptance refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LandingRefusal {
    /// The proposal no longer proposes what the bloom proved.
    Drifted {
        /// What moved, in the terms a reader of the journal needs.
        detail: String,
    },
    /// Mainline is no longer the base the bloom sealed against.
    BaseMoved {
        /// The sealed base the bloom proved against.
        expected: Digest,
        /// The base mainline actually stands at now.
        actual: Digest,
    },
    /// The source itself refused the merge.
    Merge {
        /// The refusing status.
        status: u16,
        /// The refusal body, verbatim.
        detail: String,
    },
}

fn refused_drift(detail: String) -> LandAcceptance {
    LandAcceptance::Refused(LandingRefusal::Drifted { detail })
}

impl fmt::Display for LandingRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Drifted { detail } => write!(f, "the landing proposal drifted off the proven head ({detail})"),
            Self::BaseMoved { expected, actual } => write!(
                f,
                "mainline moved off the sealed base after the proposal opened (sealed `{}`, now `{}`)",
                to_hex(expected),
                to_hex(actual),
            ),
            Self::Merge { status, detail } => write!(f, "the source refused the merge with {status}: {detail}"),
        }
    }
}

/// The title a landing proposal falls back to.
#[must_use]
pub fn landing_floor_title(bloom: &BloomId) -> String {
    format!("chore(meta): land bloom {}", short_hex(&bloom.0))
}

fn render_provenance_footer(
    bloom: &BloomId,
    expected_base: &Digest,
    new_head: &Digest,
    mainline: &MainlineRef,
) -> String {
    format!(
        "---\n\n\
         Landing bloom `{}` onto `{mainline}`.\n\n\
         - sealed base: `{}`\n\
         - resolved head: `{}`\n\n\
         Bloomery opened this proposal after the bloom resolved and its aggregate \
         review passed. Merging it is what lands the bloom: the merge is observed, \
         a `Fact::Land` is admitted against the commit mainline actually becomes, \
         and the next bloom seals on that receipt.\n\n\
         Closing it without merging leaves the bloom resolved and supersedable.\n",
        to_hex(&bloom.0),
        to_hex(expected_base),
        to_hex(new_head),
    )
}

fn render_landing_proposal(
    bloom: &BloomId,
    expected_base: &Digest,
    new_head: &Digest,
    mainline: &MainlineRef,
    proposal: Option<&LandingProposal>,
) -> (String, String) {
    let footer = render_provenance_footer(bloom, expected_base, new_head, mainline);
    let title = proposal.and_then(|proposal| proposal.title.clone()).unwrap_or_else(|| landing_floor_title(bloom));
    let body = match proposal.map(|proposal| proposal.body.trim()) {
        Some(assembled) if !assembled.is_empty() => format!("{assembled}\n\n{footer}"),
        _ => footer,
    };
    (title, body)
}

/// The GitHub landing-assembly face: propose, poll, accept, and close.
pub trait LandingSource: Send + Sync {
    /// The human-authored title of issue `number`, or `None` when absent.
    ///
    /// # Errors
    /// The surface is unreachable or returned a non-404 error status.
    fn issue_title(&self, number: u64) -> Result<Option<String>, SourceError>;

    /// Propose landing `new_head` onto mainline under caller-assembled prose.
    ///
    /// # Errors
    /// [`SourceError::LandingDisabled`] while the land gate is off, or a
    /// transport/backend fault.
    fn land_proposal(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        proposal: Option<&LandingProposal>,
    ) -> Result<ProposalOutcome, SourceError>;

    /// Accept the landing proposal numbered `number`.
    ///
    /// # Errors
    /// [`SourceError::LandingDisabled`] while the land gate is off, or a
    /// transport/backend fault.
    fn accept_land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        number: u64,
    ) -> Result<LandAcceptance, SourceError>;

    /// Read where a previously issued land proposal has got to.
    ///
    /// # Errors
    /// A transport or backend fault.
    fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, number: u64) -> Result<LandProposal, SourceError>;

    /// Close issue `number` after leaving `comment` on it.
    ///
    /// # Errors
    /// The surface is unreachable, the issue is absent, or either write was refused.
    fn close_issue(&self, number: u64, comment: &str) -> Result<(), SourceError>;
}

/// GitHub landing over a repository-backed [`GitSource`].
pub struct GithubLanding<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi> {
    source: Arc<GitSource<C>>,
}

impl<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi> Clone for GithubLanding<C> {
    fn clone(&self) -> Self {
        Self { source: Arc::clone(&self.source) }
    }
}

impl<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi> GithubLanding<C> {
    /// Wrap a git-data [`GitSource`] as the GitHub landing face.
    #[must_use]
    pub fn new(source: GitSource<C>) -> Self {
        Self { source: Arc::new(source) }
    }

    /// Share an existing repository backend as the GitHub landing face.
    #[must_use]
    pub fn from_arc(source: Arc<GitSource<C>>) -> Self {
        Self { source }
    }

    /// Borrow the underlying repository backend.
    #[must_use]
    pub fn source(&self) -> &GitSource<C> {
        &self.source
    }
}

impl<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi + Send + Sync> LandingSource for GithubLanding<C> {
    fn issue_title(&self, number: u64) -> Result<Option<String>, SourceError> {
        Ok(self
            .source
            .client()
            .issue_title(number)?
            .map(|title| title.trim().to_owned())
            .filter(|title| !title.is_empty()))
    }

    fn close_issue(&self, number: u64, comment: &str) -> Result<(), SourceError> {
        let commented =
            self.source.client().create_comment(&NewComment { issue_number: number, body: comment.to_owned() });
        let closed = self.source.client().close_issue(number);
        commented?;
        Ok(closed?)
    }

    fn land_proposal(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        proposal: Option<&LandingProposal>,
    ) -> Result<ProposalOutcome, SourceError> {
        if !self.source.cas_land_enabled() {
            return Err(SourceError::LandingDisabled);
        }
        let branch = landing_branch(bloom);
        if let Some(existing) = self.source.client().find_pull_request_for_head(&branch)? {
            if existing.state == PullRequestState::Open {
                let proven = self.source.resolve_git_sha(new_head, "land new head digest")?;
                if existing.head_sha != proven {
                    self.source.point_landing_branch(bloom, &proven)?;
                }
            }
            return Ok(ProposalOutcome::Proposed { number: existing.number });
        }

        let actual = self.source.mainline_digest(&self.source.mainline_head()?.sha)?;
        if actual != *expected_base {
            return Ok(ProposalOutcome::BaseMoved { expected: *expected_base, actual });
        }

        self.source.point_landing_branch(bloom, &self.source.resolve_git_sha(new_head, "land new head digest")?)?;
        let (title, body) = render_landing_proposal(bloom, expected_base, new_head, self.source.mainline(), proposal);
        let opening =
            NewPullRequest { title, body, head: branch.clone(), base: self.source.mainline().branch().to_owned() };
        match self.source.client().create_pull_request(&opening) {
            Ok(opened) => Ok(ProposalOutcome::Proposed { number: opened.number }),
            Err(GitDataError::RefConflict(detail)) => self
                .source
                .client()
                .find_pull_request_for_head(&branch)?
                .map(|raced| ProposalOutcome::Proposed { number: raced.number })
                .ok_or(SourceError::Git(GitDataError::RefConflict(detail))),
            Err(error) => Err(SourceError::Git(error)),
        }
    }

    fn accept_land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        number: u64,
    ) -> Result<LandAcceptance, SourceError> {
        if !self.source.cas_land_enabled() {
            return Err(SourceError::LandingDisabled);
        }

        let Some(pull) = self.source.client().get_pull_request(number)? else {
            return Ok(refused_drift(format!("proposal #{number} is gone")));
        };
        if pull.merged {
            return Ok(LandAcceptance::Accepted);
        }
        if pull.state != PullRequestState::Open {
            return Ok(refused_drift(format!("proposal #{number} is closed without having merged")));
        }
        let branch = landing_branch(bloom);
        if strip_heads(&pull.head_ref) != branch {
            return Ok(refused_drift(format!(
                "proposal #{number} proposes `{}`, not this bloom's landing branch `{branch}`",
                pull.head_ref,
            )));
        }
        if strip_heads(&pull.base) != self.source.mainline().branch() {
            return Ok(refused_drift(format!(
                "proposal #{number} aims at `{}`, not `{}`",
                pull.base,
                self.source.mainline().branch(),
            )));
        }

        let proven = self.source.resolve_git_sha(new_head, "landing acceptance head digest")?;
        if pull.head_sha != proven {
            return Ok(refused_drift(format!(
                "proposal #{number} is at `{}`, not the proven head `{proven}`",
                pull.head_sha,
            )));
        }

        let actual = self.source.mainline_digest(&self.source.mainline_head()?.sha)?;
        if actual != *expected_base {
            return Ok(LandAcceptance::Refused(LandingRefusal::BaseMoved { expected: *expected_base, actual }));
        }

        match self.source.client().squash_merge_pull_request(number, &pull.head_sha)? {
            PullMergeResult::Merged { .. } => Ok(LandAcceptance::Accepted),
            PullMergeResult::Refused { status, detail } => {
                Ok(LandAcceptance::Refused(LandingRefusal::Merge { status, detail }))
            }
        }
    }

    fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, number: u64) -> Result<LandProposal, SourceError> {
        let Some(pull) = self.source.client().get_pull_request(number)? else {
            return Ok(LandProposal::Declined);
        };
        let Some(merge_commit) = pull.merge_commit_sha else {
            if pull.state != PullRequestState::Open {
                return Ok(LandProposal::Declined);
            }
            return Ok(LandProposal::Open);
        };

        let new_head = self.source.record_landed_commit(&merge_commit)?;
        Ok(LandProposal::Landed(LandingReceipt { bloom: *bloom, previous_base: *expected_base, new_head }))
    }
}

impl<T: LandingSource + ?Sized> LandingSource for &T {
    fn issue_title(&self, number: u64) -> Result<Option<String>, SourceError> {
        (**self).issue_title(number)
    }

    fn land_proposal(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        proposal: Option<&LandingProposal>,
    ) -> Result<ProposalOutcome, SourceError> {
        (**self).land_proposal(bloom, expected_base, new_head, proposal)
    }

    fn accept_land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        number: u64,
    ) -> Result<LandAcceptance, SourceError> {
        (**self).accept_land(bloom, expected_base, new_head, number)
    }

    fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, number: u64) -> Result<LandProposal, SourceError> {
        (**self).poll_land(bloom, expected_base, number)
    }

    fn close_issue(&self, number: u64, comment: &str) -> Result<(), SourceError> {
        (**self).close_issue(number, comment)
    }
}

impl<T: LandingSource + ?Sized> LandingSource for Arc<T> {
    fn issue_title(&self, number: u64) -> Result<Option<String>, SourceError> {
        (**self).issue_title(number)
    }

    fn land_proposal(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        proposal: Option<&LandingProposal>,
    ) -> Result<ProposalOutcome, SourceError> {
        (**self).land_proposal(bloom, expected_base, new_head, proposal)
    }

    fn accept_land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        number: u64,
    ) -> Result<LandAcceptance, SourceError> {
        (**self).accept_land(bloom, expected_base, new_head, number)
    }

    fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, number: u64) -> Result<LandProposal, SourceError> {
        (**self).poll_land(bloom, expected_base, number)
    }

    fn close_issue(&self, number: u64, comment: &str) -> Result<(), SourceError> {
        (**self).close_issue(number, comment)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aether_bloomery::{BloomId, Digest};
    use aether_bloomery_git::{GitSource, MainlineRef, landing_branch, to_hex};

    use super::{
        GithubLanding, LandAcceptance, LandProposal, LandingRefusal, LandingSource, ProposalOutcome,
        landing_floor_title,
    };
    use crate::client::ChecksState;
    use crate::client::PullRequestApi;
    use crate::testing::FakeGithub;

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn bloom() -> BloomId {
        BloomId(digest(1))
    }

    fn landing(fake: &FakeGithub, cas_land_enabled: bool) -> GithubLanding<FakeGithub> {
        landing_on(fake, cas_land_enabled, MainlineRef::default())
    }

    fn landing_on(fake: &FakeGithub, cas_land_enabled: bool, mainline: MainlineRef) -> GithubLanding<FakeGithub> {
        GithubLanding::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), cas_land_enabled, mainline))
    }

    fn proposed(fake: &FakeGithub, base: &Digest, new_head: &Digest) -> (GithubLanding<FakeGithub>, u64, String) {
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(base, &"a1".repeat(20));
        fake.seed_git_object(new_head);
        let source = landing(fake, true);
        let ProposalOutcome::Proposed { number } =
            source.land_proposal(&bloom(), base, new_head, None).expect("the landing fixture")
        else {
            panic!("the port opens the proposal it is then asked to accept");
        };
        let head_sha = fake.pull_request_head_sha(number).expect("the proposal has a head");
        (source, number, head_sha)
    }

    #[test]
    fn land_proposes_the_resolved_head_and_never_writes_mainline() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        let mainline_sha1 = "a1".repeat(20);
        fake.seed_ref("heads/main", &mainline_sha1);
        fake.seed_correspondence(&base, &mainline_sha1);
        let new_head = digest(90);
        fake.seed_git_object(&new_head);

        match landing(&fake, false).land_proposal(&bloom, &base, &new_head, None) {
            Err(aether_bloomery_git::SourceError::LandingDisabled) => {}
            other => panic!("expected LandingDisabled, got {other:?}"),
        }

        let enabled = landing(&fake, true);
        let number = match enabled.land_proposal(&bloom, &base, &new_head, None).expect("the landing fixture") {
            ProposalOutcome::Proposed { number } => number,
            other @ ProposalOutcome::BaseMoved { .. } => panic!("expected Proposed, got {other:?}"),
        };
        assert_eq!(
            fake.ref_target(&format!("heads/{}", landing_branch(&bloom))),
            Some(to_hex(&new_head)),
            "the landing branch points at the resolved head",
        );
        // Tripwire: GitHub presentation must never write mainline. A slip back
        // to the repository CAS would 403 against a protected real repo.
        assert_eq!(fake.ref_target("heads/main"), Some(mainline_sha1), "land never writes mainline");
        assert!(fake.get_pull_request(number).expect("the landing fixture").is_some(), "the proposal exists");
    }

    #[test]
    fn a_repointed_mainline_proposes_against_the_configured_ref() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let day = MainlineRef::new("refs/heads/bloomery/daily/2026-08-13");
        let base = digest(10);
        let day_sha = "a1".repeat(20);
        fake.seed_ref(day.git_ref(), &day_sha);
        fake.seed_correspondence(&base, &day_sha);
        fake.seed_ref("heads/main", &"b2".repeat(20));
        let new_head = digest(90);
        fake.seed_git_object(&new_head);

        let source = landing_on(&fake, true, day.clone());
        let ProposalOutcome::Proposed { number } =
            source.land_proposal(&bloom, &base, &new_head, None).expect("the landing fixture")
        else {
            panic!("the sealed base is what the day branch holds, so the land proposes");
        };
        assert_eq!(
            fake.get_pull_request(number).expect("the landing fixture").expect("the proposal exists").base,
            day.branch(),
            "the landing is proposed onto the day branch",
        );
        let (_, body) = fake.pull_request_proposal(number).expect("the proposal's prose is recorded");
        assert!(body.contains(&day.to_string()), "the provenance footer names the ref it lands onto: {body}");
    }

    #[test]
    fn land_adopts_the_proposal_it_already_opened() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(&base, &"a1".repeat(20));
        let new_head = digest(90);
        fake.seed_git_object(&new_head);
        let source = landing(&fake, true);

        let first = source.land_proposal(&bloom, &base, &new_head, None).expect("the landing fixture");
        let second = source.land_proposal(&bloom, &base, &new_head, None).expect("the landing fixture");
        assert_eq!(first, second, "a re-issued land adopts the same proposal");
    }

    #[test]
    fn land_after_a_refine_repoints_an_open_proposal_onto_the_new_proven_head() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(&base, &"a1".repeat(20));
        let refused = digest(90);
        let refined = digest(91);
        fake.seed_git_object(&refused);
        fake.seed_git_object(&refined);
        let source = landing(&fake, true);

        let ProposalOutcome::Proposed { number } =
            source.land_proposal(&bloom, &base, &refused, None).expect("the landing fixture")
        else {
            panic!("the first land opens the proposal");
        };
        assert_eq!(
            source.land_proposal(&bloom, &base, &refined, None).expect("the landing fixture"),
            ProposalOutcome::Proposed { number },
            "the second land adopts the same proposal",
        );
        assert_eq!(
            fake.ref_target(&format!("heads/{}", landing_branch(&bloom))),
            Some(to_hex(&refined)),
            "the adopted proposal's branch now points at the refined head",
        );
    }

    #[test]
    fn an_open_proposal_is_adopted_even_after_mainline_moved_off_the_sealed_base() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(&base, &"a1".repeat(20));
        let new_head = digest(90);
        fake.seed_git_object(&new_head);
        let enabled = landing(&fake, true);

        let ProposalOutcome::Proposed { number } =
            enabled.land_proposal(&bloom, &base, &new_head, None).expect("the landing fixture")
        else {
            panic!("the first land opens the proposal");
        };
        fake.seed_ref("heads/main", &"d4".repeat(20));
        assert_eq!(
            enabled.land_proposal(&bloom, &base, &new_head, None).expect("the landing fixture"),
            ProposalOutcome::Proposed { number },
            "the same proposal is re-adopted so the watch can still reach its terminal",
        );
    }

    #[test]
    fn an_open_proposal_stays_open_regardless_of_check_state() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, head_sha) = proposed(&fake, &base, &new_head);

        for state in [
            ChecksState::Absent,
            ChecksState::Pending,
            ChecksState::Passed,
            ChecksState::Failed { failing: vec!["Clippy".into(), "Rustdoc".into()] },
        ] {
            fake.seed_checks(&head_sha, state.clone());
            assert_eq!(
                source.poll_land(&bloom(), &base, number).expect("the landing fixture"),
                LandProposal::Open,
                "a {state:?} check is not a landing verdict",
            );
        }
    }

    #[test]
    fn poll_land_reports_the_squash_commit_as_the_landed_head_and_records_it() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);
        assert_eq!(source.poll_land(&bloom(), &base, number).expect("the landing fixture"), LandProposal::Open);

        let squashed = "5c".repeat(20);
        fake.merge_pull_request(number, &squashed);
        let LandProposal::Landed(receipt) = source.poll_land(&bloom(), &base, number).expect("the landing fixture")
        else {
            panic!("expected Landed");
        };
        assert_eq!(receipt.previous_base, base);
        assert_ne!(receipt.new_head, new_head, "the landed head is the squash commit, not the proposed head");
    }

    #[test]
    fn poll_land_reports_declined_for_a_closed_or_vanished_proposal() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);
        fake.close_pull_request(number);
        assert_eq!(source.poll_land(&bloom(), &base, number).expect("the landing fixture"), LandProposal::Declined);
        assert_eq!(source.poll_land(&bloom(), &base, 9999).expect("the landing fixture"), LandProposal::Declined);
    }

    #[test]
    fn a_structurally_valid_proposal_is_accepted() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);

        assert_eq!(
            source.accept_land(&bloom(), &base, &new_head, number).expect("the landing fixture"),
            LandAcceptance::Accepted
        );
        assert_eq!(fake.pull_request_merged(number), Some(true));
        let LandProposal::Landed(receipt) = source.poll_land(&bloom(), &base, number).expect("the landing fixture")
        else {
            panic!("the accepted proposal reads as landed");
        };
        assert_ne!(receipt.new_head, new_head);
    }

    #[test]
    fn accepting_a_landing_refuses_while_the_land_gate_is_off() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (_, number, _) = proposed(&fake, &base, &new_head);

        match landing(&fake, false).accept_land(&bloom(), &base, &new_head, number) {
            Err(aether_bloomery_git::SourceError::LandingDisabled) => {}
            other => panic!("expected LandingDisabled, got {other:?}"),
        }
        assert_eq!(fake.pull_request_merged(number), Some(false));
    }

    #[test]
    fn a_proposal_is_accepted_without_consulting_check_state() {
        for state in [
            ChecksState::Absent,
            ChecksState::Pending,
            ChecksState::Passed,
            ChecksState::Failed { failing: vec!["CI pass".into()] },
        ] {
            let fake = FakeGithub::new();
            let (base, new_head) = (digest(10), digest(90));
            let (source, number, head_sha) = proposed(&fake, &base, &new_head);
            fake.seed_checks(&head_sha, state.clone());

            assert_eq!(
                source.accept_land(&bloom(), &base, &new_head, number).expect("the landing fixture"),
                LandAcceptance::Accepted,
                "a {state:?} check does not block acceptance",
            );
        }
    }

    #[test]
    fn a_proposal_whose_head_moved_off_the_proven_one_is_refused() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);
        let pushed = "ee".repeat(20);
        fake.push_to_pull_request(number, &pushed);

        match source.accept_land(&bloom(), &base, &new_head, number).expect("the landing fixture") {
            LandAcceptance::Refused(LandingRefusal::Drifted { detail }) => {
                assert!(detail.contains(&pushed), "the refusal names the head it found: {detail}");
            }
            other => panic!("expected a drift refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_base_that_moved_after_the_proposal_opened_refuses_the_merge() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);
        let moved = digest(77);
        fake.seed_ref("heads/main", &"d4".repeat(20));
        fake.seed_correspondence(&moved, &"d4".repeat(20));

        match source.accept_land(&bloom(), &base, &new_head, number).expect("the landing fixture") {
            LandAcceptance::Refused(LandingRefusal::BaseMoved { expected, actual }) => {
                assert_eq!(expected, base);
                assert_eq!(actual, moved);
            }
            other => panic!("expected a base-moved refusal, got {other:?}"),
        }
    }

    #[test]
    fn accepting_an_already_merged_proposal_is_the_idempotent_no_op() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);
        fake.merge_pull_request(number, &"5c".repeat(20));
        assert_eq!(
            source.accept_land(&bloom(), &base, &new_head, number).expect("the landing fixture"),
            LandAcceptance::Accepted
        );
    }

    #[test]
    fn close_issue_comments_then_closes() {
        let fake = FakeGithub::new();
        fake.seed_issue(7, "the order");
        landing(&fake, false).close_issue(7, "landed via pull request #3").expect("the landing fixture");
        assert_eq!(fake.comments_on(7), ["landed via pull request #3"]);
        assert_eq!(fake.issue_is_closed(7), Some(true));
    }

    #[test]
    fn close_issue_on_a_missing_target_is_an_error() {
        let fake = FakeGithub::new();
        assert!(landing(&fake, false).close_issue(7, "landed via pull request #3").is_err());
        assert_eq!(fake.issue_is_closed(7), None);
    }

    #[test]
    fn landing_floor_title_names_the_bloom() {
        let title = landing_floor_title(&bloom());
        assert!(title.starts_with("chore(meta): land bloom "), "{title}");
    }
}
