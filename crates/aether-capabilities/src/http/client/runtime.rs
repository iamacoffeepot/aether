//! The `aether.http` egress runtime half (ADR-0122 identity/runtime split).
//! Compiled only under `feature = "runtime"` (the `mod runtime;` declaration
//! in the parent carries the gate), so a transport-only build of the
//! `HttpCapability` identity never names these types nor pulls
//! `aether_substrate`. The substrate-typed imports are gated once by this
//! module rather than line-by-line; the `#[runtime] impl NativeActor` reaches
//! the state, ctx, and adapter types directly here.
//!
//! Holds the state-bearing `HttpCapabilityState` (the resolved adapter + the
//! default per-request timeout), the adapter abstraction (`FetchRequest`,
//! `FetchResponse`, the `HttpAdapter` trait, `DisabledHttpAdapter`), and the
//! `ureq`-backed `UreqHttpAdapter` stack.

// Parent-level items this module names. `HttpCapability` is the impl's `Self`
// type and `HttpConfig` is named by `init`'s signature.
use super::{HttpCapability, HttpConfig};
use aether_actor::runtime;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use ureq::http::Method;
use ureq::http::Request;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

use crate::http::kinds::{Fetch, FetchResult, HttpError, HttpHeader, HttpMethod};

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
/// responsible for initial-URL allowlist enforcement, URL validation, body
/// caps, and timeout application; the cap just moves bytes between
/// wire and adapter.
pub trait HttpAdapter: Send + Sync {
    fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, HttpError>;
}

/// Adapter returned when `AETHER_HTTP_DISABLE=1` or when adapter
/// construction fails at boot. Every fetch replies
/// `HttpError::Disabled` so callers learn why nothing's happening
/// instead of hanging or silently dropping.
pub struct DisabledHttpAdapter;

impl HttpAdapter for DisabledHttpAdapter {
    fn fetch(&self, _req: FetchRequest) -> Result<FetchResponse, HttpError> {
        Err(HttpError::Disabled)
    }
}

/// `aether.http` runtime state (ADR-0043). Owns the resolved adapter
/// and the default per-request timeout applied when `Fetch.timeout_ms`
/// is `None`. The dispatcher holds this as the cap's state and routes
/// envelopes through the macro-emitted `Dispatch` impl; replies return
/// directly from the `#[handler]` method (ADR-0112). The addressing
/// identity is the distinct ZST `HttpCapability`. Living in this private
/// module keeps it `pub`-enough to satisfy the `NativeActor::State`
/// interface without exposing it as crate-public API.
pub struct HttpCapabilityState {
    pub adapter: Arc<dyn HttpAdapter>,
    pub default_timeout: Duration,
}

#[cfg(test)]
impl HttpCapabilityState {
    /// Test-only direct constructor. Production boots through
    /// `Builder::with_actor::<HttpCapability>(config)` which calls the
    /// generated `Lifecycle::init`; tests that drive the handler with a
    /// stub adapter hand it in directly.
    pub fn from_adapter(adapter: Arc<dyn HttpAdapter>, default_timeout: Duration) -> Self {
        Self { adapter, default_timeout }
    }
}

#[runtime]
impl NativeActor for HttpCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// resolved adapter plus the default per-request timeout.
    type State = HttpCapabilityState;

    type Config = HttpConfig;

    /// ADR-0043 + ADR-0074 Phase 5 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.http";

    /// Build the HTTP adapter from the resolved config. The adapter is
    /// built immediately so configuration errors surface at chassis-
    /// builder time, not at first fetch.
    fn init(config: HttpConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<HttpCapabilityState, BootError> {
        let default_timeout = config.default_timeout;
        Ok(HttpCapabilityState { adapter: build_http_adapter(config), default_timeout })
    }

    /// Run a fetch request and reply with the response.
    ///
    /// # Agent
    /// Reply: `FetchResult`. Synchronous on the dispatcher thread —
    /// long-running fetches block other HTTP mail until they finish.
    #[handler::single]
    fn on_fetch(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: Fetch) -> FetchResult {
        let timeout = mail.timeout_ms.map_or(state.default_timeout, |ms| Duration::from_millis(u64::from(ms)));

        let url = mail.url.clone();
        let adapter_req =
            FetchRequest { url: mail.url, method: mail.method, headers: mail.headers, body: mail.body, timeout };

        match state.adapter.fetch(adapter_req) {
            Ok(r) => FetchResult::Ok { url, status: r.status, headers: r.headers, body: r.body },
            Err(error) => FetchResult::Err { url, error },
        }
    }
}

