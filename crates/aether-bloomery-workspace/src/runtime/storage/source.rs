//! Reading a task's inputs through its source: windows read ahead of the
//! archive, released as it writes them.
//!
//! A session's reads share one budget, `AETHER_WORKSPACE_PREFETCH_BYTES`,
//! which bounds the bytes the session holds (members in its map and the
//! blobs of the last batched read) plus the bytes it has asked for (the
//! `limit_bytes` of each closure read in flight). A closure read reserves
//! its limit when it is sent; its answer is charged to what the session
//! holds and the reservation is refunded.
//!
//! Each directory listing [`TreeSource::tree`] hands the archive queues the
//! child directories the map lacks at the front of the read-ahead queue, in
//! listing order, so the queue is in the order the archive reaches them.
//! The queue is pumped: a `ReadClosure` for its front, asking for at most
//! [`READ_AHEAD_BYTES`], is sent without waiting while the budget left
//! covers a whole window (half the budget, when that is less), so the source
//! reads the next windows while the worker encodes and uploads this one. `tree` waits only for the answer it
//! needs. A closure over its window (`TooLarge`) reads that one tree node
//! with a `ReadArtifact`, and its subtrees are queued when the archive lists
//! it, so only the spine of oversized directories and the blobs directly
//! inside them are left out of the map. A directory the archive reaches
//! before it was sent waits for the reads in flight to free their
//! reservations; with none in flight it is read with whatever budget is
//! left, or node alone, so a tiny budget never waits on bytes that will not
//! free.
//!
//! [`TreeSource::blob`] takes the blob it hands the archive out of the map
//! and refunds its bytes, so the window slides; [`TreeSource::tree`] does
//! the same with the node it decodes. A closure answers its members as views
//! of one shared allocation, so a node kept after its listing was decoded
//! would hold its whole window resident. [`SourceReader::open`] and
//! [`SourceReader::load`], for a caller that is not writing an archive, take
//! nothing out of the map and keep nothing they read into it.
//!
//! The blobs directly inside an oversized directory are read as the archive
//! reaches them, a batch at a time. The reader remembers each directory
//! listing it loads, its file digests in entry order (the order the archive
//! opens them). A blob the map misses that a remembered listing holds is
//! read with one `ReadArtifacts` naming it and every later file in its
//! listing the map does not hold, under [`READ_MANY_BYTES`]; the answered
//! prefix goes into a window each open takes from, charged to the budget
//! and replaced by the next batch, so it holds at most one batch. Any other
//! lookup the map misses is one `ReadArtifact`.
//!
//! Every member is verified against the digest it is read under before it is
//! trusted: a tree through [`ClosureArtifact::load`], a blob through a
//! [`StoredBlob`] that hashes as it streams and fails at its end on a
//! mismatch, so a corrupt blob never crosses into a container whole.

use std::collections::{HashMap, HashSet, VecDeque, hash_map};
use std::io::{self, Read};
use std::iter;

use aether_bloomery_kinds::{
    ArtifactDigests, ClosureArtifact, ClosureLimit, Node, ReadArtifact, ReadArtifactResult, ReadArtifacts,
    ReadArtifactsResult, ReadClosure, ReadClosureResult, Tree, VerifiedRead,
};
use aether_bloomery_tar::{SourceBlob, TreeSource};
use aether_data::{Digest, Kind, OpaqueBytes, Ref, Storage};

use super::{StorageAnswer, StorageCall, StorageError, StoragePort};

/// The bytes a stored blob holds beyond its payload: its kind prefix.
const PREFIX_BYTES: u64 = 8;

/// Largest stored length one batched read answers past its first blob: the
/// stage batch's byte bound.
pub const READ_MANY_BYTES: u64 = 64 << 20;

/// Largest closure one read-ahead request asks for: the stage batch's byte
/// bound, so even a tree that fits the whole budget is read as a sequence of
/// windows the archive can overlap with.
pub const READ_AHEAD_BYTES: u64 = 64 << 20;

