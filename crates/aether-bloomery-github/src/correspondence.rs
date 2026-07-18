//! The persisted git-object↔bloom-digest correspondence (ADR-0150, amended
//! 2026-07-18 for [#3590]; this slice [#3603]).
//!
//! A bloomery [`Digest`] is a pure content-address — a sha256 over a bloom
//! *value*'s aether-wire bytes — and is **never** the sha of any git object.
//! Real GitHub repositories are sha1 (20-byte / 40-hex) object format, so the
//! source port cannot hand a digest to git as an object sha. Instead it persists
//! the mapping as *data*: given a real commit or tree object, which bloom value
//! that object carries, and back. The git side of each correspondence is
//! **format-tagged bytes** ([`GitObjectId`]) — `sha1`/20 today, `sha256`/32 if
//! GitHub ships SHA-256 repositories — so the schema survives the object-format
//! transition unchanged.
//!
//! This trait is the port-level seam the [`GitSource`](crate::GitSource) backend
//! resolves through, mirroring the [`GitDataApi`](crate::GitDataApi) /
//! [`ActionsApi`](crate::ActionsApi) seams: the durable implementation lives in
//! the host (a `SQLite`-backed table), and the in-process double
//! ([`FakeGithub`](crate::testing::FakeGithub)) implements it for token- and
//! network-free tests. The crate owns [`GitObjectId`] because it is the one
//! crate permitted to reason about git object shas — core `aether_bloomery` keeps
//! [`Digest`] a pure content-address with no git concept.
//!
//! [#3590]: https://github.com/iamacoffeepot/aether/issues/3590
//! [#3603]: https://github.com/iamacoffeepot/aether/issues/3603

use std::error::Error;
use std::fmt;

use aether_bloomery::Digest;

/// The object format a [`GitObjectId`]'s bytes are in — the format tag ADR-0150
/// requires so the correspondence schema survives a SHA-256 object-format
/// transition. `sha1` is today's GitHub; `sha256` is the future GitHub may ship.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum GitObjectFormat {
    /// A 20-byte sha1 object id (today's GitHub).
    Sha1,
    /// A 32-byte sha256 object id (a future SHA-256 repository).
    Sha256,
}

impl GitObjectFormat {
    /// The raw byte length an object id of this format carries.
    #[must_use]
    pub const fn byte_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }
}

/// A real git object id: its format tag plus the raw object-sha bytes. The
/// format tag ↔ byte-length invariant is enforced at construction — a `Sha1`
/// carries exactly 20 bytes, a `Sha256` exactly 32 — so a mis-tagged id can
/// never enter the correspondence.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct GitObjectId {
    format: GitObjectFormat,
    bytes: Vec<u8>,
}

impl GitObjectId {
    /// Build an object id from its `format` tag and raw `bytes`, or `None` when
    /// the byte length does not match the format (`sha1`/20, `sha256`/32) — the
    /// tripwire that keeps a mis-tagged id out of the store.
    #[must_use]
    pub fn new(format: GitObjectFormat, bytes: Vec<u8>) -> Option<Self> {
        (bytes.len() == format.byte_len()).then_some(Self { format, bytes })
    }

    /// Parse a git object sha rendered as lowercase (or uppercase) hex, inferring
    /// the format from its length: 40 hex → `sha1`, 64 hex → `sha256`. Any other
    /// length, or a non-hex character, is `None` (git never hands back such a
    /// string, so a `None` here is a malformed sha, not an expected miss).
    #[must_use]
    pub fn from_hex(sha: &str) -> Option<Self> {
        let format = match sha.len() {
            40 => GitObjectFormat::Sha1,
            64 => GitObjectFormat::Sha256,
            _ => return None,
        };
        let raw = sha.as_bytes();
        let mut bytes = vec![0u8; format.byte_len()];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = (hex_nibble(raw[i * 2])? << 4) | hex_nibble(raw[i * 2 + 1])?;
        }
        Some(Self { format, bytes })
    }

    /// Render the object id as the lowercase-hex git object sha — the form the
    /// Git Data / Actions / worktree surfaces take.
    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(self.bytes.len() * 2);
        for byte in &self.bytes {
            out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
        }
        out
    }

    /// The object format tag.
    #[must_use]
    pub const fn format(&self) -> GitObjectFormat {
        self.format
    }

    /// The raw object-sha bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

