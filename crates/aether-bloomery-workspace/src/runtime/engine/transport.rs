//! The one private transport enum: a byte stream to the daemon.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use rustls::{ClientConnection, StreamOwned};

use super::{Endpoint, TlsEndpoint};

/// The longest one read or write may block. A pull's progress stream and an
/// export's tar stream both keep bytes moving, so a stall this long is a
/// wedged daemon, not a slow one. A step's `wait` sets its own read timeout,
/// the run's remaining deadline.
const IO_TIMEOUT: Duration = Duration::from_mins(5);

/// The shortest read timeout [`Transport::set_read_timeout`] sets: a zero
/// timeout means "none" to the socket, never "now".
const MIN_READ_TIMEOUT: Duration = Duration::from_millis(1);

/// One connection to the daemon, opened per request.
pub enum Transport {
    #[cfg(unix)]
    Unix(UnixStream),
    /// A TCP connection whose mutual TLS handshake has completed.
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Transport {
    /// Connect to `endpoint` with read and write timeouts set. A TLS
    /// connection finishes its handshake here, so a daemon whose certificate
    /// the configured CA did not issue fails the connect, not a later write.
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        match *endpoint {
            #[cfg(unix)]
            Endpoint::Unix(ref path) => {
                let stream = UnixStream::connect(path)?;
                stream.set_read_timeout(Some(IO_TIMEOUT))?;
                stream.set_write_timeout(Some(IO_TIMEOUT))?;
                Ok(Self::Unix(stream))
            }
            Endpoint::Tcp(ref tls) => connect_tls(tls).map(|stream| Self::Tls(Box::new(stream))),
        }
    }

    /// Bound every later read by `timeout`, at least [`MIN_READ_TIMEOUT`].
    /// rustls hands the socket's timeout error back unchanged, so a TLS read
    /// past it fails exactly as a Unix one does.
    pub fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        let timeout = Some(timeout.max(MIN_READ_TIMEOUT));
        match *self {
            #[cfg(unix)]
            Self::Unix(ref stream) => stream.set_read_timeout(timeout),
            Self::Tls(ref stream) => stream.sock.set_read_timeout(timeout),
        }
    }

    /// Close the writing half, so the peer reads end of stream: how a
    /// hijacked attach tells the container its stdin has ended. Over TLS the
    /// `close_notify` goes first, so the peer reads a clean end rather than a
    /// truncated stream.
    pub fn shutdown_write(&mut self) -> io::Result<()> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref stream) => stream.shutdown(Shutdown::Write),
            Self::Tls(ref mut stream) => {
                stream.conn.send_close_notify();
                stream.flush()?;
                stream.sock.shutdown(Shutdown::Write)
            }
        }
    }

    /// A second handle on this connection's socket that can shut it down
    /// from another thread. rustls state cannot be cloned, so a TLS
    /// connection's handle is its TCP socket.
    pub fn closer(&self) -> io::Result<Closer> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref stream) => stream.try_clone().map(Closer::Unix),
            Self::Tls(ref stream) => stream.sock.try_clone().map(Closer::Tcp),
        }
    }
}

/// A second handle on a [`Transport`]'s socket, for shutting it down while
/// another thread reads it.
pub enum Closer {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Closer {
    /// Shut both halves of the socket down, so a read blocked on the
    /// connection returns. Over TLS no `close_notify` is sent: the reader
    /// sees a cut stream.
    pub fn shutdown(&self) -> io::Result<()> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref stream) => stream.shutdown(Shutdown::Both),
            Self::Tcp(ref stream) => stream.shutdown(Shutdown::Both),
        }
    }
}

/// Dial the first address `tls.host` resolves to that accepts, then complete
/// the handshake over it.
fn connect_tls(tls: &TlsEndpoint) -> io::Result<StreamOwned<ClientConnection, TcpStream>> {
    let mut refused = None;
    for address in (tls.host.as_str(), tls.port).to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, IO_TIMEOUT) {
            Ok(sock) => return handshake(tls, sock),
            Err(error) => refused = Some(error),
        }
    }
    Err(refused.unwrap_or_else(|| io::Error::new(ErrorKind::NotFound, format!("{} resolves to no address", tls.host))))
}

/// Set the socket's timeouts, then drive the handshake to its end.
fn handshake(tls: &TlsEndpoint, sock: TcpStream) -> io::Result<StreamOwned<ClientConnection, TcpStream>> {
    sock.set_read_timeout(Some(IO_TIMEOUT))?;
    sock.set_write_timeout(Some(IO_TIMEOUT))?;
    // Every TLS record goes out at once: a request's head is several small
    // writes, and Nagle would hold each behind the peer's delayed ACK.
    sock.set_nodelay(true)?;

    let conn = ClientConnection::new(Arc::clone(&tls.config), tls.server_name.clone()).map_err(io::Error::other)?;
    let mut stream = StreamOwned::new(conn, sock);
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock)?;
    }
    Ok(stream)
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref mut stream) => stream.read(buf),
            Self::Tls(ref mut stream) => stream.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref mut stream) => stream.write(buf),
            Self::Tls(ref mut stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref mut stream) => stream.flush(),
            Self::Tls(ref mut stream) => stream.flush(),
        }
    }
}
