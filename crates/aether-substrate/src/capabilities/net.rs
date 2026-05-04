//! Issue 545 PR E1: collapsed `aether.net` cap. Pre-PR-E1 the cap
//! lived split across `aether-kinds::net::NetCapability<B>` (facade
//! generic) and this file (concrete `NetAdapterBackend`). The facade
//! pattern (ADR-0075) is retired — caps are now regular `#[actor]`
//! blocks, same shape as wasm components.
//!
//! Owns the full HTTP egress stack — `NetAdapter` trait, the `ureq`-
//! backed `UreqNetAdapter`, env-driven `NetConfig`, and the
//! [`NetCapability`] itself. Chassis mains resolve a [`NetConfig`]
//! (typically via [`NetConfig::from_env`]), call
//! [`NetCapability::new`], and hand the cap to the chassis builder.
//!
//! v1 semantics (ADR-0043):
//! - Blocking on the dispatcher thread (one request at a time).
//! - Buffered request + response bodies; streaming is deferred.
//! - Deny-by-default allowlist via `AETHER_NET_ALLOWLIST`.
//! - Response size capped at `AETHER_NET_MAX_BODY_BYTES` (16MB).
//! - Default request timeout 30s, per-request override via
//!   `Fetch.timeout_ms`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use aether_actor::{Actor, Singleton};
use aether_data::ReplyTo;
use aether_kinds::{Fetch, FetchResult, HttpHeader, HttpMethod, NetError};

use crate::mailer::Mailer;

/// Default response-body cap when `AETHER_NET_MAX_BODY_BYTES` is
/// unset. 16MB matches ADR-0043 §3.
pub const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Default per-request timeout when `AETHER_NET_TIMEOUT_MS` is unset
/// and the fetch itself supplies no `timeout_ms`. 30s matches
/// ADR-0043 §4.
pub const DEFAULT_TIMEOUT_MS: u32 = 30_000;

/// Adapter-facing request shape. Converted from the wire `Fetch`
/// kind by the cap before handing to the adapter.
pub struct FetchRequest {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

/// Adapter-facing response shape. Converted to the wire
/// `FetchResult::Ok` by the cap.
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

/// HTTP backend. One method — `fetch` — takes a validated request
/// and returns the response or a structured error. The adapter is
/// responsible for allowlist enforcement, URL validation, body
/// caps, and timeout application; the cap just moves bytes between
/// wire and adapter.
pub trait NetAdapter: Send + Sync {
    fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, NetError>;
}

/// Adapter returned when `AETHER_NET_DISABLE=1` or when adapter
/// construction fails at boot. Every fetch replies
/// `NetError::Disabled` so callers learn why nothing's happening
/// instead of hanging or silently dropping.
pub struct DisabledNetAdapter;

impl NetAdapter for DisabledNetAdapter {
    fn fetch(&self, _req: FetchRequest) -> Result<FetchResponse, NetError> {
        Err(NetError::Disabled)
    }
}

/// `ureq`-backed adapter. Holds the shared agent, the allowlist
/// (empty = deny all), the response cap, and the `require_https`
/// flag. Thread-safe: `ureq::Agent` is cheaply cloneable and
/// internally synchronised, so the same adapter drives the cap from
/// one dispatch thread today and would parallelise cleanly behind a
/// multi-thread dispatcher later.
pub struct UreqNetAdapter {
    agent: ureq::Agent,
    allowlist: HashSet<String>,
    require_https: bool,
    max_body_bytes: usize,
}

impl UreqNetAdapter {
    /// Construct an adapter with explicit knobs. Chassis code uses
    /// [`build_default_adapter`] for env-derived construction;
    /// tests build adapters directly to avoid env contamination.
    pub fn new(allowlist: HashSet<String>, require_https: bool, max_body_bytes: usize) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build();
        let agent = ureq::Agent::new_with_config(config);
        Self {
            agent,
            allowlist,
            require_https,
            max_body_bytes,
        }
    }

    fn check_allowlist(&self, host: &str) -> Result<(), NetError> {
        if self.allowlist.contains(host) {
            Ok(())
        } else {
            Err(NetError::AllowlistDenied)
        }
    }
}

