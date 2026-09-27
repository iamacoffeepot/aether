//! A container's stats stream, folded into its peak memory.
//!
//! `GET /containers/{id}/stats?stream=true` answers one JSON object per line,
//! about once a second, while the container runs. A sample's memory in use
//! is `memory_stats.usage` less the reclaimable file cache: `inactive_file`
//! under cgroup v2, `total_inactive_file` under v1. Under v1 the daemon also
//! reports `max_usage`, the kernel's own high-water mark; v2 reports none,
//! so there the peak is only as fine as the samples.

use std::io::{self, BufRead, BufReader, ErrorKind, Read};

use serde_json::Value;

use super::http::Body;
use super::transport::{Closer, Transport};

/// The longest sample line accepted.
const MAX_LINE_BYTES: u64 = 1 << 20;

/// An open stats stream: the response body, and a second handle on its
/// socket that can shut it down from another thread.
pub struct StatsStream {
    body: Body<Transport>,
    closer: Closer,
}

impl StatsStream {
    pub(super) const fn new(body: Body<Transport>, closer: Closer) -> Self {
        Self { body, closer }
    }

    /// The samples, for a reader thread, and the handle that ends them.
    pub fn split(self) -> (Body<Transport>, StatsStop) {
        (self.body, StatsStop(self.closer))
    }
}

/// Ends a stats stream whose samples another thread is reading.
pub struct StatsStop(Closer);

impl StatsStop {
    /// Shut the stream's socket down, so a read blocked on it returns.
    pub fn stop(&self) {
        // Ignored: a stream the daemon already closed needs no shutdown.
        let _ = self.0.shutdown();
    }
}

/// Read samples until the stream ends and answer the peak memory in use, or
/// `None` when no sample reported any.
///
/// A stream cut short ends the samples rather than failing them: stopping a
/// stream cuts it, so the peak so far is the answer.
///
/// # Errors
///
/// A line over 1 MiB ([`ErrorKind::InvalidData`]), a line that is not JSON,
/// or a failed read other than the cut.
pub fn peak_bytes(samples: impl Read) -> io::Result<Option<u64>> {
    let mut reader = BufReader::new(samples);
    let mut line = Vec::new();
    let mut peak: Option<u64> = None;
    loop {
        line.clear();
        match (&mut reader).take(MAX_LINE_BYTES + 1).read_until(b'\n', &mut line) {
            Ok(0) => return Ok(peak),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(peak),
            Err(error) => return Err(error),
        }
        if line.len() as u64 > MAX_LINE_BYTES {
            return Err(io::Error::new(ErrorKind::InvalidData, "a stats line longer than 1 MiB"));
        }
        if line.trim_ascii().is_empty() {
            continue;
        }
        let sample = serde_json::from_slice::<Value>(&line).map_err(io::Error::other)?;
        peak = peak.max(in_use(&sample)).filter(|&bytes| bytes > 0);
    }
}

/// The most memory one sample reports in use: usage less the inactive file
/// cache, or the high-water mark when it is higher.
fn in_use(sample: &Value) -> Option<u64> {
    let memory = sample.get("memory_stats")?;
    let field = |pointer: &str| memory.pointer(pointer).and_then(Value::as_u64);
    let inactive = field("/stats/inactive_file").or_else(|| field("/stats/total_inactive_file")).unwrap_or(0);
    let usage = field("/usage").map(|usage| usage.saturating_sub(inactive));
    usage.max(field("/max_usage"))
}
