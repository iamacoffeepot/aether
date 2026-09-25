//! Why [`crate::decode()`] refused an input.

use std::error::Error;
use std::fmt;

use aether_bloomery_kinds::{NameError, PathError};

use crate::MAX_DEPTH;

/// One input a tree cannot represent, or a stream that is not a whole tar
/// archive. Carried by [`crate::DecodeError::Refused`] next to the entry it
/// names. The kinds' name and path errors are embedded unchanged, so a tar
/// entry fails the same rule a hand-built tree would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The stream ended before an all-zero header block.
    Truncated,
    /// The header checksum is neither the signed nor the unsigned sum.
    BadChecksum,
    /// The header magic is neither POSIX `ustar\0` nor GNU `ustar `.
    UnknownFormat,
    /// A numeric header field is neither octal nor positive base-256.
    BadNumber,
    /// A typeflag a tree has no node for: devices, FIFOs, contiguous files,
    /// PAX global headers, GNU sparse and multi-volume entries, and the rest.
    UnsupportedType(u8),
    /// A PAX `GNU.sparse.*` record.
    Sparse,
    /// A PAX or GNU long-name header over 1 MiB.
    ExtendedTooLarge,
    /// Two extended headers of the same kind before one entry.
    ExtendedRepeated,
    /// An extended header followed by the end of the archive.
    ExtendedDangling,
    /// A PAX record whose length, separator, terminator, or value is wrong.
    ExtendedMalformed,
    /// The entry path or link target is not UTF-8.
    NotUtf8,
    /// The entry path begins with `/`.
    Absolute,
    /// The entry path has a `..` segment.
    ParentSegment,
    /// A path segment fails a [`aether_bloomery_kinds::Name`] rule.
    Name(NameError),
    /// The root path (`.`) names something other than a directory.
    RootNotDirectory,
    /// The entry path has more than [`MAX_DEPTH`] segments.
    TooDeep,
    /// A directory entry carries content.
    DirectorySize,
    /// A symlink target fails a [`aether_bloomery_kinds::Path`] rule.
    LinkTarget(PathError),
    /// A hardlink whose target is not an earlier File or Executable entry.
    HardlinkTarget,
    /// A path that an earlier entry already named.
    Duplicate,
    /// An entry under a path an earlier entry made a file or symlink.
    ParentNotDirectory,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("the stream ended before the end-of-archive block"),
            Self::BadChecksum => f.write_str("header checksum mismatch"),
            Self::UnknownFormat => f.write_str("header magic is neither POSIX nor GNU"),
            Self::BadNumber => f.write_str("malformed numeric header field"),
            Self::UnsupportedType(flag) if flag.is_ascii_graphic() => {
                write!(f, "unsupported typeflag '{}'", char::from(*flag))
            }
            Self::UnsupportedType(flag) => write!(f, "unsupported typeflag {flag:#04x}"),
            Self::Sparse => f.write_str("sparse file"),
            Self::ExtendedTooLarge => f.write_str("extended header over 1 MiB"),
            Self::ExtendedRepeated => f.write_str("repeated extended header"),
            Self::ExtendedDangling => f.write_str("extended header not followed by an entry"),
            Self::ExtendedMalformed => f.write_str("malformed PAX record"),
            Self::NotUtf8 => f.write_str("not UTF-8"),
            Self::Absolute => f.write_str("absolute path"),
            Self::ParentSegment => f.write_str("`..` segment"),
            Self::Name(error) => write!(f, "invalid name: {error}"),
            Self::RootNotDirectory => f.write_str("the root is not a directory"),
            Self::TooDeep => write!(f, "more than {MAX_DEPTH} path segments"),
            Self::DirectorySize => f.write_str("directory with content"),
            Self::LinkTarget(error) => write!(f, "invalid symlink target: {error}"),
            Self::HardlinkTarget => f.write_str("hardlink target is not an earlier file"),
            Self::Duplicate => f.write_str("duplicate path"),
            Self::ParentNotDirectory => f.write_str("parent is not a directory"),
        }
    }
}

impl Error for Refusal {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Name(error) => Some(error),
            Self::LinkTarget(error) => Some(error),
            _ => None,
        }
    }
}
