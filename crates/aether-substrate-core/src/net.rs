//! Substrate HTTP egress (ADR-0043). The `NetAdapter` trait is the
//! extension point for HTTP backends — `ureq` today, anything else
//! that implements the trait later. The chassis that wires the
//! `"aether.sink.net"` sink builds an adapter from env config, hands it to
//! `net_sink_handler`, and registers the result.
//!
//! v1 semantics:
//! - Blocking on the sink dispatch thread (one request at a time).
//! - Buffered request + response bodies; streaming is deferred.
//! - Deny-by-default allowlist via `AETHER_NET_ALLOWLIST`.
//! - Response size capped at `AETHER_NET_MAX_BODY_BYTES` (16MB).
//! - Default request timeout 30s, per-request override via
//!   `Fetch.timeout_ms`.
//!
//! Permissioning by env var is the stopgap until the capabilities
//! ADR lands; this module's surface doesn't change when that
//! arrives — the allowlist source moves from process env to
//! per-component declarations gated at `load_component`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use aether_kinds::{Fetch, FetchResult, HttpHeader, HttpMethod, NetError};
use aether_mail::Kind;

use crate::mail::ReplyTo;
use crate::mailer::Mailer;
use crate::registry::SinkHandler;

/// Default response-body cap when `AETHER_NET_MAX_BODY_BYTES` is
/// unset. 16MB matches ADR-0043 §3.
pub const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Default per-request timeout when `AETHER_NET_TIMEOUT_MS` is unset
/// and the fetch itself supplies no `timeout_ms`. 30s matches
/// ADR-0043 §4.
pub const DEFAULT_TIMEOUT_MS: u32 = 30_000;

/// Adapter-facing request shape. Converted from the wire `Fetch`
/// kind by the dispatcher before handing to the adapter.
pub struct FetchRequest {
    pub url: String,
    pub method: HttpMethod,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

/// Adapter-facing response shape. Converted to the wire
/// `FetchResult::Ok` by the dispatcher.
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

/// HTTP backend. One method — `fetch` — takes a validated request
/// and returns the response or a structured error. The adapter is
/// responsible for allowlist enforcement, URL validation, body
/// caps, and timeout application; the dispatcher just moves bytes
/// between wire and adapter.
pub trait NetAdapter: Send + Sync {
    fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, NetError>;
}

/// Adapter returned when `AETHER_NET_DISABLE=1` or when backend
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
/// internally synchronised, so the same adapter drives the sink
/// from one dispatch thread today and would parallelise cleanly
/// behind a multi-thread dispatcher later.
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
        // `http_status_as_error(false)` surfaces non-2xx responses
        // as `FetchResult::Ok { status: 4xx/5xx, ... }` rather than
        // `Err(AdapterError)` — a 404 from a correctly-reached API
        // is not a network failure.
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

