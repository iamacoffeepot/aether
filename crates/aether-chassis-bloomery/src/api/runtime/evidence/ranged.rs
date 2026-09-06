//! Byte-addressed, line-snapped pages of a dispatch evidence file.
//!
//! Reads must never touch the file's mtime: it is the executor's live-progress
//! signal (ADR-0195 §8). Open read-only and do not write metadata.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::str;

use super::super::reads::{clamp_limit, pairs, parse_u64};
use crate::api::dto::DispatchFilePage;

/// Default page size for transcript / prompt reads.
pub const TRANSCRIPT_DEFAULT_LIMIT: u64 = 64 * 1024;
/// Hard ceiling for one transcript / prompt page.
pub const TRANSCRIPT_MAX_LIMIT: u64 = 512 * 1024;
/// Per-line cap so one huge tool result cannot starve the page.
pub const TRANSCRIPT_LINE_CAP: usize = 16 * 1024;

/// Parsed `GET /dispatches/{nonce}/transcript` (and `/prompt`) query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileQuery {
    /// Byte cursor. `None` means tail.
    pub cursor: Option<u64>,
    /// Applied page size, already clamped.
    pub limit: u64,
    /// Set when the caller named a limit above the clamp.
    pub notice: Option<String>,
}

impl FileQuery {
    /// Parse `query`. An unparseable number is a `400`.
    pub fn parse(query: &str) -> Result<Self, String> {
        let mut cursor = None;
        let mut requested = None;
        for (key, value) in pairs(query) {
            match key.as_str() {
                "cursor" => cursor = Some(parse_u64("cursor", &value)?),
                "limit" => requested = Some(parse_u64("limit", &value)?),
                _ => {}
            }
        }
        let (limit, notice) = clamp_limit(requested, TRANSCRIPT_DEFAULT_LIMIT, TRANSCRIPT_MAX_LIMIT);
        Ok(Self { cursor, limit, notice })
    }
}

/// Why a ranged read failed.
#[derive(Debug)]
pub enum RangedError {
    /// The path does not exist.
    NotFound,
    /// A filesystem error other than not-found.
    Io(io::Error),
}

/// Read one line-snapped page of `path`.
///
/// Must never touch the file's mtime: it is the executor's live-progress
/// signal (ADR-0195 §8).
pub fn read_ranged(path: &Path, cursor: Option<u64>, limit: u64) -> Result<DispatchFilePage, RangedError> {
    let mut file = File::open(path).map_err(map_open)?;
    let length = file.metadata().map_err(RangedError::Io)?.len();
    if length == 0 {
        return Ok(DispatchFilePage { lines: Vec::new(), cursor: 0, next_cursor: None, length, notice: None });
    }

    let raw_start = cursor.map_or_else(|| length.saturating_sub(limit), |offset| offset.min(length));
    let start = snap_start(&mut file, raw_start, length).map_err(RangedError::Io)?;
    if start >= length {
        return Ok(DispatchFilePage { lines: Vec::new(), cursor: start, next_cursor: None, length, notice: None });
    }

    let want = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut buf = vec![0_u8; want.saturating_add(1)];
    file.seek(SeekFrom::Start(start)).map_err(RangedError::Io)?;
    let got = file.read(&mut buf).map_err(RangedError::Io)?;
    buf.truncate(got);

    let at_eof = start + u64::try_from(got).unwrap_or(u64::MAX) >= length;
    let (mut lines, mut consumed) = page_lines(&buf, at_eof);
    if consumed == 0 && !at_eof && got > 0 {
        // No newline in the page window and the file continues: this line is
        // larger than the budget. Emit a capped prefix and skip the rest so
        // next_cursor lands on a line boundary rather than repeating `start`.
        lines.push(cap_line(&buf));
        consumed = skip_oversized_line(&mut file, got).map_err(RangedError::Io)?;
    }
    let end = start.saturating_add(consumed);
    let next_cursor = (end < length).then_some(end);
    Ok(DispatchFilePage { lines, cursor: start, next_cursor, length, notice: None })
}

fn map_open(error: io::Error) -> RangedError {
    if error.kind() == io::ErrorKind::NotFound {
        RangedError::NotFound
    } else {
        RangedError::Io(error)
    }
}

/// Snap `offset` forward to the next line start when it lands mid-line.
fn snap_start(file: &mut File, offset: u64, length: u64) -> io::Result<u64> {
    if offset == 0 || offset >= length {
        return Ok(offset.min(length));
    }
    file.seek(SeekFrom::Start(offset.saturating_sub(1)))?;
    let mut prev = [0_u8; 1];
    if file.read(&mut prev)? == 1 && prev[0] == b'\n' {
        return Ok(offset);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut byte = [0_u8; 1];
    let mut walked = offset;
    while walked < length {
        match file.read(&mut byte)? {
            0 => break,
            _ if byte[0] == b'\n' => return Ok(walked.saturating_add(1)),
            _ => walked = walked.saturating_add(1),
        }
    }
    Ok(length)
}

/// Split `buf` into complete lines. The last incomplete line is dropped unless
/// `at_eof` (the file ended without a trailing newline — still a complete
/// record for a finished prompt, not a live JSONL event). A non-EOF window
/// with no newline consumes zero; the caller skips that oversized line.
fn page_lines(buf: &[u8], at_eof: bool) -> (Vec<String>, u64) {
    let mut lines = Vec::new();
    let mut consumed = 0_u64;
    let mut start = 0_usize;
    for (index, byte) in buf.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let raw = &buf[start..=index];
        lines.push(cap_line(raw));
        consumed = consumed.saturating_add(u64::try_from(raw.len()).unwrap_or(0));
        start = index.saturating_add(1);
    }
    if at_eof && start < buf.len() {
        let raw = &buf[start..];
        lines.push(cap_line(raw));
        consumed = consumed.saturating_add(u64::try_from(raw.len()).unwrap_or(0));
    }
    (lines, consumed)
}

/// Skip the unread remainder of a line that did not fit in the page window.
///
/// `already_read` is the window that contained no newline. Returns the total
/// byte count from the page start through the terminating newline, or through
/// EOF when the file has no further newline. Reads are chunked so the line is
/// never loaded whole.
fn skip_oversized_line(file: &mut File, already_read: usize) -> io::Result<u64> {
    let mut consumed = u64::try_from(already_read).unwrap_or(u64::MAX);
    let mut chunk = [0_u8; 8192];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            return Ok(consumed);
        }
        if let Some(index) = chunk[..n].iter().position(|&byte| byte == b'\n') {
            return Ok(consumed.saturating_add(u64::try_from(index.saturating_add(1)).unwrap_or(0)));
        }
        consumed = consumed.saturating_add(u64::try_from(n).unwrap_or(0));
    }
}

fn cap_line(raw: &[u8]) -> String {
    let without_nl = raw.strip_suffix(b"\n").unwrap_or(raw);
    let without_cr = without_nl.strip_suffix(b"\r").unwrap_or(without_nl);
    if without_cr.len() <= TRANSCRIPT_LINE_CAP {
        return String::from_utf8_lossy(without_cr).into_owned();
    }
    let mut end = TRANSCRIPT_LINE_CAP;
    while end > 0 && str::from_utf8(&without_cr[..end]).is_err() {
        end -= 1;
    }
    String::from_utf8_lossy(&without_cr[..end]).into_owned()
}
