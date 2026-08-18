//! Local-authority landing face (ADR-0199).
//!
//! The land reactor speaks [`LandingSource`], which on GitHub is a pull-request
//! ceremony. A fleet-local authority has no pull requests: the compare-and-swap
//! on the bare repository *is* the land. This adapter presents that swap as the
//! propose → accept → poll loop the reactor already drives, so a local boot
//! reuses the reactor unchanged.

use aether_bloomery::{BloomId, Digest, LandOutcome, LandingReceipt};
use aether_bloomery_github::{
    LandAcceptance, LandProposal, LandingProposal, LandingRefusal, LandingSource, ProposalOutcome, SourceError,
};

use super::SourceShell;

/// Synthetic proposal number the local face reports. The reactor needs a handle
/// to re-poll; there is only ever one in-flight land per bloom, so a constant
/// is enough and `close_issue` has nothing to close.
const LOCAL_PROPOSAL: u64 = 1;

/// [`LandingSource`] over a local [`SourceShell`]: land is
/// [`SourceShell::land`], not a hosted merge.
pub struct LocalLanding {
    source: SourceShell,
}

impl LocalLanding {
    /// Wrap the already-connected local source shell.
    #[must_use]
    pub fn new(source: SourceShell) -> Self {
        Self { source }
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

    fn close_issue(&self, _number: u64, _comment: &str) -> Result<(), SourceError> {
        Ok(())
    }
}
