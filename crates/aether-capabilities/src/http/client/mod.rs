//! `aether.http` cap. Owns the full HTTP egress stack — `HttpAdapter`
//! trait, the `ureq`-backed `UreqHttpAdapter`, env-driven `HttpConfig`,
//! and the [`HttpCapability`] itself. Chassis mains resolve a
//! [`HttpConfig`] (typically via [`HttpConfig::from_env`]) and pass it
//! to `with_actor::<HttpCapability>(config)`.
//!
//! v1 semantics (ADR-0043):
//! - Blocking on the dispatcher thread (one request at a time).
//! - Buffered request + response bodies; streaming is deferred.
//! - Deny-by-default allowlist via `AETHER_HTTP_ALLOWLIST`.
//! - Response size capped at `AETHER_HTTP_MAX_BODY_BYTES` (16MB).
//! - Default request timeout 30s, per-request override via
//!   `Fetch.timeout_ms`.

use std::time::Duration;

mod config;

pub use config::HttpConfig;
// The `Config` derive on `HttpConfig` emits these native-only sibling types
// in `config`; chassis CLI / boot wiring addresses them through the
// `client::` path, so re-export them here.
#[cfg(feature = "native")]
pub use config::{HttpConfigLayer, HttpOverlay};

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use crate::http::kinds::{Fetch, HttpError, HttpHeader, HttpMethod};
use aether_actor::WasmActorMailbox;
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

/// Default response-body cap when `AETHER_HTTP_MAX_BODY_BYTES` is
/// unset. 16MB matches ADR-0043 §3.
pub const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Default per-request timeout when `AETHER_HTTP_TIMEOUT_MS` is unset
/// and the fetch itself supplies no `timeout_ms`. 30s matches
/// ADR-0043 §4.
pub const DEFAULT_TIMEOUT_MILLIS: u32 = 30_000;

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

/// Sender-side facade for actors addressed via
/// `ctx.actor::<HttpCapability>()`.
///
/// Lifts the two most common HTTP verbs to a typed method so callers
/// stop reconstructing `Fetch { method: HttpMethod::Get, headers:
/// vec![], body: vec![], timeout_ms: None, .. }` for a basic
/// request. Same shape and rationale as [`crate::fs::FsMailboxExt`].
///
/// All methods are fire-and-forget. Replies arrive as
/// `aether.http.fetch_result`, correlated by the echoed `url`
/// (ADR-0043).
///
/// For requests that need custom headers, body, method, or a
/// per-request timeout, the generic escape hatch is unchanged:
/// `mailbox.send(&Fetch { ... })` still works because the cap
/// declares `HandlesKind<Fetch>`. The facade only exists for the
/// no-options cases that don't benefit from spelling out a five-
/// field struct.
///
/// Impl'd for both transports `ctx.actor::<HttpCapability>()` can
/// return:
///
/// - [`WasmActorMailbox<HttpCapability>`] — always-on, for
///   wasm-component callers.
/// - [`NativeActorMailbox<'_, HttpCapability>`] — native cap-to-cap
///   sends, gated on `#[cfg(not(target_arch = "wasm32"))]`.
pub trait HttpMailboxExt {
    /// Mail `aether.http.fetch { url, method: Get, headers: [], body: [], timeout_ms: None }`
    /// to the cap. Uses the chassis default timeout.
    fn get(&self, url: &str);

    /// Mail `aether.http.fetch { url, method: Post, headers: [], body, timeout_ms: None }`
    /// to the cap. Uses the chassis default timeout.
    fn post(&self, url: &str, body: &[u8]);
}

