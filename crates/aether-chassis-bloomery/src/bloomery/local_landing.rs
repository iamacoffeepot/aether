//! Local-authority landing face (ADR-0199).
//!
//! The land reactor speaks [`LandingSource`], which on GitHub is a pull-request
//! ceremony. A fleet-local authority has no pull requests: the compare-and-swap
//! on the bare repository *is* the land. This adapter presents that swap as the
//! propose → accept → poll loop the reactor already drives, so a local boot
//! reuses the reactor unchanged. A local land has no pull request to close
//! through, but the repository it replicates onto still has the issue.

use std::sync::{Arc, Mutex, PoisonError};

use aether_bloomery::{BloomId, Digest, LandOutcome, LandingReceipt};
use aether_bloomery_github::{
    LandAcceptance, LandProposal, LandingProposal, LandingRefusal, LandingSource, ProposalOutcome, SourceError,
};

use super::SourceShell;

/// Synthetic proposal number the local face reports. The reactor needs a handle
/// to re-poll; there is only ever one in-flight land per bloom, so a constant
/// is enough.
const LOCAL_PROPOSAL: u64 = 1;

/// The head [`LocalLanding::land_proposal`] offered. Local land has no pull
/// request whose merge commit can name the tip, so [`LocalLanding::poll_land`]
/// matches live mainline against this rather than treating any movement as CAS.
struct LocalProposal {
    bloom: BloomId,
    expected_base: Digest,
    new_head: Digest,
}

/// [`LandingSource`] over a local [`SourceShell`]: land is
/// [`SourceShell::land`], not a hosted merge.
pub struct LocalLanding {
    source: SourceShell,
    issues: Option<Arc<dyn LandingSource>>,
    proposed: Mutex<Option<LocalProposal>>,
}

impl LocalLanding {
    /// Wrap the already-connected local source shell with no GitHub closer.
    #[must_use]
    pub fn new(source: SourceShell) -> Self {
        Self::with_issues(source, None)
    }

    /// Wrap the local source shell, optionally with a GitHub-backed closer.
    ///
    /// The land gate guards [`LandingSource::land_proposal`], not
    /// [`LandingSource::close_issue`], so a closer is usable with CAS land off.
    #[must_use]
    pub fn with_issues(source: SourceShell, issues: Option<Arc<dyn LandingSource>>) -> Self {
        Self { source, issues, proposed: Mutex::new(None) }
    }
}

impl LandingSource for LocalLanding {
    fn issue_title(&self, _number: u64) -> Result<Option<String>, SourceError> {
        Ok(None)
    }

    fn land_proposal(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        _proposal: Option<&LandingProposal>,
    ) -> Result<ProposalOutcome, SourceError> {
        let actual = self.source.observe_mainline_head()?;
        if actual == *new_head || actual == *expected_base {
            *self.proposed.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(LocalProposal { bloom: *bloom, expected_base: *expected_base, new_head: *new_head });
            return Ok(ProposalOutcome::Proposed { number: LOCAL_PROPOSAL });
        }
        Ok(ProposalOutcome::BaseMoved { expected: *expected_base, actual })
    }