/// `ureq`-backed adapter. Holds the shared agent, the initial-host allowlist
/// (empty = deny all), the response cap, and the `require_https`
/// flag. Thread-safe: `ureq::Agent` is cheaply cloneable and
/// internally synchronised, so the same adapter drives the cap from
/// one dispatch thread today and would parallelise cleanly behind a
/// multi-thread dispatcher later.
pub struct UreqHttpAdapter {
    agent: ureq::Agent,
    allowlist: HashSet<String>,
    require_https: bool,
    max_body_bytes: usize,
}

impl UreqHttpAdapter {
    /// Construct an adapter with explicit knobs. Chassis code uses
    /// [`build_http_adapter`] for env-derived construction;
    /// tests build adapters directly to avoid env contamination.
    #[must_use]
    pub fn new(allowlist: HashSet<String>, require_https: bool, max_body_bytes: usize) -> Self {
        // ureq's default redirect policy is intentionally unchanged here. The
        // adapter validates only the initial URL host; redirect targets are not
        // rechecked against `allowlist` in the current implementation.
        let config = ureq::Agent::config_builder().http_status_as_error(false).build();
        let agent = ureq::Agent::new_with_config(config);
        Self { agent, allowlist, require_https, max_body_bytes }
    }

    fn check_allowlist(&self, host: &str) -> Result<(), HttpError> {
        if self.allowlist.contains(host) {
            Ok(())
        } else {
            Err(HttpError::AllowlistDenied)
        }
    }
}

