//! Tagged opaque ids (ADR-0064). Every `u64` id (mailbox, kind,
//! reply-handle) carries a 4-bit type tag in its high bits + a 60-bit
//! hash in its low bits. The string form (`mbx-q3lr-bv2x-mtdr` etc.)
//! is the wire encoding for the MCP boundary and human-facing
//! diagnostics; internal types stay raw `u64`.
//!
//! Bit layout: `[tag: 4][hash: 60]`. The tag identifies the id space
//! (mailbox / kind / handle); the hash is the low 60 bits of an
//! FNV-1a output (mailbox, kind) or a counter (handle). The byte-
//! domain prefixes from ADR-0029/0030 (`MAILBOX_DOMAIN`, `KIND_DOMAIN`)
//! still ride on the FNV input — the type ends up encoded twice (in
//! the tag bits *and* avalanched into the hash via the domain
//! prefix), and the two layers cross-check each other.
//!
//! `0x0` is intentionally invalid so a zero-initialised `u64` can
//! never be mistaken for a real id.

use alloc::string::String;
use core::fmt;

pub use aether_hub_protocol::tag_bits::{HASH_MASK, TAG_HANDLE, TAG_KIND, TAG_MAILBOX, TAG_SHIFT};

/// Type tag identifying an id space. Encoded in the high 4 bits of
/// every tagged id. `0x0` is reserved as an invalid sentinel — a
/// zero-initialised `u64` is never a valid tagged id, which catches
/// uninitialised state at the boundary instead of silently routing
/// to a hash collision.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Tag {
    /// Mailbox id (ADR-0029) — recipient of mail.
    Mailbox = TAG_MAILBOX,
    /// Kind id (ADR-0030) — schema-hashed payload identity.
    Kind = TAG_KIND,
    /// Handle id (ADR-0045) — entry in the substrate's handle store.
    Handle = TAG_HANDLE,
}

impl Tag {
    /// Three-letter wire prefix for the string form (`mbx`, `knd`,
    /// `hdl`). Concatenated with `-` and the base32 body to produce
    /// the full encoded id.
    pub const fn prefix(self) -> &'static str {
        match self {
            Tag::Mailbox => "mbx",
            Tag::Kind => "knd",
            Tag::Handle => "hdl",
        }
    }

    /// Decode a 4-bit tag value from the high nibble of a `u64`.
    /// Returns `None` for `0x0` (the reserved invalid sentinel) and
    /// for any reserved-future value (`0x4..=0xF`).
    pub const fn from_bits(bits: u8) -> Option<Tag> {
        match bits {
            TAG_MAILBOX => Some(Tag::Mailbox),
            TAG_KIND => Some(Tag::Kind),
            TAG_HANDLE => Some(Tag::Handle),
            _ => None,
        }
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.prefix())
    }
}

/// Stamp `tag` into the high 4 bits of `hash`'s low 60 bits, dropping
/// the hash's natural high 4 bits. Const-fold-friendly so the `Kind`
/// derive and `mailbox_id_from_name` can bake the tag at compile time.
pub const fn with_tag(tag: Tag, hash: u64) -> u64 {
    ((tag as u64) << TAG_SHIFT) | (hash & HASH_MASK)
}

/// Read the tag bits out of a tagged id. `None` on the `0x0`
/// sentinel or a reserved-future tag value.
pub const fn tag_of(id: u64) -> Option<Tag> {
    Tag::from_bits((id >> TAG_SHIFT) as u8 & 0x0F)
}

/// Return the 60-bit hash body with tag bits stripped.
pub const fn body_of(id: u64) -> u64 {
    id & HASH_MASK
}

