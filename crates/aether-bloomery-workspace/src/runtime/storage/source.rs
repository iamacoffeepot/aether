//! Reading a task's inputs through its source: closure first, then descend.
//!
//! [`SourceReader::prefetch`] asks for a tree's whole closure in one
//! `ReadClosure` under the rest of the session's read budget. `Found` fills
//! the session's map in one round trip, its members the source's own shared
//! [`Blob`](aether_data::Blob)s, never copied. `TooLarge` reads that one tree
//! node with a `ReadArtifact` and pushes its subtrees onto an explicit work
//! stack, each prefetched in turn, so only the spine of oversized directories
//! and the blobs directly inside them are left out of the map.
//!
//! Those blobs are read as the archive reaches them, a batch at a time. The
//! reader remembers each directory listing it loads, its file digests in
//! entry order (the order the archive opens them). A blob the map misses
//! that a remembered listing holds is read with one `ReadArtifacts` naming it
//! and every later file in its listing the map does not hold, under
//! [`READ_MANY_BYTES`]; the answered prefix goes into a transient window that
//! each open takes from, never charged to the read budget and replaced by the
//! next batch, so it holds at most one batch. Any other lookup the map
//! misses is one `ReadArtifact`.
//!
//! Every member is verified against the digest it is read under before it is
//! trusted: a tree through [`ClosureArtifact::load`], a blob through a
//! [`StoredBlob`] that hashes as it streams and fails at its end on a
//! mismatch, so a corrupt blob never crosses into a container whole.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read};
use std::iter;

use aether_bloomery_kinds::{
    ArtifactDigests, ClosureArtifact, ClosureLimit, Digest, Node, OpaqueBytes, ReadArtifact, ReadArtifactResult,
    ReadArtifacts, ReadArtifactsResult, ReadClosure, ReadClosureResult, Ref, Tree, VerifiedRead,
};
use aether_bloomery_tar::{SourceBlob, TreeSource};
use aether_data::{Kind, Storage};

use super::{StorageAnswer, StorageCall, StorageError, StoragePort};

/// The bytes a stored blob holds beyond its payload: its kind prefix.
const PREFIX_BYTES: u64 = 8;

/// Largest stored length one batched read answers past its first blob: the
/// stage batch's byte bound.
pub const READ_MANY_BYTES: u64 = 64 << 20;

/// What a session has read, keyed by digest, and how much of its read budget
/// is left; the directory listings it has loaded; and the blobs the last
/// batched read answered that no open has taken yet.
pub struct Fetched {
    members: HashMap<Digest, ClosureArtifact>,
    remaining_bytes: u64,
    /// Each file digest's latest listing and its position there.
    listed: HashMap<Digest, (usize, usize)>,
    /// Each loaded listing's file digests, in entry order.
    listings: Vec<Vec<Digest>>,
    /// The last batch's members not yet opened; not charged to the budget.
    window: HashMap<Digest, ClosureArtifact>,
}

impl Fetched {
    pub fn new(budget: ClosureLimit) -> Self {
        Self {
            members: HashMap::new(),
            remaining_bytes: budget.get(),
            listed: HashMap::new(),
            listings: Vec::new(),
            window: HashMap::new(),
        }
    }

    /// Remember `tree`'s file digests in entry order, the order an archive
    /// opens them.
    fn list(&mut self, tree: &Tree) {
        let index = self.listings.len();
        let files = tree
            .entries()
            .values()
            .filter_map(|node| match node {
                Node::File(blob) | Node::Executable(blob) => Some(blob.digest()),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (position, digest) in files.iter().enumerate() {
            self.listed.insert(*digest, (index, position));
        }
        self.listings.push(files);
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
    /// map misses is taken from the window, read in a batch with the later
    /// blobs of its listing, or read alone, and never kept.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] when the source lacks it, or why it did not
    /// read; [`StorageError::OtherKind`] when it is not stored as bytes.
    pub fn open(&mut self, blob: &Ref<OpaqueBytes>) -> Result<StoredBlob, StorageError> {
        let digest = blob.digest();
        let member = if let Some(member) = self.fetched.window.remove(&digest) {
            member
        } else if self.fetched.members.contains_key(&digest) {
            self.member(digest, false)?
        } else if let Some(&at) = self.fetched.listed.get(&digest) {
            self.read_batch(digest, at)?
        } else {
            self.member(digest, false)?
        };
        if member.kind() != OpaqueBytes::ID {
            return Err(StorageError::OtherKind(digest));
        }
        Ok(StoredBlob(member.verified_reader(digest)))
    }

    /// Read the blob `digest` names with one `ReadArtifacts` naming it and
    /// every later file of its listing, `at`, that the map does not hold,
    /// returning its member and replacing the window with the rest of the
    /// answered prefix.
    fn read_batch(
        &mut self,
        digest: Digest,
        (listing, position): (usize, usize),
    ) -> Result<ClosureArtifact, StorageError> {
        let fetched = &mut *self.fetched;
        fetched.window.clear();
        let mut named = HashSet::from([digest]);
        let later = fetched.listings[listing][position + 1..]
            .iter()
            .copied()
            .filter(|later| !fetched.members.contains_key(later) && named.insert(*later));
        let digests = iter::once(digest).chain(later).take(ReadArtifacts::MAX_ARTIFACTS).collect::<Vec<_>>();

        let request = ReadArtifacts {
            digests: ArtifactDigests::new(digests).map_err(|_| StorageError::Answer)?,
            limit_bytes: ClosureLimit::new(READ_MANY_BYTES).expect("64 MiB is within a closure limit's bounds"),
        };
        let StorageAnswer::ReadMany(result) = self.port.call(StorageCall::ReadMany(request))? else {
            return Err(StorageError::Answer);
        };
        let mut artifacts = match result {
            ReadArtifactsResult::Found { artifacts } => artifacts.into_iter(),
            ReadArtifactsResult::Missing { digest } => return Err(StorageError::Missing(digest)),
            ReadArtifactsResult::Err { message } => return Err(StorageError::Refused(message)),
        };
        let first =
            artifacts.next().filter(|first| first.claimed().unverified() == digest).ok_or(StorageError::Answer)?;
        self.fetched.window.extend(artifacts.map(|member| (member.claimed().unverified(), member)));
        Ok(first)
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
        let tree = self.load(tree)?;
        self.fetched.list(&tree);
        Ok(tree)
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
