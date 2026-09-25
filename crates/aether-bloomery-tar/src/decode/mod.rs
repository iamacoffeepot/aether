//! Tar stream → tree.

mod entry;
mod refusal;
mod skeleton;

use std::error::Error;
use std::fmt;
use std::io::{self, ErrorKind, Read};
use std::mem;

use aether_bloomery_kinds::{Node, OpaqueBytes, Ref, Tree};

pub use refusal::Refusal;

use crate::block::{self, BLOCK_BYTES, RawHeader, chunk_len, padding_len, typeflag};
use crate::pax::{self, Records};
use crate::store::{BlobWriter, TreeSink};
use entry::{EntryKind, Target};
use skeleton::Skeleton;

const COPY_BUFFER_BYTES: usize = 64 * 1024;

/// The largest PAX or GNU long-name header body decode accepts.
const EXTENDED_MAX_BYTES: u64 = 1 << 20;

/// Why [`decode()`] stopped. Blobs and trees already handed to the sink are
/// not referenced by any result.
#[derive(Debug)]
pub enum DecodeError<E> {
    /// The [`TreeSink`] failed to store a blob or a tree.
    Sink(E),
    /// Reading the input failed.
    Read(io::Error),
    /// The input holds something a tree cannot represent, or is not a whole
    /// archive. `entry` is the archive path of the refused entry (lossy UTF-8),
    /// or empty for a stream that ends at a header boundary.
    Refused { entry: String, refusal: Refusal },
}

impl<E: fmt::Display> fmt::Display for DecodeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sink(error) => write!(f, "tree sink: {error}"),
            Self::Read(error) => write!(f, "reading the archive: {error}"),
            Self::Refused { entry, refusal } => write!(f, "entry {entry:?} refused: {refusal}"),
        }
    }
}

impl<E: Error + 'static> Error for DecodeError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sink(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Refused { refusal, .. } => Some(refusal),
        }
    }
}

/// Read a tar stream from `input` into trees and blobs stored through `sink`,
/// and return the root tree's reference.
///
/// A file's bytes stream through one copy buffer into the sink; the only
/// state kept across entries is a name and a reference per entry. Nothing
/// after the first all-zero header block is read.
///
/// # Errors
///
/// [`DecodeError`] names the failing read, sink call, or refused entry.
pub fn decode<K: TreeSink, R: Read>(input: R, sink: &mut K) -> Result<Ref<Tree>, DecodeError<K::Error>> {
    let mut reader = Reader { input, buffer: vec![0; COPY_BUFFER_BYTES] };
    let mut skeleton = Skeleton::new();
    let mut extended = Extended::default();
    let mut block = [0; BLOCK_BYTES];

    loop {
        reader.exact(&mut block, "")?;
        if block::is_zero(&block) {
            break;
        }
        let header = block::parse(&block).map_err(|refusal| refused(block::name_field(&block), refusal))?;

        match header.typeflag {
            typeflag::PAX => {
                let records =
                    pax::parse(&reader.extended(&header)?).map_err(|refusal| refused(&header.name, refusal))?;
                once(&mut extended.pax, records).map_err(|refusal| refused(&header.name, refusal))?;
                extended.header_name = header.name;
            }
            typeflag::GNU_LONG_NAME | typeflag::GNU_LONG_LINK => {
                let mut value = reader.extended(&header)?;
                value.truncate(value.iter().position(|&byte| byte == 0).unwrap_or(value.len()));
                let slot = if header.typeflag == typeflag::GNU_LONG_NAME {
                    &mut extended.long_name
                } else {
                    &mut extended.long_link
                };
                once(slot, value).map_err(|refusal| refused(&header.name, refusal))?;
                extended.header_name = header.name;
            }
            _ => place(&mut reader, &mut skeleton, sink, header, extended.take())?,
        }
    }

    if !extended.is_empty() {
        return Err(refused(&extended.header_name, Refusal::ExtendedDangling));
    }
    skeleton.finish(sink)
}

/// The extended headers collected for the next entry: at most one of each.
#[derive(Default)]
struct Extended {
    pax: Option<Records>,
    long_name: Option<Vec<u8>>,
    long_link: Option<Vec<u8>>,
    /// The name of the latest extended header, to name a dangling one.
    header_name: Vec<u8>,
}

impl Extended {
    fn is_empty(&self) -> bool {
        self.pax.is_none() && self.long_name.is_none() && self.long_link.is_none()
    }

    fn take(&mut self) -> Self {
        mem::take(self)
    }
}

fn once<T>(slot: &mut Option<T>, value: T) -> Result<(), Refusal> {
    if slot.is_some() {
        return Err(Refusal::ExtendedRepeated);
    }
    *slot = Some(value);
    Ok(())
}