impl NetAdapter for UreqNetAdapter {
    fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, NetError> {
        let parsed = url::Url::parse(&req.url).map_err(|e| NetError::InvalidUrl(format!("{e}")))?;

        if self.require_https && parsed.scheme() != "https" {
            return Err(NetError::InvalidUrl(
                "http scheme not allowed (AETHER_NET_REQUIRE_HTTPS=1)".to_string(),
            ));
        }

        let host = parsed
            .host_str()
            .ok_or_else(|| NetError::InvalidUrl("no host in url".to_string()))?;
        self.check_allowlist(host)?;

        if req.body.len() > self.max_body_bytes {
            return Err(NetError::BodyTooLarge);
        }

        let mut builder = ureq::http::Request::builder()
            .method(http_method_to_http_crate(req.method))
            .uri(&req.url);

        // Host header is derived from the URL by ureq; reject any
        // caller-set Host so it can't be used to bypass the
        // allowlist (component requests allowlisted A, TLS SNI is A,
        // but `Host: B` routes the vhost to B server-side). User-
        // Agent defaults to `aether/<version>` if not set.
        let mut saw_user_agent = false;
        for h in &req.headers {
            if h.name.eq_ignore_ascii_case("host") {
                tracing::warn!(
                    target: "aether_substrate::net",
                    value = %h.value,
                    "stripping caller-set Host header",
                );
                continue;
            }
            if h.name.eq_ignore_ascii_case("user-agent") {
                saw_user_agent = true;
            }
            builder = builder.header(&h.name, &h.value);
        }
        if !saw_user_agent {
            builder = builder.header("User-Agent", concat!("aether/", env!("CARGO_PKG_VERSION")));
        }

        let http_req = builder
            .body(req.body)
            .map_err(|e| NetError::InvalidUrl(format!("{e}")))?;

        use ureq::RequestExt;
        let mut response = http_req
            .with_agent(&self.agent)
            .configure()
            .timeout_global(Some(req.timeout))
            .build()
            .run()
            .map_err(ureq_error_to_net_error)?;

        let status = response.status().as_u16();

        let mut headers = Vec::with_capacity(response.headers().len());
        for (name, value) in response.headers() {
            // Non-UTF8 header values are rare but real (binary
            // cookies, broken servers). Skip rather than fail the
            // whole fetch.
            if let Ok(value_str) = value.to_str() {
                headers.push(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value_str.to_string(),
                });
            }
        }

        let body = match response
            .body_mut()
            .with_config()
            .limit(self.max_body_bytes as u64)
            .read_to_vec()
        {
            Ok(b) => b,
            Err(ureq::Error::BodyExceedsLimit(_)) => return Err(NetError::BodyTooLarge),
            Err(e) => return Err(NetError::AdapterError(format!("body read: {e}"))),
        };

        Ok(FetchResponse {
            status,
            headers,
            body,
        })
    }
}

fn http_method_to_http_crate(m: HttpMethod) -> ureq::http::Method {
    match m {
        HttpMethod::Get => ureq::http::Method::GET,
        HttpMethod::Post => ureq::http::Method::POST,
        HttpMethod::Put => ureq::http::Method::PUT,
        HttpMethod::Delete => ureq::http::Method::DELETE,
        HttpMethod::Patch => ureq::http::Method::PATCH,
        HttpMethod::Head => ureq::http::Method::HEAD,
        HttpMethod::Options => ureq::http::Method::OPTIONS,
    }
}

fn ureq_error_to_net_error(e: ureq::Error) -> NetError {
    match e {
        ureq::Error::Timeout(_) => NetError::Timeout,
        ureq::Error::BodyExceedsLimit(_) => NetError::BodyTooLarge,
        other => NetError::AdapterError(format!("{other}")),
    }
}

/// Resolved configuration for the substrate's net adapter. Chassis
/// mains read env vars (`AETHER_NET_DISABLE`, `AETHER_NET_ALLOWLIST`,
/// `AETHER_NET_REQUIRE_HTTPS`, `AETHER_NET_MAX_BODY_BYTES`,
/// `AETHER_NET_TIMEOUT_MS`) into a `NetConfig` and pass it to
/// [`NetCapability::new`]. Tests build a `NetConfig` directly,
/// never touching process env (issue 464).
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// `AETHER_NET_DISABLE=1` swaps the `UreqNetAdapter` for a
    /// `DisabledNetAdapter` that replies `NetError::Disabled` to
    /// every fetch.
    pub disabled: bool,
    /// Hostnames the adapter will dial. Empty = deny all
    /// (deny-by-default per ADR-0043).
    pub allowlist: HashSet<String>,
    /// `AETHER_NET_REQUIRE_HTTPS=1` rejects `http://` URLs with
    /// `NetError::InvalidUrl`.
    pub require_https: bool,
    /// Cap on inbound and outbound body bytes. Defaults to
    /// [`DEFAULT_MAX_BODY_BYTES`] (16 MB).
    pub max_body_bytes: usize,
    /// Default per-request timeout when `Fetch.timeout_ms` is
    /// `None`. Defaults to [`DEFAULT_TIMEOUT_MS`] (30 s).
    pub default_timeout: Duration,
}

