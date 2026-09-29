//! The routed handler fixtures: actors that claim a path through the typed
//! `#[http::router]` / `#[http::route]` authoring surface (ADR-0131), the
//! path-template resource (ADR-0154), and the macro-authored precedence
//! handlers the routing tests drive.

use aether_actor::actor;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate as http;
use crate::kinds::{HttpServerRequest, HttpServerResponse, RegisterRouteSelf, UnregisterRouteSelf};
use crate::server::HttpServerCapability;

/// Claims `/api` through the typed authoring surface (`#[http::router]`
/// / `#[http::route]`, ADR-0131): the macro emits the router's
/// `HttpServerRequest` handler and injects its `wire` registration. The
/// handler echoes the decoded path, proving the payload round-tripped as a
/// request (not merely that dispatch picked the right mailbox).
pub struct ApiRouteHandler;
pub struct ApiRouteHandlerState;

#[http::router]
#[actor(singleton, root, depends(HttpServerCapability))]
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
#[actor(singleton, root, depends(HttpServerCapability))]
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
/// request to `/tmp` falls back to the default handler. The router types
/// the route's ctx by this actor, so the route reaches the server through
/// the proven reference its typed ctx mints for the declared dependency.
pub struct TmpRouteHandler;
pub struct TmpRouteHandlerState;

#[http::router]
#[actor(singleton, root, depends(HttpServerCapability))]
impl NativeActor for TmpRouteHandler {
    type State = TmpRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_tmp";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<TmpRouteHandlerState, BootError> {
        Ok(TmpRouteHandlerState)
    }

    /// Release `/tmp`, then reply the `tmp` tag.
    #[http::route(any, "/tmp")]
    fn on_tmp(_state: &mut TmpRouteHandlerState, mut ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        let server = ctx.actor_ref::<HttpServerCapability>();
        ctx.send_to(server, &UnregisterRouteSelf { prefix: "/tmp".to_string(), method: None });
        HttpServerResponse { status: 200, headers: Vec::new(), body: b"tmp".to_vec() }
    }
}

/// A macro route alongside a hand-written `wire`: the macro appends
/// its `/wired` registration to the author's `wire`, which independently
/// claims `/wired-extra` on the raw surface. The router's one request
/// handler serves both claims and has no route for `/wired-extra`, so that
/// path answers the router's `404`: the author's registration survived the
/// append.
pub struct WiredRouteHandler;
pub struct WiredRouteHandlerState;

#[http::router]
#[actor(singleton, root, depends(HttpServerCapability))]
impl NativeActor for WiredRouteHandler {
    type State = WiredRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_wired";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<WiredRouteHandlerState, BootError> {
        Ok(WiredRouteHandlerState)
    }

    fn wire(_state: &mut WiredRouteHandlerState, ctx: &mut NativeCtx<'_>) {
        ctx.send::<HttpServerCapability>(&RegisterRouteSelf {
            prefix: "/wired-extra".to_string(),
            method: None,
            shared: false,
        });
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
#[actor(singleton, root, depends(HttpServerCapability))]
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

/// Two route groups where one static head extends the other: `/a/{x}/{y}`
/// claims `/a`, and `/a/b` claims `/a/b`. The server sends `/a/b/c` to the
/// `/a/b` key, the longer prefix, so the router must answer it from the
/// `/a/b` group, which has no three-segment template, rather than from the
/// looser `/a` template that would also match it.
pub struct NestedRouteHandler;
pub struct NestedRouteHandlerState;

#[http::router]
#[actor(singleton, root, depends(HttpServerCapability))]
impl NativeActor for NestedRouteHandler {
    type State = NestedRouteHandlerState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.http.test_route_nested";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<NestedRouteHandlerState, BootError> {
        Ok(NestedRouteHandlerState)
    }

    /// `GET /a/{x}/{y}` — the `/a` group's one template.
    #[http::route(Get, "/a/{x}/{y}")]
    fn pair(
        _state: &mut NestedRouteHandlerState,
        _ctx: http::Ctx<'_, NativeCtx<'_>>,
        x: http::Path<String>,
        y: http::Path<String>,
    ) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: format!("a:{}:{}", x.0, y.0).into_bytes() }
    }

    /// `GET /a/b` — the `/a/b` group's one template.
    #[http::route(Get, "/a/b")]
    fn b(_state: &mut NestedRouteHandlerState, _ctx: http::Ctx<'_, NativeCtx<'_>>) -> HttpServerResponse {
        HttpServerResponse { status: 200, headers: Vec::new(), body: b"a/b".to_vec() }
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
        #[actor(singleton, root, depends(HttpServerCapability))]
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
