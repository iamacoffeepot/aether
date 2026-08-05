//! The routed handler fixtures: actors that claim a path through the typed
//! `#[http::router]` / `#[http::route]` authoring surface (ADR-0131), the
//! path-template resource (ADR-0154), the deferred-route relay pair, and the
//! macro-authored precedence handlers the routing tests drive.

use aether_actor::{Manual, actor};
use aether_data::Kind;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate as http;
use crate::kinds::{HttpServerRequest, HttpServerResponse, RegisterRouteSelf, UnregisterRouteSelf};
use crate::server::HttpServerCapability;

/// Claims `/api` through the typed authoring surface (`#[http::router]`
/// / `#[http::route]`, ADR-0131): the macro mints the route's
/// request-shaped kind, injects its `wire` registration, and decodes
/// the dispatched payload under the minted kind. The handler echoes
/// the decoded path, proving the payload round-tripped as the minted
/// kind (not merely that dispatch picked the right mailbox).
pub struct ApiRouteHandler;
pub struct ApiRouteHandlerState;

#[http::router]
#[actor(singleton, root)]
impl NativeActor for ApiRouteHandler {
    type State = ApiRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_api";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ApiRouteHandlerState, BootError> {
        Ok(ApiRouteHandlerState)
    }

    /// Echo the decoded request path back under `/api`.
    #[http::route(any, "/api")]
    fn on_api(_state: &mut ApiRouteHandlerState, ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        HttpServerResponse {
            status: 200,
            headers: Vec::new(),
            body: format!("api:{}", ctx.request().path).into_bytes(),
        }
    }
}

/// A required-`name`-query extractor: parses `?name=…` off the
/// request, or returns the `400` the routed glue replies with in
/// place of dispatching the handler (ADR-0131's typed boundary).
pub struct QueryName(pub String);

impl http::FromRequest for QueryName {
    fn from_request(request: &HttpServerRequest) -> Result<Self, HttpServerResponse> {
        for pair in request.query.split('&') {
            if let Some(value) = pair.strip_prefix("name=") {
                return Ok(Self(value.to_string()));
            }
        }
        Err(HttpServerResponse { status: 400, headers: Vec::new(), body: b"missing name query parameter".to_vec() })
    }
}

/// Claims `/extract` and threads a real [`QueryName`] extractor into
/// the routed method, so a request to `/extract` either dispatches
/// with the extracted value (echoed at `200`) or short-circuits to
/// the extractor's `400` before the handler runs.
pub struct ExtractRouteHandler;
pub struct ExtractRouteHandlerState;

#[http::router]
#[actor(singleton, root)]
impl NativeActor for ExtractRouteHandler {
    type State = ExtractRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_extract";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<ExtractRouteHandlerState, BootError> {
        Ok(ExtractRouteHandlerState)
    }

    /// Echo the extracted `name` query value; the glue never reaches
    /// here when the extractor returns its `400`.
    #[http::route(any, "/extract")]
    fn on_extract(
        _state: &mut ExtractRouteHandlerState,
        _ctx: http::Ctx<'_, NativeCtx<'_>>,
        name: QueryName,
    ) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: format!("hello:{}", name.0).into_bytes() }
    }
}

/// Claims `/tmp` through the macro surface; on any request the routed
/// method releases its own route via the raw `unregister_route_self`
/// (a protocol op the typed surface leaves to the body), so the next
/// request to `/tmp` falls back to the default handler. `ctx` derefs
/// to `NativeCtx`, so the raw send reads exactly as an ordinary
/// handler's.
pub struct TmpRouteHandler;
pub struct TmpRouteHandlerState;

#[http::router]
#[actor(singleton, root)]
impl NativeActor for TmpRouteHandler {
    type State = TmpRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_tmp";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<TmpRouteHandlerState, BootError> {
        Ok(TmpRouteHandlerState)
    }

    /// Release `/tmp`, then reply the `tmp` tag.
    #[http::route(any, "/tmp")]
    fn on_tmp(_state: &mut TmpRouteHandlerState, ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        ctx.actor::<HttpServerCapability>().send(&UnregisterRouteSelf { prefix: "/tmp".to_string(), method: None });
        HttpServerResponse { status: 200, headers: Vec::new(), body: b"tmp".to_vec() }
    }
}

