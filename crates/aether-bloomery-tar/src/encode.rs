//! Tree → canonical tar stream, or the same stream with the files that
//! differ from a base tree stamped.

use std::collections::btree_map;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};

use aether_bloomery_kinds::{Name, Node, Tree};
use aether_data::{OpaqueBytes, Ref};

use crate::block::{BLOCK_BYTES, Header, NAME_FIELD_BYTES, chunk_len, padding_len, typeflag};
use crate::store::{SourceBlob, TreeSource};
use crate::{CANONICAL_MTIME_SECS, MAX_DEPTH, pax};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// The first size the 11-digit octal size field cannot hold.
const PAX_SIZE_BYTES: u64 = 8 << 30;

const FILE_MODE: u32 = 0o644;
const EXECUTABLE_MODE: u32 = 0o755;
const DIRECTORY_MODE: u32 = 0o755;
const SYMLINK_MODE: u32 = 0o777;

/// Why [`encode()`] or [`encode_stamped()`] stopped. Bytes already written
/// to the output are not a valid archive.
#[derive(Debug)]
pub enum EncodeError<E> {
    /// The [`TreeSource`] failed to load a tree or open a blob.
    Source(E),
    /// Writing the output failed.
    Write(io::Error),
    /// Reading a blob's bytes failed.
    BlobRead(io::Error),
    /// The reader of `blob` disagreed with the length its source stated.
    /// `actual` is how many bytes it yielded before the encoder stopped
    /// reading: fewer than `expected` when it ended early, or `expected + 1`
    /// when it had more.
    BlobLength { blob: Ref<OpaqueBytes>, expected: u64, actual: u64 },
    /// An entry would have more than [`MAX_DEPTH`] path segments.
    TooDeep,
}

impl<E: fmt::Display> fmt::Display for EncodeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "tree source: {error}"),
            Self::Write(error) => write!(f, "writing the archive: {error}"),
            Self::BlobRead(error) => write!(f, "reading a blob: {error}"),
            Self::BlobLength { blob, expected, actual } => {
                write!(f, "blob {} stated {expected} bytes and its reader yielded {actual}", blob.digest())
            }
            Self::TooDeep => write!(f, "an entry has more than {MAX_DEPTH} path segments"),
        }
    }
}

impl<E: Error + 'static> Error for EncodeError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Write(error) | Self::BlobRead(error) => Some(error),
            Self::BlobLength { .. } | Self::TooDeep => None,
        }
    }
}

/// Write the canonical tar stream of the tree `root` names to `out`.
///
/// The walk keeps one loaded directory listing per open directory and one
/// copy buffer; a file's bytes stream from its reader straight to `out`.
///
/// # Errors
///
/// [`EncodeError`] names the failing store call, write, blob, or depth.
pub fn encode<S: TreeSource, W: Write>(root: &Ref<Tree>, source: &mut S, out: W) -> Result<(), EncodeError<S::Error>> {
    walk(root, Against::Unchanged, CANONICAL_MTIME_SECS, source, out)
}

/// What a stamped stream marks: the files that differ from `base` get
/// `mtime_secs`, and everything else the canonical mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// The tree to compare against. `None` stamps every file.
    pub base: Option<Ref<Tree>>,
    /// The mtime a file that differs from `base` carries.
    pub mtime_secs: u64,
}

/// Write the tar stream of `root` to `out` with the canonical encoding,
/// except that every File, Executable, or Symlink that `stamp.base` lacks at
/// the same path, or holds as another node, carries `stamp.mtime_secs`.
///
/// The comparison is part of the walk: each open directory carries the base
/// directory at its path, and a subdirectory whose digest equals the base's
/// is written without loading any more of the base. Directory entries always
/// carry the canonical mtime. A stamped stream is never hashed: a file's
/// mtime is not part of its tree.
///
/// # Errors
///
/// [`EncodeError`] names the failing store call, write, blob, or depth. A
/// base listing the source cannot load is [`EncodeError::Source`].
pub fn encode_stamped<S: TreeSource, W: Write>(
    root: &Ref<Tree>,
    stamp: &Stamp,
    source: &mut S,
    out: W,
) -> Result<(), EncodeError<S::Error>> {
    let against = match stamp.base {
        Some(base) if base == *root => Against::Unchanged,
        Some(base) => Against::Base(load(source, &base)?),
        None => Against::Added,
    };
    walk(root, against, stamp.mtime_secs, source, out)
}

