//! `StubVendor`: a loopback HTTP server that answers a model vendor's
//! requests from a reply function, so a scenario or a benchmark drives a
//! program that dials a vendor without a real model behind it.

use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{Builder, Scope, ScopedJoinHandle};
use std::time::Duration;

/// How long the stub waits on one connection's request bytes, so a client
/// that dials and never writes cannot hold the accept thread.
const READ_PATIENCE: Duration = Duration::from_secs(45);

/// One request the stub served: its head lines and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StubRequest {
    /// The request line and header lines, each ending `\r\n`, without the
    /// blank line that ends the head.
    pub head: String,
    /// Exactly the `Content-Length` bytes that followed the head.
    pub body: Vec<u8>,
}

/// A loopback HTTP server on `127.0.0.1` that answers every request
/// `200 OK` with the JSON body its reply function returns for it, and
/// records each request it served.
///
/// It accepts on a named thread of the caller's [`Scope`], blocking in
/// `accept`, so it adds no poll latency to what a benchmark measures. It
/// serves one request per connection (`Connection: close`), in arrival
/// order, and records a request before it writes the reply, so a caller
/// that has its answer finds the request in [`StubVendor::served`]. Dropping
/// it stops the accept thread and joins it.
pub struct StubVendor<'scope> {
    addr: SocketAddr,
    served: Arc<Mutex<Vec<StubRequest>>>,
    stop: Arc<AtomicBool>,
    accept: Option<ScopedJoinHandle<'scope, ()>>,
}

impl<'scope> StubVendor<'scope> {
    /// Bind an ephemeral loopback port and serve it on a thread of `scope`,
    /// answering each request with the body `reply` returns for it.
    ///
    /// # Errors
    ///
    /// Fails when the port cannot be bound or the OS refuses the thread.
    pub fn start<'env>(
        scope: &'scope Scope<'scope, 'env>,
        reply: impl Fn(&StubRequest) -> Vec<u8> + Send + 'scope,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let served = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let accept = {
            let served = Arc::clone(&served);
            let stop = Arc::clone(&stop);
            Builder::new().name("stub-vendor".to_owned()).spawn_scoped(scope, move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    // A connection that fails before its reply is written
                    // leaves its client without an answer, which the client
                    // reports; the stub serves the next one.
                    let _ = stream.and_then(|stream| serve(&stream, &reply, &served));
                }
            })?
        };
        Ok(Self { addr, served, stop, accept: Some(accept) })
    }

    /// The responses endpoint a turn input posts to:
    /// `http://127.0.0.1:<port>/v1/responses`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}/v1/responses", self.addr)
    }

    /// Every request served so far, in arrival order.
    #[must_use]
    pub fn served(&self) -> Vec<StubRequest> {
        self.served.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Drop for StubVendor<'_> {
    /// Set the stop flag, then dial the stub once so its blocked `accept`
    /// returns and sees it, and join the thread. When that dial fails the
    /// thread is left to the scope, which joins it at its end.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if TcpStream::connect(self.addr).is_ok()
            && let Some(accept) = self.accept.take()
        {
            let _ = accept.join();
        }
    }
}

/// Read one request from `stream`, record it in `served`, and answer it
/// `200 OK` with the body `reply` returns for it.
fn serve(
    mut stream: &TcpStream,
    reply: &impl Fn(&StubRequest) -> Vec<u8>,
    served: &Mutex<Vec<StubRequest>>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(READ_PATIENCE))?;
    let request = read_request(stream)?;
    let body = reply(&request);
    served.lock().unwrap_or_else(PoisonError::into_inner).push(request);

    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

/// Read one request: the head up to its blank line, then exactly
/// `Content-Length` body bytes.
fn read_request(stream: &TcpStream) -> io::Result<StubRequest> {
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(ErrorKind::UnexpectedEof.into());
        }
        if line == "\r\n" {
            break;
        }
        head.push_str(&line);
    }

    let length = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(Ok(0), |(_, value)| value.trim().parse().map_err(|_| io::Error::from(ErrorKind::InvalidData)))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(StubRequest { head, body })
}
