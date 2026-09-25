//! Tree → canonical tar stream.

use std::collections::btree_map;
use std::error::Error;
use std::fmt;
use std::io::{self, Read, Write};

use aether_bloomery_kinds::{Name, Node, OpaqueBytes, Ref, Tree};

use crate::block::{BLOCK_BYTES, Header, NAME_FIELD_BYTES, chunk_len, padding_len, typeflag};
use crate::store::{SourceBlob, TreeSource};
use crate::{MAX_DEPTH, pax};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// The first size the 11-digit octal size field cannot hold.
const PAX_SIZE_BYTES: u64 = 8 << 30;

const FILE_MODE: u32 = 0o644;
const EXECUTABLE_MODE: u32 = 0o755;
const DIRECTORY_MODE: u32 = 0o755;
const SYMLINK_MODE: u32 = 0o777;

/// Why [`encode()`] stopped. Bytes already written to the output are not a
/// valid archive.
#[derive(Debug)]
pub enum EncodeError<E> {
    /// The [`TreeSource`] failed to load a tree or open a blob.
    Source(E),
    /// Writing the output failed.
    Write(io::Error),
    /// Reading a blob's bytes failed.
    BlobRead(io::Error),
    /// A blob's reader disagreed with the length its source stated.
    /// `actual` is how many bytes it yielded before the encoder stopped
    /// reading: fewer than `expected` when it ended early, or `expected + 1`
    /// when it had more.
    BlobLength { expected: u64, actual: u64 },
    /// An entry would have more than [`MAX_DEPTH`] path segments.
    TooDeep,
}

impl<E: fmt::Display> fmt::Display for EncodeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "tree source: {error}"),
            Self::Write(error) => write!(f, "writing the archive: {error}"),
            Self::BlobRead(error) => write!(f, "reading a blob: {error}"),
            Self::BlobLength { expected, actual } => {
                write!(f, "a blob stated {expected} bytes and its reader yielded {actual}")
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
    let mut writer = Writer { out, buffer: vec![0; COPY_BUFFER_BYTES] };
    let mut path = String::new();
    let mut stack = vec![Frame { base_len: 0, depth: 0, entries: listing(source, root)? }];

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

        match node {
            Node::File(blob) => writer.file(&path, FILE_MODE, source, &blob)?,
            Node::Executable(blob) => writer.file(&path, EXECUTABLE_MODE, source, &blob)?,
            Node::Symlink(target) => {
                writer.entry(Entry {
                    path: &path,
                    typeflag: typeflag::SYMLINK,
                    mode: SYMLINK_MODE,
                    size: 0,
                    link: target.as_str(),
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
                })?;
                let entries = listing(source, &tree)?;
                stack.push(Frame { base_len: path.len(), depth, entries });
            }
        }
    }

    writer.out.write_all(&[0; 2 * BLOCK_BYTES]).map_err(EncodeError::Write)
}

/// One open directory: where its children's paths start in the shared path
/// buffer, its depth, and the entries not yet written.
struct Frame {
    base_len: usize,
    depth: usize,
    entries: btree_map::IntoIter<Name, Node>,
}

fn listing<S: TreeSource>(
    source: &mut S,
    tree: &Ref<Tree>,
) -> Result<btree_map::IntoIter<Name, Node>, EncodeError<S::Error>> {
    Ok(source.tree(tree).map_err(EncodeError::Source)?.entries().clone().into_iter())
}

/// The fields of one entry that vary.
#[derive(Clone, Copy)]
struct Entry<'a> {
    path: &'a str,
    typeflag: u8,
    mode: u32,
    size: u64,
    link: &'a str,
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
        source: &mut S,
        blob: &Ref<OpaqueBytes>,
    ) -> Result<(), EncodeError<S::Error>> {
        let SourceBlob { len, mut reader } = source.blob(blob).map_err(EncodeError::Source)?;
        self.entry(Entry { path, typeflag: typeflag::REGULAR, mode, size: len, link: "" })?;
        self.copy(&mut reader, len)?;
        self.pad(len)
    }

    /// Copy exactly `len` bytes, then probe for one more so a long reader is
    /// caught rather than silently cut.
    fn copy<E>(&mut self, reader: &mut impl Read, len: u64) -> Result<(), EncodeError<E>> {
        let mut copied = 0;
        while copied < len {
            let want = chunk_len(len - copied, self.buffer.len());
            let read = reader.read(&mut self.buffer[..want]).map_err(EncodeError::BlobRead)?;
            if read == 0 {
                return Err(EncodeError::BlobLength { expected: len, actual: copied });
            }
            self.out.write_all(&self.buffer[..read]).map_err(EncodeError::Write)?;
            copied += read as u64;
        }
        if reader.read(&mut [0; 1]).map_err(EncodeError::BlobRead)? != 0 {
            return Err(EncodeError::BlobLength { expected: len, actual: len + 1 });
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
            let header =
                Header { name: pax::HEADER_NAME, typeflag: typeflag::PAX, mode: FILE_MODE, size: len, linkname: b"" };
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
