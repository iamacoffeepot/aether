//! The `aether.http` host secret table (ADR-0235 §6): each operator-bound
//! secret keyed by exact host and header, and the per-request header build
//! that attaches it.
//!
//! Actors never see a value and never choose one: `Fetch` names no secret. The
//! adapter attaches a host's bound headers itself, inside one hop, after the
//! allowlist check, so a value never enters the caller's header list and a
//! cross-host redirect hop never carries it.

use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::fmt;

use aether_substrate::chassis::error::BootError;
use aether_substrate::config::{Secret, SecretName, Secrets};
use ureq::http::{HeaderName, HeaderValue, header};

use crate::kinds::{HttpError, HttpHeader};

/// The header-spec keyword for `Authorization: Bearer <value>` (RFC 6750).
const BEARER: &str = "bearer";

/// One bound header on one host.
struct Bound {
    header: HeaderName,
    /// The binding's secret name, kept for the duplicate-binding refusal.
    name: SecretName,
    secret: Secret,
}

/// The host secret table: host → header → exactly one bound [`Secret`]. Built
/// once at boot from the cap's `--http-secrets` bindings; empty when none are
/// bound.
#[derive(Default)]
pub struct HostSecrets {
    hosts: HashMap<String, Vec<Bound>>,
}

/// A binding [`HostSecrets::bind`] refuses at boot. Every message names hosts,
/// headers, and secret names, never a value.
#[derive(Debug)]
enum BindingError {
    /// The key is not `<host>/<header-name>`.
    Malformed { key: String },
    /// The bound host is not in the http allowlist, so the secret could never
    /// be sent.
    HostNotAllowed { host: String },
    /// The header name is not an HTTP token.
    BadHeader { host: String, header: String },
    /// The secret's value cannot be an HTTP header value.
    BadValue { host: String, header: HeaderName, name: SecretName },
    /// A second binding for one host and header.
    Duplicate { host: String, header: HeaderName, first: SecretName, second: SecretName },
}

impl fmt::Display for BindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { key } => {
                write!(f, "http secret key {key:?} is not `<host>/<header-name>` or `<host>/bearer`")
            }
            Self::HostNotAllowed { host } => {
                write!(f, "http secret bound to host {host:?}, which is not in the http allowlist")
            }
            Self::BadHeader { host, header } => {
                write!(f, "http secret for host {host:?} names header {header:?}, which is not an HTTP token")
            }
            Self::BadValue { host, header, name } => {
                write!(f, "secret `{name}` bound to host {host:?} header `{header}` is not a valid header value")
            }
            Self::Duplicate { host, header, first, second } => write!(
                f,
                "host {host:?} header `{header}` is bound twice, to secrets `{first}` and `{second}`: a host has \
                 one secret per header (ADR-0235 §6)"
            ),
        }
    }
}

impl StdError for BindingError {}

impl From<BindingError> for BootError {
    fn from(error: BindingError) -> Self {
        Self::Other(Box::new(error))
    }
}

impl HostSecrets {
    /// Build the table from the cap's loaded bindings. Each key is
    /// `<host>/<header-name>` or `<host>/bearer`; the bearer form stores
    /// `Bearer <value>` under `authorization`.
    ///
    /// # Errors
    ///
    /// A boot error when a key is malformed, a bound host is missing from
    /// `allowlist`, a header name is not an HTTP token, a value cannot be a
    /// header value, or a host and header are bound twice — the refusal names
    /// the host, the header, and both secret names, never a value.
    pub fn bind(secrets: Secrets, allowlist: &HashSet<String>) -> Result<Self, BootError> {
        let mut hosts: HashMap<String, Vec<Bound>> = HashMap::new();
        for (key, name, secret) in secrets {
            let (host, spec) = key
                .split_once('/')
                .filter(|(host, spec)| !host.is_empty() && !spec.is_empty())
                .ok_or_else(|| BindingError::Malformed { key: key.clone() })?;
            if !allowlist.contains(host) {
                return Err(BindingError::HostNotAllowed { host: host.to_owned() }.into());
            }
            let (header, secret) = if spec.eq_ignore_ascii_case(BEARER) {
                (header::AUTHORIZATION, secret.with_prefix("Bearer "))
            } else {
                let header = HeaderName::from_bytes(spec.as_bytes())
                    .map_err(|_| BindingError::BadHeader { host: host.to_owned(), header: spec.to_owned() })?;
                (header, secret)
            };
            if HeaderValue::from_str(secret.expose()).is_err() {
                return Err(BindingError::BadValue { host: host.to_owned(), header, name }.into());
            }
            let bound = hosts.entry(host.to_owned()).or_default();
            if let Some(first) = bound.iter().find(|existing| existing.header == header) {
                let first = first.name.clone();
                return Err(BindingError::Duplicate { host: host.to_owned(), header, first, second: name }.into());
            }
            bound.push(Bound { header, name, secret });
        }
        Ok(Self { hosts })
    }

