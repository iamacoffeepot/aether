//! The private Docker Engine API client (ADR-0237 decision 8).
//!
//! A small hand-rolled HTTP/1.1 client that runs on the actor's worker thread
//! with no async runtime. Every request opens its own connection and sends
//! `Connection: close`; a response body is framed by `Content-Length`, by
//! chunked transfer encoding, or by the connection closing. The endpoint comes
//! only from [`crate::WorkspaceConfig`], never from `DOCKER_HOST`, and the API
//! version is pinned in every request path.
//!
//! The client covers only the endpoints import uses; the run endpoints arrive
//! with their consumer.

mod api;
mod http;
mod progress;
mod transport;

#[cfg(all(test, unix))]
mod tests;

use std::error::Error;
use std::fmt;
use std::io;
#[cfg(unix)]
use std::path::PathBuf;

pub use api::ContainerId;
use transport::Transport;

/// The config key an endpoint refusal names.
const ENDPOINT_KEY: &str = "AETHER_WORKSPACE_ENDPOINT";

/// Where the daemon listens. Only a Unix socket, and only on Unix, until TCP
/// with TLS and the Windows named pipe arrive (#6721).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `unix://<absolute path>`.
    #[cfg(unix)]
    Unix(PathBuf),
}

impl Endpoint {
    /// Parse a configured endpoint.
    ///
    /// # Errors
    ///
    /// [`EndpointError`] naming the key, the value, and why it is refused: a
    /// scheme other than `unix://`, a socket path that is not absolute, or any
    /// value at all off Unix.
    pub fn parse(value: &str) -> Result<Self, EndpointError> {
        let refuse = |reason| EndpointError { value: value.to_owned(), reason };
        #[cfg(unix)]
        {
            let path = value.strip_prefix("unix://").ok_or_else(|| refuse("only the unix:// scheme is supported"))?;
            if !path.starts_with('/') {
                return Err(refuse("the socket path must be absolute"));
            }
            Ok(Self::Unix(PathBuf::from(path)))
        }
        #[cfg(not(unix))]
        {
            Err(refuse("no Engine API transport is supported on this platform yet"))
        }
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref path) => write!(f, "unix://{}", path.display()),
        }
    }
}

/// Why [`Endpoint::parse`] refused a configured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointError {
    value: String,
    reason: &'static str,
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ENDPOINT_KEY}={} is not a usable Engine API endpoint: {}", self.value, self.reason)
    }
}

impl Error for EndpointError {}

/// A client for one daemon. Cheap to clone; it holds no connection.
#[derive(Debug, Clone)]
pub struct Engine {
    endpoint: Endpoint,
}

impl Engine {
    #[must_use]
    pub fn new(endpoint: Endpoint) -> Self {
        Self { endpoint }
    }

    /// Open a fresh connection for one request.
    fn connect(&self) -> Result<Transport, EngineError> {
        Transport::connect(&self.endpoint)
            .map_err(|source| EngineError::Connect { endpoint: self.endpoint.to_string(), source })
    }
}

/// Why an Engine API call failed.
#[derive(Debug)]
pub enum EngineError {
    /// The daemon's socket did not accept a connection.
    Connect { endpoint: String, source: io::Error },
    /// Reading or writing the connection failed, or it timed out.
    Io(io::Error),
    /// The daemon's response was not HTTP/1.1 this client understands.
    Protocol(String),
    /// The daemon answered a non-2xx status.
    Status { status: u16, message: String },
    /// The pull's progress stream carried an error object.
    Pull(String),
    /// A JSON body did not parse as the endpoint's documented shape.
    Json(serde_json::Error),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect { endpoint, source } => write!(f, "connecting to the Docker daemon at {endpoint}: {source}"),
            Self::Io(error) => write!(f, "talking to the Docker daemon: {error}"),
            Self::Protocol(detail) => write!(f, "malformed Engine API response: {detail}"),
            Self::Status { status, message } => write!(f, "the Docker daemon answered {status}: {message}"),
            Self::Pull(message) => write!(f, "the image pull failed: {message}"),
            Self::Json(error) => write!(f, "malformed Engine API JSON: {error}"),
        }
    }
}

impl Error for EngineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Connect { source, .. } => Some(source),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Protocol(_) | Self::Status { .. } | Self::Pull(_) => None,
        }
    }
}

impl From<io::Error> for EngineError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