impl HttpMailboxExt for WasmActorMailbox<'_, HttpCapability> {
    //noinspection DuplicatedCode
    fn get(&self, url: &str) {
        self.send(&Fetch {
            url: url.into(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: None,
        });
    }
    //noinspection DuplicatedCode
    fn post(&self, url: &str, body: &[u8]) {
        self.send(&Fetch {
            url: url.into(),
            method: HttpMethod::Post,
            headers: Vec::new(),
            body: body.to_vec(),
            timeout_ms: None,
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HttpMailboxExt for NativeActorMailbox<'_, HttpCapability> {
    //noinspection DuplicatedCode
    fn get(&self, url: &str) {
        self.send(&Fetch {
            url: url.into(),
            method: HttpMethod::Get,
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: None,
        });
    }
    //noinspection DuplicatedCode
    fn post(&self, url: &str, body: &[u8]) {
        self.send(&Fetch {
            url: url.into(),
            method: HttpMethod::Post,
            headers: Vec::new(),
            body: body.to_vec(),
            timeout_ms: None,
        });
    }
}

#[aether_actor::bridge(singleton)]
mod native {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        DisabledHttpAdapter, Fetch, FetchRequest, FetchResponse, HttpAdapter, HttpConfig,
        HttpError, HttpHeader, HttpMethod,
    };
    use crate::http::kinds::FetchResult;
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use ureq::http::Method;
    use ureq::http::Request;

    /// `ureq`-backed adapter. Holds the shared agent, the allowlist
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

            let parsed =
                url::Url::parse(&req.url).map_err(|e| HttpError::InvalidUrl(format!("{e}")))?;

            if self.require_https && parsed.scheme() != "https" {
                return Err(HttpError::InvalidUrl(
                    "http scheme not allowed (AETHER_HTTP_REQUIRE_HTTPS=1)".to_string(),
                ));
            }

            let host = parsed
                .host_str()
                .ok_or_else(|| HttpError::InvalidUrl("no host in url".to_string()))?;
            self.check_allowlist(host)?;

            if req.body.len() > self.max_body_bytes {
                return Err(HttpError::BodyTooLarge);
            }

            let mut builder = Request::builder()
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
                builder =
                    builder.header("User-Agent", concat!("aether/", env!("CARGO_PKG_VERSION")));
            }

            let http_req = builder
                .body(req.body)
                .map_err(|e| HttpError::InvalidUrl(format!("{e}")))?;

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
                Err(ureq::Error::BodyExceedsLimit(_)) => return Err(HttpError::BodyTooLarge),
                Err(e) => return Err(HttpError::AdapterError(format!("body read: {e}"))),
            };

            Ok(FetchResponse {
                status,
                headers,
                body,
            })
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

        Arc::new(UreqHttpAdapter::new(
            config.allowlist,
            config.require_https,
            config.max_body_bytes,
        ))
    }

    /// `aether.http` mailbox cap. Owns the resolved adapter and the
    /// default per-request timeout applied when `Fetch.timeout_ms` is
    /// `None`. The dispatcher thread holds an `Arc<Self>` and routes
    /// envelopes through the macro-emitted `NativeDispatch` impl;
    /// replies route via `ctx.reply(&result)` through the substrate's
    /// `Mailer::send_reply`.
    pub struct HttpCapability {
        adapter: Arc<dyn HttpAdapter>,
        default_timeout: Duration,
    }

    #[cfg(test)]
    impl HttpCapability {
        /// Test-only direct constructor. Production boots through
        /// `Builder::with_actor::<HttpCapability>(config)` which calls
        /// `init`; tests that drive the cap with a stub adapter hand it
        /// in directly.
        pub(crate) fn from_adapter(
            adapter: Arc<dyn HttpAdapter>,
            default_timeout: Duration,
        ) -> Self {
            Self {
                adapter,
                default_timeout,
            }
        }
    }

    #[actor]
    impl NativeActor for HttpCapability {
        type Config = HttpConfig;

        /// ADR-0043 + ADR-0074 Phase 5 chassis-owned mailbox.
        const NAMESPACE: &'static str = "aether.http";

        /// Build the HTTP adapter from the resolved config. The adapter is
        /// built immediately so configuration errors surface at chassis-
        /// builder time, not at first fetch.
        fn init(config: HttpConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let default_timeout = config.default_timeout;
            Ok(Self {
                adapter: build_http_adapter(config),
                default_timeout,
            })
        }

        /// Run a fetch request and reply with the response.
        ///
        /// # Agent
        /// Reply: `FetchResult`. Synchronous on the dispatcher thread —
        /// long-running fetches block other HTTP mail until they finish.
        #[handler]
        fn on_fetch(&self, _ctx: &mut NativeCtx<'_>, mail: Fetch) -> FetchResult {
            let timeout = mail.timeout_ms.map_or(self.default_timeout, |ms| {
                Duration::from_millis(u64::from(ms))
            });

            let url = mail.url.clone();
            let adapter_req = FetchRequest {
                url: mail.url,
                method: mail.method,
                headers: mail.headers,
                body: mail.body,
                timeout,
            };

            match self.adapter.fetch(adapter_req) {
                Ok(r) => FetchResult::Ok {
                    url,
                    status: r.status,
                    headers: r.headers,
                    body: r.body,
                },
                Err(error) => FetchResult::Err { url, error },
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Mutex;

        use super::super::{
            DEFAULT_MAX_BODY_BYTES, DisabledHttpAdapter, FetchRequest, FetchResponse, HttpAdapter,
            HttpConfig, HttpError, HttpHeader, HttpMethod,
        };
        use super::{
            Arc, Duration, Fetch, FetchResult, HashSet, HttpCapability, UreqHttpAdapter,
            build_http_adapter,
        };
        use aether_actor::Addressable;
        use aether_data::MailboxId;
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::actor::native::ctx::NativeCtx;
        use aether_substrate::chassis::builder::Builder;
        use aether_substrate::chassis::error::BootError;
        use aether_substrate::mail::Source;

        use crate::test_chassis::{TestChassis, fresh_substrate};
        use aether_substrate::mail::registry;

        // ADR-0090: the defaults check loads the layer with no `.env()`
        // source. The env-value behavior (trim, empty → default, garbage
        // → hard-error) is now confique's native deserialization, covered
        // by `aether_substrate::config`'s confique tests; the CSV split is
        // covered by `parse_csv_set` there.

        #[test]
        fn http_from_env_defaults_match() {
            use super::super::HttpConfigLayer;
            use confique::Config as _;
            // No `.env()` source: loads literal defaults only, so this is
            // env-free and guards the layer's literal defaults against the
            // named consts + `HttpConfig::default()`.
            let layer = HttpConfigLayer::builder().load().expect("defaults load");
            let default = HttpConfig::default();
            assert!(!layer.disabled);
            assert!(layer.allowlist.is_empty());
            assert!(!layer.require_https);
            assert_eq!(layer.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
            assert_eq!(
                Duration::from_millis(u64::from(layer.timeout_ms)),
                default.default_timeout
            );
        }

        struct StubAdapter {
            response: Mutex<Option<Result<FetchResponse, HttpError>>>,
            last_request: Mutex<Option<FetchRequest>>,
        }

        impl StubAdapter {
            fn with(response: Result<FetchResponse, HttpError>) -> Arc<Self> {
                Arc::new(Self {
                    response: Mutex::new(Some(response)),
                    last_request: Mutex::new(None),
                })
            }
        }

        impl HttpAdapter for StubAdapter {
            fn fetch(&self, req: FetchRequest) -> Result<FetchResponse, HttpError> {
                *self
                    .last_request
                    .lock()
                    .expect("test stub: last_request mutex poisoned") = Some(FetchRequest {
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

        use crate::test_chassis::test_mailer_and_rx;

        /// Boot the cap against a default disabled `HttpConfig` and confirm
        /// the mailbox is registered.
        #[test]
        fn capability_boots_and_registers_mailbox() {
            let (registry, mailer) = fresh_substrate();
            let config = HttpConfig {
                disabled: true,
                ..HttpConfig::default()
            };
            let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HttpCapability>(config)
                .build_passive()
                .expect("http capability boots");
            assert!(
                registry.lookup(HttpCapability::NAMESPACE).is_some(),
                "http mailbox registered"
            );
            drop(chassis);
        }

        /// Builder rejects a duplicate claim.
        #[test]
        fn duplicate_claim_rejects_with_typed_error() {
            let (registry, mailer) = fresh_substrate();
            registry.register_inbox(HttpCapability::NAMESPACE, registry::noop_handler());
            let config = HttpConfig {
                disabled: true,
                ..HttpConfig::default()
            };

            let err = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<HttpCapability>(config)
                .build_passive()
                .expect_err("collision must surface as BootError");
            assert!(matches!(
                err,
                BootError::MailboxAlreadyClaimed { ref name }
                    if name == HttpCapability::NAMESPACE
            ));
        }

        #[test]
        fn disabled_adapter_replies_disabled() {
            let (mailer, _) = test_mailer_and_rx();
            let cap = HttpCapability::from_adapter(
                Arc::new(DisabledHttpAdapter),
                HttpConfig::default().default_timeout,
            );
            let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
            let mut ctx = NativeCtx::new(
                &transport,
                session_sender(),
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            );
            match cap.on_fetch(
                &mut ctx,
                Fetch {
                    url: "https://api.example.com/".to_string(),
                    method: HttpMethod::Get,
                    headers: vec![],
                    body: vec![],
                    timeout_ms: None,
                },
            ) {
                FetchResult::Err {
                    url,
                    error: HttpError::Disabled,
                } => {
                    assert_eq!(url, "https://api.example.com/");
                }
                other => panic!("expected Err Disabled, got {other:?}"),
            }
        }

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
                headers: vec![HttpHeader {
                    name: "content-type".to_string(),
                    value: "application/json".to_string(),
                }],
                body: b"{}".to_vec(),
            }));
            let cap = HttpCapability::from_adapter(
                stub as Arc<dyn HttpAdapter>,
                HttpConfig::default().default_timeout,
            );
            let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
            let mut ctx = NativeCtx::new(
                &transport,
                session_sender(),
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            );
            match cap.on_fetch(
                &mut ctx,
                Fetch {
                    url: "https://api.example.com/v1".to_string(),
                    method: HttpMethod::Get,
                    headers: vec![],
                    body: vec![],
                    timeout_ms: Some(5000),
                },
            ) {
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
            let (mailer, _) = test_mailer_and_rx();
            let cap = HttpCapability::from_adapter(
                StubAdapter::with(Err(HttpError::Timeout)) as Arc<dyn HttpAdapter>,
                HttpConfig::default().default_timeout,
            );
            let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
            let mut ctx = NativeCtx::new(
                &transport,
                session_sender(),
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            );
            match cap.on_fetch(
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
            let stub = StubAdapter::with(Ok(FetchResponse {
                status: 200,
                headers: vec![],
                body: vec![],
            }));
            let stub_clone = Arc::clone(&stub);
            let cap = HttpCapability::from_adapter(
                stub as Arc<dyn HttpAdapter>,
                HttpConfig::default().default_timeout,
            );
            let transport = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(0)));
            let mut ctx = NativeCtx::new(
                &transport,
                session_sender(),
                aether_data::MailId::NONE,
                aether_data::MailId::NONE,
            );
            let _ = cap.on_fetch(
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
            let cfg = HttpConfig {
                disabled: true,
                ..HttpConfig::default()
            };
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
}
