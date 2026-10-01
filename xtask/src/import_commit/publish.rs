//! Stage every batch through the journal's fenced publish, with no head move.
//!
//! A publish that moves no head appends no event, so the fence read once at
//! the start holds across every batch. A stale fence answers `Conflict`, and
//! the batch is resent at the actual sequence: staging is idempotent, so a
//! resend can only store what the first attempt did not.

use aether_bloomery_kinds::{Digest, EncodedArtifact, Publish, PublishResult};
use anyhow::{Result, bail};

pub(super) use crate::bloomery::Stage;
use crate::bloomery::publish_at_fence;

/// Stage `batches` in order, starting at the whole-journal fence `fence`.
///
/// # Errors
/// The journal refused a batch, answered with digests other than those
/// sent, or the transport failed.
pub(super) fn publish(stage: &mut impl Stage, mut fence: u64, batches: Vec<Vec<EncodedArtifact>>) -> Result<()> {
    let count = batches.len();
    for (index, artifacts) in batches.into_iter().enumerate() {
        let number = index + 1;
        let sent: Vec<Digest> = artifacts.iter().map(EncodedArtifact::digest).collect();

        fence = match publish_at_fence(stage, Publish::new(artifacts, Vec::new(), fence))? {
            PublishResult::Committed { head, artifacts } if artifacts == sent => head,
            PublishResult::Committed { artifacts, .. } => bail!(
                "batch {number} of {count}: the journal staged {} digests that differ from the {} sent",
                artifacts.len(),
                sent.len()
            ),
            PublishResult::Conflict { actual } => {
                bail!("batch {number} of {count}: the journal reported a conflict at the fence {actual} it was sent")
            }
            PublishResult::Err { message } => bail!("batch {number} of {count}: the journal refused it: {message}"),
        };
        eprintln!("import-commit: staged batch {number} of {count} ({} artifacts)", sent.len());
    }
    Ok(())
}