        // Build an `http::Request` and route it through `with_agent
        // → configure → build → run`. That path is uniform across
        // body-bearing and bodyless methods (no WithBody/WithoutBody
        // typestate to branch on) and lets us set `timeout_global`
        // per-request on top of the agent-level defaults.
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

/// Build the default adapter from environment variables per
/// ADR-0043 §5/§8. `AETHER_NET_DISABLE=1` short-circuits to a
/// `DisabledNetAdapter`; otherwise reads the allowlist, body cap,
/// and https flag and returns a `UreqNetAdapter`.
///
/// Deny-by-default: unset or empty `AETHER_NET_ALLOWLIST` means the
/// allowlist is empty, and every fetch replies `AllowlistDenied`.
/// Set the var to a comma-separated list of hostnames to allow
/// those hosts.
pub fn build_default_adapter() -> Arc<dyn NetAdapter> {
    if disable_flag() {
        tracing::info!(
            target: "aether_substrate::net",
            "AETHER_NET_DISABLE=1 — net sink replies Disabled for every fetch",
        );
        return Arc::new(DisabledNetAdapter);
    }

    let allowlist = parse_allowlist();
    let require_https = https_flag();
    let max_body_bytes = parse_max_body_bytes();

    tracing::info!(
        target: "aether_substrate::net",
        allowlist_size = allowlist.len(),
        require_https,
        max_body_bytes,
        "net adapter configured",
    );

    Arc::new(UreqNetAdapter::new(
        allowlist,
        require_https,
        max_body_bytes,
    ))
}

fn disable_flag() -> bool {
    std::env::var("AETHER_NET_DISABLE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn https_flag() -> bool {
    std::env::var("AETHER_NET_REQUIRE_HTTPS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn parse_allowlist() -> HashSet<String> {
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

fn parse_max_body_bytes() -> usize {
    std::env::var("AETHER_NET_MAX_BODY_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_BODY_BYTES)
}

fn parse_default_timeout() -> Duration {
    let ms = std::env::var("AETHER_NET_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(ms as u64)
}

/// Build the `"aether.sink.net"` sink handler. The chassis calls this
/// at boot after `build_default_adapter` and passes the result to
/// `registry.register_sink("aether.sink.net", handler)`. The returned closure
/// decodes incoming `Fetch` mail, hands it to the adapter, and
/// replies with the paired `FetchResult` via `mailer.send_reply`.
///
/// The reply router is `Mailer::send_reply`, so session / engine-
/// mailbox / local-component replies all funnel through one path —
/// identical to the io sink.
///
/// Fetches run synchronously on the sink dispatch thread — ADR-0043
/// §2 flags this as the head-of-line blocking source to fix via a
/// multi-threaded dispatcher ADR.
pub fn net_sink_handler(adapter: Arc<dyn NetAdapter>, mailer: Arc<Mailer>) -> SinkHandler {
    let default_timeout = parse_default_timeout();
    Arc::new(
        move |kind_id: u64,
              _kind_name: &str,
              _origin: Option<&str>,
              sender: ReplyTo,
              bytes: &[u8],
              _count: u32| {
            dispatch_net_mail(
                adapter.as_ref(),
                &mailer,
                kind_id,
                sender,
                bytes,
                default_timeout,
            );
        },
    )
}

fn dispatch_net_mail(
    adapter: &dyn NetAdapter,
    mailer: &Mailer,
    kind_id: u64,
    sender: ReplyTo,
    bytes: &[u8],
    default_timeout: Duration,
) {
    if kind_id != <Fetch as Kind>::ID {
        tracing::warn!(
            target: "aether_substrate::net",
            kind_id,
            "net sink received unknown kind — dropping",
        );
        return;
    }

    let req: Fetch = match postcard::from_bytes(bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "aether_substrate::net",
                error = %e,
                "fetch: decode failed, replying Err",
            );
            mailer.send_reply(
                sender,
                &FetchResult::Err {
                    url: String::new(),
                    error: NetError::AdapterError(format!("decode failed: {e}")),
                },
            );
            return;
        }
    };

    let timeout = req
        .timeout_ms
        .map(|ms| Duration::from_millis(ms as u64))
        .unwrap_or(default_timeout);

    let url = req.url.clone();
    let adapter_req = FetchRequest {
        url: req.url,
        method: req.method,
        headers: req.headers,
        body: req.body,
        timeout,
    };

    let reply = match adapter.fetch(adapter_req) {
        Ok(r) => FetchResult::Ok {
            url,
            status: r.status,
            headers: r.headers,
            body: r.body,
        },
        Err(error) => FetchResult::Err { url, error },
    };
    mailer.send_reply(sender, &reply);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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

    use crate::hub_client::HubOutbound;
    use aether_hub_protocol::{EngineToHub, SessionToken, Uuid};

    fn session_sender() -> ReplyTo {
        ReplyTo::to(crate::mail::ReplyTarget::Session(SessionToken(Uuid::nil())))
    }

    fn test_mailer_and_rx() -> (Arc<Mailer>, std::sync::mpsc::Receiver<EngineToHub>) {
        use std::collections::HashMap;
        use std::sync::RwLock;

        let (outbound, rx) = HubOutbound::test_channel();
        let mailer = Arc::new(Mailer::new());
        mailer.wire(
            Arc::new(crate::registry::Registry::new()),
            Arc::new(RwLock::new(HashMap::new())),
        );
        mailer.wire_outbound(outbound);
        (mailer, rx)
    }

    fn decode_reply<K: aether_mail::Kind + serde::de::DeserializeOwned>(
        rx: &std::sync::mpsc::Receiver<EngineToHub>,
    ) -> K {
        let frame = rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let EngineToHub::Mail(m) = frame else {
            panic!("expected Mail frame, got {frame:?}");
        };
        assert_eq!(m.kind_name, K::NAME);
        postcard::from_bytes(&m.payload).unwrap()
    }

    #[test]
    fn disabled_adapter_replies_disabled() {
        let adapter: Arc<dyn NetAdapter> = Arc::new(DisabledNetAdapter);
        let (mailer, rx) = test_mailer_and_rx();
        let handler = net_sink_handler(Arc::clone(&adapter), Arc::clone(&mailer));
        let req = postcard::to_allocvec(&Fetch {
            url: "https://api.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout_ms: None,
        })
        .unwrap();
        handler(
            <Fetch as Kind>::ID,
            Fetch::NAME,
            None,
            session_sender(),
            &req,
            1,
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
    fn dispatch_fetch_ok_replies_with_response() {
        let adapter = StubAdapter::with(Ok(FetchResponse {
            status: 200,
            headers: vec![HttpHeader {
                name: "content-type".to_string(),
                value: "application/json".to_string(),
            }],
            body: b"{}".to_vec(),
        })) as Arc<dyn NetAdapter>;
        let (mailer, rx) = test_mailer_and_rx();
        let handler = net_sink_handler(Arc::clone(&adapter), Arc::clone(&mailer));
        let req = postcard::to_allocvec(&Fetch {
            url: "https://api.example.com/v1".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout_ms: Some(5000),
        })
        .unwrap();
        handler(
            <Fetch as Kind>::ID,
            Fetch::NAME,
            None,
            session_sender(),
            &req,
            1,
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
    fn dispatch_fetch_err_echoes_url_and_error() {
        let adapter = StubAdapter::with(Err(NetError::Timeout)) as Arc<dyn NetAdapter>;
        let (mailer, rx) = test_mailer_and_rx();
        let handler = net_sink_handler(Arc::clone(&adapter), Arc::clone(&mailer));
        let req = postcard::to_allocvec(&Fetch {
            url: "https://slow.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout_ms: None,
        })
        .unwrap();
        handler(
            <Fetch as Kind>::ID,
            Fetch::NAME,
            None,
            session_sender(),
            &req,
            1,
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
    fn dispatch_malformed_bytes_replies_adapter_error_with_empty_url() {
        let adapter = StubAdapter::with(Ok(FetchResponse {
            status: 200,
            headers: vec![],
            body: vec![],
        })) as Arc<dyn NetAdapter>;
        let (mailer, rx) = test_mailer_and_rx();
        let handler = net_sink_handler(Arc::clone(&adapter), Arc::clone(&mailer));
        handler(
            <Fetch as Kind>::ID,
            Fetch::NAME,
            None,
            session_sender(),
            &[0xffu8; 8],
            1,
        );
        match decode_reply::<FetchResult>(&rx) {
            FetchResult::Err {
                url,
                error: NetError::AdapterError(_),
            } => {
                assert_eq!(url, "");
            }
            other => panic!("expected Err AdapterError with empty url, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_unknown_kind_does_not_reply() {
        let adapter = StubAdapter::with(Err(NetError::Disabled)) as Arc<dyn NetAdapter>;
        let (mailer, rx) = test_mailer_and_rx();
        let handler = net_sink_handler(Arc::clone(&adapter), Arc::clone(&mailer));
        handler(0xdead_beef, "some.other", None, session_sender(), &[], 1);
        assert!(rx.try_recv().is_err(), "unexpected reply on unknown kind");
    }

    #[test]
    fn dispatch_fetch_uses_default_timeout_when_none_provided() {
        // StubAdapter captures the request; we just need to confirm
        // the dispatcher picks a reasonable default when
        // `timeout_ms: None` arrives on the wire. Default path
        // resolves to `parse_default_timeout()` which honours
        // `AETHER_NET_TIMEOUT_MS` at closure construction time.
        let adapter_stub = StubAdapter::with(Ok(FetchResponse {
            status: 200,
            headers: vec![],
            body: vec![],
        }));
        let adapter: Arc<dyn NetAdapter> = Arc::clone(&adapter_stub) as Arc<dyn NetAdapter>;
        let (mailer, _rx) = test_mailer_and_rx();
        let handler = net_sink_handler(adapter, mailer);
        let req = postcard::to_allocvec(&Fetch {
            url: "https://api.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout_ms: None,
        })
        .unwrap();
        handler(
            <Fetch as Kind>::ID,
            Fetch::NAME,
            None,
            session_sender(),
            &req,
            1,
        );
        let observed = adapter_stub
            .last_request
            .lock()
            .unwrap()
            .take()
            .expect("adapter was not called");
        // Default is 30s unless AETHER_NET_TIMEOUT_MS was set; a
        // nonzero timeout is enough to prove the closure handed a
        // default through, without coupling the test to the exact
        // env state.
        assert!(observed.timeout > Duration::ZERO);
    }

    #[test]
    fn build_default_adapter_with_disable_returns_disabled() {
        // Set the env var only for this test; clear after. Running
        // single-threaded is the safe path (cargo test --test-threads=1)
        // but we scope the mutation tightly regardless.
        // SAFETY: std::env::set_var in Rust 2024 is `unsafe` because
        // of POSIX getenv thread-safety.
        unsafe { std::env::set_var("AETHER_NET_DISABLE", "1") };
        let a = build_default_adapter();
        let resp = a.fetch(FetchRequest {
            url: "https://example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        unsafe { std::env::remove_var("AETHER_NET_DISABLE") };
        assert!(matches!(resp, Err(NetError::Disabled)));
    }
}