/// A macro route alongside a hand-written `wire`: the macro appends
/// its `/wired` registration to the author's `wire` (which
/// independently claims `/wired-extra` on the raw surface for a
/// generic-kind `#[handler]`), so both routes dispatch and the
/// hand-written registration survives the append.
pub struct WiredRouteHandler;
pub struct WiredRouteHandlerState;

#[http::router]
#[actor(singleton, root)]
impl NativeActor for WiredRouteHandler {
    type State = WiredRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_wired";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<WiredRouteHandlerState, BootError> {
        Ok(WiredRouteHandlerState)
    }

    fn wire(_state: &mut WiredRouteHandlerState, ctx: &mut NativeCtx<'_>) {
        ctx.actor::<HttpServerCapability>().send(&RegisterRouteSelf {
            prefix: "/wired-extra".to_string(),
            method: None,
            kind: <HttpServerRequest as Kind>::ID,
            shared: false,
        });
    }

    /// Generic-kind dispatch for the hand-registered `/wired-extra`.
    #[handler::single]
    fn on_extra(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _request: HttpServerRequest) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: b"wired-raw".to_vec() }
    }

    /// The macro route whose registration is appended to `wire`.
    #[http::route(any, "/wired")]
    fn on_wired(_state: &mut WiredRouteHandlerState, _ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: b"wired-macro".to_vec() }
    }
}

/// Path-template routing (ADR-0154) over a small `/books` REST
/// resource: nested routes that share the `/books` static head
/// collapse into one registration per method and dispatch by segment,
/// and `{id}` binds through `http::Path<u64>` — a non-numeric segment
/// short-circuits to the `FromPathSegment` `400`. `GET /books`
/// (collection) and `GET /books/{id}` (member) share one group;
/// `POST /books/{id}/checkout` is the sibling POST group under the
/// same static head.
pub struct BookRouteHandler;
pub struct BookRouteHandlerState;

#[http::router]
#[actor(singleton, root)]
impl NativeActor for BookRouteHandler {
    type State = BookRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_books";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<BookRouteHandlerState, BootError> {
        Ok(BookRouteHandlerState)
    }

    /// `GET /books` — the collection.
    #[http::route(Get, "/books")]
    fn list_books(_state: &mut BookRouteHandlerState, _ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: b"books:list".to_vec() }
    }

    /// `GET /books/{id}` — one member, `{id}` bound through `Path<u64>`.
    #[http::route(Get, "/books/{id}")]
    fn get_book(
        _state: &mut BookRouteHandlerState,
        _ctx: http::Ctx<'_, NativeCtx<'_>>,
        id: http::Path<u64>,
    ) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: format!("books:get:{}", id.0).into_bytes() }
    }

    /// `POST /books/{id}/checkout` — an action on a member, the sibling
    /// POST group under the same static head.
    #[http::route(Post, "/books/{id}/checkout")]
    fn checkout_book(
        _state: &mut BookRouteHandlerState,
        _ctx: http::Ctx<'_, NativeCtx<'_>>,
        id: http::Path<u64>,
    ) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: format!("books:checkout:{}", id.0).into_bytes() }
    }
}

/// The request/reply kind pair for the deferred-route fixtures
/// (ADR-0154 §2): a route forwards `EchoAsk` to a peer cap, which
/// replies `EchoSay`.
#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize)]
#[kind(name = "aether.http.test_echo_ask")]
pub struct EchoAsk {
    pub text: String,
}

#[derive(aether_data::Kind, aether_data::Schema, serde::Serialize, serde::Deserialize)]
#[kind(name = "aether.http.test_echo_say")]
pub struct EchoSay {
    pub text: String,
}

/// A peer cap that answers `EchoAsk` with `EchoSay` — the downstream a
/// deferred route forwards to and answers on.
pub struct EchoPeer;
pub struct EchoPeerState;

