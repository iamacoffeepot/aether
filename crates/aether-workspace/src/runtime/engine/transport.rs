//! The one private transport enum: a byte stream to the daemon.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::Endpoint;

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
}

impl Transport {
    /// Connect to `endpoint` with read and write timeouts set.
    pub fn connect(endpoint: &Endpoint) -> io::Result<Self> {
        match *endpoint {
            #[cfg(unix)]
            Endpoint::Unix(ref path) => {
                let stream = UnixStream::connect(path)?;
                stream.set_read_timeout(Some(IO_TIMEOUT))?;
                stream.set_write_timeout(Some(IO_TIMEOUT))?;
                Ok(Self::Unix(stream))
            }
        }
    }

    /// Bound every later read by `timeout`, at least [`MIN_READ_TIMEOUT`].
    pub fn set_read_timeout(&self, timeout: Duration) -> io::Result<()> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref stream) => stream.set_read_timeout(Some(timeout.max(MIN_READ_TIMEOUT))),
        }
    }

    /// Close the writing half, so the peer reads end of stream: how a
    /// hijacked attach tells the container its stdin has ended.
    pub fn shutdown_write(&self) -> io::Result<()> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref stream) => stream.shutdown(Shutdown::Write),
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref mut stream) => stream.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref mut stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref mut stream) => stream.flush(),
        }
    }
}
