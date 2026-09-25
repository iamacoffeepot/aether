//! The one private transport enum: a byte stream to the daemon.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::Endpoint;

/// The longest one read or write may block. A pull's progress stream and an
/// export's tar stream both keep bytes moving, so a stall this long is a
/// wedged daemon, not a slow one.
const IO_TIMEOUT: Duration = Duration::from_mins(5);

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
