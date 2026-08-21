//! Local-authority landing face (ADR-0199).
//!
//! The land reactor speaks [`LandingSource`], which on GitHub is a pull-request
//! ceremony. A fleet-local authority has no pull requests: the compare-and-swap
//! on the bare repository *is* the land. This adapter presents that swap as the
//! propose → accept → poll loop the reactor already drives, so a local boot
//! reuses the reactor unchanged. A local land has no pull request to close
//! through, but the repository it replicates onto still has the issue.

use std::sync::Arc;

use aether_bloomery::{BloomId, Digest, LandOutcome, LandingReceipt};
use aether_bloomery_github::{
    LandAcceptance, LandProposal, LandingProposal, LandingRefusal, LandingSource, ProposalOutcome, SourceError,
};

use super::SourceShell;

/// Synthetic proposal number the local face reports. The reactor needs a handle
/// to re-poll; there is only ever one in-flight land per bloom, so a constant
/// is enough.
const LOCAL_PROPOSAL: u64 = 1;

/// [`LandingSource`] over a local [`SourceShell`]: land is
/// [`SourceShell::land`], not a hosted merge.
pub struct LocalLanding {
    source: SourceShell,
    issues: Option<Arc<dyn LandingSource>>,
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
        Self { source, issues }
    }
}

impl LandingSource for LocalLanding {
    fn issue_title(&self, _number: u64) -> Result<Option<String>, SourceError> {
        Ok(None)
    }

    fn land_proposal(
        &self,
        _bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        _proposal: Option<&LandingProposal>,
    ) -> Result<ProposalOutcome, SourceError> {
        let actual = self.source.observe_mainline_head()?;
        if actual == *new_head || actual == *expected_base {
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
        Ok(LandProposal::Landed(LandingReceipt { bloom: *bloom, previous_base: *expected_base, new_head: actual }))
    }

    fn close_issue(&self, number: u64, key: &str, comment: &str) -> Result<(), SourceError> {
        self.issues.as_ref().map_or(Ok(()), |issues| issues.close_issue(number, key, comment))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use aether_bloomery_github::{GitSource, GithubLanding, LandingSource, MainlineRef, testing::FakeGithub};

    use super::LocalLanding;
    use crate::bloomery::SourceShell;

    fn shell(fake: &FakeGithub) -> SourceShell {
        SourceShell::new(Arc::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), false, MainlineRef::default())))
    }

    fn github(fake: &FakeGithub) -> GithubLanding<FakeGithub> {
        GithubLanding::new(GitSource::new(fake.clone(), Arc::new(fake.clone()), false, MainlineRef::default()))
    }

    #[test]
    fn a_local_authority_land_closes_through_its_issue_face() {
        // A coordinator running local authority reaches GitHub through no other
        // path, so an absent face would silently keep today's behaviour: the
        // issue stays open with no landing comment.
        let fake = FakeGithub::new();
        fake.seed_issue(4242, "the order");
        LocalLanding::with_issues(shell(&fake), Some(Arc::new(github(&fake))))
            .close_issue(4242, "receipt:bloom:abcd", "landed")
            .unwrap();
        assert_eq!(fake.issue_is_closed(4242), Some(true));
        assert_eq!(fake.comments_on(4242).len(), 1);
        assert!(fake.comments_on(4242)[0].contains("landed"), "{}", fake.comments_on(4242)[0]);

        let ignored = FakeGithub::new();
        ignored.seed_issue(4242, "the order");
        LocalLanding::new(shell(&ignored)).close_issue(4242, "receipt:bloom:abcd", "landed").unwrap();
        assert_eq!(ignored.issue_is_closed(4242), Some(false), "the None construction is an Ok(()) no-op");
        assert!(ignored.comments_on(4242).is_empty());
    }
}
