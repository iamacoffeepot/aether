//! Breadth-first transitive closure over the stored citation edges, under a byte budget.

use std::collections::{HashSet, VecDeque};

use aether_bloomery_kinds::{ClosureArtifact, ClosureLimit, artifact_digest};
use rusqlite::{Connection, OptionalExtension, params};

use crate::Digest;
use crate::artifact::split_artifact;
use crate::blobs::BlobDir;
use crate::journal::JournalError;

/// Outcome of [`crate::Journal::read_closure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closure {
    /// Every distinct reachable artifact: the root first, then breadth-first
    /// levels, each member's children in ascending digest byte order.
    Found(Vec<ClosureArtifact>),
    /// A member has no stored artifact. In a healthy journal only the root can be.
    Missing(Digest),
    /// The total stored blob length exceeds the limit. Nothing is returned.
    TooLarge,
}

/// Walk `root`'s closure in one read snapshot. Each digest is enqueued at
/// most once; the running total, taken from each row's recorded size, is
/// checked before the member's file is read, so the budget bounds the walk.
pub fn walk_closure(
    conn: &Connection,
    blobs: &BlobDir,
    root: Digest,
    limit: ClosureLimit,
) -> Result<Closure, JournalError> {
    let tx = conn.unchecked_transaction()?;
    let mut length = tx.prepare("SELECT size_bytes FROM artifacts WHERE digest = ?1")?;
    let mut children = tx.prepare("SELECT to_digest FROM citations WHERE from_digest = ?1 ORDER BY to_digest")?;

    let mut queue = VecDeque::from([root]);
    let mut visited = HashSet::from([root]);
    let mut total: u64 = 0;
    let mut artifacts = Vec::new();

    while let Some(digest) = queue.pop_front() {
        let key = digest.as_bytes().as_slice();
        let Some(size) = length.query_row(params![key], |row| row.get::<_, i64>(0)).optional()? else {
            return Ok(Closure::Missing(digest));
        };
        let size = u64::try_from(size).map_err(|_| JournalError::IntegerRange)?;
        total = total.saturating_add(size);
        if total > limit.get() {
            return Ok(Closure::TooLarge);
        }

        let bytes = blobs.read(&digest, size)?;
        let (kind, payload) = split_artifact(&bytes)?;
        if artifact_digest(kind, payload) != digest {
            return Err(JournalError::ArtifactDigestMismatch(digest));
        }
        artifacts.push(ClosureArtifact::new(kind, payload.to_vec()));

        let mut rows = children.query(params![key])?;
        while let Some(row) = rows.next()? {
            let raw: Vec<u8> = row.get(0)?;
            let child = Digest::from_bytes(raw.try_into().map_err(|_| JournalError::CorruptCitation)?);
            if visited.insert(child) {
                queue.push_back(child);
            }
        }
    }
    Ok(Closure::Found(artifacts))
}