impl Default for NetConfig {
    fn default() -> Self {
        Self {
            disabled: false,
            allowlist: HashSet::new(),
            require_https: false,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            default_timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS as u64),
        }
    }
}

impl NetConfig {
    /// Resolve every field from the corresponding `AETHER_NET_*`
    /// environment variable. Used by chassis mains; tests build
    /// `NetConfig` directly so they never read process env.
    pub fn from_env() -> Self {
        Self {
            disabled: disable_flag_env(),
            allowlist: parse_allowlist_env(),
            require_https: https_flag_env(),
            max_body_bytes: parse_max_body_bytes_env(),
            default_timeout: parse_default_timeout_env(),
        }
    }
}

/// Build a net adapter from explicit configuration.
pub fn build_net_adapter(config: NetConfig) -> Arc<dyn NetAdapter> {
    if config.disabled {
        tracing::info!(
            target: "aether_substrate::net",
            "net adapter disabled — every fetch replies Disabled",
        );
        return Arc::new(DisabledNetAdapter);
    }

    tracing::info!(
        target: "aether_substrate::net",
        allowlist_size = config.allowlist.len(),
        require_https = config.require_https,
        max_body_bytes = config.max_body_bytes,
        "net adapter configured",
    );

    Arc::new(UreqNetAdapter::new(
        config.allowlist,
        config.require_https,
        config.max_body_bytes,
    ))
}

/// Env-driven wrapper around [`build_net_adapter`].
pub fn build_default_adapter() -> Arc<dyn NetAdapter> {
    build_net_adapter(NetConfig::from_env())
}

