//! Segment grammar shared by [`crate::Namespace`], [`crate::LoadName`], and
//! each part of an [`crate::ActorPath`] segment: the rules of
//! `aether_actor::model::validate_namespace_segment` as a byte loop usable
//! in `const` context, so an invalid `Namespace` literal fails to compile
//! while `LoadName` enforces the same rules at run time.

/// Byte-length cap for a segment. Mirrors
/// `aether_actor::model::NAMESPACE_SEGMENT_MAX_LEN`; the two must stay in
/// lockstep or a name valid here fails registration there.
const MAX_SEGMENT_BYTES: usize = 256;

/// One violated segment rule. Carries no payload: every site enforces the
/// same limit, so the fault alone determines the message.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SegmentFault {
    /// The segment is empty.
    Empty,
    /// The segment exceeds `MAX_SEGMENT_BYTES` bytes.
    TooLong,
    /// The segment contains `:` or `/`.
    ContainsSeparator,
    /// The segment contains a control or whitespace character. Malformed
    /// UTF-8 also lands here: callers pass `str` bytes, so it is
    /// unreachable, but the check fails closed.
    ContainsControlOrWhitespace,
}

impl SegmentFault {
    /// Human-readable description of the violated rule. Used as the
    /// [`crate::Namespace`] `const`-`panic` message and under
    /// [`crate::LoadNameError`]'s `Display`.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Empty => "segment must not be empty",
            Self::TooLong => "segment exceeds 256 bytes",
            Self::ContainsSeparator => "segment must not contain ':' or '/'",
            Self::ContainsControlOrWhitespace => "segment must not contain control or whitespace characters",
        }
    }
}

/// Whether `code` is Unicode whitespace (`char::is_whitespace`). The ASCII
/// members (`0x09..=0x0D`, `0x20`) and `U+0085` are already covered by the
/// control ranges in [`check_segment`]; the code points listed here are the
/// multi-byte ones, each reached through its UTF-8 decoding.
const fn is_whitespace(code: u32) -> bool {
    matches!(code, 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000)
}

/// Decode the `tail`-byte UTF-8 tail starting after `bytes[start]`,
/// continuing from the lead byte's payload `code`. Returns the decoded code
/// point and the index past the sequence, or `None` when the tail runs past
/// the end or holds a non-continuation byte.
const fn decode_tail(bytes: &[u8], start: usize, tail: usize, code: u32) -> Option<(u32, usize)> {
    let mut code = code;
    let mut index = start + 1;
    let end = start + 1 + tail;
    if end > bytes.len() {
        return None;
    }
    while index < end {
        let byte = bytes[index];
        if !matches!(byte, 0x80..0xC0) {
            return None;
        }
        code = (code << 6) | ((byte & 0x3F) as u32);
        index += 1;
    }
    Some((code, end))
}

/// Check `bytes` against the namespace-segment grammar: non-empty, at most
/// 256 bytes, no `:` or `/`, no control or whitespace characters.
/// Multi-byte UTF-8 is accepted; each character is decoded and the code
/// point tested, so the C1 controls (`U+0080..=U+009F`) and the Unicode
/// whitespace code points are rejected exactly as
/// `validate_namespace_segment` rejects them via `char::is_control` and
/// `char::is_whitespace`.
pub const fn check_segment(bytes: &[u8]) -> Result<(), SegmentFault> {
    if bytes.is_empty() {
        return Err(SegmentFault::Empty);
    }
    if bytes.len() > MAX_SEGMENT_BYTES {
        return Err(SegmentFault::TooLong);
    }
    let mut index = 0;
    while index < bytes.len() {
        let first = bytes[index];
        let (code, next) = if first < 0x80 {
            (first as u32, index + 1)
        } else if first < 0xC2 {
            return Err(SegmentFault::ContainsControlOrWhitespace);
        } else if first < 0xE0 {
            let Some(decoded) = decode_tail(bytes, index, 1, (first & 0x1F) as u32) else {
                return Err(SegmentFault::ContainsControlOrWhitespace);
            };
            decoded
        } else if first < 0xF0 {
            let Some(decoded) = decode_tail(bytes, index, 2, (first & 0x0F) as u32) else {
                return Err(SegmentFault::ContainsControlOrWhitespace);
            };
            decoded
        } else if first < 0xF5 {
            let Some(decoded) = decode_tail(bytes, index, 3, (first & 0x07) as u32) else {
                return Err(SegmentFault::ContainsControlOrWhitespace);
            };
            decoded
        } else {
            return Err(SegmentFault::ContainsControlOrWhitespace);
        };
        if code == b':' as u32 || code == b'/' as u32 {
            return Err(SegmentFault::ContainsSeparator);
        }
        if code < 0x20 || matches!(code, 0x7F..=0x9F) || is_whitespace(code) {
            return Err(SegmentFault::ContainsControlOrWhitespace);
        }
        index = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(check_segment(b""), Err(SegmentFault::Empty));
    }

    #[test]
    fn rejects_too_long() {
        assert_eq!(check_segment(&vec![b'x'; 257]), Err(SegmentFault::TooLong));
    }

    #[test]
    fn rejects_separator() {
        assert_eq!(check_segment(b"a/b"), Err(SegmentFault::ContainsSeparator));
    }

    #[test]
    fn rejects_control_or_whitespace() {
        assert_eq!(check_segment(b"a b"), Err(SegmentFault::ContainsControlOrWhitespace));
    }

    #[test]
    fn rejects_multi_byte_whitespace_and_control() {
        assert_eq!(check_segment("a\u{3000}b".as_bytes()), Err(SegmentFault::ContainsControlOrWhitespace));
        assert_eq!(check_segment("a\u{0085}b".as_bytes()), Err(SegmentFault::ContainsControlOrWhitespace));
    }

    #[test]
    fn accepts_multi_byte() {
        assert_eq!(check_segment("aether.日本語".as_bytes()), Ok(()));
    }
}