impl HttpAdapter for UreqHttpAdapter {
    fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, HttpError> {
        use ureq::RequestExt;

        let parsed = url::Url::parse(&req.url).map_err(|e| HttpError::InvalidUrl(format!("{e}")))?;

        if self.require_https && parsed.scheme() != "https" {
            return Err(HttpError::InvalidUrl("http scheme not allowed (AETHER_HTTP_REQUIRE_HTTPS=1)".to_string()));
        }

        let host = parsed.host_str().ok_or_else(|| HttpError::InvalidUrl("no host in url".to_string()))?;
        self.check_allowlist(host)?;

        if req.body.len() > self.max_body_bytes {
            return Err(HttpError::BodyTooLarge);
        }

        let mut builder = Request::builder().method(http_method_to_http_crate(req.method)).uri(&req.url);

        // Host header is derived from the URL by ureq; reject any
        // caller-set Host so it can't be used to bypass the
        // allowlist (component requests allowlisted A, TLS SNI is A,
        // but `Host: B` routes the vhost to B server-side). User-
        // Agent defaults to `aether/<version>` if not set.
        let mut saw_user_agent = false;
        for h in &req.headers {
            if h.name.eq_ignore_ascii_case("host") {
                tracing::warn!(
                    target: "aether_substrate::http",
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

        let http_req = builder.body(req.body).map_err(|e| HttpError::InvalidUrl(format!("{e}")))?;

        let mut response = http_req
            .with_agent(&self.agent)
            .configure()
            .timeout_global(Some(req.timeout))
            .build()
            .run()
            .map_err(ureq_error_to_http_error)?;

        let status = response.status().as_u16();

        let mut headers = Vec::with_capacity(response.headers().len());
        for (name, value) in response.headers() {
            // Non-UTF8 header values are rare but real (binary
            // cookies, broken servers). Skip rather than fail the
            // whole fetch.
            if let Ok(value_str) = value.to_str() {
                headers.push(HttpHeader { name: name.as_str().to_string(), value: value_str.to_string() });
            }
        }

        let body = match response.body_mut().with_config().limit(self.max_body_bytes as u64).read_to_vec() {
            Ok(b) => b,
            Err(ureq::Error::BodyExceedsLimit(_)) => return Err(HttpError::BodyTooLarge),
            Err(e) => return Err(HttpError::AdapterError(format!("body read: {e}"))),
        };

        Ok(FetchResponse { status, headers, body })
    }
}

fn http_method_to_http_crate(m: HttpMethod) -> Method {
    match m {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Head => Method::HEAD,
        HttpMethod::Options => Method::OPTIONS,
    }
}

fn ureq_error_to_http_error(e: ureq::Error) -> HttpError {
    match e {
        ureq::Error::Timeout(_) => HttpError::Timeout,
        ureq::Error::BodyExceedsLimit(_) => HttpError::BodyTooLarge,
        other => HttpError::AdapterError(format!("{other}")),
    }
}

/// Build an HTTP adapter from explicit configuration.
pub fn build_http_adapter(config: HttpConfig) -> Arc<dyn HttpAdapter> {
    if config.disabled {
        tracing::info!(
            target: "aether_substrate::http",
            "http adapter disabled — every fetch replies Disabled",
        );
        return Arc::new(DisabledHttpAdapter);
    }

    tracing::info!(
        target: "aether_substrate::http",
        allowlist_size = config.allowlist.len(),
        require_https = config.require_https,
        max_body_bytes = config.max_body_bytes,
        "http adapter configured",
    );

    Arc::new(UreqHttpAdapter::new(config.allowlist, config.require_https, config.max_body_bytes))
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::{FetchRequest, FetchResponse, HttpAdapter, HttpCapabilityState, UreqHttpAdapter, build_http_adapter};
    use crate::http::client::{DEFAULT_MAX_BODY_BYTES, HttpCapability, HttpConfig};
    use crate::http::kinds::{Fetch, FetchResult, HttpError, HttpHeader, HttpMethod};
    use aether_data::MailboxId;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::ctx::NativeCtx;
    use aether_substrate::mail::Source;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // ADR-0090: the defaults check loads the layer with no `.env()`
    // source. The env-value behavior (trim, empty → default, garbage
    // → hard-error) is now confique's native deserialization, covered
    // by `aether_substrate::config`'s confique tests; the CSV split is
    // covered by `parse_csv_set` there.

    struct StubAdapter {
        response: Mutex<Option<Result<FetchResponse, HttpError>>>,
        last_request: Mutex<Option<FetchRequest>>,
    }

    impl StubAdapter {
        fn with(response: Result<FetchResponse, HttpError>) -> Arc<Self> {
            Arc::new(Self { response: Mutex::new(Some(response)), last_request: Mutex::new(None) })
        }
    }

    impl HttpAdapter for StubAdapter {
        fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, HttpError> {
            *self.last_request.lock().expect("test stub: last_request mutex poisoned") = Some(FetchRequest {
                url: req.url.clone(),
                method: req.method,
                headers: req.headers.clone(),
                body: req.body.clone(),
                timeout: req.timeout,
            });
            self.response
                .lock()
                .expect("test stub: response mutex poisoned")
                .take()
                .expect("stub response already consumed")
        }
    }

    use aether_data::{SessionToken, SourceAddr, Uuid};

    fn session_sender() -> Source {
        Source::to(SourceAddr::Session(SessionToken(Uuid::nil())))
    }

    use aether_substrate::testing::test_mailer_and_rx;

    #[test]
    fn allowlist_empty_rejects_every_host() {
        let adapter = UreqHttpAdapter::new(HashSet::new(), false, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "https://api.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(HttpError::AllowlistDenied)));
    }

    #[test]
    fn allowlist_miss_returns_denied_without_making_request() {
        let mut allowlist = HashSet::new();
        allowlist.insert("allowed.example.com".to_string());
        let adapter = UreqHttpAdapter::new(allowlist, false, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "https://denied.example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(HttpError::AllowlistDenied)));
    }

    #[test]
    fn invalid_url_returns_invalid_url_variant() {
        let adapter = UreqHttpAdapter::new(HashSet::new(), false, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "not-a-url".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(HttpError::InvalidUrl(_))));
    }

    #[test]
    fn require_https_rejects_http_scheme() {
        let mut allowlist = HashSet::new();
        allowlist.insert("example.com".to_string());
        let adapter = UreqHttpAdapter::new(allowlist, true, DEFAULT_MAX_BODY_BYTES);
        let resp = adapter.fetch(FetchRequest {
            url: "http://example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(HttpError::InvalidUrl(_))));
    }

    #[test]
    fn oversize_request_body_returns_body_too_large() {
        let mut allowlist = HashSet::new();
        allowlist.insert("example.com".to_string());
        let adapter = UreqHttpAdapter::new(allowlist, false, 10);
        let resp = adapter.fetch(FetchRequest {
            url: "https://example.com/".to_string(),
            method: HttpMethod::Post,
            headers: vec![],
            body: vec![0u8; 20],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(HttpError::BodyTooLarge)));
    }

    #[test]
    fn cap_fetch_ok_replies_with_response() {
        let (mailer, _) = test_mailer_and_rx();
        let stub = StubAdapter::with(Ok(FetchResponse {
            status: 200,
            headers: vec![HttpHeader { name: "content-type".to_string(), value: "application/json".to_string() }],
            body: b"{}".to_vec(),
        }));
        let mut state =
            HttpCapabilityState::from_adapter(stub as Arc<dyn HttpAdapter>, HttpConfig::default().default_timeout);
        let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
        let mut ctx =
            NativeCtx::new(&transport, session_sender(), aether_data::MailId::NONE, aether_data::MailId::NONE);
        match HttpCapability::on_fetch(
            &mut state,
            &mut ctx,
            Fetch {
                url: "https://api.example.com/v1".to_string(),
                method: HttpMethod::Get,
                headers: vec![],
                body: vec![],
                timeout_ms: Some(5000),
            },
        ) {
            FetchResult::Ok { url, status, headers, body } => {
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
        let (mailer, _) = test_mailer_and_rx();
        let mut state = HttpCapabilityState::from_adapter(
            StubAdapter::with(Err(HttpError::Timeout)) as Arc<dyn HttpAdapter>,
            HttpConfig::default().default_timeout,
        );
        let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
        let mut ctx =
            NativeCtx::new(&transport, session_sender(), aether_data::MailId::NONE, aether_data::MailId::NONE);
        match HttpCapability::on_fetch(
            &mut state,
            &mut ctx,
            Fetch {
                url: "https://slow.example.com/".to_string(),
                method: HttpMethod::Get,
                headers: vec![],
                body: vec![],
                timeout_ms: None,
            },
        ) {
            FetchResult::Err { url, error } => {
                assert_eq!(url, "https://slow.example.com/");
                assert_eq!(error, HttpError::Timeout);
            }
            FetchResult::Ok { .. } => panic!("expected Err"),
        }
    }

    #[test]
    fn cap_uses_default_timeout_when_none_provided() {
        let (mailer, _rx) = test_mailer_and_rx();
        let stub = StubAdapter::with(Ok(FetchResponse { status: 200, headers: vec![], body: vec![] }));
        let stub_clone = Arc::clone(&stub);
        let mut state =
            HttpCapabilityState::from_adapter(stub as Arc<dyn HttpAdapter>, HttpConfig::default().default_timeout);
        let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
        let mut ctx =
            NativeCtx::new(&transport, session_sender(), aether_data::MailId::NONE, aether_data::MailId::NONE);
        let _ = HttpCapability::on_fetch(
            &mut state,
            &mut ctx,
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
            .expect("test stub: last_request mutex poisoned")
            .take()
            .expect("adapter was not called");
        assert!(observed.timeout > Duration::ZERO);
    }

    #[test]
    fn build_http_adapter_with_disable_returns_disabled() {
        let cfg = HttpConfig { disabled: true, ..HttpConfig::default() };
        let a = build_http_adapter(cfg);
        let resp = a.fetch(FetchRequest {
            url: "https://example.com/".to_string(),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(30),
        });
        assert!(matches!(resp, Err(HttpError::Disabled)));
    }
}
