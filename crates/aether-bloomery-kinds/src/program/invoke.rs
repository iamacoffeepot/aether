//! Mail a native driver exchanges with a program bundle.

use alloc::vec;
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::fmt;

use aether_actor::HeldReply;
use aether_data::{Blob, BlobReader, KindId};

use crate::artifact::{ArtifactHasher, Sink, blob_digest, stream};
use crate::program::executor::ExecutorFault;
use crate::program::fault::Detail;
use crate::program::name::ProgramName;
use crate::program::refusal::Refusal;
use crate::{Digest, EncodedArtifact};

/// The digest a sender claims for the bytes beside it. Nothing checked it on
/// decode, and it cannot be read as a proven [`Digest`].
///
/// Only [`ClosureArtifact::new`], which computes it, and decode build one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, aether_data::Schema)]
pub struct ClaimedDigest(Digest);

impl ClaimedDigest {
    /// The claimed value, for lookup and correlation only. A match proves
    /// nothing about the bytes; [`ClosureArtifact::load`] does.
    #[must_use]
    pub const fn unverified(self) -> Digest {
        self.0
    }
}

/// One artifact carried over mail: its kind, its payload, and the digest its
/// sender claims.
///
/// The payload is a [`Blob`], so carrying a member copies nothing. There is
/// no accessor for it: [`Self::load`] is the one reader, and it verifies.
#[derive(Debug, Clone, aether_data::Schema)]
pub struct ClosureArtifact {
    claimed: ClaimedDigest,
    kind: KindId,
    bytes: Blob,
}

impl ClosureArtifact {
    /// Carry `bytes` under `kind`. The claim is computed here, by streaming
    /// the bytes once through [`BlobReader`] and [`ArtifactHasher`], so a
    /// constructed member's claim is true.
    #[must_use]
    pub fn new(kind: KindId, bytes: impl Into<Blob>) -> Self {
        let bytes = bytes.into();
        Self { claimed: ClaimedDigest(blob_digest(kind, &bytes)), kind, bytes }
    }

    /// The digest the sender claims. It is unverified until [`Self::load`].
    #[must_use]
    pub const fn claimed(&self) -> ClaimedDigest {
        self.claimed
    }

    /// Stored artifact kind: the blob's prefix, which the digest covers.
    #[must_use]
    pub const fn kind(&self) -> KindId {
        self.kind
    }

    /// The payload length in bytes, without the kind prefix. It reads no bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        BlobReader::open(&self.bytes).len()
    }

    /// Whether the payload is empty. It reads no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every payload byte, streamed through [`BlobReader`] into
    /// [`ArtifactHasher`] seeded with the kind prefix, and returned only when
    /// the whole payload hashes to `expected`.
    ///
    /// Every byte is hashed before any is returned, so a returned `Vec` is
    /// the whole payload. There is no partial read.
    ///
    /// # Errors
    ///
    /// [`DigestMismatch`] when the kind and payload do not hash to
    /// `expected`, or when the payload cannot be read whole.
    pub fn load(&self, expected: Digest) -> Result<Vec<u8>, DigestMismatch> {
        let mismatch = DigestMismatch { expected };
        let len = usize::try_from(self.len()).map_err(|_| mismatch)?;
        let mut whole = vec![0; len];
        let streamed = stream(self.kind, &self.bytes, Sink::Whole(&mut whole));
        if streamed.read == len && streamed.digest == expected {
            Ok(whole)
        } else {
            Err(mismatch)
        }
    }
}

impl ClosureArtifact {
    /// A reader over the payload that hashes each window as it hands it out
    /// and, at the end of the payload, fails unless every byte, under the
    /// kind prefix, hashed to `expected`.
    ///
    /// It streams: nothing past one caller buffer is held, so a large member
    /// crosses into a writer a window at a time. The windows before the end
    /// are unverified until the end is reached, so a caller that must not act
    /// on bad bytes reads to the end before trusting any of them.
    #[must_use]
    pub fn verified_reader(&self, expected: Digest) -> VerifiedRead {
        VerifiedRead {
            expected,
            bytes: self.bytes.clone(),
            offset: 0,
            hasher: Some(ArtifactHasher::new(self.kind)),
            mismatched: false,
        }
    }
}

