//! Split the emitted artifacts into publish batches under a byte budget.
//!
//! The split keeps emission order, so a tree whose children landed in an
//! earlier batch still verifies: the journal checks each citation against
//! rows staged in the same batch or already stored.

use std::mem;

use aether_bloomery_kinds::EncodedArtifact;
use anyhow::{Result, bail};

use super::tree::Staged;

/// Split `artifacts` into consecutive batches whose payload bytes sum to at
/// most `budget_bytes`, keeping their order.
///
/// # Errors
/// One artifact alone is over the budget; the error names its path.
pub(super) fn split(artifacts: Vec<Staged>, budget_bytes: usize) -> Result<Vec<Vec<EncodedArtifact>>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut batch_bytes = 0;
    for Staged { path, artifact } in artifacts {
        let size_bytes = artifact.bytes().len();
        if size_bytes > budget_bytes {
            bail!("`{path}` is {size_bytes} bytes, over the {budget_bytes}-byte publish budget (half the frame cap)");
        }

        if batch_bytes + size_bytes > budget_bytes {
            batches.push(mem::take(&mut batch));
            batch_bytes = 0;
        }
        batch.push(artifact);
        batch_bytes += size_bytes;
    }

    if !batch.is_empty() {
        batches.push(batch);
    }
    Ok(batches)
}
