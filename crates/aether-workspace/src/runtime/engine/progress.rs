//! A pull's progress stream: one JSON object per line.
//!
//! `POST /images/create` answers 200 as soon as the pull starts, then streams
//! progress. A failure arrives as an object carrying `error` (and
//! `errorDetail`) inside that 200 stream, so the pull has only succeeded once
//! the stream ends without one.

use std::io::{BufRead, BufReader, Read};

use serde_json::Value;

use super::EngineError;

/// The longest progress line accepted.
const MAX_LINE_BYTES: u64 = 1 << 20;

/// Read a pull's progress stream to its end.
///
/// # Errors
///
/// [`EngineError::Pull`] for an object carrying `error`,
/// [`EngineError::Json`] for a line that is not JSON,
/// [`EngineError::Protocol`] for a line past the bound, and
/// [`EngineError::Io`] when reading fails or the stream is cut.
pub fn drain(body: impl Read) -> Result<(), EngineError> {
    let mut reader = BufReader::new(body);
    let mut line = Vec::new();
    loop {
        line.clear();
        if (&mut reader).take(MAX_LINE_BYTES + 1).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.len() as u64 > MAX_LINE_BYTES {
            return Err(EngineError::Protocol("a pull progress line longer than 1 MiB".to_owned()));
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        let object = serde_json::from_slice::<Value>(&line).map_err(EngineError::Json)?;
        if let Some(error) = object.get("error") {
            let message = error.as_str().map_or_else(|| error.to_string(), str::to_owned);
            return Err(EngineError::Pull(message));
        }
    }
}
