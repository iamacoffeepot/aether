//! Reading a task's inputs through its source: closure first, then descend.
//!
//! [`SourceReader::prefetch`] asks for a tree's whole closure in one
//! `ReadClosure` under the rest of the session's read budget. `Found` fills
//! the session's map in one round trip, its members the source's own shared
//! [`Blob`](aether_data::Blob)s, never copied. `TooLarge` reads that one tree
//! node with a `ReadArtifact` and pushes its subtrees onto an explicit work
//! stack, each prefetched in turn, so only the spine of oversized directories
//! and the blobs directly inside them are read one at a time. A lookup the
//! map misses is one `ReadArtifact`.
//!
//! Every member is verified against the digest it is read under before it is
//! trusted: a tree through [`ClosureArtifact::load`], a blob through a
//! [`StoredBlob`] that hashes as it streams and fails at its end on a
//! mismatch, so a corrupt blob never crosses into a container whole.

use std::collections::HashMap;
use std::io::{self, Read};

use aether_bloomery_kinds::{
    ClosureArtifact, ClosureLimit, Digest, Node, OpaqueBytes, ReadArtifact, ReadArtifactResult, ReadClosure,
    ReadClosureResult, Ref, Tree, VerifiedRead,
};
use aether_bloomery_tar::{SourceBlob, TreeSource};
use aether_data::{Kind, Storage};

use super::{StorageAnswer, StorageCall, StorageError, StoragePort};

/// The bytes a stored blob holds beyond its payload: its kind prefix.
const PREFIX_BYTES: u64 = 8;

/// What a session has read, keyed by digest, and how much of its read budget
/// is left.
pub struct Fetched {
    members: HashMap<Digest, ClosureArtifact>,
    remaining_bytes: u64,
}

impl Fetched {
    pub fn new(budget: ClosureLimit) -> Self {
        Self { members: HashMap::new(), remaining_bytes: budget.get() }
    }

    /// Keep `member` under `digest`, charging its stored length to the
    /// budget.
    fn keep(&mut self, digest: Digest, member: ClosureArtifact) {
        self.remaining_bytes = self.remaining_bytes.saturating_sub(member.len().saturating_add(PREFIX_BYTES));
        self.members.insert(digest, member);
    }

    /// The closure limit the rest of the budget allows, or `None` when it
    /// cannot cover even one kind prefix.
    fn limit(&self) -> Option<ClosureLimit> {
        ClosureLimit::new(self.remaining_bytes.min(ClosureLimit::MAX_BYTES)).ok()
    }
}

/// Reads through one session's port into its map.
pub struct SourceReader<'a> {
    port: &'a mut StoragePort,
    fetched: &'a mut Fetched,
}

impl<'a> SourceReader<'a> {
    pub(super) fn new(port: &'a mut StoragePort, fetched: &'a mut Fetched) -> Self {
        Self { port, fetched }
    }

    /// Read `root` and everything under it into the map: its closure in one
    /// request when it fits the budget left, otherwise its node alone and
    /// then each subtree the same way, over an explicit work stack.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] naming the first member the source lacks,
    /// or the failure of the read that broke.
    pub fn prefetch(&mut self, root: &Ref<Tree>) -> Result<(), StorageError> {
        let mut pending = vec![root.digest()];
        while let Some(digest) = pending.pop() {
            if self.fetched.members.contains_key(&digest) {
                continue;
            }
            let fits = match self.fetched.limit() {
                Some(limit_bytes) => self.closure(digest, limit_bytes)?,
                None => false,
            };
            if !fits {
                let tree: Tree = self.load(&Ref::from_digest(digest))?;
                pending.extend(tree.entries().values().filter_map(|node| match node {
                    Node::Directory(child) => Some(child.digest()),
                    _ => None,
                }));
            }
        }
        Ok(())
    }