/// Errors decoding a tagged-id string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// String didn't match the `<prefix>-XXXX-XXXX-XXXX` shape (wrong
    /// length, missing dashes, or unknown 3-letter prefix).
    Malformed,
    /// Body contained a character outside the lowercase base32
    /// alphabet (`a-z` + `2-7`).
    InvalidChar(char),
    /// Tag value the caller expected (e.g. they called
    /// `decode_mailbox`) didn't match the tag implied by the prefix.
    TagMismatch { expected: Tag, found: Tag },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Malformed => f.write_str("malformed tagged id"),
            DecodeError::InvalidChar(c) => write!(f, "invalid base32 char: {c:?}"),
            DecodeError::TagMismatch { expected, found } => {
                write!(f, "tag mismatch: expected {expected}, found {found}")
            }
        }
    }
}

/// RFC 4648 base32 alphabet, lowercase. 32 chars covering 5 bits each
/// — `a..z` + `2..7`. Skipping `0`/`1`/`8`/`9` keeps digit/letter
/// look-alikes (`0`/`O`, `1`/`l`) out of the encoded body.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Encode a tagged id to its string form. Renders as
/// `<prefix>-XXXX-XXXX-XXXX` where `<prefix>` is the 3-letter tag and
/// the body is the 60-bit hash in lowercase base32, grouped 4-4-4.
///
/// Returns `None` if the id's tag bits are reserved or invalid (the
/// `0x0` sentinel or `0x4..=0xF`). Callers that need a printable form
/// for a possibly-malformed id should fall back to hex via
/// `format!("{:#x}", id)`.
pub fn encode(id: u64) -> Option<String> {
    let tag = tag_of(id)?;
    let body = body_of(id);
    let mut out = String::with_capacity(3 + 1 + 12 + 2);
    out.push_str(tag.prefix());
    // Emit 12 base32 chars (60 bits / 5 bits per char), grouped
    // into three 4-char chunks separated by `-`. The shift starts
    // at bit 55 (the most significant of the 60 hash bits) and
    // counts down by 5 each char so the leftmost chars carry the
    // most significant bits — same convention as hex.
    for i in 0..12 {
        if i % 4 == 0 {
            out.push('-');
        }
        let shift = 55 - i * 5;
        let nibble = ((body >> shift) & 0x1F) as usize;
        out.push(ALPHABET[nibble] as char);
    }
    Some(out)
}

/// Decode a tagged-id string back to its `u64` form, or fail with a
/// typed error. Case-insensitive.
pub fn decode(s: &str) -> Result<u64, DecodeError> {
    // Total length: 3 (prefix) + 1 (dash) + 4 + 1 + 4 + 1 + 4 = 18.
    if s.len() != 18 {
        return Err(DecodeError::Malformed);
    }
    let bytes = s.as_bytes();
    let prefix = &bytes[0..3];
    let tag = match prefix {
        b"mbx" | b"MBX" => Tag::Mailbox,
        b"knd" | b"KND" => Tag::Kind,
        b"hdl" | b"HDL" => Tag::Handle,
        _ => return Err(DecodeError::Malformed),
    };
    if bytes[3] != b'-' || bytes[8] != b'-' || bytes[13] != b'-' {
        return Err(DecodeError::Malformed);
    }
    let mut body: u64 = 0;
    let group_starts = [4usize, 9, 14];
    for &start in &group_starts {
        for i in 0..4 {
            let c = bytes[start + i];
            let v = decode_char(c)?;
            body = (body << 5) | v as u64;
        }
    }
    Ok(with_tag(tag, body))
}

/// Decode a tagged-id string and assert its tag matches `expected`.
/// Convenience for callers that know which space they're decoding
/// into — e.g. `decode_with_tag(s, Tag::Mailbox)?` for the
/// `mailbox_id` argument of an MCP tool.
pub fn decode_with_tag(s: &str, expected: Tag) -> Result<u64, DecodeError> {
    let id = decode(s)?;
    let found = tag_of(id).ok_or(DecodeError::Malformed)?;
    if found != expected {
        return Err(DecodeError::TagMismatch { expected, found });
    }
    Ok(id)
}

