//! `aether.http` cap. Owns the full HTTP egress stack — `HttpAdapter`
//! trait, the `ureq`-backed `UreqHttpAdapter`, env-driven `HttpConfig`,
//! and the [`HttpCapability`] itself. Chassis mains resolve a
//! [`HttpConfig`] (typically via [`HttpConfig::from_env`]) and pass it
//! to `with_actor::<HttpCapability>(config)`.
//!
//! v1 semantics (ADR-0043):
//! - Blocking on the dispatcher thread (one request at a time).
//! - Buffered request + response bodies; streaming is deferred.
//! - Deny-by-default initial-URL host allowlist via `AETHER_HTTP_ALLOWLIST`.
//!   The current default ureq redirect policy is not revalidated per hop.
//! - Response size capped at `AETHER_HTTP_MAX_BODY_BYTES` (16MB).
//! - Default request timeout 30s, per-request override via
//!   `Fetch.timeout_ms`.

mod config;

pub use config::HttpConfig;
// The `Config` derive on `HttpConfig` emits these native-only sibling types
// in `config`; chassis CLI / boot wiring addresses them through the
// `client::` path, so re-export them here.
#[cfg(feature = "runtime")]
pub use config::{HttpConfigLayer, HttpOverlay};

// Handler-signature kinds resolve at file root through this import —
// `#[actor]` emits the `impl HandlesKind<K> for X {}` markers always-on
// against the identity (outside the `feature = "runtime"` gate), so they
// reference `Fetch` from here. `HttpMethod` is named by `HttpMailboxExt`.
use crate::http::kinds::{Fetch, HttpMethod};
use aether_actor::WasmActorMailbox;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeActorMailbox;

// The wire reply kind. Named by the always-on (non-wasm) handler-inventory
// submission `#[actor]` emits for `on_fetch`'s reply type, so it gates on
// `not(wasm32)` rather than `runtime`; the runtime-gated handler in
// `runtime.rs` names it through the kinds path directly.
#[cfg(not(target_family = "wasm"))]
use crate::http::kinds::FetchResult;

/// Default response-body cap when `AETHER_HTTP_MAX_BODY_BYTES` is
/// unset. 16MB matches ADR-0043 §3.
pub const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Default per-request timeout when `AETHER_HTTP_TIMEOUT_MS` is unset
/// and the fetch itself supplies no `timeout_ms`. 30s matches
/// ADR-0043 §4.
pub const DEFAULT_TIMEOUT_MILLIS: u32 = 30_000;

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
///   sends, gated on `#[cfg(not(target_family = "wasm"))]`.
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

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
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

/// `aether.http` cap **identity** (ADR-0122 identity/runtime split). A ZST
/// carrying only the addressing — `Addressable` (`NAMESPACE`, `Resolver`),
/// the per-handler `HandlesKind` markers, and the name-inventory entry,
/// all emitted always-on by `#[actor]`. The state-bearing runtime
/// (`HttpCapabilityState`, which holds the resolved `Arc<dyn HttpAdapter>`)
/// plus the `ureq` adapter stack live behind the one `feature = "runtime"`
/// gate, so a transport-only build never names `HttpCapabilityState` nor
/// pulls `aether_substrate` through this cap.
#[actor(singleton)]
pub struct HttpCapability;

// The struct-hosted `#[actor(singleton)]` reads the sibling `runtime` module
// off disk, lifts the `NAMESPACE` + `#[handler]` kinds out of the
// `#[runtime] impl NativeActor` there, and emits the always-on identity
// markers (`Addressable`, `HandlesKind<Fetch>`, the name-inventory entry)
// against this struct. The handler kind those markers name (`Fetch`) is
// imported at file root above.
use aether_actor::actor;

// The runtime half — the whole `aether_substrate`- / `ureq`-typed surface
// (`HttpCapabilityState`, the adapter abstraction, the `UreqHttpAdapter`
// stack, the `#[runtime] impl NativeActor` with the handler bodies) — lives
// in `runtime.rs`, gated once here. The cap's unit tests drive `on_fetch`
// directly, so they live alongside it in `runtime.rs` (the handler method is
// private to that module), matching the `gemini` / `ui` split caps.
#[cfg(feature = "runtime")]
mod runtime;