/// What a session has read, keyed by digest, and what its budget holds and
/// has reserved; the directory listings it has loaded; the blobs the last
/// batched read answered that no open has taken yet; and the directories it
/// reads ahead of the archive.
pub struct Fetched {
    members: HashMap<Digest, ClosureArtifact>,
    /// The most bytes the session holds or has asked for at once.
    budget: u64,
    /// The stored length of every member and window blob held.
    held_bytes: u64,
    /// The limit of every closure read in flight.
    reserved_bytes: u64,
    /// Each file digest's latest listing and its position there.
    listed: HashMap<Digest, (usize, usize)>,
    /// Each loaded listing's file digests, in entry order.
    listings: Vec<Vec<Digest>>,
    /// The last batch's members not yet opened.
    window: HashMap<Digest, ClosureArtifact>,
    /// Directories the archive will reach, in the order it reaches them, not
    /// yet asked for.
    ahead: VecDeque<Digest>,
    /// Reads sent ahead of the archive and not yet applied, oldest first.
    pending: Vec<Pending>,
}

/// One read sent ahead of the archive: the directory it is for, the sequence
/// its answer comes back under, and what was asked.
struct Pending {
    directory: Digest,
    seq: u64,
    read: Ahead,
}

enum Ahead {
    /// The directory's closure, under the limit it reserved.
    Closure(ClosureLimit),
    /// The directory's node alone, after its closure was over its limit.
    Node,
}

/// A member's stored length: its payload plus its kind prefix.
fn stored_bytes(member: &ClosureArtifact) -> u64 {
    member.len().saturating_add(PREFIX_BYTES)
}

