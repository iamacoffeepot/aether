//! The private Docker Engine API client (ADR-0237 decision 8).
//!
//! A small hand-rolled HTTP/1.1 client that runs on the actor's worker thread
//! with no async runtime. Every request opens its own connection and sends
//! `Connection: close`, except a stdin attach, which the daemon hijacks; a
//! request body is a small JSON document or a chunked stream, and a response
//! body is framed by `Content-Length`, by chunked transfer encoding, or by
//! the connection closing. The endpoint and its TLS files come only from
//! [`crate::WorkspaceConfig`], never from `DOCKER_HOST` or any other
//! `DOCKER_*` variable, and the API version is pinned in every request path.
//! A `tcp://` daemon is always spoken to over mutual TLS whose only trust
//! root is the configured CA.

mod api;
mod http;
pub mod logs;
mod progress;
pub mod stats;
mod tls;
mod transport;

#[cfg(all(test, unix))]
mod tests;

use std::error::Error;
use std::fmt;
use std::io;
#[cfg(unix)]
use std::path::PathBuf;

use rustls::pki_types::ServerName;

pub use api::{ContainerId, VolumeName, Waited};
pub use tls::TlsEndpoint;
pub use transport::Transport;

use crate::{DEFAULT_ENDPOINT, WorkspaceConfig};
use tls::TlsFiles;

/// The config key naming the endpoint.
const ENDPOINT_KEY: &str = "AETHER_WORKSPACE_ENDPOINT";

/// The config key naming the PEM file of the CA a `tcp://` daemon's
/// certificate must chain to.
const TLS_CA_FILE_KEY: &str = "AETHER_WORKSPACE_TLS_CA_FILE";

/// The config key naming the PEM file of the client certificate chain.
const TLS_CERT_FILE_KEY: &str = "AETHER_WORKSPACE_TLS_CERT_FILE";

/// The config key naming the PEM file of the client certificate's key.
const TLS_KEY_FILE_KEY: &str = "AETHER_WORKSPACE_TLS_KEY_FILE";

/// Where the daemon listens: a Unix socket on Unix, or a TCP address spoken
/// to over mutual TLS anywhere. The Windows named pipe is not supported yet
/// (#6775).
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// `unix://<absolute path>`.
    #[cfg(unix)]
    Unix(PathBuf),
    /// `tcp://<host>:<port>`, always TLS with a client certificate.
    Tcp(TlsEndpoint),
}

/// A configured endpoint's address, before its TLS files are checked.
enum Address {
    #[cfg(unix)]
    Unix(PathBuf),
    Tcp {
        host: String,
        port: u16,
    },
}

impl Endpoint {
    /// The endpoint `config` names, its TLS client config built when it is
    /// `tcp://`. An unset endpoint is [`DEFAULT_ENDPOINT`].
    ///
    /// # Errors
    ///
    /// [`EndpointError`] naming the key to fix: a scheme other than `unix://`
    /// or `tcp://`, a relative socket path, a `tcp://` address without a
    /// port, a `tcp://` endpoint missing any of its three TLS files, a TLS
    /// file set beside another scheme, or a TLS file that cannot be read or
    /// that rustls refuses.
    pub fn from_config(config: &WorkspaceConfig) -> Result<Self, EndpointError> {
        let value = config.endpoint.as_deref().unwrap_or(DEFAULT_ENDPOINT);
        let files = [
            (TLS_CA_FILE_KEY, config.tls_ca_file.as_deref()),
            (TLS_CERT_FILE_KEY, config.tls_cert_file.as_deref()),
            (TLS_KEY_FILE_KEY, config.tls_key_file.as_deref()),
        ];
        match parse(value)? {
            #[cfg(unix)]
            Address::Unix(path) => {
                if let Some((key, Some(path))) = files.into_iter().find(|(_, path)| path.is_some()) {
                    return Err(EndpointError::new(key, path, "a TLS file applies only to a tcp:// endpoint"));
                }
                Ok(Self::Unix(path))
            }
            Address::Tcp { host, port } => {
                let server_name = ServerName::try_from(host.clone()).map_err(|error| {
                    EndpointError::new(ENDPOINT_KEY, value, format!("the host is not a name: {error}"))
                })?;
                let [ca, cert, key] = files.map(|(key, path)| {
                    path.ok_or_else(|| EndpointError::new(key, "", "a tcp:// endpoint requires mutual TLS"))
                });
                let config = tls::client_config(TlsFiles { ca: ca?, cert: cert?, key: key? })?;
                Ok(Self::Tcp(TlsEndpoint { host, port, server_name, config }))
            }
        }
    }
}

/// Parse an endpoint's scheme and address.
fn parse(value: &str) -> Result<Address, EndpointError> {
    let refuse = |reason| EndpointError::new(ENDPOINT_KEY, value, reason);
    if let Some(address) = value.strip_prefix("tcp://") {
        let (host, port) = match address.strip_prefix('[') {
            Some(bracketed) => bracketed
                .split_once("]:")
                .ok_or_else(|| refuse("an IPv6 host needs brackets and a port: tcp://[<address>]:<port>"))?,
            None => address
                .rsplit_once(':')
                .filter(|(host, _)| !host.contains(':'))
                .ok_or_else(|| refuse("the address needs a port: tcp://<host>:<port>"))?,
        };
        if host.is_empty() {
            return Err(refuse("the address needs a host: tcp://<host>:<port>"));
        }
        let port = port
            .parse::<u16>()
            .ok()
            .filter(|&port| port != 0)
            .ok_or_else(|| refuse("the port must be a number from 1 to 65535"))?;
        return Ok(Address::Tcp { host: host.to_owned(), port });
    }
    #[cfg(unix)]
    if let Some(path) = value.strip_prefix("unix://") {
        if !path.starts_with('/') {
            return Err(refuse("the socket path must be absolute"));
        }
        return Ok(Address::Unix(PathBuf::from(path)));
    }
    Err(refuse(if cfg!(unix) {
        "only the unix:// and tcp:// schemes are supported"
    } else {
        "only the tcp:// scheme is supported on this platform"
    }))
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            #[cfg(unix)]
            Self::Unix(ref path) => write!(f, "unix://{}", path.display()),
            Self::Tcp(ref tls) if tls.host.contains(':') => write!(f, "tcp://[{}]:{}", tls.host, tls.port),
            Self::Tcp(ref tls) => write!(f, "tcp://{}:{}", tls.host, tls.port),
        }
    }
}

/// Why [`Endpoint::from_config`] refused the configuration: the key to fix,
/// its value (empty when unset), and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointError {
    key: &'static str,
    value: String,
    reason: String,
}

impl EndpointError {
    fn new(key: &'static str, value: &str, reason: impl Into<String>) -> Self {
        Self { key, value: value.to_owned(), reason: reason.into() }
    }
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { key, value, reason } = self;
        if value.is_empty() {
            write!(f, "{key} is unset, and the Engine API endpoint needs it: {reason}")
        } else {
            write!(f, "{key}={value} does not configure a usable Engine API endpoint: {reason}")
        }
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
    /// The daemon's socket did not accept a connection, or its TLS
    /// handshake failed.
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

/// Why a call whose request body the caller streams failed.
#[derive(Debug)]
pub enum UploadError<E> {
    /// The call itself failed, including a daemon that refused the body.
    Engine(EngineError),
    /// The caller's body writer failed.
    Body(E),
}

impl From<io::Error> for EngineError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