// One hex character to its 0..=15 nibble, or `None` for a non-hex byte.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A correspondence-store fault — a durable-storage read/write that failed
/// (transport, disk, or a decode of a stored row). Distinct from a clean absent
/// correspondence, which the resolve methods report as `Ok(None)` rather than an
/// error: a never-recorded object is an expected state (the boundary this slice
/// draws), not a fault.
#[derive(Debug)]
pub struct CorrespondenceError {
    message: String,
}

impl CorrespondenceError {
    /// Wrap a storage fault description.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for CorrespondenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "git correspondence store: {}", self.message)
    }
}

impl Error for CorrespondenceError {}

/// The persisted git-object↔bloom-digest correspondence the source port resolves
/// real git shas through (ADR-0150). One record maps one bloom [`Digest`] to the
/// real git object ([`GitObjectId`]) that carries its value, resolvable in both
/// directions. The durable implementation lives in the host; the in-process
/// [`FakeGithub`](crate::testing::FakeGithub) implements it for tests.
///
/// `&self` methods with interior mutability so the source backend — which holds
/// the correspondence behind a shared handle and calls it from `SourceBackend`'s
/// `&self` methods — can record and resolve without an exclusive borrow.
pub trait Correspondence {
    /// Record that git object `git` carries bloom value `digest` (both
    /// directions). Last-writer-wins on the digest key, so re-recording the same
    /// digest is idempotent and a rebuild that re-inserts is a no-op.
    ///
    /// # Errors
    /// The durable store could not be written.
    fn record(&self, digest: &Digest, git: &GitObjectId) -> Result<(), CorrespondenceError>;

    /// The real git object recorded for `digest` (the forward direction), or
    /// `None` when no correspondence was ever recorded.
    ///
    /// # Errors
    /// The durable store could not be read.
    fn resolve_git(&self, digest: &Digest) -> Result<Option<GitObjectId>, CorrespondenceError>;

    /// The bloom digest recorded for git object `git` (the reverse direction), or
    /// `None` when no correspondence was ever recorded.
    ///
    /// # Errors
    /// The durable store could not be read.
    fn resolve_digest(&self, git: &GitObjectId) -> Result<Option<Digest>, CorrespondenceError>;
}

#[cfg(test)]
mod tests {
    use super::{GitObjectFormat, GitObjectId};

    #[test]
    fn git_object_id_enforces_the_format_to_byte_length_invariant() {
        // Tripwire: the format tag ↔ byte length invariant (`sha1` is 20 bytes,
        // `sha256` is 32) — a mis-tagged id must never construct, or a sha1 sha
        // would be mistaken for a sha256 one across the object-format transition.
        assert!(GitObjectId::new(GitObjectFormat::Sha1, vec![0u8; 20]).is_some(), "sha1 accepts 20 bytes");
        assert!(GitObjectId::new(GitObjectFormat::Sha256, vec![0u8; 32]).is_some(), "sha256 accepts 32 bytes");
        assert!(GitObjectId::new(GitObjectFormat::Sha1, vec![0u8; 32]).is_none(), "sha1 rejects 32 bytes");
        assert!(GitObjectId::new(GitObjectFormat::Sha256, vec![0u8; 20]).is_none(), "sha256 rejects 20 bytes");
        assert!(GitObjectId::new(GitObjectFormat::Sha1, vec![0u8; 19]).is_none(), "sha1 rejects a short id");
    }

    #[test]
    fn from_hex_infers_the_format_from_length_and_round_trips() {
        // Tripwire: a 40-hex sha is sha1/20, a 64-hex sha is sha256/32, and the
        // hex render is the inverse — the real-repo sha1 case (40-hex) the
        // reported failure could not parse under the old fixed-64-hex gate.
        let sha1 = "a".repeat(40);
        let id = GitObjectId::from_hex(&sha1).expect("40-hex parses");
        assert_eq!(id.format(), GitObjectFormat::Sha1);
        assert_eq!(id.bytes().len(), 20);
        assert_eq!(id.to_hex(), sha1);

        let sha256 = "b".repeat(64);
        let id = GitObjectId::from_hex(&sha256).expect("64-hex parses");
        assert_eq!(id.format(), GitObjectFormat::Sha256);
        assert_eq!(id.bytes().len(), 32);
        assert_eq!(id.to_hex(), sha256);

        assert!(GitObjectId::from_hex(&"c".repeat(39)).is_none(), "a 39-hex sha is neither format");
        assert!(GitObjectId::from_hex(&"z".repeat(40)).is_none(), "a non-hex char is rejected");
    }
}
