//! The canonical tar codec for bloomery trees.
//!
//! [`encode()`] turns a [`Tree`](aether_bloomery_kinds::Tree) into a canonical
//! tar stream, and [`decode()`] turns a tar stream into a tree. The container
//! runtime's archive endpoints speak tar, so this is where a tree's identity
//! meets bytes: the same tree always encodes to the same bytes, and
//! `decode(encode(t)) == t` for every tree.
//!
//! The codec does no I/O of its own. Bytes come from an [`std::io::Read`] and
//! go to an [`std::io::Write`]; blobs and trees come from a [`TreeSource`] and
//! go to a [`TreeSink`], both implemented by the caller. Neither direction
//! holds a file's bytes in memory, and neither recurses: both walks keep an
//! explicit stack bounded by [`MAX_DEPTH`].
//!
//! # Encoding
//!
//! | Concern | Canonical value |
//! |---|---|
//! | Order | Depth-first pre-order: a directory's entry before its children, siblings in [`Tree::entries`](aether_bloomery_kinds::Tree::entries) order. |
//! | Root | No entry for the root. Paths are relative (`a`, `a/b`); directory paths end in `/`. |
//! | Typeflag, mode | File `0` 0644, Executable `0` 0755, Directory `5` 0755, Symlink `2` 0777 with the target as `linkname`. |
//! | size | The blob's length for File and Executable; 0 otherwise. |
//! | uid / gid / uname / gname | 0 / 0 / empty / empty |
//! | mtime | [`CANONICAL_MTIME_SECS`] |
//! | Header | POSIX magic `ustar\0`, version `00`, empty prefix, zero devmajor and devminor, zero-padded NUL-terminated octal numbers, checksum `%06o\0 `. |
//! | Extended header | One PAX `x` block named `././@PaxHeader` before an entry exactly when its path or link target is non-ASCII or longer than 100 bytes, or its size is at least 8 GiB. It holds only the `linkpath`, `path`, and `size` records, sorted by key. The ustar name and linkname fields then hold the longest prefix that fits in 100 bytes, cut on a char boundary, and the size field holds 0 when `size` is a record. |
//! | End | Exactly two zero blocks, no padding to a record size. |
//!
//! The mtime is a constant, never read from the environment.
//!
//! # Decoding
//!
//! Accepted formats are POSIX ustar (prefix applied), GNU `ustar  \0` (prefix
//! ignored), PAX `x` records, and GNU `L` / `K` long names. PAX wins over GNU
//! `L` / `K`, which win over the header fields. The checksum may be the signed
//! or the unsigned sum, and numbers may be octal or GNU base-256.
//!
//! | Input | Result |
//! |---|---|
//! | `0` or NUL | File, or Executable when `mode & 0o100`. |
//! | `5` | Directory. |
//! | `2` | Symlink, the target checked by [`Path::new`](aether_bloomery_kinds::Path::new). |
//! | `1` | A copy of an earlier File or Executable entry's node. |
//! | `5`, `2`, or `1` with a non-zero size | [`Refusal::HeaderOnlyContent`] |
//! | Any other typeflag | [`Refusal::UnsupportedType`] |
//! | PAX `GNU.sparse.*` | [`Refusal::Sparse`] |
//! | Other PAX keys | Ignored. |
//! | Path | Leading `./` runs and one trailing `/` stripped; a leading `/`, a `..` segment, an invalid name, or non-UTF-8 bytes refused. A bare `.` is the root: ignored as a directory, refused otherwise. |
//! | Missing parent | Created as a directory. |
//! | The same path twice | [`Refusal::Duplicate`], except an explicit directory over an implicit one. |
//! | An entry under a file or symlink | [`Refusal::ParentNotDirectory`] |
//! | More than [`MAX_DEPTH`] segments | [`Refusal::TooDeep`] |
//! | Extended header over 1 MiB, repeated, or not followed by an entry | Refused. |
//! | EOF before an all-zero header block | [`Refusal::Truncated`]. Nothing after the first zero block is read. |
//! | Owner, group, times, mode bits other than owner-exec, xattrs | Dropped. |
//!
//! Decode is many-to-one, so `encode(decode(x))` is the canonical form of any
//! accepted `x`, and equals `x` byte for byte when `x` is canonical.

#![forbid(unsafe_code)]

mod block;
mod decode;
mod encode;
mod pax;
mod store;

pub use decode::{DecodeError, Refusal, decode};
pub use encode::{EncodeError, encode};
pub use store::{BlobWriter, SourceBlob, TreeSink, TreeSource};

/// The most path segments an entry may have, on both sides of the codec.
pub const MAX_DEPTH: usize = 256;

/// Every entry's mtime: 1980-01-01T00:00:00Z. Not 0, because some tools read
/// 0 as "unknown" and zip refuses times before 1980.
pub const CANONICAL_MTIME_SECS: u64 = 315_532_800;