/// One member's payload read a window at a time and verified at its end, from
/// [`ClosureArtifact::verified_reader`].
pub struct VerifiedRead {
    expected: Digest,
    bytes: Blob,
    offset: u64,
    /// Taken when the end is reached and the digest compared.
    hasher: Option<ArtifactHasher>,
    mismatched: bool,
}

impl VerifiedRead {
    /// The payload length in bytes. It reads no bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        BlobReader::open(&self.bytes).len()
    }

    /// Whether the payload is empty. It reads no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the next window of the payload into `buf` and return how many
    /// bytes it holds; `0` once the whole payload has been read and verified,
    /// or when `buf` is empty.
    ///
    /// # Errors
    ///
    /// [`DigestMismatch`] at the end of the payload, and on every read after
    /// it, when the kind and payload do not hash to the expected digest.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, DigestMismatch> {
        let mismatch = DigestMismatch { expected: self.expected };
        if self.mismatched {
            return Err(mismatch);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let copied = BlobReader::open(&self.bytes).read_range(self.offset, buf);
        if copied > 0 {
            if let Some(hasher) = &mut self.hasher {
                hasher.update(&buf[..copied]);
            }
            self.offset += copied as u64;
            return Ok(copied);
        }
        let verified =
            self.hasher.take().is_none_or(|hasher| hasher.finish() == self.expected && self.offset == self.len());
        if verified {
            Ok(0)
        } else {
            self.mismatched = true;
            Err(mismatch)
        }
    }
}

/// Bytes read under a digest they do not hash to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigestMismatch {
    expected: Digest,
}

impl DigestMismatch {
    /// The digest the bytes were read under.
    #[must_use]
    pub const fn expected(&self) -> Digest {
        self.expected
    }
}

impl fmt::Display for DigestMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "artifact bytes do not hash to {}", self.expected)
    }
}

impl StdError for DigestMismatch {}

/// Ask a bundle to run one named program over an injected closure.
#[aether_data::kind(name = "aether.bloomery.program.invoke", no_serde)]
pub struct Invoke {
    seq: u64,
    program: ProgramName,
    input: Digest,
    closure: Vec<ClosureArtifact>,
}

impl Invoke {
    /// Address program `program` at `input`, injecting `closure`.
    ///
    /// `seq` is the driver's `Requested` sequence.
    #[must_use]
    pub fn new(seq: u64, program: ProgramName, input: Digest, closure: Vec<ClosureArtifact>) -> Self {
        Self { seq, program, input, closure }
    }

    /// Driver's `Requested` sequence for this invocation.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// Program name unique within the bundle.
    #[must_use]
    pub const fn program(&self) -> &ProgramName {
        &self.program
    }

    /// Digest of the input artifact, which must appear in [`Self::closure`].
    #[must_use]
    pub const fn input(&self) -> Digest {
        self.input
    }

    /// Transitive closure the guest may read.
    #[must_use]
    pub fn closure(&self) -> &[ClosureArtifact] {
        &self.closure
    }

    /// Take the sequence, program, input, and closure.
    #[must_use]
    pub fn into_parts(self) -> (u64, ProgramName, Digest, Vec<ClosureArtifact>) {
        (self.seq, self.program, self.input, self.closure)
    }
}

/// Reply to one [`Invoke`]. Written by the bundle; the driver records the journal event.
#[aether_data::kind(name = "aether.bloomery.program.invoked", eq, no_serde)]
pub enum Invoked {
    /// The program ran and staged these artifacts, rooted at `result`.
    Completed { seq: u64, result: Digest, staged: Vec<EncodedArtifact> },
    /// The program returned a [`Refusal`].
    Refused { seq: u64, refusal: Refusal },
    /// The bundle refused the request as protocol, not as a program outcome.
    Rejected { seq: u64, reason: Detail },
    /// An executor the program called ended the invocation; nothing it staged is recorded.
    Faulted { seq: u64, fault: ExecutorFault },
    /// The bundle closed before the invocation answered (ADR-0243 §1). It
    /// carries no seq: the host sends it in the bundle's place.
    Closed,
}

impl Invoked {
    /// The driver's `Requested` sequence this reply answers; `None` for
    /// [`Self::Closed`], which answers the request its reply handle names.
    #[must_use]
    pub const fn seq(&self) -> Option<u64> {
        match self {
            Self::Completed { seq, .. }
            | Self::Refused { seq, .. }
            | Self::Rejected { seq, .. }
            | Self::Faulted { seq, .. } => Some(*seq),
            Self::Closed => None,
        }
    }
}