    /// Whether any secret is bound to exactly `host`.
    #[must_use]
    pub fn binds(&self, host: &str) -> bool {
        self.hosts.contains_key(host)
    }

    /// The bound hosts, sorted — names only, for the boot log.
    #[must_use]
    pub fn hosts(&self) -> Vec<&str> {
        let mut hosts: Vec<&str> = self.hosts.keys().map(String::as_str).collect();
        hosts.sort_unstable();
        hosts
    }

    /// The headers bound to exactly `host`.
    pub fn for_host(&self, host: &str) -> impl Iterator<Item = (&HeaderName, &Secret)> {
        self.hosts.get(host).into_iter().flatten().map(|bound| (&bound.header, &bound.secret))
    }
}

/// Assemble one hop's request headers for `host`. A caller-set `Host` is
/// stripped (it could route a vhost around the allowlist) and `User-Agent`
/// defaults to `aether/<version>`. A caller header whose name is bound for
/// this host is dropped with a warning naming only the header and host, and
/// each bound header is appended with its value marked sensitive, so a debug
/// print of the request shows `Sensitive`.
///
/// # Errors
///
/// [`HttpError::InvalidUrl`] when a caller header name or value is not valid
/// HTTP, matching the request builder's own refusal.
pub fn request_headers(
    caller: &[HttpHeader],
    secrets: &HostSecrets,
    host: &str,
) -> Result<Vec<(HeaderName, HeaderValue)>, HttpError> {
    let invalid = |error: &dyn fmt::Display| HttpError::InvalidUrl(format!("{error}"));

    let mut headers = Vec::with_capacity(caller.len() + 1);
    let mut saw_user_agent = false;
    for h in caller {
        let name = HeaderName::from_bytes(h.name.as_bytes()).map_err(|error| invalid(&error))?;
        if name == header::HOST {
            tracing::warn!(target: "aether_http", value = %h.value, "stripping caller-set Host header");
            continue;
        }
        if secrets.for_host(host).any(|(bound, _)| *bound == name) {
            tracing::warn!(
                target: "aether_http",
                header = %name,
                host,
                "dropping caller-set header: the operator bound a secret to it for this host",
            );
            continue;
        }
        saw_user_agent |= name == header::USER_AGENT;
        headers.push((name, HeaderValue::from_bytes(h.value.as_bytes()).map_err(|error| invalid(&error))?));
    }
    if !saw_user_agent {
        headers.push((header::USER_AGENT, HeaderValue::from_static(concat!("aether/", env!("CARGO_PKG_VERSION")))));
    }

    for (name, secret) in secrets.for_host(host) {
        let mut value = HeaderValue::from_str(secret.expose()).map_err(|error| invalid(&error))?;
        value.set_sensitive(true);
        headers.push((name.clone(), value));
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use aether_substrate::config::{Secret, SecretName, Secrets};
    use ureq::http::{HeaderName, HeaderValue};

    use super::{HostSecrets, request_headers};
    use crate::kinds::HttpHeader;

    fn allowlist(hosts: &[&str]) -> HashSet<String> {
        hosts.iter().map(|host| (*host).to_owned()).collect()
    }

    /// A `Secrets` collection of obviously fake values: `(key, name, value)`.
    fn secrets(entries: &[(&str, &str, &str)]) -> Secrets {
        Secrets::from_entries(
            entries
                .iter()
                .map(|(key, name, value)| {
                    let name = SecretName::try_from(*name).expect("valid test name");
                    ((*key).to_owned(), name, Secret::new((*value).to_owned()))
                })
                .collect(),
        )
    }

    fn bound(entries: &[(&str, &str, &str)], hosts: &[&str]) -> HostSecrets {
        HostSecrets::bind(secrets(entries), &allowlist(hosts)).expect("bindings are valid")
    }

    fn values<'a>(headers: &'a [(HeaderName, HeaderValue)], name: &str) -> Vec<&'a HeaderValue> {
        headers.iter().filter(|(header, _)| header.as_str() == name).map(|(_, value)| value).collect()
    }

    #[test]
    fn a_secret_rides_only_its_exact_https_host() {
        let table = bound(&[("api.example.com/x-api-key", "one", "fake-value-one")], &["api.example.com"]);

        let own = request_headers(&[], &table, "api.example.com").expect("headers build");
        assert_eq!(values(&own, "x-api-key"), [&HeaderValue::from_static("fake-value-one")]);

        let current = url::Url::parse("https://api.example.com/start").expect("test URL parses");
        let redirect = current.join("https://other.example.com/next").expect("redirect target joins");
        for host in [
            "other.example.com",
            "sub.api.example.com",
            "api.example.com.evil.example",
            redirect.host_str().expect("redirect host"),
        ] {
            let headers = request_headers(&[], &table, host).expect("headers build");
            assert!(values(&headers, "x-api-key").is_empty(), "no secret rides a request to {host}");
        }
    }

    #[test]
    fn a_second_binding_for_one_host_and_header_is_refused() {
        let refused = HostSecrets::bind(
            secrets(&[
                ("api.example.com/x-api-key", "one", "fake-value-one"),
                ("api.example.com/X-Api-Key", "two", "fake-value-two"),
            ]),
            &allowlist(&["api.example.com"]),
        );

        let text = refused.err().expect("a second binding is refused").to_string();
        for named in ["api.example.com", "x-api-key", "`one`", "`two`"] {
            assert!(text.contains(named), "the refusal names {named}: {text}");
        }
        assert!(!text.contains("fake-value"), "the refusal never carries a value: {text}");

        let bearer_twice = HostSecrets::bind(
            secrets(&[("h.example/bearer", "one", "fake-one"), ("h.example/authorization", "two", "fake-two")]),
            &allowlist(&["h.example"]),
        );
        assert!(bearer_twice.is_err(), "bearer and authorization are one header");
    }

    #[test]
    fn a_bound_header_replaces_the_callers() {
        let table = bound(&[("api.example.com/x-api-key", "one", "fake-bound-value")], &["api.example.com"]);
        let caller = [HttpHeader { name: "X-Api-Key".to_owned(), value: "fake-caller-value".to_owned() }];

        let headers = request_headers(&caller, &table, "api.example.com").expect("headers build");

        assert_eq!(values(&headers, "x-api-key"), [&HeaderValue::from_static("fake-bound-value")]);
    }

    #[test]
    fn bearer_binding_builds_the_authorization_header() {
        let table = bound(&[("api.muse.example/bearer", "muse", "fake-token")], &["api.muse.example"]);

        let headers = request_headers(&[], &table, "api.muse.example").expect("headers build");

        assert_eq!(values(&headers, "authorization"), [&HeaderValue::from_static("Bearer fake-token")]);
    }

    #[test]
    fn injected_values_are_marked_sensitive() {
        let table = bound(&[("api.example.com/x-api-key", "one", "fake-sensitive-value")], &["api.example.com"]);
        let caller = [HttpHeader { name: "accept".to_owned(), value: "application/json".to_owned() }];

        let headers = request_headers(&caller, &table, "api.example.com").expect("headers build");

        let injected = values(&headers, "x-api-key");
        assert!(injected.iter().all(|value| value.is_sensitive()), "the bound value is marked sensitive");
        let printed = format!("{headers:?}");
        assert!(!printed.contains("fake-sensitive-value"), "a debug print hides the value: {printed}");
        assert!(printed.contains("application/json"), "caller headers print as usual: {printed}");
    }

    #[test]
    fn binding_refuses_hosts_outside_the_allowlist_and_bad_headers() {
        for (key, why) in [
            ("denied.example/x-api-key", "a host outside the allowlist"),
            ("api.example.com/bad header", "a header that is not an HTTP token"),
            ("api.example.com", "a key with no header"),
            ("/x-api-key", "a key with no host"),
        ] {
            let refused = HostSecrets::bind(secrets(&[(key, "one", "fake-value")]), &allowlist(&["api.example.com"]));
            let text = refused.err().unwrap_or_else(|| panic!("{why} must be refused")).to_string();
            assert!(!text.contains("fake-value"), "the refusal never carries a value: {text}");
        }
    }
}