    /// Read `root`'s closure under `limit_bytes` into the map, answering
    /// whether it fit.
    fn closure(&mut self, root: Digest, limit_bytes: ClosureLimit) -> Result<bool, StorageError> {
        let StorageAnswer::ReadClosure(result) =
            self.port.call(StorageCall::ReadClosure(ReadClosure { root, limit_bytes }))?
        else {
            return Err(StorageError::Answer);
        };
        match result {
            ReadClosureResult::Found { artifacts, .. } => {
                for member in artifacts {
                    self.fetched.keep(member.claimed().unverified(), member);
                }
                Ok(true)
            }
            ReadClosureResult::TooLarge { .. } => Ok(false),
            ReadClosureResult::Missing { digest, .. } => Err(StorageError::Missing(digest)),
            ReadClosureResult::Err { message, .. } => Err(StorageError::Refused(message)),
        }
    }

    /// Load and decode the artifact `artifact` names, verified against its
    /// digest, keeping it in the map.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] when the source lacks it, or why it did not
    /// read, verify, or decode.
    pub fn load<K: Storage>(&mut self, artifact: &Ref<K>) -> Result<K, StorageError> {
        let digest = artifact.digest();
        let member = self.member(digest, true)?;
        if member.kind() != K::ID {
            return Err(StorageError::OtherKind(digest));
        }
        let bytes = member.load(digest).map_err(StorageError::Mismatch)?;
        K::decode_storage(&bytes).map(|data| data.value).map_err(|_| StorageError::Decode(digest))
    }

    /// Read the blob `blob` names into the map without opening it, so a later
    /// [`Self::open`] finds it there.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] when the source lacks it, or why it did not
    /// read.
    pub fn require(&mut self, blob: &Ref<OpaqueBytes>) -> Result<(), StorageError> {
        self.member(blob.digest(), true).map(drop)
    }

    /// Open the blob `blob` names as a reader verified at its end. A blob the
    /// map misses is read and not kept.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] when the source lacks it, or why it did not
    /// read; [`StorageError::OtherKind`] when it is not stored as bytes.
    pub fn open(&mut self, blob: &Ref<OpaqueBytes>) -> Result<StoredBlob, StorageError> {
        let digest = blob.digest();
        let member = self.member(digest, false)?;
        if member.kind() != OpaqueBytes::ID {
            return Err(StorageError::OtherKind(digest));
        }
        Ok(StoredBlob(member.verified_reader(digest)))
    }

    /// The member under `digest`: from the map, or read with one
    /// `ReadArtifact` and kept when `keep` is set.
    fn member(&mut self, digest: Digest, keep: bool) -> Result<ClosureArtifact, StorageError> {
        if let Some(member) = self.fetched.members.get(&digest) {
            return Ok(member.clone());
        }
        let StorageAnswer::Read(result) = self.port.call(StorageCall::Read(ReadArtifact { digest }))? else {
            return Err(StorageError::Answer);
        };
        let member = match result {
            ReadArtifactResult::Found { artifact } => artifact,
            ReadArtifactResult::Missing { digest } => return Err(StorageError::Missing(digest)),
            ReadArtifactResult::Err { message, .. } => return Err(StorageError::Refused(message)),
        };
        if keep {
            self.fetched.keep(digest, member.clone());
        }
        Ok(member)
    }
}

impl TreeSource for SourceReader<'_> {
    type Error = StorageError;
    type Blob<'b>
        = StoredBlob
    where
        Self: 'b;

    fn tree(&mut self, tree: &Ref<Tree>) -> Result<Tree, StorageError> {
        self.load(tree)
    }

    fn blob(&mut self, blob: &Ref<OpaqueBytes>) -> Result<SourceBlob<StoredBlob>, StorageError> {
        self.open(blob).map(|reader| SourceBlob { len: reader.len(), reader })
    }
}

/// A stored blob read a window at a time, whose last read fails with
/// [`io::ErrorKind::InvalidData`] when its bytes do not hash to the digest it
/// was opened under.
pub struct StoredBlob(VerifiedRead);

impl StoredBlob {
    /// The blob's length in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.0.len()
    }
}

impl Read for StoredBlob {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf).map_err(|mismatch| io::Error::new(io::ErrorKind::InvalidData, mismatch))
    }
}