impl HeldReply for Invoked {
    fn unanswered() -> Self {
        Self::Closed
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use aether_data::wire::{decode_from_slice, encode_to_vec};
    use aether_data::{Kind, MAX_READ_BYTES};

    use super::{ClosureArtifact, DigestMismatch, VerifiedRead};
    use crate::{Digest, Utf8Text, artifact_digest};

    /// `artifact` decoded again after one byte of its claimed digest was flipped: the member a
    /// sender that lies about its digest carries.
    fn with_altered_claim(artifact: &ClosureArtifact) -> ClosureArtifact {
        let mut bytes = encode_to_vec(artifact).expect("encode member");
        bytes[0] ^= 0x01;
        let altered: ClosureArtifact = decode_from_slice(&bytes).expect("decode member");
        assert_ne!(altered.claimed(), artifact.claimed(), "the flipped byte is the claim's");
        altered
    }

    #[test]
    fn a_claim_covers_the_kind_prefix_and_every_read_window() {
        // Catches a hash that stops after the first read window, or one that leaves out the kind
        // prefix; and a `load` that returns only the first window.
        let mut payload = vec![b'a'; MAX_READ_BYTES + 1];
        payload[MAX_READ_BYTES] = b'z';
        let artifact = ClosureArtifact::new(Utf8Text::ID, payload.clone());
        let expected = artifact_digest(Utf8Text::ID, &payload);

        assert_eq!(artifact.claimed().unverified(), expected);
        assert_eq!(artifact.len(), (MAX_READ_BYTES + 1) as u64);
        assert_eq!(artifact.load(expected), Ok(payload));
    }

    #[test]
    fn load_refuses_an_altered_claim_and_a_digest_the_bytes_do_not_hash_to() {
        // Catches a `load` that trusts the decoded claim, or one that checks the bytes against
        // the claim instead of `expected`.
        let artifact = ClosureArtifact::new(Utf8Text::ID, b"payload".to_vec());
        let altered = with_altered_claim(&artifact);
        let claimed = altered.claimed().unverified();
        assert_eq!(altered.load(claimed), Err(DigestMismatch { expected: claimed }));

        let foreign = Digest::from_bytes([7; 32]);
        assert_eq!(artifact.load(foreign), Err(DigestMismatch { expected: foreign }));
        assert_eq!(artifact.load(artifact.claimed().unverified()), Ok(b"payload".to_vec()));
    }

    /// Every window `reader` hands out, then the result of the read past the end.
    fn read_all(reader: &mut VerifiedRead) -> (Vec<u8>, Result<usize, DigestMismatch>) {
        let mut buffer = vec![0; MAX_READ_BYTES];
        let mut read = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return (read, Ok(0)),
                Ok(copied) => read.extend_from_slice(&buffer[..copied]),
                Err(mismatch) => return (read, Err(mismatch)),
            }
        }
    }

    #[test]
    fn a_verified_reader_hands_out_every_window_and_fails_only_at_the_end_on_a_mismatch() {
        // Catches a reader that verifies only its first window, returns `Ok(0)` at the end of bytes that do
        // not hash to the expected digest, or stops handing out windows before the end.
        let mut payload = vec![b'a'; MAX_READ_BYTES + 1];
        let artifact = ClosureArtifact::new(Utf8Text::ID, payload.clone());
        let matching = artifact_digest(Utf8Text::ID, &payload);
        payload[MAX_READ_BYTES] = b'z';
        let altered_last = artifact_digest(Utf8Text::ID, &payload);

        let (read, end) = read_all(&mut artifact.verified_reader(altered_last));
        assert_eq!(read.len(), MAX_READ_BYTES + 1, "every window before the end");
        assert_eq!(end, Err(DigestMismatch { expected: altered_last }));

        let mut reader = artifact.verified_reader(matching);
        let (read, end) = read_all(&mut reader);
        assert_eq!((read.len(), end), (MAX_READ_BYTES + 1, Ok(0)));
        assert_eq!(reader.read(&mut [0; 8]), Ok(0), "a read past a verified end stays at the end");
    }
}