    fn accept_land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        _number: u64,
    ) -> Result<LandAcceptance, SourceError> {
        match self.source.land(bloom, expected_base, new_head)? {
            LandOutcome::Landed { .. } => Ok(LandAcceptance::Accepted),
            LandOutcome::BaseMoved { expected, actual } => {
                Ok(LandAcceptance::Refused(LandingRefusal::BaseMoved { expected, actual }))
            }
        }
    }

    fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, _number: u64) -> Result<LandProposal, SourceError> {
        let actual = self.source.observe_mainline_head()?;
        if actual == *expected_base {
            return Ok(LandProposal::Open);
        }
        // A local land has no pull-request merge commit. The reactor takes Landed
        // as the CAS having succeeded and skips accept_land, so any other tip than
        // the head this bloom proposed is someone else's write: stay Open so the
        // accept path can refuse BaseMoved rather than close issues on a foreign tip.
        let proposed = self.proposed.lock().unwrap_or_else(PoisonError::into_inner);
        if proposed.as_ref().is_some_and(|proposal| {
            proposal.bloom == *bloom && proposal.expected_base == *expected_base && proposal.new_head == actual
        }) {
            return Ok(LandProposal::Landed(LandingReceipt {
                bloom: *bloom,
                previous_base: *expected_base,
                new_head: actual,
            }));
        }
        Ok(LandProposal::Open)
    }

    fn close_issue(&self, number: u64, key: &str, comment: &str) -> Result<(), SourceError> {
        self.issues.as_ref().map_or(Ok(()), |issues| issues.close_issue(number, key, comment))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use aether_bloomery::{BloomId, Digest};
    use aether_bloomery_github::{
        GitSource, GithubLanding, LandAcceptance, LandProposal, LandingRefusal, LandingSource, MainlineRef,
        ProposalOutcome, testing::FakeGithub,
    };

    use super::{LOCAL_PROPOSAL, LocalLanding};
    use crate::bloomery::SourceShell;

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn shell(fake: &FakeGithub, cas_land_enabled: bool) -> SourceShell {
        SourceShell::new(Arc::new(GitSource::new(
            fake.clone(),
            Arc::new(fake.clone()),
            cas_land_enabled,
            MainlineRef::default(),
        )))
    }

    fn github(fake: &FakeGithub) -> GithubLanding<FakeGithub> {
        GithubLanding::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), false, MainlineRef::default()))
    }

    fn local(fake: &FakeGithub) -> LocalLanding {
        LocalLanding::new(shell(fake, true))
    }

    fn bloom() -> BloomId {
        BloomId(digest(1))
    }

    fn proposed(fake: &FakeGithub, base: &Digest, new_head: &Digest) -> LocalLanding {
        fake.seed_git_object(new_head);
        fake.seed_ref_at("heads/main", base);
        let landing = local(fake);
        match landing.land_proposal(&bloom(), base, new_head, None).unwrap() {
            ProposalOutcome::Proposed { number } => assert_eq!(number, LOCAL_PROPOSAL),
            other => panic!("expected Proposed, got {other:?}"),
        }
        landing
    }

    #[test]
    fn a_local_authority_land_closes_through_its_issue_face() {
        // A coordinator running local authority reaches GitHub through no other
        // path, so an absent face would silently keep today's behaviour: the
        // issue stays open with no landing comment.
        let fake = FakeGithub::new();
        fake.seed_issue(4242, "the order");
        LocalLanding::with_issues(shell(&fake, false), Some(Arc::new(github(&fake))))
            .close_issue(4242, "receipt:bloom:abcd", "landed")
            .unwrap();
        assert_eq!(fake.issue_is_closed(4242), Some(true));
        assert_eq!(fake.comments_on(4242).len(), 1);
        assert!(fake.comments_on(4242)[0].contains("landed"), "{}", fake.comments_on(4242)[0]);

        let ignored = FakeGithub::new();
        ignored.seed_issue(4242, "the order");
        LocalLanding::new(shell(&ignored, false)).close_issue(4242, "receipt:bloom:abcd", "landed").unwrap();
        assert_eq!(ignored.issue_is_closed(4242), Some(false), "the None construction is an Ok(()) no-op");
        assert!(ignored.comments_on(4242).is_empty());
    }

    #[test]
    fn poll_land_does_not_treat_unrelated_mainline_movement_as_this_blooms_land() {
        // The reactor takes LandProposal::Landed as the CAS having already
        // succeeded and skips accept_land. Any other head than the one this
        // bloom proposed is someone else's write: stay Open so accept_land can
        // refuse BaseMoved rather than closing issues against a foreign tip.
        let fake = FakeGithub::new();
        let base = fake.seed_base_commit(&digest(10));
        let new_head = digest(90);
        let unrelated = digest(77);
        let landing = proposed(&fake, &base, &new_head);

        fake.seed_git_object(&unrelated);
        fake.seed_ref_at("heads/main", &unrelated);
        match landing.poll_land(&bloom(), &base, LOCAL_PROPOSAL).unwrap() {
            LandProposal::Landed(receipt) => {
                panic!("unrelated tip {} was taken as this bloom's land", receipt.new_head.to_hex())
            }
            other => assert_eq!(other, LandProposal::Open),
        }
        match landing.accept_land(&bloom(), &base, &new_head, LOCAL_PROPOSAL).unwrap() {
            LandAcceptance::Refused(LandingRefusal::BaseMoved { expected, actual }) => {
                assert_eq!(expected, base);
                assert_eq!(actual, unrelated);
            }
            other => panic!("expected BaseMoved, got {other:?}"),
        }
    }

    #[test]
    fn poll_land_reports_landed_only_when_mainline_is_the_proposed_head() {
        let fake = FakeGithub::new();
        let base = fake.seed_base_commit(&digest(10));
        let new_head = digest(90);
        let landing = proposed(&fake, &base, &new_head);
        assert_eq!(landing.poll_land(&bloom(), &base, LOCAL_PROPOSAL).unwrap(), LandProposal::Open);

        fake.seed_ref_at("heads/main", &new_head);
        let LandProposal::Landed(receipt) = landing.poll_land(&bloom(), &base, LOCAL_PROPOSAL).unwrap() else {
            panic!("the proposed head on mainline is this bloom's land");
        };
        assert_eq!(receipt.bloom, bloom());
        assert_eq!(receipt.previous_base, base);
        assert_eq!(receipt.new_head, new_head);
    }

    #[test]
    fn a_proposal_opened_against_an_already_landed_head_polls_as_that_land() {
        // land_proposal treats actual == new_head as Proposed so a restart after
        // the CAS can re-observe. poll_land must name that head, not stay Open
        // and not accept some later foreign tip as the land.
        let fake = FakeGithub::new();
        let base = digest(10);
        let new_head = digest(90);
        fake.seed_git_object(&new_head);
        fake.seed_ref_at("heads/main", &new_head);
        let landing = local(&fake);
        match landing.land_proposal(&bloom(), &base, &new_head, None).unwrap() {
            ProposalOutcome::Proposed { number } => assert_eq!(number, LOCAL_PROPOSAL),
            other => panic!("expected Proposed, got {other:?}"),
        }
        let LandProposal::Landed(receipt) = landing.poll_land(&bloom(), &base, LOCAL_PROPOSAL).unwrap() else {
            panic!("mainline already at the proposed head is this bloom's land");
        };
        assert_eq!(receipt.new_head, new_head);
        assert_eq!(receipt.previous_base, base);
    }
}
