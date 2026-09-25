//! One 512-byte tar header block: the canonical writer and the tolerant parser.

use std::ops::Range;

use crate::{CANONICAL_MTIME_SECS, Refusal};

/// Every tar record is a whole number of these.
pub const BLOCK_BYTES: usize = 512;

/// The capacity of the ustar name and linkname fields.
pub const NAME_FIELD_BYTES: usize = 100;

const NAME: Range<usize> = 0..100;
const MODE: Range<usize> = 100..108;
const UID: Range<usize> = 108..116;
const GID: Range<usize> = 116..124;
const SIZE: Range<usize> = 124..136;
const MTIME: Range<usize> = 136..148;
const CHECKSUM: Range<usize> = 148..156;
const TYPEFLAG: usize = 156;
const LINKNAME: Range<usize> = 157..257;
const MAGIC: Range<usize> = 257..263;
const VERSION: Range<usize> = 263..265;
const DEVMAJOR: Range<usize> = 329..337;
const DEVMINOR: Range<usize> = 337..345;
const PREFIX: Range<usize> = 345..500;

const POSIX_MAGIC: &[u8] = b"ustar\0";
const GNU_MAGIC: &[u8] = b"ustar ";

/// The typeflag bytes the codec writes or dispatches on.
pub mod typeflag {
    pub const REGULAR: u8 = b'0';
    pub const REGULAR_OLD: u8 = 0;
    pub const HARDLINK: u8 = b'1';
    pub const SYMLINK: u8 = b'2';
    pub const DIRECTORY: u8 = b'5';
    pub const PAX: u8 = b'x';
    pub const GNU_LONG_NAME: u8 = b'L';
    pub const GNU_LONG_LINK: u8 = b'K';
}

/// The fields a canonical header varies; every other field is fixed.
pub struct Header<'a> {
    /// At most [`NAME_FIELD_BYTES`]; the caller cuts a longer path.
    pub name: &'a [u8],
    pub typeflag: u8,
    pub mode: u32,
    /// Below 8 GiB, the limit of the 11-digit octal field.
    pub size: u64,
    /// At most [`NAME_FIELD_BYTES`]; the caller cuts a longer target.
    pub linkname: &'a [u8],
}

impl Header<'_> {
    /// The canonical block: numeric owners 0, [`CANONICAL_MTIME_SECS`], POSIX
    /// magic, an empty prefix, and the checksum.
    pub fn to_block(&self) -> [u8; BLOCK_BYTES] {
        let mut block = [0; BLOCK_BYTES];
        put_bytes(&mut block[NAME], self.name);
        put_octal(&mut block[MODE], u64::from(self.mode));
        put_octal(&mut block[UID], 0);
        put_octal(&mut block[GID], 0);
        put_octal(&mut block[SIZE], self.size);
        put_octal(&mut block[MTIME], CANONICAL_MTIME_SECS);
        block[TYPEFLAG] = self.typeflag;
        put_bytes(&mut block[LINKNAME], self.linkname);
        block[MAGIC].copy_from_slice(POSIX_MAGIC);
        block[VERSION].copy_from_slice(b"00");
        put_octal(&mut block[DEVMAJOR], 0);
        put_octal(&mut block[DEVMINOR], 0);
        seal(&mut block);
        block
    }
}

/// Write the unsigned checksum of `block` into its checksum field.
pub fn seal(block: &mut [u8; BLOCK_BYTES]) {
    let (unsigned, _) = checksums(block);
    block[CHECKSUM].copy_from_slice(format!("{unsigned:06o}\0 ").as_bytes());
}

/// The header of a POSIX or a GNU archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Posix,
    Gnu,
}

/// A parsed header block, before any extended header is applied.
pub struct RawHeader {
    pub name: Vec<u8>,
    pub prefix: Vec<u8>,
    pub linkname: Vec<u8>,
    pub typeflag: u8,
    pub mode: u64,
    pub size: u64,
    pub format: Format,
}

impl RawHeader {
    /// The entry path the header fields spell: the POSIX prefix joined to the
    /// name, or the name alone for GNU, which reuses the prefix bytes.
    pub fn path(&self) -> Vec<u8> {
        if self.format == Format::Gnu || self.prefix.is_empty() {
            return self.name.clone();
        }
        let mut path = self.prefix.clone();
        path.push(b'/');
        path.extend_from_slice(&self.name);
        path
    }
}

/// How many zero bytes follow `len` bytes of content to fill its last block.
pub fn padding_len(len: u64) -> usize {
    let tail = usize::try_from(len % BLOCK_BYTES as u64).unwrap_or_default();
    (BLOCK_BYTES - tail) % BLOCK_BYTES
}