/// The walk both encodings share: depth-first pre-order from `root`, each
/// file's mtime chosen by what its directory is compared `against`.
fn walk<S: TreeSource, W: Write>(
    root: &Ref<Tree>,
    against: Against,
    mtime_secs: u64,
    source: &mut S,
    out: W,
) -> Result<(), EncodeError<S::Error>> {
    let mut writer = Writer { out, buffer: vec![0; COPY_BUFFER_BYTES] };
    let mut path = String::new();
    let mut stack = vec![Frame { base_len: 0, depth: 0, entries: listing(source, root)?, against }];

    while let Some(frame) = stack.last_mut() {
        let Some((name, node)) = frame.entries.next() else {
            stack.pop();
            continue;
        };
        let depth = frame.depth + 1;
        if depth > MAX_DEPTH {
            return Err(EncodeError::TooDeep);
        }
        path.truncate(frame.base_len);
        path.push_str(name.as_str());
        let mtime = frame.against.mtime(&name, &node, mtime_secs);

        match node {
            Node::File(blob) => writer.file(&path, FILE_MODE, mtime, source, &blob)?,
            Node::Executable(blob) => writer.file(&path, EXECUTABLE_MODE, mtime, source, &blob)?,
            Node::Symlink(target) => {
                writer.entry(Entry {
                    path: &path,
                    typeflag: typeflag::SYMLINK,
                    mode: SYMLINK_MODE,
                    size: 0,
                    link: target.as_str(),
                    mtime,
                })?;
            }
            Node::Directory(tree) => {
                path.push('/');
                writer.entry(Entry {
                    path: &path,
                    typeflag: typeflag::DIRECTORY,
                    mode: DIRECTORY_MODE,
                    size: 0,
                    link: "",
                    mtime: CANONICAL_MTIME_SECS,
                })?;
                let against = frame.against.child(&name, &tree, source)?;
                let entries = listing(source, &tree)?;
                stack.push(Frame { base_len: path.len(), depth, entries, against });
            }
        }
    }

    writer.write(&[0; 2 * BLOCK_BYTES])?;
    writer.out.flush().map_err(EncodeError::Write)
}

/// One open directory: where its children's paths start in the shared path
/// buffer, its depth, the entries not yet written, and what its files are
/// compared against.
struct Frame {
    base_len: usize,
    depth: usize,
    entries: btree_map::IntoIter<Name, Node>,
    against: Against,
}

/// What an open directory's entries are compared against.
enum Against {
    /// Nothing differs: the canonical encoding, or a directory equal to the
    /// base's at its path. Every entry keeps the canonical mtime.
    Unchanged,
    /// The base's directory at the same path: an entry it holds as the same
    /// node keeps the canonical mtime, and any other file is stamped.
    Base(Tree),
    /// The base has no directory here: every file is stamped.
    Added,
}

impl Against {
    /// The mtime of the non-directory entry `name`, holding `node`.
    fn mtime(&self, name: &Name, node: &Node, mtime_secs: u64) -> u64 {
        match self {
            Self::Unchanged => CANONICAL_MTIME_SECS,
            Self::Base(base) => {
                let same = base.entries().get(name) == Some(node);
                if same {
                    CANONICAL_MTIME_SECS
                } else {
                    mtime_secs
                }
            }
            Self::Added => mtime_secs,
        }
    }

    /// What the subdirectory `name`, holding `tree`, is compared against: an
    /// equal base subtree is not loaded.
    fn child<S: TreeSource>(
        &self,
        name: &Name,
        tree: &Ref<Tree>,
        source: &mut S,
    ) -> Result<Self, EncodeError<S::Error>> {
        match self {
            Self::Unchanged => Ok(Self::Unchanged),
            Self::Added => Ok(Self::Added),
            Self::Base(base) => match base.entries().get(name) {
                Some(Node::Directory(based)) if based == tree => Ok(Self::Unchanged),
                Some(Node::Directory(based)) => Ok(Self::Base(load(source, based)?)),
                _ => Ok(Self::Added),
            },
        }
    }
}

fn listing<S: TreeSource>(
    source: &mut S,
    tree: &Ref<Tree>,
) -> Result<btree_map::IntoIter<Name, Node>, EncodeError<S::Error>> {
    Ok(load(source, tree)?.entries().clone().into_iter())
}

fn load<S: TreeSource>(source: &mut S, tree: &Ref<Tree>) -> Result<Tree, EncodeError<S::Error>> {
    source.tree(tree).map_err(EncodeError::Source)
}

/// The fields of one entry that vary.
#[derive(Clone, Copy)]
struct Entry<'a> {
    path: &'a str,
    typeflag: u8,
    mode: u32,
    size: u64,
    link: &'a str,
    mtime: u64,
}

struct Writer<W> {
    out: W,
    buffer: Vec<u8>,
}

impl<W: Write> Writer<W> {
    /// A File or Executable: the header, exactly `len` bytes from the blob's
    /// reader, then padding.
    fn file<S: TreeSource>(
        &mut self,
        path: &str,
        mode: u32,
        mtime: u64,
        source: &mut S,
        blob: &Ref<OpaqueBytes>,
    ) -> Result<(), EncodeError<S::Error>> {
        let SourceBlob { len, mut reader } = source.blob(blob).map_err(EncodeError::Source)?;
        self.entry(Entry { path, typeflag: typeflag::REGULAR, mode, size: len, link: "", mtime })?;
        self.copy(&mut reader, blob, len)?;
        self.pad(len)
    }