impl Fetched {
    pub fn new(budget: ClosureLimit) -> Self {
        Self {
            members: HashMap::new(),
            budget: budget.get(),
            held_bytes: 0,
            reserved_bytes: 0,
            listed: HashMap::new(),
            listings: Vec::new(),
            window: HashMap::new(),
            ahead: VecDeque::new(),
            pending: Vec::new(),
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

    /// Keep `member` under `digest`, charging its stored length to what the
    /// session holds. A member already held is not charged twice.
    fn keep(&mut self, digest: Digest, member: ClosureArtifact) {
        if let hash_map::Entry::Vacant(slot) = self.members.entry(digest) {
            self.held_bytes = self.held_bytes.saturating_add(stored_bytes(&member));
            slot.insert(member);
        }
    }

    /// Take the blob under `digest` out of the window, refunding its stored
    /// length.
    fn take_window(&mut self, digest: Digest) -> Option<ClosureArtifact> {
        let member = self.window.remove(&digest)?;
        self.held_bytes = self.held_bytes.saturating_sub(stored_bytes(&member));
        Some(member)
    }

    /// Take the member under `digest` out of the window or the map,
    /// refunding its stored length.
    fn release(&mut self, digest: Digest) -> Option<ClosureArtifact> {
        if let Some(member) = self.take_window(digest) {
            return Some(member);
        }
        let member = self.members.remove(&digest)?;
        self.held_bytes = self.held_bytes.saturating_sub(stored_bytes(&member));
        Some(member)
    }

    /// Replace the window with `members`, refunding the old one and charging
    /// the new.
    fn replace_window(&mut self, members: impl Iterator<Item = ClosureArtifact>) {
        let refund = self.window.drain().map(|(_, member)| stored_bytes(&member)).sum::<u64>();
        self.held_bytes = self.held_bytes.saturating_sub(refund);
        for member in members {
            self.held_bytes = self.held_bytes.saturating_add(stored_bytes(&member));
            self.window.insert(member.claimed().unverified(), member);
        }
    }

    /// The limit one closure read asks for at most: [`READ_AHEAD_BYTES`], or
    /// the whole budget when that is smaller.
    fn whole_window(&self) -> u64 {
        READ_AHEAD_BYTES.min(ClosureLimit::MAX_BYTES).min(self.budget)
    }

    /// The least budget left a read ahead of the archive is sent under: a
    /// whole window, or half the budget when that is smaller, so a budget of
    /// a few windows never splits one and a small budget still reads ahead.
    fn ahead_floor(&self) -> u64 {
        self.whole_window().min(self.budget / 2).max(ClosureLimit::MIN_BYTES)
    }

    /// Reserve a closure limit of the whole window or the free budget,
    /// whichever is less, or `None` when that is below `at_least`.
    fn reserve(&mut self, at_least: u64) -> Option<ClosureLimit> {
        let free = self.budget.saturating_sub(self.held_bytes.saturating_add(self.reserved_bytes));
        let bytes = self.whole_window().min(free);
        if bytes < at_least {
            return None;
        }
        let limit = ClosureLimit::new(bytes).ok()?;
        self.reserved_bytes += bytes;
        Some(limit)
    }

    /// Refund the reservation of a closure read that has answered.
    fn settle(&mut self, limit: ClosureLimit) {
        self.reserved_bytes = self.reserved_bytes.saturating_sub(limit.get());
    }

    /// Queue `tree`'s child directories the map lacks at the front of the
    /// read-ahead queue, in listing order.
    fn queue_children(&mut self, tree: &Tree) {
        let children = tree
            .entries()
            .values()
            .rev()
            .filter_map(|node| match node {
                Node::Directory(child) => Some(child.digest()),
                _ => None,
            })
            .filter(|child| !self.members.contains_key(child))
            .collect::<Vec<_>>();
        for child in children {
            self.ahead.push_front(child);
        }
    }

    /// Whether a read for `directory` is in flight.
    fn is_pending(&self, directory: Digest) -> bool {
        self.pending.iter().any(|pending| pending.directory == directory)
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

    /// Make sure the tree node `directory` names is in the map: from the
    /// read in flight for it, or read now.
    fn arrive(&mut self, directory: Digest) -> Result<(), StorageError> {
        loop {
            if self.fetched.members.contains_key(&directory) {
                return Ok(());
            }
            if let Some(index) = self.fetched.pending.iter().position(|pending| pending.directory == directory) {
                self.wait_on(index)?;
                continue;
            }
            self.fetched.ahead.retain(|queued| *queued != directory);
            self.apply_ready()?;
            if let Some(limit) = self.fetched.reserve(self.fetched.ahead_floor()) {
                self.send_closure(directory, limit)?;
            } else if !self.fetched.pending.is_empty() {
                // The reads in flight hold the budget; each answer frees its
                // reservation's unused part.
                self.wait_on(0)?;
            } else if let Some(limit) = self.fetched.reserve(ClosureLimit::MIN_BYTES) {
                self.send_closure(directory, limit)?;
            } else {
                return self.member(directory, true).map(drop);
            }
        }
    }

    /// Send a `ReadClosure` for every queued directory, front first, while
    /// the budget left covers a whole window, after applying every answer
    /// that has arrived.
    fn pump(&mut self) -> Result<(), StorageError> {
        self.apply_ready()?;
        while let Some(&directory) = self.fetched.ahead.front() {
            if self.fetched.members.contains_key(&directory) || self.fetched.is_pending(directory) {
                self.fetched.ahead.pop_front();
                continue;
            }
            let Some(limit) = self.fetched.reserve(self.fetched.ahead_floor()) else {
                break;
            };
            self.fetched.ahead.pop_front();
            self.send_closure(directory, limit)?;
        }
        Ok(())
    }

    fn send_closure(&mut self, directory: Digest, limit: ClosureLimit) -> Result<(), StorageError> {
        let seq = self.port.send(StorageCall::ReadClosure(ReadClosure { root: directory, limit_bytes: limit }))?;
        self.fetched.pending.push(Pending { directory, seq, read: Ahead::Closure(limit) });
        Ok(())
    }

    /// Apply every read in flight whose answer has arrived.
    fn apply_ready(&mut self) -> Result<(), StorageError> {
        let mut index = 0;
        while let Some(pending) = self.fetched.pending.get(index) {
            match self.port.poll(pending.seq) {
                Some(answer) => {
                    let pending = self.fetched.pending.remove(index);
                    self.apply(pending, answer)?;
                }
                None => index += 1,
            }
        }
        Ok(())
    }

    /// Wait for the read in flight at `index` and apply its answer.
    fn wait_on(&mut self, index: usize) -> Result<(), StorageError> {
        let pending = self.fetched.pending.remove(index);
        let answer = self.port.wait(pending.seq)?;
        self.apply(pending, answer)
    }

    /// Apply one read-ahead answer: a closure's members are kept and its
    /// reservation refunded, and one over its limit sends a read of its node
    /// alone.
    fn apply(&mut self, pending: Pending, answer: StorageAnswer) -> Result<(), StorageError> {
        match (pending.read, answer) {
            (Ahead::Closure(limit), StorageAnswer::ReadClosure(result)) => {
                self.fetched.settle(limit);
                match result {
                    ReadClosureResult::Found { artifacts, .. } => {
                        for member in artifacts {
                            self.fetched.keep(member.claimed().unverified(), member);
                        }
                        Ok(())
                    }
                    ReadClosureResult::TooLarge { .. } => {
                        let directory = pending.directory;
                        let seq = self.port.send(StorageCall::Read(ReadArtifact { digest: directory }))?;
                        self.fetched.pending.push(Pending { directory, seq, read: Ahead::Node });
                        Ok(())
                    }
                    ReadClosureResult::Missing { digest, .. } => Err(StorageError::Missing(digest)),
                    ReadClosureResult::Err { message, .. } => Err(StorageError::Refused(message)),
                }
            }
            (Ahead::Node, StorageAnswer::Read(result)) => match result {
                ReadArtifactResult::Found { artifact } => {
                    self.fetched.keep(pending.directory, artifact);
                    Ok(())
                }
                ReadArtifactResult::Missing { digest } => Err(StorageError::Missing(digest)),
                ReadArtifactResult::Err { message, .. } => Err(StorageError::Refused(message)),
            },
            _ => Err(StorageError::Answer),
        }
    }

    /// Load and decode the artifact `artifact` names, verified against its
    /// digest: from the map, or read with one `ReadArtifact` and not kept, so
    /// a root resolution checks is still read with its closure when the
    /// archive reaches it.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] when the source lacks it, or why it did not
    /// read, verify, or decode.
    pub fn load<K: Storage>(&mut self, artifact: &Ref<K>) -> Result<K, StorageError> {
        let digest = artifact.digest();
        let member = self.member(digest, false)?;
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

    /// Open the blob `blob` names as a reader verified at its end, leaving a
    /// member the map holds there. A blob the map misses is taken from the
    /// window, read in a batch with the later blobs of its listing, or read
    /// alone, and never kept.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] when the source lacks it, or why it did not
    /// read; [`StorageError::OtherKind`] when it is not stored as bytes.
    pub fn open(&mut self, blob: &Ref<OpaqueBytes>) -> Result<StoredBlob, StorageError> {
        let digest = blob.digest();
        let member = match self.fetched.take_window(digest) {
            Some(member) => member,
            None if self.fetched.members.contains_key(&digest) => self.member(digest, false)?,
            None => self.miss(digest)?,
        };
        verified(digest, &member)
    }

    /// Open the blob `blob` names for the archive: taken out of the window or
    /// the map, its bytes refunded to the budget, then the read-ahead pumped
    /// into what that freed.
    fn take(&mut self, blob: &Ref<OpaqueBytes>) -> Result<StoredBlob, StorageError> {
        let digest = blob.digest();
        let member = match self.fetched.release(digest) {
            Some(member) => member,
            None => self.miss(digest)?,
        };
        self.pump()?;
        verified(digest, &member)
    }

    /// Read a blob neither the map nor the window holds: in a batch when a
    /// remembered listing holds it, otherwise alone.
    fn miss(&mut self, digest: Digest) -> Result<ClosureArtifact, StorageError> {
        match self.fetched.listed.get(&digest) {
            Some(&at) => self.read_batch(digest, at),
            None => self.member(digest, false),
        }
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
        fetched.replace_window(iter::empty());
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
        self.fetched.replace_window(artifacts);
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

    fn tree(&mut self, directory: &Ref<Tree>) -> Result<Tree, StorageError> {
        self.arrive(directory.digest())?;
        let tree = self.load(directory)?;
        self.fetched.release(directory.digest());
        self.fetched.list(&tree);
        self.fetched.queue_children(&tree);
        self.pump()?;
        Ok(tree)
    }

    fn blob(&mut self, blob: &Ref<OpaqueBytes>) -> Result<SourceBlob<StoredBlob>, StorageError> {
        self.take(blob).map(|reader| SourceBlob { len: reader.len(), reader })
    }
}

/// `member` as a blob reader verified against `digest` at its end.
fn verified(digest: Digest, member: &ClosureArtifact) -> Result<StoredBlob, StorageError> {
    if member.kind() != OpaqueBytes::ID {
        return Err(StorageError::OtherKind(digest));
    }
    Ok(StoredBlob(member.verified_reader(digest)))
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
