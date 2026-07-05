//! A concurrent, closed-loop HTTP/1.1 load generator (pure `std`).
//!
//! `run` spawns `concurrency` worker threads, each holding one keep-alive
//! connection and looping request→await-response→repeat for `duration`. Every
//! completed request records an end-to-end latency (write start → last body
//! byte read); a connection error reconnects and is counted. The merged
//! samples yield req/s and latency percentiles — the same shape the mail-perf
//! harness reports, but measured over the wire from the client's side rather
//! than harvested from the trace ring.
//!
//! This runs in the *driver* process, separate from the forked server, so the
//! generator's threads do not contend with the server's worker pool.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};

/// One load run's parameters.
#[derive(Clone)]
pub struct LoadConfig {
    /// Server address, e.g. `"127.0.0.1"`.
    pub host: String,
    /// Server port (the OS-assigned port the forked server reported).
    pub port: u16,
    /// Number of concurrent keep-alive connections / worker threads.
    pub concurrency: usize,
    /// How long to sustain load.
    pub duration: Duration,
    /// Request path, e.g. `"/"`.
    pub path: String,
}

/// One load run's outcome. `latencies_nanos` is sorted ascending.
pub struct LoadResult {
    pub concurrency: usize,
    pub total: u64,
    pub errors: u64,
    pub elapsed: Duration,
    pub latencies_nanos: Vec<u64>,
}

impl LoadResult {
    /// Completed requests per second over the wall-clock run.
    #[must_use]
    pub fn requests_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.total as f64 / secs
        } else {
            0.0
        }
    }

    /// Nearest-rank percentile (`q` in `0.0..=1.0`) over the sorted latency
    /// samples, in nanoseconds. `0` when there are no samples.
    #[must_use]
    pub fn percentile_nanos(&self, q: f64) -> u64 {
        let n = self.latencies_nanos.len();
        if n == 0 {
            return 0;
        }
        let idx = (((n - 1) as f64) * q).round() as usize;
        self.latencies_nanos[idx.min(n - 1)]
    }
}

/// Per-worker accumulator.
struct WorkerStats {
    latencies_nanos: Vec<u64>,
    completed: u64,
    errors: u64,
}

/// Run one load test to completion and return the merged result.
#[must_use]
pub fn run(config: &LoadConfig) -> LoadResult {
    let start = Instant::now();
    let deadline = start + config.duration;

    let handles: Vec<_> = (0..config.concurrency)
        .map(|_| {
            let cfg = config.clone();
            thread::spawn(move || worker(&cfg, deadline))
        })
        .collect();

    let mut latencies_nanos = Vec::new();
    let mut total = 0u64;
    let mut errors = 0u64;
    for handle in handles {
        // A panicked worker (should not happen) drops its samples but never
        // aborts the run — treat it as contributing nothing.
        if let Ok(stats) = handle.join() {
            latencies_nanos.extend_from_slice(&stats.latencies_nanos);
            total += stats.completed;
            errors += stats.errors;
        }
    }
    let elapsed = start.elapsed();
    latencies_nanos.sort_unstable();

    LoadResult {
        concurrency: config.concurrency,
        total,
        errors,
        elapsed,
        latencies_nanos,
    }
}

/// One worker: hold a keep-alive connection and drive requests until the
/// deadline, reconnecting on any I/O error.
fn worker(config: &LoadConfig, deadline: Instant) -> WorkerStats {
    let addr = format!("{}:{}", config.host, config.port);
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        config.path, config.host
    );
    let request = request.as_bytes();

    let mut stats = WorkerStats {
        latencies_nanos: Vec::new(),
        completed: 0,
        errors: 0,
    };

    let mut conn: Option<BufReader<TcpStream>> = None;
    while Instant::now() < deadline {
        // Establish (or re-establish) the keep-alive connection.
        let reader = match conn.take() {
            Some(reader) => reader,
            None => match connect(&addr) {
                Ok(reader) => reader,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            },
        };
        conn = Some(reader);
        let reader = conn.as_mut().expect("connection just set");

        let sent = Instant::now();
        if reader.get_mut().write_all(request).is_err() {
            stats.errors += 1;
            conn = None;
            continue;
        }
        match read_response(reader) {
            Ok(true) => {
                stats.latencies_nanos.push(elapsed_nanos(sent));
                stats.completed += 1;
                // `keep_alive == true` leaves `conn` in place for reuse.
            }
            Ok(false) => {
                // Server signalled `Connection: close` — count the request,
                // then drop the connection so the next iteration reconnects.
                stats.latencies_nanos.push(elapsed_nanos(sent));
                stats.completed += 1;
                conn = None;
            }
            Err(_) => {
                stats.errors += 1;
                conn = None;
            }
        }
    }
    stats
}

/// Open a TCP connection with `TCP_NODELAY` set (latency, not batching).
fn connect(addr: &str) -> std::io::Result<BufReader<TcpStream>> {
    let stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    Ok(BufReader::new(stream))
}

/// Read one full HTTP/1.1 response off `reader` (head + body), consuming
/// exactly the framed bytes so the connection is left positioned at the next
/// response. Returns `Ok(true)` to keep the connection alive, `Ok(false)` when
/// the response asked to close it. Handles `Content-Length` and
/// `Transfer-Encoding: chunked`; a bodyless status is length 0.
fn read_response(reader: &mut BufReader<TcpStream>) -> std::io::Result<bool> {
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut keep_alive = true;
    let mut saw_status = false;

    // Head: read header lines until the blank line.
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            // EOF before a complete head — connection died mid-response.
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of head
        }
        if !saw_status {
            // Status line, e.g. "HTTP/1.1 200 OK" — HTTP/1.0 defaults to close.
            saw_status = true;
            if trimmed.starts_with("HTTP/1.0") {
                keep_alive = false;
            }
            continue;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            match name.as_str() {
                "content-length" => content_length = value.parse::<usize>().ok(),
                "transfer-encoding" => {
                    if value.eq_ignore_ascii_case("chunked") {
                        chunked = true;
                    }
                }
                "connection" => {
                    if value.eq_ignore_ascii_case("close") {
                        keep_alive = false;
                    }
                }
                _ => {}
            }
        }
    }

    // Body.
    if chunked {
        read_chunked_body(reader)?;
    } else {
        let len = content_length.unwrap_or(0);
        let mut remaining = len;
        let mut buf = [0u8; 8192];
        while remaining > 0 {
            let want = remaining.min(buf.len());
            let read = reader.read(&mut buf[..want])?;
            if read == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            remaining -= read;
        }
    }
    Ok(keep_alive)
}

/// Consume a `Transfer-Encoding: chunked` body up to and including the
/// zero-length terminating chunk and its trailing CRLF.
fn read_chunked_body(reader: &mut BufReader<TcpStream>) -> std::io::Result<()> {
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        let size = usize::from_str_radix(size_line.trim(), 16)
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        if size == 0 {
            // Terminating chunk; consume the final CRLF (and any trailers).
            let mut trailer = String::new();
            loop {
                trailer.clear();
                let read = reader.read_line(&mut trailer)?;
                if read == 0 || trailer.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            return Ok(());
        }
        // Chunk data + trailing CRLF.
        let mut remaining = size + 2;
        let mut buf = [0u8; 8192];
        while remaining > 0 {
            let want = remaining.min(buf.len());
            let read = reader.read(&mut buf[..want])?;
            if read == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            remaining -= read;
        }
    }
}

/// Elapsed nanoseconds since `since`, saturating into a `u64`.
fn elapsed_nanos(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