fn disable_flag_env() -> bool {
    std::env::var("AETHER_NET_DISABLE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn https_flag_env() -> bool {
    std::env::var("AETHER_NET_REQUIRE_HTTPS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn parse_allowlist_env() -> HashSet<String> {
    std::env::var("AETHER_NET_ALLOWLIST")
        .ok()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_max_body_bytes_env() -> usize {
    std::env::var("AETHER_NET_MAX_BODY_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

fn parse_default_timeout_env() -> Duration {
    let ms = std::env::var("AETHER_NET_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(ms as u64)
}

/// `aether.net` mailbox cap. Owns the resolved adapter, the chassis
/// [`Arc<Mailer>`] for routing replies, and the default per-request
/// timeout applied when `Fetch.timeout_ms` is `None`. The dispatcher
/// thread owns this through the macro-emitted `Dispatch` impl.
pub struct NetCapability {
    adapter: Arc<dyn NetAdapter>,
    mailer: Arc<Mailer>,
    default_timeout: Duration,
}

impl NetCapability {
    /// Construct from a [`NetConfig`] (resolved by the chassis main
    /// — typically via [`NetConfig::from_env`]) and the chassis's
    /// [`Mailer`]. The adapter is built immediately so any
    /// configuration error surfaces at chassis-builder time, not at
    /// first fetch.
    pub fn new(config: NetConfig, mailer: Arc<Mailer>) -> Self {
        let default_timeout = config.default_timeout;
        Self {
            adapter: build_net_adapter(config),
            mailer,
            default_timeout,
        }
    }

    /// Construct from an explicit adapter. Used by tests that supply
    /// a stub.
    pub fn from_adapter(
        adapter: Arc<dyn NetAdapter>,
        mailer: Arc<Mailer>,
        default_timeout: Duration,
    ) -> Self {
        Self {
            adapter,
            mailer,
            default_timeout,
        }
    }
}

impl Actor for NetCapability {
    /// ADR-0043 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.net";
}

impl Singleton for NetCapability {}

#[aether_data::actor]
impl NetCapability {
    /// Run a fetch request and reply with the response.
    ///
    /// # Agent
    /// Reply: `FetchResult`. Synchronous on the dispatcher thread —
    /// long-running fetches block other net mail until they finish.
    #[aether_data::handler]
    fn on_fetch(&mut self, sender: ReplyTo, mail: Fetch) {
        let timeout = mail
            .timeout_ms
            .map(|ms| Duration::from_millis(ms as u64))
            .unwrap_or(self.default_timeout);

        let url = mail.url.clone();
        let adapter_req = FetchRequest {
            url: mail.url,
            method: mail.method,
            headers: mail.headers,
            body: mail.body,
            timeout,
        };

        let reply = match self.adapter.fetch(adapter_req) {
            Ok(r) => FetchResult::Ok {
                url,
                status: r.status,
                headers: r.headers,
                body: r.body,
            },
            Err(error) => FetchResult::Err { url, error },
        };
        self.mailer.send_reply(sender, &reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{BootError, ChassisBuilder};
    use crate::registry::Registry;
    use aether_data::Kind;
    use std::sync::Mutex;

    fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
        let registry = Arc::new(Registry::new());
        for d in aether_kinds::descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        (registry, Arc::new(Mailer::new()))
    }

    struct StubAdapter {
        response: Mutex<Option<Result<FetchResponse, NetError>>>,
        last_request: Mutex<Option<FetchRequest>>,
    }

    impl StubAdapter {
        fn with(response: Result<FetchResponse, NetError>) -> Arc<Self> {
            Arc::new(Self {
                response: Mutex::new(Some(response)),
                last_request: Mutex::new(None),
            })
        }
    }

    impl NetAdapter for StubAdapter {
        fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, NetError> {
            *self.last_request.lock().unwrap() = Some(FetchRequest {
                url: req.url.clone(),
                method: req.method,
                headers: req.headers.clone(),
                body: req.body.clone(),
                timeout: req.timeout,
            });
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("stub response already consumed")
        }
    }

    use aether_data::{ReplyTarget, SessionToken, Uuid};

    use crate::outbound::EgressEvent;

    fn session_sender() -> ReplyTo {
        ReplyTo::to(ReplyTarget::Session(SessionToken(Uuid::nil())))
    }

    fn test_mailer_and_rx() -> (Arc<Mailer>, std::sync::mpsc::Receiver<EgressEvent>) {
        use std::collections::HashMap;
        use std::sync::RwLock;

        let (outbound, rx) = crate::outbound::HubOutbound::attached_loopback();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(
            Arc::new(crate::registry::Registry::new()),
            Arc::new(RwLock::new(HashMap::new())),
        );
        mailer.wire_outbound(outbound);
        (mailer, rx)
    }

    fn decode_reply<K: aether_data::Kind + serde::de::DeserializeOwned>(
        rx: &std::sync::mpsc::Receiver<EgressEvent>,
    ) -> K {
        let event = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let EgressEvent::ToSession {
            kind_name, payload, ..
        } = event
        else {
            panic!("expected ToSession egress, got {event:?}");
        };
        assert_eq!(kind_name, K::NAME);
        postcard::from_bytes(&payload).unwrap()
    }

    /// Boot the cap against a default disabled NetConfig and confirm
    /// the mailbox is registered.
    #[test]
    fn capability_boots_and_registers_mailbox() {
        let (registry, mailer) = fresh_substrate();
        let config = NetConfig {
            disabled: true,
            ..NetConfig::default()
        };
        let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(NetCapability::new(config, Arc::clone(&mailer)))
            .build()
            .expect("net capability boots");
        assert!(
            registry.lookup(NetCapability::NAMESPACE).is_some(),
            "net mailbox registered"
        );
        chassis.shutdown();
    }

    /// Builder rejects a duplicate claim.
    #[test]
    fn duplicate_claim_rejects_with_typed_error() {
        let (registry, mailer) = fresh_substrate();
        registry.register_sink(NetCapability::NAMESPACE, Arc::new(|_, _, _, _, _, _| {}));
        let config = NetConfig {
            disabled: true,
            ..NetConfig::default()
        };

        let err = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with(NetCapability::new(config, Arc::clone(&mailer)))
            .build()
            .expect_err("collision must surface as BootError");
        assert!(matches!(
            err,
            BootError::MailboxAlreadyClaimed { ref name }
                if name == NetCapability::NAMESPACE
        ));
    }

    #[test]
    fn disabled_adapter_replies_disabled() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut cap = NetCapability::from_adapter(
            Arc::new(DisabledNetAdapter),
            mailer,
            NetConfig::default().default_timeout,
        );
        cap.on_fetch(
            session_sender(),
            Fetch {
                url: "https://api.example.com/".to_string(),
                method: HttpMethod::Get,
                headers: vec![],
                body: vec![],
                timeout_ms: None,
            },
        );
        match decode_reply::<FetchResult>(&rx) {
            FetchResult::Err {
                url,
                error: NetError::Disabled,
            } => {
                assert_eq!(url, "https://api.example.com/");
            }
            other => panic!("expected Err Disabled, got {other:?}"),
        }
    }

    #[test]
    fn allowlist_empty_rejects_every_host() {
        let adapter = UreqNetAdapter::new(HashSet::new(), false, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "https://api.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(NetError::AllowlistDenied)));
    }

    #[test]
    fn allowlist_miss_returns_denied_without_making_request() {
        let mut allowlist = HashSet::new();
        allowlist.insert("allowed.example.com".to_string());
        let adapter = UreqNetAdapter::new(allowlist, false, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "https://denied.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(NetError::AllowlistDenied)));
    }

    #[test]
    fn invalid_url_returns_invalid_url_variant() {
        let adapter = UreqNetAdapter::new(HashSet::new(), false, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "not-a-url".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(NetError::InvalidUrl(_))));
    }

    #[test]
    fn require_https_rejects_http_scheme() {
        let mut allowlist = HashSet::new();
        allowlist.insert("example.com".to_string());
        let adapter = UreqNetAdapter::new(allowlist, true, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "http://example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(NetError::InvalidUrl(_))));
    }

    #[test]
    fn oversize_request_body_returns_body_too_large() {
        let mut allowlist = HashSet::new();
        allowlist.insert("example.com".to_string());
        let adapter = UreqNetAdapter::new(allowlist, false, 10);
        let resp = adapter.fetch(FetchRequest {
            url: "https://example.com/".to_string(),
            method: HttpMethod::Post,
            headers: vec![],
            body: vec![0u8; 20],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(NetError::BodyTooLarge)));
    }

    #[test]
    fn cap_fetch_ok_replies_with_response() {
        let (mailer, rx) = test_mailer_and_rx();
        let stub = StubAdapter::with(Ok(FetchResponse {
            status: 200,
            headers: vec![HttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            }],
            body: b"{}".to_vec(),
        }));
        let mut cap = NetCapability::from_adapter(
            stub as Arc<dyn NetAdapter>,
            mailer,
            NetConfig::default().default_timeout,
        );
        cap.on_fetch(
            session_sender(),
            Fetch {
                url: "https://api.example.com/v1".to_string(),
                method: HttpMethod::Get,
                headers: vec![],
                body: vec![],
                timeout_ms: Some(5000),
            },
        );
        match decode_reply::<FetchResult>(&rx) {
            FetchResult::Ok {
                url,
                status,
                headers,
                body,
            } => {
                assert_eq!(url, "https://api.example.com/v1");
                assert_eq!(status, 200);
                assert_eq!(headers.len(), 1);
                assert_eq!(body, b"{}".to_vec());
            }
            FetchResult::Err { error, .. } => panic!("expected Ok, got Err({error:?})"),
        }
    }

    #[test]
    fn cap_fetch_err_echoes_url_and_error() {
        let (mailer, rx) = test_mailer_and_rx();
        let mut cap = NetCapability::from_adapter(
            StubAdapter::with(Err(NetError::Timeout)) as Arc<dyn NetAdapter>,
            mailer,
            NetConfig::default().default_timeout,
        );
        cap.on_fetch(
            session_sender(),
            Fetch {
                url: "https://slow.example.com/".to_string(),
                method: HttpMethod::Get,
                headers: vec![],
                body: vec![],
                timeout_ms: None,
            },
        );
        match decode_reply::<FetchResult>(&rx) {
            FetchResult::Err { url, error } => {
                assert_eq!(url, "https://slow.example.com/");
                assert_eq!(error, NetError::Timeout);
            }
            FetchResult::Ok { .. } => panic!("expected Err"),
        }
    }

    #[test]
    fn cap_uses_default_timeout_when_none_provided() {
        let (mailer, _rx) = test_mailer_and_rx();
        let stub = StubAdapter::with(Ok(FetchResponse {
            status: 200,
            headers: vec![],
            body: vec![],
        }));
        let stub_clone = Arc::clone(&stub);
        let mut cap = NetCapability::from_adapter(
            stub as Arc<dyn NetAdapter>,
            mailer,
            NetConfig::default().default_timeout,
        );
        cap.on_fetch(
            session_sender(),
            Fetch {
                url: "https://api.example.com/".to_string(),
                method: HttpMethod::Get,
                headers: vec![],
                body: vec![],
                timeout_ms: None,
            },
        );
        let observed = stub_clone
            .last_request
            .lock()
            .unwrap()
            .take()
            .expect("adapter was not called");
        assert!(observed.timeout > Duration::ZERO);
    }

    #[test]
    fn build_net_adapter_with_disable_returns_disabled() {
        let cfg = NetConfig {
            disabled: true,
            ..NetConfig::default()
        };
        let a = build_net_adapter(cfg);
        let resp = a.fetch(FetchRequest {
            url: "https://example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(NetError::Disabled)));
    }

    /// Silence `Kind` unused-import (handy for the test mod's
    /// `decode_reply` bound).
    #[allow(dead_code)]
    fn _silence_kind<K: Kind>() {}
}