    /// Copy exactly `len` bytes of `blob`, then probe for one more so a long
    /// reader is caught rather than silently cut.
    fn copy<E>(&mut self, reader: &mut impl Read, blob: &Ref<OpaqueBytes>, len: u64) -> Result<(), EncodeError<E>> {
        let mismatch = |actual| EncodeError::BlobLength { blob: *blob, expected: len, actual };
        let mut copied = 0;
        while copied < len {
            let want = chunk_len(len - copied, self.buffer.len());
            let read = read_retrying(reader, &mut self.buffer[..want])?;
            if read == 0 {
                return Err(mismatch(copied));
            }
            self.out.write_all(&self.buffer[..read]).map_err(EncodeError::Write)?;
            copied += read as u64;
        }
        if read_retrying(reader, &mut [0; 1])? != 0 {
            return Err(mismatch(len + 1));
        }
        Ok(())
    }

    /// A PAX header when the entry needs one, then the ustar header.
    fn entry<E>(&mut self, entry: Entry<'_>) -> Result<(), EncodeError<E>> {
        let pax_link = needs_pax(entry.link);
        let pax_path = needs_pax(entry.path);
        let pax_size = entry.size >= PAX_SIZE_BYTES;
        if pax_link || pax_path || pax_size {
            let mut records = Vec::new();
            if pax_link {
                pax::write_record(&mut records, "linkpath", entry.link.as_bytes());
            }
            if pax_path {
                pax::write_record(&mut records, "path", entry.path.as_bytes());
            }
            if pax_size {
                pax::write_record(&mut records, "size", entry.size.to_string().as_bytes());
            }
            let len = records.len() as u64;
            let header = Header {
                name: pax::HEADER_NAME,
                typeflag: typeflag::PAX,
                mode: FILE_MODE,
                size: len,
                linkname: b"",
                mtime: CANONICAL_MTIME_SECS,
            };
            self.write(&header.to_block())?;
            self.write(&records)?;
            self.pad(len)?;
        }
        let header = Header {
            name: field_prefix(entry.path).as_bytes(),
            typeflag: entry.typeflag,
            mode: entry.mode,
            size: if pax_size {
                0
            } else {
                entry.size
            },
            linkname: field_prefix(entry.link).as_bytes(),
            mtime: entry.mtime,
        };
        self.write(&header.to_block())
    }

    /// Zero bytes up to the next block boundary after `len` bytes of content.
    fn pad<E>(&mut self, len: u64) -> Result<(), EncodeError<E>> {
        self.write(&[0; BLOCK_BYTES][..padding_len(len)])
    }

    fn write<E>(&mut self, bytes: &[u8]) -> Result<(), EncodeError<E>> {
        self.out.write_all(bytes).map_err(EncodeError::Write)
    }
}

/// One `read`, retried while it reports [`io::ErrorKind::Interrupted`], the
/// way [`Read::read_exact`] retries.
fn read_retrying<E>(reader: &mut impl Read, into: &mut [u8]) -> Result<usize, EncodeError<E>> {
    loop {
        match reader.read(into) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result.map_err(EncodeError::BlobRead),
        }
    }
}

fn needs_pax(text: &str) -> bool {
    !text.is_ascii() || text.len() > NAME_FIELD_BYTES
}

/// The longest prefix of `text` that fits a ustar name field, cut on a char
/// boundary.
fn field_prefix(text: &str) -> &str {
    let mut end = text.len().min(NAME_FIELD_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::{Entry, FILE_MODE, PAX_SIZE_BYTES, Writer};
    use crate::block::{self, BLOCK_BYTES, typeflag};
    use crate::{CANONICAL_MTIME_SECS, pax};

    /// The header records `Writer::entry` writes for a file of `size` bytes.
    fn headers(size: u64) -> Vec<u8> {
        let mut writer = Writer { out: Vec::new(), buffer: Vec::new() };
        let entry = Entry {
            path: "f",
            typeflag: typeflag::REGULAR,
            mode: FILE_MODE,
            size,
            link: "",
            mtime: CANONICAL_MTIME_SECS,
        };
        writer.entry::<()>(entry).expect("a Vec takes every write");
        writer.out
    }

    fn header_size(block: &[u8]) -> u64 {
        block::parse(block.try_into().expect("one block")).map(|header| header.size).expect("a valid header")
    }

    #[test]
    fn sizes_from_8_gibibytes_move_to_a_pax_size_record() {
        // Catches an off-by-one at the PAX size boundary: one byte below it
        // must fit the 11-digit octal field, and the boundary itself must not
        // reach that field, which cannot hold it. An 8 GiB stream is too
        // large to run, so only the headers are written.
        let below = headers(PAX_SIZE_BYTES - 1);
        assert_eq!(below.len(), BLOCK_BYTES);
        assert_eq!(header_size(&below), PAX_SIZE_BYTES - 1);

        let at = headers(PAX_SIZE_BYTES);
        assert_eq!(at.len(), 3 * BLOCK_BYTES);
        let body_len = usize::try_from(header_size(&at[..BLOCK_BYTES])).expect("a small body");
        let records = pax::parse(&at[BLOCK_BYTES..BLOCK_BYTES + body_len]).expect("valid records");
        assert_eq!((records.size, records.path), (Some(PAX_SIZE_BYTES), None));
        assert_eq!(header_size(&at[2 * BLOCK_BYTES..]), 0);
    }
}