/// The next chunk to copy: `left` bytes, at most a buffer of `capacity`.
pub fn chunk_len(left: u64, capacity: usize) -> usize {
    usize::try_from(left).map_or(capacity, |left| left.min(capacity))
}

/// Whether `block` is all zeros, the end-of-archive marker.
pub fn is_zero(block: &[u8; BLOCK_BYTES]) -> bool {
    block.iter().all(|&byte| byte == 0)
}

/// The raw name field, for naming a block that fails to parse.
pub fn name_field(block: &[u8; BLOCK_BYTES]) -> &[u8] {
    until_nul(&block[NAME])
}

/// Parse a non-zero header block.
///
/// # Errors
///
/// [`Refusal::BadChecksum`] when the stored checksum is neither the signed
/// nor the unsigned sum, [`Refusal::UnknownFormat`] for a magic that is not
/// POSIX or GNU, and [`Refusal::BadNumber`] for a malformed numeric field.
pub fn parse(block: &[u8; BLOCK_BYTES]) -> Result<RawHeader, Refusal> {
    let stored = number(&block[CHECKSUM])?;
    let (unsigned, signed) = checksums(block);
    if stored != u64::from(unsigned) && i64::try_from(stored).ok() != Some(i64::from(signed)) {
        return Err(Refusal::BadChecksum);
    }
    let format = match &block[MAGIC] {
        POSIX_MAGIC => Format::Posix,
        GNU_MAGIC => Format::Gnu,
        _ => return Err(Refusal::UnknownFormat),
    };
    Ok(RawHeader {
        name: until_nul(&block[NAME]).to_vec(),
        prefix: until_nul(&block[PREFIX]).to_vec(),
        linkname: until_nul(&block[LINKNAME]).to_vec(),
        typeflag: block[TYPEFLAG],
        mode: number(&block[MODE])?,
        size: number(&block[SIZE])?,
        format,
    })
}

/// The unsigned and signed sums of `block` with its checksum field read as
/// eight spaces. Historic writers used either.
fn checksums(block: &[u8; BLOCK_BYTES]) -> (u32, i32) {
    block.iter().enumerate().fold((0, 0), |(unsigned, signed), (index, &byte)| {
        let byte = if CHECKSUM.contains(&index) {
            b' '
        } else {
            byte
        };
        (unsigned + u32::from(byte), signed + i32::from(byte.cast_signed()))
    })
}

fn put_bytes(field: &mut [u8], bytes: &[u8]) {
    field[..bytes.len()].copy_from_slice(bytes);
}

/// Zero-padded octal filling every byte of `field` but the last, which stays NUL.
fn put_octal(field: &mut [u8], value: u64) {
    let digits = format!("{value:0width$o}", width = field.len() - 1);
    field[..digits.len()].copy_from_slice(digits.as_bytes());
}

fn until_nul(field: &[u8]) -> &[u8] {
    field.iter().position(|&byte| byte == 0).map_or(field, |end| &field[..end])
}

/// A numeric field: GNU base-256 when the high bit of the first byte is set,
/// otherwise octal with optional leading spaces and NUL or space terminators.
fn number(field: &[u8]) -> Result<u64, Refusal> {
    match field.split_first() {
        Some((&first, rest)) if first & 0x80 != 0 => base256(first, rest),
        _ => octal(field),
    }
}

fn base256(first: u8, rest: &[u8]) -> Result<u64, Refusal> {
    if first & 0x40 != 0 {
        return Err(Refusal::BadNumber);
    }
    rest.iter().try_fold(u64::from(first & 0x3f), |value, &byte| {
        value.checked_mul(256).and_then(|value| value.checked_add(u64::from(byte))).ok_or(Refusal::BadNumber)
    })
}

fn octal(field: &[u8]) -> Result<u64, Refusal> {
    let start = field.iter().position(|&byte| byte != b' ').unwrap_or(field.len());
    let digits = &field[start..];
    let end = digits.iter().position(|byte| !(b'0'..=b'7').contains(byte)).unwrap_or(digits.len());
    if !digits[end..].iter().all(|&byte| byte == 0 || byte == b' ') {
        return Err(Refusal::BadNumber);
    }
    digits[..end].iter().try_fold(0u64, |value, &digit| {
        value.checked_mul(8).and_then(|value| value.checked_add(u64::from(digit - b'0'))).ok_or(Refusal::BadNumber)
    })
}
