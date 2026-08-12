//! Git object semantics at the backend-correspondence adapter edge.
//!
//! A bloomery [`Digest`](aether_bloomery::Digest) is a pure content-address — a sha256 over a bloom
//! *value*'s aether-wire bytes — and is **never** the sha of any git object.
//! Real GitHub repositories are sha1 (20-byte / 40-hex) object format, so the
//! source port cannot hand a digest to git as an object sha. The domain-owned
//! [`aether_bloomery::Correspondence`] stores backend object bytes opaquely;
//! this module owns the checked conversion between those bytes and the
//! **format-tagged** [`GitObjectId`] consumed by Git APIs.
//!
//! Every consumer holds the domain correspondence directly; the conversion below
//! is the only Git-specific step, applied where a Git API is actually called.
//!
//! [#3590]: https://github.com/iamacoffeepot/aether/issues/3590
//! [#3603]: https://github.com/iamacoffeepot/aether/issues/3603

use aether_bloomery::{BackendObjectId, CorrespondenceError};

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

impl From<GitObjectId> for BackendObjectId {
    fn from(object: GitObjectId) -> Self {
        Self::new(object.bytes)
    }
}

impl From<&GitObjectId> for BackendObjectId {
    fn from(object: &GitObjectId) -> Self {
        Self::new(object.bytes.clone())
    }
}

impl TryFrom<BackendObjectId> for GitObjectId {
    type Error = CorrespondenceError;

    fn try_from(object: BackendObjectId) -> Result<Self, Self::Error> {
        let format = match object.as_bytes().len() {
            20 => GitObjectFormat::Sha1,
            32 => GitObjectFormat::Sha256,
            length => {
                return Err(CorrespondenceError::new(format!(
                    "backend object id is {length} bytes; a git object id must be 20-byte SHA-1 or 32-byte SHA-256",
                )));
            }
        };
        Self::new(format, object.into_bytes())
            .ok_or_else(|| CorrespondenceError::new("backend object id does not match the inferred git format"))
    }
}

impl TryFrom<&BackendObjectId> for GitObjectId {
    type Error = CorrespondenceError;

    fn try_from(object: &BackendObjectId) -> Result<Self, Self::Error> {
        Self::try_from(object.clone())
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

#[cfg(test)]
mod tests {
    use aether_bloomery::BackendObjectId;

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

    #[test]
    fn backend_conversion_revalidates_sha1_and_sha256_lengths() {
        for (length, format) in [(20, GitObjectFormat::Sha1), (32, GitObjectFormat::Sha256)] {
            let backend = BackendObjectId::new(vec![0xAB; length]);
            let git = GitObjectId::try_from(backend.clone()).expect("a supported git object length converts");

            assert_eq!(git.format(), format);
            assert_eq!(BackendObjectId::from(git), backend);
        }

        for length in [0, 19, 21, 31, 33] {
            assert!(
                GitObjectId::try_from(BackendObjectId::new(vec![0; length])).is_err(),
                "an opaque {length}-byte id is not a Git object id",
            );
        }
    }
}
