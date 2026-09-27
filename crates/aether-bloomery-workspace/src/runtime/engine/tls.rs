//! Mutual TLS for a `tcp://` endpoint (ADR-0237 decision 8).
//!
//! The client config is built once, at boot, from the three configured PEM
//! files. Its only trust root is the configured CA: no platform store and no
//! bundled roots, so nothing ambient decides which daemon is trusted. The
//! ring provider is passed explicitly, so no process-wide default is read.

use std::fmt;
use std::sync::Arc;

use rustls::crypto::ring;
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};

use super::{EndpointError, TLS_CA_FILE_KEY, TLS_CERT_FILE_KEY, TLS_KEY_FILE_KEY};

/// A `tcp://` endpoint and the client config every connection to it shares.
#[derive(Clone)]
pub struct TlsEndpoint {
    /// A DNS name or an IP literal, an IPv6 one without its brackets.
    pub host: String,
    pub port: u16,
    /// `host` as the name the handshake sends and verifies.
    pub server_name: ServerName<'static>,
    pub config: Arc<ClientConfig>,
}

impl fmt::Debug for TlsEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsEndpoint").field("host", &self.host).field("port", &self.port).finish_non_exhaustive()
    }
}

/// The three configured PEM file paths.
#[derive(Clone, Copy)]
pub struct TlsFiles<'a> {
    /// The CA the daemon's certificate must chain to.
    pub ca: &'a str,
    /// The client certificate chain, leaf first.
    pub cert: &'a str,
    /// The client certificate's private key.
    pub key: &'a str,
}

/// Read the three files and build the client config.
///
/// # Errors
///
/// [`EndpointError`] naming the key and path of a file that cannot be read,
/// holds no certificate or no private key, or that rustls refuses.
pub fn client_config(files: TlsFiles<'_>) -> Result<Arc<ClientConfig>, EndpointError> {
    let mut roots = RootCertStore::empty();
    for ca in certificates(TLS_CA_FILE_KEY, files.ca)? {
        roots.add(ca).map_err(|error| {
            EndpointError::new(TLS_CA_FILE_KEY, files.ca, format!("rustls refuses a CA certificate: {error}"))
        })?;
    }
    let chain = certificates(TLS_CERT_FILE_KEY, files.cert)?;
    let key = PrivateKeyDer::from_pem_file(files.key).map_err(|error| match error {
        pem::Error::NoItemsFound => EndpointError::new(TLS_KEY_FILE_KEY, files.key, "the file holds no private key"),
        other => EndpointError::new(TLS_KEY_FILE_KEY, files.key, format!("reading a PEM private key: {other}")),
    })?;

    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| {
            EndpointError::new(TLS_CA_FILE_KEY, files.ca, format!("rustls offers no safe protocol version: {error}"))
        })?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .map_err(|error| {
            EndpointError::new(TLS_KEY_FILE_KEY, files.key, format!("rustls refuses the client key: {error}"))
        })?;
    Ok(Arc::new(config))
}

/// Every certificate in the PEM file at `path`, at least one.
fn certificates(key: &'static str, path: &str) -> Result<Vec<CertificateDer<'static>>, EndpointError> {
    let read = |error: pem::Error| EndpointError::new(key, path, format!("reading PEM certificates: {error}"));
    let certificates =
        CertificateDer::pem_file_iter(path).map_err(read)?.collect::<Result<Vec<_>, _>>().map_err(read)?;
    if certificates.is_empty() {
        return Err(EndpointError::new(key, path, "the file holds no certificate"));
    }
    Ok(certificates)
}
