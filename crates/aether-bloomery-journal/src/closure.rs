//! Breadth-first transitive closure over the stored citation edges, under a byte budget.
//!
//! A read plans the closure from the database rows first, [`plan_closure`],
//! so the budget and a missing member are decided before any member file is
//! read. It then reads the planned members: into one slab through a
//! [`BlobCheckIn`] on the journal actor's worker ([`read_slab`], called by
//! the crate's worker reader), or one buffer per member for
//! [`crate::Journal::read_closure`].

use std::collections::{HashSet, VecDeque};

use aether_bloomery_kinds::{ClosureArtifact, ClosureLimit};
use aether_data::{Blob, KindId};
use aether_substrate::actor::native::BlobCheckIn;
use rusqlite::{Connection, OptionalExtension, params};

use crate::Digest;
use crate::blobs::{self, BlobDir};
use crate::journal::JournalError;

/// Outcome of [`crate::Journal::read_closure`].
#[derive(Debug, Clone)]
pub enum Closure {
    /// Every distinct reachable artifact: the root first, then breadth-first
    /// levels, each member's children in ascending digest byte order.
    Found(Vec<ClosureArtifact>),
    /// A member has no stored artifact. In a healthy journal only the root can be.
    Missing(Digest),
    /// The total stored blob length exceeds the limit. Nothing is returned.
    TooLarge,
}

/// One planned member: its digest and its recorded stored length.
pub struct PlannedMember {
    digest: Digest,
    size_bytes: u64,
}

/// A closure planned from the database rows alone, before any member file
/// is read.
pub enum ClosurePlan {
    /// Every member in walk order, within the budget.
    Members(Vec<PlannedMember>),
    /// A member has no artifact row.
    Missing(Digest),
    /// The recorded sizes exceed the limit.
    TooLarge,
}

impl ClosurePlan {
    /// Read the planned members with `read`, or answer `Missing` or
    /// `TooLarge` without reading anything.
    pub fn read_with(
        self,
        read: impl FnOnce(Vec<PlannedMember>) -> Result<Vec<ClosureArtifact>, JournalError>,
    ) -> Result<Closure, JournalError> {
        match self {
            Self::Members(members) => read(members).map(Closure::Found),
            Self::Missing(digest) => Ok(Closure::Missing(digest)),
            Self::TooLarge => Ok(Closure::TooLarge),
        }
    }
}

/// Plan `root`'s closure in one read snapshot from the artifact and citation
/// rows only. Each digest is enqueued at most once, and the running total,
/// taken from each row's recorded size, is checked as each member is
/// planned, so the budget bounds the walk and no member file is read.
pub fn plan_closure(conn: &Connection, root: Digest, limit: ClosureLimit) -> Result<ClosurePlan, JournalError> {
    let tx = conn.unchecked_transaction()?;
    let mut length = tx.prepare("SELECT size_bytes FROM artifacts WHERE digest = ?1")?;
    let mut children = tx.prepare("SELECT to_digest FROM citations WHERE from_digest = ?1 ORDER BY to_digest")?;

    let mut queue = VecDeque::from([root]);
    let mut visited = HashSet::from([root]);
    let mut total: u64 = 0;
    let mut members = Vec::new();

    while let Some(digest) = queue.pop_front() {
        let key = digest.as_bytes().as_slice();
        let Some(size) = length.query_row(params![key], |row| row.get::<_, i64>(0)).optional()? else {
            return Ok(ClosurePlan::Missing(digest));
        };
        let size_bytes = u64::try_from(size).map_err(|_| JournalError::IntegerRange)?;
        total = total.saturating_add(size_bytes);
        if total > limit.get() {
            return Ok(ClosurePlan::TooLarge);
        }
        members.push(PlannedMember { digest, size_bytes });

        let mut rows = children.query(params![key])?;
        while let Some(row) = rows.next()? {
            let raw: Vec<u8> = row.get(0)?;
            let child = Digest::from_bytes(raw.try_into().map_err(|_| JournalError::CorruptCitation)?);
            if visited.insert(child) {
                queue.push_back(child);
            }
        }
    }
    Ok(ClosurePlan::Members(members))
}

/// Read each planned member into its own buffer, hand it to `check_in`, and
/// build the member over the [`Blob`] that returns.
pub fn read_each(
    blobs: &BlobDir,
    members: &[PlannedMember],
    mut check_in: impl FnMut(Box<[u8]>) -> Blob,
) -> Result<Vec<ClosureArtifact>, JournalError> {
    members
        .iter()
        .map(|member| {
            let (kind, payload) = blobs.read_payload(&member.digest, member.size_bytes)?;
            verified(member.digest, kind, check_in(payload))
        })
        .collect()
}

/// Read every planned member straight into its region of one slab checked
/// in through `check_in`, then build each member over its [`Blob`]. An error
/// before the slab is finished drops it, which frees it.
pub fn read_slab(
    blobs: &BlobDir,
    members: &[PlannedMember],
    check_in: &BlobCheckIn,
) -> Result<Vec<ClosureArtifact>, JournalError> {
    let lens = members.iter().map(|member| blobs::payload_len(member.size_bytes)).collect::<Result<Vec<_>, _>>()?;
    let mut slab = check_in.slab(&lens);
    let kinds = members
        .iter()
        .zip(slab.regions())
        .map(|(member, region)| blobs.read_payload_into(&member.digest, member.size_bytes, region))
        .collect::<Result<Vec<_>, _>>()?;

    members
        .iter()
        .zip(kinds)
        .zip(slab.finish())
        .map(|((member, kind), blob)| verified(member.digest, kind, blob))
        .collect()
}

/// The member of `kind` over `blob`, once its claim is checked against the
/// `digest` it was stored under.
fn verified(digest: Digest, kind: KindId, blob: Blob) -> Result<ClosureArtifact, JournalError> {
    let artifact = ClosureArtifact::new(kind, blob);
    if artifact.claimed().unverified() == digest {
        Ok(artifact)
    } else {
        Err(JournalError::ArtifactDigestMismatch(digest))
    }
}
