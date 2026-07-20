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

use crate::kinds::{Fetch, FetchResult, HttpError, HttpHeader, HttpMethod};

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

/// Redirect budget for [`UreqHttpAdapter::fetch`]'s hand-rolled follow loop
/// (issue #3463). Not an operator concern — a fixed cap, not a config field.
const MAX_REDIRECTS: usize = 10;

/// Per-hop redirect decision, returned by the pure [`classify_redirect`].
/// `Deny` carries the same `HttpError` a direct request to that URL would
/// have returned, so a denied redirect is indistinguishable from a direct
/// denial at the adapter's error boundary.
#[derive(Debug, PartialEq, Eq)]
enum RedirectDecision {
    /// Not a followable redirect (non-3xx, no `Location`, or budget spent) —
    /// return the response to the caller as-is.
    Return,
    /// A 3xx to an allowlisted, scheme-valid host — follow it.
    Follow(url::Url),
    /// A 3xx to a denied host or scheme — fail closed with the same error
    /// a direct request to that target would produce.
    Deny(HttpError),
}

/// Decide whether to follow one redirect hop. Pure and unit-testable: given
/// the just-received status/`Location` and the current URL, re-runs the same
/// `require_https` + allowlist checks the initial URL gets, so every hop that
/// actually egresses is validated (the SSRF-adjacent gap issue #3463 closes).
fn classify_redirect(
    status: u16,
    location: Option<&str>,
    current: &url::Url,
    allowlist: &HashSet<String>,
    require_https: bool,
    hops_left: usize,
) -> RedirectDecision {
    if !(300..400).contains(&status) || hops_left == 0 {
        return RedirectDecision::Return;
    }
    let Some(location) = location else {
        return RedirectDecision::Return;
    };
    let Ok(target) = current.join(location) else {
        return RedirectDecision::Return;
    };

    if require_https && target.scheme() != "https" {
        return RedirectDecision::Deny(HttpError::InvalidUrl(
            "http scheme not allowed (AETHER_HTTP_REQUIRE_HTTPS=1)".to_string(),
        ));
    }
    let Some(host) = target.host_str() else {
        return RedirectDecision::Deny(HttpError::InvalidUrl("no host in url".to_string()));
    };
    if let Err(error) = check_allowlist(allowlist, host) {
        return RedirectDecision::Deny(error);
    }

    RedirectDecision::Follow(target)
}

/// Shared allowlist gate: both the initial-URL path
/// ([`UreqHttpAdapter::fetch_once`]) and the redirect-hop path
/// ([`classify_redirect`]) call this one function, so a future change to
/// allowlist semantics (case-folding, wildcard support, …) can't silently
/// apply to one path and not the other.
fn check_allowlist(allowlist: &HashSet<String>, host: &str) -> Result<(), HttpError> {
    if allowlist.contains(host) {
        Ok(())
    } else {
        Err(HttpError::AllowlistDenied)
    }
}

/// The standard redirect method/body rule: 303 always drops to GET with no
/// body; 307/308 preserve the original method and body (unlike `ureq`'s own
/// automatic follower, which bails on a body-bearing method rather than
/// resend it — our loop keeps the body, so it can); any other 3xx (301, 302,
/// …) follows curl's convention — GET/HEAD keep their method, everything
/// else becomes GET, body dropped either way. Returns `(method, keep_body)`.
fn redirect_method_and_body(status: u16, method: HttpMethod) -> (HttpMethod, bool) {
    match status {
        303 => (HttpMethod::Get, false),
        307 | 308 => (method, true),
        _ if matches!(method, HttpMethod::Get | HttpMethod::Head) => (method, false),
        _ => (HttpMethod::Get, false),
    }
}

/// Strip credential-bearing headers before re-issuing a cross-host redirect
/// — mirrors the protection `ureq`'s built-in follower gives via
/// `RedirectAuthHeaders::SameHost`, which a hand-rolled loop must reproduce
/// rather than silently drop.
fn strip_credential_headers_for_redirect(headers: &mut Vec<HttpHeader>) {
    headers.retain(|h| {
        !h.name.eq_ignore_ascii_case("authorization")
            && !h.name.eq_ignore_ascii_case("cookie")
            && !h.name.eq_ignore_ascii_case("proxy-authorization")
    });
}