/// Apply the extended headers to one entry header and place the entry in
/// the skeleton, streaming a file's bytes to the sink.
fn place<K: TreeSink, R: Read>(
    reader: &mut Reader<R>,
    skeleton: &mut Skeleton,
    sink: &mut K,
    header: RawHeader,
    extended: Extended,
) -> Result<(), DecodeError<K::Error>> {
    let pax = extended.pax.unwrap_or_default();
    let path = pax.path.or(extended.long_name).unwrap_or_else(|| header.path());
    let link = pax.linkpath.or(extended.long_link).unwrap_or(header.linkname);
    let size = pax.size.unwrap_or(header.size);
    let name = String::from_utf8_lossy(&path).into_owned();
    let refuse = |refusal| DecodeError::Refused { entry: name.clone(), refusal };

    let kind = entry::classify(header.typeflag, header.mode).map_err(refuse)?;
    match (entry::normalize(&path).map_err(refuse)?, kind) {
        (Target::Root, EntryKind::Directory) if size == 0 => Ok(()),
        (_, EntryKind::Directory) if size != 0 => Err(refuse(Refusal::DirectorySize)),
        (Target::Root, _) => Err(refuse(Refusal::RootNotDirectory)),
        (Target::Entry(path), EntryKind::Directory) => skeleton.directory(&path).map_err(refuse),
        (Target::Entry(path), EntryKind::Regular { executable }) => {
            let vacancy = skeleton.vacancy(&path).map_err(refuse)?;
            let blob = reader.blob(size, sink, &name)?;
            skeleton.fill(
                vacancy,
                if executable {
                    Node::Executable(blob)
                } else {
                    Node::File(blob)
                },
            );
            Ok(())
        }
        (Target::Entry(path), EntryKind::Symlink) => {
            let target = entry::link_target(&link).map_err(refuse)?;
            let vacancy = skeleton.vacancy(&path).map_err(refuse)?;
            reader.skip(size, &name)?;
            skeleton.fill(vacancy, Node::Symlink(target));
            Ok(())
        }
        (Target::Entry(path), EntryKind::Hardlink) => {
            let Ok(Target::Entry(target)) = entry::normalize(&link) else {
                return Err(refuse(Refusal::HardlinkTarget));
            };
            let node = skeleton.hardlink(&target).map_err(refuse)?;
            let vacancy = skeleton.vacancy(&path).map_err(refuse)?;
            reader.skip(size, &name)?;
            skeleton.fill(vacancy, node);
            Ok(())
        }
    }
}

fn refused<E>(entry: &[u8], refusal: Refusal) -> DecodeError<E> {
    DecodeError::Refused { entry: String::from_utf8_lossy(entry).into_owned(), refusal }
}

/// The input and the one copy buffer every content read goes through.
struct Reader<R> {
    input: R,
    buffer: Vec<u8>,
}

impl<R: Read> Reader<R> {
    fn exact<E>(&mut self, into: &mut [u8], entry: &str) -> Result<(), DecodeError<E>> {
        read_exact(&mut self.input, into, entry)
    }

    /// Stream `size` content bytes into a new blob, then skip the padding.
    fn blob<K: TreeSink>(
        &mut self,
        size: u64,
        sink: &mut K,
        entry: &str,
    ) -> Result<Ref<OpaqueBytes>, DecodeError<K::Error>> {
        let mut writer = sink.begin_blob(size).map_err(DecodeError::Sink)?;
        let mut left = size;
        while left > 0 {
            let chunk = &mut self.buffer[..chunk_len(left, COPY_BUFFER_BYTES)];
            read_exact(&mut self.input, chunk, entry)?;
            writer.write_chunk(chunk).map_err(DecodeError::Sink)?;
            left -= chunk.len() as u64;
        }
        let blob = writer.finish().map_err(DecodeError::Sink)?;
        self.padding(size, entry)?;
        Ok(blob)
    }

    /// Discard `size` content bytes and their padding.
    fn skip<E>(&mut self, size: u64, entry: &str) -> Result<(), DecodeError<E>> {
        let mut left = size;
        while left > 0 {
            let chunk = &mut self.buffer[..chunk_len(left, COPY_BUFFER_BYTES)];
            read_exact(&mut self.input, chunk, entry)?;
            left -= chunk.len() as u64;
        }
        self.padding(size, entry)
    }

    fn padding<E>(&mut self, size: u64, entry: &str) -> Result<(), DecodeError<E>> {
        read_exact(&mut self.input, &mut self.buffer[..padding_len(size)], entry)
    }

    /// Read the body of a PAX or GNU long-name header.
    fn extended<E>(&mut self, header: &RawHeader) -> Result<Vec<u8>, DecodeError<E>> {
        let too_large = || refused(&header.name, Refusal::ExtendedTooLarge);
        if header.size > EXTENDED_MAX_BYTES {
            return Err(too_large());
        }
        let mut body = vec![0; usize::try_from(header.size).map_err(|_| too_large())?];
        let entry = String::from_utf8_lossy(&header.name).into_owned();
        read_exact(&mut self.input, &mut body, &entry)?;
        self.padding(header.size, &entry)?;
        Ok(body)
    }
}

/// `read_exact`, with an early end of input refused as [`Refusal::Truncated`]
/// so a dropped stream never decodes as a smaller tree.
fn read_exact<E>(input: &mut impl Read, into: &mut [u8], entry: &str) -> Result<(), DecodeError<E>> {
    input.read_exact(into).map_err(|error| match error.kind() {
        ErrorKind::UnexpectedEof => DecodeError::Refused { entry: entry.to_owned(), refusal: Refusal::Truncated },
        _ => DecodeError::Read(error),
    })
}

#[cfg(test)]
mod tests;