#[actor(singleton, root)]
impl NativeActor for EchoPeer {
    type State = EchoPeerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_echo_peer";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<EchoPeerState, BootError> {
        Ok(EchoPeerState)
    }

    #[handler::single]
    fn on_ask(_state: &mut EchoPeerState, _ctx: &mut NativeCtx<'_>, ask: EchoAsk) -> EchoSay {
        EchoSay { text: ask.text }
    }
}

/// A peer that receives `EchoAsk` and never replies — the downstream a
/// deferred route's `502` settlement net catches.
pub struct SilentPeer;
pub struct SilentPeerState;

#[actor(singleton, root)]
impl NativeActor for SilentPeer {
    type State = SilentPeerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_silent_peer";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<SilentPeerState, BootError> {
        Ok(SilentPeerState)
    }

    // Deliberately manual + no reply: the deferred route's downstream
    // chain settles without an answer, arming the `504`.
    #[handler::manual]
    fn on_ask(_state: &mut SilentPeerState, _ctx: &mut NativeCtx<'_, Manual>, _ask: EchoAsk) {}
}

/// A deferred-route handler (ADR-0154 §2): `/echo` forwards to
/// [`EchoPeer`] and answers on its `EchoSay` reply; `/blackhole`
/// forwards to [`SilentPeer`] and is answered `502` by the settlement
/// net when that chain settles without a reply.
pub struct DeferRouteHandler;
pub struct DeferRouteHandlerState;

#[http::router]
#[actor(singleton, root)]
impl NativeActor for DeferRouteHandler {
    type State = DeferRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_defer";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<DeferRouteHandlerState, BootError> {
        Ok(DeferRouteHandlerState)
    }

    /// `GET /echo` — forward to the echo peer by type, answer on its
    /// reply. `defer(&request)` captures the request; `.to::<R>()` forwards it.
    #[http::route(Get, "/echo")]
    fn echo(_state: &mut DeferRouteHandlerState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        ctx.defer(&EchoAsk { text: "hi".to_string() }).to::<EchoPeer>()
    }

    /// `GET /blackhole` — forward to the silent peer; it settles without a
    /// reply, so the server's own `502` net answers.
    #[http::route(Get, "/blackhole")]
    fn blackhole(_state: &mut DeferRouteHandlerState, ctx: http::Ctx<'_, NativeCtx<'_, Manual>>) -> http::Outcome {
        ctx.defer(&EchoAsk { text: "void".to_string() }).to::<SilentPeer>()
    }

    /// Map the peer's `EchoSay` reply into the response answered through
    /// the held request obligation.
    #[http::reply]
    fn on_say(
        _state: &mut DeferRouteHandlerState,
        _ctx: &mut NativeCtx<'_, Manual>,
        say: EchoSay,
    ) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: format!("echoed:{}", say.text).into_bytes() }
    }
}

/// A macro-authored routed handler that claims its prefixes through
/// `#[http::route]` and replies `200` with a fixed tag body. Drives
/// the longest-prefix and method-filter precedence tests through the
/// typed authoring surface (the macro emits the registration).
macro_rules! routed_handler {
    ($ty:ident, $state:ident, $namespace:literal, $tag:literal,
     $method:ident, $prefix:literal) => {
        pub struct $ty;
        pub struct $state;

        #[http::router]
        #[actor(singleton, root)]
        impl NativeActor for $ty {
            type State = $state;
            type Config = ();
            const NAMESPACE: &'static str = $namespace;

            fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<$state, BootError> {
                Ok($state)
            }

            #[http::route($method, $prefix)]
            fn on_route(_state: &mut $state, _ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
                HttpServerResponse { status: 200, headers: Vec::new(), body: $tag.to_vec() }
            }
        }
    };
}

routed_handler!(ApiV2Handler, ApiV2HandlerState, "aether.http.test_route_api_v2", b"api-v2", any, "/api/v2");
routed_handler!(MethodPostHandler, MethodPostHandlerState, "aether.http.test_route_post_m", b"post-m", Post, "/m");
routed_handler!(MethodAnyHandler, MethodAnyHandlerState, "aether.http.test_route_any_m", b"any-m", any, "/m");