fn find_header_value(headers: &[HttpHeader], name: &str) -> Option<String> {
    headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)).map(|h| h.value.clone())
}

/// Whether a redirect hop must drop credential headers before re-issuing:
/// it crosses hosts, or it isn't `https` (a same-host downgrade to plain
/// `http` must not carry credentials over the wire in the clear either).
fn redirect_needs_credential_strip(current: &url::Url, target: &url::Url) -> bool {
    current.host_str() != target.host_str() || target.scheme() != "https"
}

impl UreqHttpAdapter {
    /// Construct an adapter with explicit knobs. Chassis code uses
    /// [`build_http_adapter`] for env-derived construction;
    /// tests build adapters directly to avoid env contamination.
    #[must_use]
    pub fn new(allowlist: HashSet<String>, require_https: bool, max_body_bytes: usize) -> Self {
        // Auto-redirect-following is off (`max_redirects(0)`): `fetch` runs
        // its own follow loop so every hop — not just the initial URL — is
        // re-validated against `allowlist` and `require_https` before it is
        // dialed (issue #3463). With `max_redirects` at 0,
        // `max_redirects_will_error` never triggers (it only fires above 0
        // redirects), so an unfollowed 3xx returns as an ordinary response.
        let config = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build();
        let agent = ureq::Agent::new_with_config(config);
        Self { agent, allowlist, require_https, max_body_bytes }
    }