const fn decode_char(c: u8) -> Result<u8, DecodeError> {
    match c {
        b'a'..=b'z' => Ok(c - b'a'),
        b'A'..=b'Z' => Ok(c - b'A'),
        b'2'..=b'7' => Ok(c - b'2' + 26),
        _ => Err(DecodeError::InvalidChar(c as char)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_tags() {
        for &tag in &[Tag::Mailbox, Tag::Kind, Tag::Handle] {
            for hash in [0u64, 0x1, 0x0FFF_FFFF_FFFF_FFFF, 0xDEAD_BEEF_CAFE_BABE] {
                let id = with_tag(tag, hash);
                assert_eq!(tag_of(id), Some(tag));
                assert_eq!(body_of(id), hash & HASH_MASK);
                let s = encode(id).unwrap();
                assert_eq!(decode(&s).unwrap(), id);
            }
        }
    }

    #[test]
    fn encoding_shape() {
        let id = with_tag(Tag::Mailbox, 0);
        let s = encode(id).unwrap();
        assert_eq!(s.len(), 18);
        assert!(s.starts_with("mbx-"));
        assert_eq!(s, "mbx-aaaa-aaaa-aaaa");
    }

    #[test]
    fn alphabet_excludes_digit_lookalikes() {
        // Body of all-1s should produce all `7`s (the highest base32
        // char), exercising the high end of the alphabet.
        let id = with_tag(Tag::Kind, HASH_MASK);
        let s = encode(id).unwrap();
        assert_eq!(s, "knd-7777-7777-7777");
        assert!(!s.contains('0'));
        assert!(!s.contains('1'));
        assert!(!s.contains('8'));
        assert!(!s.contains('9'));
    }

    #[test]
    fn decode_rejects_zero_tag() {
        // 0x0 in the high nibble is reserved invalid. `encode(0u64)`
        // should refuse rather than emit a string that decodes to a
        // bogus id.
        assert!(encode(0u64).is_none());
        // Hand-crafted "rsv-" prefix isn't in the table → Malformed.
        assert_eq!(decode("rsv-aaaa-aaaa-aaaa"), Err(DecodeError::Malformed));
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert_eq!(decode("mbx-aaaa-aaaa"), Err(DecodeError::Malformed));
        assert_eq!(decode("mbx-aaaa-aaaa-aaaaa"), Err(DecodeError::Malformed));
    }

    #[test]
    fn decode_rejects_invalid_chars() {
        // `0` and `1` are not in the base32 alphabet (they're the
        // ones we deliberately skipped).
        assert_eq!(
            decode("mbx-0aaa-aaaa-aaaa"),
            Err(DecodeError::InvalidChar('0'))
        );
        assert_eq!(
            decode("mbx-aaaa-1aaa-aaaa"),
            Err(DecodeError::InvalidChar('1'))
        );
    }

    #[test]
    fn decode_is_case_insensitive() {
        let id = with_tag(Tag::Handle, 0x1234_5678_9abc_def0 & HASH_MASK);
        let lower = encode(id).unwrap();
        let upper = lower.to_uppercase();
        assert_eq!(decode(&lower).unwrap(), id);
        assert_eq!(decode(&upper).unwrap(), id);
    }

    #[test]
    fn decode_with_tag_catches_mismatch() {
        let mailbox = encode(with_tag(Tag::Mailbox, 0x42)).unwrap();
        let err = decode_with_tag(&mailbox, Tag::Kind).unwrap_err();
        assert_eq!(
            err,
            DecodeError::TagMismatch {
                expected: Tag::Kind,
                found: Tag::Mailbox,
            }
        );
    }

    #[test]
    fn body_drops_high_bits() {
        // Caller passes a "raw" hash with high bits set; with_tag
        // masks them off before OR-ing the tag in. Critical that
        // the natural FNV-1a high bits don't leak past the boundary
        // and corrupt the tag.
        let id = with_tag(Tag::Mailbox, 0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(tag_of(id), Some(Tag::Mailbox));
        assert_eq!(body_of(id), HASH_MASK);
    }
}
