//! The typed route-authoring runtime surface (ADR-0131): the trait and
//! ctx types the `#[http::router]` / `#[http::route]` macros compile
//! down to. Always-on and wasm-safe — it names only `HttpServerRequest`
//! / `HttpServerResponse` / `HttpMethod` from `kinds.rs` and the core
//! `Deref` machinery, so a `default-features = false` guest that authors
//! routes gets it without the native runtime.
//!
//! ADR-0130 forecloses cap-side field extraction: the wire payload for a
//! routed kind is always request-shaped, so parsing a request into
//! domain values is guest-side, ordinary type-checked Rust. [`FromRequest`]
//! is that parse; its `Err` is the response the glue replies with,
//! typically a `400`.

use core::ops::{Deref, DerefMut};

use super::kinds::{HttpMethod, HttpServerRequest, HttpServerResponse};

/// Parse a value out of an inbound [`HttpServerRequest`]. The `Ok` value
/// is threaded to a routed method as a parameter; the `Err` is the
/// [`HttpServerResponse`] the generated glue replies with instead of
/// calling the handler — the boundary where a malformed request becomes
/// a `400` rather than an ad-hoc parse failure inside the handler.
///
/// Implement it for the domain types a route wants to receive
/// (`Json<T>`, a path capture, a validated query struct). The blanket
/// impl for [`HttpServerRequest`] itself hands a handler the raw request
/// as a parameter.
pub trait FromRequest: Sized {
    /// Parse `self` from `request`, or return the response to send in
    /// place of dispatching the handler.
    ///
    /// # Errors
    ///
    /// Returns the [`HttpServerResponse`] (typically a `400`) to reply
    /// with when the request cannot be parsed into this type.
    fn from_request(request: &HttpServerRequest) -> Result<Self, HttpServerResponse>;
}

/// Identity extractor: a routed method that wants the whole request as a
/// parameter takes an `HttpServerRequest`, cloned from the dispatched
/// payload.
impl FromRequest for HttpServerRequest {
    fn from_request(request: &HttpServerRequest) -> Result<Self, HttpServerResponse> {
        Ok(request.clone())
    }
}

/// A path parameter captured from a route template's `{name}` segment
/// (ADR-0154). A routed method receives the parsed value wrapped in
/// `Path`; the `#[http::route]` glue binds captures to the method's
/// `Path<_>` parameters positionally, in template-capture order, so
/// `#[http::route(Get, "/drafts/{id}")] fn get(.., id: Path<u64>)` binds
/// `{id}` into `id.0`. The parse is [`FromPathSegment`]; its `Err` short-
/// circuits to the response the glue replies with (typically a `400`).
pub struct Path<T>(pub T);

/// Parse a value out of one captured path segment — the path-parameter
/// twin of [`FromRequest`] (ADR-0154). The `Ok` value is threaded into a
/// routed method wrapped in [`Path`]; the `Err` is the
/// [`HttpServerResponse`] the generated glue replies with instead of
/// dispatching the handler, the boundary where an unparseable segment
/// becomes a `400` rather than an ad-hoc failure inside the handler.
///
/// Implemented for [`String`] (any segment) and the integer id types; a
/// domain id type implements it to be captured directly.
pub trait FromPathSegment: Sized {
    /// Parse `self` from one raw path segment, or return the response to
    /// send in place of dispatching the handler.
    ///
    /// # Errors
    ///
    /// Returns the [`HttpServerResponse`] (typically a `400`) to reply
    /// with when the segment cannot be parsed into this type.
    fn from_path_segment(segment: &str) -> Result<Self, HttpServerResponse>;
}

/// Any segment is a valid `String` capture.
impl FromPathSegment for String {
    fn from_path_segment(segment: &str) -> Result<Self, HttpServerResponse> {
        Ok(segment.to_string())
    }
}

/// Integer path parameters parse through `FromStr`; a non-numeric segment
/// is the glue's `400`.
macro_rules! from_path_segment_via_fromstr {
    ($($ty:ty),* $(,)?) => {$(
        impl FromPathSegment for $ty {
            fn from_path_segment(segment: &str) -> Result<Self, HttpServerResponse> {
                segment.parse::<$ty>().map_err(|_| HttpServerResponse {
                    status: 400,
                    headers: Vec::new(),
                    body: b"path parameter is not a valid integer".to_vec(),
                })
            }
        }
    )*};
}

from_path_segment_via_fromstr!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

/// The route a glue handler serves — compile-time constants the
/// `#[http::route]` macro stamps in, surfaced through [`Ctx::route`].
/// `prefix` is the claimed path prefix; `method` is the method filter
/// (`None` for a method-agnostic route).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    /// The path prefix this route claimed.
    pub prefix: &'static str,
    /// The HTTP method filter, or `None` for a method-agnostic route.
    pub method: Option<HttpMethod>,
}

/// The routing ctx handed to a routed method: the transport ctx (`C` —
/// `WasmCtx` or `NativeCtx`) plus the original request and the matched
/// route. Derefs to `C`, so a routed method sends mail and reaches
/// capabilities exactly as an ordinary handler does; [`Ctx::request`]
/// and [`Ctx::route`] are both total because a routed payload is always
/// request-shaped (ADR-0130) and the route is known statically per glue
/// handler.
pub struct Ctx<'a, C> {
    transport: &'a mut C,
    request: HttpServerRequest,
    route: Route,
}

impl<'a, C> Ctx<'a, C> {
    /// Wrap a transport ctx with the dispatched request and matched
    /// route. Called by the `#[http::route]` glue, not by hand.
    pub fn new(transport: &'a mut C, request: HttpServerRequest, route: Route) -> Self {
        Self { transport, request, route }
    }

    /// The original inbound request this route dispatched.
    #[must_use]
    pub fn request(&self) -> &HttpServerRequest {
        &self.request
    }

    /// The route that selected this handler.
    #[must_use]
    pub fn route(&self) -> Route {
        self.route
    }
}

impl<C> Deref for Ctx<'_, C> {
    type Target = C;

    fn deref(&self) -> &C {
        self.transport
    }
}

impl<C> DerefMut for Ctx<'_, C> {
    fn deref_mut(&mut self) -> &mut C {
        self.transport
    }
}