    /// Validate one URL against `require_https` + the allowlist and issue
    /// exactly one non-following request. The redirect follow loop lives in
    /// [`HttpAdapter::fetch`]; this runs the same gate the initial URL gets,
    /// for whichever URL — initial or a re-validated redirect target — is
    /// current.
    fn fetch_once(
        &self,
        url: &str,
        method: HttpMethod,
        headers: &[HttpHeader],
        body: &[u8],
        timeout: Duration,
    ) -> Result<FetchResponse, HttpError> {
        use ureq::RequestExt;

        let parsed = url::Url::parse(url).map_err(|e| HttpError::InvalidUrl(format!("{e}")))?;

        if self.require_https && parsed.scheme() != "https" {
            return Err(HttpError::InvalidUrl("http scheme not allowed (AETHER_HTTP_REQUIRE_HTTPS=1)".to_string()));
        }

        let host = parsed.host_str().ok_or_else(|| HttpError::InvalidUrl("no host in url".to_string()))?;
        check_allowlist(&self.allowlist, host)?;

        if body.len() > self.max_body_bytes {
            return Err(HttpError::BodyTooLarge);
        }

        let mut builder = Request::builder().method(http_method_to_http_crate(method)).uri(url);

        // Host header is derived from the URL by ureq; reject any
        // caller-set Host so it can't be used to bypass the
        // allowlist (component requests allowlisted A, TLS SNI is A,
        // but `Host: B` routes the vhost to B server-side). User-
        // Agent defaults to `aether/<version>` if not set.
        let mut saw_user_agent = false;
        for h in headers {
            if h.name.eq_ignore_ascii_case("host") {
                tracing::warn!(
                    target: "aether_http",
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

        let http_req = builder.body(body.to_vec()).map_err(|e| HttpError::InvalidUrl(format!("{e}")))?;

        let mut response = http_req
            .with_agent(&self.agent)
            .configure()
            .timeout_global(Some(timeout))
            .build()
            .run()
            .map_err(ureq_error_to_http_error)?;

        let status = response.status().as_u16();

        let mut resp_headers = Vec::with_capacity(response.headers().len());
        for (name, value) in response.headers() {
            // Non-UTF8 header values are rare but real (binary
            // cookies, broken servers). Skip rather than fail the
            // whole fetch.
            if let Ok(value_str) = value.to_str() {
                resp_headers.push(HttpHeader { name: name.as_str().to_string(), value: value_str.to_string() });
            }
        }

        let body = match response.body_mut().with_config().limit(self.max_body_bytes as u64).read_to_vec() {
            Ok(b) => b,
            Err(ureq::Error::BodyExceedsLimit(_)) => return Err(HttpError::BodyTooLarge),
            Err(e) => return Err(HttpError::AdapterError(format!("body read: {e}"))),
        };

        Ok(FetchResponse { status, headers: resp_headers, body })
    }
}

impl HttpAdapter for UreqHttpAdapter {
    fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, HttpError> {
        let mut current = url::Url::parse(&req.url).map_err(|e| HttpError::InvalidUrl(format!("{e}")))?;
        let mut method = req.method;
        let mut headers = req.headers;
        let mut body = req.body;
        let mut hops_left = MAX_REDIRECTS;

        loop {
            let response = self.fetch_once(current.as_str(), method, &headers, &body, req.timeout)?;
            let location = find_header_value(&response.headers, "location");

            match classify_redirect(
                response.status,
                location.as_deref(),
                &current,
                &self.allowlist,
                self.require_https,
                hops_left,
            ) {
                RedirectDecision::Return => return Ok(response),
                RedirectDecision::Deny(error) => return Err(error),
                RedirectDecision::Follow(target) => {
                    let (new_method, keep_body) = redirect_method_and_body(response.status, method);
                    if redirect_needs_credential_strip(&current, &target) {
                        strip_credential_headers_for_redirect(&mut headers);
                    }
                    method = new_method;
                    if !keep_body {
                        body = Vec::new();
                    }
                    current = target;
                    hops_left -= 1;
                }
            }
        }
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
            target: "aether_http",
            "http adapter disabled — every fetch replies Disabled",
        );
        return Arc::new(DisabledHttpAdapter);
    }

    tracing::info!(
        target: "aether_http",
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
    use crate::client::{DEFAULT_MAX_BODY_BYTES, HttpCapability, HttpConfig};
    use crate::kinds::{Fetch, FetchResult, HttpError, HttpHeader, HttpMethod};
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

    // issue #3463: per-hop redirect re-validation. `classify_redirect` is
    // the pure seam carrying the SSRF-boundary invariant, so it is tested
    // directly rather than through a live redirecting server.
    mod redirect_classification {
        use super::super::{RedirectDecision, classify_redirect, redirect_method_and_body};
        use crate::kinds::{HttpError, HttpMethod};
        use std::collections::HashSet;

        fn allowlist(hosts: &[&str]) -> HashSet<String> {
            hosts.iter().map(|h| (*h).to_string()).collect()
        }

        fn url(s: &str) -> url::Url {
            url::Url::parse(s).expect("test fixture URL must parse")
        }

        #[test]
        fn redirect_to_denied_host_is_denied() {
            // Tripwire: the SSRF boundary this issue closes — a redirect
            // target outside the allowlist must fail exactly like a direct
            // request to that host would.
            let current = url("https://allowed.example.com/start");
            let decision = classify_redirect(
                302,
                Some("https://denied.example.com/next"),
                &current,
                &allowlist(&["allowed.example.com"]),
                false,
                10,
            );
            assert!(matches!(decision, RedirectDecision::Deny(HttpError::AllowlistDenied)));
        }

        #[test]
        fn redirect_within_allowlist_follows() {
            let current = url("https://allowed.example.com/start");
            let decision = classify_redirect(
                302,
                Some("https://allowed.example.com/next"),
                &current,
                &allowlist(&["allowed.example.com"]),
                false,
                10,
            );
            match decision {
                RedirectDecision::Follow(target) => assert_eq!(target.as_str(), "https://allowed.example.com/next"),
                other => panic!("expected Follow, got {other:?}"),
            }
        }

        #[test]
        fn non_redirect_status_returns() {
            let current = url("https://allowed.example.com/start");
            let decision = classify_redirect(200, None, &current, &allowlist(&["allowed.example.com"]), false, 10);
            assert_eq!(decision, RedirectDecision::Return);
        }

        #[test]
        fn redirect_without_location_returns() {
            let current = url("https://allowed.example.com/start");
            let decision = classify_redirect(302, None, &current, &allowlist(&["allowed.example.com"]), false, 10);
            assert_eq!(decision, RedirectDecision::Return);
        }

        #[test]
        fn http_hop_denied_under_require_https() {
            let current = url("https://allowed.example.com/start");
            let decision = classify_redirect(
                302,
                Some("http://allowed.example.com/next"),
                &current,
                &allowlist(&["allowed.example.com"]),
                true,
                10,
            );
            assert!(matches!(decision, RedirectDecision::Deny(HttpError::InvalidUrl(_))));
        }

        #[test]
        fn spent_budget_returns_instead_of_following() {
            let current = url("https://allowed.example.com/start");
            let decision = classify_redirect(
                302,
                Some("https://allowed.example.com/next"),
                &current,
                &allowlist(&["allowed.example.com"]),
                false,
                0,
            );
            assert_eq!(decision, RedirectDecision::Return);
        }

        #[test]
        fn redirect_method_and_body_rule() {
            assert_eq!(redirect_method_and_body(303, HttpMethod::Post), (HttpMethod::Get, false));
            assert_eq!(redirect_method_and_body(307, HttpMethod::Post), (HttpMethod::Post, true));
            assert_eq!(redirect_method_and_body(308, HttpMethod::Put), (HttpMethod::Put, true));
            assert_eq!(redirect_method_and_body(302, HttpMethod::Get), (HttpMethod::Get, false));
            assert_eq!(redirect_method_and_body(301, HttpMethod::Post), (HttpMethod::Get, false));
        }
    }

    #[test]
    fn cross_host_redirect_strips_credential_headers() {
        let mut headers = vec![
            HttpHeader { name: "Authorization".to_string(), value: "Bearer secret".to_string() },
            HttpHeader { name: "Cookie".to_string(), value: "session=abc".to_string() },
            HttpHeader { name: "Proxy-Authorization".to_string(), value: "Basic xyz".to_string() },
            HttpHeader { name: "Accept".to_string(), value: "application/json".to_string() },
        ];
        super::strip_credential_headers_for_redirect(&mut headers);
        assert_eq!(headers, vec![HttpHeader { name: "Accept".to_string(), value: "application/json".to_string() }]);
    }

    #[test]
    fn redirect_needs_credential_strip_detects_cross_host_and_downgrade() {
        let current = url::Url::parse("https://allowed.example.com/start").expect("test fixture URL must parse");
        let same_host_https = url::Url::parse("https://allowed.example.com/next").expect("test fixture URL must parse");
        let cross_host = url::Url::parse("https://other.example.com/next").expect("test fixture URL must parse");
        let same_host_downgrade =
            url::Url::parse("http://allowed.example.com/next").expect("test fixture URL must parse");
        assert!(!super::redirect_needs_credential_strip(&current, &same_host_https));
        assert!(super::redirect_needs_credential_strip(&current, &cross_host));
        assert!(super::redirect_needs_credential_strip(&current, &same_host_downgrade));
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // test-only loopback server thread; no actor lineage or runtime work.
    fn redirect_to_denied_host_end_to_end_never_connects_onward() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        // A local server the allowlist permits, which 302s to a host the
        // allowlist denies. Proves `fetch` never dials the second hop: if it
        // did, the denied host wouldn't resolve/connect and the error would
        // be an adapter/connect failure rather than `AllowlistDenied`.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener local addr");

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept one connection");
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).expect("read request");
            let response = b"HTTP/1.1 302 Found\r\n\
                Location: https://denied.example.invalid/\r\n\
                Content-Length: 0\r\n\
                Connection: close\r\n\
                \r\n";
            stream.write_all(response).expect("write redirect response");
        });

        let mut allowlist = HashSet::new();
        allowlist.insert(addr.ip().to_string());
        let adapter = UreqHttpAdapter::new(allowlist, false, DEFAULT_MAX_BODY_BYTES);

        let resp = adapter.fetch(FetchRequest {
            url: format!("http://{addr}/"),
            method: HttpMethod::Get,
            headers: vec![],
            body: vec![],
            timeout: Duration::from_secs(5),
        });

        assert!(matches!(resp, Err(HttpError::AllowlistDenied)));
        server.join().expect("server thread panicked");
    }
}
